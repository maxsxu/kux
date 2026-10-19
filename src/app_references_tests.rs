use super::*;
use crate::{
    model::{ResourceDescriptor, ResourceKind, ResourceOperations},
    source::ReferenceEntry,
};

fn descriptor(group: &str, kind: &str, plural: &str, namespaced: bool) -> ResourceDescriptor {
    ResourceDescriptor::new(
        group,
        "v1",
        if group.is_empty() {
            "v1".to_owned()
        } else {
            format!("{group}/v1")
        },
        kind,
        plural,
        namespaced,
        ResourceOperations::all(),
    )
}

fn source() -> ResourceDescriptor {
    descriptor("", "ConfigMap", "configmaps", true)
}

fn app() -> App {
    let mut app = App::new(
        "demo",
        None,
        ResourceKind::Pods,
        false,
        vec!["demo".to_owned()],
    );
    app.kind = source();
    app.history[0] = app.current_view();
    snapshot(
        &mut app,
        vec![ResourceRow::new("team", "settings", "-", "-", "", 0)],
    );
    app
}

fn snapshot(app: &mut App, rows: Vec<ResourceRow>) {
    app.apply_snapshot(ResourceSnapshot {
        generation: app.generation,
        kind: app.kind.clone(),
        rows: Arc::new(rows.into_iter().map(Arc::new).collect()),
        table_columns: Arc::from([]),
        state: SourceState::Ready,
        revision: app.snapshot.revision + 1,
        emitted_at: Instant::now(),
    });
}

fn press(app: &mut App, code: KeyCode) -> AppCommand {
    app.handle_key(KeyEvent::new(code, KeyModifiers::NONE))
}

fn load(app: &mut App) -> (u64, ResourceTarget) {
    let AppCommand::Action(ActionIntent::LoadReferences { id, target }) =
        press(app, KeyCode::Char('u'))
    else {
        panic!("UsedBy scan");
    };
    (id, target)
}

fn report() -> ReferenceScan {
    ReferenceScan {
        entries: vec![
            ReferenceEntry {
                target: ResourceTarget {
                    kind: ResourceKind::Deployments.into(),
                    namespace: Some("team".to_owned()),
                    name: "shared".to_owned(),
                },
            },
            ReferenceEntry {
                target: ResourceTarget {
                    kind: descriptor("batch", "Job", "jobs", true),
                    namespace: Some("team".to_owned()),
                    name: "shared".to_owned(),
                },
            },
            ReferenceEntry {
                target: ResourceTarget {
                    kind: ResourceKind::StatefulSets.into(),
                    namespace: Some("other".to_owned()),
                    name: "shared".to_owned(),
                },
            },
        ],
        errors: Vec::new(),
        scanned: 30,
    }
}

fn open(app: &mut App) {
    let (id, target) = load(app);
    app.show_references(id, &target, Ok(report()));
    assert!(app.is_reference_view());
}

#[test]
fn usedby_is_available_in_readonly_mode_for_all_five_source_kinds() {
    for kind in [
        source(),
        descriptor("", "Secret", "secrets", true),
        descriptor("", "PersistentVolumeClaim", "persistentvolumeclaims", true),
        descriptor("", "ServiceAccount", "serviceaccounts", true),
        descriptor(
            "scheduling.k8s.io",
            "PriorityClass",
            "priorityclasses",
            false,
        ),
    ] {
        let mut app = app();
        app.kind = kind.clone();
        app.snapshot.kind = kind.clone();
        app.read_only = true;
        let (_, target) = load(&mut app);
        assert_eq!(target.kind, kind);
        assert_eq!(target.name, "settings");
    }
}

#[test]
fn references_keep_full_identity_and_sort_gvr_without_marking_or_mutating() {
    let mut app = app();
    open(&mut app);
    assert_eq!(app.snapshot.rows.len(), 3);
    let keys = app
        .snapshot
        .rows
        .iter()
        .map(|row| row.key.clone())
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(keys.len(), 3);
    press(&mut app, KeyCode::Char('V'));
    assert_eq!(app.sort_field, SortField::Details);
    let descending = app.sort_descending;
    press(&mut app, KeyCode::Char('V'));
    assert_ne!(app.sort_descending, descending);
    for event in [
        KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL),
        KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL),
        KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE),
        KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE),
    ] {
        assert_eq!(app.handle_key(event), AppCommand::None);
        assert!(app.is_reference_view());
    }
    assert_eq!(app.marks.len(), 0);
}

