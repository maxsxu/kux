use super::{App, AppCommand, ContentKind, FilterMatcher, InputMode, Overlay};
use crate::{
    log_view::plain_log_line,
    screen_dumps::{DumpFormat, ScreenDumpEntry},
    ui::TableExportSnapshot,
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DumpPayload {
    Text { body: String, format: DumpFormat },
    Table(TableExportSnapshot),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DumpIntent {
    List { id: u64 },
    Preview { id: u64, entry: ScreenDumpEntry },
    Edit { id: u64, entry: ScreenDumpEntry },
    Delete { id: u64, entry: ScreenDumpEntry },
    Save { name: String, payload: DumpPayload },
}

#[derive(Debug, Default)]
pub struct ScreenDumpState {
    pub entries: Vec<ScreenDumpEntry>,
    pub loading: bool,
    pub delete_pending: Option<ScreenDumpEntry>,
    pub request_id: u64,
    list_query: String,
}

impl App {
    #[allow(clippy::too_many_lines)]
    pub(super) fn export_view(&mut self) -> AppCommand {
        if self.input_mode != InputMode::Browse {
            return AppCommand::None;
        }
        let (name, payload) = match self.overlay {
            Overlay::None => (
                self.kind.plural.to_string(),
                DumpPayload::Table(TableExportSnapshot::capture(self)),
            ),
            Overlay::Content if self.is_logs_view() => {
                return AppCommand::SaveLogs {
                    name: self.content_title.clone(),
                    body: self.log_export_text(),
                };
            }
            Overlay::Content => {
                let yaml = matches!(
                    self.content_kind,
                    Some(ContentKind::Yaml | ContentKind::SecretData | ContentKind::HelmValues)
                );
                let body = self
                    .active_content_body()
                    .lines()
                    .filter(|line| {
                        self.content_kind == Some(ContentKind::Yaml) || self.body_line_matches(line)
                    })
                    .map(plain_log_line)
                    .collect::<Vec<_>>()
                    .join("\n");
                (
                    self.content_title.clone(),
                    DumpPayload::Text {
                        body,
                        format: if yaml {
                            DumpFormat::Yaml
                        } else {
                            DumpFormat::Text
                        },
                    },
                )
            }
            Overlay::Aliases => {
                let mut out = Vec::new();
                let _ = crate::ui::export::write_record(&mut out, ["RESOURCE", "ALIASES"]);
                for idx in self.visible_resource_indices() {
                    if let Some(res) = self.resource_options.get(idx) {
                        let mut aliases = res.command_aliases();
                        for (custom_alias, target) in &self.user_config.aliases {
                            if target == &res.gvr() || target == &res.qualified_name() {
                                aliases.push(custom_alias.clone());
                            }
                        }
                        let _ = crate::ui::export::write_record(
                            &mut out,
                            [res.qualified_name().as_str(), aliases.join(" ").as_str()],
                        );
                    }
                }
                (
                    "aliases".to_owned(),
                    DumpPayload::Text {
                        body: String::from_utf8_lossy(&out).into_owned(),
                        format: DumpFormat::Csv,
                    },
                )
            }
            Overlay::Namespaces => {
                let mut out = Vec::new();
                let _ = crate::ui::export::write_record(&mut out, ["NAME", "STATUS", "AGE"]);
                for idx in self.visible_namespace_indices() {
                    if let Some(ns) = self.namespace_options.get(idx) {
                        let age = ns
                            .age_seconds(self.now_unix)
                            .map_or_else(|| "-".to_owned(), crate::model::format_kubectl_age);
                        let _ = crate::ui::export::write_record(
                            &mut out,
                            [ns.name.as_str(), ns.status.as_str(), age.as_str()],
                        );
                    }
                }
                (
                    "namespaces".to_owned(),
                    DumpPayload::Text {
                        body: String::from_utf8_lossy(&out).into_owned(),
                        format: DumpFormat::Csv,
                    },
                )
            }
            Overlay::Contexts => {
                let mut out = Vec::new();
                let _ = crate::ui::export::write_record(
                    &mut out,
                    ["NAME", "CLUSTER", "USER", "NAMESPACE"],
                );
                for idx in self.visible_context_indices() {
                    if let Some(ctx) = self.context_options.get(idx) {
                        let _ = crate::ui::export::write_record(
                            &mut out,
                            [
                                ctx.name.as_str(),
                                ctx.cluster_display(),
                                ctx.user_display(),
                                ctx.namespace_display(),
                            ],
                        );
                    }
                }
                (
                    "contexts".to_owned(),
                    DumpPayload::Text {
                        body: String::from_utf8_lossy(&out).into_owned(),
                        format: DumpFormat::Csv,
                    },
                )
            }
            Overlay::PortForwards => {
                let mut out = Vec::new();
                let _ = crate::ui::export::write_record(
                    &mut out,
                    [
                        "NAMESPACE",
                        "KIND",
                        "NAME",
                        "LOCAL->REMOTE",
                        "ADDRESS",
                        "STATUS",
                    ],
                );
                for pf in &self.port_forwards {
                    let ns = pf.target.namespace.as_deref().unwrap_or("-");
                    let ports = format!("{}->{}", pf.local_port, pf.remote_port);
                    let _ = crate::ui::export::write_record(
                        &mut out,
                        [
                            ns,
                            pf.target.kind.kind.as_ref(),
                            pf.target.name.as_str(),
                            ports.as_str(),
                            pf.address.as_str(),
                            pf.state.label(),
                        ],
                    );
                }
                (
                    "portforwards".to_owned(),
                    DumpPayload::Text {
                        body: String::from_utf8_lossy(&out).into_owned(),
                        format: DumpFormat::Csv,
                    },
                )
            }
            Overlay::ScreenDumps => {
                let mut out = Vec::new();
                let _ = crate::ui::export::write_record(&mut out, ["NAME", "DIR", "SIZE", "AGE"]);
                for idx in self.visible_dump_indices() {
                    if let Some(entry) = self.dumps.entries.get(idx) {
                        let dir = entry.dir_display();
                        let size = format!("{} B", entry.size_bytes);
                        let age_secs =
                            u64::try_from(self.now_unix.saturating_sub(entry.modified_unix))
                                .unwrap_or(0);
                        let age = crate::model::format_kubectl_age(age_secs);
                        let _ = crate::ui::export::write_record(
                            &mut out,
                            [
                                entry.name.as_str(),
                                dir.as_str(),
                                size.as_str(),
                                age.as_str(),
                            ],
                        );
                    }
                }
                (
                    "screendumps".to_owned(),
                    DumpPayload::Text {
                        body: String::from_utf8_lossy(&out).into_owned(),
                        format: DumpFormat::Csv,
                    },
                )
            }
            _ => return self.unsupported("Open a resource table or content view to export"),
        };
        AppCommand::Dump(DumpIntent::Save { name, payload })
    }

    pub(super) fn open_screen_dumps(&mut self) -> AppCommand {
        self.overlay = Overlay::ScreenDumps;
        self.body_query.clear();
        self.filter_error = None;
        self.overlay_selected = 0;
        self.dumps.delete_pending = None;
        self.dumps.loading = false;
        self.reload_screen_dumps()
    }

    pub fn reload_screen_dumps(&mut self) -> AppCommand {
        if self.dumps.loading {
            return AppCommand::None;
        }
        self.dumps.request_id = self.dumps.request_id.wrapping_add(1);
        self.dumps.loading = true;
        self.notice = Some("Loading screen dumps…".to_owned());
        AppCommand::Dump(DumpIntent::List {
            id: self.dumps.request_id,
        })
    }

    #[must_use]
    pub fn visible_dump_indices(&self) -> Vec<usize> {
        let matcher = FilterMatcher::new(&self.body_query);
        self.dumps
            .entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| matcher.matches_text(&entry.name))
            .map(|(index, _)| index)
            .collect()
    }

    pub fn show_screen_dumps(&mut self, id: u64, entries: Vec<ScreenDumpEntry>) {
        if id != self.dumps.request_id {
            return;
        }
        self.dumps.loading = false;
        if self.overlay != Overlay::ScreenDumps {
            return;
        }
        let selected = self
            .dumps
            .entries
            .get(self.overlay_selected)
            .map(|entry| entry.path.clone());
        self.dumps.entries = entries;
        let visible = self.visible_dump_indices();
        self.overlay_selected = selected
            .and_then(|path| {
                self.dumps
                    .entries
                    .iter()
                    .position(|entry| entry.path == path)
            })
            .filter(|index| visible.contains(index))
            .or_else(|| visible.first().copied())
            .unwrap_or(usize::MAX);
        self.notice = None;
    }

    pub fn screen_dump_error(&mut self, id: u64, error: String) {
        if id == self.dumps.request_id {
            self.dumps.loading = false;
            if self.overlay == Overlay::ScreenDumps {
                self.show_transient_notice(error);
            }
        }
    }

    pub fn show_screen_dump_content(&mut self, id: u64, name: String, body: String) {
        if id != self.dumps.request_id {
            return;
        }
        self.dumps.loading = false;
        if self.overlay != Overlay::ScreenDumps {
            return;
        }
        self.dumps.list_query.clone_from(&self.body_query);
        self.show_content(name, body, None, None, None, ContentKind::ScreenDump);
    }

    pub(super) fn leave_screen_dump_content(&mut self) -> AppCommand {
        self.overlay = Overlay::ScreenDumps;
        self.body_query.clone_from(&self.dumps.list_query);
        self.content_kind = None;
        self.notice = None;
        AppCommand::None
    }

    pub(super) fn handle_screen_dumps_key(&mut self, key: KeyEvent) -> AppCommand {
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return if self.user_config.settings.no_exit_on_ctrl_c {
                AppCommand::None
            } else {
                AppCommand::Quit
            };
        }
        if self.dumps.delete_pending.is_none()
            && matches!(key.code, KeyCode::Esc | KeyCode::Char('h' | 'q' | '?'))
        {
            self.dumps.request_id = self.dumps.request_id.wrapping_add(1);
            self.dumps.loading = false;
            return self.close_overlay_on(key);
        }
        if self.dumps.delete_pending.is_some() {
            if key.code == KeyCode::Esc {
                self.dumps.delete_pending = None;
            } else if key.code == KeyCode::Enter {
                let entry = self.dumps.delete_pending.take().expect("pending dump");
                if self.read_only {
                    return self.unsupported("Dump deletion is disabled in read-only mode");
                }
                self.dumps.request_id = self.dumps.request_id.wrapping_add(1);
                self.dumps.loading = true;
                return AppCommand::Dump(DumpIntent::Delete {
                    id: self.dumps.request_id,
                    entry,
                });
            }
            return AppCommand::None;
        }
        let visible = self.visible_dump_indices();
        if self.handle_chooser_navigation(&key, &visible) {
            return AppCommand::None;
        }
        if key.code == KeyCode::Char('r') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return self.reload_screen_dumps();
        }
        if self.dumps.loading {
            return self.close_overlay_on(key);
        }
        let selected = visible
            .contains(&self.overlay_selected)
            .then(|| self.dumps.entries.get(self.overlay_selected).cloned())
            .flatten();
        match key.code {
            KeyCode::Char('c') => {
                let Some(entry) = selected else {
                    return AppCommand::None;
                };
                AppCommand::Copy(entry.path.to_string_lossy().into_owned())
            }
            KeyCode::Char('N') if key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.dumps.entries.sort_by(|a, b| a.name.cmp(&b.name));
                self.select_first_visible_chooser_item();
                AppCommand::None
            }
            KeyCode::Char('A') if key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.dumps
                    .entries
                    .sort_by_key(|a| std::cmp::Reverse(a.modified_unix));
                self.select_first_visible_chooser_item();
                AppCommand::None
            }
            KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if self.read_only {
                    return self.unsupported("Dump deletion is disabled in read-only mode");
                }
                self.dumps.delete_pending = selected;
                AppCommand::None
            }
            KeyCode::Enter | KeyCode::Char('e' | 'v' | 'l') => {
                let Some(entry) = selected else {
                    return AppCommand::None;
                };
                self.dumps.request_id = self.dumps.request_id.wrapping_add(1);
                self.dumps.loading = true;
                let id = self.dumps.request_id;
                AppCommand::Dump(if matches!(key.code, KeyCode::Char('v' | 'l')) {
                    DumpIntent::Preview { id, entry }
                } else {
                    DumpIntent::Edit { id, entry }
                })
            }
            _ => self.close_overlay_on(key),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{model::ResourceKind, screen_dumps::ScreenDumpStore};
    use std::{
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
    };
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    struct Directory(PathBuf);
    impl Directory {
        fn new() -> Self {
            Self(std::env::temp_dir().join(format!(
                "kux-app-dumps-{}-{}",
                std::process::id(),
                SEQUENCE.fetch_add(1, Ordering::Relaxed)
            )))
        }
    }
    impl Drop for Directory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn app() -> App {
        App::new(
            "demo",
            None,
            ResourceKind::Pods,
            false,
            vec!["demo".to_owned()],
        )
    }
    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }
    fn control(code: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(code), KeyModifiers::CONTROL)
    }

    #[test]
    fn exports_full_yaml_but_respects_display_filter_for_describe() {
        let mut app = app();
        app.show_content(
            "yaml",
            "kind: Pod\nmetadata:\n  name: app",
            None,
            None,
            None,
            ContentKind::Yaml,
        );
        app.body_query = "kind".to_owned();
        let AppCommand::Dump(DumpIntent::Save {
            payload: DumpPayload::Text { body, format },
            ..
        }) = app.handle_key(control('s'))
        else {
            panic!("yaml export");
        };
        assert_eq!(format, DumpFormat::Yaml);
        assert!(body.contains("name: app"));
        app.show_content(
            "describe",
            "Name: app\nStatus: Running",
            None,
            None,
            None,
            ContentKind::Describe,
        );
        app.body_query = "Name".to_owned();
        let AppCommand::Dump(DumpIntent::Save {
            payload: DumpPayload::Text { body, .. },
            ..
        }) = app.handle_key(control('s'))
        else {
            panic!("describe export");
        };
        assert_eq!(body, "Name: app");
    }

    #[test]
    fn dump_selection_preview_return_and_confirmation_use_exact_entry() {
        let directory = Directory::new();
        let store = ScreenDumpStore::new(directory.0.clone(), None, "demo").unwrap();
        store.save("first", "first body", DumpFormat::Text).unwrap();
        store
            .save("second", "second body", DumpFormat::Yaml)
            .unwrap();
        let mut app = app();
        let AppCommand::Dump(DumpIntent::List { id }) = app.run_command("sd") else {
            panic!("list dumps");
        };
        app.show_screen_dumps(id, store.list().unwrap());
        app.body_query = "second".to_owned();
        app.select_first_visible_chooser_item();
        let AppCommand::Dump(DumpIntent::Preview { id, entry }) =
            app.handle_key(key(KeyCode::Char('v')))
        else {
            panic!("preview");
        };
        assert!(entry.name.contains("second"));
        app.show_screen_dump_content(id, entry.name.clone(), store.read(&entry).unwrap());
        assert_eq!(app.overlay, Overlay::Content);
        assert_eq!(app.content_kind, Some(ContentKind::ScreenDump));
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.overlay, Overlay::ScreenDumps);
        assert_eq!(app.body_query, "second");
        app.handle_key(control('d'));
        assert_eq!(app.dumps.delete_pending.as_ref(), Some(&entry));
        app.handle_key(key(KeyCode::Esc));
        assert!(app.dumps.delete_pending.is_none());
        app.handle_key(control('d'));
        assert!(
            matches!(app.handle_key(key(KeyCode::Enter)), AppCommand::Dump(DumpIntent::Delete { entry: selected, .. }) if selected == entry)
        );
    }

    #[test]
    fn leaving_dump_browser_invalidates_pending_results_and_readonly_rejects_delete() {
        let directory = Directory::new();
        let store = ScreenDumpStore::new(directory.0.clone(), None, "demo").unwrap();
        store.save("selection", "body", DumpFormat::Text).unwrap();
        let entries = store.list().unwrap();
        let mut app = app();
        let AppCommand::Dump(DumpIntent::List { id }) = app.run_command("screendump") else {
            panic!("list");
        };
        app.handle_key(key(KeyCode::Esc));
        app.show_screen_dumps(id, entries.clone());
        assert_eq!(app.overlay, Overlay::None);
        assert_eq!(app.dumps.entries.len(), 0);
        app.run_command("sd");
        app.show_screen_dumps(app.dumps.request_id, entries);
        app.read_only = true;
        assert_eq!(app.handle_key(control('d')), AppCommand::None);
        assert!(app.dumps.delete_pending.is_none());
        assert!(app.notice.as_deref().unwrap().contains("read-only"));
    }

    #[test]
    fn chooser_csv_export_and_screen_dump_sort_work() {
        let mut app = app();
        app.overlay = Overlay::Aliases;
        let AppCommand::Dump(DumpIntent::Save {
            name,
            payload: DumpPayload::Text { body, format },
        }) = app.handle_key(control('s'))
        else {
            panic!("aliases export");
        };
        assert_eq!(name, "aliases");
        assert_eq!(format, DumpFormat::Csv);
        assert!(body.starts_with("\"RESOURCE\",\"ALIASES\"\n"));

        let directory = Directory::new();
        let store = ScreenDumpStore::new(directory.0.clone(), None, "demo").unwrap();
        store.save("zebra", "z", DumpFormat::Text).unwrap();
        store.save("alpha", "a", DumpFormat::Text).unwrap();
        app.run_command("sd");
        app.show_screen_dumps(app.dumps.request_id, store.list().unwrap());
        app.handle_key(KeyEvent::new(KeyCode::Char('N'), KeyModifiers::SHIFT));
        assert!(app.dumps.entries[0].name.starts_with("alpha"));
    }
}
