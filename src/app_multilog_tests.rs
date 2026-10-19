use std::{sync::Arc, time::Instant};

use super::*;
use crate::source::{LogLine, LogSessionMeta, LogSource};

fn app(kind: ResourceDescriptor) -> App {
    let mut app = App::new(
        "demo",
        None,
        ResourceKind::Pods,
        true,
        vec!["demo".to_owned()],
    );
    app.kind = kind;
    app.apply_snapshot(ResourceSnapshot {
        generation: app.generation,
        kind: app.kind.clone(),
        rows: Arc::new(vec![
            Arc::new(ResourceRow::new("team", "selected", "Ready", "1/1", "", 0)),
            Arc::new(ResourceRow::new("team", "z-marked", "Ready", "1/1", "", 0)),
        ]),
        table_columns: Arc::from([]),
        state: SourceState::Ready,
        revision: 1,
        emitted_at: Instant::now(),
    });
    app
}

fn descriptor(group: &str, kind: &str, plural: &str) -> ResourceDescriptor {
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
        true,
        ResourceOperations::all(),
    )
}

fn metadata(multi_pods: bool) -> LogSessionMeta {
    LogSessionMeta {
        default_container: Some("app".to_owned()),
        selected_container: Some("app".to_owned()),
        all_containers: false,
        multi_pods,
        single_container: false,
        source_count: 2,
    }
}

fn line(pod: &str, container: &str, message: &str) -> LogLine {
    LogLine {
        source: Some(LogSource {
            namespace: "team".to_owned(),
            pod: pod.to_owned(),
            container: container.to_owned(),
            single_container: false,
        }),
        text: format!("2026-10-13T01:02:03Z {message}"),
    }
}

fn key(value: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(value), KeyModifiers::NONE)
}

fn started(command: AppCommand) -> LogRequest {
    let AppCommand::Action(ActionIntent::StartLogs { request }) = command else {
        panic!("expected a fresh log request");
    };
    request
}

#[test]
fn all_supported_resource_log_keys_resolve_the_selected_scope_in_read_only_mode() {
    for kind in [
        ResourceKind::Pods.into(),
        ResourceKind::Deployments.into(),
        ResourceKind::DaemonSets.into(),
        ResourceKind::StatefulSets.into(),
        ResourceKind::Services.into(),
        descriptor("batch", "Job", "jobs"),
    ] {
        for previous in [false, true] {
            let mut app = app(kind.clone());
            app.marks.insert(Arc::from("team/z-marked"));
            let request = started(app.handle_key(key(if previous { 'p' } else { 'l' })));
            assert_eq!(request.target.resource.kind, kind);
            assert_eq!(request.target.namespace(), "team");
            assert_eq!(request.target.name(), "selected");
            assert_eq!(request.target.previous, previous);
            assert_eq!(request.target.container, None);
            assert!(!request.all_containers);
            assert!(app.is_logs_view());
            assert!(!app.can_toggle_all_log_containers());
        }
    }
}

#[test]
fn unsupported_kinds_and_missing_namespaces_do_not_open_log_streams() {
    for kind in [
        descriptor("example.test", "Deployment", "deployments"),
        descriptor("batch", "CronJob", "cronjobs"),
        descriptor("apps", "ReplicaSet", "replicasets"),
    ] {
        for key_value in ['l', 'p'] {
            let mut app = app(kind.clone());
            assert!(!matches!(
                app.handle_key(key(key_value)),
                AppCommand::Action(ActionIntent::StartLogs { .. })
            ));
        }
    }
    let mut app = app(ResourceKind::Pods.into());
    app.snapshot.rows = Arc::new(vec![Arc::new(ResourceRow::new(
        "-", "pod", "Ready", "1/1", "", 0,
    ))]);
    app.rebuild_filter(None);
    assert_eq!(app.resource_log_action(false), AppCommand::None);
    assert!(app.log_view.is_none());
}

