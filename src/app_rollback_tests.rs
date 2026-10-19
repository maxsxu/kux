use std::{collections::BTreeMap, sync::Arc, time::Instant};

use ratatui::{Terminal, backend::TestBackend};

use super::*;
use crate::{
    config::UserConfig,
    model::{
        ResourceDescriptor, ResourceKind, ResourceOperations, ResourceRow, ResourceSnapshot,
        SourceState,
    },
};

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn app() -> App {
    let mut app = App::new(
        "demo",
        None,
        ResourceKind::Pods,
        false,
        vec!["demo".to_owned()],
    );
    app.kind = ResourceDescriptor::new(
        "apps",
        "v1",
        "apps/v1",
        "ReplicaSet",
        "replicasets",
        true,
        ResourceOperations::all(),
    );
    app.apply_snapshot(ResourceSnapshot {
        generation: app.generation,
        kind: app.kind.clone(),
        rows: Arc::new(vec![
            Arc::new(ResourceRow::new(
                "team-a",
                "old-revision",
                "Ready",
                "0/0",
                "0",
                0,
            )),
            Arc::new(ResourceRow::new(
                "team-b",
                "z-marked-revision",
                "Ready",
                "0/0",
                "0",
                0,
            )),
        ]),
        table_columns: Arc::from([]),
        state: SourceState::Ready,
        revision: 1,
        emitted_at: Instant::now(),
    });
    app
}

fn request(app: &mut App) -> (u64, ResourceTarget) {
    let AppCommand::Action(ActionIntent::LoadRollback { id, target }) =
        app.handle_key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::CONTROL))
    else {
        panic!("load rollback preview");
    };
    (id, target)
}

fn snapshot(target: ResourceTarget) -> RollbackSnapshot {
    RollbackSnapshot {
        target,
        uid: "rs-uid".to_owned(),
        resource_version: "40".to_owned(),
        deployment: "api".to_owned(),
        deployment_uid: "deployment-uid".to_owned(),
        deployment_resource_version: "100".to_owned(),
        revision: 3,
        template: serde_json::json!({"spec": {"containers": [{"name":"api", "image":"api:v3"}]}}),
        annotations: BTreeMap::new(),
        already_matches: false,
    }
}

fn render(app: &mut App, width: u16, height: u16) {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| crate::ui::draw(frame, app)).unwrap();
}

fn open(app: &mut App) -> RollbackSnapshot {
    let (id, target) = request(app);
    let value = snapshot(target.clone());
    app.show_rollback(id, &target, Ok(value.clone()));
    render(app, 120, 30);
    value
}

#[test]
fn rollback_uses_selected_revision_and_requires_explicit_confirmation() {
    let mut app = app();
    app.marks.insert(Arc::clone(&app.snapshot.rows[1].key));
    let original = open(&mut app);
    assert_eq!(original.target.namespace.as_deref(), Some("team-a"));
    assert_eq!(original.target.name, "old-revision");
    assert!(!app.rollback.dialog.as_ref().unwrap().confirm);
    assert_eq!(app.handle_key(key(KeyCode::Enter)), AppCommand::None);
    assert_eq!(app.overlay, Overlay::None);

    let original = open(&mut app);
    app.handle_key(key(KeyCode::Tab));
    assert_eq!(
        app.handle_key(key(KeyCode::Enter)),
        AppCommand::Action(ActionIntent::Rollback {
            snapshot: Box::new(original)
        }),
    );
    assert_eq!(app.overlay, Overlay::None);
}

