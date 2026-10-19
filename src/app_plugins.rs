use std::collections::BTreeMap;

use super::{App, AppCommand, InputMode, ListMode, Overlay};
use crate::{
    model::ResourceKind,
    plugins::{InputType, Plugin, PluginEnvironment, PluginInvocation},
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

#[derive(Debug, Clone)]
pub struct PluginDialog {
    pub plugin: Plugin,
    pub environment: PluginEnvironment,
    pub values: BTreeMap<String, String>,
    pub focus: usize,
    pub confirmation: Option<PluginInvocation>,
    pub preview_scroll: u16,
    previous_overlay: Overlay,
    edited_field: Option<usize>,
}

impl PluginDialog {
    #[must_use]
    pub fn cancel_index(&self) -> usize {
        if self.confirmation.is_some() {
            0
        } else {
            self.plugin.inputs.len()
        }
    }

    fn move_focus(&mut self, forward: bool) {
        let count = self.cancel_index() + 2;
        self.focus = if forward {
            (self.focus + 1) % count
        } else {
            (self.focus + count - 1) % count
        };
        self.edited_field = None;
    }

    fn edit(&mut self, key: KeyEvent) {
        if self.confirmation.is_some() {
            return;
        }
        let Some(input) = self.plugin.inputs.get(self.focus) else {
            return;
        };
        let value = self.values.entry(input.name.clone()).or_default();
        match (&input.kind, key.code) {
            (InputType::Bool, KeyCode::Char(' ') | KeyCode::Left | KeyCode::Right) => {
                *value = (value != "true").to_string();
            }
            (InputType::Dropdown, KeyCode::Char(' ') | KeyCode::Left | KeyCode::Right)
                if !input.options.is_empty() =>
            {
                let options: Vec<&str> = (!input.required)
                    .then_some("")
                    .into_iter()
                    .chain(input.options.iter().map(String::as_str))
                    .collect();
                let current = options.iter().position(|option| *option == value);
                let index = if key.code == KeyCode::Left {
                    current.map_or(options.len() - 1, |index| {
                        (index + options.len() - 1) % options.len()
                    })
                } else {
                    current.map_or(0, |index| (index + 1) % options.len())
                };
                options[index].clone_into(value);
            }
            (InputType::String | InputType::Number, KeyCode::Backspace) => {
                value.pop();
                self.edited_field = Some(self.focus);
            }
            (InputType::String | InputType::Number, KeyCode::Char('u'))
                if key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                value.clear();
                self.edited_field = Some(self.focus);
            }
            (InputType::String | InputType::Number, KeyCode::Char(character))
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                if self.edited_field != Some(self.focus) {
                    value.clear();
                }
                if value.len() < 16_384 {
                    value.push(character);
                }
                self.edited_field = Some(self.focus);
            }
            _ => {}
        }
    }
}

impl App {
    pub(crate) fn plugin_scopes(&self) -> Vec<String> {
        match self.overlay {
            Overlay::Contexts => vec![
                "contexts".to_owned(),
                "context".to_owned(),
                "ctx".to_owned(),
            ],
            Overlay::Namespaces => vec![
                "namespaces".to_owned(),
                "namespace".to_owned(),
                "ns".to_owned(),
                "v1/namespaces".to_owned(),
            ],
            Overlay::PortForwards => vec![
                "portforwards".to_owned(),
                "portforward".to_owned(),
                "pf".to_owned(),
            ],
            Overlay::ScreenDumps => vec![
                "screendumps".to_owned(),
                "screendump".to_owned(),
                "sd".to_owned(),
            ],
            Overlay::Aliases => vec!["aliases".to_owned(), "alias".to_owned()],
            _ if self.is_reference_view() => vec!["references".to_owned(), "ref".to_owned()],
            _ if self.is_policy_view() => vec!["policy".to_owned(), "pol".to_owned()],
            _ if self.is_container_view() => vec![
                "containers".to_owned(),
                "container".to_owned(),
                "co".to_owned(),
            ],
            _ => {
                let mut scopes = self.kind.command_aliases();
                scopes.push(self.kind.gvr());
                scopes
            }
        }
    }

