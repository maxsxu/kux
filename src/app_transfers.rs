use super::{
    ActionIntent, App, AppCommand, KeyCode, KeyEvent, KeyModifiers, Overlay, ResourceKind,
    ResourceTarget,
};
use crate::transfer::{TransferCompletion, TransferIntent, TransferPod, TransferRequest};

#[derive(Debug, Default)]
pub struct TransferState {
    pub id: u64,
    pub pending: Option<ResourceTarget>,
    pub dialog: Option<TransferDialog>,
}

#[derive(Debug)]
pub struct TransferDialog {
    pub target: ResourceTarget,
    pub containers: Vec<String>,
    /// From, To, Container, Retries.
    pub values: [String; 4],
    pub download: bool,
    pub no_preserve: bool,
    pub focus: usize,
    pub cursor: usize,
    pub running: bool,
}

impl TransferDialog {
    #[must_use]
    pub const fn field_index(&self) -> Option<usize> {
        match self.focus {
            1 => Some(0),
            2 => Some(1),
            4 => Some(2),
            5 => Some(3),
            _ => None,
        }
    }

    fn move_focus(&mut self, forward: bool) {
        self.focus = (self.focus + if forward { 1 } else { 7 }) % 8;
        self.cursor = self
            .field_index()
            .map_or(0, |index| self.values[index].len());
    }

    fn toggle(&mut self) {
        if self.focus == 0 {
            self.download = !self.download;
            self.values.swap(0, 1);
        } else if self.focus == 3 {
            self.no_preserve = !self.no_preserve;
        }
    }

    fn edit(&mut self, key: KeyEvent) {
        let Some(index) = self.field_index() else {
            return;
        };
        let value = &mut self.values[index];
        self.cursor = self.cursor.min(value.len());
        let previous = value[..self.cursor]
            .char_indices()
            .next_back()
            .map_or(0, |(index, _)| index);
        match key.code {
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = value.len(),
            KeyCode::Left => self.cursor = previous,
            KeyCode::Right => {
                self.cursor += value[self.cursor..]
                    .chars()
                    .next()
                    .map_or(0, char::len_utf8);
            }
            KeyCode::Backspace if self.cursor > 0 => {
                value.replace_range(previous..self.cursor, "");
                self.cursor = previous;
            }
            KeyCode::Delete if self.cursor < value.len() => {
                value.remove(self.cursor);
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                value.clear();
                self.cursor = 0;
            }
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                    && !character.is_control()
                    && value.len() + character.len_utf8() <= 4096 =>
            {
                value.insert(self.cursor, character);
                self.cursor += character.len_utf8();
            }
            _ => {}
        }
    }

    fn request(&self) -> Result<TransferRequest, String> {
        TransferRequest {
            target: self.target.clone(),
            download: self.download,
            from: self.values[0].clone(),
            to: self.values[1].clone(),
            no_preserve: self.no_preserve,
            container: self.values[2].clone(),
            retries: self.values[3]
                .trim()
                .parse()
                .map_err(|_| "Retries must be a whole number".to_owned())?,
        }
        .normalized()
    }
}

impl App {
    pub(super) fn open_transfer(&mut self) -> AppCommand {
        if self.read_only {
            return self.unsupported("File transfers are disabled in read-only mode");
        }
        if !self.is_resource_view() || self.kind.known_kind() != Some(ResourceKind::Pods) {
            return self.unsupported("File transfer requires a selected Pod");
        }
        let Some(target) = self.selected_target() else {
            return AppCommand::None;
        };
        self.transfers.id = self.transfers.id.wrapping_add(1);
        self.transfers.pending = Some(target.clone());
        self.transfers.dialog = None;
        self.overlay = Overlay::TransferDialog;
        self.notice = None;
        AppCommand::Action(ActionIntent::LoadTransferPod {
            id: self.transfers.id,
            target,
        })
    }

    pub(super) fn close_transfer(&mut self) {
        self.transfers.id = self.transfers.id.wrapping_add(1);
        self.transfers.pending = None;
        self.transfers.dialog = None;
        if self.overlay == Overlay::TransferDialog {
            self.overlay = Overlay::None;
            self.notice = None;
        }
    }

