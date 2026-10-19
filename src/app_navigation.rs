use std::{
    cmp::Ordering,
    collections::{BTreeMap, HashSet},
    sync::Arc,
};

use super::{
    ActionIntent, App, AppCommand, InputMode, Overlay, ResourceDescriptor, ResourceKind,
    ResourceTarget, SortField, UiFlags, ViewState,
};
use crate::model::{ResourceOperations, ResourceRow, ResourceSnapshot, SourceState};
use crate::source::BatchNavigationFilter;

#[derive(Debug, Default)]
pub struct NavigationState {
    id: u64,
    pending: Option<PendingNavigation>,
    workload_history: BTreeMap<usize, WorkloadHistory>,
    restored_selection: Option<(u64, Arc<str>)>,
}

#[derive(Debug, Clone)]
struct WorkloadHistory {
    view: ViewState,
    snapshot: ResourceSnapshot,
    query: String,
    sort: SortField,
    descending: bool,
    selected: Option<Arc<str>>,
    marks: HashSet<Arc<str>>,
    mark_anchor: Option<usize>,
    selected_column: usize,
    offset: usize,
    horizontal_scroll: u16,
    faults: bool,
}

#[derive(Debug)]
struct PendingNavigation {
    target: ResourceTarget,
    mode: NavigationMode,
    view: ViewState,
    generation: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NavigationMode {
    Deployment { replica_sets: bool },
    Batch,
}

impl App {
    fn workload_sort_columns(&self) -> Vec<SortField> {
        let mut columns = vec![
            SortField::Kind,
            SortField::Namespace,
            SortField::Name,
            SortField::Status,
            SortField::Ready,
        ];
        if self.ui.wide() {
            columns.push(SortField::Validity);
        }
        columns.push(SortField::Age);
        columns
    }

    pub(super) fn select_workload_sort_column(&mut self, field: SortField) {
        if !self.kind.is_workloads() {
            return;
        }
        let field = if self.workload_sort_columns().contains(&field) {
            field
        } else {
            SortField::Kind
        };
        self.selected_column = SortField::ALL
            .iter()
            .position(|value| *value == field)
            .unwrap_or(0);
    }

    pub(super) fn move_workload_sort_column(&mut self, forward: bool) {
        let columns = self.workload_sort_columns();
        let selected = SortField::ALL[self.selected_column];
        let index = columns
            .iter()
            .position(|field| *field == selected)
            .unwrap_or(0);
        let next = if forward {
            (index + 1) % columns.len()
        } else {
            (index + columns.len() - 1) % columns.len()
        };
        self.select_workload_sort_column(columns[next]);
    }

    pub(super) fn open_workload_instance(&mut self) -> AppCommand {
        let Some(target) = self.selected_target() else {
            return AppCommand::None;
        };
        if target.kind.is_workloads() || target.namespace.as_deref().is_none_or(str::is_empty) {
            return self.unsupported("Workload identity is unavailable; refresh and retry");
        }
        let command = self.request_view(
            ViewState {
                context: self.context.clone(),
                kind: target.kind.clone(),
                namespace: self.namespace.clone(),
                instance: Some(target),
                field_selector: None,
                label_selector: None,
            },
            true,
        );
        self.query.clear();
        self.sort_field = SortField::Name;
        self.sort_descending = false;
        self.selected_column = SortField::ALL
            .iter()
            .position(|field| *field == SortField::Name)
            .unwrap_or(0);
        self.ui.set(UiFlags::FAULTS, false);
        command
    }

    pub(super) fn save_workloads_history(&mut self) {
        if !self.kind.is_workloads() || !self.is_resource_view() {
            return;
        }
        self.navigation.workload_history.insert(
            self.history_cursor,
            WorkloadHistory {
                view: self.current_view(),
                snapshot: self.snapshot.clone(),
                query: self.query.clone(),
                sort: self.sort_field,
                descending: self.sort_descending,
                selected: self.selected_row().map(|row| Arc::clone(&row.key)),
                marks: self.marks.clone(),
                mark_anchor: self.mark_anchor,
                selected_column: self.selected_column,
                offset: self.offset,
                horizontal_scroll: self.list_horizontal_scroll,
                faults: self.ui.faults(),
            },
        );
    }

