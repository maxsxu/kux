use super::*;
use crate::{
    app::ResourceTarget,
    model::{
        OwnerReferenceData, ResourceDescriptor, ResourceRow, ResourceSnapshot, TableCell,
        TableColumn,
    },
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{Terminal, backend::TestBackend};
use std::{sync::Arc, time::Instant};

fn workload_app(namespace: Option<String>) -> App {
    let mut app = App::new(
        "demo",
        namespace,
        ResourceKind::Pods,
        false,
        vec!["demo".to_owned()],
    );
    app.kind = ResourceDescriptor::workloads();
    app.sort_field = SortField::Kind;
    let rows = [
        (ResourceKind::Services, "same", "OK", ""),
        (ResourceKind::Deployments, "same", "DEGRADED", "1/2"),
        (ResourceKind::Pods, "worker", "OK", "1/1"),
    ]
    .into_iter()
    .map(|(kind, name, status, ready)| {
        let descriptor = ResourceDescriptor::from(kind);
        let cells = [
            descriptor.gvr(),
            "team-a".into(),
            name.into(),
            status.into(),
            ready.into(),
            if status == "OK" {
                String::new()
            } else {
                "DEGRADED".into()
            },
            "stale age".into(),
        ]
        .into_iter()
        .map(|text| TableCell {
            text: text.into(),
            timestamp_unix: None,
        })
        .collect();
        Arc::new(
            ResourceRow::new("team-a", name, status, ready, "", app.now_unix - 60)
                .with_workload_kind(descriptor)
                .with_owner_reference_data(OwnerReferenceData {
                    api_version: Arc::from("apps/v1"),
                    kind: Arc::from("ReplicaSet"),
                    name: Arc::from("parent"),
                    uid: Arc::from("owner-uid"),
                })
                .with_table_cells(cells),
        )
    })
    .collect();
    let columns = [
        "KIND",
        "NAMESPACE",
        "NAME",
        "STATUS",
        "READY",
        "VALID",
        "AGE",
    ]
    .into_iter()
    .map(|name| TableColumn {
        name: Arc::from(name),
        format: Arc::from(""),
        priority: i32::from(name == "VALID"),
    })
    .collect::<Vec<_>>();
    app.apply_snapshot(ResourceSnapshot {
        generation: app.generation,
        kind: app.kind.clone(),
        rows: Arc::new(rows),
        table_columns: columns.into(),
        state: SourceState::Ready,
        revision: 1,
        emitted_at: Instant::now(),
    });
    app
}

fn lines(terminal: &Terminal<TestBackend>, width: u16) -> Vec<String> {
    terminal
        .backend()
        .buffer()
        .content()
        .chunks(usize::from(width))
        .map(|row| row.iter().map(ratatui::buffer::Cell::symbol).collect())
        .collect()
}

#[test]
fn workload_columns_preserve_pinned_order_scope_and_wide_export() {
    for namespace in [None, Some("team-a".to_owned())] {
        let mut app = workload_app(namespace);
        for width in [48, 80, 180] {
            let columns = columns_for_view(&app, width);
            let labels = columns
                .iter()
                .filter(|column| !column.label.is_empty())
                .map(|column| column.label.as_str())
                .collect::<Vec<_>>();
            assert_eq!(
                labels,
                ["KIND", "NAMESPACE", "NAME", "STATUS", "READY", "AGE"]
            );
            assert_eq!(columns[1].field, Some(SortField::Kind));
            assert_eq!(columns[2].field, Some(SortField::Namespace));
            assert!(columns.last().unwrap().table_age);
        }
        let mut csv = Vec::new();
        TableExportSnapshot::capture(&app)
            .write_csv(&mut csv)
            .unwrap();
        let csv = String::from_utf8(csv).unwrap();
        assert!(csv.starts_with("\"KIND\",\"NAMESPACE\",\"NAME\",\"STATUS\",\"READY\",\"AGE\"\n"));
        assert!(csv.contains("apps/v1/deployments\",\"team-a\",\"same"));
        assert!(csv.contains("v1/services\",\"team-a\",\"same"));
        assert!(!csv.contains("stale age"));
        app.handle_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL));
        let mut wide = Vec::new();
        TableExportSnapshot::capture(&app)
            .write_csv(&mut wide)
            .unwrap();
        let wide = String::from_utf8(wide).unwrap();
        assert!(
            wide.lines()
                .next()
                .unwrap()
                .contains("\"READY\",\"VALID\",\"AGE\"")
        );
        assert!(wide.contains("\"DEGRADED\",\"1/2\",\"DEGRADED\""));
        assert!(wide.contains("\"OK\",\"\",\"\""));
        let values = plugin_column_values(&app, &app.snapshot.rows[1]);
        assert!(values.iter().any(|(key, _)| key == "VALID"));
    }
}