    pub fn show_transfer_pod(
        &mut self,
        id: u64,
        target: &ResourceTarget,
        result: Result<TransferPod, String>,
    ) {
        if self.transfers.id != id
            || self.transfers.pending.as_ref() != Some(target)
            || self.overlay != Overlay::TransferDialog
        {
            return;
        }
        if self.read_only {
            self.close_transfer();
            return;
        }
        self.transfers.pending = None;
        match result {
            Ok(pod) if !pod.containers.is_empty() => {
                let remote = format!(
                    "{}/{}:",
                    target.namespace.as_deref().unwrap_or_default(),
                    target.name
                );
                self.transfers.dialog = Some(TransferDialog {
                    target: target.clone(),
                    values: [
                        remote,
                        String::new(),
                        pod.containers[0].clone(),
                        "999".to_owned(),
                    ],
                    containers: pod.containers,
                    download: true,
                    no_preserve: false,
                    focus: 0,
                    cursor: 0,
                    running: false,
                });
                self.notice = None;
            }
            Ok(_) => self.show_transient_notice("The Pod has no eligible containers"),
            Err(error) => self.show_transient_notice(error),
        }
    }

    pub(super) fn handle_transfer_key(&mut self, key: KeyEvent) -> AppCommand {
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return if self.user_config.settings.no_exit_on_ctrl_c {
                AppCommand::None
            } else {
                AppCommand::Quit
            };
        }
        if key.code == KeyCode::Esc {
            return self.cancel_transfer();
        }
        if self.read_only {
            self.close_transfer();
            return self.unsupported("File transfers are disabled in read-only mode");
        }
        let Some(dialog) = self.transfers.dialog.as_mut() else {
            return AppCommand::None;
        };
        if dialog.running {
            if key.code == KeyCode::Enter {
                return self.cancel_transfer();
            }
            return AppCommand::None;
        }
        match key.code {
            KeyCode::Tab | KeyCode::Down => {
                dialog.move_focus(!key.modifiers.contains(KeyModifiers::SHIFT));
            }
            KeyCode::BackTab | KeyCode::Up => dialog.move_focus(false),
            KeyCode::Char(' ') if matches!(dialog.focus, 0 | 3) => dialog.toggle(),
            KeyCode::Enter if matches!(dialog.focus, 0 | 3) => dialog.toggle(),
            KeyCode::Enter if dialog.focus == 6 => self.close_transfer(),
            KeyCode::Enter if dialog.focus == 7 => return self.submit_transfer(),
            KeyCode::Enter => dialog.move_focus(true),
            _ => dialog.edit(key),
        }
        AppCommand::None
    }

    fn cancel_transfer(&mut self) -> AppCommand {
        let id = self.transfers.id;
        let loading = self.transfers.pending.is_some();
        let running = self
            .transfers
            .dialog
            .as_ref()
            .is_some_and(|dialog| dialog.running);
        self.close_transfer();
        if loading {
            AppCommand::Action(ActionIntent::StopContentRefresh)
        } else if running {
            self.show_transient_notice("File transfer cancellation requested");
            AppCommand::Transfer(TransferIntent::Cancel { id })
        } else {
            AppCommand::None
        }
    }

    fn submit_transfer(&mut self) -> AppCommand {
        if self.read_only {
            return self.unsupported("File transfers are disabled in read-only mode");
        }
        let Some(dialog) = &mut self.transfers.dialog else {
            return AppCommand::None;
        };
        match dialog.request() {
            Ok(request) => {
                dialog.running = true;
                self.notice = Some("Transferring… Esc cancels".to_owned());
                AppCommand::Transfer(TransferIntent::Start {
                    id: self.transfers.id,
                    request: Box::new(request),
                })
            }
            Err(error) => {
                self.show_transient_notice(error);
                AppCommand::None
            }
        }
    }

    pub fn transfer_failed(&mut self, id: u64, error: String) {
        if id != self.transfers.id || self.overlay != Overlay::TransferDialog {
            return;
        }
        if let Some(dialog) = &mut self.transfers.dialog {
            dialog.running = false;
        }
        self.show_transient_notice(error);
    }

    pub fn apply_transfer_result(&mut self, completion: TransferCompletion) {
        if completion.context != self.context
            || completion.generation != self.generation
            || completion.id != self.transfers.id
            || self.overlay != Overlay::TransferDialog
        {
            return;
        }
        match completion.result {
            Ok(notice) => {
                self.close_transfer();
                self.show_transient_notice(notice);
            }
            Err(error) => self.transfer_failed(completion.id, error),
        }
    }
}

#[cfg(test)]
#[path = "app_transfers_tests.rs"]
mod tests;
