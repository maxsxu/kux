use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::*;
use crate::{config::UserConfig, model::ResourceKind};

fn app() -> App {
    App::new(
        "demo",
        None,
        ResourceKind::Pods,
        false,
        vec!["demo".to_owned()],
    )
}

fn target(name: &str) -> ResourceTarget {
    ResourceTarget {
        kind: ResourceKind::Pods.into(),
        namespace: Some("team".to_owned()),
        name: name.to_owned(),
    }
}

fn begin(app: &mut App, target: &ResourceTarget, kind: ContentKind) -> u64 {
    let AppCommand::Action(ActionIntent::Content {
        id,
        target: actual,
        content,
    }) = app.request_content(target.clone(), kind)
    else {
        panic!("expected initial content request");
    };
    assert_eq!(&actual, target);
    assert_eq!(content, kind);
    assert_eq!(app.overlay, Overlay::Content);
    assert_eq!(app.content_kind, Some(kind));
    assert_eq!(app.content_body, "Loading…");
    assert!(app.yaml_refresh.pending);
    id
}

fn content(kind: ContentKind, body: &str) -> ActionResult {
    ActionResult::Content {
        title: "Opened resource".to_owned(),
        body: body.to_owned(),
        managed_fields: (kind == ContentKind::Yaml).then(String::new),
        decoded_body: None,
        decoded_managed_fields: None,
        content: kind,
    }
}

#[test]
fn closing_initial_load_cancels_and_ignores_late_success_and_error_for_all_content_kinds() {
    for kind in [
        ContentKind::Yaml,
        ContentKind::Describe,
        ContentKind::SecretData,
    ] {
        let mut app = app();
        let target = target("first");
        let id = begin(&mut app, &target, kind);
        assert_eq!(app.cancel_inactive_content_refresh(), None);
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(
            app.cancel_inactive_content_refresh(),
            Some(ActionIntent::StopContentRefresh)
        );
        assert!(!app.yaml_refresh.pending);
        assert_eq!(app.yaml_refresh.opening, None);
        app.apply_opened_content(id, &target, content(kind, "stale"));
        app.apply_opened_content(id, &target, ActionResult::Error("stale failure".to_owned()));
        assert_eq!(app.overlay, Overlay::None);
        assert_eq!(app.notice, None);
        assert_eq!(app.cancel_inactive_content_refresh(), None);
    }
}

#[test]
fn second_open_and_same_target_reopen_reject_prior_payloads_in_the_same_generation() {
    for next in [target("first"), target("second")] {
        for kind in [
            ContentKind::Yaml,
            ContentKind::Describe,
            ContentKind::SecretData,
        ] {
            let mut app = app();
            let first = target("first");
            let old_id = begin(&mut app, &first, kind);
            let generation = app.generation;
            let id = begin(&mut app, &next, kind);
            assert_ne!(id, old_id);
            assert_eq!(app.generation, generation);
            app.apply_opened_content(old_id, &first, content(kind, "old body"));
            app.apply_opened_content(
                old_id,
                &first,
                ActionResult::Error("old failure".to_owned()),
            );
            assert_eq!(app.content_body, "Loading…");
            assert_eq!(app.notice, None);
            assert!(app.yaml_refresh.pending);
            app.apply_opened_content(id, &next, content(kind, "current body"));
            assert_eq!(app.content_body, "current body");
            assert_eq!(app.content_title, "Opened resource");
            assert_eq!(app.content_target.as_ref(), Some(&next));
            assert!(!app.yaml_refresh.pending);
            assert_eq!(app.yaml_refresh.opening, None);
            app.apply_opened_content(
                id,
                &next,
                ActionResult::Error("duplicate failure".to_owned()),
            );
            assert_eq!(app.notice, None);
        }
    }
}

#[test]
fn initial_success_preserves_open_time_defaults_and_changes_made_while_loading() {
    for enabled in [false, true] {
        let mut app = app();
        let mut config = UserConfig::default();
        config.settings.live_view_auto_refresh = enabled;
        app.apply_user_config(config.clone());
        let target = target("first");
        let id = begin(&mut app, &target, ContentKind::Yaml);
        config.settings.live_view_auto_refresh = !enabled;
        config.settings.refresh_rate = Some(Duration::from_secs(600));
        app.apply_user_config(config);
        app.body_query = "spec".to_owned();
        app.content_scroll = 2;
        app.content_horizontal_scroll = 3;
        app.ui.toggle(UiFlags::WRAP);
        app.ui.toggle(UiFlags::FULLSCREEN);
        let flags = app.ui;
        let finished = Instant::now();
        app.apply_opened_content(id, &target, content(ContentKind::Yaml, "spec: updated"));
        assert_eq!(app.content_body, "spec: updated");
        assert_eq!(app.body_query, "spec");
        assert_eq!(app.content_scroll, 2);
        assert_eq!(app.content_horizontal_scroll, 3);
        assert_eq!(app.ui, flags);
        assert_eq!(app.content_auto_refresh(), enabled);
        assert!(app.yaml_refresh.due >= finished + LIVE_REFRESH_INTERVAL);
        assert_eq!(app.poll_yaml_refresh(), AppCommand::None);
        begin(&mut app, &target, ContentKind::Yaml);
        assert_eq!(app.content_auto_refresh(), !enabled);
    }
}

