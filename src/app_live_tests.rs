use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::*;
use crate::{
    config::UserConfig,
    model::{ResourceKind, ResourceRow, ResourceSnapshot, SourceState},
};

fn app_with_resources() -> App {
    let mut app = App::new(
        "demo",
        None,
        ResourceKind::Pods,
        false,
        vec!["demo".to_owned()],
    );
    app.apply_snapshot(ResourceSnapshot {
        generation: app.generation,
        kind: ResourceKind::Pods.into(),
        rows: Arc::new(vec![
            Arc::new(ResourceRow::new("prod", "api", "Running", "1/1", "0", 0)),
            Arc::new(ResourceRow::new("prod", "worker", "Running", "1/1", "0", 0)),
        ]),
        table_columns: Arc::from([]),
        state: SourceState::Ready,
        revision: 1,
        emitted_at: Instant::now(),
    });
    app
}

fn press(app: &mut App, key: char) -> AppCommand {
    app.handle_key(KeyEvent::new(KeyCode::Char(key), KeyModifiers::NONE))
}

fn open(app: &mut App, kind: ContentKind) -> ResourceTarget {
    app.overlay = Overlay::None;
    let key = if kind == ContentKind::Describe {
        'd'
    } else {
        'y'
    };
    let AppCommand::Action(ActionIntent::Content {
        target, content, ..
    }) = press(app, key)
    else {
        panic!("expected a content action");
    };
    assert_eq!(content, kind);
    app.show_content("Opened resource", "original", None, None, None, kind);
    target
}

fn refresh_id(command: AppCommand, expected: &ResourceTarget, kind: ContentKind) -> u64 {
    match command {
        AppCommand::Action(ActionIntent::RefreshYaml { id, target, .. })
            if kind == ContentKind::Yaml =>
        {
            assert_eq!(&target, expected);
            id
        }
        AppCommand::Action(ActionIntent::RefreshDescribe { id, target })
            if kind == ContentKind::Describe =>
        {
            assert_eq!(&target, expected);
            id
        }
        command => panic!("expected {kind:?} refresh, got {command:?}"),
    }
}

fn apply_result(
    app: &mut App,
    id: u64,
    target: &ResourceTarget,
    kind: ContentKind,
    result: Result<&str, &str>,
) {
    let result = result.map(str::to_owned).map_err(str::to_owned);
    if kind == ContentKind::Describe {
        app.apply_describe_refresh(id, target, result);
    } else {
        app.apply_yaml_refresh(
            id,
            target,
            result.map(|body| YamlContent {
                body,
                with_managed_fields: None,
                decoded_body: None,
                decoded_with_managed_fields: None,
            }),
        );
    }
}

#[test]
fn config_default_is_off_and_reload_applies_to_the_next_opened_live_view() {
    for kind in [ContentKind::Yaml, ContentKind::Describe] {
        let mut app = app_with_resources();
        open(&mut app, kind);
        assert!(app.is_live_content());
        assert!(!app.content_auto_refresh());
        app.yaml_refresh.due = Instant::now();
        assert_eq!(app.poll_yaml_refresh(), AppCommand::None);

        let mut config = UserConfig::default();
        config.settings.live_view_auto_refresh = true;
        app.apply_user_config(config.clone());
        assert!(
            !app.content_auto_refresh(),
            "reload preserves the open view"
        );
        let opened = Instant::now();
        open(&mut app, kind);
        assert!(app.content_auto_refresh());
        assert!(app.yaml_refresh.due >= opened + LIVE_REFRESH_INTERVAL);
        assert!(app.yaml_refresh.due <= Instant::now() + LIVE_REFRESH_INTERVAL);
        assert_eq!(app.poll_yaml_refresh(), AppCommand::None);

        config.settings.live_view_auto_refresh = false;
        app.apply_user_config(config);
        assert!(app.content_auto_refresh(), "reload preserves the open view");
        open(&mut app, kind);
        assert!(!app.content_auto_refresh());
        app.yaml_refresh.due = Instant::now();
        assert_eq!(app.poll_yaml_refresh(), AppCommand::None);
    }
}

