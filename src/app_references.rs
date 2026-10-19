use std::{sync::Arc, time::Instant};

use super::{
    ActionIntent, App, AppCommand, InputMode, KeyCode, KeyEvent, KeyModifiers, ListMode, Overlay,
    ResourceTarget, SavedResourceList, SortField, UiFlags, ViewState,
};
use crate::{
    model::{ResourceData, ResourceRow, ResourceSnapshot, SourceState},
    source::{ReferenceScan, supports_references},
};

#[derive(Debug, Default)]
pub struct ReferencesState {
    id: u64,
    pub target: Option<ResourceTarget>,
    pub loading: bool,
    origin: Option<ViewState>,
    generation: u64,
    due: Option<Instant>,
    back: Option<Box<ReferenceBack>>,
}

#[derive(Debug)]
struct ReferenceBack {
    origin: ViewState,
    destination: ViewState,
    history_cursor: usize,
    snapshot: ResourceSnapshot,
    parent: Option<SavedResourceList>,
    mode: ListMode,
    query: String,
    sort_field: SortField,
    descending: bool,
    selected: Option<Arc<str>>,
    notice: Option<String>,
    faults_only: bool,
}

impl App {
    #[must_use]
    pub fn is_reference_view(&self) -> bool {
        matches!(self.list_mode, ListMode::References { .. })
    }

    pub(super) fn open_references(&mut self) -> AppCommand {
        if !self.is_resource_view() || !supports_references(&self.kind) {
            return AppCommand::None;
        }
        let Some(target) = self.selected_target() else {
            return AppCommand::None;
        };
        self.references.back = None;
        self.references.target = Some(target);
        self.references.origin = Some(self.current_view());
        self.load_references()
    }

    fn load_references(&mut self) -> AppCommand {
        let Some(target) = self.references.target.clone() else {
            return AppCommand::None;
        };
        if self.references.loading {
            return AppCommand::None;
        }
        self.references.id = self.references.id.wrapping_add(1);
        self.references.generation = self.generation;
        self.references.loading = true;
        self.notice = Some("Scanning references… Esc cancels".to_owned());
        AppCommand::Action(ActionIntent::LoadReferences {
            id: self.references.id,
            target,
        })
    }

    fn reference_scan_is_current(&self) -> bool {
        self.references.generation == self.generation
            && self.references.origin.as_ref() == Some(&self.current_view())
            && self.overlay == Overlay::None
            && self.input_mode == InputMode::Browse
            && (self.is_reference_view()
                || (self.is_resource_view() && self.selected_target() == self.references.target))
    }

    pub fn show_references(
        &mut self,
        id: u64,
        target: &ResourceTarget,
        result: Result<ReferenceScan, String>,
    ) {
        if id != self.references.id
            || self.references.target.as_ref() != Some(target)
            || !self.references.loading
        {
            return;
        }
        self.references.loading = false;
        self.references.due = Some(Instant::now() + self.refresh_interval());
        if !self.reference_scan_is_current() {
            self.invalidate_reference_scan();
            return;
        }
        let report = match result {
            Ok(report) => report,
            Err(error) => {
                self.show_transient_notice(format!("Reference scan failed: {error}"));
                return;
            }
        };
        let notice = if report.errors.is_empty() {
            None
        } else {
            Some(format!(
                "Partial results: {} references; {} scan error(s): {}",
                report.entries.len(),
                report.errors.len(),
                report.errors.join("; ")
            ))
        };
        if report.entries.is_empty() && !self.is_reference_view() {
            self.show_transient_notice(notice.unwrap_or_else(|| "No references found".to_owned()));
            return;
        }
        let rows = report
            .entries
            .into_iter()
            .map(|entry| {
                let target = entry.target;
                let namespace = target.namespace.as_deref().unwrap_or("-");
                let gvr = format!("{}/{}", target.kind.api_version, target.kind.plural);
                let mut row = ResourceRow::new(namespace, &target.name, "-", "-", &gvr, 0);
                row.key = Arc::from(format!("{gvr}/{namespace}/{}", target.name));
                row.data = ResourceData::Reference { kind: target.kind };
                Arc::new(row)
            })
            .collect();
        if self.is_reference_view() {
            let selected = self.selected_row().map(|row| Arc::clone(&row.key));
            self.snapshot.rows = Arc::new(rows);
            self.snapshot.revision = self.snapshot.revision.wrapping_add(1);
            self.snapshot.state = SourceState::Ready;
            self.rebuild_filter(selected.as_deref());
        } else {
            self.show_child_rows(
                ListMode::References {
                    title: format!(
                        "UsedBy {} {}/{}",
                        target.kind.kind,
                        target.namespace.as_deref().unwrap_or("cluster"),
                        target.name
                    ),
                },
                rows,
            );
        }
        self.notice = notice;
    }

    pub fn poll_view_refresh(&mut self) -> AppCommand {
        if !self.is_reference_view() {
            return self.poll_yaml_refresh();
        }
        if self.overlay == Overlay::None
            && self.input_mode == InputMode::Browse
            && !self.references.loading
            && self.references.due.is_some_and(|due| Instant::now() >= due)
        {
            self.load_references()
        } else {
            AppCommand::None
        }
    }

