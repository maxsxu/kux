use super::*;
use crate::{app::ListMode, config::UserConfig, source::SanitizeReport};

fn app(namespace: Option<&str>) -> App {
    App::new(
        "demo",
        namespace.map(str::to_owned),
        ResourceKind::Pods,
        false,
        vec!["demo".to_owned()],
    )
}

fn press(app: &mut App, code: KeyCode) -> AppCommand {
    let result = app.handle_key(KeyEvent::new(code, KeyModifiers::NONE));
    if code == KeyCode::Char('z') && app.overlay == Overlay::SanitizeDialog {
        render(app, 100, 30);
    }
    result
}

fn render(app: &mut App, width: u16, height: u16) -> String {
    let mut terminal =
        ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| crate::ui::draw(frame, app)).unwrap();
    terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect()
}

fn type_phrase(app: &mut App, value: &str) {
    for character in value.chars() {
        assert_eq!(press(app, KeyCode::Char(character)), AppCommand::None);
    }
}

fn submit(app: &mut App) -> AppCommand {
    press(app, KeyCode::Tab);
    press(app, KeyCode::Tab);
    press(app, KeyCode::Enter)
}

#[test]
fn namespace_scope_is_captured_without_selection_and_filters_do_not_narrow_it() {
    for namespace in [None, Some("team")] {
        let mut app = app(namespace);
        app.query = "api".to_owned();
        app.field_selector = Some("spec.nodeName=worker".to_owned());
        app.label_selector = Some("app=api".to_owned());
        app.marks.insert("team/marked".into());
        assert_eq!(press(&mut app, KeyCode::Char('z')), AppCommand::None);
        assert_eq!(app.overlay, Overlay::SanitizeDialog);
        assert!(app.selected_row().is_none());
        type_phrase(&mut app, SANITIZE_CONFIRMATION);
        assert_eq!(
            submit(&mut app),
            AppCommand::Action(ActionIntent::SanitizePods {
                namespace: namespace.map(str::to_owned)
            })
        );
        assert_eq!(app.overlay, Overlay::None);
        assert!(app.sanitize_dialog.is_none());
    }
}

#[test]
fn confirmation_requires_exact_phrase_and_explicit_ok() {
    for invalid in ["", "yes please!", "Yes Please", "Yes Please! "] {
        let mut app = app(Some("team"));
        press(&mut app, KeyCode::Char('z'));
        type_phrase(&mut app, invalid);
        assert_eq!(submit(&mut app), AppCommand::None);
        assert_eq!(app.overlay, Overlay::SanitizeDialog);
        assert!(app.notice.as_deref().unwrap().contains("exactly"));
    }
    let mut app = app(None);
    press(&mut app, KeyCode::Char('z'));
    type_phrase(&mut app, SANITIZE_CONFIRMATION);
    assert_eq!(press(&mut app, KeyCode::Enter), AppCommand::None);
    assert_eq!(app.sanitize_dialog.as_ref().unwrap().focus, 1);
    assert_eq!(press(&mut app, KeyCode::Enter), AppCommand::None);
    assert_eq!(app.overlay, Overlay::None, "Cancel is the first button");
}

#[test]
fn escape_cancels_and_modal_text_does_not_trigger_navigation_or_hotkeys() {
    let mut app = app(Some("team"));
    press(&mut app, KeyCode::Char('z'));
    type_phrase(&mut app, ":pods/[123]zy");
    assert_eq!(
        app.sanitize_dialog.as_ref().unwrap().confirmation,
        ":pods/[123]zy"
    );
    assert_eq!(app.namespace.as_deref(), Some("team"));
    assert_eq!(app.overlay, Overlay::SanitizeDialog);
    app.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
    assert_eq!(app.sanitize_dialog.as_ref().unwrap().confirmation, "");
    press(&mut app, KeyCode::Esc);
    assert!(app.sanitize_dialog.is_none());
    assert_eq!(app.overlay, Overlay::None);
}

#[test]
fn non_pod_and_container_views_have_no_sanitize_action() {
    let mut app = app(None);
    app.kind = ResourceKind::Deployments.into();
    press(&mut app, KeyCode::Char('z'));
    assert!(app.sanitize_dialog.is_none());
    app.kind = ResourceKind::Pods.into();
    app.list_mode = ListMode::Containers {
        namespace: "team".to_owned(),
        pod: "api".to_owned(),
    };
    press(&mut app, KeyCode::Char('z'));
    assert!(app.sanitize_dialog.is_none());
}