    pub(super) fn truncate_workloads_history(&mut self) {
        self.navigation
            .workload_history
            .retain(|index, _| *index <= self.history_cursor);
    }

    pub(super) fn restore_workloads_history(&mut self) {
        let Some(saved) = self
            .navigation
            .workload_history
            .get(&self.history_cursor)
            .cloned()
        else {
            return;
        };
        if saved.view != self.current_view() {
            return;
        }
        self.snapshot = saved.snapshot;
        self.snapshot.generation = self.generation;
        self.query = saved.query;
        self.sort_field = saved.sort;
        self.sort_descending = saved.descending;
        self.marks = saved.marks;
        self.mark_anchor = saved.mark_anchor;
        self.selected_column = saved.selected_column;
        self.select_workload_sort_column(SortField::ALL[self.selected_column]);
        self.offset = saved.offset;
        self.list_horizontal_scroll = saved.horizontal_scroll;
        self.ui.set(UiFlags::FAULTS, saved.faults);
        self.rebuild_filter(saved.selected.as_deref());
        self.navigation.restored_selection = saved.selected.map(|key| (self.generation, key));
    }

    pub(super) fn clear_restored_workload_selection(&mut self) {
        self.navigation.restored_selection = None;
    }

    pub(super) fn restored_workload_selection(
        &mut self,
        snapshot: &ResourceSnapshot,
    ) -> Option<Arc<str>> {
        let (generation, key) = self.navigation.restored_selection.as_ref()?;
        if *generation != snapshot.generation || !snapshot.kind.is_workloads() {
            self.navigation.restored_selection = None;
            return None;
        }
        let key = Arc::clone(key);
        if matches!(snapshot.state, SourceState::Ready | SourceState::Error(_)) {
            self.navigation.restored_selection = None;
        }
        Some(key)
    }

    pub(super) fn open_deployment_pods(&mut self) -> AppCommand {
        self.open_deployment_children(false)
    }

    pub(super) fn open_deployment_replica_sets(&mut self) -> AppCommand {
        self.open_deployment_children(true)
    }

    fn open_deployment_children(&mut self, replica_sets: bool) -> AppCommand {
        if !self.is_resource_view() || self.kind.known_kind() != Some(ResourceKind::Deployments) {
            return AppCommand::None;
        }
        let Some(target) = self.selected_target() else {
            return AppCommand::None;
        };
        if target.namespace.as_deref().is_none_or(str::is_empty) {
            return self.unsupported("Deployment namespace is unavailable; refresh and retry");
        }
        let selector = self
            .selected_row()
            .and_then(|row| row.child_resource_data())
            .map(|data| data.label_selector.trim())
            .filter(|selector| usable_selector(selector))
            .map(str::to_owned);
        self.clear_pending_navigation();
        if let Some(selector) = selector {
            return self.open_deployment_selector(&target, replica_sets, selector);
        }
        self.navigation.pending = Some(PendingNavigation {
            target: target.clone(),
            mode: NavigationMode::Deployment { replica_sets },
            view: self.current_view(),
            generation: self.generation,
        });
        self.show_transient_notice("Loading Deployment selector… Esc cancels");
        AppCommand::Action(ActionIntent::LoadDeploymentSelector {
            id: self.navigation.id,
            target,
            replica_sets,
        })
    }