#[test]
fn r_immediately_enables_and_cancels_reads_and_rejects_results_from_prior_toggle() {
    for kind in [ContentKind::Yaml, ContentKind::Describe] {
        let mut app = app_with_resources();
        app.read_only = true;
        let target = open(&mut app, kind);
        let old_id = refresh_id(press(&mut app, 'r'), &target, kind);
        assert!(app.content_auto_refresh());
        assert!(app.yaml_refresh.pending);
        assert_eq!(app.poll_yaml_refresh(), AppCommand::None);
        assert!(!app.user_config.settings.live_view_auto_refresh);

        assert_eq!(
            press(&mut app, 'r'),
            AppCommand::Action(ActionIntent::StopContentRefresh)
        );
        assert!(!app.content_auto_refresh());
        assert!(!app.yaml_refresh.pending);
        let disabled_notice = app.notice.clone();
        apply_result(&mut app, old_id, &target, kind, Ok("stale body"));
        apply_result(&mut app, old_id, &target, kind, Err("stale failure"));
        assert_eq!(app.content_body, "original");
        assert_eq!(app.notice, disabled_notice);

        let new_id = refresh_id(press(&mut app, 'r'), &target, kind);
        assert_ne!(new_id, old_id);
        apply_result(&mut app, old_id, &target, kind, Ok("stale body"));
        assert!(app.yaml_refresh.pending);
        assert_eq!(app.content_body, "original");
        apply_result(&mut app, new_id, &target, kind, Ok("current body"));
        assert!(!app.yaml_refresh.pending);
        assert_eq!(app.content_body, "current body");
    }
}

#[test]
fn describe_refresh_preserves_view_state_and_rejects_wrong_target_kind_or_generation() {
    let mut app = app_with_resources();
    app.user_config.settings.live_view_auto_refresh = true;
    let target = open(&mut app, ContentKind::Describe);
    app.selected = 1;
    app.content_scroll = 7;
    app.content_horizontal_scroll = 3;
    app.body_query = "Ready|Events".to_owned();
    app.ui.toggle(UiFlags::WRAP);
    app.ui.toggle(UiFlags::FULLSCREEN);
    let flags = app.ui;
    app.yaml_refresh.due = Instant::now();
    let id = refresh_id(app.poll_yaml_refresh(), &target, ContentKind::Describe);
    assert_ne!(app.selected_target().as_ref(), Some(&target));

    let mut other_name = target.clone();
    other_name.name = "worker".to_owned();
    let mut other_namespace = target.clone();
    other_namespace.namespace = Some("dev".to_owned());
    let mut other_kind = target.clone();
    other_kind.kind = ResourceKind::Deployments.into();
    for wrong_target in [other_name, other_namespace, other_kind] {
        app.apply_describe_refresh(id, &wrong_target, Ok("wrong target".to_owned()));
        app.apply_describe_refresh(id, &wrong_target, Err("wrong failure".to_owned()));
    }
    app.apply_describe_refresh(
        id.wrapping_add(1),
        &target,
        Ok("wrong generation".to_owned()),
    );
    apply_result(
        &mut app,
        id,
        &target,
        ContentKind::Yaml,
        Ok("wrong content kind"),
    );
    assert_eq!(app.content_body, "original");
    assert_eq!(app.notice, None);
    assert!(app.yaml_refresh.pending);

    let finished = Instant::now();
    app.apply_describe_refresh(id, &target, Ok("Ready: True\nEvents: created".to_owned()));
    assert_eq!(app.content_body, "Ready: True\nEvents: created");
    assert_eq!(app.content_title, "Opened resource");
    assert_eq!(app.content_target, Some(target));
    assert_eq!(app.content_scroll, 7);
    assert_eq!(app.content_horizontal_scroll, 3);
    assert_eq!(app.body_query, "Ready|Events");
    assert_eq!(app.ui, flags);
    assert_eq!(app.overlay, Overlay::Content);
    assert!(!app.yaml_refresh.pending);
    assert!(app.yaml_refresh.due >= finished + LIVE_REFRESH_INTERVAL);
    assert_eq!(app.poll_yaml_refresh(), AppCommand::None);
}

#[test]
fn failures_retain_body_and_recovery_resets_backoff_without_clearing_unrelated_notice() {
    for kind in [ContentKind::Yaml, ContentKind::Describe] {
        let mut app = app_with_resources();
        let target = open(&mut app, kind);
        let id = refresh_id(press(&mut app, 'r'), &target, kind);
        app.body_query = "keep filter".to_owned();
        app.content_scroll = 9;
        app.content_horizontal_scroll = 4;
        let failed = Instant::now();
        apply_result(&mut app, id, &target, kind, Err("unavailable"));
        assert_eq!(app.content_body, "original");
        assert_eq!(app.body_query, "keep filter");
        assert_eq!(app.content_scroll, 9);
        assert_eq!(app.content_horizontal_scroll, 4);
        assert!(!app.yaml_refresh.pending);
        assert!(!app.yaml_refresh.exhausted);
        assert!(app.yaml_refresh.due >= failed + Duration::from_millis(2_500));
        assert!(app.yaml_refresh.due <= Instant::now() + Duration::from_millis(7_500));
        assert!(
            app.notice
                .as_deref()
                .unwrap()
                .contains("refresh failed: unavailable")
        );

        app.yaml_refresh.due = Instant::now();
        let recovered_id = refresh_id(app.poll_yaml_refresh(), &target, kind);
        apply_result(&mut app, recovered_id, &target, kind, Ok("recovered"));
        assert_eq!(app.content_body, "recovered");
        assert_eq!(app.notice, None);
        assert_eq!(app.yaml_refresh.backoff, LIVE_REFRESH_INTERVAL);
        assert_eq!(app.poll_yaml_refresh(), AppCommand::None);

        app.yaml_refresh.due = Instant::now();
        let updated_id = refresh_id(app.poll_yaml_refresh(), &target, kind);
        app.notice = Some("Copied resource name".to_owned());
        apply_result(&mut app, updated_id, &target, kind, Ok("updated again"));
        assert_eq!(app.notice.as_deref(), Some("Copied resource name"));
    }
}

