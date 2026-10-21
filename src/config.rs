//! User configuration, command aliases, and command hotkeys.
//!
//! Files use the K9s YAML formats, but are read only from Kux directories.
//! Context files override global entries. Polling keeps the last valid version
//! of a malformed file until it is repaired, without losing other file updates.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs::File;
use std::io::{ErrorKind, Read};
use std::path::{Path, PathBuf};

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use serde::Deserialize;

const MAX_CONFIG_BYTES: u64 = 1_048_576;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigPaths {
    pub config_dir: PathBuf,
    pub data_dir: PathBuf,
}

impl ConfigPaths {
    #[must_use]
    pub fn new(config_dir: impl Into<PathBuf>, data_dir: impl Into<PathBuf>) -> Self {
        Self {
            config_dir: config_dir.into(),
            data_dir: data_dir.into(),
        }
    }

    /// Resolve Kux/K9s overrides, then XDG paths, then home-directory defaults.
    ///
    /// # Errors
    /// Returns an error if a selected path is relative or no home directory is
    /// available for a required default.
    pub fn from_env() -> Result<Self, String> {
        Self::from_lookup(|key| std::env::var_os(key).map(PathBuf::from), Path::exists)
    }

    fn from_lookup(
        lookup: impl Fn(&str) -> Option<PathBuf>,
        exists: impl Fn(&Path) -> bool,
    ) -> Result<Self, String> {
        let directory = |override_name, k9s_override, xdg_name, fallback: &str| {
            let (name, path) = if let Some(path) =
                lookup(override_name).filter(|p| !p.as_os_str().is_empty())
            {
                (override_name, path)
            } else if let Some(path) = lookup(k9s_override).filter(|p| !p.as_os_str().is_empty()) {
                (k9s_override, path)
            } else if let Some(path) = lookup(xdg_name).filter(|p| !p.as_os_str().is_empty()) {
                let kux_path = path.join("kux");
                let k9s_path = path.join("k9s");
                if !exists(&kux_path) && exists(&k9s_path) {
                    (xdg_name, k9s_path)
                } else {
                    (xdg_name, kux_path)
                }
            } else {
                let home = lookup("HOME")
                    .filter(|p| !p.as_os_str().is_empty())
                    .ok_or_else(|| {
                        format!(
                            "Set {override_name}, {xdg_name}, or HOME to locate Kux configuration"
                        )
                    })?;
                let base = home.join(fallback);
                let kux_path = base.join("kux");
                let k9s_path = base.join("k9s");
                if !exists(&kux_path) && exists(&k9s_path) {
                    ("HOME", k9s_path)
                } else {
                    ("HOME", kux_path)
                }
            };
            if !path.is_absolute() {
                return Err(format!(
                    "{name} must be an absolute path: {}",
                    path.display()
                ));
            }
            Ok(path)
        };
        Ok(Self::new(
            directory(
                "KUX_CONFIG_DIR",
                "K9S_CONFIG_DIR",
                "XDG_CONFIG_HOME",
                ".config",
            )?,
            directory(
                "KUX_DATA_DIR",
                "K9S_DATA_DIR",
                "XDG_DATA_HOME",
                ".local/share",
            )?,
        ))
    }

    /// Encode names as individual path components; context names may contain
    /// slashes, colons, or cloud resource identifiers.
    #[must_use]
    pub fn context_dir(&self, cluster: &str, context: &str) -> PathBuf {
        self.data_dir
            .join("clusters")
            .join(path_component(cluster))
            .join(path_component(context))
    }
}