    pub(super) fn open_batch_children(&mut self) -> AppCommand {
        if !self.is_resource_view() || !(self.kind.is_cron_job() || self.kind.is_job()) {
            return AppCommand::None;
        }
        let Some(target) = self.selected_target() else {
            return AppCommand::None;
        };
        if target.namespace.as_deref().is_none_or(str::is_empty) {
            return self.unsupported(&format!(
                "{} namespace is unavailable; refresh and retry",
                target.kind.kind
            ));
        }
        self.clear_pending_navigation();
        self.navigation.pending = Some(PendingNavigation {
            target: target.clone(),
            mode: NavigationMode::Batch,
            view: self.current_view(),
            generation: self.generation,
        });
        self.show_transient_notice(format!(
            "Loading {} children… Esc cancels",
            target.kind.kind
        ));
        AppCommand::Action(ActionIntent::LoadBatchNavigation {
            id: self.navigation.id,
            target,
        })
    }

    pub fn show_batch_navigation(
        &mut self,
        id: u64,
        target: &ResourceTarget,
        result: Result<BatchNavigationFilter, String>,
    ) -> AppCommand {
        let Some(pending) = self.navigation.pending.as_ref() else {
            return AppCommand::None;
        };
        if id != self.navigation.id
            || &pending.target != target
            || pending.mode != NavigationMode::Batch
        {
            return AppCommand::None;
        }
        let current = self.navigation_is_current(pending);
        self.clear_pending_navigation();
        if !current {
            return AppCommand::None;
        }
        let (kind, field_selector, label_selector) = match result {
            Ok(BatchNavigationFilter::CronJobJobs { owner_name })
                if target.kind.is_cron_job() && owner_name == target.name =>
            {
                let kind = self
                    .resource_options
                    .iter()
                    .find(|resource| resource.is_job())
                    .cloned()
                    .unwrap_or_else(job_descriptor);
                (kind, Some(format!("kux.ownerName={owner_name}")), None)
            }
            Ok(BatchNavigationFilter::JobPods { label_selector }) if target.kind.is_job() => {
                let field_selector = label_selector
                    .is_none()
                    .then(|| "kux.matchNone=true".to_owned());
                (ResourceKind::Pods.into(), field_selector, label_selector)
            }
            Ok(_) => {
                return self.unsupported("Child navigation returned an unexpected resource filter");
            }
            Err(error) => {
                self.show_transient_notice(error);
                return AppCommand::None;
            }
        };
        let command = self.request_view(
            ViewState {
                instance: None,
                context: self.context.clone(),
                kind,
                namespace: target.namespace.clone(),
                field_selector,
                label_selector,
            },
            true,
        );
        if target.kind.is_cron_job() {
            self.sort_field = SortField::Age;
            self.sort_descending = true;
        }
        command
    }

    fn open_deployment_selector(
        &mut self,
        target: &ResourceTarget,
        replica_sets: bool,
        selector: String,
    ) -> AppCommand {
        let kind = if replica_sets {
            self.resource_options
                .iter()
                .find(|resource| resource.is_apps_v1_replica_set())
                .cloned()
                .unwrap_or_else(replica_set_descriptor)
        } else {
            ResourceKind::Pods.into()
        };
        self.request_view(
            ViewState {
                instance: None,
                context: self.context.clone(),
                kind,
                namespace: target.namespace.clone(),
                field_selector: None,
                label_selector: Some(selector),
            },
            true,
        )
    }

    fn navigation_is_current(&self, pending: &PendingNavigation) -> bool {
        pending.generation == self.generation
            && pending.view == self.current_view()
            && self.is_resource_view()
            && self.overlay == Overlay::None
            && self.input_mode == InputMode::Browse
            && self.selected_target().as_ref() == Some(&pending.target)
    }

    pub fn show_deployment_selector(
        &mut self,
        id: u64,
        target: &ResourceTarget,
        replica_sets: bool,
        result: Result<String, String>,
    ) -> AppCommand {
        let Some(pending) = self.navigation.pending.as_ref() else {
            return AppCommand::None;
        };
        if id != self.navigation.id
            || &pending.target != target
            || pending.mode != (NavigationMode::Deployment { replica_sets })
        {
            return AppCommand::None;
        }
        let current = self.navigation_is_current(pending);
        self.clear_pending_navigation();
        if !current {
            return AppCommand::None;
        }
        match result {
            Ok(selector) if usable_selector(&selector) => {
                self.open_deployment_selector(target, replica_sets, selector)
            }
            Ok(_) => {
                self.show_transient_notice(
                    "Deployment has no usable selector; child navigation was canceled",
                );
                AppCommand::None
            }
            Err(error) => {
                self.show_transient_notice(error);
                AppCommand::None
            }
        }
    }