#[test]
fn goto_opens_exact_singleton_and_restores_reference_state() {
    let mut app = app();
    app.query = "settings".to_owned();
    app.rebuild_filter(None);
    open(&mut app);
    app.query = "shared".to_owned();
    app.rebuild_filter(None);
    press(&mut app, KeyCode::Char('V'));
    app.select(1);
    let selected = app.selected_row().unwrap().clone();
    let sort = (app.sort_field, app.sort_descending);
    let AppCommand::Request(view) = press(&mut app, KeyCode::Enter) else {
        panic!("Goto resource");
    };
    let ResourceData::Reference { kind } = &selected.data else {
        panic!("typed reference");
    };
    assert_eq!(&view.kind, kind);
    assert_eq!(view.namespace.as_deref(), Some(selected.namespace.as_ref()));
    assert_eq!(
        view.instance,
        Some(ResourceTarget {
            kind: kind.clone(),
            namespace: Some(selected.namespace.to_string()),
            name: selected.name.to_string(),
        })
    );
    assert_eq!(app.instance, view.instance);
    assert_eq!(view.field_selector, None);
    assert_eq!(view.label_selector, None);
    assert_eq!(app.query, "");
    snapshot(
        &mut app,
        vec![ResourceRow::new(
            selected.namespace.as_ref(),
            "shared",
            "-",
            "-",
            "",
            0,
        )],
    );
    assert_eq!(app.selected_row().unwrap().name.as_ref(), "shared");
    assert!(matches!(
        press(&mut app, KeyCode::Esc),
        AppCommand::Request(_)
    ));
    assert!(app.is_reference_view());
    assert_eq!(app.query, "shared");
    assert_eq!((app.sort_field, app.sort_descending), sort);
    assert_eq!(app.selected_row().unwrap().key, selected.key);
    assert_eq!(press(&mut app, KeyCode::Esc), AppCommand::None);
    assert!(app.is_resource_view());
    assert_eq!(app.query, "settings");
    assert_eq!(app.selected_row().unwrap().name.as_ref(), "settings");
}

#[test]
fn empty_complete_and_partial_scans_do_not_claim_the_same_result() {
    for errors in [Vec::new(), vec!["Jobs forbidden".to_owned()]] {
        let mut app = app();
        let (id, target) = load(&mut app);
        app.show_references(
            id,
            &target,
            Ok(ReferenceScan {
                entries: Vec::new(),
                scanned: 0,
                errors: errors.clone(),
            }),
        );
        assert!(app.is_resource_view());
        assert!(!app.references.loading);
        assert!(
            app.notice
                .as_deref()
                .unwrap()
                .contains(if errors.is_empty() {
                    "No references found"
                } else {
                    "Partial results"
                })
        );
    }
}

#[test]
fn escape_and_reopen_reject_old_success_and_failure_without_losing_origin() {
    let mut app = app();
    let (old_id, _) = load(&mut app);
    assert_eq!(
        press(&mut app, KeyCode::Esc),
        AppCommand::Action(ActionIntent::StopContentRefresh)
    );
    let (id, target) = load(&mut app);
    app.show_references(old_id, &target, Ok(report()));
    assert!(app.is_resource_view());
    app.show_references(old_id, &target, Err("old error".to_owned()));
    assert!(!app.notice.as_deref().unwrap().contains("old error"));
    app.show_references(id, &target, Ok(report()));
    assert!(app.is_reference_view());
}

#[test]
fn scans_cancel_for_new_input_overlay_selection_or_commands() {
    for mode in 0..4 {
        let mut app = app();
        let (id, target) = load(&mut app);
        let command = match mode {
            0 => {
                app.input_mode = InputMode::Filter;
                AppCommand::None
            }
            1 => {
                app.overlay = Overlay::Help;
                AppCommand::None
            }
            2 => {
                snapshot(&mut app, Vec::new());
                AppCommand::None
            }
            _ => AppCommand::Action(ActionIntent::ResolveOwner {
                target: target.clone(),
            }),
        };
        assert_eq!(
            app.cancel_references_before_command(&command),
            Some(ActionIntent::StopContentRefresh)
        );
        app.show_references(id, &target, Ok(report()));
        assert!(!app.is_reference_view());
        assert!(!app.references.loading);
    }
}

