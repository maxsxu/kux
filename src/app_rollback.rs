use super::{
    ActionIntent, App, AppCommand, KeyCode, KeyEvent, KeyModifiers, Overlay, ResourceTarget,
};
use crate::source::RollbackSnapshot;

#[derive(Debug, Default)]
pub struct RollbackState {
    id: u64,
    context: String,
    generation: u64,
    pub pending: Option<ResourceTarget>,
    pub dialog: Option<RollbackDialog>,
}

#[derive(Debug)]
pub struct RollbackDialog {
    pub snapshot: RollbackSnapshot,
    pub confirm: bool,
    pub scope_visible: bool,
}

impl App {
    pub(super) fn open_rollback(&mut self) -> AppCommand {
        if !self.is_resource_view() || !self.kind.is_apps_v1_replica_set() {
            return AppCommand::None;
        }
        if self.read_only {
            return self.unsupported("Rollback is disabled in read-only mode");
        }
        let Some(target) = self.selected_target() else {
            return AppCommand::None;
        };
        self.rollback.id = self.rollback.id.wrapping_add(1);
        self.rollback.context.clone_from(&self.context);
        self.rollback.generation = self.generation;
        self.rollback.pending = Some(target.clone());
        self.rollback.dialog = None;
        self.overlay = Overlay::RollbackDialog;
        self.notice = None;
        AppCommand::Action(ActionIntent::LoadRollback {
            id: self.rollback.id,
            target,
        })
    }

    pub(super) fn close_rollback(&mut self) {
        self.rollback.id = self.rollback.id.wrapping_add(1);
        self.rollback.pending = None;
        self.rollback.dialog = None;
        if self.overlay == Overlay::RollbackDialog {
            self.overlay = Overlay::None;
            self.notice = None;
        }
    }

    pub fn show_rollback(
        &mut self,
        id: u64,
        target: &ResourceTarget,
        result: Result<RollbackSnapshot, String>,
    ) {
        if id != self.rollback.id
            || self.rollback.pending.as_ref() != Some(target)
            || self.rollback.context != self.context
            || self.rollback.generation != self.generation
            || self.overlay != Overlay::RollbackDialog
        {
            return;
        }
        if self.read_only {
            self.close_rollback();
            return;
        }
        self.rollback.pending = None;
        match result {
            Ok(snapshot) if &snapshot.target == target => {
                self.rollback.dialog = Some(RollbackDialog {
                    snapshot,
                    confirm: false,
                    scope_visible: false,
                });
                self.notice = None;
            }
            Ok(_) => self.show_transient_notice("Rollback preview returned a different ReplicaSet"),
            Err(error) => self.show_transient_notice(error),
        }
    }

    pub(super) fn handle_rollback_key(&mut self, key: KeyEvent) -> AppCommand {
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return if self.user_config.settings.no_exit_on_ctrl_c {
                AppCommand::None
            } else {
                AppCommand::Quit
            };
        }
        if key.code == KeyCode::Esc {
            let loading = self.rollback.pending.is_some();
            self.close_rollback();
            return if loading {
                AppCommand::Action(ActionIntent::StopContentRefresh)
            } else {
                AppCommand::None
            };
        }
        if self.read_only {
            self.close_rollback();
            return self.unsupported("Rollback is disabled in read-only mode");
        }
        if self.rollback.context != self.context || self.rollback.generation != self.generation {
            self.close_rollback();
            return self.unsupported("The view changed; reopen rollback");
        }
        let Some(dialog) = self.rollback.dialog.as_mut() else {
            return AppCommand::None;
        };
        match key.code {
            KeyCode::Tab | KeyCode::BackTab | KeyCode::Up | KeyCode::Down => {
                dialog.confirm = !dialog.confirm;
            }
            KeyCode::Left | KeyCode::Char('h') => dialog.confirm = false,
            KeyCode::Right | KeyCode::Char('l') => dialog.confirm = true,
            KeyCode::Enter if !dialog.confirm => self.close_rollback(),
            KeyCode::Enter if !dialog.scope_visible => {
                self.show_transient_notice(
                    "Enlarge the terminal to review the full rollback scope",
                );
            }
            KeyCode::Enter => {
                let dialog = self.rollback.dialog.take().expect("rollback dialog exists");
                self.close_rollback();
                return AppCommand::Action(ActionIntent::Rollback {
                    snapshot: Box::new(dialog.snapshot),
                });
            }
            _ => {}
        }
        AppCommand::None
    }
}

#[cfg(test)]
#[path = "app_rollback_tests.rs"]
mod tests;
