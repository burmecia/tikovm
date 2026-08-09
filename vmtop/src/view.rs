//! Pure rendering model built from a raw `Vec<Vm>` poll snapshot.
//!
//! Everything here is a plain function of the input snapshot plus display
//! options (group vs. flat, filter) — no terminal, no IO — so the grouping,
//! sorting, filtering, and selection logic is trivially unit-testable.
//! `View` owns the ordered visible rows and keeps selection stable across
//! refreshes by keying it on the VM id.

use std::collections::BTreeMap;

use crate::model::{Vm, VmState};

/// How a group's VMs are ordered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SortOrder {
    /// `Started` above paused/suspended above the rest; ties by name.
    State,
    /// Alphabetical by name.
    Name,
}

/// One row in the flat list the list renderer walks. Headers are rendered
/// but not selectable — navigation skips them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RowId {
    /// A project section header. The `usize` indexes `View::groups`.
    Group(usize),
    /// A VM row; indexes into `View::vms`.
    Vm(usize),
}

/// Per-state counters (plus the total) shown in the header and per project.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct StateCounts {
    pub total: usize,
    pub started: usize,
    pub paused: usize,
    pub suspended: usize,
    pub destroyed: usize,
    /// Everything else: transitional states plus `created`.
    pub other: usize,
}

/// One project's section header followed by its VM rows.
#[derive(Debug, Clone)]
pub(crate) struct Group {
    pub project_id: u64,
    pub counts: StateCounts,
    /// The project bridge subnet, taken from the first group VM that has one
    /// allocated — every VM in a project shares the same subnet.
    pub subnet: Option<String>,
}

/// Display/aggregation view over a VM snapshot.
#[derive(Debug, Clone)]
pub(crate) struct View {
    /// Raw inventory from the latest successful poll (unfiltered).
    all: Vec<Vm>,
    /// Filtered + sorted copy backing the rows.
    vms: Vec<Vm>,
    groups: Vec<Group>,
    /// Flattened display order: in grouped mode [Header, Vm, Vm, ...] per
    /// project; in flat mode just Vm rows.
    rows: Vec<RowId>,
    /// Selection keyed by VM id so it survives refreshes; `selected_row` is
    /// its position in `rows`.
    selected_id: Option<String>,
    selected_row: Option<usize>,
    /// Display mode.
    pub grouped: bool,
    /// Current sort.
    pub sort: SortOrder,
    /// Active substring filter; empty means show everything.
    pub filter: String,
}

impl Default for View {
    fn default() -> Self {
        Self::new(true, SortOrder::State)
    }
}

/// Display rank so running machines sort first.
fn state_rank(state: VmState) -> u8 {
    match state {
        VmState::Started => 0,
        VmState::Paused => 1,
        VmState::Suspended => 2,
        VmState::Created => 3,
        VmState::Destroyed => 5,
        _ => 4, // transitional (creating/starting/pausing/.../destroying)
    }
}

impl StateCounts {
    fn add(&mut self, state: VmState) {
        self.total += 1;
        match state {
            VmState::Started => self.started += 1,
            VmState::Paused => self.paused += 1,
            VmState::Suspended => self.suspended += 1,
            VmState::Destroyed => self.destroyed += 1,
            _ => self.other += 1,
        }
    }
}

/// Sort in place, using the VM name as the stable tie-breaker.
fn sort_vms(vms: &mut [Vm], order: SortOrder) {
    match order {
        SortOrder::State => vms.sort_by_key(|v| (state_rank(v.state), v.vm_config.name.clone())),
        SortOrder::Name => vms.sort_by(|a, b| a.vm_config.name.cmp(&b.vm_config.name)),
    }
}

impl View {
    pub(crate) fn new(grouped: bool, sort: SortOrder) -> Self {
        Self {
            all: Vec::new(),
            vms: Vec::new(),
            groups: Vec::new(),
            rows: Vec::new(),
            selected_id: None,
            selected_row: None,
            grouped,
            sort,
            filter: String::new(),
        }
    }

    /// Feed a fresh poll snapshot, reapplying filter/sort/group and
    /// re-pinning the selection to the same VM if it still exists.
    pub(crate) fn update(&mut self, vms: Vec<Vm>) {
        self.all = vms;
        self.rebuild();
    }

    fn rebuild(&mut self) {
        let mut filtered: Vec<Vm> = self
            .all
            .iter()
            .filter(|vm| self.filter.is_empty() || vm.matches(&self.filter))
            .cloned()
            .collect();
        sort_vms(&mut filtered, self.sort);
        self.vms = filtered;

        self.groups.clear();
        self.rows.clear();

        if self.grouped {
            let mut members: BTreeMap<u64, Vec<usize>> = BTreeMap::new();
            for (i, vm) in self.vms.iter().enumerate() {
                members.entry(vm.vm_config.project_id).or_default().push(i);
            }
            for (project_id, indices) in members {
                let first_vm = &self.vms[indices[0]];
                let subnet = first_vm.net.as_ref().map(|n| n.subnet.clone());
                let mut counts = StateCounts::default();
                for &i in &indices {
                    counts.add(self.vms[i].state);
                }
                self.groups.push(Group {
                    project_id,
                    counts,
                    subnet,
                });
                let gi = self.groups.len() - 1;
                self.rows.push(RowId::Group(gi));
                for i in indices {
                    self.rows.push(RowId::Vm(i));
                }
            }
        } else {
            for i in 0..self.vms.len() {
                self.rows.push(RowId::Vm(i));
            }
        }

        self.repin_selection();
    }

