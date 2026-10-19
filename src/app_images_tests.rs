use std::{sync::Arc, time::Instant};

use super::*;
use crate::{
    config::UserConfig,
    model::{ResourceKind, ResourceRow, ResourceSnapshot, SourceState},
};

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn app(kind: ResourceKind, read_only: bool) -> App {
    let mut app = App::new("demo", None, kind, read_only, vec!["demo".to_owned()]);
    replace_row(&mut app, "selected", 1);
    app
}

fn replace_row(app: &mut App, name: &str, revision: u64) {
    app.apply_snapshot(ResourceSnapshot {
        generation: app.generation,
        kind: app.kind.clone(),
        rows: Arc::new(vec![Arc::new(ResourceRow::new(
            "team-a", name, "Running", "1/1", "0", 0,
        ))]),
        table_columns: Arc::from([]),
        state: SourceState::Ready,
        revision,
        emitted_at: Instant::now(),
    });
}

fn request(app: &mut App) -> (u64, ResourceTarget) {
    let AppCommand::Action(ActionIntent::LoadImages { id, target }) =
        app.handle_key(key(KeyCode::Char('i')))
    else {
        panic!("image key must load selected images");
    };
    (id, target)
}

fn snapshot(target: ResourceTarget) -> ImageSnapshot {
    ImageSnapshot {
        target,
        uid: "loaded-uid".to_owned(),
        resource_version: "1042".to_owned(),
        images: vec![
            ContainerImage {
                name: "setup".to_owned(),
                image: "setup:v1".to_owned(),
                init: true,
            },
            ContainerImage {
                name: "api".to_owned(),
                image: "api:v1".to_owned(),
                init: false,
            },
            ContainerImage {
                name: "sidecar".to_owned(),
                image: "sidecar:v1".to_owned(),
                init: false,
            },
        ],
    }
}

fn open(app: &mut App) -> ImageSnapshot {
    let (id, target) = request(app);
    let snapshot = snapshot(target.clone());
    app.show_images(id, &target, Ok(snapshot.clone()));
    assert_eq!(app.overlay, Overlay::ImageDialog);
    assert!(app.image_editor.pending.is_none());
    snapshot
}

fn replace_value(app: &mut App, value: &str) {
    assert_eq!(
        app.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL)),
        AppCommand::None
    );
    for character in value.chars() {
        assert_eq!(
            app.handle_key(key(KeyCode::Char(character))),
            AppCommand::None
        );
    }
}

fn save(app: &mut App) -> AppCommand {
    let dialog = app.image_editor.dialog.as_ref().unwrap();
    let remaining = dialog.values.len() + 1 - dialog.focus;
    for _ in 0..remaining {
        assert_eq!(app.handle_key(key(KeyCode::Tab)), AppCommand::None);
    }
    app.handle_key(key(KeyCode::Enter))
}

#[test]
fn image_key_uses_selected_namespace_for_each_supported_kind() {
    for kind in [
        ResourceKind::Pods,
        ResourceKind::Deployments,
        ResourceKind::StatefulSets,
        ResourceKind::DaemonSets,
    ] {
        let mut app = app(kind, false);
        let (_, target) = request(&mut app);
        assert_eq!(target.kind, kind.into());
        assert_eq!(target.namespace.as_deref(), Some("team-a"));
        assert_eq!(target.name, "selected");
        assert_eq!(app.image_editor.pending.as_ref(), Some(&target));
        assert_eq!(app.overlay, Overlay::ImageDialog);
    }
}

#[test]
fn image_key_rejects_unsupported_resources_and_read_only_mode() {
    for (kind, read_only, reason) in [
        (ResourceKind::Services, false, "require"),
        (ResourceKind::Nodes, false, "require"),
        (ResourceKind::Pods, true, "read-only"),
    ] {
        let mut app = app(kind, read_only);
        assert_eq!(app.handle_key(key(KeyCode::Char('i'))), AppCommand::None);
        assert_eq!(app.overlay, Overlay::None);
        assert!(app.image_editor.pending.is_none());
        assert!(app.notice.as_deref().unwrap().contains(reason));
    }
}

#[test]
fn load_results_require_the_request_id_and_exact_target() {
    let mut app = app(ResourceKind::Pods, false);
    let (id, target) = request(&mut app);
    let mut other = target.clone();
    other.namespace = Some("other-team".to_owned());
    app.show_images(id.wrapping_add(1), &target, Ok(snapshot(target.clone())));
    app.show_images(id, &other, Ok(snapshot(other.clone())));
    assert!(app.image_editor.dialog.is_none());
    assert_eq!(app.image_editor.pending.as_ref(), Some(&target));
    app.show_images(id, &target, Ok(snapshot(other)));
    assert!(app.image_editor.dialog.is_none());
    assert!(app.image_editor.pending.is_none());
    assert!(app.notice.as_deref().unwrap().contains("No editable"));
}

