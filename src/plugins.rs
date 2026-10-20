//! K9s-compatible plugin definitions, scoped loading, inputs, and argv expansion.
//!
//! No command runs while loading or expanding a plugin. The caller controls
//! confirmation and read-only policy before handing an invocation to a runner.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::OpenOptions;
use std::io::{ErrorKind, Read};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use crossterm::event::{KeyEvent, KeyEventKind};
use regex::Regex;
use serde::Deserialize;

use crate::config::{ConfigDiagnostic, ConfigPaths, KeyShortcut};

const MAX_FILE_BYTES: u64 = 1_048_576;
const MAX_DIRECTORY_FILES: usize = 256;
const MAX_DIRECTORY_DEPTH: usize = 32;

pub type PluginEnvironment = BTreeMap<String, String>;
pub type PluginInputValues = BTreeMap<String, String>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InputType {
    String,
    Number,
    Bool,
    Dropdown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginInput {
    pub name: String,
    pub label: String,
    pub kind: InputType,
    pub required: bool,
    pub default: Option<String>,
    pub options: Vec<String>,
}

impl PluginInput {
    /// Validate one form value without adding shell quotes or interpreting it.
    ///
    /// # Errors
    /// Returns an error for a missing required value or an invalid typed value.
    pub fn validate(&self, value: &str) -> Result<(), String> {
        if value.is_empty() {
            return if self.required && self.kind != InputType::Bool {
                Err(format!("Input {:?} is required", self.name))
            } else {
                Ok(())
            };
        }
        let valid = match self.kind {
            InputType::String => !value.contains('\0'),
            InputType::Number => value.parse::<f64>().is_ok_and(f64::is_finite),
            InputType::Bool => matches!(value, "true" | "false"),
            InputType::Dropdown => self.options.iter().any(|option| option == value),
        };
        if valid {
            Ok(())
        } else {
            Err(format!(
                "Input {:?} has an invalid {:?} value",
                self.name, self.kind
            ))
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)]
pub struct Plugin {
    pub name: String,
    pub shortcut: String,
    pub description: String,
    pub scopes: Vec<String>,
    pub command: String,
    pub args: Vec<String>,
    pub pipes: Vec<String>,
    pub override_builtin: bool,
    pub confirm: Option<bool>,
    pub background: bool,
    pub dangerous: bool,
    pub overwrite_output: bool,
    pub inputs: Vec<PluginInput>,
    binding: KeyShortcut,
}

impl Plugin {
    #[must_use]
    pub fn key_event(&self) -> KeyEvent {
        self.binding.event()
    }

    #[must_use]
    pub fn matches_key(&self, key: KeyEvent) -> bool {
        key.kind != KeyEventKind::Release
            && self.binding == KeyShortcut::normalized(key.code, key.modifiers)
    }

    /// The caller supplies resource aliases, including its full GVR and any
    /// view-specific identities such as `containers` or `helm/history`.
    #[must_use]
    pub fn in_scope(&self, aliases: &[String]) -> bool {
        self.scopes
            .iter()
            .any(|scope| scope == "all" || aliases.iter().any(|alias| alias == scope))
    }

    #[must_use]
    pub fn should_confirm(&self) -> bool {
        self.confirm.unwrap_or(!self.inputs.is_empty())
    }

    #[must_use]
    pub fn default_inputs(&self) -> PluginInputValues {
        self.inputs
            .iter()
            .map(|input| {
                let value = input.default.clone().unwrap_or_else(|| {
                    if input.kind == InputType::Bool {
                        "false".to_owned()
                    } else {
                        String::new()
                    }
                });
                (input.name.clone(), value)
            })
            .collect()
    }

    /// Validate the whole form and omit empty optional text/number/dropdown
    /// values, matching K9s input substitution behavior.
    ///
    /// # Errors
    /// Returns an error for unknown fields or invalid/missing typed values.
    pub fn validate_inputs(&self, values: &PluginInputValues) -> Result<PluginInputValues, String> {
        if let Some(name) = values
            .keys()
            .find(|name| !self.inputs.iter().any(|input| &input.name == *name))
        {
            return Err(format!("Unknown plugin input {name:?}"));
        }
        let mut resolved = self.default_inputs();
        resolved.extend(values.clone());
        for input in &self.inputs {
            let value = resolved.get(&input.name).map_or("", String::as_str);
            let value = if input.kind == InputType::Bool && value.is_empty() {
                "false"
            } else {
                value
            };
            input.validate(value)?;
            if value.is_empty() && !input.required {
                resolved.remove(&input.name);
            } else {
                resolved.insert(input.name.clone(), value.to_owned());
            }
        }
        Ok(resolved)
    }

    /// Construct direct program/argv/environment values. Whitespace or shell
    /// metacharacters in substituted values never add arguments or commands.
    /// Shell evaluation only occurs if a plugin explicitly selects a shell.
    ///
    /// # Errors
    /// Returns an error for invalid inputs, malformed pipe arguments, or NUL
    /// bytes unsupported by process APIs.
    pub fn invocation(
        &self,
        environment: &PluginEnvironment,
        values: &PluginInputValues,
    ) -> Result<PluginInvocation, String> {
        let mut env = environment.clone();
        for (name, value) in self.validate_inputs(values)? {
            env.insert(format!("INPUT_{}", name.to_ascii_uppercase()), value);
        }
        let args = self
            .args
            .iter()
            .map(|arg| expand_variables(arg, &env))
            .collect();
        let pipes = self
            .pipes
            .iter()
            .map(|pipe| {
                let mut tokens = split_pipe(pipe)?.into_iter();
                let program = tokens
                    .next()
                    .ok_or_else(|| "Plugin pipe command is empty".to_owned())?;
                Ok(PluginStage {
                    program,
                    args: tokens.map(|arg| expand_variables(&arg, &env)).collect(),
                })
            })
            .collect::<Result<Vec<_>, String>>()?;
        let invocation = PluginInvocation {
            name: self.name.clone(),
            description: self.description.clone(),
            dangerous: self.dangerous,
            program: self.command.clone(),
            args,
            env,
            pipes,
            background: self.background,
            overwrite_output: self.overwrite_output,
        };
        invocation.validate()?;
        Ok(invocation)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginStage {
    pub program: String,
    pub args: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginInvocation {
    pub name: String,
    pub description: String,
    pub dangerous: bool,
    pub program: String,
    pub args: Vec<String>,
    pub env: PluginEnvironment,
    pub pipes: Vec<PluginStage>,
    pub background: bool,
    pub overwrite_output: bool,
}

impl PluginInvocation {
    /// Display argv boundaries for confirmation; this text is never executed.
    #[must_use]
    pub fn preview(&self) -> String {
        std::iter::once((&self.program, &self.args))
            .chain(self.pipes.iter().map(|pipe| (&pipe.program, &pipe.args)))
            .map(|(program, args)| {
                std::iter::once(program)
                    .chain(args.iter())
                    .map(|argument| format!("{argument:?}"))
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .collect::<Vec<_>>()
            .join(" | ")
    }

    /// # Errors
    /// Returns an error for an empty program or unsupported process bytes.
    pub fn validate(&self) -> Result<(), String> {
        if self.pipes.len() > 15 {
            return Err("Plugin pipelines support at most sixteen stages".to_owned());
        }
        for (program, args) in std::iter::once((&self.program, &self.args))
            .chain(self.pipes.iter().map(|pipe| (&pipe.program, &pipe.args)))
        {
            if program.is_empty()
                || program.contains('\0')
                || args.iter().any(|arg| arg.contains('\0'))
            {
                return Err(
                    "Plugin program/arguments contain an empty program or a NUL byte".to_owned(),
                );
            }
        }
        if self.env.iter().any(|(name, value)| {
            name.is_empty() || name.contains(['=', '\0']) || value.contains('\0')
        }) {
            return Err(
                "Plugin environment contains an invalid variable name or a NUL byte".to_owned(),
            );
        }
        Ok(())
    }
}

/// Substitute K9s `$NAME`, `${COL-%CPU/L}`, and boolean `$!COL-SUSPEND`
/// expressions in a single pass. Unknown variables remain literal. Replacing a
/// value never recursively interprets `$` expressions supplied by that value.
#[must_use]
pub fn expand_variables(argument: &str, environment: &PluginEnvironment) -> String {
    static VARIABLES: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"\$(!?)([\w\-]+)|\$\{(!?)([\w\-%/: ]+)\}").expect("valid plugin variable regex")
    });
    VARIABLES
        .replace_all(argument, |capture: &regex::Captures<'_>| {
            let (name, inverse) = if let Some(name) = capture.get(2) {
                (
                    name.as_str(),
                    capture.get(1).is_some_and(|value| value.as_str() == "!"),
                )
            } else {
                (
                    capture.get(4).map_or("", |value| value.as_str()),
                    capture.get(3).is_some_and(|value| value.as_str() == "!"),
                )
            };
            let Some(value) = environment.get(&name.to_ascii_uppercase()) else {
                return capture[0].to_owned();
            };
            match value.as_str() {
                "true" | "True" | "TRUE" | "t" | "T" => (!inverse).to_string(),
                "false" | "False" | "FALSE" | "f" | "F" => inverse.to_string(),
                "1" if inverse => "false".to_owned(),
                "0" if inverse => "true".to_owned(),
                _ => value.clone(),
            }
        })
        .into_owned()
}

fn split_pipe(command: &str) -> Result<Vec<String>, String> {
    let mut arguments = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut escaped = false;
    let mut present = false;
    for character in command.chars() {
        if escaped {
            current.push(character);
            escaped = false;
            present = true;
        } else if character == '\\' && quote != Some('\'') {
            escaped = true;
            present = true;
        } else if quote == Some(character) {
            quote = None;
        } else if quote.is_some() {
            current.push(character);
        } else if matches!(character, '\'' | '"') {
            quote = Some(character);
            present = true;
        } else if character.is_whitespace() {
            if present {
                arguments.push(std::mem::take(&mut current));
                present = false;
            }
        } else {
            current.push(character);
            present = true;
        }
    }
    if quote.is_some() || escaped {
        return Err("Plugin pipe has an unclosed quote or trailing escape".to_owned());
    }
    if present {
        arguments.push(current);
    }
    Ok(arguments)
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PluginCatalog {
    pub plugins: BTreeMap<String, Plugin>,
    pub diagnostics: Vec<ConfigDiagnostic>,
}

impl PluginCatalog {
    /// Resolve collisions deterministically by plugin name. A later matching
    /// plugin replaces an earlier binding only when it opts into override.
    #[must_use]
    pub fn for_key(
        &self,
        key: KeyEvent,
        aliases: &[String],
        read_only: bool,
        builtin_conflict: bool,
    ) -> Option<&Plugin> {
        let mut selected = None;
        for plugin in self.plugins.values().filter(|plugin| {
            plugin.matches_key(key) && plugin.in_scope(aliases) && !(read_only && plugin.dangerous)
        }) {
            if plugin.override_builtin || (selected.is_none() && !builtin_conflict) {
                selected = Some(plugin);
            }
        }
        selected
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(clippy::struct_excessive_bools)]
struct RawPlugin {
    #[serde(rename = "shortCut")]
    shortcut: String,
    description: String,
    scopes: Vec<String>,
    command: String,
    #[serde(default)]
    args: Vec<Argument>,
    #[serde(default)]
    pipes: Vec<String>,
    #[serde(default, rename = "override")]
    override_builtin: bool,
    confirm: Option<bool>,
    #[serde(default)]
    background: bool,
    #[serde(default)]
    dangerous: bool,
    #[serde(default, rename = "overwriteOutput")]
    overwrite_output: bool,
    #[serde(default)]
    inputs: Vec<RawInput>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum Argument {
    String(String),
    Number(serde_yaml::Number),
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum InputDefault {
    String(String),
    Bool(bool),
    Number(serde_yaml::Number),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawInput {
    name: String,
    #[serde(default)]
    label: String,
    #[serde(rename = "type")]
    kind: InputType,
    #[serde(default)]
    required: bool,
    default: Option<InputDefault>,
    #[serde(default)]
    options: Vec<String>,
}

impl RawPlugin {
    fn validate(self, name: String) -> Result<Plugin, String> {
        if name.is_empty() || self.command.trim().is_empty() || self.scopes.is_empty() {
            return Err("Plugin name, command, and scopes must not be empty".to_owned());
        }
        if self.inputs.len() > 5 {
            return Err("Plugins support at most five input fields".to_owned());
        }
        let binding = KeyShortcut::parse(&self.shortcut)?;
        let mut names = BTreeSet::new();
        let mut inputs = Vec::new();
        for input in self.inputs {
            if input.name.is_empty()
                || !input
                    .name
                    .chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-'))
            {
                return Err(
                    "Plugin input names must contain only letters, digits, underscores, or hyphens"
                        .to_owned(),
                );
            }
            if !names.insert(input.name.to_ascii_uppercase()) {
                return Err(format!("Duplicate plugin input name {:?}", input.name));
            }
            if input.default.is_some() && !input.required {
                return Err(format!(
                    "Input {:?} with a default must be required",
                    input.name
                ));
            }
            let default = input.default.map(|value| match value {
                InputDefault::String(value) => value,
                InputDefault::Bool(value) => value.to_string(),
                InputDefault::Number(value) => value.to_string(),
            });
            let field = PluginInput {
                label: if input.label.is_empty() {
                    input.name.clone()
                } else {
                    input.label
                },
                name: input.name,
                kind: input.kind,
                required: input.required,
                default,
                options: input.options,
            };
            if field.kind == InputType::Dropdown && field.options.is_empty() {
                return Err(format!(
                    "Dropdown input {:?} needs at least one option",
                    field.name
                ));
            }
            if let Some(default) = &field.default
                && !default.is_empty()
            {
                field.validate(default)?;
            }
            inputs.push(field);
        }
        for pipe in &self.pipes {
            if split_pipe(pipe)?.is_empty() {
                return Err("Plugin pipe command is empty".to_owned());
            }
        }
        Ok(Plugin {
            name,
            shortcut: self.shortcut,
            description: self.description,
            scopes: self.scopes,
            command: self.command,
            args: self
                .args
                .into_iter()
                .map(|arg| match arg {
                    Argument::String(value) => value,
                    Argument::Number(value) => value.to_string(),
                })
                .collect(),
            pipes: self.pipes,
            override_builtin: self.override_builtin,
            confirm: self.confirm,
            background: self.background,
            dangerous: self.dangerous,
            overwrite_output: self.overwrite_output,
            inputs,
            binding,
        })
    }
}

/// Parse a full `plugins:` document, a named plugin map, or a single plugin
/// whose name comes from the YAML filename. Validation is atomic per file.
///
/// # Errors
/// Returns syntax, schema, shortcut, or form-validation diagnostics.
pub fn parse_plugin_file(path: &Path, text: &str) -> Result<BTreeMap<String, Plugin>, String> {
    let mut value: serde_yaml::Value =
        serde_yaml::from_str(text).map_err(|error| error.to_string())?;
    if value.is_null() {
        return Ok(BTreeMap::new());
    }
    let mapping = value
        .as_mapping_mut()
        .ok_or_else(|| "Plugins YAML must be a mapping".to_owned())?;
    let plugin_named_plugins = mapping
        .get("plugins")
        .and_then(serde_yaml::Value::as_mapping)
        .is_some_and(|plugin| {
            plugin
                .get("command")
                .is_some_and(serde_yaml::Value::is_string)
        });
    if mapping.contains_key("plugins") && !plugin_named_plugins {
        if mapping.len() != 1 {
            return Err("A full plugin document must contain only the plugins key".to_owned());
        }
        value = mapping.remove("plugins").unwrap_or(serde_yaml::Value::Null);
    } else if mapping
        .get("command")
        .is_some_and(serde_yaml::Value::is_string)
        || mapping
            .get("shortCut")
            .is_some_and(serde_yaml::Value::is_string)
    {
        let name = path
            .file_stem()
            .and_then(|name| name.to_str())
            .ok_or_else(|| "Plugin filename must be UTF-8".to_owned())?;
        let plugin: RawPlugin = serde_yaml::from_value(value).map_err(|error| error.to_string())?;
        return plugin
            .validate(name.to_owned())
            .map(|plugin| BTreeMap::from([(name.to_owned(), plugin)]));
    }
    let raw: BTreeMap<String, RawPlugin> =
        serde_yaml::from_value(value).map_err(|error| error.to_string())?;
    raw.into_iter()
        .map(|(name, plugin)| {
            plugin
                .validate(name.clone())
                .map(|plugin| (name.clone(), plugin))
                .map_err(|error| format!("Plugin {name:?}: {error}"))
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum FileContent {
    Missing,
    Text(String),
    Error(String),
}

#[derive(Debug)]
struct CachedFile {
    content: FileContent,
    valid: BTreeMap<String, Plugin>,
    diagnostic: Option<ConfigDiagnostic>,
}

/// A synchronous watcher intended for the application's configuration thread.
/// Global files load first, then context file, then system/user plugin folders,
/// matching upstream precedence. Directory traversal is sorted and bounded.
#[derive(Debug)]
pub struct PluginWatcher {
    paths: ConfigPaths,
    data_dirs: Vec<PathBuf>,
    cluster: Option<String>,
    context: Option<String>,
    files: BTreeMap<PathBuf, CachedFile>,
    last: Option<PluginCatalog>,
}

impl PluginWatcher {
    #[must_use]
    pub fn new(paths: ConfigPaths, cluster: Option<&str>, context: Option<&str>) -> Self {
        let data_dirs = std::env::var_os("XDG_DATA_DIRS")
            .filter(|value| !value.is_empty())
            .map_or_else(
                || {
                    vec![
                        PathBuf::from("/usr/local/share"),
                        PathBuf::from("/usr/share"),
                    ]
                },
                |value| {
                    std::env::split_paths(&value)
                        .filter(|path| path.is_absolute())
                        .collect()
                },
            );
        Self {
            paths,
            data_dirs,
            cluster: cluster.map(str::to_owned),
            context: context.map(str::to_owned),
            files: BTreeMap::new(),
            last: None,
        }
    }

    #[must_use]
    pub fn with_data_dirs(mut self, directories: Vec<PathBuf>) -> Self {
        self.data_dirs = directories;
        self
    }

    #[must_use]
    pub fn set_context(&mut self, cluster: Option<&str>, context: Option<&str>) -> PluginCatalog {
        self.cluster = cluster.map(str::to_owned);
        self.context = context.map(str::to_owned);
        let _ = self.reload_if_changed();
        self.last.clone().unwrap_or_default()
    }

    #[must_use]
    pub fn reload_if_changed(&mut self) -> Option<PluginCatalog> {
        let mut ordered = vec![self.paths.config_dir.join("plugins.yaml")];
        if let (Some(cluster), Some(context)) = (&self.cluster, &self.context) {
            ordered.push(
                self.paths
                    .context_dir(cluster, context)
                    .join("plugins.yaml"),
            );
        }
        let directories = self
            .data_dirs
            .iter()
            .flat_map(|dir| [dir.join("kux/plugins"), dir.join("k9s/plugins")])
            .chain([
                self.paths.data_dir.join("plugins"),
                self.paths.config_dir.join("plugins"),
            ])
            .collect::<Vec<_>>();
        let mut catalog = PluginCatalog::default();
        for directory in directories {
            let mut visited = BTreeSet::new();
            let mut found = Vec::new();
            let first_diagnostic = catalog.diagnostics.len();
            scan_directory(
                &directory,
                0,
                &mut visited,
                &mut found,
                &mut catalog.diagnostics,
            );
            for diagnostic in &catalog.diagnostics[first_diagnostic..] {
                found.extend(
                    self.files
                        .keys()
                        .filter(|path| path.starts_with(&diagnostic.path))
                        .cloned(),
                );
            }
            found.sort();
            found.dedup();
            ordered.extend(found);
        }
        let active_paths = ordered.iter().cloned().collect::<BTreeSet<_>>();
        for path in ordered {
            let content = read_plugin_file(&path);
            if self
                .files
                .get(&path)
                .is_none_or(|cached| cached.content != content)
            {
                let (valid, error) = match &content {
                    FileContent::Missing => (Some(BTreeMap::new()), None),
                    FileContent::Text(text) => match parse_plugin_file(&path, text) {
                        Ok(plugins) => (Some(plugins), None),
                        Err(error) => (None, Some(error)),
                    },
                    FileContent::Error(error) => (None, Some(error.clone())),
                };
                let valid = valid.unwrap_or_else(|| {
                    self.files
                        .get(&path)
                        .map_or_else(BTreeMap::new, |cached| cached.valid.clone())
                });
                self.files.insert(
                    path.clone(),
                    CachedFile {
                        content,
                        valid,
                        diagnostic: error.map(|message| ConfigDiagnostic {
                            path: path.clone(),
                            message,
                        }),
                    },
                );
            }
            if let Some(cached) = self.files.get(&path) {
                catalog.plugins.extend(cached.valid.clone());
                catalog.diagnostics.extend(cached.diagnostic.clone());
            }
        }
        self.files.retain(|path, _| active_paths.contains(path));
        if self.last.as_ref() == Some(&catalog) {
            None
        } else {
            self.last = Some(catalog.clone());
            Some(catalog)
        }
    }
}

fn scan_directory(
    path: &Path,
    depth: usize,
    visited: &mut BTreeSet<PathBuf>,
    files: &mut Vec<PathBuf>,
    diagnostics: &mut Vec<ConfigDiagnostic>,
) {
    let mut scan = || -> Result<(), String> {
        if depth > MAX_DIRECTORY_DEPTH || files.len() >= MAX_DIRECTORY_FILES {
            return Err("Plugin directory exceeds depth or file-count limit".to_owned());
        }
        let canonical = match path.canonicalize() {
            Ok(path) => path,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(format!("Unable to inspect plugin directory: {error}")),
        };
        if !visited.insert(canonical) {
            return Ok(());
        }
        let mut children = std::fs::read_dir(path)
            .map_err(|error| format!("Unable to read plugin directory: {error}"))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| format!("Unable to read plugin entry: {error}"))?;
        children.sort_by_key(std::fs::DirEntry::file_name);
        for child in children {
            let path = child.path();
            if path.is_dir() {
                scan_directory(&path, depth + 1, visited, files, diagnostics);
            } else if path
                .extension()
                .and_then(|extension| extension.to_str())
                .is_some_and(|extension| matches!(extension, "yaml" | "yml"))
            {
                if files.len() >= MAX_DIRECTORY_FILES {
                    return Err("Plugin directory exceeds file-count limit".to_owned());
                }
                files.push(path);
            }
        }
        Ok(())
    };
    if let Err(message) = scan() {
        diagnostics.push(ConfigDiagnostic {
            path: path.to_owned(),
            message,
        });
    }
}

fn read_plugin_file(path: &Path) -> FileContent {
    let read = || -> Result<Option<String>, String> {
        let metadata = match std::fs::metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(format!("Unable to inspect plugin file: {error}")),
        };
        if !metadata.is_file() || metadata.len() > MAX_FILE_BYTES {
            return Err(
                "Plugin configuration must be a regular file no larger than 1 MiB".to_owned(),
            );
        }
        let mut contents = String::new();
        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NONBLOCK);
        }
        let file = options
            .open(path)
            .map_err(|error| format!("Unable to open plugin file: {error}"))?;
        if !file
            .metadata()
            .map_err(|error| format!("Unable to inspect opened plugin file: {error}"))?
            .is_file()
        {
            return Err("Plugin configuration must be a regular file".to_owned());
        }
        file.take(MAX_FILE_BYTES + 1)
            .read_to_string(&mut contents)
            .map_err(|error| format!("Unable to read UTF-8 plugin file: {error}"))?;
        if contents.len() as u64 > MAX_FILE_BYTES {
            return Err("Plugin file grew beyond the 1 MiB limit".to_owned());
        }
        Ok(Some(contents))
    };
    match read() {
        Ok(Some(text)) => FileContent::Text(text),
        Ok(None) => FileContent::Missing,
        Err(error) => FileContent::Error(error),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use crossterm::event::{KeyCode, KeyModifiers};

    use super::*;

    static SEQUENCE: AtomicU64 = AtomicU64::new(0);

    struct TempDirectory(PathBuf);

    impl TempDirectory {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "kux-plugins-{}-{}",
                std::process::id(),
                SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn paths(&self) -> ConfigPaths {
            ConfigPaths::new(self.0.join("config"), self.0.join("data"))
        }
    }

    impl Drop for TempDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn definition(command: &str) -> String {
        format!(
            "shortCut: Ctrl-T\ndescription: Test\nscopes: [pods, v1/pods]\ncommand: {command}\n"
        )
    }

    fn plugin(extra: &str) -> Plugin {
        parse_plugin_file(Path::new("test.yaml"), &(definition("example") + extra))
            .unwrap()
            .remove("test")
            .unwrap()
    }

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn named(name: &str, command: &str) -> String {
        format!("{name}:\n{}", indented(&definition(command)))
    }

    fn indented(text: &str) -> String {
        text.lines().fold(String::new(), |mut output, line| {
            output.push_str("  ");
            output.push_str(line);
            output.push('\n');
            output
        })
    }

    #[test]
    fn parses_single_named_and_full_forms_with_numeric_arguments() {
        let single = definition("kubectl")
            + "args: [get, pods, 12]\nbackground: true\noverwriteOutput: true\n";
        let parsed = parse_plugin_file(Path::new("pods.yaml"), &single).unwrap();
        assert_eq!(parsed["pods"].args, ["get", "pods", "12"]);
        assert!(parsed["pods"].background);
        assert!(parsed["pods"].overwrite_output);
        assert!(!parsed["pods"].should_confirm());
        let multi = named("one", "first") + &named("two", "second");
        assert_eq!(
            parse_plugin_file(Path::new("multi.yaml"), &multi)
                .unwrap()
                .len(),
            2
        );
        let full = format!("plugins:\n{}", indented(&multi));
        assert_eq!(
            parse_plugin_file(Path::new("plugins.yaml"), &full)
                .unwrap()
                .len(),
            2
        );
        for name in ["command", "shortCut", "plugins"] {
            assert!(
                parse_plugin_file(Path::new("named.yaml"), &named(name, "example"))
                    .unwrap()
                    .contains_key(name)
            );
        }
    }

    #[test]
    fn inputs_validate_types_defaults_requirements_and_confirmation() {
        let definition = plugin(
            "inputs:\n  - {name: text, label: Text, type: string, required: true}\n  - {name: replicas, type: number, required: true, default: 1}\n  - {name: force, type: bool}\n  - {name: mode, type: dropdown, required: true, options: [fast, slow], default: fast}\n  - {name: optional, type: string}\n",
        );
        assert!(definition.should_confirm());
        assert_eq!(definition.default_inputs()["replicas"], "1");
        assert_eq!(definition.default_inputs()["force"], "false");
        assert!(
            definition
                .validate_inputs(&BTreeMap::new())
                .unwrap_err()
                .contains("required")
        );
        let valid =
            PluginInputValues::from([("text".to_owned(), "a b; $(touch /nope)".to_owned())]);
        let values = definition.validate_inputs(&valid).unwrap();
        assert!(!values.contains_key("optional"));
        assert_eq!(values["text"], "a b; $(touch /nope)");
        for (name, value) in [
            ("replicas", "NaN"),
            ("replicas", "1;echo pwn"),
            ("force", "1"),
            ("mode", "unknown"),
            ("unknown", "x"),
        ] {
            let mut invalid = valid.clone();
            invalid.insert(name.to_owned(), value.to_owned());
            assert!(definition.validate_inputs(&invalid).is_err(), "{name}");
        }
        assert!(!plugin("confirm: false\ninputs: [{name: ok, type: bool}]\n").should_confirm());
        assert!(plugin("confirm: true\n").should_confirm());
    }

    #[test]
    fn invalid_schema_and_input_definitions_are_diagnostic() {
        for suffix in [
            "surprise: true\n",
            "inputs: [{name: n, type: number, default: 1}]\n",
            "inputs: [{name: n, type: number, required: true, default: nope}]\n",
            "inputs: [{name: n, type: dropdown, required: true, default: bad, options: [good]}]\n",
            "inputs: [{name: n, type: bool, required: true, default: invalid}]\n",
            "inputs: [{name: n, type: dropdown}]\n",
            "inputs: [{name: n, type: unsupported}]\n",
            "inputs: [{name: n, type: string}, {name: N, type: string}]\n",
            "inputs: [{name: a, type: string}, {name: b, type: string}, {name: c, type: string}, {name: d, type: string}, {name: e, type: string}, {name: f, type: string}]\n",
            "pipes: [\"unterminated '\"]\n",
        ] {
            assert!(
                parse_plugin_file(Path::new("test.yaml"), &(definition("example") + suffix))
                    .is_err(),
                "{suffix}"
            );
        }
        assert!(parse_plugin_file(Path::new("x.yaml"), "plugins: {}\nunknown: true").is_err());
        assert!(parse_plugin_file(Path::new("x.yaml"), "[one, two]").is_err());
        assert!(parse_plugin_file(Path::new("x.yaml"), "command: example").is_err());
    }

    #[test]
    fn variable_expansion_is_single_pass_and_preserves_numeric_inputs() {
        let environment = PluginEnvironment::from([
            ("NAME".to_owned(), "a b; $(echo injected)".to_owned()),
            ("COL-SUSPEND".to_owned(), "false".to_owned()),
            ("COL-%CPU/L".to_owned(), "42".to_owned()),
            ("COL-MEM/R:L".to_owned(), "32:32".to_owned()),
            ("READINESS GATES".to_owned(), "3".to_owned()),
            ("A".to_owned(), "$NAME".to_owned()),
            ("AA".to_owned(), "other".to_owned()),
            ("INPUT_REPLICAS".to_owned(), "1".to_owned()),
        ]);
        assert_eq!(
            expand_variables("$name ${NAME}", &environment),
            "a b; $(echo injected) a b; $(echo injected)"
        );
        assert_eq!(
            expand_variables("$!COL-SUSPEND ${!COL-SUSPEND}", &environment),
            "true true"
        );
        assert_eq!(
            expand_variables(
                "${COL-%CPU/L}/${COL-MEM/R:L}/${READINESS GATES}",
                &environment
            ),
            "42/32:32/3"
        );
        assert_eq!(expand_variables("$A $AA", &environment), "$NAME other");
        assert_eq!(expand_variables("${INPUT_REPLICAS}", &environment), "1");
        assert_eq!(
            expand_variables("$UNKNOWN ${NAME", &environment),
            "$UNKNOWN ${NAME"
        );
    }

    #[test]
    fn invocation_preserves_argv_and_only_explicit_shells_can_evaluate() {
        let definition = plugin(
            "args: ['$NAME', '--message=${INPUT_TEXT}']\npipes:\n  - >-\n    processor \"$NAME\" 'fixed text'\ninputs: [{name: text, type: string, required: true}]\n",
        );
        let environment =
            PluginEnvironment::from([("NAME".to_owned(), "x; touch /never".to_owned())]);
        let invocation = definition
            .invocation(
                &environment,
                &PluginInputValues::from([("text".to_owned(), "a b".to_owned())]),
            )
            .unwrap();
        assert_eq!(invocation.program, "example");
        assert_eq!(invocation.args, ["x; touch /never", "--message=a b"]);
        assert_eq!(invocation.pipes[0].program, "processor");
        assert_eq!(invocation.pipes[0].args, ["x; touch /never", "fixed text"]);
        assert_eq!(invocation.env["INPUT_TEXT"], "a b");
        assert_eq!(
            invocation.preview(),
            "\"example\" \"x; touch /never\" \"--message=a b\" | \"processor\" \"x; touch /never\" \"fixed text\""
        );
        let mut invalid = environment;
        invalid.insert("NAME".to_owned(), "with\0nul".to_owned());
        assert!(
            definition
                .invocation(
                    &invalid,
                    &PluginInputValues::from([("text".to_owned(), "ok".to_owned())])
                )
                .is_err()
        );
    }

    #[test]
    fn scope_shortcut_override_and_readonly_policies_are_deterministic() {
        let first = plugin("dangerous: true\n");
        assert!(first.in_scope(&["v1/pods".to_owned()]));
        assert!(!first.in_scope(&["deployments".to_owned()]));
        let key = KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL);
        assert!(first.matches_key(key));
        let shifted = parse_plugin_file(
            Path::new("shifted.yaml"),
            &definition("example").replace("Ctrl-T", "Shift-F"),
        )
        .unwrap()
        .remove("shifted")
        .unwrap();
        assert_eq!(shifted.key_event().code, KeyCode::Char('F'));
        assert!(shifted.matches_key(shifted.key_event()));
        let mut catalog = PluginCatalog {
            plugins: BTreeMap::from([("first".to_owned(), first)]),
            ..PluginCatalog::default()
        };
        assert!(
            catalog
                .for_key(key, &["pods".to_owned()], false, false)
                .is_some()
        );
        assert!(
            catalog
                .for_key(key, &["pods".to_owned()], true, false)
                .is_none()
        );
        assert!(
            catalog
                .for_key(key, &["pods".to_owned()], false, true)
                .is_none()
        );
        let mut override_plugin = plugin("override: true\n");
        override_plugin.name = "last".to_owned();
        catalog.plugins.insert("last".to_owned(), override_plugin);
        assert_eq!(
            catalog
                .for_key(key, &["pods".to_owned()], true, true)
                .unwrap()
                .name,
            "last"
        );
    }

    #[test]
    fn loader_uses_documented_precedence_contexts_and_sorted_directories() {
        let directory = TempDirectory::new();
        let paths = directory.paths();
        let system = directory.0.join("system");
        write(
            &paths.config_dir.join("plugins.yaml"),
            &named("one", "global"),
        );
        write(
            &paths
                .context_dir("cluster", "production")
                .join("plugins.yaml"),
            &named("one", "context"),
        );
        write(&system.join("kux/plugins/a.yaml"), &named("one", "system"));
        write(
            &paths.data_dir.join("plugins/a.yaml"),
            &named("one", "data"),
        );
        write(
            &paths.config_dir.join("plugins/b.yaml"),
            &named("one", "config-b"),
        );
        write(
            &paths.config_dir.join("plugins/a.yaml"),
            &named("one", "config-a"),
        );
        let mut watcher = PluginWatcher::new(paths.clone(), Some("cluster"), Some("production"))
            .with_data_dirs(vec![system]);
        assert_eq!(
            watcher.reload_if_changed().unwrap().plugins["one"].command,
            "config-b"
        );
        assert!(watcher.reload_if_changed().is_none());
        std::fs::remove_dir_all(paths.config_dir.join("plugins")).unwrap();
        assert_eq!(
            watcher.reload_if_changed().unwrap().plugins["one"].command,
            "data"
        );
        std::fs::remove_dir_all(paths.data_dir.join("plugins")).unwrap();
        watcher.data_dirs.clear();
        assert_eq!(
            watcher.reload_if_changed().unwrap().plugins["one"].command,
            "context"
        );
        assert_eq!(
            watcher.set_context(Some("cluster"), Some("other")).plugins["one"].command,
            "global"
        );
    }

    #[test]
    fn reload_preserves_last_valid_file_and_recovers_after_repair_or_removal() {
        let directory = TempDirectory::new();
        let paths = directory.paths();
        let global = paths.config_dir.join("plugins.yaml");
        write(&global, &named("one", "old"));
        let mut watcher = PluginWatcher::new(paths.clone(), None, None).with_data_dirs(Vec::new());
        assert_eq!(
            watcher.reload_if_changed().unwrap().plugins["one"].command,
            "old"
        );
        write(&global, "plugins: [bad\n");
        write(
            &paths.config_dir.join("plugins/other.yaml"),
            &definition("other"),
        );
        let invalid = watcher.reload_if_changed().unwrap();
        assert_eq!(invalid.plugins["one"].command, "old");
        assert!(invalid.plugins.contains_key("other"));
        assert_eq!(invalid.diagnostics.len(), 1);
        assert_eq!(invalid.diagnostics[0].path, global);
        write(&global, &named("one", "repaired"));
        assert_eq!(
            watcher.reload_if_changed().unwrap().plugins["one"].command,
            "repaired"
        );
        std::fs::remove_file(global).unwrap();
        let deleted = watcher.reload_if_changed().unwrap();
        assert!(!deleted.plugins.contains_key("one"));
        assert_eq!(deleted.diagnostics, Vec::<ConfigDiagnostic>::new());
    }

    #[cfg(unix)]
    #[test]
    fn directories_follow_symlinks_without_cycles_and_reject_nonregular_files() {
        let directory = TempDirectory::new();
        let paths = directory.paths();
        let plugins = paths.config_dir.join("plugins");
        write(&plugins.join("nested/a.yml"), &definition("a"));
        std::os::unix::fs::symlink(&plugins, plugins.join("nested/loop")).unwrap();
        let _socket = std::os::unix::net::UnixListener::bind(plugins.join("socket.yaml")).unwrap();
        let mut watcher = PluginWatcher::new(paths, None, None).with_data_dirs(Vec::new());
        let catalog = watcher.reload_if_changed().unwrap();
        assert_eq!(catalog.plugins.len(), 1);
        assert_eq!(catalog.diagnostics.len(), 1);
        assert!(catalog.diagnostics[0].message.contains("regular file"));
    }
}