#[test]
fn explicit_container_scope_never_expands_when_default_metadata_arrives() {
    let mut app = app(ResourceKind::Pods.into());
    let request = started(app.open_logs(LogTarget::pod(
        "team",
        "selected",
        Some("init".to_owned()),
        true,
    )));
    app.show_log_resolution(request.id, metadata(false));
    assert_eq!(app.content_title, "Previous Logs(team/selected:init)");
    assert!(!app.can_toggle_all_log_containers());
    assert_eq!(app.handle_key(key('a')), AppCommand::None);
    assert_eq!(app.log_view.as_ref().unwrap().request, request);
}

#[test]
fn all_container_toggle_restarts_with_new_identity_and_preserves_other_controls() {
    let mut app = app(ResourceKind::Deployments.into());
    app.user_config.settings.logger.since_seconds = 300;
    let initial = started(app.resource_log_action(true));
    app.show_log_resolution(initial.id, metadata(true));
    assert_eq!(app.content_title, "Previous Logs(team/selected:app)");
    app.append_log_batch(
        initial.id,
        vec![line("pod-a", "app", "initial")],
        false,
        None,
    );
    for value in ['s', 'L', 't', 'w'] {
        app.handle_key(key(value));
    }
    app.body_query = "app".to_owned();
    let ranged = started(app.handle_key(key('a')));
    assert_ne!(ranged.id, initial.id);
    assert!(ranged.all_containers);
    assert_eq!(app.content_title, "Previous Logs(team/selected)");
    assert_eq!(ranged.target, initial.target);
    assert_eq!(ranged.range, LogRange::SinceSeconds(300));
    assert_eq!(ranged.tail_count, initial.tail_count);
    assert_eq!(app.content_body, "");
    assert_eq!(app.body_query, "app");
    let view = app.log_view.as_ref().unwrap();
    assert!(!view.auto_scroll);
    assert!(view.column_lock);
    assert!(view.metadata.is_none());
    assert!(app.ui.timestamps());
    assert!(app.ui.wrap());
    assert!(!app.can_toggle_all_log_containers());
    app.show_log_resolution(initial.id, metadata(true));
    app.append_log_batch(initial.id, vec![line("pod-a", "app", "stale")], true, None);
    assert_eq!(app.content_body, "");
    assert!(app.log_view.as_ref().unwrap().metadata.is_none());
    let mut all = metadata(true);
    all.all_containers = true;
    all.selected_container = None;
    app.show_log_resolution(ranged.id, all);
    assert_eq!(app.content_title, "Previous Logs(team/selected)");
    let restored = started(app.handle_key(key('a')));
    assert!(!restored.all_containers);
    assert_ne!(restored.id, ranged.id);
    assert_eq!(app.content_title, "Previous Logs(team/selected:app)");
}

#[test]
fn default_and_single_container_metadata_control_the_exact_toggle_behavior() {
    let mut app = app(ResourceKind::Pods.into());
    let request = started(app.resource_log_action(false));
    let mut no_default = metadata(false);
    no_default.default_container = None;
    no_default.all_containers = true;
    app.show_log_resolution(request.id, no_default);
    assert!(app.log_view.as_ref().unwrap().request.all_containers);
    assert!(!app.can_toggle_all_log_containers());
    assert_eq!(app.handle_key(key('a')), AppCommand::None);
    let mut single = metadata(false);
    single.single_container = true;
    app.show_log_resolution(request.id, single);
    assert!(app.can_toggle_all_log_containers());
    assert_eq!(app.handle_key(key('a')), AppCommand::None);
    assert_eq!(app.log_view.as_ref().unwrap().request.id, request.id);
    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    app.show_log_resolution(request.id, metadata(false));
    assert!(app.log_view.is_none());
    assert!(!app.can_toggle_all_log_containers());
}

