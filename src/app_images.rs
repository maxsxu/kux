use super::{
    ActionIntent, App, AppCommand, KeyCode, KeyEvent, KeyModifiers, Overlay, ResourceTarget,
};
use crate::source::{ContainerImage, ImageSnapshot, supports_images};

#[cfg(test)]
#[path = "app_images_tests.rs"]
mod tests;

#[derive(Debug, Default)]
pub struct ImageEditorState {
    id: u64,
    pub pending: Option<ResourceTarget>,
    pub dialog: Option<ImageDialog>,
}

#[derive(Debug)]
pub struct ImageDialog {
    pub snapshot: ImageSnapshot,
    pub values: Vec<String>,
    pub focus: usize,
    pub cursor: usize,
}

impl ImageDialog {
    fn move_focus(&mut self, forward: bool) {
        let count = self.values.len() + 2;
        self.focus = if forward {
            (self.focus + 1) % count
        } else {
            (self.focus + count - 1) % count
        };
        self.cursor = self.values.get(self.focus).map_or(0, String::len);
    }

    fn edit(&mut self, key: KeyEvent) {
        let Some(value) = self.values.get_mut(self.focus) else {
            return;
        };
        self.cursor = self.cursor.min(value.len());
        let previous = || {
            value[..self.cursor]
                .char_indices()
                .next_back()
                .map_or(0, |(index, _)| index)
        };
        match key.code {
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = value.len(),
            KeyCode::Left => self.cursor = previous(),
            KeyCode::Right => {
                self.cursor += value[self.cursor..]
                    .chars()
                    .next()
                    .map_or(0, char::len_utf8);
            }
            KeyCode::Backspace if self.cursor > 0 => {
                let start = previous();
                value.replace_range(start..self.cursor, "");
                self.cursor = start;
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
}

impl App {
    pub(super) fn open_image_editor(&mut self) -> AppCommand {
        if self.read_only {
            return self.unsupported("Image updates are disabled in read-only mode");
        }
        if !supports_images(&self.kind) {
            return self
                .unsupported("Image updates require a Pod, Deployment, StatefulSet, or DaemonSet");
        }
        let Some(target) = self.selected_target() else {
            return AppCommand::None;
        };
        self.image_editor.id = self.image_editor.id.wrapping_add(1);
        self.image_editor.pending = Some(target.clone());
        self.image_editor.dialog = None;
        self.overlay = Overlay::ImageDialog;
        self.notice = None;
        AppCommand::Action(ActionIntent::LoadImages {
            id: self.image_editor.id,
            target,
        })
    }

    pub(super) fn close_image_editor(&mut self) {
        self.image_editor.id = self.image_editor.id.wrapping_add(1);
        self.image_editor.pending = None;
        self.image_editor.dialog = None;
        if self.overlay == Overlay::ImageDialog {
            self.overlay = Overlay::None;
        }
    }

    pub fn show_images(
        &mut self,
        id: u64,
        target: &ResourceTarget,
        result: Result<ImageSnapshot, String>,
    ) {
        if id != self.image_editor.id
            || self.image_editor.pending.as_ref() != Some(target)
            || self.overlay != Overlay::ImageDialog
        {
            return;
        }
        if self.read_only {
            self.close_image_editor();
            return;
        }
        self.image_editor.pending = None;
        match result {
            Ok(snapshot) if &snapshot.target == target && !snapshot.images.is_empty() => {
                let values: Vec<_> = snapshot
                    .images
                    .iter()
                    .map(|image| image.image.clone())
                    .collect();
                let cursor = values.first().map_or(0, String::len);
                self.image_editor.dialog = Some(ImageDialog {
                    snapshot,
                    values,
                    focus: 0,
                    cursor,
                });
                self.notice = None;
            }
            Ok(_) => self.show_transient_notice("No editable container images were returned"),
            Err(error) => self.show_transient_notice(error),
        }
    }

    pub(super) fn handle_image_key(&mut self, key: KeyEvent) -> AppCommand {
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return if self.user_config.settings.no_exit_on_ctrl_c {
                AppCommand::None
            } else {
                AppCommand::Quit
            };
        }
        if key.code == KeyCode::Esc {
            let loading = self.image_editor.pending.is_some();
            self.close_image_editor();
            return if loading {
                AppCommand::Action(ActionIntent::StopContentRefresh)
            } else {
                AppCommand::None
            };
        }
        if self.read_only {
            self.close_image_editor();
            return self.unsupported("Image updates are disabled in read-only mode");
        }
        let Some(dialog) = self.image_editor.dialog.as_mut() else {
            return AppCommand::None;
        };
        match key.code {
            KeyCode::Tab | KeyCode::Down => {
                dialog.move_focus(!key.modifiers.contains(KeyModifiers::SHIFT));
            }
            KeyCode::BackTab | KeyCode::Up => dialog.move_focus(false),
            KeyCode::Enter if dialog.focus == dialog.values.len() => self.close_image_editor(),
            KeyCode::Enter if dialog.focus == dialog.values.len() + 1 => {
                return self.submit_images();
            }
            KeyCode::Enter => dialog.move_focus(true),
            _ => dialog.edit(key),
        }
        AppCommand::None
    }

    fn submit_images(&mut self) -> AppCommand {
        if self.read_only {
            return self.unsupported("Image updates are disabled in read-only mode");
        }
        let Some(dialog) = self.image_editor.dialog.take() else {
            return AppCommand::None;
        };
        let images = dialog
            .snapshot
            .images
            .iter()
            .zip(dialog.values)
            .map(|(original, value)| ContainerImage {
                name: original.name.clone(),
                init: original.init,
                image: if value.trim().is_empty() {
                    original.image.clone()
                } else {
                    value.trim().to_owned()
                },
            })
            .collect::<Vec<_>>();
        self.close_image_editor();
        if images == dialog.snapshot.images {
            return self.unsupported("Images are unchanged");
        }
        AppCommand::Action(ActionIntent::SetImages {
            snapshot: dialog.snapshot,
            images,
        })
    }
}
