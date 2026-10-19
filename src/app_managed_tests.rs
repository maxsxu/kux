use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::*;
use crate::model::ResourceKind;

fn app(auto_refresh: bool) -> (App, ResourceTarget) {
    let mut app = App::new(
        "demo",
        Some("team-a".to_owned()),
        ResourceKind::Pods,
        false,
        vec!["demo".to_owned()],
    );
    app.user_config.settings.live_view_auto_refresh = auto_refresh;
    let target = ResourceTarget {
        kind: ResourceKind::Pods.into(),
        namespace: Some("team-a".to_owned()),
        name: "selected".to_owned(),
    };
    app.content_target = Some(target.clone());
    open(&mut app, "original YAML");
    (app, target)
}

fn open(app: &mut App, body: &str) {
    app.show_content(
        "Selected Pod",
        body,
        Some(String::new()),
        Some("decoded original YAML".to_owned()),
        None,
        ContentKind::Yaml,
    );
}

fn press(app: &mut App, character: char) -> AppCommand {
    app.handle_key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
}

fn managed_request(app: &mut App, expected: &ResourceTarget) -> u64 {
    let AppCommand::Action(ActionIntent::ManagedYaml {
        id,
        target,
        body,
        decoded_body,
    }) = press(app, 'm')
    else {
        panic!("m must lazily request managed fields");
    };
    assert_eq!(&target, expected);
    assert_eq!(body, app.content_body);
    assert_eq!(decoded_body, app.content_decoded_body);
    assert!(app.yaml_refresh.pending);
    id
}

fn stale_results() -> [Result<(String, Option<String>), String>; 2] {
    [
        Ok((
            "stale managed YAML".to_owned(),
            Some("stale decoded YAML".to_owned()),
        )),
        Err("stale managed failure".to_owned()),
    ]
}

#[test]
fn managed_results_from_a_previous_open_of_the_same_target_are_ignored() {
    for result in stale_results() {
        let (mut app, target) = app(false);
        let old_id = managed_request(&mut app, &target);
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        open(&mut app, "reopened YAML");
        app.show_transient_notice("current view notice");
        let current_id = app.yaml_refresh.id;
        assert_ne!(old_id, current_id);
        app.show_managed_yaml(old_id, &target, result);
        assert_eq!(app.overlay, Overlay::Content);
        assert_eq!(app.content_body, "reopened YAML");
        assert_eq!(app.content_managed_fields.as_deref(), Some(""));
        assert!(app.content_decoded_managed_fields.is_none());
        assert!(!app.ui.managed_fields());
        assert!(!app.yaml_refresh.pending);
        assert_eq!(app.yaml_refresh.id, current_id);
        assert_eq!(app.notice.as_deref(), Some("current view notice"));
    }
}

#[test]
fn disabling_refresh_invalidates_pending_managed_fields() {
    for result in stale_results() {
        let (mut app, target) = app(true);
        let old_id = managed_request(&mut app, &target);
        assert_eq!(
            press(&mut app, 'r'),
            AppCommand::Action(ActionIntent::StopContentRefresh)
        );
        assert!(!app.content_auto_refresh());
        assert!(!app.yaml_refresh.pending);
        let notice = app.notice.clone();
        let current_id = app.yaml_refresh.id;
        let due = app.yaml_refresh.due;
        app.show_managed_yaml(old_id, &target, result);
        assert_eq!(app.yaml_refresh.id, current_id);
        assert_eq!(app.yaml_refresh.due, due);
        assert!(!app.yaml_refresh.pending);
        assert_eq!(app.content_body, "original YAML");
        assert_eq!(app.content_managed_fields.as_deref(), Some(""));
        assert!(!app.ui.managed_fields());
        assert_eq!(app.notice, notice);
    }
}

#[test]
fn edit_pause_invalidates_managed_fields_without_changing_refresh_preference() {
    for enabled in [false, true] {
        for result in stale_results() {
            let (mut app, target) = app(enabled);
            let old_id = managed_request(&mut app, &target);
            assert_eq!(press(&mut app, 'e'), AppCommand::Edit(target.clone()));
            app.pause_content_refresh();
            let paused_id = app.yaml_refresh.id;
            assert_ne!(old_id, paused_id);
            app.show_managed_yaml(old_id, &target, result);
            assert_eq!(app.overlay, Overlay::Content);
            assert_eq!(app.content_auto_refresh(), enabled);
            assert_eq!(app.yaml_refresh.id, paused_id);
            assert!(!app.yaml_refresh.pending);
            assert_eq!(app.content_body, "original YAML");
            assert_eq!(app.content_managed_fields.as_deref(), Some(""));
            assert!(app.notice.is_none());
        }
    }
}