    pub(super) fn clear_pending_navigation(&mut self) {
        self.navigation.id = self.navigation.id.wrapping_add(1);
        if self.navigation.pending.take().is_some()
            && self
                .notice
                .as_deref()
                .is_some_and(|notice| notice.starts_with("Loading"))
        {
            self.notice = None;
        }
    }

    pub(super) fn cancel_pending_navigation(&mut self) -> Option<AppCommand> {
        self.navigation.pending.as_ref()?;
        self.clear_pending_navigation();
        self.notice = None;
        Some(AppCommand::Action(ActionIntent::StopContentRefresh))
    }

    pub fn cancel_navigation_before_command(
        &mut self,
        command: &AppCommand,
    ) -> Option<ActionIntent> {
        let pending = self.navigation.pending.as_ref()?;
        let current_load = match command {
            AppCommand::Action(ActionIntent::LoadDeploymentSelector {
                id,
                target,
                replica_sets,
            }) => {
                *id == self.navigation.id
                    && target == &pending.target
                    && pending.mode
                        == (NavigationMode::Deployment {
                            replica_sets: *replica_sets,
                        })
            }
            AppCommand::Action(ActionIntent::LoadBatchNavigation { id, target }) => {
                *id == self.navigation.id
                    && target == &pending.target
                    && pending.mode == NavigationMode::Batch
            }
            _ => false,
        };
        if self.navigation_is_current(pending)
            && (current_load || matches!(command, AppCommand::None))
        {
            return None;
        }
        self.clear_pending_navigation();
        Some(ActionIntent::StopContentRefresh)
    }
}

pub(super) fn compare_workload_rows(
    left: &ResourceRow,
    right: &ResourceRow,
    field: SortField,
    now: i64,
) -> Ordering {
    let primary = match field {
        SortField::Ready => natural_compare(&left.ready, &right.ready),
        SortField::Age => {
            let age = |row: &ResourceRow| {
                if row.created_unix == 0 {
                    0
                } else {
                    row.age_seconds(now)
                }
            };
            age(left).cmp(&age(right))
        }
        _ => return super::compare_rows(left, right, field),
    };
    primary.then_with(|| natural_compare(&left.key, &right.key))
}

fn natural_compare(left: &str, right: &str) -> Ordering {
    let (mut left, mut right) = (left.as_bytes(), right.as_bytes());
    while let (Some(&a), Some(&b)) = (left.first(), right.first()) {
        let order = match (a.is_ascii_digit(), b.is_ascii_digit()) {
            (true, false) => return Ordering::Less,
            (false, true) => return Ordering::Greater,
            (false, false) => {
                left = &left[1..];
                right = &right[1..];
                a.cmp(&b)
            }
            (true, true) => {
                let (a, tail_a) = digit_run(left);
                let (b, tail_b) = digit_run(right);
                left = tail_a;
                right = tail_b;
                let significant_a =
                    &a[a.iter().position(|byte| *byte != b'0').unwrap_or(a.len())..];
                let significant_b =
                    &b[b.iter().position(|byte| *byte != b'0').unwrap_or(b.len())..];
                significant_a
                    .len()
                    .cmp(&significant_b.len())
                    .then_with(|| significant_a.cmp(significant_b))
                    .then(a.len().cmp(&b.len()))
            }
        };
        if order != Ordering::Equal {
            return order;
        }
    }
    left.len().cmp(&right.len())
}

fn digit_run(value: &[u8]) -> (&[u8], &[u8]) {
    value.split_at(
        value
            .iter()
            .take_while(|byte| byte.is_ascii_digit())
            .count(),
    )
}

fn usable_selector(selector: &str) -> bool {
    let selector = selector.trim();
    !selector.is_empty() && selector != "<none>" && selector != "-"
}

fn replica_set_descriptor() -> ResourceDescriptor {
    ResourceDescriptor::new(
        "apps",
        "v1",
        "apps/v1",
        "ReplicaSet",
        "replicasets",
        true,
        ResourceOperations::all(),
    )
    .with_short_names(["rs"])
}

fn job_descriptor() -> ResourceDescriptor {
    ResourceDescriptor::new(
        "batch",
        "v1",
        "batch/v1",
        "Job",
        "jobs",
        true,
        ResourceOperations::all(),
    )
}

#[cfg(test)]
#[path = "app_batch_navigation_tests.rs"]
mod batch_tests;

#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Instant};