    pub fn cancel_references_before_command(
        &mut self,
        command: &AppCommand,
    ) -> Option<ActionIntent> {
        if !self.references.loading {
            return None;
        }
        let own_request = matches!(command, AppCommand::Action(ActionIntent::LoadReferences { id, target }) if *id == self.references.id && Some(target) == self.references.target.as_ref());
        if self.reference_scan_is_current() && (own_request || matches!(command, AppCommand::None))
        {
            return None;
        }
        self.invalidate_reference_scan();
        Some(ActionIntent::StopContentRefresh)
    }

    pub(super) fn invalidate_reference_scan(&mut self) {
        self.references.id = self.references.id.wrapping_add(1);
        self.references.loading = false;
        self.references.due = Some(Instant::now() + self.refresh_interval());
        if self.notice.as_deref().is_some_and(|message| {
            message.starts_with("Scanning references") || message == "Loading…"
        }) {
            self.notice = None;
        }
    }

    pub(super) fn references_view_requested(&mut self, view: &ViewState) {
        self.invalidate_reference_scan();
        if view.context != self.context {
            self.references = ReferencesState::default();
        }
    }

    pub(super) fn cancel_initial_reference_scan(&mut self) -> Option<AppCommand> {
        if !self.references.loading || self.is_reference_view() {
            return None;
        }
        self.invalidate_reference_scan();
        Some(AppCommand::Action(ActionIntent::StopContentRefresh))
    }

    pub(super) fn reference_key(&mut self, key: KeyEvent) -> Option<AppCommand> {
        if !self.is_reference_view() {
            return None;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            return match key.code {
                KeyCode::Char('r') => Some(self.load_references()),
                KeyCode::Char('s' | 'd' | 'k' | 'w' | 'l' | 'z' | ' ' | '\\') => {
                    Some(AppCommand::None)
                }
                _ => None,
            };
        }
        match key.code {
            KeyCode::Enter | KeyCode::Char('l') => Some(self.goto_reference()),
            KeyCode::Char('V') => {
                self.set_sort(SortField::Details);
                Some(AppCommand::None)
            }
            KeyCode::Esc | KeyCode::Char('h') => Some(self.leave_references()),
            KeyCode::Char(character) if character.is_ascii_digit() => Some(AppCommand::None),
            KeyCode::Char(
                'A' | 'S' | 'O' | ' ' | 'y' | 'd' | 'e' | 'i' | 'u' | 's' | 'r' | 't' | 'a' | 'p'
                | 'f' | 'w' | 'x' | 'o' | 'b' | 'z',
            ) => Some(AppCommand::None),
            _ => None,
        }
    }

    fn leave_references(&mut self) -> AppCommand {
        let loading = self.references.loading;
        self.invalidate_reference_scan();
        self.references.target = None;
        self.references.origin = None;
        self.references.back = None;
        self.notice = None;
        self.restore_resource_list();
        if loading {
            AppCommand::Action(ActionIntent::StopContentRefresh)
        } else {
            AppCommand::None
        }
    }

    fn goto_reference(&mut self) -> AppCommand {
        let Some(row) = self.selected_row() else {
            return AppCommand::None;
        };
        let ResourceData::Reference { kind } = &row.data else {
            return AppCommand::None;
        };
        let target = ResourceTarget {
            kind: kind.clone(),
            namespace: Some(row.namespace.to_string()),
            name: row.name.to_string(),
        };
        let destination = ViewState {
            instance: Some(target.clone()),
            context: self.context.clone(),
            kind: target.kind.clone(),
            namespace: target.namespace.clone(),
            field_selector: None,
            label_selector: None,
        };
        let back = ReferenceBack {
            origin: self.current_view(),
            destination: destination.clone(),
            history_cursor: self.history_cursor,
            snapshot: self.snapshot.clone(),
            parent: self.saved_resource_list.take(),
            mode: self.list_mode.clone(),
            query: self.query.clone(),
            sort_field: self.sort_field,
            descending: self.sort_descending,
            selected: self.selected_row().map(|row| Arc::clone(&row.key)),
            notice: self.notice.clone(),
            faults_only: self.ui.faults(),
        };
        let command = self.request_view(destination, true);
        self.sort_field = SortField::Name;
        self.sort_descending = false;
        if self.ui.faults() {
            self.ui.toggle(UiFlags::FAULTS);
        }
        self.references.back = Some(Box::new(back));
        command
    }

    pub(super) fn restore_references_on_escape(&mut self) -> Option<AppCommand> {
        if !self.is_resource_view() || !self.query.is_empty() {
            return None;
        }
        let back = self.references.back.as_ref()?;
        if back.destination != self.current_view() || self.history_cursor != back.history_cursor + 1
        {
            return None;
        }
        let back = *self.references.back.take()?;
        let command = self.request_view(back.origin, false);
        self.history_cursor = back.history_cursor;
        self.snapshot = back.snapshot;
        self.snapshot.generation = self.generation;
        self.saved_resource_list = back.parent;
        if let Some(parent) = self.saved_resource_list.as_mut() {
            parent.snapshot.generation = self.generation;
        }
        self.list_mode = back.mode;
        self.query = back.query;
        self.sort_field = back.sort_field;
        self.sort_descending = back.descending;
        if self.ui.faults() != back.faults_only {
            self.ui.toggle(UiFlags::FAULTS);
        }
        self.references.generation = self.generation;
        self.references.due = Some(Instant::now());
        self.rebuild_filter(back.selected.as_deref());
        self.notice = back.notice;
        Some(command)
    }
}

#[cfg(test)]
#[path = "app_references_tests.rs"]
mod tests;