    pub(crate) fn visible_plugins(&self) -> Vec<&Plugin> {
        if !matches!(
            self.overlay,
            Overlay::None
                | Overlay::Contexts
                | Overlay::Namespaces
                | Overlay::PortForwards
                | Overlay::ScreenDumps
                | Overlay::Aliases
                | Overlay::Help
        ) {
            return Vec::new();
        }
        let scopes = self.plugin_scopes();
        let mut bindings = std::collections::HashMap::new();
        for plugin in self
            .user_config
            .plugins
            .plugins
            .values()
            .filter(|plugin| plugin.in_scope(&scopes) && !(self.read_only && plugin.dangerous))
        {
            let key = plugin.key_event();
            if plugin.override_builtin
                || (!bindings.contains_key(&key) && !self.builtin_key_bound(key))
            {
                bindings.insert(key, plugin);
            }
        }
        let mut visible: Vec<_> = bindings.into_values().collect();
        visible.sort_unstable_by(|left, right| left.name.cmp(&right.name));
        visible
    }

    pub(crate) fn plugin_for_key(&self, key: KeyEvent) -> Option<&Plugin> {
        if self.input_mode != InputMode::Browse
            || !matches!(
                self.overlay,
                Overlay::None
                    | Overlay::Namespaces
                    | Overlay::Contexts
                    | Overlay::PortForwards
                    | Overlay::ScreenDumps
                    | Overlay::Aliases
            )
        {
            return None;
        }
        self.user_config.plugins.for_key(
            key,
            &self.plugin_scopes(),
            self.read_only,
            self.builtin_key_bound(key),
        )
    }

    fn workload_builtin_key(&self, key: KeyEvent) -> Option<bool> {
        if self.kind.is_subject() && matches!(self.overlay, Overlay::None | Overlay::Help) {
            if key.modifiers.contains(KeyModifiers::CONTROL) {
                return matches!(key.code, KeyCode::Char('d' | 'k')).then_some(false);
            }
            if !key.modifiers.contains(KeyModifiers::ALT) {
                return matches!(key.code, KeyCode::Char('y' | 'd' | 'e' | 'J')).then_some(false);
            }
        }
        if !self.kind.is_workloads() || !matches!(self.overlay, Overlay::None | Overlay::Help) {
            return None;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            return (key.code == KeyCode::Char('k')).then_some(false);
        }
        if key.modifiers.contains(KeyModifiers::ALT) {
            return None;
        }
        match key.code {
            KeyCode::Char('K' | 'R') => Some(true),
            KeyCode::Char('l' | 'J') => Some(false),
            _ => None,
        }
    }