#[test]
fn stale_managed_result_cannot_finish_a_newer_yaml_refresh() {
    for result in stale_results() {
        let (mut app, target) = app(true);
        let old_id = managed_request(&mut app, &target);
        let AppCommand::Action(ActionIntent::RefreshYaml { id, .. }) = app.refresh_content_once()
        else {
            panic!("newer YAML refresh must start");
        };
        assert_ne!(id, old_id);
        let due = app.yaml_refresh.due;
        app.show_managed_yaml(old_id, &target, result);
        assert!(app.yaml_refresh.pending);
        assert_eq!(app.yaml_refresh.id, id);
        assert_eq!(app.yaml_refresh.due, due);
        assert_eq!(app.content_managed_fields.as_deref(), Some(""));
        assert_eq!(app.content_body, "original YAML");
        assert!(app.notice.is_none());
        app.apply_yaml_refresh(
            id,
            &target,
            Ok(YamlContent {
                body: "newer YAML".to_owned(),
                with_managed_fields: Some(String::new()),
                decoded_body: None,
                decoded_with_managed_fields: None,
            }),
        );
        assert!(!app.yaml_refresh.pending);
        assert_eq!(app.content_body, "newer YAML");
    }
}

#[test]
fn matching_managed_failure_retains_body_and_allows_m_to_retry() {
    let (mut app, target) = app(false);
    let failed_id = managed_request(&mut app, &target);
    app.body_query = "metadata".to_owned();
    app.content_scroll = 7;
    app.content_horizontal_scroll = 3;
    app.show_managed_yaml(failed_id, &target, Err("forbidden".to_owned()));
    assert_eq!(app.overlay, Overlay::Content);
    assert_eq!(app.content_body, "original YAML");
    assert_eq!(
        app.content_decoded_body.as_deref(),
        Some("decoded original YAML")
    );
    assert_eq!(app.content_managed_fields.as_deref(), Some(""));
    assert!(!app.ui.managed_fields());
    assert!(!app.yaml_refresh.pending);
    assert!(app.notice.as_deref().unwrap().contains("forbidden"));
    assert_eq!(app.body_query, "metadata");
    assert_eq!(app.content_scroll, 7);
    assert_eq!(app.content_horizontal_scroll, 3);
    let retried_id = managed_request(&mut app, &target);
    assert_ne!(retried_id, failed_id);
    app.show_managed_yaml(
        retried_id,
        &target,
        Ok(("retried managed YAML".to_owned(), None)),
    );
    assert!(app.ui.managed_fields());
    assert!(!app.yaml_refresh.pending);
    assert_eq!(app.active_content_body(), "retried managed YAML");
    assert!(app.notice.is_none());
}

#[test]
fn matching_managed_success_preserves_search_scroll_and_other_view_flags() {
    let (mut app, target) = app(true);
    let id = managed_request(&mut app, &target);
    app.body_query = "metadata".to_owned();
    app.content_scroll = 11;
    app.content_horizontal_scroll = 4;
    app.ui.toggle(UiFlags::WRAP);
    app.ui.toggle(UiFlags::FULLSCREEN);
    app.ui.toggle(UiFlags::SECRET_DECODED);
    let mut expected_flags = app.ui;
    expected_flags.set(UiFlags::MANAGED_FIELDS, true);
    app.show_managed_yaml(
        id,
        &target,
        Ok((
            "encoded managed YAML".to_owned(),
            Some("decoded managed YAML".to_owned()),
        )),
    );
    assert_eq!(app.overlay, Overlay::Content);
    assert_eq!(app.content_body, "original YAML");
    assert_eq!(
        app.content_decoded_body.as_deref(),
        Some("decoded original YAML")
    );
    assert_eq!(
        app.content_managed_fields.as_deref(),
        Some("encoded managed YAML")
    );
    assert_eq!(
        app.content_decoded_managed_fields.as_deref(),
        Some("decoded managed YAML")
    );
    assert_eq!(app.active_content_body(), "decoded managed YAML");
    assert_eq!(app.ui, expected_flags);
    assert_eq!(app.body_query, "metadata");
    assert_eq!(app.content_scroll, 11);
    assert_eq!(app.content_horizontal_scroll, 4);
    assert!(app.content_auto_refresh());
    assert!(!app.yaml_refresh.pending);
}

#[test]
fn matching_id_still_requires_yaml_kind_and_the_exact_target() {
    for result in stale_results() {
        let (mut app, target) = app(false);
        let id = managed_request(&mut app, &target);
        let mut other = target.clone();
        other.namespace = Some("other-team".to_owned());
        app.show_managed_yaml(id, &other, result.clone());
        assert!(app.yaml_refresh.pending);
        assert!(app.notice.is_none());
        app.content_kind = Some(ContentKind::Describe);
        app.show_managed_yaml(id, &target, result);
        assert!(app.yaml_refresh.pending);
        assert_eq!(app.content_body, "original YAML");
        assert_eq!(app.content_managed_fields.as_deref(), Some(""));
        assert!(!app.ui.managed_fields());
        assert!(app.notice.is_none());
    }
}
