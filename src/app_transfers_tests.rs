use std::{sync::Arc, time::Instant};

use super::*;
use crate::model::{ResourceRow, ResourceSnapshot, SourceState};

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
    app.apply_snapshot(ResourceSnapshot {
        generation: app.generation,
        kind: app.kind.clone(),
        rows: Arc::new(vec![Arc::new(ResourceRow::new(
            "team", "chosen", "Running", "1/1", "0", 0,
        ))]),
        table_columns: Arc::from([]),
        state: SourceState::Ready,
        revision: 1,
        emitted_at: Instant::now(),
    });
    app
}

fn request(app: &mut App) -> (u64, ResourceTarget) {
    let AppCommand::Action(ActionIntent::LoadTransferPod { id, target }) =
        app.handle_key(key(KeyCode::Char('t')))
    else {
        panic!("t should load Pod");
    };
    (id, target)
}

fn open(app: &mut App) -> (u64, ResourceTarget) {
    let (id, target) = request(app);
    app.show_transfer_pod(
        id,
        &target,
        Ok(TransferPod {
            containers: vec!["main".to_owned(), "sidecar".to_owned()],
        }),
    );
    (id, target)
}

fn submit(app: &mut App) -> TransferIntent {
    let dialog = app.transfers.dialog.as_mut().unwrap();
    dialog.values[0].push_str("/tmp/from");
    dialog.values[1] = "/tmp/to".to_owned();
    dialog.focus = 7;
    let AppCommand::Transfer(intent) = app.handle_key(key(KeyCode::Enter)) else {
        panic!("OK should submit");
    };
    intent
}

#[test]
fn defaults_capture_selected_pod_and_direction_switch_swaps_paths() {
    let mut app = app();
    let (_, target) = open(&mut app);
    assert_eq!(target.namespace.as_deref(), Some("team"));
    assert_eq!(target.name, "chosen");
    let dialog = app.transfers.dialog.as_ref().unwrap();
    assert_eq!(dialog.values, ["team/chosen:", "", "main", "999"]);
    assert!(dialog.download);
    assert!(!dialog.no_preserve);
    app.handle_key(key(KeyCode::Char(' ')));
    let dialog = app.transfers.dialog.as_ref().unwrap();
    assert!(!dialog.download);
    assert_eq!(dialog.values[0], "");
    assert_eq!(dialog.values[1], "team/chosen:");
    app.handle_key(key(KeyCode::Enter));
    assert!(app.transfers.dialog.as_ref().unwrap().download);
}

#[test]
fn transfer_uses_selected_pod_even_when_a_different_pod_is_marked() {
    let mut app = app();
    app.marks.insert("other-team/marked".into());
    let (_, target) = request(&mut app);
    assert_eq!(target.name, "chosen");
    assert_eq!(target.namespace.as_deref(), Some("team"));
    assert_eq!(app.marks.len(), 1);
}

#[test]
fn canceled_or_mismatched_load_never_reopens_dialog() {
    let mut app = app();
    let (id, target) = request(&mut app);
    let mut other = target.clone();
    other.namespace = Some("elsewhere".to_owned());
    app.show_transfer_pod(
        id,
        &other,
        Ok(TransferPod {
            containers: vec!["wrong".to_owned()],
        }),
    );
    assert!(app.transfers.dialog.is_none());
    assert_eq!(
        app.handle_key(key(KeyCode::Esc)),
        AppCommand::Action(ActionIntent::StopContentRefresh)
    );
    app.show_transfer_pod(
        id,
        &target,
        Ok(TransferPod {
            containers: vec!["late".to_owned()],
        }),
    );
    assert_eq!(app.overlay, Overlay::None);
    let (next, _) = open(&mut app);
    assert_ne!(next, id);
    app.show_transfer_pod(id, &target, Err("stale".to_owned()));
    assert!(app.notice.is_none());
}

