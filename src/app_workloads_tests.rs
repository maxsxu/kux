use std::{sync::Arc, time::Instant};

use super::*;
use crate::{
    app::{ContentKind, KeyCode, KeyEvent, KeyModifiers, ListMode, PendingDelete},
    model::{ResourceRow, SourceState},
};

fn press(app: &mut App, code: KeyCode, modifiers: KeyModifiers) -> AppCommand {
    app.handle_key(KeyEvent::new(code, modifiers))
}

fn row(kind: ResourceKind, namespace: &str, name: &str, status: &str) -> ResourceRow {
    ResourceRow::new(namespace, name, status, "1/2", "", 1).with_workload_kind(kind.into())
}

fn snapshot(app: &mut App, rows: Vec<ResourceRow>) {
    app.apply_snapshot(ResourceSnapshot {
        generation: app.generation,
        kind: app.kind.clone(),
        rows: Arc::new(rows.into_iter().map(Arc::new).collect()),
        table_columns: Arc::from([]),
        state: SourceState::Ready,
        revision: 1,
        emitted_at: Instant::now(),
    });
}

fn app(namespace: Option<&str>) -> App {
    let mut app = App::new(
        "demo",
        namespace.map(str::to_owned),
        ResourceKind::Pods,
        false,
        vec!["demo".to_owned()],
    );
    assert!(matches!(app.run_command("wk"), AppCommand::Request(_)));
    snapshot(
        &mut app,
        vec![
            row(ResourceKind::Pods, "team", "shared", "DEGRADED"),
            row(ResourceKind::Deployments, "team", "shared", "DEGRADED"),
            row(ResourceKind::Pods, "other", "shared", "OK"),
        ],
    );
    app
}

#[test]
fn catalog_keeps_workloads_through_discovery_and_context_reset() {
    let mut app = app(None);
    for resources in [Vec::new(), vec![ResourceKind::Pods.into()]] {
        app.set_resource_options(resources);
        assert_eq!(
            app.resource_options
                .iter()
                .filter(|kind| kind.is_workloads())
                .count(),
            1
        );
        assert!(
            app.command_completions_for("w")
                .contains(&"workloads".to_owned())
        );
        assert!(app.command_completions_for("w").contains(&"wk".to_owned()));
        let AppCommand::Request(view) = app.run_command("workloads team app=web") else {
            panic!("Workloads command");
        };
        assert!(view.kind.is_workloads());
        assert_eq!(view.namespace.as_deref(), Some("team"));
        assert_eq!(view.label_selector.as_deref(), Some("app=web"));
        assert_eq!(view.instance, None);
    }
    app.run_command("ctx another");
    assert!(
        app.resource_options
            .iter()
            .any(ResourceDescriptor::is_workloads)
    );
    assert!(matches!(app.run_command("wk"), AppCommand::Request(_)));
}

#[test]
fn aggregate_default_kind_sort_ready_age_and_faults_match_projected_rows() {
    let mut app = app(None);
    assert_eq!(
        (app.sort_field, app.sort_descending),
        (SortField::Kind, false)
    );
    assert_eq!(
        app.selected_target().unwrap().kind,
        ResourceKind::Deployments.into()
    );
    press(&mut app, KeyCode::Char('K'), KeyModifiers::SHIFT);
    assert_eq!(
        (app.sort_field, app.sort_descending),
        (SortField::Kind, true)
    );
    press(&mut app, KeyCode::Char('R'), KeyModifiers::SHIFT);
    assert_eq!(app.sort_field, SortField::Ready);
    press(&mut app, KeyCode::Char('A'), KeyModifiers::SHIFT);
    assert_eq!(app.sort_field, SortField::Age);
    press(&mut app, KeyCode::Char('z'), KeyModifiers::CONTROL);
    assert_eq!(app.filtered.len(), 2);
    assert!(
        app.visible_rows()
            .all(|(_, row)| row.status.as_ref() == "DEGRADED")
    );
}

#[test]
fn marked_duplicate_names_retain_each_concrete_kind_and_namespace() {
    let mut app = app(None);
    app.marks
        .extend(app.snapshot.rows.iter().map(|row| Arc::clone(&row.key)));
    assert_eq!(app.marks.len(), 3);
    let targets = app.selected_or_marked_targets();
    assert_eq!(targets.len(), 3);
    assert_eq!(targets[0].kind, ResourceKind::Pods.into());
    assert_eq!(targets[1].kind, ResourceKind::Deployments.into());
    assert_eq!(targets[2].namespace.as_deref(), Some("other"));
    assert!(targets.iter().all(|target| !target.kind.is_workloads()));
    press(&mut app, KeyCode::Char('d'), KeyModifiers::CONTROL);
    assert_eq!(app.pending_delete, Some(PendingDelete::Resources(targets)));
}