    #[allow(clippy::too_many_lines)]
    fn builtin_key_bound(&self, key: KeyEvent) -> bool {
        if self.user_config.hotkey_for(key).is_some() {
            return true;
        }
        if let Some(bound) = self.workload_builtin_key(key) {
            return bound;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            return matches!(
                key.code,
                KeyCode::Char(
                    'a' | 'b'
                        | 'c'
                        | 'd'
                        | 'e'
                        | 'f'
                        | 'k'
                        | 'l'
                        | 'r'
                        | 's'
                        | 'w'
                        | 'z'
                        | ' '
                        | '\\'
                )
            );
        }
        if key.modifiers.contains(KeyModifiers::ALT) {
            return false;
        }
        if matches!(
            key.code,
            KeyCode::Enter
                | KeyCode::Esc
                | KeyCode::Up
                | KeyCode::Down
                | KeyCode::Left
                | KeyCode::Right
                | KeyCode::PageUp
                | KeyCode::PageDown
                | KeyCode::Home
                | KeyCode::End
        ) {
            return true;
        }
        if let KeyCode::Char(character) = key.code {
            if character.is_ascii_digit() || ":/?[]-hjklgGcn ".contains(character) {
                return true;
            }
            if matches!(self.overlay, Overlay::Contexts | Overlay::Namespaces) {
                return matches!(character, 'q' | 'y' | 'e' | 'u' | 'r')
                    || (character == 'N' && !self.body_query.is_empty());
            }
            if self.overlay == Overlay::PortForwards {
                return matches!(character, 'q' | 'f' | 'b' | 'P' | 'U');
            }
            if self.overlay == Overlay::ScreenDumps {
                return matches!(character, 'q' | 'e' | 'v' | 'l' | 'N' | 'A');
            }
            if self.overlay == Overlay::Aliases {
                return character == 'q' || (character == 'N' && !self.body_query.is_empty());
            }
            if "ONAPSJ".contains(character) {
                return true;
            }
            if self.is_reference_view() && character == 'V' {
                return true;
            }
            let kind = self.kind.known_kind();
            match character {
                'y' | 'd' | 'e' => self.is_resource_view(),
                'i' => self.is_resource_view() && crate::source::supports_images(&self.kind),
                'z' => {
                    self.is_resource_view()
                        && matches!(kind, Some(ResourceKind::Pods | ResourceKind::Deployments))
                }
                'x' => self.kind.is_core_v1_secret(),
                'p' => {
                    self.is_container_view()
                        || (self.is_resource_view() && crate::source::supports_logs(&self.kind))
                }
                'a' => self.is_container_view() || kind == Some(ResourceKind::Pods),
                'o' => kind == Some(ResourceKind::Pods),
                's' => {
                    self.is_container_view()
                        || kind == Some(ResourceKind::Pods)
                        || self.kind.is_scalable()
                        || self.kind.is_cron_job()
                }
                'f' | 'F' => matches!(kind, Some(ResourceKind::Pods | ResourceKind::Services)),
                'w' => self.namespace.is_none() && self.kind.is_namespaced(),
                'r' => self.is_restartable_workload() || kind == Some(ResourceKind::Nodes),
                't' => {
                    self.kind.is_cron_job()
                        || (self.is_resource_view() && kind == Some(ResourceKind::Pods))
                }
                'u' => {
                    kind == Some(ResourceKind::Nodes)
                        || (self.is_resource_view()
                            && crate::source::supports_references(&self.kind))
                }
                'b' => kind == Some(ResourceKind::Services),
                _ => false,
            }
        } else {
            false
        }
    }

    fn base_plugin_environment(&self) -> PluginEnvironment {
        let context = self
            .context_options
            .iter()
            .find(|item| item.name == self.context);
        let mut env = BTreeMap::from([
            ("CONTEXT".to_owned(), self.context.clone()),
            (
                "CLUSTER".to_owned(),
                context
                    .and_then(|item| item.cluster.clone())
                    .unwrap_or_default(),
            ),
            (
                "USER".to_owned(),
                context
                    .and_then(|item| item.user.clone())
                    .unwrap_or_default(),
            ),
            ("GROUPS".to_owned(), String::new()),
            ("KUBECONFIG".to_owned(), String::new()),
            ("RESOURCE_GROUP".to_owned(), self.kind.group.to_string()),
            ("RESOURCE_VERSION".to_owned(), self.kind.version.to_string()),
            ("RESOURCE_NAME".to_owned(), self.kind.plural.to_string()),
        ]);
        env.extend(self.connection.plugin_env());
        if self.is_container_view() || self.is_policy_view() || self.is_reference_view() {
            env.insert("RESOURCE_GROUP".to_owned(), String::new());
            env.insert("RESOURCE_VERSION".to_owned(), String::new());
            env.insert(
                "RESOURCE_NAME".to_owned(),
                if self.is_reference_view() {
                    "references"
                } else if self.is_container_view() {
                    "containers"
                } else {
                    "policy"
                }
                .to_owned(),
            );
        }
        env
    }

