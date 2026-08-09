//! Rendering of the interactive screen.
//!
//! `draw` is stateless with respect to persistence: it takes the `App`
//! (which owns the `View` and the latest poll `Snapshot`) and paints a
//! header, the scrollable grouped VM list, and a footer. Rows are drawn as
//! position-packed, width-truncated strings so nothing ever wraps and the
//! scroll offset stays predictable.

use chrono::{DateTime, Utc};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::app::{App, FilterMode};
use crate::format;
use crate::model::{Vm, VmMode, VmState};
use crate::view::{Group, RowId, View};

// --- column widths (in display characters) -------------------------------------

const C_PAD: usize = 1; // blank gap after each column
const C_PROJ: usize = 7;
const C_STATE: usize = 10;
const C_ID: usize = 26;
const C_NAME: usize = 20;
const C_IP: usize = 12;
const C_SUBNET: usize = 12;
const C_IMAGE: usize = 10;
const C_CPU: usize = 4;
const C_MEM: usize = 6;
const C_DISK: usize = 7;
const C_MODE: usize = 6;
const C_PORTS: usize = 16;
const C_TAGS: usize = 14;
const C_UPTIME: usize = 8;

/// Per-column widths for the fields after the state column.
const FIELD_WIDTHS: [usize; 12] = [
    C_ID, C_NAME, C_IP, C_SUBNET, C_IMAGE, C_CPU, C_MEM, C_DISK, C_MODE, C_PORTS, C_TAGS, C_UPTIME,
];
/// Column labels matching [`FIELD_WIDTHS`].
const FIELD_LABELS: [&str; 12] = [
    "VM ID", "NAME", "IP", "SUBNET", "IMAGE", "CPU", "MEM", "DISK", "MODE", "PORTS", "TAGS",
    "UPTIME",
];

/// Keep the first `count` characters of `s`.
fn cut_right(s: &str, count: usize) -> String {
    s.chars().take(count).collect()
}

/// Keep the *last* `count` characters of `s` (VM ids identify the machine by
/// their random suffix), prefixing `..` when shortened.
fn cut_tail(s: &str, count: usize) -> String {
    let n = s.chars().count();
    if n <= count {
        s.to_string()
    } else if count >= 4 {
        format!("..{}", s.chars().skip(n - count + 2).collect::<String>())
    } else {
        s.chars().take(count).collect()
    }
}

/// Left-justify `s` into exactly `count` padded display columns.
fn col(s: &str, count: usize) -> String {
    let t = cut_right(s, count);
    let pad = count.saturating_sub(t.chars().count());
    format!("{t}{}", " ".repeat(pad))
}

/// Truncate `s` to at most `width` characters (whole-line helper).
fn clip(s: &str, width: usize) -> String {
    cut_right(s, width)
}

/// Text color for a VM state name.
fn state_color(state: VmState) -> Color {
    match state {
        VmState::Started => Color::Green,
        VmState::Paused => Color::Yellow,
        VmState::Suspended => Color::Cyan,
        VmState::Destroyed => Color::DarkGray,
        VmState::Created => Color::LightYellow,
        _ => Color::Magenta, // transitional
    }
}

/// The per-column payloads of one VM row, after the state column.
fn vm_fields(vm: &Vm, now: DateTime<Utc>) -> Vec<String> {
    let ports = vm
        .vm_config
        .network_config
        .exposed_ports
        .iter()
        .map(|p| p.port.to_string())
        .collect::<Vec<_>>()
        .join(",");
    vec![
        cut_tail(&vm.vm_id, C_ID),
        cut_right(&vm.vm_config.name, C_NAME),
        vm.guest_ip().map_or_else(String::new, |ip| ip.to_string()),
        vm.net
            .as_ref()
            .map_or_else(String::new, |n| n.subnet.clone()),
        cut_right(&vm.vm_config.image, C_IMAGE),
        vm.vm_config.cpus.to_string(),
        format::memory_mib(u64::from(vm.vm_config.memory_mb)),
        format::disk_mib(u64::from(vm.vm_config.disk_size_mb)),
        match vm.vm_config.mode {
            VmMode::Ephemeral => "eph",
            VmMode::Permanent => "perm",
            VmMode::Schedule => "sched",
        }
        .to_string(),
        ports,
        vm.vm_config.tags.join(","),
        format::uptime(now, vm.created_at),
    ]
}

/// A project section header line.
fn group_header(group: &Group, width: usize) -> Line<'static> {
    let mut text = format!("project {:<cw$}", group.project_id, cw = C_PROJ);
    let c = &group.counts;
    let mut counts = Vec::new();
    if c.started > 0 {
        counts.push(format!("{} running", c.started));
    }
    if c.paused > 0 {
        counts.push(format!("{} paused", c.paused));
    }
    if c.suspended > 0 {
        counts.push(format!("{} suspended", c.suspended));
    }
    if c.destroyed > 0 {
        counts.push(format!("{} gone", c.destroyed));
    }
    if c.other > 0 {
        counts.push(format!("{} other", c.other));
    }
    if !counts.is_empty() {
        let plural = if c.total == 1 { "" } else { "s" };
        text.push_str(&format!(
            "   [{} · {} VM{plural}]",
            counts.join(", "),
            c.total
        ));
    }
    if let Some(subnet) = &group.subnet {
        text.push_str(&format!("   subnet {subnet}"));
    }
    Line::styled(
        clip(&text, width),
        Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
    )
}

