//! Application state, the background poller, and the interactive event loop.
//!
//! A dedicated tokio task polls `GET /api/vms` on a fixed cadence (or an
//! explicit nudge) and publishes the latest server state over a
//! `tokio::sync::watch` channel; the event loop re-renders from that snapshot
//! at a frame cadence, so a hanging hostd never stalls the UI. All screen and
//! terminal interaction lives here and in `ui`; `view` stays pure.

use std::io::Stdout;
use std::time::Duration;

use crossterm::ExecutableCommand;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use crossterm::terminal::{EnterAlternateScreen, enable_raw_mode};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use tokio::sync::{mpsc, watch};

use crate::api::ApiClient;
use crate::error::{Error, Result};
use crate::model::Vm;
use crate::view::{SortOrder, View};

/// Interactivity input modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FilterMode {
    /// Normal browsing; key bindings are active.
    Normal,
    /// The `filter:` prompt is open; typed characters edit the needle.
    Filtering,
}

/// Current poll outcome, published to the event loop.
#[derive(Debug, Clone)]
pub(crate) struct Snapshot {
    /// Last successfully fetched inventory (kept across failed polls so the
    /// screen does not blank out when hostd restarts).
    pub vms: Option<Vec<Vm>>,
    /// Wall time of the last successful poll.
    pub last_ok: Option<chrono::DateTime<chrono::Utc>>,
    /// Wall time of the most recent poll attempt.
    pub last_attempt: Option<chrono::DateTime<chrono::Utc>>,
    /// Textual reason the latest poll failed, if it did.
    pub error: Option<String>,
}

impl Snapshot {
    pub(crate) fn fresh() -> Self {
        Self {
            vms: None,
            last_ok: None,
            last_attempt: None,
            error: None,
        }
    }

    /// True when the last poll succeeded and data is present.
    pub(crate) fn connected(&self) -> bool {
        self.vms.is_some() && self.error.is_none()
    }
}

/// The whole interactive app: watch receiver, view, and input state.
pub(crate) struct App {
    interval: Duration,
    base_url: String,
    trigger: mpsc::UnboundedSender<()>,
    rx: watch::Receiver<Snapshot>,
    snap: Snapshot,
    /// The rendering model; updated only when fresh data arrives.
    view: View,
    mode: FilterMode,
    filter: String,
    quit: bool,
}

/// How long to sit between redraws. Polls are much slower (1s).
const FRAME_MS: u64 = 100;

/// Frame cadence, and the nudge channel capacity.
const FRAME_DUR: Duration = Duration::from_millis(FRAME_MS);

impl App {
    pub(crate) fn new(
        base_url: String,
        interval: Duration,
        trigger: mpsc::UnboundedSender<()>,
        rx: watch::Receiver<Snapshot>,
        grouped: bool,
        sort: SortOrder,
    ) -> Self {
        Self {
            interval,
            base_url,
            trigger,
            rx,
            snap: Snapshot::fresh(),
            view: View::new(grouped, sort),
            mode: FilterMode::Normal,
            filter: String::new(),
            quit: false,
        }
    }

    /// The API base, without a scheme, for the header.
    pub(crate) fn host_disp(&self) -> String {
        self.base_url
            .trim_start_matches("http://")
            .trim_start_matches("https://")
            .trim_end_matches('/')
            .to_string()
    }

    /// Reflect the latest published snapshot into the view model.
    fn refresh(&mut self) {
        {
            let snap = self.rx.borrow_and_update();
            self.snap = snap.clone();
        }
        if let Some(vms) = &self.snap.vms {
            self.view.update(vms.clone());
        }
    }

    /// Run the interactive main loop until the user quits.
    pub(crate) fn run(&mut self, terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
        let mut last_frame = std::time::Instant::now();
        self.refresh();
        while !self.quit {
            if event::poll(Duration::from_millis(10))?
                && let Event::Key(key) = event::read()?
            {
                self.on_key(key)?;
            }
            // Consume any fast-following key events in batches.
            while let Ok(true) = event::poll(Duration::from_millis(1)) {
                if let Event::Key(key) = event::read()? {
                    self.on_key(key)?;
                }
            }
            self.refresh();
            if last_frame.elapsed() >= FRAME_DUR {
                terminal.draw(|frame| crate::ui::draw(frame, self))?;
                last_frame = std::time::Instant::now();
            }
        }
        Ok(())
    }

