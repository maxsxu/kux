use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::{ActionIntent, App, AppCommand, Overlay, ResourceKind};

pub const SANITIZE_CONFIRMATION: &str = "Yes Please!";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SanitizeDialog {
    pub context: String,
    pub namespace: Option<String>,
    pub confirmation: String,
    pub focus: usize,
    pub scope_visible: bool,
    generation: u64,
}

impl App {
    pub fn show_sanitized(
        &mut self,
        namespace: Option<&str>,
        result: Result<crate::source::SanitizeReport, String>,
    ) {
        let scope = namespace.map_or_else(
            || "all namespaces".to_owned(),
            |value| format!("namespace {value}"),
        );
        let message = match result {
            Ok(report) => {
                let summary = format!(
                    "Deleted {} of {} eligible Pods in {scope}",
                    report.deleted, report.eligible
                );
                report.failure.map_or(summary.clone(), |error| {
                    format!("{summary}; stopped: {error}")
                })
            }
            Err(error) => format!("Pod sanitization failed in {scope}: {error}"),
        };
        self.show_transient_notice(message);
    }

    pub(super) fn open_sanitize_dialog(&mut self) -> AppCommand {
        if !self.is_resource_view() || self.kind.known_kind() != Some(ResourceKind::Pods) {
            return AppCommand::None;
        }
        if self.read_only {
            return self.unsupported("Pod sanitization is disabled in read-only mode");
        }
        self.sanitize_dialog = Some(SanitizeDialog {
            context: self.context.clone(),
            namespace: self.namespace.clone(),
            confirmation: String::new(),
            focus: 0,
            scope_visible: false,
            generation: self.generation,
        });
        self.overlay = Overlay::SanitizeDialog;
        self.notice = None;
        AppCommand::None
    }

    pub(super) fn close_sanitize_dialog(&mut self) {
        self.sanitize_dialog = None;
        if self.overlay == Overlay::SanitizeDialog {
            self.overlay = Overlay::None;
        }
    }

    pub(super) fn handle_sanitize_key(&mut self, key: KeyEvent) -> AppCommand {
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return if self.user_config.settings.no_exit_on_ctrl_c {
                AppCommand::None
            } else {
                AppCommand::Quit
            };
        }
        if key.code == KeyCode::Esc {
            self.close_sanitize_dialog();
            self.notice = None;
            return AppCommand::None;
        }
        let Some(dialog) = self.sanitize_dialog.as_mut() else {
            self.overlay = Overlay::None;
            return AppCommand::None;
        };
        match key.code {
            KeyCode::Tab | KeyCode::Down => dialog.focus = (dialog.focus + 1) % 3,
            KeyCode::BackTab | KeyCode::Up => dialog.focus = (dialog.focus + 2) % 3,
            KeyCode::Left | KeyCode::Right if dialog.focus != 0 => {
                dialog.focus = if dialog.focus == 1 { 2 } else { 1 };
            }
            KeyCode::Enter => match dialog.focus {
                0 => dialog.focus = 1,
                1 => self.close_sanitize_dialog(),
                _ => return self.submit_sanitize(),
            },
            KeyCode::Backspace if dialog.focus == 0 => {
                dialog.confirmation.pop();
            }
            KeyCode::Char('u')
                if dialog.focus == 0 && key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                dialog.confirmation.clear();
            }
            KeyCode::Char(character)
                if dialog.focus == 0
                    && !character.is_control()
                    && !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                    && dialog.confirmation.len() < 128 =>
            {
                dialog.confirmation.push(character);
            }
            _ => {}
        }
        AppCommand::None
    }

    fn submit_sanitize(&mut self) -> AppCommand {
        let Some(dialog) = self.sanitize_dialog.as_ref() else {
            return AppCommand::None;
        };
        if self.read_only {
            self.close_sanitize_dialog();
            return self.unsupported("Pod sanitization is disabled in read-only mode");
        }
        if dialog.generation != self.generation || dialog.context != self.context {
            self.close_sanitize_dialog();
            return self.unsupported("The view changed; reopen Pod sanitization");
        }
        if dialog.confirmation != SANITIZE_CONFIRMATION {
            self.notice = Some(format!("Type {SANITIZE_CONFIRMATION} exactly to confirm"));
            return AppCommand::None;
        }
        if !dialog.scope_visible {
            self.notice = Some(
                "Enlarge the terminal to review the deletion scope before confirming".to_owned(),
            );
            return AppCommand::None;
        }
        let namespace = dialog.namespace.clone();
        self.close_sanitize_dialog();
        AppCommand::Action(ActionIntent::SanitizePods { namespace })
    }
}

#[cfg(test)]
#[path = "app_sanitize_tests.rs"]
mod tests;