#[test]
fn repeated_failures_stop_within_retry_window_and_toggle_restarts_immediately() {
    for kind in [ContentKind::Yaml, ContentKind::Describe] {
        let mut app = app_with_resources();
        let target = open(&mut app, kind);
        let id = refresh_id(press(&mut app, 'r'), &target, kind);
        app.yaml_refresh.epoch = Instant::now().checked_sub(RETRY_WINDOW).unwrap();
        apply_result(&mut app, id, &target, kind, Err("unavailable"));
        assert_eq!(app.content_body, "original");
        assert!(app.yaml_refresh.exhausted);
        assert!(!app.yaml_refresh.pending);
        assert!(
            app.notice
                .as_deref()
                .unwrap()
                .contains("auto-refresh stopped")
        );
        app.yaml_refresh.due = Instant::now();
        assert_eq!(app.poll_yaml_refresh(), AppCommand::None);
        assert_eq!(
            press(&mut app, 'r'),
            AppCommand::Action(ActionIntent::StopContentRefresh)
        );
        let restarted = refresh_id(press(&mut app, 'r'), &target, kind);
        assert_ne!(restarted, id);
        assert!(!app.yaml_refresh.exhausted);
        assert_eq!(app.yaml_refresh.backoff, LIVE_REFRESH_INTERVAL);
        apply_result(&mut app, restarted, &target, kind, Ok("recovered"));
        assert_eq!(app.content_body, "recovered");
    }
}

#[test]
fn leaving_a_pending_view_cancels_once_and_reopening_rejects_the_old_result() {
    for kind in [ContentKind::Yaml, ContentKind::Describe] {
        let mut app = app_with_resources();
        let target = open(&mut app, kind);
        let id = refresh_id(press(&mut app, 'r'), &target, kind);
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.overlay, Overlay::None);
        assert_eq!(
            app.cancel_inactive_content_refresh(),
            Some(ActionIntent::StopContentRefresh)
        );
        assert_eq!(app.cancel_inactive_content_refresh(), None);
        assert!(!app.yaml_refresh.pending);
        apply_result(&mut app, id, &target, kind, Err("late failure"));
        assert_eq!(app.notice, None);
        assert_eq!(app.poll_yaml_refresh(), AppCommand::None);

        app.user_config.settings.live_view_auto_refresh = true;
        open(&mut app, kind);
        apply_result(&mut app, id, &target, kind, Ok("stale after reopen"));
        assert_eq!(app.content_body, "original");
        assert_eq!(app.cancel_inactive_content_refresh(), None);
    }
}

#[test]
fn fullscreen_setting_is_reapplied_when_opening_a_fresh_live_view() {
    for fullscreen in [false, true] {
        let mut app = app_with_resources();
        let mut config = UserConfig::default();
        config.settings.ui.fullscreen = fullscreen;
        app.apply_user_config(config);
        for kind in [ContentKind::Yaml, ContentKind::Describe] {
            open(&mut app, kind);
            assert_eq!(app.ui.fullscreen(), fullscreen);
            press(&mut app, 'f');
            assert_eq!(app.ui.fullscreen(), !fullscreen);
            open(&mut app, kind);
            assert_eq!(app.ui.fullscreen(), fullscreen);
        }
    }
}