    /// Handle a single key, updating `quit` / mode / selection as needed.
    fn on_key(&mut self, key: KeyEvent) -> Result<()> {
        if key.code == KeyCode::Esc && self.mode == FilterMode::Filtering {
            self.mode = FilterMode::Normal;
            self.filter.clear();
            self.view.clear_filter();
            return Ok(());
        }
        match self.mode {
            FilterMode::Filtering => match key.code {
                KeyCode::Char(c) => {
                    self.filter.push(c);
                    self.view.set_filter(self.filter.clone());
                }
                KeyCode::Backspace => {
                    self.filter.pop();
                    self.view.set_filter(self.filter.clone());
                }
                KeyCode::Enter => {
                    self.mode = FilterMode::Normal;
                }
                _ => {}
            },
            FilterMode::Normal => match key.code {
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.quit = true;
                }
                KeyCode::Char('q') => self.quit = true,
                KeyCode::Down | KeyCode::Char('j') => self.view.move_selected(1),
                KeyCode::Up | KeyCode::Char('k') => self.view.move_selected(-1),
                KeyCode::PageDown => self.view.page_selected(5),
                KeyCode::PageUp => self.view.page_selected(-5),
                KeyCode::Home | KeyCode::Char('g') => self.view.jump_first(),
                KeyCode::End | KeyCode::Char('G') => self.view.jump_last(),
                KeyCode::Char('f') => self.view.toggle_grouped(),
                KeyCode::Char('s') => self.view.set_sort(match self.view.sort {
                    SortOrder::State => SortOrder::Name,
                    SortOrder::Name => SortOrder::State,
                }),
                KeyCode::Char('/') => {
                    self.filter.clear();
                    self.mode = FilterMode::Filtering;
                }
                KeyCode::Char('r') => {
                    let _ = self.trigger.send(());
                }
                _ => {}
            },
        }
        Ok(())
    }

    pub(crate) fn interval(&self) -> Duration {
        self.interval
    }

    pub(crate) fn snap(&self) -> &Snapshot {
        &self.snap
    }

    pub(crate) fn view(&self) -> &View {
        &self.view
    }

    pub(crate) fn filter_mode(&self) -> FilterMode {
        self.mode
    }

    pub(crate) fn filter(&self) -> &str {
        &self.filter
    }
}

/// The background poll task: fetch on every tick of `interval` plus each
/// explicit nudge received on `trigger`.
pub(crate) async fn poll_loop(
    client: ApiClient,
    interval: Duration,
    trigger: mpsc::UnboundedReceiver<()>,
    tx: watch::Sender<Snapshot>,
) {
    let mut ticker = tokio::time::interval(interval);
    let mut trigger = trigger;
    loop {
        tokio::select! {
            _ = ticker.tick() => {}
            _ = trigger.recv() => {}
        }
        let now = chrono::Utc::now();
        let outcome = client.list_vms().await;
        tx.send_modify(|snap| {
            snap.last_attempt = Some(now);
            match outcome {
                Ok(vms) => {
                    snap.vms = Some(vms);
                    snap.last_ok = Some(now);
                    snap.error = None;
                }
                Err(err) => snap.error = Some(err.to_string()),
            }
        });
    }
}

/// Enter raw mode + the alternate screen; wrap stdout in a ratatui terminal.
pub(crate) fn init_terminal() -> Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode()?;
    std::io::stdout().execute(EnterAlternateScreen)?;
    Terminal::new(CrosstermBackend::new(std::io::stdout())).map_err(Error::Io)
}

/// Restore the terminal to its pre-app state.
pub(crate) fn restore_terminal() -> Result<()> {
    use crossterm::terminal::{LeaveAlternateScreen, disable_raw_mode};
    std::io::stdout().execute(LeaveAlternateScreen)?;
    disable_raw_mode()?;
    Ok(())
}