#[test]
fn toggling_refresh_while_initial_read_is_pending_preserves_that_read() {
    for enabled in [false, true] {
        for kind in [ContentKind::Yaml, ContentKind::Describe] {
            let mut app = app();
            app.user_config.settings.live_view_auto_refresh = enabled;
            let target = target("first");
            let id = begin(&mut app, &target, kind);
            assert_eq!(
                app.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE)),
                AppCommand::None
            );
            assert_eq!(app.content_auto_refresh(), !enabled);
            assert!(app.yaml_refresh.pending);
            assert_eq!(app.yaml_refresh.id, id);
            assert_eq!(app.poll_yaml_refresh(), AppCommand::None);
            app.apply_opened_content(id, &target, content(kind, "loaded"));
            assert_eq!(app.content_body, "loaded");
            assert_eq!(app.content_auto_refresh(), !enabled);
            app.yaml_refresh.due = Instant::now();
            assert_eq!(app.poll_yaml_refresh() == AppCommand::None, enabled);
        }
    }
}

#[test]
fn forced_refresh_invalidates_initial_result_and_loading_is_not_reopened() {
    for kind in [ContentKind::Yaml, ContentKind::Describe] {
        let mut app = app();
        let target = target("first");
        let initial_id = begin(&mut app, &target, kind);
        app.pause_content_refresh();
        assert_eq!(app.yaml_refresh.opening, None);
        app.refresh_content_once();
        let id = app.yaml_refresh.id;
        assert_ne!(id, initial_id);
        app.apply_opened_content(initial_id, &target, content(kind, "old"));
        assert!(app.yaml_refresh.pending);
        if kind == ContentKind::Yaml {
            app.apply_yaml_refresh(
                id,
                &target,
                Ok(YamlContent {
                    body: "refreshed".to_owned(),
                    with_managed_fields: None,
                    decoded_body: None,
                    decoded_with_managed_fields: None,
                }),
            );
        } else {
            app.apply_describe_refresh(id, &target, Ok("refreshed".to_owned()));
        }
        app.apply_opened_content(initial_id, &target, ActionResult::Error("late".to_owned()));
        assert_eq!(app.content_body, "refreshed");
        assert_eq!(app.notice, None);
        assert!(!app.content_auto_refresh());
    }
}

#[test]
fn initial_failure_clears_loading_and_successful_retry_clears_failure_notice() {
    for kind in [ContentKind::Yaml, ContentKind::Describe] {
        let mut app = app();
        let target = target("first");
        let id = begin(&mut app, &target, kind);
        app.apply_opened_content(id, &target, ActionResult::Error("unavailable".to_owned()));
        assert_eq!(app.content_body.len(), 0);
        assert_eq!(app.overlay, Overlay::Content);
        assert!(!app.yaml_refresh.pending);
        assert_eq!(app.yaml_refresh.opening, None);
        assert!(app.notice.as_deref().unwrap().contains("unavailable"));
        app.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE));
        let retry = app.yaml_refresh.id;
        if kind == ContentKind::Yaml {
            app.apply_yaml_refresh(
                retry,
                &target,
                Ok(YamlContent {
                    body: "recovered".to_owned(),
                    with_managed_fields: None,
                    decoded_body: None,
                    decoded_with_managed_fields: None,
                }),
            );
        } else {
            app.apply_describe_refresh(retry, &target, Ok("recovered".to_owned()));
        }
        assert_eq!(app.content_body, "recovered");
        assert!(
            !app.notice
                .as_deref()
                .is_some_and(|notice| notice.contains("unavailable"))
        );
    }
}

#[test]
fn wrong_target_or_kind_and_non_opening_content_cannot_accept_initial_results() {
    let mut app = app();
    let target = target("first");
    let id = begin(&mut app, &target, ContentKind::Yaml);
    let mut wrong_target = target.clone();
    wrong_target.namespace = Some("other".to_owned());
    app.apply_opened_content(id, &wrong_target, content(ContentKind::Yaml, "wrong"));
    app.apply_opened_content(id, &target, content(ContentKind::Describe, "wrong kind"));
    assert_eq!(app.content_body, "Loading…");
    assert!(app.yaml_refresh.pending);
    for kind in [ContentKind::Logs, ContentKind::ScreenDump] {
        app.show_content("Local content", "keep", None, None, None, kind);
        app.apply_opened_content(id, &target, ActionResult::Error("late".to_owned()));
        assert_eq!(app.content_body, "keep");
        assert_eq!(app.notice, None);
    }
}