#[test]
fn refresh_is_nonoverlapping_and_preserves_rows_selection_and_partial_status() {
    let mut app = app();
    open(&mut app);
    app.select(1);
    let selected = app.selected_row().unwrap().key.clone();
    app.references.due = Some(Instant::now());
    let AppCommand::Action(ActionIntent::LoadReferences { id, target }) = app.poll_view_refresh()
    else {
        panic!("scheduled scan");
    };
    assert_eq!(app.poll_view_refresh(), AppCommand::None);
    assert!(app.is_reference_view());
    assert_eq!(app.snapshot.rows.len(), 3);
    let mut next = report();
    next.entries.reverse();
    next.errors.push("DaemonSets forbidden".to_owned());
    app.show_references(id, &target, Ok(next));
    assert_eq!(app.selected_row().unwrap().key, selected);
    assert!(app.notice.as_deref().unwrap().contains("Partial results"));
    assert!(!app.references.loading);
}

#[test]
fn parent_watch_updates_are_restored_after_leaving_references() {
    let mut app = app();
    open(&mut app);
    snapshot(
        &mut app,
        vec![ResourceRow::new("team", "settings", "Updated", "-", "", 0)],
    );
    assert_eq!(app.snapshot.rows.len(), 3);
    press(&mut app, KeyCode::Esc);
    assert_eq!(app.selected_row().unwrap().status.as_ref(), "Updated");
}

#[test]
fn back_from_filtered_references_cancels_refresh_and_cannot_reopen_from_late_results() {
    for key in [KeyCode::Esc, KeyCode::Char('h')] {
        let mut app = app();
        open(&mut app);
        app.query = "shared".to_owned();
        app.rebuild_filter(None);
        app.references.due = Some(Instant::now());
        let AppCommand::Action(ActionIntent::LoadReferences { id, target }) =
            app.poll_view_refresh()
        else {
            panic!("refresh scan");
        };
        assert_eq!(
            press(&mut app, key),
            AppCommand::Action(ActionIntent::StopContentRefresh)
        );
        assert!(app.is_resource_view());
        assert!(app.references.target.is_none());
        app.show_references(id, &target, Ok(report()));
        assert!(app.is_resource_view());
        assert!(app.notice.is_none());
    }
}

#[test]
fn parent_selection_disappearing_during_initial_scan_clears_its_loading_notice() {
    let mut app = app();
    let (id, target) = load(&mut app);
    snapshot(&mut app, Vec::new());
    app.show_references(id, &target, Ok(report()));
    assert!(app.is_resource_view());
    assert!(!app.references.loading);
    assert!(app.notice.is_none());
}

#[test]
fn origin_faults_filter_cannot_hide_references_or_goto_target_and_is_restored_on_back() {
    let mut app = app();
    app.kind = descriptor("", "PersistentVolumeClaim", "persistentvolumeclaims", true);
    app.history[0] = app.current_view();
    snapshot(
        &mut app,
        vec![
            ResourceRow::new("team", "pending", "Pending", "-", "", 0),
            ResourceRow::new("team", "healthy", "Ready", "-", "", 0),
        ],
    );
    app.ui.toggle(UiFlags::FAULTS);
    app.rebuild_filter(None);
    assert_eq!(app.filtered.len(), 1);
    open(&mut app);
    assert!(app.ui.faults());
    assert_eq!(app.filtered.len(), 3);
    let row = app.selected_row().unwrap().clone();
    assert!(matches!(
        press(&mut app, KeyCode::Enter),
        AppCommand::Request(_)
    ));
    assert!(!app.ui.faults());
    snapshot(
        &mut app,
        vec![ResourceRow::new(
            row.namespace.as_ref(),
            row.name.as_ref(),
            "Ready",
            "-",
            "",
            0,
        )],
    );
    assert_eq!(app.filtered.len(), 1);
    assert_eq!(app.selected_row().unwrap().name, row.name);
    press(&mut app, KeyCode::Esc);
    assert!(app.is_reference_view());
    assert!(app.ui.faults());
    assert_eq!(app.filtered.len(), 3);
    press(&mut app, KeyCode::Esc);
    assert!(app.is_resource_view());
    assert!(app.ui.faults());
    assert_eq!(app.filtered.len(), 1);
    assert_eq!(app.selected_row().unwrap().name.as_ref(), "pending");
}