#[test]
fn edit_and_one_shot_refresh_preserve_view_state_and_auto_refresh_preference() {
    for enabled in [false, true] {
        for kind in [ContentKind::Yaml, ContentKind::Describe] {
            let mut app = app_with_resources();
            app.user_config.settings.live_view_auto_refresh = enabled;
            let target = open(&mut app, kind);
            let old_id = refresh_id(app.refresh_content_once(), &target, kind);
            app.selected = 1;
            app.content_scroll = 9;
            app.content_horizontal_scroll = 4;
            app.body_query = "spec".to_owned();
            app.ui.toggle(UiFlags::WRAP);
            app.ui.toggle(UiFlags::FULLSCREEN);
            let flags = app.ui;

            assert_eq!(press(&mut app, 'e'), AppCommand::Edit(target.clone()));
            assert_eq!(app.overlay, Overlay::Content);
            app.pause_content_refresh();
            assert!(!app.yaml_refresh.pending);
            apply_result(&mut app, old_id, &target, kind, Ok("pre-edit response"));
            assert_eq!(app.content_body, "original");
            let edited_id = refresh_id(app.refresh_content_once(), &target, kind);
            assert_ne!(old_id, edited_id);
            assert_eq!(app.content_auto_refresh(), enabled);
            apply_result(&mut app, edited_id, &target, kind, Ok("edited body"));
            assert_eq!(app.content_body, "edited body");
            assert_eq!(app.content_title, "Opened resource");
            assert_eq!(app.content_target.as_ref(), Some(&target));
            assert_eq!(app.content_scroll, 9);
            assert_eq!(app.content_horizontal_scroll, 4);
            assert_eq!(app.body_query, "spec");
            assert_eq!(app.ui, flags);
            assert_eq!(app.content_auto_refresh(), enabled);
            assert!(!app.yaml_refresh.pending);
            assert_eq!(app.poll_yaml_refresh(), AppCommand::None);

            apply_result(&mut app, edited_id, &target, kind, Ok("duplicate response"));
            assert_eq!(app.content_body, "edited body");
            app.yaml_refresh.due = Instant::now();
            let next = app.poll_yaml_refresh();
            if enabled {
                let next_id = refresh_id(next, &target, kind);
                assert_ne!(next_id, edited_id);
            } else {
                assert_eq!(next, AppCommand::None);
            }
        }
    }
}

#[test]
fn failed_one_shot_retains_content_without_starting_disabled_auto_refresh() {
    for kind in [ContentKind::Yaml, ContentKind::Describe] {
        let mut app = app_with_resources();
        let target = open(&mut app, kind);
        let id = refresh_id(app.refresh_content_once(), &target, kind);
        apply_result(&mut app, id, &target, kind, Err("post-edit read failed"));
        assert_eq!(app.overlay, Overlay::Content);
        assert_eq!(app.content_body, "original");
        assert!(!app.content_auto_refresh());
        assert!(!app.yaml_refresh.pending);
        assert!(
            app.notice
                .as_deref()
                .unwrap()
                .contains("post-edit read failed")
        );
        app.yaml_refresh.due = Instant::now();
        assert_eq!(app.poll_yaml_refresh(), AppCommand::None);
    }
}

#[test]
fn successful_one_shot_after_exhaustion_restores_enabled_refresh_cadence() {
    let mut app = app_with_resources();
    app.user_config.settings.live_view_auto_refresh = true;
    let target = open(&mut app, ContentKind::Describe);
    let failed_id = refresh_id(app.refresh_content_once(), &target, ContentKind::Describe);
    app.yaml_refresh.epoch = Instant::now().checked_sub(RETRY_WINDOW).unwrap();
    app.apply_describe_refresh(failed_id, &target, Err("unavailable".to_owned()));
    assert!(app.yaml_refresh.exhausted);
    let recovered_id = refresh_id(app.refresh_content_once(), &target, ContentKind::Describe);
    app.apply_describe_refresh(recovered_id, &target, Ok("recovered".to_owned()));
    assert!(!app.yaml_refresh.exhausted);
    assert!(app.content_auto_refresh());
    assert_eq!(app.poll_yaml_refresh(), AppCommand::None);
    app.yaml_refresh.due = Instant::now();
    refresh_id(app.poll_yaml_refresh(), &target, ContentKind::Describe);
}

#[test]
fn retry_backoff_grows_with_bounded_jitter_and_stops_before_exceeding_window() {
    let mut state = LiveRefreshState::new();
    state.restart(true);
    let epoch = state.epoch;
    assert!(state.retry(epoch, 0.0));
    assert_eq!(state.due - epoch, Duration::from_millis(2_500));
    assert!(state.retry(epoch, 2.0));
    assert_eq!(state.due - epoch, Duration::from_millis(11_250));
    state.backoff = Duration::from_secs(60);
    assert!(state.retry(epoch + Duration::from_secs(30), 1.5));
    assert_eq!(state.due - epoch, RETRY_WINDOW);
    assert_eq!(state.backoff, Duration::from_secs(60));
    assert!(!state.retry(
        epoch + Duration::from_secs(30) + Duration::from_nanos(1),
        1.5
    ));
    assert!(state.exhausted);
}