fn path_component(value: &str) -> String {
    use std::fmt::Write;

    if value.is_empty() {
        return "%00".to_owned();
    }
    let mut output = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.') {
            output.push(char::from(byte));
        } else {
            let _ = write!(output, "%{byte:02X}");
        }
    }
    if output == "." || output == ".." {
        output = output.replace('.', "%2E");
    }
    output
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)]
pub struct Settings {
    pub read_only: bool,
    pub no_exit_on_ctrl_c: bool,
    pub live_view_auto_refresh: bool,
    pub default_view: Option<String>,
    pub refresh_rate: Option<std::time::Duration>,
    pub screen_dump_dir: Option<PathBuf>,
    pub port_forward_address: Option<String>,
    pub max_conn_retry: Option<u32>,
    pub disable_pod_counting: bool,
    pub thresholds: ThresholdSettings,
    pub ui: UiSettings,
    pub logger: LoggerSettings,
    pub shell_pod: ShellPodSettings,
    pub image_scans: ImageScansSettings,
    pub feature_gates: BTreeMap<String, FeatureGates>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct SeverityThreshold {
    #[serde(default = "default_critical_threshold")]
    pub critical: u8,
    #[serde(default = "default_warn_threshold")]
    pub warn: u8,
}

const fn default_critical_threshold() -> u8 {
    90
}

const fn default_warn_threshold() -> u8 {
    70
}

impl Default for SeverityThreshold {
    fn default() -> Self {
        Self {
            critical: 90,
            warn: 70,
        }
    }
}

impl SeverityThreshold {
    #[must_use]
    pub const fn normalized(mut self) -> Self {
        if self.warn == 0 || self.warn > 100 {
            self.warn = 70;
        }
        if self.critical == 0 || self.critical > 100 {
            self.critical = 90;
        }
        self
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct ThresholdSettings {
    #[serde(default)]
    pub cpu: SeverityThreshold,
    #[serde(default)]
    pub memory: SeverityThreshold,
}

impl ThresholdSettings {
    #[must_use]
    pub const fn normalized(self) -> Self {
        Self {
            cpu: self.cpu.normalized(),
            memory: self.memory.normalized(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageScansSettings {
    #[serde(default)]
    pub enable: bool,
    #[serde(default)]
    pub exclusions: ScanExcludes,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanExcludes {
    #[serde(default)]
    pub namespaces: Vec<String>,
    #[serde(default)]
    pub labels: BTreeMap<String, Vec<String>>,
}

impl ImageScansSettings {
    #[must_use]
    pub fn should_exclude(&self, namespace: &str, labels_csv: &str) -> bool {
        if !self.enable {
            return false;
        }
        if self.exclusions.namespaces.iter().any(|ns| ns == namespace) {
            return true;
        }
        if self.exclusions.labels.is_empty() || labels_csv.is_empty() {
            return false;
        }
        for pair in labels_csv.split(',') {
            if let Some((k, v)) = pair.split_once('=')
                && let Some(vals) = self.exclusions.labels.get(k.trim())
                && vals.iter().any(|val| val == v.trim())
            {
                return true;
            }
        }
        false
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FeatureGates {
    #[serde(default)]
    pub node_shell: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellPodSettings {
    pub image: String,
    pub namespace: String,
    pub limits: BTreeMap<String, String>,
    pub command: Vec<String>,
    pub args: Vec<String>,
    pub host_path_volume: Vec<HostPathMount>,
}

impl Default for ShellPodSettings {
    fn default() -> Self {
        let mut limits = BTreeMap::new();
        limits.insert("cpu".to_owned(), "100m".to_owned());
        limits.insert("memory".to_owned(), "100Mi".to_owned());
        Self {
            image: "busybox:1.35.0".to_owned(),
            namespace: "default".to_owned(),
            limits,
            command: Vec::new(),
            args: Vec::new(),
            host_path_volume: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HostPathMount {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub mount_path: String,
    #[serde(default)]
    pub sub_path: String,
    #[serde(default)]
    pub host_path: String,
    #[serde(default)]
    pub read_only: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CustomViewSpec {
    pub sort_column: Option<(String, bool)>,
    pub columns: Vec<CustomColumnSpec>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CustomColumnSpec {
    pub name: String,
    pub json_path: Option<String>,
    pub attrs: String,
}

impl CustomColumnSpec {
    #[must_use]
    pub fn parse(spec: &str) -> Option<Self> {
        let trimmed = spec.trim();
        if trimmed.is_empty() {
            return None;
        }
        let (left, attrs) = match trimmed.split_once('|') {
            Some((left, attrs)) => (left.trim(), attrs.trim().to_owned()),
            None => (trimmed, String::new()),
        };
        let (name, json_path) = match left.split_once(':') {
            Some((name, path)) => {
                let name = name.trim();
                let path = path.trim();
                if name.is_empty() {
                    return None;
                }
                (name.to_owned(), (!path.is_empty()).then(|| path.to_owned()))
            }
            None => (left.to_owned(), None),
        };
        Some(Self {
            name,
            json_path,
            attrs,
        })
    }

    #[must_use]
    pub fn is_wide_only(&self) -> bool {
        self.attrs.contains('W')
    }

    #[must_use]
    pub fn is_hidden(&self) -> bool {
        self.attrs.contains('H')
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CustomJumpSpec {
    pub resource: String,
    pub namespace: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
pub struct BenchAuth {
    #[serde(default)]
    pub user: String,
    #[serde(default)]
    pub password: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct BenchHttp {
    #[serde(default = "default_bench_method")]
    pub method: String,
    #[serde(default)]
    pub host: String,
    #[serde(default = "default_bench_path")]
    pub path: String,
    #[serde(default)]
    pub http2: bool,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub headers: BTreeMap<String, Vec<String>>,
}

fn default_bench_method() -> String {
    "GET".to_owned()
}

fn default_bench_path() -> String {
    "/".to_owned()
}

impl Default for BenchHttp {
    fn default() -> Self {
        Self {
            method: default_bench_method(),
            host: String::new(),
            path: default_bench_path(),
            http2: false,
            body: String::new(),
            headers: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BenchConfig {
    pub concurrency: usize,
    pub requests: usize,
    pub auth: BenchAuth,
    pub http: BenchHttp,
}

impl Default for BenchConfig {
    fn default() -> Self {
        Self {
            concurrency: 1,
            requests: 200,
            auth: BenchAuth::default(),
            http: BenchHttp::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BenchmarksConfig {
    pub defaults_concurrency: usize,
    pub defaults_requests: usize,
    pub services: BTreeMap<String, BenchConfig>,
    pub containers: BTreeMap<String, BenchConfig>,
}

impl Default for BenchmarksConfig {
    fn default() -> Self {
        Self {
            defaults_concurrency: 1,
            defaults_requests: 200,
            services: BTreeMap::new(),
            containers: BTreeMap::new(),
        }
    }
}

pub const DEFAULT_LOG_TAIL: i64 = 100;
pub const MAX_LOG_BUFFER_LINES: usize = 5_000;

/// Defaults applied whenever a log viewer opens. The numeric bounds match
/// K9s logger validation and keep both stream backlogs and displayed text bounded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)]
pub struct LoggerSettings {
    pub tail: i64,
    pub buffer: usize,
    pub since_seconds: i64,
    pub text_wrap: bool,
    pub disable_autoscroll: bool,
    pub column_lock: bool,
    pub show_time: bool,
}

impl Default for LoggerSettings {
    fn default() -> Self {
        Self {
            tail: DEFAULT_LOG_TAIL,
            buffer: MAX_LOG_BUFFER_LINES,
            since_seconds: -1,
            text_wrap: false,
            disable_autoscroll: false,
            column_lock: false,
            show_time: false,
        }
    }
}

impl LoggerSettings {
    #[must_use]
    pub fn normalized(mut self) -> Self {
        self.tail = if self.tail <= 0 {
            DEFAULT_LOG_TAIL
        } else {
            self.tail.min(5_000)
        };
        if self.buffer == 0 || self.buffer > MAX_LOG_BUFFER_LINES {
            self.buffer = MAX_LOG_BUFFER_LINES;
        }
        if self.since_seconds == 0 {
            self.since_seconds = -1;
        }
        self
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)]
pub struct UiSettings {
    pub headless: bool,
    pub logoless: bool,
    pub crumbsless: bool,
    pub no_icons: bool,
    pub splashless: bool,
    pub invert: bool,
    pub fullscreen: bool,
    pub reactive: bool,
    pub enable_mouse: bool,
    pub skin: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigDiagnostic {
    pub path: PathBuf,
    pub message: String,
}

impl fmt::Display for ConfigDiagnostic {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.path.display(), self.message)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hotkey {
    pub name: String,
    pub shortcut: String,
    pub description: String,
    pub command: String,
    binding: KeyShortcut,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UserConfig {
    pub aliases: BTreeMap<String, String>,
    pub hotkeys: Vec<Hotkey>,
    pub views: BTreeMap<String, CustomViewSpec>,
    pub jumps: BTreeMap<String, CustomJumpSpec>,
    pub benchmarks: BenchmarksConfig,
    pub settings: Settings,
    pub diagnostics: Vec<ConfigDiagnostic>,
    pub plugins: crate::plugins::PluginCatalog,
    pub theme: crate::theme::Skin,
}

impl UserConfig {
    #[must_use]
    pub fn load(paths: &ConfigPaths, cluster: Option<&str>, context: Option<&str>) -> Self {
        ConfigWatcher::new(paths.clone(), cluster, context)
            .reload_if_changed()
            .unwrap_or_default()
    }

    #[must_use]
    pub fn custom_view_for(
        &self,
        descriptor: &crate::model::ResourceDescriptor,
        namespace: Option<&str>,
    ) -> Option<&CustomViewSpec> {
        for (cfg_key, spec) in &self.views {
            if let Some((res_part, ns_part)) = cfg_key.split_once('@')
                && Some(ns_part) == namespace.filter(|ns| !ns.is_empty())
                && custom_resource_key_matches(descriptor, res_part)
            {
                return Some(spec);
            }
        }
        for (cfg_key, spec) in &self.views {
            if !cfg_key.contains('@') && custom_resource_key_matches(descriptor, cfg_key) {
                return Some(spec);
            }
        }
        None
    }

    #[must_use]
    pub fn custom_jump_for(
        &self,
        descriptor: &crate::model::ResourceDescriptor,
    ) -> Option<&CustomJumpSpec> {
        for (cfg_key, spec) in &self.jumps {
            if custom_resource_key_matches(descriptor, cfg_key) {
                return Some(spec);
            }
        }
        None
    }

    #[must_use]
    pub fn node_shell_enabled(&self, context: &str) -> bool {
        self.settings
            .feature_gates
            .get(context)
            .or_else(|| self.settings.feature_gates.get("default"))
            .is_some_and(|gates| gates.node_shell)
    }

    #[must_use]
    pub fn shell_pod_for(&self, _context: &str) -> &ShellPodSettings {
        &self.settings.shell_pod
    }

    #[must_use]
    pub fn bench_config_for_service(&self, key: &str) -> BenchConfig {
        if let Some(cfg) = self.benchmarks.services.get(key) {
            return cfg.clone();
        }
        BenchConfig {
            concurrency: self.benchmarks.defaults_concurrency.max(1),
            requests: self.benchmarks.defaults_requests.max(1),
            ..BenchConfig::default()
        }
    }

    #[must_use]
    pub fn bench_config_for_container(&self, key: &str) -> BenchConfig {
        if let Some(cfg) = self.benchmarks.containers.get(key) {
            return cfg.clone();
        }
        if let Some((svc_key, _)) = key.split_once(':')
            && let Some(cfg) = self.benchmarks.services.get(svc_key)
        {
            return cfg.clone();
        }
        BenchConfig {
            concurrency: self.benchmarks.defaults_concurrency.max(1),
            requests: self.benchmarks.defaults_requests.max(1),
            ..BenchConfig::default()
        }
    }

    /// Expand the first command word repeatedly, retaining arguments verbatim.
    /// An optional leading command-mode colon is accepted.
    ///
    /// # Errors
    /// Returns an error for recursive alias definitions.
    pub fn expand_command(&self, command: &str) -> Result<String, String> {
        let mut expanded = command.trim().trim_start_matches(':').to_owned();
        let mut seen = BTreeSet::new();
        loop {
            let first_end = expanded.find(char::is_whitespace).unwrap_or(expanded.len());
            let name = &expanded[..first_end];
            let Some(value) = self.aliases.get(name) else {
                return Ok(expanded);
            };
            if !seen.insert(name.to_owned()) {
                return Err(format!("Alias cycle detected at {name:?}"));
            }
            expanded = format!(
                "{}{}",
                value.trim().trim_start_matches(':'),
                &expanded[first_end..]
            );
        }
    }

    #[must_use]
    pub fn hotkey_for(&self, key: KeyEvent) -> Option<&Hotkey> {
        if key.kind == KeyEventKind::Release {
            return None;
        }
        let binding = KeyShortcut::normalized(key.code, key.modifiers);
        self.hotkeys.iter().find(|hotkey| hotkey.binding == binding)
    }
}

fn custom_resource_key_matches(
    descriptor: &crate::model::ResourceDescriptor,
    cfg_key: &str,
) -> bool {
    let gvr = format!("{}/{}", descriptor.api_version, descriptor.plural);
    let mut keys = vec![
        gvr,
        descriptor.qualified_name(),
        descriptor.plural.to_string(),
        descriptor.kind.to_ascii_lowercase(),
    ];
    for alias in descriptor.command_aliases() {
        if !keys.contains(&alias) {
            keys.push(alias);
        }
    }
    if keys.iter().any(|k| k.eq_ignore_ascii_case(cfg_key)) {
        return true;
    }
    if !descriptor.group.is_empty()
        && let Some((group_ver, plural)) = cfg_key.rsplit_once('/')
        && let Some((group, _version)) = group_ver.split_once('/')
        && group.eq_ignore_ascii_case(&descriptor.group)
        && (plural.eq_ignore_ascii_case(&descriptor.plural)
            || plural.eq_ignore_ascii_case(&descriptor.kind))
    {
        return true;
    }
    false
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct KeyShortcut {
    code: KeyCode,
    modifiers: KeyModifiers,
}

impl KeyShortcut {
    pub(crate) fn event(self) -> KeyEvent {
        let mut code = self.code;
        if self.modifiers.contains(KeyModifiers::SHIFT)
            && let KeyCode::Char(character) = code
        {
            if character.is_ascii_lowercase() {
                code = KeyCode::Char(character.to_ascii_uppercase());
            } else if let Some(index) = "0123456789-=[]\\;',./`"
                .chars()
                .position(|base| base == character)
                && let Some(shifted) = ")!@#$%^&*(_+{}|:\"<>?~".chars().nth(index)
            {
                code = KeyCode::Char(shifted);
            }
        }
        KeyEvent::new(code, self.modifiers)
    }

    pub(crate) fn parse(shortcut: &str) -> Result<Self, String> {
        let mut remaining = shortcut.trim();
        let mut modifiers = KeyModifiers::NONE;
        while let Some((prefix, rest)) = remaining.split_once('-') {
            let modifier = match prefix.to_ascii_lowercase().as_str() {
                "ctrl" | "control" => KeyModifiers::CONTROL,
                "alt" | "option" => KeyModifiers::ALT,
                "shift" => KeyModifiers::SHIFT,
                "super" | "cmd" => KeyModifiers::SUPER,
                "meta" => KeyModifiers::META,
                "hyper" => KeyModifiers::HYPER,
                _ => break,
            };
            modifiers.insert(modifier);
            remaining = rest;
        }
        let lower = remaining.to_ascii_lowercase();
        let code = match lower.as_str() {
            "backspace" => KeyCode::Backspace,
            "enter" | "return" => KeyCode::Enter,
            "left" => KeyCode::Left,
            "right" => KeyCode::Right,
            "up" => KeyCode::Up,
            "down" => KeyCode::Down,
            "home" => KeyCode::Home,
            "end" => KeyCode::End,
            "pageup" | "pgup" => KeyCode::PageUp,
            "pagedown" | "pgdn" => KeyCode::PageDown,
            "tab" => KeyCode::Tab,
            "backtab" => KeyCode::BackTab,
            "delete" | "del" => KeyCode::Delete,
            "insert" | "ins" => KeyCode::Insert,
            "esc" | "escape" => KeyCode::Esc,
            "space" => KeyCode::Char(' '),
            _ => {
                if let Some(number) = lower.strip_prefix('f').and_then(|n| n.parse::<u8>().ok()) {
                    if !(1..=24).contains(&number) {
                        return Err(format!(
                            "Unsupported function key {remaining:?}; use F1 through F24"
                        ));
                    }
                    KeyCode::F(number)
                } else {
                    let mut characters = remaining.chars();
                    match (characters.next(), characters.next()) {
                        (Some(character), None) => {
                            // Uppercase letters in names like Ctrl-P are conventional
                            // key labels, not an implicit Shift modifier.
                            KeyCode::Char(if modifiers.is_empty() {
                                character
                            } else {
                                character.to_ascii_lowercase()
                            })
                        }
                        _ => return Err(format!("Unsupported shortcut {shortcut:?}")),
                    }
                }
            }
        };
        Ok(Self::normalized(code, modifiers))
    }

    pub(crate) fn normalized(mut code: KeyCode, mut modifiers: KeyModifiers) -> Self {
        if code == KeyCode::BackTab {
            code = KeyCode::Tab;
            modifiers.insert(KeyModifiers::SHIFT);
        }
        if let KeyCode::Char(character) = code {
            if character.is_ascii_uppercase() {
                code = KeyCode::Char(character.to_ascii_lowercase());
                modifiers.insert(KeyModifiers::SHIFT);
            } else if let Some(index) = ")!@#$%^&*(_+{}|:\"<>?~"
                .chars()
                .position(|item| item == character)
                && let Some(base) = "0123456789-=[]\\;',./`".chars().nth(index)
            {
                code = KeyCode::Char(base);
                modifiers.insert(KeyModifiers::SHIFT);
            }
        }
        Self { code, modifiers }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SettingsPatch {
    read_only: Option<bool>,
    no_exit_on_ctrl_c: Option<bool>,
    live_view_auto_refresh: Option<bool>,
    default_view: Option<String>,
    #[serde(default, deserialize_with = "deserialize_refresh_rate")]
    refresh_rate: Option<std::time::Duration>,
    screen_dump_dir: Option<PathBuf>,
    port_forward_address: Option<String>,
    max_conn_retry: Option<u32>,
    disable_pod_counting: Option<bool>,
    thresholds: Option<ThresholdSettings>,
    skin: Option<String>,
    #[serde(default)]
    ui: UiPatch,
    #[serde(default)]
    logger: LoggerPatch,
    #[serde(default)]
    shell_pod: ShellPodPatch,
    #[serde(default)]
    image_scans: Option<ImageScansSettings>,
    #[serde(default)]
    feature_gates: Option<FeatureGates>,
    #[serde(default)]
    clusters: BTreeMap<String, ClusterSettingsPatch>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClusterSettingsPatch {
    #[serde(default)]
    feature_gates: Option<FeatureGates>,
    #[serde(default)]
    shell_pod: Option<ShellPodPatch>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ShellPodPatch {
    image: Option<String>,
    namespace: Option<String>,
    limits: Option<BTreeMap<String, String>>,
    command: Option<Vec<String>>,
    args: Option<Vec<String>>,
    #[serde(default, alias = "hostPathVolume")]
    host_path_volume: Option<Vec<HostPathMount>>,
}

impl ShellPodPatch {
    fn apply(&self, settings: &mut ShellPodSettings) {
        if let Some(image) = &self.image
            && !image.trim().is_empty()
        {
            image.trim().clone_into(&mut settings.image);
        }
        if let Some(namespace) = &self.namespace
            && !namespace.trim().is_empty()
        {
            namespace.trim().clone_into(&mut settings.namespace);
        }
        if let Some(limits) = &self.limits
            && !limits.is_empty()
        {
            settings.limits.clone_from(limits);
        }
        if let Some(command) = &self.command {
            settings.command.clone_from(command);
        }
        if let Some(args) = &self.args {
            settings.args.clone_from(args);
        }
        if let Some(volumes) = &self.host_path_volume {
            settings.host_path_volume.clone_from(volumes);
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LoggerPatch {
    tail: Option<i64>,
    buffer: Option<i64>,
    since_seconds: Option<i64>,
    text_wrap: Option<bool>,
    disable_autoscroll: Option<bool>,
    column_lock: Option<bool>,
    show_time: Option<bool>,
}

impl LoggerPatch {
    fn apply(&self, settings: &mut LoggerSettings) {
        if let Some(value) = self.tail {
            settings.tail = value;
        }
        if let Some(value) = self.buffer {
            settings.buffer = usize::try_from(value).unwrap_or_default();
        }
        if let Some(value) = self.since_seconds {
            settings.since_seconds = value;
        }
        for (target, value) in [
            (&mut settings.text_wrap, self.text_wrap),
            (&mut settings.disable_autoscroll, self.disable_autoscroll),
            (&mut settings.column_lock, self.column_lock),
            (&mut settings.show_time, self.show_time),
        ] {
            if let Some(value) = value {
                *target = value;
            }
        }
        *settings = settings.normalized();
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UiPatch {
    headless: Option<bool>,
    logoless: Option<bool>,
    crumbsless: Option<bool>,
    no_icons: Option<bool>,
    splashless: Option<bool>,
    invert: Option<bool>,
    reactive: Option<bool>,
    enable_mouse: Option<bool>,
    #[serde(rename = "defaultsToFullScreen", alias = "fullscreen")]
    fullscreen: Option<bool>,
    skin: Option<String>,
}

impl SettingsPatch {
    fn apply(&self, settings: &mut Settings) {
        self.logger.apply(&mut settings.logger);
        self.shell_pod.apply(&mut settings.shell_pod);
        if let Some(scans) = &self.image_scans {
            settings.image_scans.clone_from(scans);
        }
        if let Some(gates) = &self.feature_gates {
            settings
                .feature_gates
                .insert("default".to_owned(), gates.clone());
        }
        for (cluster_name, cluster_patch) in &self.clusters {
            if let Some(gates) = &cluster_patch.feature_gates {
                settings
                    .feature_gates
                    .insert(cluster_name.clone(), gates.clone());
            }
            if let Some(shell_pod) = &cluster_patch.shell_pod {
                shell_pod.apply(&mut settings.shell_pod);
            }
        }
        if let Some(value) = self.refresh_rate {
            settings.refresh_rate = Some(value);
        }
        if let Some(dir) = &self.screen_dump_dir
            && !dir.as_os_str().is_empty()
        {
            settings.screen_dump_dir = Some(dir.clone());
        }
        if let Some(addr) = &self.port_forward_address
            && !addr.trim().is_empty()
        {
            settings.port_forward_address = Some(addr.trim().to_owned());
        }
        if let Some(retry) = self.max_conn_retry {
            settings.max_conn_retry = Some(retry);
        }
        if let Some(disable) = self.disable_pod_counting {
            settings.disable_pod_counting = disable;
        }
        if let Some(thresholds) = &self.thresholds {
            settings.thresholds = thresholds.clone().normalized();
        }
        if let Some(value) = self.read_only {
            settings.read_only = value;
        }
        if let Some(value) = self.no_exit_on_ctrl_c {
            settings.no_exit_on_ctrl_c = value;
        }
        if let Some(value) = self.live_view_auto_refresh {
            settings.live_view_auto_refresh = value;
        }
        if let Some(value) = &self.default_view {
            settings.default_view = (!value.trim().is_empty()).then(|| value.clone());
        }
        if let Some(value) = self.ui.skin.as_ref().or(self.skin.as_ref()) {
            settings.ui.skin = (!value.trim().is_empty()).then(|| value.clone());
        }
        for (target, value) in [
            (&mut settings.ui.headless, self.ui.headless),
            (&mut settings.ui.logoless, self.ui.logoless),
            (&mut settings.ui.crumbsless, self.ui.crumbsless),
            (&mut settings.ui.no_icons, self.ui.no_icons),
            (&mut settings.ui.splashless, self.ui.splashless),
            (&mut settings.ui.invert, self.ui.invert),
            (&mut settings.ui.fullscreen, self.ui.fullscreen),
            (&mut settings.ui.reactive, self.ui.reactive),
            (&mut settings.ui.enable_mouse, self.ui.enable_mouse),
        ] {
            if let Some(value) = value {
                *target = value;
            }
        }
    }
}

fn deserialize_refresh_rate<'de, D>(
    deserializer: D,
) -> Result<Option<std::time::Duration>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let Some(seconds) = Option::<f64>::deserialize(deserializer)? else {
        return Ok(None);
    };
    if !seconds.is_finite() || seconds <= 0.0 {
        return Err(serde::de::Error::custom(
            "refreshRate must be a finite positive number of seconds",
        ));
    }
    let duration = std::time::Duration::try_from_secs_f64(seconds.max(2.0))
        .map_err(|_| serde::de::Error::custom("refreshRate is too large"))?;
    if std::time::Instant::now().checked_add(duration).is_none() {
        return Err(serde::de::Error::custom(
            "refreshRate exceeds the platform timer range",
        ));
    }
    Ok(Some(duration))
}

#[derive(Debug, Deserialize)]
struct SettingsDocument {
    #[serde(alias = "k9s")]
    kux: SettingsPatch,
}

#[derive(Debug, Deserialize)]
struct AliasDocument {
    aliases: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
struct HotkeyDocument {
    #[serde(rename = "hotKeys")]
    hotkeys: BTreeMap<String, RawHotkey>,
}

#[derive(Debug, Deserialize)]
struct RawHotkey {
    #[serde(rename = "shortCut", alias = "shortcut")]
    shortcut: String,
    #[serde(default)]
    description: String,
    command: String,
}

#[derive(Debug, Deserialize)]
struct ViewsDocument {
    #[serde(alias = "kux")]
    k9s: Option<ViewsRoot>,
    #[serde(default)]
    views: BTreeMap<String, RawViewSpec>,
}

#[derive(Debug, Deserialize)]
struct ViewsRoot {
    #[serde(default)]
    views: BTreeMap<String, RawViewSpec>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawViewSpec {
    sort_column: Option<String>,
    #[serde(default)]
    columns: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct JumpsDocument {
    #[serde(default)]
    jumps: BTreeMap<String, RawJumpSpec>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawJumpSpec {
    resource: String,
    namespace: Option<String>,
}

#[derive(Debug, Deserialize)]
struct BenchmarksDocument {
    benchmarks: Option<RawBenchmarksSection>,
}

#[derive(Debug, Default, Deserialize)]
struct RawBenchmarksSection {
    defaults: Option<RawBenchDefaults>,
    #[serde(default)]
    services: BTreeMap<String, RawBenchEntry>,
    #[serde(default)]
    containers: BTreeMap<String, RawBenchEntry>,
}

#[derive(Debug, Default, Deserialize)]
struct RawBenchDefaults {
    concurrency: Option<usize>,
    requests: Option<usize>,
}

#[derive(Debug, Default, Deserialize)]
struct RawBenchEntry {
    concurrency: Option<usize>,
    requests: Option<usize>,
    #[serde(default)]
    auth: BenchAuth,
    #[serde(default)]
    http: BenchHttp,
}

#[derive(Debug, Clone, Copy)]
enum FileKind {
    Settings,
    Aliases,
    Hotkeys,
    Views,
    Jumps,
    Benchmarks,
}

#[derive(Debug, Clone)]
enum ConfigLayer {
    Settings(Box<SettingsPatch>),
    Aliases(BTreeMap<String, String>),
    Hotkeys(Vec<Hotkey>),
    Views(BTreeMap<String, CustomViewSpec>),
    Jumps(BTreeMap<String, CustomJumpSpec>),
    Benchmarks(BenchmarksConfig),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum FileContents {
    Missing,
    Text(String),
    Error(String),
}

#[derive(Debug)]
struct WatchedFile {
    path: PathBuf,
    kind: FileKind,
    contents: Option<FileContents>,
    valid: Option<ConfigLayer>,
    diagnostic: Option<ConfigDiagnostic>,
}

impl WatchedFile {
    fn new(path: PathBuf, kind: FileKind) -> Self {
        Self {
            path,
            kind,
            contents: None,
            valid: None,
            diagnostic: None,
        }
    }

    fn refresh(&mut self) -> bool {
        let contents = read_file(&self.path);
        if self.contents.as_ref() == Some(&contents) {
            return false;
        }
        self.diagnostic = None;
        let result = match &contents {
            FileContents::Missing => Ok(None),
            FileContents::Text(text) => parse_layer(self.kind, text),
            FileContents::Error(error) => Err(error.clone()),
        };
        match result {
            Ok(layer) => self.valid = layer,
            Err(message) => {
                self.diagnostic = Some(ConfigDiagnostic {
                    path: self.path.clone(),
                    message,
                });
            }
        }
        self.contents = Some(contents);
        true
    }
}

/// Pollable configuration loader. A malformed edit retains the last valid
/// version of that file and exposes the error through `UserConfig::diagnostics`.
#[derive(Debug)]
pub struct ConfigWatcher {
    paths: ConfigPaths,
    files: Vec<WatchedFile>,
}

impl ConfigWatcher {
    #[must_use]
    pub fn new(paths: ConfigPaths, cluster: Option<&str>, context: Option<&str>) -> Self {
        let mut watcher = Self {
            paths,
            files: Vec::new(),
        };
        watcher.reset_files(cluster, context);
        watcher
    }

    fn reset_files(&mut self, cluster: Option<&str>, context: Option<&str>) {
        let mut files = vec![
            (
                self.paths.config_dir.join("config.yaml"),
                FileKind::Settings,
            ),
            (
                self.paths.config_dir.join("aliases.yaml"),
                FileKind::Aliases,
            ),
            (self.paths.data_dir.join("hotkeys.yaml"), FileKind::Hotkeys),
            (self.paths.config_dir.join("views.yaml"), FileKind::Views),
            (self.paths.config_dir.join("jumps.yaml"), FileKind::Jumps),
            (
                self.paths.config_dir.join("benchmarks.yaml"),
                FileKind::Benchmarks,
            ),
        ];
        if self.paths.config_dir != self.paths.data_dir {
            files.extend([
                (
                    self.paths.config_dir.join("hotkeys.yaml"),
                    FileKind::Hotkeys,
                ),
                (self.paths.data_dir.join("views.yaml"), FileKind::Views),
                (self.paths.data_dir.join("jumps.yaml"), FileKind::Jumps),
                (
                    self.paths.data_dir.join("benchmarks.yaml"),
                    FileKind::Benchmarks,
                ),
            ]);
        }
        if let (Some(cluster), Some(context)) = (cluster, context) {
            let directory = self.paths.context_dir(cluster, context);
            files.extend([
                (directory.join("config.yaml"), FileKind::Settings),
                (directory.join("aliases.yaml"), FileKind::Aliases),
                (directory.join("hotkeys.yaml"), FileKind::Hotkeys),
                (directory.join("views.yaml"), FileKind::Views),
                (directory.join("jumps.yaml"), FileKind::Jumps),
                (directory.join("benchmarks.yaml"), FileKind::Benchmarks),
            ]);
        }
        let mut previous = std::mem::take(&mut self.files);
        self.files = files
            .into_iter()
            .map(|(path, kind)| {
                previous
                    .iter()
                    .position(|file| file.path == path)
                    .map_or_else(
                        || WatchedFile::new(path, kind),
                        |index| previous.remove(index),
                    )
            })
            .collect();
    }

    #[must_use]
    pub fn set_context(&mut self, cluster: Option<&str>, context: Option<&str>) -> UserConfig {
        self.reset_files(cluster, context);
        for file in &mut self.files {
            file.refresh();
        }
        self.assemble()
    }

    /// Returns a complete effective configuration only when any file changes.
    /// Missing files are normal. File contents are compared to also detect
    /// atomic replacements and changes with unchanged timestamps and lengths.
    #[must_use]
    pub fn reload_if_changed(&mut self) -> Option<UserConfig> {
        let mut changed = false;
        for file in &mut self.files {
            changed |= file.refresh();
        }
        changed.then(|| self.assemble())
    }

    fn assemble(&self) -> UserConfig {
        let mut config = UserConfig::default();
        let mut hotkeys: BTreeMap<String, Hotkey> = BTreeMap::new();
        for file in &self.files {
            if let Some(diagnostic) = &file.diagnostic {
                config.diagnostics.push(diagnostic.clone());
            }
            match &file.valid {
                Some(ConfigLayer::Settings(settings)) => settings.apply(&mut config.settings),
                Some(ConfigLayer::Aliases(aliases)) => config.aliases.extend(aliases.clone()),
                Some(ConfigLayer::Hotkeys(bindings)) => {
                    for hotkey in bindings {
                        // A later layer can override either the named entry or
                        // the shortcut, with exactly one effective action per key.
                        hotkeys.retain(|_, existing| existing.binding != hotkey.binding);
                        hotkeys.insert(hotkey.name.clone(), hotkey.clone());
                    }
                }
                Some(ConfigLayer::Views(views)) => config.views.extend(views.clone()),
                Some(ConfigLayer::Jumps(jumps)) => config.jumps.extend(jumps.clone()),
                Some(ConfigLayer::Benchmarks(bench)) => {
                    config.benchmarks.defaults_concurrency = bench.defaults_concurrency;
                    config.benchmarks.defaults_requests = bench.defaults_requests;
                    config.benchmarks.services.extend(bench.services.clone());
                    config
                        .benchmarks
                        .containers
                        .extend(bench.containers.clone());
                }
                None => {}
            }
        }
        config.hotkeys = hotkeys.into_values().collect();
        config
    }
}

fn read_file(path: &Path) -> FileContents {
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return FileContents::Missing,
        Err(error) => {
            return FileContents::Error(format!("Unable to inspect configuration: {error}"));
        }
    };
    if !metadata.is_file() {
        return FileContents::Error("Configuration must be a regular file".to_owned());
    }
    if metadata.len() > MAX_CONFIG_BYTES {
        return FileContents::Error("Configuration exceeds the 1 MiB size limit".to_owned());
    }
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::NotFound => return FileContents::Missing,
        Err(error) => return FileContents::Error(format!("Unable to read configuration: {error}")),
    };
    let mut contents = String::new();
    if let Err(error) = file
        .take(MAX_CONFIG_BYTES + 1)
        .read_to_string(&mut contents)
    {
        return FileContents::Error(format!("Unable to read UTF-8 configuration: {error}"));
    }
    if contents.len() as u64 > MAX_CONFIG_BYTES {
        return FileContents::Error("Configuration exceeds the 1 MiB size limit".to_owned());
    }
    FileContents::Text(contents)
}

fn parse_layer(kind: FileKind, text: &str) -> Result<Option<ConfigLayer>, String> {
    // Empty documents are useful for intentionally clearing an entire layer.
    let value: serde_yaml::Value = serde_yaml::from_str(text).map_err(|error| error.to_string())?;
    if value.is_null() {
        return Ok(None);
    }
    match kind {
        FileKind::Settings => serde_yaml::from_str::<SettingsDocument>(text)
            .map(|document| Some(ConfigLayer::Settings(Box::new(document.kux))))
            .map_err(|error| error.to_string()),
        FileKind::Aliases => {
            let document: AliasDocument =
                serde_yaml::from_str(text).map_err(|error| error.to_string())?;
            for (name, command) in &document.aliases {
                if name.is_empty() || name.contains(char::is_whitespace) || name.starts_with(':') {
                    return Err(format!(
                        "Invalid alias {name:?}: use one command word without a leading colon"
                    ));
                }
                if command.trim().trim_start_matches(':').trim().is_empty() {
                    return Err(format!("Alias {name:?} has an empty command"));
                }
            }
            Ok(Some(ConfigLayer::Aliases(document.aliases)))
        }
        FileKind::Hotkeys => {
            let document: HotkeyDocument =
                serde_yaml::from_str(text).map_err(|error| error.to_string())?;
            let mut hotkeys: Vec<Hotkey> = Vec::new();
            for (name, raw) in document.hotkeys {
                if raw.command.trim().trim_start_matches(':').trim().is_empty() {
                    return Err(format!("Hotkey {name:?} has an empty command"));
                }
                let binding = KeyShortcut::parse(&raw.shortcut)
                    .map_err(|error| format!("Hotkey {name:?}: {error}"))?;
                if let Some(existing) = hotkeys.iter().find(|item| item.binding == binding) {
                    return Err(format!(
                        "Hotkeys {:?} and {name:?} use the same shortcut {:?}",
                        existing.name, raw.shortcut
                    ));
                }
                hotkeys.push(Hotkey {
                    name,
                    shortcut: raw.shortcut,
                    description: raw.description,
                    command: raw.command,
                    binding,
                });
            }
            Ok(Some(ConfigLayer::Hotkeys(hotkeys)))
        }
        FileKind::Views => parse_views_layer(text),
        FileKind::Jumps => parse_jumps_layer(text),
        FileKind::Benchmarks => parse_benchmarks_layer(text),
    }
}

fn parse_views_layer(text: &str) -> Result<Option<ConfigLayer>, String> {
    let document: ViewsDocument = serde_yaml::from_str(text).map_err(|error| error.to_string())?;
    let mut raw_views = document.views;
    if let Some(root) = document.k9s {
        raw_views.extend(root.views);
    }
    let mut views = BTreeMap::new();
    for (key, raw) in raw_views {
        let key = key.trim();
        if key.is_empty() {
            return Err("View configuration has an empty resource key".to_owned());
        }
        let sort_column = match raw.sort_column.as_deref().map(str::trim) {
            None | Some("") => None,
            Some(spec) => {
                let (col, dir) = spec.split_once(':').ok_or_else(|| {
                    format!(
                        "View {key:?} sortColumn {spec:?} must be formatted as <column>:<asc|desc>"
                    )
                })?;
                let col = col.trim();
                if col.is_empty() {
                    return Err(format!("View {key:?} sortColumn has an empty column name"));
                }
                let asc = match dir.trim().to_ascii_lowercase().as_str() {
                    "asc" => true,
                    "desc" => false,
                    other => {
                        return Err(format!(
                            "View {key:?} sortColumn direction {other:?} must be asc or desc"
                        ));
                    }
                };
                Some((col.to_owned(), asc))
            }
        };
        let mut columns = Vec::with_capacity(raw.columns.len());
        for col_spec in &raw.columns {
            let parsed = CustomColumnSpec::parse(col_spec).ok_or_else(|| {
                format!("View {key:?} has an invalid column specification {col_spec:?}")
            })?;
            columns.push(parsed);
        }
        views.insert(
            key.to_owned(),
            CustomViewSpec {
                sort_column,
                columns,
            },
        );
    }
    Ok(Some(ConfigLayer::Views(views)))
}

fn parse_jumps_layer(text: &str) -> Result<Option<ConfigLayer>, String> {
    let document: JumpsDocument = serde_yaml::from_str(text).map_err(|error| error.to_string())?;
    let mut jumps = BTreeMap::new();
    for (key, raw) in document.jumps {
        let key = key.trim();
        if key.is_empty() {
            return Err("Jump configuration has an empty resource key".to_owned());
        }
        let resource = raw.resource.trim().trim_start_matches(':').trim();
        if resource.is_empty() {
            return Err(format!("Jump {key:?} has an empty target resource"));
        }
        let namespace = raw
            .namespace
            .as_deref()
            .map(str::trim)
            .filter(|ns| !ns.is_empty())
            .map(str::to_owned);
        jumps.insert(
            key.to_owned(),
            CustomJumpSpec {
                resource: resource.to_owned(),
                namespace,
            },
        );
    }
    Ok(Some(ConfigLayer::Jumps(jumps)))
}

fn parse_benchmarks_layer(text: &str) -> Result<Option<ConfigLayer>, String> {
    let document: BenchmarksDocument =
        serde_yaml::from_str(text).map_err(|error| error.to_string())?;
    let raw = document.benchmarks.unwrap_or_default();
    let defaults_concurrency = raw
        .defaults
        .as_ref()
        .and_then(|d| d.concurrency)
        .unwrap_or(1)
        .max(1);
    let defaults_requests = raw
        .defaults
        .as_ref()
        .and_then(|d| d.requests)
        .unwrap_or(200)
        .max(1);
    let to_cfg = |entry: RawBenchEntry| BenchConfig {
        concurrency: entry.concurrency.unwrap_or(defaults_concurrency).max(1),
        requests: entry.requests.unwrap_or(defaults_requests).max(1),
        auth: entry.auth,
        http: BenchHttp {
            method: if entry.http.method.trim().is_empty() {
                default_bench_method()
            } else {
                entry.http.method.trim().to_ascii_uppercase()
            },
            host: entry.http.host,
            path: if entry.http.path.trim().is_empty() {
                default_bench_path()
            } else {
                entry.http.path
            },
            http2: entry.http.http2,
            body: entry.http.body,
            headers: entry.http.headers,
        },
    };
    let services = raw
        .services
        .into_iter()
        .map(|(k, v)| (k, to_cfg(v)))
        .collect();
    let containers = raw
        .containers
        .into_iter()
        .map(|(k, v)| (k, to_cfg(v)))
        .collect();
    Ok(Some(ConfigLayer::Benchmarks(BenchmarksConfig {
        defaults_concurrency,
        defaults_requests,
        services,
        containers,
    })))
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("kux-config-{}-{sequence}", std::process::id()));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn paths(&self) -> ConfigPaths {
            ConfigPaths::new(self.0.join("config"), self.0.join("data"))
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    #[test]
    fn paths_use_kux_overrides_then_xdg_then_home() {
        let mut environment = BTreeMap::from([("HOME", PathBuf::from("/home/user"))]);
        let paths =
            ConfigPaths::from_lookup(|key| environment.get(key).cloned(), |_| false).unwrap();
        assert_eq!(paths.config_dir, PathBuf::from("/home/user/.config/kux"));
        assert_eq!(paths.data_dir, PathBuf::from("/home/user/.local/share/kux"));
        let k9s_fallback = ConfigPaths::from_lookup(
            |key| environment.get(key).cloned(),
            |path| path.ends_with("k9s"),
        )
        .unwrap();
        assert_eq!(
            k9s_fallback.config_dir,
            PathBuf::from("/home/user/.config/k9s")
        );
        assert_eq!(
            k9s_fallback.data_dir,
            PathBuf::from("/home/user/.local/share/k9s")
        );
        environment.insert("XDG_CONFIG_HOME", PathBuf::from("/xdg/config"));
        environment.insert("XDG_DATA_HOME", PathBuf::from("/xdg/data"));
        let paths =
            ConfigPaths::from_lookup(|key| environment.get(key).cloned(), |_| false).unwrap();
        assert_eq!(paths.config_dir, PathBuf::from("/xdg/config/kux"));
        assert_eq!(paths.data_dir, PathBuf::from("/xdg/data/kux"));
        environment.insert("KUX_CONFIG_DIR", PathBuf::from("/custom/config"));
        environment.insert("KUX_DATA_DIR", PathBuf::from("/custom/data"));
        let paths =
            ConfigPaths::from_lookup(|key| environment.get(key).cloned(), |_| false).unwrap();
        assert_eq!(paths.config_dir, PathBuf::from("/custom/config"));
        assert_eq!(paths.data_dir, PathBuf::from("/custom/data"));
        environment.insert("KUX_CONFIG_DIR", PathBuf::from("relative"));
        assert!(
            ConfigPaths::from_lookup(|key| environment.get(key).cloned(), |_| false)
                .unwrap_err()
                .contains("absolute")
        );
        assert!(ConfigPaths::from_lookup(|_| None, |_| false).is_err());
    }

    #[test]
    fn context_paths_encode_identifiers_without_traversal_or_collisions() {
        let paths = ConfigPaths::new("/config", "/data");
        assert_eq!(
            paths.context_dir("cluster-a", "prod"),
            PathBuf::from("/data/clusters/cluster-a/prod")
        );
        assert_eq!(
            paths.context_dir("..", "/../../x"),
            PathBuf::from("/data/clusters/%2E%2E/%2F..%2F..%2Fx")
        );
        assert_ne!(
            paths.context_dir("c", "a/b"),
            paths.context_dir("c", "a%2Fb")
        );
        assert_eq!(
            paths.context_dir("arn:aws:cluster", "ctx"),
            PathBuf::from("/data/clusters/arn%3Aaws%3Acluster/ctx")
        );
    }

    #[test]
    fn missing_files_are_valid_and_never_created() {
        let directory = TestDirectory::new();
        let paths = directory.paths();
        assert_eq!(
            UserConfig::load(&paths, Some("cluster"), Some("ctx")),
            UserConfig::default()
        );
        assert!(!paths.config_dir.exists());
        assert!(!paths.data_dir.exists());
    }

    #[test]
    fn loads_documented_alias_and_hotkey_formats_and_context_overrides() {
        let directory = TestDirectory::new();
        let paths = directory.paths();
        write(
            &paths.config_dir.join("aliases.yaml"),
            "aliases:\n  pp: v1/pods\n  dep: apps/v1/deployments\n  pos: pod kube-system app=fred,blee=duh\n",
        );
        write(
            &paths.data_dir.join("hotkeys.yaml"),
            "hotKeys:\n  shift-0:\n    shortCut: Shift-0\n    description: Viewing pods\n    command: pods app=kindnet\n  shift-1:\n    shortCut: Shift-1\n    command: dp\n",
        );
        let context_dir = paths.context_dir("cluster", "ctx");
        write(
            &context_dir.join("aliases.yaml"),
            "aliases:\n  pp: pods production\n",
        );
        write(
            &context_dir.join("hotkeys.yaml"),
            "hotKeys:\n  production:\n    shortCut: Shift-0\n    description: Production pods\n    command: pp\n",
        );
        let config = UserConfig::load(&paths, Some("cluster"), Some("ctx"));
        assert_eq!(config.diagnostics, Vec::<ConfigDiagnostic>::new());
        assert_eq!(
            config.expand_command(":pp app=api").unwrap(),
            "pods production app=api"
        );
        assert_eq!(config.expand_command("dep").unwrap(), "apps/v1/deployments");
        assert_eq!(config.hotkeys.len(), 2);
        let hotkey = config
            .hotkey_for(KeyEvent::new(KeyCode::Char(')'), KeyModifiers::NONE))
            .unwrap();
        assert_eq!(hotkey.command, "pp");
        assert_eq!(hotkey.description, "Production pods");
    }

    #[test]
    fn aliases_expand_chains_preserve_arguments_and_detect_cycles() {
        let config = UserConfig {
            aliases: BTreeMap::from([
                ("p".to_owned(), "prod app=api".to_owned()),
                ("prod".to_owned(), "pods production".to_owned()),
                ("cycle".to_owned(), "loop".to_owned()),
                ("loop".to_owned(), "cycle --extra".to_owned()),
            ]),
            ..UserConfig::default()
        };
        assert_eq!(
            config.expand_command("p  -l 'a in (x,y)' --flag").unwrap(),
            "pods production app=api  -l 'a in (x,y)' --flag"
        );
        assert_eq!(config.expand_command("unknown arg").unwrap(), "unknown arg");
        assert!(
            config
                .expand_command("cycle test")
                .unwrap_err()
                .contains("cycle")
        );
        assert_eq!(
            config.expand_command("  :pods default  ").unwrap(),
            "pods default"
        );
    }

    #[test]
    fn shortcuts_match_modifiers_shifted_terminal_characters_and_special_keys() {
        for (label, code, modifiers) in [
            ("Ctrl-P", KeyCode::Char('p'), KeyModifiers::CONTROL),
            ("Ctrl-Shift-P", KeyCode::Char('P'), KeyModifiers::CONTROL),
            ("Alt-p", KeyCode::Char('p'), KeyModifiers::ALT),
            ("Shift-0", KeyCode::Char(')'), KeyModifiers::NONE),
            ("Shift-1", KeyCode::Char('!'), KeyModifiers::SHIFT),
            ("Shift-P", KeyCode::Char('P'), KeyModifiers::NONE),
            ("P", KeyCode::Char('P'), KeyModifiers::SHIFT),
            ("Shift-Tab", KeyCode::BackTab, KeyModifiers::NONE),
            ("Ctrl-Space", KeyCode::Char(' '), KeyModifiers::CONTROL),
            ("Ctrl-\\", KeyCode::Char('\\'), KeyModifiers::CONTROL),
            ("F12", KeyCode::F(12), KeyModifiers::NONE),
            ("Esc", KeyCode::Esc, KeyModifiers::NONE),
        ] {
            assert_eq!(
                KeyShortcut::parse(label).unwrap(),
                KeyShortcut::normalized(code, modifiers),
                "{label}"
            );
        }
        assert_ne!(
            KeyShortcut::parse("p").unwrap(),
            KeyShortcut::parse("P").unwrap()
        );
        assert_ne!(
            KeyShortcut::parse("Ctrl-P").unwrap(),
            KeyShortcut::parse("Ctrl-Shift-P").unwrap()
        );
        for invalid in ["", "Ctrl-", "F25", "Hyper-unsupported", "no-such-key"] {
            assert!(KeyShortcut::parse(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn settings_merge_only_explicit_context_fields() {
        let directory = TestDirectory::new();
        let paths = directory.paths();
        write(
            &paths.config_dir.join("config.yaml"),
            "kux:\n  readOnly: true\n  noExitOnCtrlC: true\n  defaultView: deployments\n  ui:\n    headless: true\n    logoless: true\n    crumbsless: true\n    noIcons: true\n",
        );
        write(
            &paths.context_dir("c", "x").join("config.yaml"),
            "k9s:\n  readOnly: false\n  ui:\n    logoless: false\n",
        );
        let config = UserConfig::load(&paths, Some("c"), Some("x"));
        assert_eq!(config.diagnostics, Vec::<ConfigDiagnostic>::new());
        assert!(!config.settings.read_only);
        assert!(config.settings.no_exit_on_ctrl_c);
        assert_eq!(config.settings.default_view.as_deref(), Some("deployments"));
        assert!(!config.settings.ui.logoless);
        assert!(config.settings.ui.headless);
        assert!(config.settings.ui.crumbsless);
        assert!(config.settings.ui.no_icons);
    }

    #[test]
    fn live_view_auto_refresh_defaults_false_and_merges_explicit_context_values() {
        assert!(!Settings::default().live_view_auto_refresh);
        let directory = TestDirectory::new();
        let paths = directory.paths();
        write(
            &paths.config_dir.join("config.yaml"),
            "k9s:\n  liveViewAutoRefresh: true\n",
        );
        write(
            &paths.context_dir("c", "paused").join("config.yaml"),
            "kux:\n  liveViewAutoRefresh: false\n",
        );
        let global = UserConfig::load(&paths, None, None);
        assert!(global.settings.live_view_auto_refresh);
        let paused = UserConfig::load(&paths, Some("c"), Some("paused"));
        assert!(!paused.settings.live_view_auto_refresh);
        let inherited = UserConfig::load(&paths, Some("c"), Some("other"));
        assert!(inherited.settings.live_view_auto_refresh);
        assert_eq!(paused.diagnostics.len(), 0);
        for invalid in ["always", "1", "[]", "{}"] {
            assert!(
                parse_layer(
                    FileKind::Settings,
                    &format!("kux:\n  liveViewAutoRefresh: {invalid}\n"),
                )
                .is_err()
            );
        }
    }

    #[test]
    fn live_view_auto_refresh_reload_retains_invalid_layer_and_clears_removed_override() {
        let directory = TestDirectory::new();
        let paths = directory.paths();
        let global = paths.config_dir.join("config.yaml");
        let contextual = paths.context_dir("c", "view").join("config.yaml");
        write(&global, "kux:\n  liveViewAutoRefresh: true\n");
        write(&contextual, "kux:\n  liveViewAutoRefresh: false\n");
        let mut watcher = ConfigWatcher::new(paths, Some("c"), Some("view"));
        assert!(
            !watcher
                .reload_if_changed()
                .unwrap()
                .settings
                .live_view_auto_refresh
        );
        write(&contextual, "kux:\n  liveViewAutoRefresh: invalid\n");
        let invalid = watcher.reload_if_changed().unwrap();
        assert!(!invalid.settings.live_view_auto_refresh);
        assert_eq!(invalid.diagnostics.len(), 1);
        assert_eq!(invalid.diagnostics[0].path, contextual);
        let other = watcher.set_context(Some("c"), Some("other"));
        assert!(other.settings.live_view_auto_refresh);
        assert_eq!(other.diagnostics.len(), 0);
        write(&contextual, "kux:\n  liveViewAutoRefresh: true\n");
        let recovered = watcher.set_context(Some("c"), Some("view"));
        assert!(recovered.settings.live_view_auto_refresh);
        assert_eq!(recovered.diagnostics.len(), 0);
        write(&global, "kux:\n  liveViewAutoRefresh: false\n");
        assert!(
            watcher
                .reload_if_changed()
                .unwrap()
                .settings
                .live_view_auto_refresh
        );
        std::fs::remove_file(&contextual).unwrap();
        assert!(
            !watcher
                .reload_if_changed()
                .unwrap()
                .settings
                .live_view_auto_refresh
        );
    }

    #[test]
    fn logger_and_ui_defaults_merge_explicit_context_settings() {
        let directory = TestDirectory::new();
        let paths = directory.paths();
        write(
            &paths.config_dir.join("config.yaml"),
            "k9s:\n  logger:\n    tail: 350\n    buffer: 700\n    sinceSeconds: 90\n    textWrap: true\n    disableAutoscroll: true\n    columnLock: true\n    showTime: true\n  ui:\n    splashless: true\n    invert: true\n    defaultsToFullScreen: true\n",
        );
        write(
            &paths.context_dir("c", "x").join("config.yaml"),
            "kux:\n  logger:\n    tail: 12\n    disableAutoscroll: false\n    showTime: false\n  ui:\n    invert: false\n",
        );
        let config = UserConfig::load(&paths, Some("c"), Some("x"));
        assert_eq!(config.diagnostics.len(), 0);
        assert_eq!(
            config.settings.logger,
            LoggerSettings {
                tail: 12,
                buffer: 700,
                since_seconds: 90,
                text_wrap: true,
                disable_autoscroll: false,
                column_lock: true,
                show_time: false,
            }
        );
        assert!(config.settings.ui.splashless);
        assert!(config.settings.ui.fullscreen);
        assert!(!config.settings.ui.invert);
    }

    #[test]
    fn logger_numeric_validation_matches_pinned_defaults_and_bounds() {
        let defaults = LoggerSettings::default();
        assert_eq!(
            (defaults.tail, defaults.buffer, defaults.since_seconds),
            (100, 5_000, -1)
        );
        assert!(
            !(defaults.text_wrap
                || defaults.disable_autoscroll
                || defaults.column_lock
                || defaults.show_time)
        );
        for (tail, buffer, since, expected_tail, expected_buffer, expected_since) in [
            (-1, -10, 0, 100, 5_000, -1),
            (0, 0, -2, 100, 5_000, -2),
            (9_000, 9_000, 25, 5_000, 5_000, 25),
            (1, 1, -1, 1, 1, -1),
        ] {
            let Some(ConfigLayer::Settings(patch)) = parse_layer(
                FileKind::Settings,
                &format!("kux:\n  logger:\n    tail: {tail}\n    buffer: {buffer}\n    sinceSeconds: {since}\n"),
            ).unwrap() else {
                panic!("logger settings layer");
            };
            let mut settings = Settings::default();
            patch.apply(&mut settings);
            assert_eq!(settings.logger.tail, expected_tail);
            assert_eq!(settings.logger.buffer, expected_buffer);
            assert_eq!(settings.logger.since_seconds, expected_since);
        }
    }

    #[test]
    fn malformed_logger_edit_preserves_last_valid_settings_atomically() {
        let directory = TestDirectory::new();
        let paths = directory.paths();
        let path = paths.config_dir.join("config.yaml");
        write(&path, "kux:\n  readOnly: true\n  logger:\n    buffer: 25\n");
        let mut watcher = ConfigWatcher::new(paths, None, None);
        let valid = watcher.reload_if_changed().unwrap();
        assert_eq!(valid.settings.logger.buffer, 25);
        write(
            &path,
            "kux:\n  readOnly: false\n  logger:\n    buffer: invalid\n",
        );
        let invalid = watcher.reload_if_changed().unwrap();
        assert_eq!(invalid.settings, valid.settings);
        assert_eq!(invalid.diagnostics.len(), 1);
        assert_eq!(invalid.diagnostics[0].path, path);
    }

    #[test]
    fn refresh_rate_supports_fractional_seconds_minimum_and_context_overrides() {
        let directory = TestDirectory::new();
        let paths = directory.paths();
        write(
            &paths.config_dir.join("config.yaml"),
            "kux:\n  refreshRate: 2.5\n",
        );
        let global = UserConfig::load(&paths, None, None);
        assert_eq!(
            global.settings.refresh_rate,
            Some(std::time::Duration::from_millis(2_500))
        );
        write(
            &paths.context_dir("c", "x").join("config.yaml"),
            "kux:\n  refreshRate: 0.5\n",
        );
        let scoped = UserConfig::load(&paths, Some("c"), Some("x"));
        assert_eq!(
            scoped.settings.refresh_rate,
            Some(std::time::Duration::from_secs(2))
        );
        assert_eq!(scoped.diagnostics.len(), 0);
        for invalid in ["0", "-1", ".nan", ".inf", "1e30", "invalid"] {
            assert!(
                parse_layer(
                    FileKind::Settings,
                    &format!("kux:\n  refreshRate: {invalid}\n")
                )
                .is_err(),
                "{invalid} must not create an invalid timer"
            );
        }
    }

    #[test]
    fn watcher_reloads_creation_deletion_invalid_edits_and_context_changes() {
        let directory = TestDirectory::new();
        let paths = directory.paths();
        let mut watcher = ConfigWatcher::new(paths.clone(), Some("c"), Some("x"));
        assert!(watcher.reload_if_changed().is_some());
        assert!(watcher.reload_if_changed().is_none());
        let global = paths.config_dir.join("config.yaml");
        let contextual = paths.context_dir("c", "x").join("aliases.yaml");
        write(&global, "kux:\n  readOnly: true\n");
        write(&contextual, "aliases:\n  pp: pods production\n");
        let config = watcher.reload_if_changed().unwrap();
        assert!(config.settings.read_only);
        assert_eq!(config.expand_command("pp").unwrap(), "pods production");
        write(&global, "kux:\n  readOnly: [invalid\n");
        write(&contextual, "aliases:\n  pp: pods staging\n");
        let config = watcher.reload_if_changed().unwrap();
        assert!(
            config.settings.read_only,
            "Invalid live edits retain the prior read-only setting"
        );
        assert_eq!(config.expand_command("pp").unwrap(), "pods staging");
        assert_eq!(config.diagnostics.len(), 1);
        assert_eq!(config.diagnostics[0].path, global);
        assert!(config.diagnostics[0].message.contains("line"));
        assert!(watcher.reload_if_changed().is_none());
        let other = watcher.set_context(Some("c"), Some("other"));
        assert!(other.aliases.is_empty());
        assert!(
            other.settings.read_only,
            "Context changes retain the last valid global file"
        );
        assert_eq!(other.diagnostics.len(), 1);
        write(&global, "kux:\n  readOnly: false\n");
        let repaired = watcher.reload_if_changed().unwrap();
        assert!(!repaired.settings.read_only);
        assert_eq!(repaired.diagnostics, Vec::<ConfigDiagnostic>::new());
        std::fs::remove_file(&global).unwrap();
        assert_eq!(watcher.reload_if_changed().unwrap(), UserConfig::default());
    }

    #[test]
    fn malformed_documents_report_the_file_and_reject_partial_layers() {
        let directory = TestDirectory::new();
        let paths = directory.paths();
        write(
            &paths.config_dir.join("config.yaml"),
            "kux:\n  readOnly: definitely\n",
        );
        write(
            &paths.config_dir.join("aliases.yaml"),
            "aliases:\n  good: pods\n  bad: []\n",
        );
        write(
            &paths.config_dir.join("hotkeys.yaml"),
            "hotKeys:\n  bad:\n    shortCut: Ctrl-Unknown\n    command: pods\n",
        );
        let config = UserConfig::load(&paths, None, None);
        assert_eq!(config.diagnostics.len(), 3);
        assert!(config.aliases.is_empty());
        assert_eq!(config.hotkeys, Vec::<Hotkey>::new());
        assert!(
            config
                .diagnostics
                .iter()
                .all(|diagnostic| !diagnostic.to_string().is_empty())
        );
        for text in [
            "aliases:\n  'two words': pods",
            "aliases:\n  ':pp': pods",
            "aliases:\n  pp: ''",
            "aliases:\n  pp: pods\n  pp: nodes",
        ] {
            assert!(parse_layer(FileKind::Aliases, text).is_err());
        }
    }

    #[test]
    fn duplicate_shortcuts_are_rejected_and_release_events_do_not_execute() {
        let duplicate = "hotKeys:\n  one:\n    shortCut: Shift-1\n    command: pods\n  two:\n    shortCut: '!'\n    command: nodes\n";
        assert!(
            parse_layer(FileKind::Hotkeys, duplicate)
                .unwrap_err()
                .contains("same shortcut")
        );
        let Some(ConfigLayer::Hotkeys(hotkeys)) = parse_layer(
            FileKind::Hotkeys,
            "hotKeys:\n  one:\n    shortCut: F1\n    command: pods\n",
        )
        .unwrap() else {
            panic!("expected hotkeys");
        };
        let config = UserConfig {
            hotkeys,
            ..UserConfig::default()
        };
        let release =
            KeyEvent::new_with_kind(KeyCode::F(1), KeyModifiers::NONE, KeyEventKind::Release);
        assert!(config.hotkey_for(release).is_none());
        assert!(
            config
                .hotkey_for(KeyEvent::new(KeyCode::F(1), KeyModifiers::NONE))
                .is_some()
        );
    }

    #[test]
    fn oversized_and_non_utf8_files_return_diagnostics() {
        let directory = TestDirectory::new();
        let paths = directory.paths();
        let file = paths.config_dir.join("aliases.yaml");
        write(
            &file,
            &" ".repeat(usize::try_from(MAX_CONFIG_BYTES + 1).unwrap()),
        );
        assert!(
            UserConfig::load(&paths, None, None).diagnostics[0]
                .message
                .contains("size limit")
        );
        std::fs::write(&file, [255_u8]).unwrap();
        assert!(
            UserConfig::load(&paths, None, None).diagnostics[0]
                .message
                .contains("UTF-8")
        );
    }

    #[test]
    fn nonregular_configuration_is_rejected_before_opening() {
        let directory = TestDirectory::new();
        assert!(
            matches!(read_file(&directory.0), FileContents::Error(message) if message.contains("regular file"))
        );

        // A socket is nonregular like a FIFO, but can be constructed without
        // spawning a platform command or risking a blocking open in the test.
        #[cfg(unix)]
        {
            let socket = directory.0.join("socket.yaml");
            let _listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
            assert!(
                matches!(read_file(&socket), FileContents::Error(message) if message.contains("regular file"))
            );
        }
    }

    #[test]
    fn oversized_sparse_regular_file_is_rejected_from_metadata() {
        let directory = TestDirectory::new();
        let path = directory.0.join("aliases.yaml");
        File::create(&path)
            .unwrap()
            .set_len(MAX_CONFIG_BYTES * 100)
            .unwrap();
        assert!(
            matches!(read_file(&path), FileContents::Error(message) if message.contains("size limit"))
        );
    }

    #[test]
    fn loads_views_jumps_and_node_shell_settings_with_context_overrides() {
        let directory = TestDirectory::new();
        let paths = directory.paths();
        write(
            &paths.config_dir.join("config.yaml"),
            "k9s:\n  featureGates:\n    nodeShell: true\n  shellPod:\n    image: alpine:3.20\n    namespace: kube-system\n    limits:\n      cpu: 250m\n      memory: 256Mi\n    command: [/bin/sh]\n    args: [-l]\n    hostPathVolume:\n      - name: var-log\n        mountPath: /host/var/log\n        hostPath: /var/log\n        readOnly: true\n  clusters:\n    locked-ctx:\n      featureGates:\n        nodeShell: false\n",
        );
        write(
            &paths.config_dir.join("views.yaml"),
            "k9s:\n  views:\n    v1/pods:\n      sortColumn: AGE:desc\n      columns:\n        - NAME\n        - IP:.status.podIP|W\n        - HIDDEN:.metadata.uid|H\n        - AGE\n    v1/pods@prod:\n      sortColumn: NAME:asc\n      columns:\n        - NAME\n        - IMAGE:.spec.containers[*].image\n",
        );
        write(
            &paths.config_dir.join("jumps.yaml"),
            "jumps:\n  v1/services:\n    resource: pods\n    namespace: default\n",
        );
        let config = UserConfig::load(&paths, Some("cluster"), Some("ctx"));
        assert_eq!(config.diagnostics, Vec::<ConfigDiagnostic>::new());
        assert!(config.node_shell_enabled("ctx"));
        assert!(!config.node_shell_enabled("locked-ctx"));
        assert_eq!(config.shell_pod_for("ctx").image, "alpine:3.20");
        assert_eq!(config.shell_pod_for("ctx").namespace, "kube-system");
        assert_eq!(config.shell_pod_for("ctx").command, vec!["/bin/sh"]);
        assert_eq!(config.shell_pod_for("ctx").args, vec!["-l"]);
        assert_eq!(config.shell_pod_for("ctx").host_path_volume.len(), 1);

        let pods = crate::model::ResourceDescriptor::from(crate::model::ResourceKind::Pods);
        let default_pod_view = config.custom_view_for(&pods, Some("staging")).unwrap();
        assert_eq!(
            default_pod_view.sort_column,
            Some(("AGE".to_owned(), false))
        );
        assert_eq!(default_pod_view.columns.len(), 4);
        assert!(default_pod_view.columns[1].is_wide_only());
        assert!(default_pod_view.columns[2].is_hidden());

        let prod_pod_view = config.custom_view_for(&pods, Some("prod")).unwrap();
        assert_eq!(prod_pod_view.sort_column, Some(("NAME".to_owned(), true)));
        assert_eq!(prod_pod_view.columns.len(), 2);

        let services = crate::model::ResourceDescriptor::from(crate::model::ResourceKind::Services);
        let jump = config.custom_jump_for(&services).unwrap();
        assert_eq!(jump.resource, "pods");
        assert_eq!(jump.namespace.as_deref(), Some("default"));
    }

    #[test]
    fn loads_image_scans_and_checks_namespace_and_label_exclusions() {
        let directory = TestDirectory::new();
        let paths = directory.paths();
        write(
            &paths.config_dir.join("config.yaml"),
            "k9s:\n  imageScans:\n    enable: true\n    exclusions:\n      namespaces:\n        - kube-system\n      labels:\n        app.kubernetes.io/part-of:\n          - istio\n",
        );
        let config = UserConfig::load(&paths, Some("cluster"), Some("ctx"));
        assert!(config.settings.image_scans.enable);
        assert!(
            config
                .settings
                .image_scans
                .should_exclude("kube-system", "app=coredns")
        );
        assert!(
            config
                .settings
                .image_scans
                .should_exclude("default", "app.kubernetes.io/part-of=istio,env=prod")
        );
        assert!(
            !config
                .settings
                .image_scans
                .should_exclude("default", "app=nginx,env=prod")
        );
    }

    #[test]
    fn loads_thresholds_screen_dump_dir_and_port_forward_address() {
        let directory = TestDirectory::new();
        let paths = directory.paths();
        write(
            &paths.config_dir.join("config.yaml"),
            "k9s:\n  screenDumpDir: /tmp/custom-dumps\n  portForwardAddress: 0.0.0.0\n  maxConnRetry: 9\n  disablePodCounting: true\n  thresholds:\n    cpu:\n      critical: 85\n      warn: 65\n    memory:\n      critical: 95\n      warn: 75\n",
        );
        let config = UserConfig::load(&paths, Some("cluster"), Some("ctx"));
        assert_eq!(
            config.settings.screen_dump_dir,
            Some(PathBuf::from("/tmp/custom-dumps"))
        );
        assert_eq!(
            config.settings.port_forward_address.as_deref(),
            Some("0.0.0.0")
        );
        assert_eq!(config.settings.max_conn_retry, Some(9));
        assert!(config.settings.disable_pod_counting);
        assert_eq!(config.settings.thresholds.cpu.critical, 85);
        assert_eq!(config.settings.thresholds.cpu.warn, 65);
        assert_eq!(config.settings.thresholds.memory.critical, 95);
        assert_eq!(config.settings.thresholds.memory.warn, 75);
    }
}