#[test]
fn aggregate_read_and_edit_actions_use_selection_while_delete_uses_marks() {
    for (key, content) in [('y', ContentKind::Yaml), ('d', ContentKind::Describe)] {
        let mut app = app(None);
        let target = app.selected_target().unwrap();
        app.marks.insert(Arc::clone(&app.snapshot.rows[0].key));
        let AppCommand::Action(ActionIntent::Content {
            target: actual,
            content: kind,
            ..
        }) = press(&mut app, KeyCode::Char(key), KeyModifiers::NONE)
        else {
            panic!("concrete content action");
        };
        assert_eq!(actual, target);
        assert_eq!(kind, content);
    }
    let mut app = app(None);
    let target = app.selected_target().unwrap();
    assert_eq!(
        press(&mut app, KeyCode::Char('e'), KeyModifiers::NONE),
        AppCommand::Edit(target)
    );
}

#[test]
fn aggregate_disallows_workload_extensions_and_readonly_mutations() {
    let mut app = app(None);
    for code in ['l', 'p', 's', 'r', 'i', 'a', 't', 'o', 'z', 'u', 'f', 'b'] {
        assert_eq!(
            press(&mut app, KeyCode::Char(code), KeyModifiers::NONE),
            AppCommand::None
        );
        assert_eq!(app.overlay, Overlay::None);
    }
    assert_eq!(
        press(&mut app, KeyCode::Char('k'), KeyModifiers::CONTROL),
        AppCommand::None
    );
    assert_eq!(
        press(&mut app, KeyCode::Char('J'), KeyModifiers::SHIFT),
        AppCommand::None
    );
    app.read_only = true;
    for event in [
        KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE),
        KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL),
    ] {
        assert_eq!(app.handle_key(event), AppCommand::None);
        assert_eq!(app.overlay, Overlay::None);
        assert!(app.pending_delete.is_none());
    }
    assert!(matches!(
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE),
        AppCommand::Request(_)
    ));
}

#[test]
fn enter_preserves_active_scope_and_back_restores_full_aggregate_state() {
    for namespace in [None, Some("team")] {
        let mut app = app(namespace);
        app.query = "shared".to_owned();
        app.label_selector = Some("app=web".to_owned());
        app.ui.set(UiFlags::FAULTS, true);
        app.set_sort(SortField::Ready);
        app.select(1);
        app.toggle_mark();
        app.history[app.history_cursor] = app.current_view();
        let origin = app.current_view();
        let selected = app.selected_row().unwrap().key.clone();
        let target = app.selected_target().unwrap();
        let marks = app.marks.clone();
        let sort = (app.sort_field, app.sort_descending);
        let AppCommand::Request(destination) = press(&mut app, KeyCode::Enter, KeyModifiers::NONE)
        else {
            panic!("singleton request");
        };
        assert_eq!(destination.namespace.as_deref(), namespace);
        assert_eq!(destination.instance, Some(target.clone()));
        assert_eq!(destination.kind, target.kind);
        assert_eq!(destination.field_selector, None);
        assert_eq!(destination.label_selector, None);
        assert_eq!(app.query, "");
        assert!(!app.ui.faults());
        assert!(app.marks.is_empty());
        assert_eq!(app.list_mode, ListMode::Resources);
        assert_eq!(
            press(&mut app, KeyCode::Esc, KeyModifiers::NONE),
            AppCommand::Request(origin)
        );
        assert_eq!(app.instance, None);
        assert_eq!(app.query, "shared");
        assert_eq!((app.sort_field, app.sort_descending), sort);
        assert!(app.ui.faults());
        assert_eq!(app.selected_row().unwrap().key, selected);
        assert_eq!(app.marks, marks);
        let restored_rows = app
            .snapshot
            .rows
            .iter()
            .map(|row| row.as_ref().clone())
            .collect();
        app.apply_snapshot(ResourceSnapshot::loading(app.generation, app.kind.clone()));
        snapshot(&mut app, restored_rows);
        assert_eq!(app.selected_row().unwrap().key, selected);
        assert_eq!(app.marks, marks);
        assert_eq!(
            press(&mut app, KeyCode::Char(']'), KeyModifiers::NONE),
            AppCommand::Request(destination.clone())
        );
        assert_eq!(app.instance, destination.instance);
    }
}