#[test]
fn rollback_loading_cancellation_and_reopen_reject_stale_results() {
    let mut app = app();
    let (old_id, old_target) = request(&mut app);
    assert_eq!(
        app.handle_key(key(KeyCode::Esc)),
        AppCommand::Action(ActionIntent::StopContentRefresh)
    );
    let (id, target) = request(&mut app);
    app.show_rollback(old_id, &old_target, Ok(snapshot(old_target.clone())));
    app.show_rollback(old_id, &old_target, Err("old failure".to_owned()));
    assert!(app.rollback.dialog.is_none());
    assert_eq!(app.notice, None);
    let mut wrong_target = target.clone();
    wrong_target.namespace = Some("wrong".to_owned());
    app.show_rollback(id, &wrong_target, Ok(snapshot(wrong_target.clone())));
    assert!(app.rollback.pending.is_some());
    app.show_rollback(id, &target, Err("Forbidden".to_owned()));
    assert!(app.rollback.pending.is_none());
    assert_eq!(app.notice.as_deref(), Some("Forbidden"));
    assert_eq!(app.handle_key(key(KeyCode::Enter)), AppCommand::None);
}

#[test]
fn rollback_confirmation_retains_loaded_revision_after_table_changes() {
    let mut app = app();
    let original = open(&mut app);
    app.snapshot.rows = Arc::new(Vec::new());
    app.handle_key(key(KeyCode::Tab));
    assert_eq!(
        app.handle_key(key(KeyCode::Enter)),
        AppCommand::Action(ActionIntent::Rollback {
            snapshot: Box::new(original)
        })
    );
}

#[test]
fn rollback_scope_must_fit_before_confirmation() {
    for (width, height) in [(0, 0), (1, 30), (20, 30), (80, 6)] {
        let mut app = app();
        open(&mut app);
        render(&mut app, width, height);
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.handle_key(key(KeyCode::Enter)), AppCommand::None);
        assert!(app.notice.as_deref().unwrap().contains("Enlarge"));
        assert_eq!(app.overlay, Overlay::RollbackDialog);
        render(&mut app, 120, 30);
        assert!(matches!(
            app.handle_key(key(KeyCode::Enter)),
            AppCommand::Action(ActionIntent::Rollback { .. })
        ));
    }
}

#[test]
fn rollback_rechecks_context_generation_and_read_only_before_dispatch() {
    for change in 0..3 {
        let mut app = app();
        open(&mut app);
        app.handle_key(key(KeyCode::Tab));
        match change {
            0 => app.read_only = true,
            1 => app.generation += 1,
            _ => app.context = "different".to_owned(),
        }
        assert_eq!(app.handle_key(key(KeyCode::Enter)), AppCommand::None);
        assert_eq!(app.overlay, Overlay::None);
    }
}

#[test]
fn rollback_config_transitions_close_confirmation_and_modal_keys_are_isolated() {
    let mut app = app();
    open(&mut app);
    for character in [':', '/', 'z', 'y', 's'] {
        assert_eq!(
            app.handle_key(key(KeyCode::Char(character))),
            AppCommand::None
        );
        assert_eq!(app.overlay, Overlay::RollbackDialog);
    }
    let mut config = UserConfig::default();
    config.settings.read_only = true;
    app.apply_user_config(config);
    assert_eq!(app.overlay, Overlay::None);
    assert!(app.rollback.dialog.is_none());
    app.read_only = false;
    open(&mut app);
    app.begin_config_load();
    assert_eq!(app.overlay, Overlay::None);
}

#[test]
fn control_l_still_toggles_logo_outside_replica_sets_and_readonly_blocks_rollback() {
    let mut app = app();
    app.read_only = true;
    assert_eq!(
        app.handle_key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::CONTROL)),
        AppCommand::None
    );
    assert_eq!(app.overlay, Overlay::None);
    assert!(app.notice.as_deref().unwrap().contains("read-only"));
    app.kind = ResourceKind::Pods.into();
    let before = app.ui.logo();
    app.handle_key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::CONTROL));
    assert_ne!(app.ui.logo(), before);
}

#[test]
fn rollback_ctrl_c_respects_configured_exit_preference() {
    let mut app = app();
    open(&mut app);
    let event = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
    assert_eq!(app.handle_key(event), AppCommand::Quit);
    app.user_config.settings.no_exit_on_ctrl_c = true;
    assert_eq!(app.handle_key(event), AppCommand::None);
    assert_eq!(app.overlay, Overlay::RollbackDialog);
}