#[test]
fn merged_prefixes_survive_timestamp_toggle_filter_copy_save_and_sanitization() {
    let mut app = app(ResourceKind::Deployments.into());
    let request = started(app.resource_log_action(false));
    app.show_log_resolution(request.id, metadata(true));
    app.append_log_batch(
        request.id,
        vec![
            line("pod-a", "app", "  \x1b[31mERROR first\x1b[0m"),
            line("pod-b", "sidecar", "INFO second"),
        ],
        false,
        None,
    );
    assert_eq!(
        app.log_export_text(),
        "pod-a app   ERROR first\npod-b sidecar INFO second"
    );
    app.body_query = "sidecar".to_owned();
    let expected = "pod-b sidecar INFO second";
    assert_eq!(
        app.handle_key(key('c')),
        AppCommand::Copy(expected.to_owned())
    );
    assert!(matches!(
        app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL)),
        AppCommand::SaveLogs { body, .. } if body == expected
    ));
    app.handle_key(key('t'));
    assert_eq!(
        app.log_export_text(),
        format!("{:<30} {expected}", "2026-10-13T01:02:03Z")
    );
    assert!(!app.content_body.contains('\x1b'));
    let view = app.log_view.as_ref().unwrap();
    assert_eq!(view.displayed_source(0).unwrap().pod, "pod-a");
    assert_eq!(view.displayed_source(1).unwrap().container, "sidecar");
}

#[test]
fn pause_mark_resume_and_eviction_keep_displayed_sources_aligned() {
    let mut app = app(ResourceKind::Deployments.into());
    app.user_config.settings.logger.buffer = 3;
    app.user_config.settings.logger.disable_autoscroll = true;
    let request = started(app.resource_log_action(false));
    app.show_log_resolution(request.id, metadata(true));
    app.append_log_batch(request.id, vec![line("pod-a", "app", "first")], false, None);
    app.append_log_batch(
        request.id,
        vec![
            line("pod-b", "app", "second"),
            line("pod-c", "app", "third"),
        ],
        false,
        None,
    );
    assert_eq!(app.content_body.lines().count(), 1);
    app.handle_key(key('m'));
    assert!(app.log_view.as_ref().unwrap().displayed_source(1).is_none());
    app.handle_key(key('s'));
    assert_eq!(app.content_body.lines().count(), 3);
    let view = app.log_view.as_ref().unwrap();
    assert!(view.displayed_source(0).is_none());
    assert_eq!(view.displayed_source(1).unwrap().pod, "pod-b");
    assert_eq!(view.displayed_source(2).unwrap().pod, "pod-c");
    assert!(!app.log_export_text().contains("first"));
    app.handle_key(key('C'));
    assert_eq!(app.content_body, "");
    assert!(app.log_view.as_ref().unwrap().displayed_source(0).is_none());
    app.append_log_batch(
        request.id,
        vec![line("pod-d", "app", "after clear")],
        false,
        None,
    );
    assert_eq!(app.log_export_text(), "pod-d app after clear");
    assert_eq!(
        app.log_view
            .as_ref()
            .unwrap()
            .displayed_source(0)
            .unwrap()
            .pod,
        "pod-d"
    );
}

#[test]
fn per_source_single_container_controls_prefix_and_preserves_message_indentation() {
    let mut app = app(ResourceKind::Pods.into());
    let request = started(app.resource_log_action(false));
    app.show_log_resolution(request.id, metadata(false));
    let mut single = line("selected", "app", "  indented");
    single.source.as_mut().unwrap().single_container = true;
    app.append_log_batch(request.id, vec![single], false, None);
    assert_eq!(app.log_export_text(), "  indented");
    app.handle_key(key('t'));
    assert_eq!(
        app.log_export_text(),
        format!("{:<30}   indented", "2026-10-13T01:02:03Z")
    );
}

#[test]
fn resolved_sole_container_title_does_not_require_a_default_annotation_or_log_output() {
    let mut app = app(ResourceKind::Pods.into());
    let request = started(app.resource_log_action(false));
    assert_eq!(app.content_title, "Logs(team/selected)");
    let mut sole = metadata(false);
    sole.default_container = None;
    sole.selected_container = Some("only-container".to_owned());
    sole.single_container = true;
    sole.source_count = 1;
    app.show_log_resolution(request.id, sole);
    assert_eq!(app.content_title, "Logs(team/selected:only-container)");
    assert_eq!(app.content_body, "");
    assert!(!app.can_toggle_all_log_containers());
    let refreshed = started(app.handle_key(key('2')));
    assert_ne!(refreshed.id, request.id);
    assert_eq!(app.content_title, "Logs(team/selected:only-container)");
}