#[test]
fn workload_hints_offer_mixed_resource_actions_and_sorting_without_typed_actions() {
    for read_only in [false, true] {
        let mut app = workload_app(None);
        app.read_only = read_only;
        let hints = resource_action_hints(&app);
        for (key, label) in [
            ("enter", "Goto"),
            ("y", "YAML"),
            ("d", "Describe"),
            ("shift-k", "Sort Kind"),
            ("shift-r", "Sort Ready"),
            ("shift-a", "Sort Age"),
        ] {
            assert!(
                hints
                    .iter()
                    .any(|hint| hint.key == key && hint.label == label)
            );
        }
        for key in ["l", "p", "s", "r", "i", "ctrl-k", "z", "t", "shift-j"] {
            assert!(
                !hints.iter().any(|hint| hint.key == key),
                "unexpected {key}"
            );
        }
        for key in ["e", "ctrl-d"] {
            assert_eq!(hints.iter().any(|hint| hint.key == key), !read_only);
        }
    }
}

#[test]
fn workload_mouse_kind_sort_and_horizontal_scroll_preserve_table_identity() {
    let mut app = workload_app(None);
    let mut terminal = Terminal::new(TestBackend::new(48, 8)).unwrap();
    terminal
        .draw(|frame| draw_resources(frame, &mut app, frame.area()))
        .unwrap();
    let initial = lines(&terminal, 48);
    let column = u16::try_from(initial[1].find("KIND").unwrap()).unwrap();
    assert!(app.handle_mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column,
        row: 1,
        modifiers: KeyModifiers::NONE
    }));
    assert_eq!(app.sort_field, SortField::Kind);
    for _ in 0..200 {
        app.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
    }
    terminal
        .draw(|frame| draw_resources(frame, &mut app, frame.area()))
        .unwrap();
    let scrolled = lines(&terminal, 48);
    assert!(scrolled[1].contains("AGE"));
    assert_eq!(initial[0], scrolled[0]);
    assert_eq!(initial[7], scrolled[7]);
    assert!(app.list_horizontal_scroll > 0);
}

#[test]
fn degraded_workload_cells_use_pending_color_without_hiding_selection_or_marks() {
    let mut app = workload_app(None);
    app.selected = 2;
    let degraded = app
        .snapshot
        .rows
        .iter()
        .find(|row| row.status.as_ref() == "DEGRADED")
        .unwrap();
    app.marks.insert(degraded.key.clone());
    let mut terminal = Terminal::new(TestBackend::new(180, 8)).unwrap();
    terminal
        .draw(|frame| draw_resources(frame, &mut app, frame.area()))
        .unwrap();
    let rendered = lines(&terminal, 180);
    let row_index = rendered
        .iter()
        .position(|line| line.contains("DEGRADED"))
        .unwrap();
    for token in ["apps/v1/deployments", "team-a", "same", "DEGRADED", "1/2"] {
        let column = rendered[row_index].find(token).unwrap();
        assert_eq!(
            terminal.backend().buffer()[(
                u16::try_from(column).unwrap(),
                u16::try_from(row_index).unwrap(),
            )]
                .fg,
            YELLOW,
            "{token}"
        );
    }
    assert!(rendered[row_index].contains('●'));
}