    #[allow(clippy::too_many_lines)]
    pub(crate) fn plugin_environment(&self) -> Option<PluginEnvironment> {
        let mut env = self.base_plugin_environment();
        let (namespace, name) = match self.overlay {
            Overlay::Contexts => {
                if !self
                    .visible_context_indices()
                    .contains(&self.overlay_selected)
                {
                    return None;
                }
                let item = self.context_options.get(self.overlay_selected)?;
                env.insert("RESOURCE_GROUP".to_owned(), String::new());
                env.insert("RESOURCE_VERSION".to_owned(), String::new());
                env.insert("RESOURCE_NAME".to_owned(), "contexts".to_owned());
                (String::new(), item.name.clone())
            }
            Overlay::Namespaces => {
                if !self
                    .visible_namespace_indices()
                    .contains(&self.overlay_selected)
                {
                    return None;
                }
                let item = self.namespace_options.get(self.overlay_selected)?;
                env.insert("RESOURCE_GROUP".to_owned(), String::new());
                env.insert("RESOURCE_VERSION".to_owned(), "v1".to_owned());
                env.insert("RESOURCE_NAME".to_owned(), "namespaces".to_owned());
                if item.name == "<all>" {
                    return None;
                }
                (String::new(), item.name.clone())
            }
            Overlay::PortForwards => {
                let pf = self.port_forwards.get(self.overlay_selected)?;
                env.insert("RESOURCE_GROUP".to_owned(), String::new());
                env.insert("RESOURCE_VERSION".to_owned(), String::new());
                env.insert("RESOURCE_NAME".to_owned(), "portforwards".to_owned());
                env.insert("ADDRESS".to_owned(), pf.address.clone());
                env.insert("LOCAL_PORT".to_owned(), pf.local_port.to_string());
                env.insert("REMOTE_PORT".to_owned(), pf.remote_port.to_string());
                (
                    pf.target.namespace.clone().unwrap_or_default(),
                    pf.target.name.clone(),
                )
            }
            Overlay::ScreenDumps => {
                if !self.visible_dump_indices().contains(&self.overlay_selected) {
                    return None;
                }
                let entry = self.dumps.entries.get(self.overlay_selected)?;
                env.insert("RESOURCE_GROUP".to_owned(), String::new());
                env.insert("RESOURCE_VERSION".to_owned(), String::new());
                env.insert("RESOURCE_NAME".to_owned(), "screendumps".to_owned());
                env.insert("COL-DIR".to_owned(), entry.dir_display());
                (String::new(), entry.name.clone())
            }
            Overlay::Aliases => {
                if !self
                    .visible_resource_indices()
                    .contains(&self.overlay_selected)
                {
                    return None;
                }
                let res = self.resource_options.get(self.overlay_selected)?;
                env.insert("RESOURCE_GROUP".to_owned(), res.group.to_string());
                env.insert("RESOURCE_VERSION".to_owned(), res.version.to_string());
                env.insert("RESOURCE_NAME".to_owned(), res.plural.to_string());
                (String::new(), res.qualified_name())
            }
            _ => {
                let row = self.selected_row()?;
                for (column, value) in crate::ui::plugin_column_values(self, row) {
                    env.insert(format!("COL-{}", column.to_uppercase()), value);
                }
                if let ListMode::Containers { namespace, pod } = &self.list_mode {
                    env.insert("POD".to_owned(), pod.clone());
                    env.insert("CONTAINER".to_owned(), row.name.to_string());
                    (namespace.clone(), row.name.to_string())
                } else {
                    if let Some(pod) = row.pod_data() {
                        env.insert("POD".to_owned(), row.name.to_string());
                        env.insert("CONTAINER".to_owned(), pod.default_container.to_string());
                    }
                    (
                        if row.namespace.as_ref() == "-" {
                            String::new()
                        } else {
                            row.namespace.to_string()
                        },
                        row.name.to_string(),
                    )
                }
            }
        };
        let filter = if matches!(self.overlay, Overlay::Contexts | Overlay::Namespaces) {
            &self.body_query
        } else {
            &self.query
        };
        env.insert(
            "FILTER".to_owned(),
            if filter.is_empty() {
                name.clone()
            } else {
                filter.clone()
            },
        );
        env.insert("NAMESPACE".to_owned(), namespace.clone());
        env.insert("NAME".to_owned(), name.clone());
        env.entry("COL-NAME".to_owned()).or_insert(name);
        env.entry("COL-NAMESPACE".to_owned()).or_insert(namespace);
        Some(env)
    }