#[test]
fn read_only_and_context_load_close_transfer_forms() {
    let mut app = app();
    open(&mut app);
    let mut config = crate::config::UserConfig::default();
    config.settings.read_only = true;
    app.apply_user_config(config);
    assert_eq!(app.overlay, Overlay::None);
    assert!(app.transfers.dialog.is_none());
    assert_eq!(app.handle_key(key(KeyCode::Char('t'))), AppCommand::None);
    assert!(app.notice.as_ref().unwrap().contains("read-only"));
    app.apply_user_config(crate::config::UserConfig::default());
    let (id, target) = request(&mut app);
    app.begin_config_load();
    app.apply_user_config(crate::config::UserConfig::default());
    app.show_transfer_pod(
        id,
        &target,
        Ok(TransferPod {
            containers: vec!["late".to_owned()],
        }),
    );
    assert_eq!(app.overlay, Overlay::None);
}

#[test]
fn path_editing_handles_unicode_and_validation_retains_form() {
    let mut app = app();
    open(&mut app);
    app.handle_key(key(KeyCode::Tab));
    for character in "/資料".chars() {
        app.handle_key(key(KeyCode::Char(character)));
    }
    app.handle_key(key(KeyCode::Left));
    app.handle_key(key(KeyCode::Backspace));
    assert_eq!(
        app.transfers.dialog.as_ref().unwrap().values[0],
        "team/chosen:/料"
    );
    app.transfers.dialog.as_mut().unwrap().focus = 7;
    assert_eq!(app.handle_key(key(KeyCode::Enter)), AppCommand::None);
    assert_eq!(app.overlay, Overlay::TransferDialog);
    assert!(app.notice.as_ref().unwrap().contains("To"));
    app.transfers.dialog.as_mut().unwrap().values[1] = "/tmp/to".to_owned();
    app.transfers.dialog.as_mut().unwrap().values[3] = "invalid".to_owned();
    assert_eq!(app.handle_key(key(KeyCode::Enter)), AppCommand::None);
    assert!(app.notice.as_ref().unwrap().contains("Retries"));
}

#[test]
fn running_transfer_cannot_edit_or_resubmit_and_escape_cancels() {
    let mut app = app();
    let (id, _) = open(&mut app);
    let intent = submit(&mut app);
    assert!(matches!(intent, TransferIntent::Start { .. }));
    let before = app.transfers.dialog.as_ref().unwrap().values.clone();
    for code in [KeyCode::Char('x'), KeyCode::Tab, KeyCode::Up] {
        assert_eq!(app.handle_key(key(code)), AppCommand::None);
    }
    assert_eq!(app.transfers.dialog.as_ref().unwrap().values, before);
    assert_eq!(
        app.handle_key(key(KeyCode::Esc)),
        AppCommand::Transfer(TransferIntent::Cancel { id })
    );
    assert_eq!(app.overlay, Overlay::None);
}

#[test]
fn completion_is_scoped_to_context_generation_and_open_form() {
    let mut app = app();
    let (id, _) = open(&mut app);
    submit(&mut app);
    for (context, generation, completion_id) in [
        ("old", app.generation, id),
        ("demo", app.generation + 1, id),
        ("demo", app.generation, id + 1),
    ] {
        app.apply_transfer_result(TransferCompletion {
            id: completion_id,
            context: context.to_owned(),
            generation,
            result: Ok("stale".to_owned()),
        });
        assert!(app.transfers.dialog.as_ref().unwrap().running);
    }
    app.apply_transfer_result(TransferCompletion {
        id,
        context: app.context.clone(),
        generation: app.generation,
        result: Err("copy failed".to_owned()),
    });
    assert!(!app.transfers.dialog.as_ref().unwrap().running);
    assert_eq!(app.notice.as_deref(), Some("copy failed"));
    app.apply_transfer_result(TransferCompletion {
        id,
        context: app.context.clone(),
        generation: app.generation,
        result: Ok("Download completed".to_owned()),
    });
    assert_eq!(app.overlay, Overlay::None);
    assert_eq!(app.notice.as_deref(), Some("Download completed"));
}

#[test]
fn loading_failures_remain_visible_until_escape() {
    let mut app = app();
    let (id, target) = request(&mut app);
    app.show_transfer_pod(id, &target, Err("forbidden".to_owned()));
    assert_eq!(app.overlay, Overlay::TransferDialog);
    assert_eq!(app.notice.as_deref(), Some("forbidden"));
    app.handle_key(key(KeyCode::Esc));
    let (id, target) = request(&mut app);
    app.show_transfer_pod(id, &target, Ok(TransferPod { containers: vec![] }));
    assert!(app.notice.as_ref().unwrap().contains("eligible"));
}