#[test]
fn explicit_scope_commands_clear_instances_but_refresh_and_history_preserve_them() {
    for command in ["ns team", "ns all", "ctx another", "pods", "wk"] {
        let mut app = app(None);
        let AppCommand::Request(instance_view) =
            press(&mut app, KeyCode::Enter, KeyModifiers::NONE)
        else {
            panic!("singleton request");
        };
        assert_eq!(
            press(&mut app, KeyCode::Char('r'), KeyModifiers::CONTROL),
            AppCommand::Request(instance_view.clone())
        );
        let AppCommand::Request(view) = app.run_command(command) else {
            panic!("explicit view command");
        };
        assert_eq!(view.instance, None, "{command}");
        assert_eq!(app.instance, None);
        assert_eq!(
            press(&mut app, KeyCode::Char('['), KeyModifiers::NONE),
            AppCommand::Request(instance_view.clone())
        );
        assert_eq!(app.instance, instance_view.instance);
    }
}

#[test]
fn namespace_hotkeys_and_chooser_clear_instance_even_for_the_same_namespace() {
    for chooser in [false, true] {
        let mut app = app(Some("team"));
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        let command = if chooser {
            app.open_namespaces();
            press(&mut app, KeyCode::Enter, KeyModifiers::NONE)
        } else {
            press(&mut app, KeyCode::Char('1'), KeyModifiers::NONE)
        };
        let AppCommand::Request(view) = command else {
            panic!("namespace request");
        };
        assert_eq!(view.namespace.as_deref(), Some("team"));
        assert_eq!(view.instance, None);
    }
}

#[test]
fn missing_or_untyped_aggregate_rows_cannot_produce_synthetic_api_actions() {
    for rows in [
        Vec::new(),
        vec![ResourceRow::new("team", "untyped", "OK", "", "", 0)],
    ] {
        let mut app = app(None);
        snapshot(&mut app, rows);
        assert!(app.selected_target().is_none());
        assert_eq!(app.selected_or_marked_targets().len(), 0);
        for key in [KeyCode::Enter, KeyCode::Char('y'), KeyCode::Char('e')] {
            assert_eq!(press(&mut app, key, KeyModifiers::NONE), AppCommand::None);
        }
        assert!(app.instance.is_none());
    }
}

#[test]
fn empty_singleton_and_stale_aggregate_snapshots_cannot_reopen_the_parent_list() {
    let mut app = app(None);
    let stale = app.snapshot.clone();
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    let instance = app.instance.clone();
    snapshot(&mut app, Vec::new());
    app.apply_snapshot(stale);
    assert_eq!(app.instance, instance);
    assert_eq!(app.snapshot.rows.len(), 0);
    assert_eq!(app.list_mode, ListMode::Resources);
}

#[test]
fn readiness_sort_uses_natural_digit_runs_with_blank_and_malformed_values() {
    let mut app = app(None);
    let expected = [
        "",
        "0/0",
        "2/2",
        "2/10",
        "02/2",
        "10/10",
        "100000000000000000000000/1",
        "-",
        "bad2",
        "bad10",
    ];
    let rows = expected
        .iter()
        .rev()
        .enumerate()
        .map(|(index, ready)| {
            ResourceRow::new("team", format!("row-{index}"), "OK", *ready, "", 1)
                .with_workload_kind(ResourceKind::Pods.into())
        })
        .collect();
    snapshot(&mut app, rows);
    press(&mut app, KeyCode::Char('R'), KeyModifiers::SHIFT);
    let actual = app
        .filtered
        .iter()
        .map(|index| app.snapshot.rows[*index].ready.as_ref())
        .collect::<Vec<_>>();
    assert_eq!(actual, expected);
    press(&mut app, KeyCode::Char('R'), KeyModifiers::SHIFT);
    let actual = app
        .filtered
        .iter()
        .map(|index| app.snapshot.rows[*index].ready.as_ref())
        .collect::<Vec<_>>();
    assert_eq!(actual, expected.into_iter().rev().collect::<Vec<_>>());
    let low = ResourceRow::new("team", "low", "OK", "2/2", "", 10);
    let high = ResourceRow::new("team", "high", "OK", "10/10", "", 20);
    assert_eq!(
        super::super::compare_rows(&low, &high, SortField::Ready),
        Ordering::Greater
    );
}