/// The column heading line; the project column only appears in flat mode.
fn heading_line(flat: bool, width: usize) -> Line<'static> {
    let mut s = format!("{:<st$}", "STATE", st = C_STATE);
    if flat {
        s.push_str(&format!("{:<pr$}", "PROJ", pr = C_PROJ + C_PAD));
    }
    for (label, width) in FIELD_LABELS.iter().zip(&FIELD_WIDTHS) {
        s.push_str(&format!("{label:<w$}", w = width + C_PAD));
    }
    Line::from(Span::styled(
        clip(&s, width),
        Style::new().add_modifier(Modifier::DIM),
    ))
}

/// One VM row; `selected` toggles the reversed highlight.
fn vm_row(vm: &Vm, now: DateTime<Utc>, flat: bool, width: usize, selected: bool) -> Line<'static> {
    let sel = if selected {
        Modifier::REVERSED | Modifier::BOLD
    } else {
        Modifier::empty()
    };
    let state_txt: String = col(&vm.state.to_string(), C_STATE);

    let mut rest = String::new();
    if flat {
        rest.push_str(&col(&vm.vm_config.project_id.to_string(), C_PROJ));
        rest.push(' ');
    }
    let fields = vm_fields(vm, now);
    let lead = C_STATE + C_PAD + if flat { C_PROJ + C_PAD } else { 0 };
    let mut budget = width.saturating_sub(lead);
    for (i, (f, w)) in fields.iter().zip(&FIELD_WIDTHS).enumerate() {
        // The last column absorbs what remains of the row's budget.
        let w = if i == fields.len() - 1 {
            budget.min(*w)
        } else {
            *w
        };
        rest.push_str(&col(&cut_right(f, w), w + C_PAD));
        budget = budget.saturating_sub(w + C_PAD);
    }

    let mut spans = Vec::with_capacity(3);
    if selected {
        spans.push(Span::styled(
            state_txt,
            Style::new().fg(state_color(vm.state)).add_modifier(sel),
        ));
        if flat {
            spans.push(Span::styled(
                col(&vm.vm_config.project_id.to_string(), C_PROJ),
                Style::new().add_modifier(sel),
            ));
        }
        spans.push(Span::styled(rest, Style::new().add_modifier(sel)));
    } else {
        spans.push(Span::styled(
            state_txt,
            Style::new().fg(state_color(vm.state)),
        ));
        if flat {
            spans.push(Span::raw(col(&vm.vm_config.project_id.to_string(), C_PROJ)));
        }
        spans.push(Span::raw(rest));
    }
    Line::from(spans)
}

/// Assemble every body line plus the index of the selected VM row.
fn body_lines(
    view: &View,
    now: DateTime<Utc>,
    width: usize,
    filter_indicator: &str,
) -> (Vec<Line<'static>>, Option<usize>) {
    let flat = !view.grouped;
    let mut lines = Vec::new();
    let mut selected_idx = None;

    if !filter_indicator.is_empty() {
        lines.push(Line::from(Span::styled(
            clip(
                &format!("filter: \"{filter_indicator}\"  (Esc clears)"),
                width,
            ),
            Style::new().fg(Color::Cyan),
        )));
    }
    lines.push(heading_line(flat, width));

    if view.is_empty() {
        let msg = if view.all_vms().is_empty() {
            "no VMs yet — waiting for the first poll"
        } else {
            "no VMs match the filter"
        };
        lines.push(Line::from(Span::styled(
            clip(msg, width),
            Style::new().add_modifier(Modifier::DIM),
        )));
        return (lines, selected_idx);
    }

    let selected = view.selected_vm();
    for row in view.rows() {
        match row {
            RowId::Group(gi) => lines.push(group_header(view.group(*gi), width)),
            RowId::Vm(i) => {
                let vm = view.vm(*i);
                let sel = selected.is_some_and(|s| s.vm_id == vm.vm_id);
                if sel {
                    selected_idx = Some(lines.len());
                }
                lines.push(vm_row(vm, now, flat, width, sel));
            }
        }
    }
    (lines, selected_idx)
}

/// Vertical offset that keeps the selected row in the visible window.
fn scroll_top(total_lines: usize, selected: Option<usize>, window: usize) -> u16 {
    if window == 0 || total_lines <= window {
        return 0;
    }
    let Some(pos) = selected else {
        return 0;
    };
    let top = (pos as i64 - window as i64 / 2).clamp(0, total_lines as i64 - window as i64);
    top as u16
}