    pub(crate) fn activate_plugin(&mut self, plugin: Plugin) -> AppCommand {
        if plugin.dangerous && self.read_only {
            return self.unsupported("This plugin is disabled in read-only mode");
        }
        let Some(environment) = self.plugin_environment() else {
            return self.unsupported("Select an item before running a plugin");
        };
        let values = plugin.default_inputs();
        let confirmation = if plugin.inputs.is_empty() {
            match plugin.invocation(&environment, &values) {
                Ok(invocation) if !plugin.should_confirm() => {
                    return AppCommand::Plugin(invocation);
                }
                Ok(invocation) => Some(invocation),
                Err(error) => return self.unsupported(&error),
            }
        } else {
            None
        };
        self.plugin_dialog = Some(PluginDialog {
            plugin,
            environment,
            values,
            focus: 0,
            confirmation,
            preview_scroll: 0,
            previous_overlay: self.overlay,
            edited_field: None,
        });
        self.overlay = Overlay::PluginDialog;
        self.notice = None;
        AppCommand::None
    }

    pub(crate) fn handle_plugin_dialog(&mut self, key: KeyEvent) -> AppCommand {
        if key.code == KeyCode::Esc {
            return self.close_plugin_dialog();
        }
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return if self.user_config.settings.no_exit_on_ctrl_c {
                self.close_plugin_dialog()
            } else {
                AppCommand::Quit
            };
        }
        let Some(dialog) = &mut self.plugin_dialog else {
            self.overlay = Overlay::None;
            return AppCommand::None;
        };
        match key.code {
            KeyCode::BackTab | KeyCode::Up => dialog.move_focus(false),
            KeyCode::Enter if dialog.focus == dialog.cancel_index() => {
                return self.close_plugin_dialog();
            }
            KeyCode::Enter if dialog.focus == dialog.cancel_index() + 1 => {
                return self.submit_plugin_dialog();
            }
            KeyCode::Enter | KeyCode::Tab | KeyCode::Down => dialog.move_focus(true),
            KeyCode::PageDown if dialog.confirmation.is_some() => {
                dialog.preview_scroll = dialog.preview_scroll.saturating_add(5);
            }
            KeyCode::PageUp if dialog.confirmation.is_some() => {
                dialog.preview_scroll = dialog.preview_scroll.saturating_sub(5);
            }
            _ => dialog.edit(key),
        }
        AppCommand::None
    }

    fn submit_plugin_dialog(&mut self) -> AppCommand {
        let Some(dialog) = &self.plugin_dialog else {
            return AppCommand::None;
        };
        if self.read_only && dialog.plugin.dangerous {
            self.close_plugin_dialog();
            return self.unsupported("This plugin is disabled in read-only mode");
        }
        if let Some(invocation) = dialog.confirmation.clone() {
            self.close_plugin_dialog();
            return AppCommand::Plugin(invocation);
        }
        let invocation = match dialog
            .plugin
            .invocation(&dialog.environment, &dialog.values)
        {
            Ok(invocation) => invocation,
            Err(error) => return self.unsupported(&error),
        };
        if dialog.plugin.should_confirm() {
            if let Some(dialog) = &mut self.plugin_dialog {
                dialog.confirmation = Some(invocation);
                dialog.focus = 0;
            }
            self.notice = None;
            AppCommand::None
        } else {
            self.close_plugin_dialog();
            AppCommand::Plugin(invocation)
        }
    }

    fn close_plugin_dialog(&mut self) -> AppCommand {
        if let Some(dialog) = self.plugin_dialog.take() {
            self.overlay = dialog.previous_overlay;
        }
        self.notice = None;
        AppCommand::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::UserConfig,
        model::{PodData, ResourceRow, ResourceSnapshot, SourceState},
        plugins::parse_plugin_file,
    };
    use std::{path::Path, sync::Arc, time::Instant};

    fn app() -> App {
        let mut app = App::new(
            "demo",
            None,
            ResourceKind::Pods,
            false,
            vec!["demo".to_owned()],
        );
        app.apply_snapshot(ResourceSnapshot {
            generation: 1,
            kind: ResourceKind::Pods.into(),
            rows: Arc::new(vec![Arc::new(
                ResourceRow::new("prod", "api-0", "Running", "1/1", "0", 0).with_pod_data(
                    PodData {
                        default_container: "main".into(),
                        ..PodData::default()
                    },
                ),
            )]),
            table_columns: Arc::from([]),
            state: SourceState::Ready,
            revision: 1,
            emitted_at: Instant::now(),
        });
        app
    }

    fn plugin(extra: &str) -> Plugin {
        let text = format!(
            "plugins:\n  inspect:\n    shortCut: v\n    description: Inspect selection\n    scopes: [pods]\n    command: echo\n    args: ['$NAME', '$NAMESPACE', '$POD', '$CONTAINER', '${{COL-NAME}}']\n{extra}"
        );
        parse_plugin_file(Path::new("plugins.yaml"), &text)
            .unwrap()
            .remove("inspect")
            .unwrap()
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn workloads_protect_sort_shortcuts_and_leave_absent_actions_available_to_plugins() {
        let mut app = app();
        app.kind = crate::model::ResourceDescriptor::workloads();
        for character in ['K', 'R'] {
            assert!(app.builtin_key_bound(key(KeyCode::Char(character))));
        }
        for event in [
            key(KeyCode::Char('l')),
            key(KeyCode::Char('J')),
            KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL),
        ] {
            assert!(!app.builtin_key_bound(event));
        }
        app.overlay = Overlay::Namespaces;
        assert!(app.builtin_key_bound(key(KeyCode::Char('l'))));
    }

    #[test]
    fn scoped_shortcuts_preserve_builtins_unless_overridden() {
        let mut app = app();
        let mut item = plugin("");
        // Parse the binding as well as its visible label.
        let text = "plugins:\n  shell:\n    shortCut: s\n    description: Shell override\n    scopes: [pods]\n    command: echo\n";
        let shell = parse_plugin_file(Path::new("plugins.yaml"), text)
            .unwrap()
            .remove("shell")
            .unwrap();
        app.user_config
            .plugins
            .plugins
            .insert("shell".to_owned(), shell.clone());
        assert_eq!(app.visible_plugins().len(), 0);
        assert!(matches!(
            app.handle_key(key(KeyCode::Char('s'))),
            AppCommand::Shell(_)
        ));
        let mut shell = shell;
        shell.override_builtin = true;
        app.user_config
            .plugins
            .plugins
            .insert("shell".to_owned(), shell);
        assert!(matches!(
            app.handle_key(key(KeyCode::Char('s'))),
            AppCommand::Plugin(_)
        ));
        item.scopes = vec!["deployments".to_owned()];
        app.user_config
            .plugins
            .plugins
            .insert("inspect".to_owned(), item);
        assert!(app.plugin_for_key(key(KeyCode::Char('v'))).is_none());
    }

    #[test]
    fn workload_previous_log_shortcut_requires_explicit_plugin_override() {
        let mut app = app();
        app.kind = ResourceKind::Deployments.into();
        app.snapshot.kind = app.kind.clone();
        let text = "plugins:\n  previous:\n    shortCut: p\n    description: Override previous logs\n    scopes: [deployments]\n    command: echo\n";
        let mut item = parse_plugin_file(Path::new("plugins.yaml"), text)
            .unwrap()
            .remove("previous")
            .unwrap();
        app.user_config
            .plugins
            .plugins
            .insert("previous".to_owned(), item.clone());
        assert_eq!(app.visible_plugins().len(), 0);
        assert!(matches!(
            app.handle_key(key(KeyCode::Char('p'))),
            AppCommand::Action(super::super::ActionIntent::StartLogs { .. })
        ));
        app.handle_key(key(KeyCode::Esc));
        item.override_builtin = true;
        app.user_config
            .plugins
            .plugins
            .insert("previous".to_owned(), item);
        assert_eq!(app.visible_plugins().len(), 1);
        assert!(matches!(
            app.handle_key(key(KeyCode::Char('p'))),
            AppCommand::Plugin(_)
        ));
    }

    #[test]
    fn plugin_menu_and_dispatch_agree_on_duplicate_overridden_shortcuts() {
        let mut app = app();
        let mut first = plugin("");
        first.name = "first".to_owned();
        let mut last = first.clone();
        last.name = "last".to_owned();
        last.override_builtin = true;
        app.user_config
            .plugins
            .plugins
            .insert(first.name.clone(), first);
        app.user_config
            .plugins
            .plugins
            .insert(last.name.clone(), last);
        assert_eq!(app.visible_plugins().len(), 1);
        assert_eq!(app.visible_plugins()[0].name, "last");
        assert_eq!(
            app.plugin_for_key(key(KeyCode::Char('v'))).unwrap().name,
            "last"
        );
        app.overlay = Overlay::Namespaces;
        assert!(app.plugin_scopes().contains(&"v1/namespaces".to_owned()));
        app.overlay = Overlay::None;
        app.list_mode = ListMode::Containers {
            namespace: "prod".to_owned(),
            pod: "parent".to_owned(),
        };
        let env = app.plugin_environment().unwrap();
        assert_eq!(env["RESOURCE_NAME"], "containers");
        assert_eq!(env["RESOURCE_VERSION"], "");
        assert!(env.contains_key("KUBECONFIG"));
        app.list_mode = ListMode::Policy {
            title: "Rules".to_owned(),
        };
        assert_eq!(app.plugin_scopes(), ["policy", "pol"]);
        assert_eq!(app.plugin_environment().unwrap()["RESOURCE_NAME"], "policy");
        app.overlay = Overlay::PortForwards;
        assert_eq!(app.plugin_scopes(), ["portforwards", "portforward", "pf"]);
        app.overlay = Overlay::ScreenDumps;
        assert_eq!(app.plugin_scopes(), ["screendumps", "screendump", "sd"]);
        app.overlay = Overlay::Aliases;
        assert_eq!(app.plugin_scopes(), ["aliases", "alias"]);
    }

    #[test]
    fn plugin_environment_tracks_selected_row_and_connection_overrides() {
        let mut app = app();
        app.connection.cluster = Some("override-cluster".to_owned());
        app.connection.impersonate = Some("alice".to_owned());
        app.connection.impersonate_groups = vec!["readers".to_owned()];
        let AppCommand::Plugin(invocation) = app.activate_plugin(plugin("")) else {
            panic!("direct invocation");
        };
        assert_eq!(invocation.args, ["api-0", "prod", "api-0", "main", "api-0"]);
        assert_eq!(invocation.env["CLUSTER"], "override-cluster");
        assert_eq!(invocation.env["USER"], "alice");
        assert_eq!(invocation.env["GROUPS"], "readers");
        assert_eq!(invocation.env["RESOURCE_VERSION"], "v1");
        assert_eq!(invocation.env["FILTER"], "api-0");
        app.overlay = Overlay::Contexts;
        app.body_query = "missing".to_owned();
        assert!(app.plugin_environment().is_none());
        app.body_query = "demo".to_owned();
        let env = app.plugin_environment().unwrap();
        assert_eq!(env["FILTER"], "demo");
        assert_eq!(env["RESOURCE_NAME"], "contexts");
    }

    #[test]
    fn input_validation_and_confirmation_freeze_selection_until_execution() {
        let mut app = app();
        let mut item = plugin(
            "    inputs:\n      - name: replicas\n        label: Replicas\n        type: number\n        required: true\n",
        );
        item.args = vec!["$NAME".to_owned(), "$INPUT_REPLICAS".to_owned()];
        assert_eq!(app.activate_plugin(item), AppCommand::None);
        assert_eq!(app.overlay, Overlay::PluginDialog);
        // Empty required field cannot be submitted.
        app.plugin_dialog.as_mut().unwrap().focus = 2;
        assert_eq!(app.handle_key(key(KeyCode::Enter)), AppCommand::None);
        assert!(app.notice.as_deref().unwrap().contains("replicas"));
        app.plugin_dialog.as_mut().unwrap().focus = 0;
        app.handle_key(key(KeyCode::Char('1')));
        app.handle_key(key(KeyCode::Tab));
        app.handle_key(key(KeyCode::Tab));
        app.handle_key(key(KeyCode::Enter));
        assert!(app.plugin_dialog.as_ref().unwrap().confirmation.is_some());
        assert_eq!(app.plugin_dialog.as_ref().unwrap().focus, 0);
        app.handle_key(key(KeyCode::Tab));
        let AppCommand::Plugin(invocation) = app.handle_key(key(KeyCode::Enter)) else {
            panic!("confirmed invocation");
        };
        assert_eq!(invocation.args, ["api-0", "1"]);
        assert_eq!(app.overlay, Overlay::None);
        assert!(app.plugin_dialog.is_none());
    }

    #[test]
    fn readonly_and_context_reload_cancel_pending_plugin_actions() {
        let mut app = app();
        let item = plugin("    dangerous: true\n    confirm: true\n");
        app.activate_plugin(item.clone());
        let mut config = UserConfig::default();
        config.settings.read_only = true;
        app.apply_user_config(config);
        assert!(app.plugin_dialog.is_none());
        assert_ne!(app.overlay, Overlay::PluginDialog);
        assert_eq!(app.activate_plugin(item), AppCommand::None);
        assert!(app.notice.as_deref().unwrap().contains("read-only"));
        // Non-dangerous plugins remain available in read-only mode.
        assert!(matches!(
            app.activate_plugin(plugin("")),
            AppCommand::Plugin(_)
        ));
        app.activate_plugin(plugin("    confirm: true\n"));
        app.apply_user_config(app.user_config.clone());
        assert_eq!(app.overlay, Overlay::PluginDialog);
        app.begin_config_load();
        assert!(app.plugin_dialog.is_none());
        assert_ne!(app.overlay, Overlay::PluginDialog);
    }

    #[test]
    fn typed_fields_cycle_and_cancel_returns_to_chooser() {
        let mut app = app();
        app.overlay = Overlay::Contexts;
        let item = plugin(
            "    inputs:\n      - name: enabled\n        type: bool\n      - name: mode\n        type: dropdown\n        options: [first, second]\n",
        );
        app.activate_plugin(item);
        app.handle_key(key(KeyCode::Char(' ')));
        app.handle_key(key(KeyCode::Tab));
        app.handle_key(key(KeyCode::Right));
        app.handle_key(key(KeyCode::Right));
        let form = app.plugin_dialog.as_ref().unwrap();
        assert_eq!(form.values["enabled"], "true");
        assert_eq!(form.values["mode"], "second");
        app.handle_key(key(KeyCode::Right));
        assert_eq!(app.plugin_dialog.as_ref().unwrap().values["mode"], "");
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.overlay, Overlay::Contexts);
    }
}
