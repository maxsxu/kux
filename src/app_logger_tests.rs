use super::{ActionIntent, App, AppCommand, ContentKind, KeyCode, KeyEvent, KeyModifiers, Overlay};
use crate::{
    config::{LoggerSettings, UserConfig},
    model::ResourceKind,
    source::{LogRange, LogRequest, LogTarget},
};

fn app_with_logger(settings: LoggerSettings) -> App {
    let mut app = App::new(
        "demo",
        None,
        ResourceKind::Pods,
        false,
        vec!["demo".to_owned()],
    );
    let mut config = UserConfig::default();
    config.settings.logger = settings;
    app.apply_user_config(config);
    app
}

fn settings() -> LoggerSettings {
    LoggerSettings {
        tail: 7,
        buffer: 3,
        since_seconds: 90,
        text_wrap: true,
        disable_autoscroll: true,
        column_lock: true,
        show_time: true,
    }
}

fn target(previous: bool) -> LogTarget {
    LogTarget::pod("prod", "api-0", Some("app".to_owned()), previous)
}

fn started(command: AppCommand) -> LogRequest {
    let AppCommand::Action(ActionIntent::StartLogs { request }) = command else {
        panic!("expected log stream request");
    };
    request
}

fn key(character: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE)
}

#[test]
fn opening_live_and_previous_logs_applies_all_configured_defaults() {
    for previous in [false, true] {
        let mut app = app_with_logger(settings());
        let request = started(app.open_logs(target(previous)));
        assert_eq!(request.target, target(previous));
        assert_eq!(request.tail_count, 7);
        assert_eq!(request.range, LogRange::SinceSeconds(90));
        assert_eq!(app.content_scroll, 0);
        assert_eq!(
            app.content_kind,
            Some(if previous {
                ContentKind::PreviousLogs
            } else {
                ContentKind::Logs
            })
        );
        assert!(app.ui.wrap());
        assert!(app.ui.timestamps());
        let view = app.log_view.as_ref().unwrap();
        assert!(!view.auto_scroll);
        assert!(view.column_lock);
        assert_eq!(view.buffer_size(), 3);
        app.append_log_batch(
            request.id,
            (1..=4)
                .map(|index| format!("2026-09-07T10:00:0{index}Z line-{index}").into())
                .collect(),
            false,
            None,
        );
        assert_eq!(app.content_body.lines().count(), 3);
        assert!(!app.content_body.contains("line-1"));
        assert_eq!(
            app.log_export_text(),
            "2026-09-07T10:00:02Z line-2\n2026-09-07T10:00:03Z line-3\n2026-09-07T10:00:04Z line-4"
        );
        assert_eq!(app.content_scroll, 0);
    }
}

#[test]
fn changing_range_preserves_toggles_while_reopening_restores_defaults() {
    let mut app = app_with_logger(settings());
    let initial = started(app.open_logs(target(false)));
    for character in ['s', 'L', 't', 'w'] {
        app.handle_key(key(character));
    }
    assert!(app.log_view.as_ref().unwrap().auto_scroll);
    assert!(!app.log_view.as_ref().unwrap().column_lock);
    assert!(!app.ui.timestamps());
    assert!(!app.ui.wrap());
    let ranged = started(app.handle_key(key('2')));
    assert_ne!(ranged.id, initial.id);
    assert_eq!(ranged.range, LogRange::SinceSeconds(60));
    assert_eq!(ranged.tail_count, 7);
    assert_eq!(app.log_view.as_ref().unwrap().buffer_size(), 3);
    assert!(app.log_view.as_ref().unwrap().auto_scroll);
    assert!(!app.log_view.as_ref().unwrap().column_lock);
    assert!(!app.ui.timestamps());
    assert!(!app.ui.wrap());
    let command = app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert_eq!(
        command,
        AppCommand::Action(ActionIntent::StopLogs { id: ranged.id })
    );
    assert_eq!(app.overlay, Overlay::None);
    let reopened = started(app.open_logs(target(false)));
    assert_ne!(reopened.id, ranged.id);
    assert_eq!(reopened.range, LogRange::SinceSeconds(90));
    assert_eq!(reopened.tail_count, 7);
    assert!(!app.log_view.as_ref().unwrap().auto_scroll);
    assert!(app.log_view.as_ref().unwrap().column_lock);
    assert!(app.ui.timestamps());
    assert!(app.ui.wrap());
    assert_eq!(app.content_scroll, 0);
}

#[test]
fn reloaded_logger_defaults_take_effect_on_the_next_opening() {
    let mut app = app_with_logger(settings());
    let initial = started(app.open_logs(target(false)));
    let mut config = app.user_config.clone();
    config.settings.logger = LoggerSettings {
        tail: 12,
        buffer: 2,
        ..LoggerSettings::default()
    };
    app.apply_user_config(config);
    assert_eq!(app.log_view.as_ref().unwrap().request, initial);
    assert_eq!(app.log_view.as_ref().unwrap().buffer_size(), 3);
    assert!(app.ui.wrap());
    assert!(app.ui.timestamps());
    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    let reopened = started(app.open_logs(target(false)));
    assert_eq!(reopened.tail_count, 12);
    assert_eq!(reopened.range, LogRange::Tail);
    let view = app.log_view.as_ref().unwrap();
    assert_eq!(view.buffer_size(), 2);
    assert!(view.auto_scroll);
    assert!(!view.column_lock);
    assert!(!app.ui.wrap());
    assert!(!app.ui.timestamps());
    assert_eq!(app.content_scroll, u16::MAX);
}

#[test]
fn configured_capacity_applies_when_resuming_and_inserting_marks() {
    let mut app = app_with_logger(settings());
    let request = started(app.open_logs(target(false)));
    app.append_log_batch(request.id, vec!["first".into()], false, None);
    app.append_log_batch(
        request.id,
        vec!["second".into(), "third".into(), "fourth".into()],
        false,
        None,
    );
    assert_eq!(app.content_body, "first\n", "paused display stays fixed");
    app.handle_key(key('s'));
    assert_eq!(app.content_body, "second\nthird\nfourth\n");
    app.handle_key(key('m'));
    assert_eq!(app.content_body.lines().count(), 3);
    assert!(!app.content_body.contains("second"));
    assert!(app.content_body.contains("third"));
    assert!(app.content_body.contains("fourth"));
}