    /// Pick the selection position after a rebuild: same VM id if present,
    /// else the row closest to the previous position if it is still a VM
    /// row, else the first VM row.
    fn repin_selection(&mut self) {
        let vm_rows: Vec<usize> = self
            .rows
            .iter()
            .enumerate()
            .filter_map(|(idx, row)| matches!(row, RowId::Vm(_)).then_some(idx))
            .collect();
        if vm_rows.is_empty() {
            self.selected_row = None;
            return;
        }
        if let Some(id) = &self.selected_id {
            let found = self
                .rows
                .iter()
                .position(|row| matches!(row, RowId::Vm(i) if self.vms[*i].vm_id == *id));
            if let Some(row) = found {
                self.selected_row = Some(row);
                return;
            }
        }
        if let Some(prev) = self.selected_row
            && let Some(&row) = vm_rows.iter().find(|r| **r >= prev)
        {
            self.selected_row = Some(row);
            return;
        }
        self.selected_row = vm_rows.first().copied();
    }

    /// The row positions holding VM rows (used by navigation).
    fn vm_rows(&self) -> Vec<usize> {
        self.rows
            .iter()
            .enumerate()
            .filter_map(|(i, row)| matches!(row, RowId::Vm(_)).then_some(i))
            .collect()
    }

    /// Move the selection by `delta` VM rows (skipping group headers),
    /// clamped to the visible list.
    pub(crate) fn move_selected(&mut self, delta: isize) {
        let vm_rows = self.vm_rows();
        if vm_rows.is_empty() {
            return;
        }
        let cur = self
            .selected_row
            .and_then(|row| vm_rows.iter().position(|x| *x == row))
            .unwrap_or(0);
        let last = vm_rows.len() as isize - 1;
        let target = ((cur as isize) + delta).clamp(0, last);
        self.selected_row = Some(vm_rows[target as usize]);
        self.stamp_selected();
    }

    /// Page by `delta` VM-row steps (used for PgUp/PgDn).
    pub(crate) fn page_selected(&mut self, delta: isize) {
        self.move_selected(delta);
    }

    pub(crate) fn jump_first(&mut self) {
        if let Some(&row) = self.vm_rows().first() {
            self.selected_row = Some(row);
            self.stamp_selected();
        }
    }

    pub(crate) fn jump_last(&mut self) {
        if let Some(row) = self.vm_rows().last().copied() {
            self.selected_row = Some(row);
            self.stamp_selected();
        }
    }

    fn stamp_selected(&mut self) {
        self.selected_id = self.selected_vm().map(|v| v.vm_id.clone());
    }