/// Detail line about the selected VM.
fn detail_line(app: &App) -> Line<'static> {
    let Some(vm) = app.view().selected_vm() else {
        return Line::from(Span::styled(
            "no VM selected",
            Style::new().add_modifier(Modifier::DIM),
        ));
    };
    let mut parts = vec![format!("selected  {}", vm.vm_id)];
    if !vm.vm_config.name.is_empty() {
        parts.push(format!("name={}", vm.vm_config.name));
    }
    if let Some(net) = &vm.net {
        parts.push(format!("gateway={}  tap={}", net.gateway_ip, net.tap_name));
    }
    if let Some(auto) = &vm.vm_config.auto_suspend {
        parts.push(format!("auto-suspend {}s", auto.idle_timeout_secs));
    }
    let num_ports = vm.exposed_port_count();
    if num_ports > 0 {
        parts.push(format!("{num_ports} exposed port(s)"));
    }
    if let Some(snap) = &vm.snapshot {
        parts.push(format!("snap {}", format::ago(Utc::now(), snap.created_at)));
    }
    if let Some(bs) = &vm.vm_config.block_storage {
        parts.push(format!("block {:.1}G", f64::from(bs.size_mb) / 1024.0));
    }
    Line::from(Span::styled(
        parts.join("   "),
        Style::new().fg(Color::LightBlue),
    ))
}

/// The bottom help / filter prompt line.
fn help_line(app: &App) -> Line<'static> {
    if app.filter_mode() == FilterMode::Filtering {
        return Line::from(Span::styled(
            format!("filter: {}▌", app.filter()),
            Style::new().fg(Color::Cyan),
        ));
    }
    const HELP: &str = "j/k or ↑/↓ move · PgUp/PgDn · g/G first/last · / filter · f group/flat · s sort · r refresh · q quit";
    Line::from(Span::styled(HELP, Style::new().add_modifier(Modifier::DIM)))
}

/// Screen header line 1: identity and connection state.
fn title_line(app: &App) -> Line<'static> {
    let snap = app.snap();
    let mut text = format!("  tikovm · {}", app.host_disp());
    text.push(' ');
    let (add, color, bold) = if snap.connected() {
        let t = snap.last_ok.map_or_else(String::new, format::clock);
        (format!("connected · last poll {t}"), Color::Green, false)
    } else {
        let since = snap
            .last_ok
            .map(|t| format!("last ok {}", format::clock(t)))
            .unwrap_or_else(|| "no data yet".to_string());
        (format!("OFFLINE · {since}"), Color::Red, true)
    };
    let mut style = Style::new().fg(color);
    if bold {
        style = style.add_modifier(Modifier::BOLD);
    }
    Line::from(vec![
        Span::styled(
            "vmtop",
            Style::new().fg(Color::Green).add_modifier(Modifier::BOLD),
        ),
        Span::styled(text, Style::new().add_modifier(Modifier::DIM)),
        Span::styled(add, style),
    ])
}

/// Screen header line 2: aggregate counters.
fn summary_line(app: &App) -> Line<'static> {
    let hc = app.view().host_counts();
    let mut s = format!("VMs {}", hc.total);
    s.push_str(&format!("   running {}", hc.started));
    s.push_str(&format!("   paused {}", hc.paused));
    s.push_str(&format!("   suspended {}", hc.suspended));
    if hc.other > 0 {
        s.push_str(&format!("   other {}", hc.other));
    }
    s.push_str(&format!("   projects {}", app.view().host_projects()));
    let (cpu, mem) = app.view().host_alloc();
    s.push_str(&format!(
        "   alloc cpu {cpu}  mem {}",
        format::memory_mib(mem)
    ));
    s.push_str(&format!("   refresh {:?}", app.interval()));
    Line::from(Span::styled(s, Style::new().add_modifier(Modifier::DIM)))
}

/// The scrollable list paragraph for the body region.
fn body_widget(app: &App, area: Rect) -> Paragraph<'static> {
    let now = Utc::now();
    let indicator = if app.filter_mode() == FilterMode::Filtering {
        app.filter().to_string()
    } else if app.view().filter.is_empty() {
        String::new()
    } else {
        app.view().filter.clone()
    };
    let width = area.width.max(1) as usize;
    let (lines, selected) = body_lines(app.view(), now, width, &indicator);
    let window = area.height as usize;
    let offset = scroll_top(lines.len(), selected, window.max(1));
    Paragraph::new(lines).scroll((offset, 0))
}

/// The main entry point invoked by `App::run` via `terminal.draw`.
pub(crate) fn draw(f: &mut Frame, app: &App) {
    let area = f.area();
    let [title, summary, body, detail, help] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(area);

    f.render_widget(title_line(app), title);
    f.render_widget(summary_line(app), summary);
    f.render_widget(body_widget(app, body), body);
    f.render_widget(detail_line(app), detail);
    f.render_widget(help_line(app), help);
}