#[test]
fn readonly_reload_and_context_loading_close_confirmation() {
    for context_load in [false, true] {
        let mut app = app(None);
        press(&mut app, KeyCode::Char('z'));
        type_phrase(&mut app, SANITIZE_CONFIRMATION);
        if context_load {
            app.begin_config_load();
        } else {
            let mut config = UserConfig::default();
            config.settings.read_only = true;
            app.apply_user_config(config);
        }
        assert!(app.sanitize_dialog.is_none());
        assert_eq!(app.overlay, Overlay::None);
        press(&mut app, KeyCode::Char('z'));
        assert!(app.sanitize_dialog.is_none());
        assert!(app.notice.as_deref().unwrap().contains("read-only"));
    }
}

#[test]
fn submission_rechecks_readonly_context_and_generation() {
    for change in 0..3 {
        let mut app = app(Some("team"));
        press(&mut app, KeyCode::Char('z'));
        type_phrase(&mut app, SANITIZE_CONFIRMATION);
        match change {
            0 => app.read_only = true,
            1 => app.generation += 1,
            _ => app.context = "changed".to_owned(),
        }
        assert_eq!(submit(&mut app), AppCommand::None);
        assert!(app.sanitize_dialog.is_none());
        assert!(app.notice.is_some());
    }
}

#[test]
fn partial_failure_reports_counts_without_dismissing_a_new_dialog() {
    let mut app = app(None);
    press(&mut app, KeyCode::Char('z'));
    type_phrase(&mut app, "new input");
    app.show_sanitized(
        Some("team"),
        Ok(SanitizeReport {
            deleted: 2,
            eligible: 5,
            failure: Some("team/stale changed".to_owned()),
        }),
    );
    assert_eq!(app.overlay, Overlay::SanitizeDialog);
    assert_eq!(
        app.sanitize_dialog.as_ref().unwrap().confirmation,
        "new input"
    );
    assert_eq!(
        app.notice.as_deref(),
        Some("Deleted 2 of 5 eligible Pods in namespace team; stopped: team/stale changed")
    );
}

#[test]
fn control_c_respects_the_configured_exit_behavior_inside_confirmation() {
    let mut app = app(None);
    press(&mut app, KeyCode::Char('z'));
    let control_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
    assert_eq!(app.handle_key(control_c), AppCommand::Quit);
    app.user_config.settings.no_exit_on_ctrl_c = true;
    assert_eq!(app.handle_key(control_c), AppCommand::None);
    assert_eq!(app.overlay, Overlay::SanitizeDialog);
}

#[test]
fn clipped_scope_blocks_confirmation_until_the_terminal_is_large_enough() {
    let mut app = app(Some("production"));
    press(&mut app, KeyCode::Char('z'));
    type_phrase(&mut app, SANITIZE_CONFIRMATION);
    let small = render(&mut app, 40, 12);
    assert!(small.contains("Resize to confirm"));
    assert!(!app.sanitize_dialog.as_ref().unwrap().scope_visible);
    assert_eq!(submit(&mut app), AppCommand::None);
    assert_eq!(app.overlay, Overlay::SanitizeDialog);
    let normal = render(&mut app, 100, 30);
    for expected in [
        "namespace production",
        "Pending",
        "Filters and marks are ignored",
        "zero grace",
        "[ OK ]",
    ] {
        assert!(normal.contains(expected), "missing {expected}");
    }
    assert!(app.sanitize_dialog.as_ref().unwrap().scope_visible);
    assert_eq!(
        press(&mut app, KeyCode::Enter),
        AppCommand::Action(ActionIntent::SanitizePods {
            namespace: Some("production".to_owned())
        })
    );
}

#[test]
fn zero_or_tiny_width_cannot_enable_confirmation_without_visible_scope() {
    for width in [0, 1, 2, 20] {
        let mut app = app(None);
        press(&mut app, KeyCode::Char('z'));
        type_phrase(&mut app, SANITIZE_CONFIRMATION);
        render(&mut app, width, 30);
        assert!(
            !app.sanitize_dialog.as_ref().unwrap().scope_visible,
            "width {width}"
        );
        assert_eq!(submit(&mut app), AppCommand::None, "width {width}");
        assert_eq!(app.overlay, Overlay::SanitizeDialog);
    }
}