    /// The currently selected VM row, if any.
    pub(crate) fn selected_vm(&self) -> Option<&Vm> {
        let row = self.selected_row?;
        match self.rows.get(row)? {
            RowId::Vm(i) => Some(&self.vms[*i]),
            RowId::Group(_) => None,
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.vms.is_empty()
    }

    #[allow(dead_code)]
    pub(crate) fn visible_count(&self) -> usize {
        self.vms.len()
    }

    #[allow(dead_code)]
    pub(crate) fn vms(&self) -> &[Vm] {
        &self.vms
    }

    pub(crate) fn all_vms(&self) -> &[Vm] {
        &self.all
    }

    #[allow(dead_code)]
    pub(crate) fn groups(&self) -> &[Group] {
        &self.groups
    }

    pub(crate) fn rows(&self) -> &[RowId] {
        &self.rows
    }

    pub(crate) fn group(&self, index: usize) -> &Group {
        &self.groups[index]
    }

    pub(crate) fn vm(&self, index: usize) -> &Vm {
        &self.vms[index]
    }

    /// Host-wide counters over the latest unfiltered snapshot.
    pub(crate) fn host_counts(&self) -> StateCounts {
        let mut c = StateCounts::default();
        for vm in &self.all {
            c.add(vm.state);
        }
        c
    }

    /// Number of distinct projects in the unfiltered snapshot.
    pub(crate) fn host_projects(&self) -> usize {
        let mut seen = std::collections::HashSet::new();
        for vm in &self.all {
            seen.insert(vm.vm_config.project_id);
        }
        seen.len()
    }

    /// Configured allocations across the unfiltered snapshot.
    pub(crate) fn host_alloc(&self) -> (u32, u64) {
        let mut cpu = 0;
        let mut mem = 0u64;
        for vm in &self.all {
            cpu += vm.vm_config.cpus;
            mem += u64::from(vm.vm_config.memory_mb);
        }
        (cpu, mem)
    }

    /// Replace the filter and rebuild; keeps the selection stable.
    pub(crate) fn set_filter(&mut self, filter: String) {
        if self.filter == filter {
            return;
        }
        self.filter = filter;
        self.rebuild();
    }

    pub(crate) fn clear_filter(&mut self) {
        if !self.filter.is_empty() {
            self.filter.clear();
            self.rebuild();
        }
    }

    pub(crate) fn toggle_grouped(&mut self) {
        self.set_grouped(!self.grouped);
    }

    pub(crate) fn set_grouped(&mut self, grouped: bool) {
        if self.grouped != grouped {
            self.grouped = grouped;
            self.rebuild();
        }
    }

    pub(crate) fn set_sort(&mut self, sort: SortOrder) {
        if self.sort != sort {
            self.sort = sort;
            self.rebuild();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::VmMode;
    use chrono::{TimeZone, Utc};

    fn vm(id: &str, project: u64, name: &str, state: VmState) -> Vm {
        Vm {
            vm_id: id.to_string(),
            state,
            net: None,
            snapshot: None,
            created_at: Utc.timestamp_opt(0, 0).unwrap(),
            vm_config: crate::model::VmConfig {
                name: name.to_string(),
                project_id: project,
                mode: VmMode::Ephemeral,
                image: "ubuntu-24".to_string(),
                cpus: 1,
                memory_mb: 512,
                disk_size_mb: 1024,
                tags: vec![],
                network_config: crate::model::NetworkConfig::default(),
                auto_suspend: None,
                block_storage: None,
            },
        }
    }

    #[test]
    fn groups_by_project_ordered() {
        let mut view = View::default();
        view.update(vec![
            vm("a1", 10, "z", VmState::Suspended),
            vm("b1", 1, "a", VmState::Started),
            vm("c1", 10, "a", VmState::Started),
            vm("d1", 7, "m", VmState::Paused),
        ]);
        assert_eq!(view.groups().len(), 3);
        assert_eq!(view.groups()[0].project_id, 1);
        assert_eq!(view.groups()[1].project_id, 7);
        assert_eq!(view.groups()[2].project_id, 10);
        // Within project 10, started sorts above suspended; ties resolve by name.
        let p10: Vec<&Vm> = view
            .vms()
            .iter()
            .filter(|v| v.vm_config.project_id == 10)
            .collect();
        assert_eq!(p10[0].vm_id, "c1");
        assert_eq!(p10[1].vm_id, "a1");
    }

    #[test]
    fn flat_mode_has_no_headers() {
        let mut view = View::new(false, SortOrder::State);
        view.update(vec![
            vm("a", 1, "b", VmState::Started),
            vm("b", 2, "a", VmState::Paused),
        ]);
        assert_eq!(view.rows().len(), 2);
        assert!(view.rows().iter().all(|r| matches!(r, RowId::Vm(_))));
    }

    #[test]
    fn host_counts_are_correct() {
        let mut view = View::default();
        view.update(vec![
            vm("a", 1, "a", VmState::Started),
            vm("b", 1, "b", VmState::Started),
            vm("c", 1, "c", VmState::Suspended),
            vm("d", 1, "d", VmState::Destroyed),
        ]);
        let h = view.host_counts();
        assert_eq!((h.total, h.started, h.suspended, h.destroyed), (4, 2, 1, 1));
        assert_eq!(view.host_alloc(), (4, 2048));
        assert_eq!(view.host_projects(), 1);
    }

    #[test]
    fn selection_skips_headers_and_repins_on_update() {
        let mut view = View::default();
        view.update(vec![
            vm("p1v1", 1, "a", VmState::Started),
            vm("p1v2", 1, "b", VmState::Started),
            vm("p2v1", 2, "c", VmState::Started),
        ]);
        view.jump_first();
        assert_eq!(view.selected_vm().unwrap().vm_id, "p1v1");
        view.move_selected(1);
        assert_eq!(view.selected_vm().unwrap().vm_id, "p1v2");
        // next jumps across the project-2 header to bump1.
        view.move_selected(1);
        assert_eq!(view.selected_vm().unwrap().vm_id, "p2v1");
        view.move_selected(1); // clamped at last
        assert_eq!(view.selected_vm().unwrap().vm_id, "p2v1");
        // Refresh keeps the selection on the same id even after reordering.
        view.update(vec![
            vm("p1v2", 1, "b", VmState::Paused),
            vm("p2v1", 2, "c", VmState::Started),
        ]);
        assert_eq!(view.selected_vm().unwrap().vm_id, "p2v1");
    }

    #[test]
    fn filter_hides_rows_and_sections() {
        let mut view = View::default();
        view.update(vec![
            vm("p1", 1, "web", VmState::Started),
            vm("p2", 2, "db", VmState::Started),
        ]);
        assert_eq!(view.visible_count(), 2);
        view.set_filter("web".to_string());
        assert_eq!(view.visible_count(), 1);
        assert_eq!(view.groups().len(), 1);
        assert_eq!(view.groups()[0].project_id, 1);
        view.clear_filter();
        assert_eq!(view.visible_count(), 2);
    }
}