    use super::*;
    use crate::{
        app::{KeyCode, KeyEvent, KeyModifiers, ListMode},
        model::{ChildResourceData, ResourceData, ResourceRow, ResourceSnapshot, SourceState},
    };

    fn row(namespace: &str, name: &str, selector: &str) -> ResourceRow {
        ResourceRow::new(namespace, name, "Ready", "1/1", "", 0).with_child_resource_data(
            ChildResourceData {
                kind: ResourceKind::Pods.into(),
                label_selector: Arc::from(selector),
            },
        )
    }

    fn app(rows: Vec<ResourceRow>) -> App {
        let mut app = App::new(
            "selected-context",
            None,
            ResourceKind::Deployments,
            false,
            vec!["selected-context".to_owned()],
        );
        app.field_selector = Some("metadata.name=api".to_owned());
        app.label_selector = Some("environment=prod".to_owned());
        app.history[0] = app.current_view();
        app.apply_snapshot(ResourceSnapshot {
            generation: app.generation,
            kind: app.kind.clone(),
            rows: Arc::new(rows.into_iter().map(Arc::new).collect()),
            table_columns: Arc::from([]),
            state: SourceState::Ready,
            revision: 1,
            emitted_at: Instant::now(),
        });
        app
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn deployment_enter_and_z_use_selected_namespace_selector_and_history() {
        for replicas in [false, true] {
            let mut app = app(vec![row("team", "api", "app=api,tier in (web,worker)")]);
            let original = app.current_view();
            app.query = "api".to_owned();
            let AppCommand::Request(view) = app.handle_key(key(if replicas {
                KeyCode::Char('z')
            } else {
                KeyCode::Enter
            })) else {
                panic!("Deployment key must request its selector-based child view");
            };
            assert_eq!(view.context, "selected-context");
            assert_eq!(view.namespace.as_deref(), Some("team"));
            assert_eq!(view.field_selector, None);
            assert_eq!(
                view.label_selector.as_deref(),
                Some("app=api,tier in (web,worker)")
            );
            if replicas {
                assert!(view.kind.is_apps_v1_replica_set());
            } else {
                assert_eq!(view.kind.known_kind(), Some(ResourceKind::Pods));
            }
            assert!(
                app.query.is_empty(),
                "parent text filter must not hide child rows"
            );
            assert_eq!(app.history, [original.clone(), view.clone()]);
            assert_eq!(
                app.handle_key(key(KeyCode::Esc)),
                AppCommand::Request(original)
            );
            assert_eq!(
                app.handle_key(key(KeyCode::Char(']'))),
                AppCommand::Request(view)
            );
        }
    }

    #[test]
    fn navigation_uses_selection_instead_of_marked_deployments() {
        let mut app = app(vec![
            row("a-team", "selected", "app=selected"),
            row("z-team", "z-marked", "app=marked"),
        ]);
        app.marks.insert(Arc::from("z-team/z-marked"));
        let AppCommand::Request(view) = app.open_deployment_replica_sets() else {
            panic!("selected deployment");
        };
        assert_eq!(view.namespace.as_deref(), Some("a-team"));
        assert_eq!(view.label_selector.as_deref(), Some("app=selected"));
    }

    #[test]
    fn missing_selection_or_namespace_never_requests_an_unfiltered_view() {
        for rows in [Vec::new(), vec![row("-", "api", "app=api")]] {
            for replicas in [false, true] {
                let mut app = app(rows.clone());
                let original = app.current_view();
                let command = if replicas {
                    app.open_deployment_replica_sets()
                } else {
                    app.open_deployment_pods()
                };
                assert_eq!(command, AppCommand::None);
                assert_eq!(app.current_view(), original);
                assert_eq!(app.history.len(), 1);
            }
        }
    }

    fn pending(app: &mut App, replica_sets: bool) -> (u64, ResourceTarget) {
        let command = if replica_sets {
            app.open_deployment_replica_sets()
        } else {
            app.open_deployment_pods()
        };
        let AppCommand::Action(ActionIntent::LoadDeploymentSelector {
            id,
            target,
            replica_sets: requested_mode,
        }) = command
        else {
            panic!("load missing selector");
        };
        assert_eq!(requested_mode, replica_sets);
        (id, target)
    }

    #[test]
    fn missing_compact_selector_fetches_without_leaving_then_opens_the_requested_children() {
        for replica_sets in [false, true] {
            for missing in [None, Some(""), Some(" "), Some("<none>")] {
                let mut resource = row("team", "api", missing.unwrap_or_default());
                if missing.is_none() {
                    resource.data = ResourceData::Generic;
                }
                let mut app = app(vec![resource]);
                let original = app.current_view();
                let (id, target) = pending(&mut app, replica_sets);
                assert_eq!(app.current_view(), original);
                assert_eq!(app.history.len(), 1);
                let AppCommand::Request(view) = app.show_deployment_selector(
                    id,
                    &target,
                    replica_sets,
                    Ok("app=api,tier in (web,worker)".to_owned()),
                ) else {
                    panic!("selector result opens child view");
                };
                assert_eq!(view.namespace.as_deref(), Some("team"));
                assert_eq!(
                    view.label_selector.as_deref(),
                    Some("app=api,tier in (web,worker)")
                );
                assert_eq!(view.kind.is_apps_v1_replica_set(), replica_sets);
                assert_eq!(app.history, [original, view]);
            }
        }
    }

    #[test]
    fn canceled_replaced_or_wrong_identity_results_cannot_navigate() {
        let mut app = app(vec![row("team", "api", "")]);
        let (old_id, target) = pending(&mut app, false);
        assert_eq!(
            app.handle_key(key(KeyCode::Esc)),
            AppCommand::Action(ActionIntent::StopContentRefresh)
        );
        assert_eq!(
            app.show_deployment_selector(old_id, &target, false, Ok("app=old".to_owned())),
            AppCommand::None
        );
        let (id, _) = pending(&mut app, true);
        for (response_id, response_target, mode) in [
            (old_id, target.clone(), false),
            (id, target.clone(), false),
            (
                id,
                ResourceTarget {
                    name: "other".to_owned(),
                    ..target.clone()
                },
                true,
            ),
        ] {
            assert_eq!(
                app.show_deployment_selector(
                    response_id,
                    &response_target,
                    mode,
                    Err("stale failure".to_owned())
                ),
                AppCommand::None
            );
            assert!(app.navigation.pending.is_some());
        }
        assert!(matches!(
            app.show_deployment_selector(id, &target, true, Ok("app=current".to_owned())),
            AppCommand::Request(_)
        ));
    }

    #[test]
    fn errors_and_blank_selectors_settle_loading_without_changing_views() {
        for result in [
            Err("forbidden".to_owned()),
            Ok(String::new()),
            Ok("<none>".to_owned()),
        ] {
            let mut app = app(vec![row("team", "api", "")]);
            let original = app.current_view();
            let (id, target) = pending(&mut app, false);
            assert_eq!(
                app.show_deployment_selector(id, &target, false, result),
                AppCommand::None
            );
            assert_eq!(app.current_view(), original);
            assert!(app.navigation.pending.is_none());
            assert!(app.notice.is_some());
        }
    }

    #[test]
    fn intervening_context_view_selection_overlay_and_input_reject_late_navigation() {
        for change in 0..6 {
            let mut app = app(vec![row("team", "api", ""), row("team", "other", "")]);
            let (id, target) = pending(&mut app, false);
            match change {
                0 => app.context = "new".to_owned(),
                1 => app.generation += 1,
                2 => app.overlay = Overlay::Help,
                3 => app.input_mode = InputMode::Command,
                4 => app.selected = 1,
                _ => app.label_selector = Some("new=selector".to_owned()),
            }
            assert_eq!(
                app.show_deployment_selector(id, &target, false, Ok("app=api".to_owned())),
                AppCommand::None
            );
            assert!(app.navigation.pending.is_none());
        }
    }

    #[test]
    fn command_cancellation_preserves_newer_action_and_does_not_cancel_its_own_load() {
        let mut app = app(vec![row("team", "api", "")]);
        let (id, target) = pending(&mut app, false);
        let own = AppCommand::Action(ActionIntent::LoadDeploymentSelector {
            id,
            target: target.clone(),
            replica_sets: false,
        });
        assert_eq!(app.cancel_navigation_before_command(&own), None);
        assert_eq!(
            app.cancel_navigation_before_command(&AppCommand::None),
            None
        );
        app.overlay = Overlay::Help;
        assert_eq!(
            app.cancel_navigation_before_command(&AppCommand::None),
            Some(ActionIntent::StopContentRefresh)
        );
        app.overlay = Overlay::None;
        let (id, target) = pending(&mut app, true);
        let newer = AppCommand::Action(ActionIntent::LoadImages {
            id: 99,
            target: target.clone(),
        });
        assert_eq!(
            app.cancel_navigation_before_command(&newer),
            Some(ActionIntent::StopContentRefresh)
        );
        assert_eq!(
            app.show_deployment_selector(id, &target, true, Ok("app=api".to_owned())),
            AppCommand::None
        );
    }

    #[test]
    fn readonly_navigation_is_allowed_and_other_resource_views_are_unchanged() {
        let mut app = app(vec![row("team", "api", "app=api")]);
        app.read_only = true;
        assert!(matches!(
            app.open_deployment_replica_sets(),
            AppCommand::Request(_)
        ));
        assert_eq!(app.open_deployment_pods(), AppCommand::None);
        app.kind = ResourceKind::Deployments.into();
        app.list_mode = ListMode::Containers {
            namespace: "team".to_owned(),
            pod: "api".to_owned(),
        };
        assert_eq!(app.open_deployment_replica_sets(), AppCommand::None);
    }

    #[test]
    fn replica_set_enter_still_opens_pods_with_its_own_selector() {
        let mut app = app(vec![row("team", "api-abc", "pod-template-hash=abc")]);
        app.kind = replica_set_descriptor();
        app.snapshot.kind = app.kind.clone();
        let AppCommand::Request(view) = app.handle_key(key(KeyCode::Enter)) else {
            panic!("ReplicaSet child Pods");
        };
        assert_eq!(view.kind.known_kind(), Some(ResourceKind::Pods));
        assert_eq!(view.namespace.as_deref(), Some("team"));
        assert_eq!(
            view.label_selector.as_deref(),
            Some("pod-template-hash=abc")
        );
    }

    #[test]
    fn discovered_replica_set_descriptor_is_used_when_available() {
        let mut app = app(vec![row("team", "api", "app=api")]);
        let discovered = replica_set_descriptor().with_short_names(["replicas", "rs"]);
        app.resource_options.push(discovered.clone());
        let AppCommand::Request(view) = app.open_deployment_replica_sets() else {
            panic!("ReplicaSet view");
        };
        assert_eq!(view.kind, discovered);
    }
}

#[cfg(test)]
#[path = "app_workloads_tests.rs"]
mod workloads_tests;