#[test]
fn instance_title_identifies_get_scope_without_rewriting_active_namespace() {
    let mut app = workload_app(Some("parent-scope".to_owned()));
    app.kind = ResourceKind::Deployments.into();
    app.instance = Some(ResourceTarget {
        kind: app.kind.clone(),
        namespace: Some("team-a".to_owned()),
        name: "same".to_owned(),
    });
    app.filtered.truncate(1);
    assert_eq!(
        resource_view_title(&app, ""),
        " deployments(team-a/same)[1] "
    );
    assert_eq!(app.namespace_display(), "parent-scope");
    app.instance.as_mut().unwrap().namespace = None;
    assert_eq!(
        resource_view_title(&app, "name"),
        " deployments(same)[1] /name "
    );
}

#[test]
fn mixed_delete_prompt_does_not_claim_every_target_has_the_first_kind() {
    let targets = [ResourceKind::Pods, ResourceKind::Services]
        .into_iter()
        .map(|kind| ResourceTarget {
            kind: kind.into(),
            namespace: Some("team-a".to_owned()),
            name: "same".to_owned(),
        })
        .collect();
    let pending = PendingDelete::Resources(targets);
    assert_eq!(
        confirmation_prompt(Some(&pending)).1,
        "Delete all 2 selected resources?"
    );
}

#[test]
fn workload_unknown_age_is_preserved_in_table_and_export_only_for_workload_rows() {
    let mut app = workload_app(None);
    let mut row = app.snapshot.rows[0].as_ref().clone();
    row.created_unix = 0;
    assert_eq!(
        column_value(&row, SortField::Age, app.now_unix),
        "<unknown>"
    );
    assert_eq!(table_column_value(&row, 6, true, app.now_unix), "<unknown>");
    app.apply_snapshot(ResourceSnapshot {
        rows: Arc::new(vec![Arc::new(row.clone())]),
        revision: 2,
        ..app.snapshot.clone()
    });
    let mut csv = Vec::new();
    TableExportSnapshot::capture(&app)
        .write_csv(&mut csv)
        .unwrap();
    assert!(String::from_utf8(csv).unwrap().contains("\"<unknown>\""));
    row.data = crate::model::ResourceData::Generic;
    assert_eq!(
        column_value(&row, SortField::Age, app.now_unix),
        format_age(row.age_seconds(app.now_unix))
    );
}

#[test]
fn wide_workload_validity_mouse_and_keyboard_selection_use_visible_columns() {
    let mut app = workload_app(None);
    app.handle_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL));
    let columns = columns_for_view(&app, 180);
    assert_eq!(
        columns
            .iter()
            .find(|column| column.label == "VALID")
            .unwrap()
            .field,
        Some(SortField::Validity)
    );
    let mut terminal = Terminal::new(TestBackend::new(180, 8)).unwrap();
    terminal
        .draw(|frame| draw_resources(frame, &mut app, frame.area()))
        .unwrap();
    let rendered = lines(&terminal, 180);
    let column = u16::try_from(rendered[1].find("VALID").unwrap()).unwrap();
    assert!(app.handle_mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column,
        row: 1,
        modifiers: KeyModifiers::NONE,
    }));
    assert_eq!(app.sort_field, SortField::Validity);
    assert_eq!(SortField::ALL[app.selected_column], SortField::Validity);
    app.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::SHIFT));
    assert_eq!(SortField::ALL[app.selected_column], SortField::Ready);
    app.handle_key(KeyEvent::new(KeyCode::Char('O'), KeyModifiers::SHIFT));
    assert_eq!(app.sort_field, SortField::Ready);
}