#[test]
fn workload_age_ascending_means_youngest_first_and_unknown_sorts_as_zero() {
    let mut app = app(None);
    let rows = [
        ("old", app.now_unix - 100),
        ("new", app.now_unix - 10),
        ("unknown", 0),
    ]
    .into_iter()
    .map(|(name, created)| {
        ResourceRow::new("team", name, "OK", "1/1", "", created)
            .with_workload_kind(ResourceKind::Pods.into())
    })
    .collect();
    snapshot(&mut app, rows);
    press(&mut app, KeyCode::Char('A'), KeyModifiers::SHIFT);
    assert!(!app.sort_descending);
    let names = app
        .filtered
        .iter()
        .map(|index| app.snapshot.rows[*index].name.as_ref())
        .collect::<Vec<_>>();
    assert_eq!(names, ["unknown", "new", "old"]);
    press(&mut app, KeyCode::Char('A'), KeyModifiers::SHIFT);
    assert!(app.sort_descending);
    let names = app
        .filtered
        .iter()
        .map(|index| app.snapshot.rows[*index].name.as_ref())
        .collect::<Vec<_>>();
    assert_eq!(names, ["old", "new", "unknown"]);
    let old = ResourceRow::new("team", "old", "OK", "", "", 10);
    let new = ResourceRow::new("team", "new", "OK", "", "", 20);
    assert_eq!(
        super::super::compare_rows(&old, &new, SortField::Age),
        Ordering::Less
    );
}

#[test]
fn workload_column_navigation_wraps_visible_order_and_sorts_wide_validity() {
    let mut app = app(None);
    assert_eq!(SortField::ALL[app.selected_column], SortField::Kind);
    let expected = [
        SortField::Namespace,
        SortField::Name,
        SortField::Status,
        SortField::Ready,
        SortField::Age,
        SortField::Kind,
    ];
    for field in expected {
        press(&mut app, KeyCode::Right, KeyModifiers::SHIFT);
        assert_eq!(SortField::ALL[app.selected_column], field);
    }
    press(&mut app, KeyCode::Left, KeyModifiers::SHIFT);
    assert_eq!(SortField::ALL[app.selected_column], SortField::Age);
    press(&mut app, KeyCode::Char('w'), KeyModifiers::CONTROL);
    press(&mut app, KeyCode::Left, KeyModifiers::SHIFT);
    assert_eq!(SortField::ALL[app.selected_column], SortField::Validity);
    press(&mut app, KeyCode::Char('O'), KeyModifiers::SHIFT);
    assert_eq!(app.sort_field, SortField::Validity);
    assert!(!app.sort_descending);
    assert_eq!(app.snapshot.rows[app.filtered[0]].status.as_ref(), "OK");
    press(&mut app, KeyCode::Char('O'), KeyModifiers::SHIFT);
    assert!(app.sort_descending);
    assert_eq!(
        app.snapshot.rows[app.filtered[0]].status.as_ref(),
        "DEGRADED"
    );
    press(&mut app, KeyCode::Char('w'), KeyModifiers::CONTROL);
    assert_eq!(SortField::ALL[app.selected_column], SortField::Kind);
    press(&mut app, KeyCode::Char('R'), KeyModifiers::SHIFT);
    assert_eq!(SortField::ALL[app.selected_column], SortField::Ready);
    press(&mut app, KeyCode::Char('A'), KeyModifiers::SHIFT);
    assert_eq!(SortField::ALL[app.selected_column], SortField::Age);
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(SortField::ALL[app.selected_column], SortField::Name);
}

#[test]
fn generic_resource_column_navigation_keeps_its_existing_behavior() {
    let mut app = App::new(
        "demo",
        None,
        ResourceKind::Pods,
        false,
        vec!["demo".to_owned()],
    );
    press(&mut app, KeyCode::Left, KeyModifiers::SHIFT);
    assert_eq!(SortField::ALL[app.selected_column], SortField::Namespace);
    press(&mut app, KeyCode::Left, KeyModifiers::SHIFT);
    assert_eq!(SortField::ALL[app.selected_column], SortField::Namespace);
}

#[test]
fn history_normalizes_a_wide_column_hidden_while_visiting_the_instance() {
    let mut app = app(None);
    press(&mut app, KeyCode::Char('w'), KeyModifiers::CONTROL);
    app.set_sort(SortField::Validity);
    assert_eq!(SortField::ALL[app.selected_column], SortField::Validity);
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    press(&mut app, KeyCode::Char('w'), KeyModifiers::CONTROL);
    press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    assert!(app.kind.is_workloads());
    assert!(!app.ui.wide());
    assert_eq!(SortField::ALL[app.selected_column], SortField::Kind);
    assert_eq!(app.sort_field, SortField::Validity);
}