#[test]
fn canceling_a_load_rejects_its_late_result_and_a_reopened_form_uses_a_new_id() {
    let mut app = app(ResourceKind::Pods, false);
    let (old_id, target) = request(&mut app);
    assert_eq!(
        app.handle_key(key(KeyCode::Esc)),
        AppCommand::Action(ActionIntent::StopContentRefresh)
    );
    app.show_images(old_id, &target, Ok(snapshot(target.clone())));
    assert_eq!(app.overlay, Overlay::None);
    assert!(app.image_editor.dialog.is_none());
    let (new_id, new_target) = request(&mut app);
    assert_ne!(old_id, new_id);
    app.show_images(old_id, &target, Ok(snapshot(target.clone())));
    assert!(app.image_editor.dialog.is_none());
    assert_eq!(app.image_editor.pending.as_ref(), Some(&new_target));
    app.show_images(new_id, &new_target, Ok(snapshot(new_target.clone())));
    assert_eq!(
        app.image_editor.dialog.as_ref().unwrap().snapshot.target,
        new_target
    );
}

#[test]
fn init_and_regular_images_submit_only_with_explicit_save() {
    let mut app = app(ResourceKind::Deployments, false);
    let original = open(&mut app);
    assert_eq!(
        app.image_editor.dialog.as_ref().unwrap().values,
        ["setup:v1", "api:v1", "sidecar:v1"]
    );
    replace_value(&mut app, "  setup:v2  ");
    assert_eq!(app.handle_key(key(KeyCode::Enter)), AppCommand::None);
    replace_value(&mut app, "api:v2");
    assert_eq!(app.handle_key(key(KeyCode::Tab)), AppCommand::None);
    let AppCommand::Action(ActionIntent::SetImages { snapshot, images }) = save(&mut app) else {
        panic!("Save must submit images");
    };
    assert_eq!(snapshot, original);
    assert_eq!(images[0].image, "setup:v2");
    assert!(images[0].init);
    assert_eq!(images[1].image, "api:v2");
    assert!(!images[1].init);
    assert_eq!(images[2], original.images[2]);
    assert_eq!(app.overlay, Overlay::None);
    assert!(app.image_editor.dialog.is_none());
}

#[test]
fn blank_and_trimmed_unchanged_images_are_preserved() {
    let mut app = app(ResourceKind::Pods, false);
    open(&mut app);
    replace_value(&mut app, "   ");
    app.handle_key(key(KeyCode::Tab));
    replace_value(&mut app, "  api:v1 ");
    assert_eq!(save(&mut app), AppCommand::None);
    assert!(app.notice.as_deref().unwrap().contains("unchanged"));
    let original = open(&mut app);
    replace_value(&mut app, "");
    app.handle_key(key(KeyCode::Tab));
    replace_value(&mut app, "api:v2");
    let AppCommand::Action(ActionIntent::SetImages { images, .. }) = save(&mut app) else {
        panic!("changed regular image must submit");
    };
    assert_eq!(images[0], original.images[0]);
    assert_eq!(images[1].image, "api:v2");
    assert_eq!(images[2], original.images[2]);
}

#[test]
fn save_retains_loaded_identity_and_revision_after_table_refresh() {
    let mut app = app(ResourceKind::StatefulSets, false);
    let original = open(&mut app);
    replace_value(&mut app, "setup:v2");
    replace_row(&mut app, "replacement", 2);
    assert_eq!(app.selected_target().unwrap().name, "replacement");
    let AppCommand::Action(ActionIntent::SetImages { snapshot, .. }) = save(&mut app) else {
        panic!("explicit Save must submit captured snapshot");
    };
    assert_eq!(snapshot, original);
    assert_eq!(snapshot.target.name, "selected");
    assert_eq!(snapshot.uid, "loaded-uid");
    assert_eq!(snapshot.resource_version, "1042");
}

#[test]
fn cancel_button_discards_edits_without_an_action() {
    let mut app = app(ResourceKind::Pods, false);
    open(&mut app);
    replace_value(&mut app, "setup:v2");
    for _ in 0..3 {
        assert_eq!(app.handle_key(key(KeyCode::Tab)), AppCommand::None);
    }
    assert_eq!(app.handle_key(key(KeyCode::Enter)), AppCommand::None);
    assert_eq!(app.overlay, Overlay::None);
    assert!(app.image_editor.dialog.is_none());
}

#[test]
fn config_read_only_closes_loaded_and_pending_forms_and_rejects_submission() {
    let mut app = app(ResourceKind::Pods, false);
    open(&mut app);
    replace_value(&mut app, "setup:v2");
    let mut config = UserConfig::default();
    config.settings.read_only = true;
    app.apply_user_config(config.clone());
    assert_eq!(app.overlay, Overlay::None);
    assert!(app.image_editor.dialog.is_none());
    assert_eq!(app.submit_images(), AppCommand::None);
    app.apply_user_config(UserConfig::default());
    let (id, target) = request(&mut app);
    app.apply_user_config(config);
    app.show_images(id, &target, Ok(snapshot(target.clone())));
    assert_eq!(app.overlay, Overlay::None);
    assert!(app.image_editor.pending.is_none());
    assert!(app.image_editor.dialog.is_none());
}

#[test]
fn context_configuration_load_invalidates_images_even_if_write_mode_returns() {
    let mut app = app(ResourceKind::Pods, false);
    let (id, target) = request(&mut app);
    app.begin_config_load();
    assert!(app.read_only);
    app.apply_user_config(UserConfig::default());
    assert!(!app.read_only);
    app.show_images(id, &target, Ok(snapshot(target.clone())));
    assert_eq!(app.overlay, Overlay::None);
    assert!(app.image_editor.dialog.is_none());
}
