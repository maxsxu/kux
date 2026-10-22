use std::{
    cmp::Ordering,
    collections::{BTreeMap, BTreeSet, HashSet},
    sync::Arc,
    time::{Duration, Instant},
};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use regex::RegexBuilder;

use crate::config::UserConfig;
use crate::kubeconfig::ContextInfo;
use crate::log_view::{LogView, append_display_lines, plain_log_line, without_log_timestamp};
use crate::model::{
    NamespaceInfo, OwnerReferenceData, PortForwardPort, ResourceData, ResourceDescriptor,
    ResourceKind, ResourceOperations, ResourceRow, ResourceSnapshot, SourceState, unix_now,
};
use crate::source::{
    LogLine, LogRange, LogRequest, LogSessionMeta, LogTarget, YamlContent, supports_logs,
};

#[path = "app_plugins.rs"]
mod plugin_ui;
pub use plugin_ui::PluginDialog;
#[path = "app_preferences.rs"]
mod preferences;
pub use preferences::LaunchPreferences;
#[path = "app_dumps.rs"]
mod dumps;
pub use dumps::{DumpIntent, DumpPayload, ScreenDumpState};
#[path = "app_live.rs"]
mod live;
#[cfg(test)]
#[path = "app_logger_tests.rs"]
mod logger_tests;
#[cfg(test)]
#[path = "app_multilog_tests.rs"]
mod multilog_tests;
use live::LiveRefreshState;
#[path = "app_images.rs"]
mod images;
pub use images::{ImageDialog, ImageEditorState};
#[path = "app_transfers.rs"]
mod transfers;
pub use transfers::{TransferDialog, TransferState};
#[path = "app_sanitize.rs"]
mod sanitize;
pub use sanitize::{SANITIZE_CONFIRMATION, SanitizeDialog};
#[path = "app_rollback.rs"]
mod rollback;
pub use rollback::{RollbackDialog, RollbackState};
#[path = "app_navigation.rs"]
mod navigation;
pub use navigation::NavigationState;
#[path = "app_references.rs"]
mod references;
pub use references::ReferencesState;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SortField {
    Kind,
    Validity,
    Namespace,
    #[default]
    Name,
    Ready,
    Status,
    Restarts,
    Cpu,
    CpuRequestPercent,
    CpuLimitPercent,
    Memory,
    MemoryRequestPercent,
    MemoryLimitPercent,
    Node,
    Taints,
    Version,
    Pods,
    CpuAllocatable,
    CpuPercent,
    MemoryAllocatable,
    MemoryPercent,
    Age,
    Details,
    ApiGroup,
    Binding,
    Get,
    List,
    Watch,
    Create,
    Patch,
    Update,
    Delete,
    DeleteCollection,
    VulnScore,
}

impl SortField {
    pub const ALL: [Self; 34] = [
        Self::Namespace,
        Self::Name,
        Self::Ready,
        Self::Status,
        Self::Restarts,
        Self::Cpu,
        Self::CpuRequestPercent,
        Self::CpuLimitPercent,
        Self::Memory,
        Self::MemoryRequestPercent,
        Self::MemoryLimitPercent,
        Self::Node,
        Self::Taints,
        Self::Version,
        Self::Pods,
        Self::CpuAllocatable,
        Self::CpuPercent,
        Self::MemoryAllocatable,
        Self::MemoryPercent,
        Self::Age,
        Self::Details,
        Self::ApiGroup,
        Self::Binding,
        Self::Get,
        Self::List,
        Self::Watch,
        Self::Create,
        Self::Patch,
        Self::Update,
        Self::Delete,
        Self::DeleteCollection,
        Self::Kind,
        Self::Validity,
        Self::VulnScore,
    ];

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Kind => "kind",
            Self::Validity => "valid",
            Self::VulnScore => "vs",
            Self::Namespace => "namespace",
            Self::Name => "name",
            Self::Ready => "ready",
            Self::Status => "status",
            Self::Restarts => "restarts",
            Self::Cpu => "cpu",
            Self::CpuRequestPercent => "%cpu/r",
            Self::CpuLimitPercent => "%cpu/l",
            Self::Memory => "mem",
            Self::MemoryRequestPercent => "%mem/r",
            Self::MemoryLimitPercent => "%mem/l",
            Self::Node => "node",
            Self::Taints => "taints",
            Self::Version => "version",
            Self::Pods => "pods",
            Self::CpuAllocatable => "cpu/a",
            Self::CpuPercent => "%cpu",
            Self::MemoryAllocatable => "mem/a",
            Self::MemoryPercent => "%mem",
            Self::Age => "age",
            Self::Details => "details",
            Self::ApiGroup => "api-group",
            Self::Binding => "binding",
            Self::Get => "get",
            Self::List => "list",
            Self::Watch => "watch",
            Self::Create => "create",
            Self::Patch => "patch",
            Self::Update => "update",
            Self::Delete => "delete",
            Self::DeleteCollection => "del-list",
        }
    }
}

impl std::fmt::Display for SortField {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewState {
    pub instance: Option<ResourceTarget>,
    pub context: String,
    pub kind: ResourceDescriptor,
    pub namespace: Option<String>,
    pub field_selector: Option<String>,
    pub label_selector: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ColumnSortKey {
    Field(SortField),
    TableIndex(usize),
    JsonPath(String),
    Computed(&'static str),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ColumnHitbox {
    pub(crate) x: u16,
    pub(crate) y: u16,
    pub(crate) width: u16,
    pub(crate) key: ColumnSortKey,
}

impl ColumnHitbox {
    const fn contains(&self, column: u16, row: u16) -> bool {
        row == self.y && column >= self.x && column < self.x.saturating_add(self.width)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct ResourceRowsHitbox {
    x: u16,
    y: u16,
    width: u16,
    first: usize,
    count: usize,
}

impl ResourceRowsHitbox {
    fn index_at(self, column: u16, row: u16) -> Option<usize> {
        let row_offset = usize::from(row.checked_sub(self.y)?);
        (column >= self.x && column < self.x.saturating_add(self.width) && row_offset < self.count)
            .then(|| self.first.saturating_add(row_offset))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentKind {
    Yaml,
    SecretData,
    HelmValues,
    Describe,
    Logs,
    PreviousLogs,
    ScreenDump,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceTarget {
    pub kind: ResourceDescriptor,
    pub namespace: Option<String>,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerTarget {
    pub namespace: String,
    pub pod: String,
    pub container: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PodShellTarget {
    pub namespace: String,
    pub pod: String,
    pub container: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeShellTarget {
    pub node: String,
    pub settings: crate::config::ShellPodSettings,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortForwardRequest {
    pub target: ResourceTarget,
    pub remote_port: u16,
    pub local_port: u16,
    pub address: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortForwardState {
    Starting,
    Running,
}

impl PortForwardState {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Starting => "Starting",
            Self::Running => "Running",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortForwardInfo {
    pub id: u64,
    pub context: String,
    pub target: ResourceTarget,
    pub remote_port: u16,
    pub local_port: u16,
    pub address: String,
    pub state: PortForwardState,
}

#[must_use]
pub fn cow_talk(says: &str) -> String {
    let says = if says.trim().is_empty() {
        "Nothing to report here. Please move along..."
    } else {
        says.trim()
    };
    let msg = format!("< Ruroh? {says} >");
    let rule = format!(" {}", "─".repeat(says.chars().count() + 11));
    [
        rule.as_str(),
        msg.as_str(),
        rule.as_str(),
        r"        \   ^__^            ",
        r"         \  (oo)\_______    ",
        r"            (__)\       )\/\",
        r"                ||----w |   ",
        r"                ||     ||   ",
    ]
    .join("\n")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortForwardDialog {
    pub target: ResourceTarget,
    pub ports: Vec<PortForwardPort>,
    pub remote_port: String,
    pub local_port: String,
    pub address: String,
}

pub const PORT_FORWARD_REMOTE_FIELD: usize = 0;
pub const PORT_FORWARD_LOCAL_FIELD: usize = 1;
pub const PORT_FORWARD_ADDRESS_FIELD: usize = 2;
pub const PORT_FORWARD_OK_FIELD: usize = 3;
pub const PORT_FORWARD_CANCEL_FIELD: usize = 4;
const PORT_FORWARD_FIELD_COUNT: usize = 5;

impl PortForwardDialog {
    fn new(
        target: ResourceTarget,
        ports: Vec<PortForwardPort>,
        default_address: &str,
        preferred_spec: Option<&str>,
    ) -> Self {
        let (remote_port, local_port) = preferred_spec
            .and_then(|spec| resolve_pf_annotation(spec, &ports))
            .unwrap_or_else(|| {
                let remote = ports
                    .first()
                    .map_or_else(String::new, PortForwardPort::selector);
                let local = ports
                    .first()
                    .map_or_else(String::new, |port| port.port.to_string());
                (remote, local)
            });
        Self {
            target,
            ports,
            remote_port,
            local_port,
            address: default_address.to_owned(),
        }
    }

    fn request(&self) -> Result<PortForwardRequest, String> {
        let remote = self
            .remote_port
            .rsplit_once("::")
            .map_or(self.remote_port.as_str(), |(_, port)| port)
            .trim();
        let remote_port = parse_port(remote, "Container/Service port")?;
        let local_port = parse_port(&self.local_port, "Local port")?;
        let address = self.address.trim();
        if address.is_empty() {
            return Err("Address cannot be empty".to_owned());
        }
        Ok(PortForwardRequest {
            target: self.target.clone(),
            remote_port,
            local_port,
            address: address.to_owned(),
        })
    }
}

fn parse_port(value: &str, label: &str) -> Result<u16, String> {
    value
        .trim()
        .parse::<u16>()
        .ok()
        .filter(|port| *port > 0)
        .ok_or_else(|| format!("{label} must be between 1 and 65535"))
}

#[must_use]
pub fn resolve_pf_annotation(spec: &str, ports: &[PortForwardPort]) -> Option<(String, String)> {
    for token in spec.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        if let Some((container, rest)) = token.split_once("::") {
            let (local_str, remote_token) = rest
                .split_once(':')
                .map_or((rest, rest), |(l, r)| (l, if r.is_empty() { l } else { r }));
            for port in ports {
                if port.container.as_deref() == Some(container)
                    && (port.name.as_ref() == remote_token || port.port.to_string() == remote_token)
                {
                    let local = if local_str.is_empty() {
                        port.port.to_string()
                    } else {
                        local_str.to_owned()
                    };
                    return Some((port.selector(), local));
                }
            }
        } else {
            let (local_str, remote_token) =
                token.split_once(':').map_or((token, token), |(l, r)| {
                    (l, if r.is_empty() { l } else { r })
                });
            for port in ports {
                if port.name.as_ref() == remote_token || port.port.to_string() == remote_token {
                    let local = if local_str.is_empty() {
                        port.port.to_string()
                    } else {
                        local_str.to_owned()
                    };
                    return Some((port.selector(), local));
                }
            }
        }
    }
    None
}

fn port_forward_field_mut(dialog: &mut PortForwardDialog, field: usize) -> &mut String {
    match field {
        PORT_FORWARD_REMOTE_FIELD => &mut dialog.remote_port,
        PORT_FORWARD_LOCAL_FIELD => &mut dialog.local_port,
        PORT_FORWARD_ADDRESS_FIELD => &mut dialog.address,
        _ => unreachable!("only editable port-forward fields are requested"),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionIntent {
    LoadReferences {
        id: u64,
        target: ResourceTarget,
    },
    LoadDeploymentSelector {
        id: u64,
        target: ResourceTarget,
        replica_sets: bool,
    },
    LoadBatchNavigation {
        id: u64,
        target: ResourceTarget,
    },
    LoadRollback {
        id: u64,
        target: ResourceTarget,
    },
    Rollback {
        snapshot: Box<crate::source::RollbackSnapshot>,
    },
    LoadTransferPod {
        id: u64,
        target: ResourceTarget,
    },
    SanitizePods {
        namespace: Option<String>,
    },
    LoadImages {
        id: u64,
        target: ResourceTarget,
    },
    SetImages {
        snapshot: crate::source::ImageSnapshot,
        images: Vec<crate::source::ContainerImage>,
    },
    Delete {
        targets: Vec<ResourceTarget>,
        immediate: bool,
    },
    DeleteContext {
        name: String,
    },
    RenameContext {
        old_name: String,
        new_name: String,
    },
    Content {
        id: u64,
        target: ResourceTarget,
        content: ContentKind,
    },
    ManagedYaml {
        id: u64,
        target: ResourceTarget,
        body: String,
        decoded_body: Option<String>,
    },
    RefreshYaml {
        id: u64,
        target: ResourceTarget,
        include_managed_fields: bool,
    },
    RefreshDescribe {
        id: u64,
        target: ResourceTarget,
    },
    StopContentRefresh,
    Containers {
        target: ResourceTarget,
    },
    ContainerLogs {
        target: ContainerTarget,
        previous: bool,
    },
    StartLogs {
        request: LogRequest,
    },
    StopLogs {
        id: u64,
    },
    AuthorizationPolicy {
        target: ResourceTarget,
    },
    FindChildResource {
        target: ResourceTarget,
        resources: Vec<ResourceDescriptor>,
    },
    ResolveOwner {
        target: ResourceTarget,
    },
    Restart {
        targets: Vec<ResourceTarget>,
    },
    Scale {
        targets: Vec<ResourceTarget>,
        replicas: i32,
    },
    TriggerCronJob {
        targets: Vec<ResourceTarget>,
    },
    ToggleCronJobSuspend {
        target: ResourceTarget,
    },
    SetSuspended {
        targets: Vec<ResourceTarget>,
        suspended: bool,
    },
    SetNodeUnschedulable {
        names: Vec<String>,
        unschedulable: bool,
    },
    DrainNode {
        names: Vec<String>,
        options: NodeDrainOptions,
    },
    HelmRollback {
        namespace: String,
        release: String,
        revision: String,
    },
    DirApply {
        path: String,
    },
    DirDelete {
        path: String,
    },
}

impl ActionIntent {
    #[must_use]
    pub(crate) const fn cancelable_read(&self) -> bool {
        matches!(
            self,
            Self::Content { .. }
                | Self::LoadReferences { .. }
                | Self::LoadDeploymentSelector { .. }
                | Self::LoadBatchNavigation { .. }
                | Self::LoadRollback { .. }
                | Self::LoadTransferPod { .. }
                | Self::LoadImages { .. }
                | Self::ManagedYaml { .. }
                | Self::RefreshYaml { .. }
                | Self::RefreshDescribe { .. }
                | Self::StopContentRefresh
                | Self::Containers { .. }
                | Self::ContainerLogs { .. }
                | Self::StartLogs { .. }
                | Self::StopLogs { .. }
                | Self::AuthorizationPolicy { .. }
                | Self::FindChildResource { .. }
                | Self::ResolveOwner { .. }
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeDrainOptions {
    pub grace_period_seconds: Option<u32>,
    pub timeout: Duration,
    pub flags: NodeDrainFlags,
}

impl Default for NodeDrainOptions {
    fn default() -> Self {
        Self {
            grace_period_seconds: None,
            timeout: Duration::from_secs(5),
            flags: NodeDrainFlags::NONE,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct NodeDrainFlags(u8);

impl NodeDrainFlags {
    pub const NONE: Self = Self(0);
    pub const IGNORE_DAEMON_SETS: Self = Self(1 << 0);
    pub const DELETE_EMPTYDIR_DATA: Self = Self(1 << 1);
    pub const FORCE: Self = Self(1 << 2);
    pub const DISABLE_EVICTION: Self = Self(1 << 3);
    pub const ALL: Self = Self(
        Self::IGNORE_DAEMON_SETS.0
            | Self::DELETE_EMPTYDIR_DATA.0
            | Self::FORCE.0
            | Self::DISABLE_EVICTION.0,
    );

    #[must_use]
    pub const fn contains(self, flag: Self) -> bool {
        self.0 & flag.0 != 0
    }

    fn toggle(&mut self, flag: Self) {
        self.0 ^= flag.0;
    }
}

impl std::ops::BitOr for NodeDrainFlags {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self::Output {
        Self(self.0 | rhs.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DrainDialog {
    pub names: Vec<String>,
    pub grace_period: String,
    pub timeout: String,
    pub flags: NodeDrainFlags,
}

pub const DRAIN_GRACE_PERIOD_FIELD: usize = 0;
pub const DRAIN_TIMEOUT_FIELD: usize = 1;
pub const DRAIN_IGNORE_DAEMON_SETS_FIELD: usize = 2;
pub const DRAIN_DELETE_EMPTYDIR_DATA_FIELD: usize = 3;
pub const DRAIN_FORCE_FIELD: usize = 4;
pub const DRAIN_DISABLE_EVICTION_FIELD: usize = 5;
pub const DRAIN_CANCEL_FIELD: usize = 6;
pub const DRAIN_OK_FIELD: usize = 7;
const DRAIN_FIELD_COUNT: usize = 8;

impl DrainDialog {
    pub(crate) fn new(names: Vec<String>) -> Self {
        Self {
            names,
            grace_period: "-1".to_owned(),
            timeout: "5s".to_owned(),
            flags: NodeDrainFlags::NONE,
        }
    }

    fn options(&self) -> Result<NodeDrainOptions, String> {
        let grace_period = self
            .grace_period
            .parse::<i64>()
            .map_err(|_| "GracePeriod must be -1 or a non-negative number of seconds".to_owned())?;
        if grace_period < -1 || grace_period > i64::from(u32::MAX) {
            return Err("GracePeriod must be -1 or a non-negative number of seconds".to_owned());
        }
        let grace_period_seconds = (grace_period >= 0)
            .then(|| u32::try_from(grace_period).expect("validated grace period"));
        let timeout = parse_drain_duration(&self.timeout)?;
        Ok(NodeDrainOptions {
            grace_period_seconds,
            timeout,
            flags: self.flags,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DrainValueEditState {
    Pristine,
    Editing,
}

fn parse_drain_duration(value: &str) -> Result<Duration, String> {
    let value = value.trim();
    let (number, multiplier) = if let Some(number) = value.strip_suffix("ms") {
        (number, 1_u64)
    } else if let Some(number) = value.strip_suffix('s') {
        (number, 1_000)
    } else if let Some(number) = value.strip_suffix('m') {
        (number, 60_000)
    } else if let Some(number) = value.strip_suffix('h') {
        (number, 3_600_000)
    } else {
        return Err("Timeout must use ms, s, m, or h, for example 5s".to_owned());
    };
    let amount = number
        .parse::<u64>()
        .map_err(|_| "Timeout must use ms, s, m, or h, for example 5s".to_owned())?;
    let millis = amount
        .checked_mul(multiplier)
        .ok_or_else(|| "Timeout is too large".to_owned())?;
    if millis == 0 {
        return Err("Timeout must be greater than zero".to_owned());
    }
    Ok(Duration::from_millis(millis))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppCommand {
    Transfer(crate::transfer::TransferIntent),
    None,
    Quit,
    Request(ViewState),
    Copy(String),
    SaveLogs { name: String, body: String },
    Dump(DumpIntent),
    EditDump(std::path::PathBuf),
    Edit(ResourceTarget),
    Shell(PodShellTarget),
    NodeShell(NodeShellTarget),
    Attach(PodShellTarget),
    Plugin(crate::plugins::PluginInvocation),
    StartPortForward(PortForwardRequest),
    ShowPortForwards,
    StopPortForward(u64),
    RunBenchmark(crate::benchmarks::BenchmarkRunRequest),
    OpenUrl(String),
    Action(ActionIntent),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PendingDelete {
    Resources(Vec<ResourceTarget>),
    Restart(Vec<ResourceTarget>),
    Scale {
        targets: Vec<ResourceTarget>,
        replicas: String,
    },
    TriggerCronJob(Vec<ResourceTarget>),
    ToggleCronJobSuspend {
        target: ResourceTarget,
        suspend: Option<bool>,
    },
    SetSuspended {
        targets: Vec<ResourceTarget>,
        suspended: bool,
    },
    Context {
        name: String,
    },
    RenameContext {
        old_name: String,
        new_name: String,
    },
    NodeScheduling {
        names: Vec<String>,
        unschedulable: bool,
    },
    DrainNode(DrainDialog),
    HelmRollback {
        namespace: String,
        release: String,
        revision: String,
    },
    DirDelete {
        path: String,
    },
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum InputMode {
    #[default]
    Browse,
    Command,
    Filter,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Overlay {
    RollbackDialog,
    TransferDialog,
    #[default]
    None,
    Help,
    Aliases,
    Namespaces,
    Contexts,
    Details,
    Content,
    ConfirmDelete,
    PortForwardDialog,
    PortForwards,
    PluginDialog,
    ScreenDumps,
    ImageDialog,
    SanitizeDialog,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum ListMode {
    References {
        title: String,
    },
    #[default]
    Resources,
    Containers {
        namespace: String,
        pod: String,
    },
    Policy {
        title: String,
    },
}

#[derive(Debug)]
struct SavedResourceList {
    snapshot: ResourceSnapshot,
    query: String,
    sort_field: SortField,
    sort_descending: bool,
    column_sort_override: Option<ColumnSortKey>,
    selected_key: Option<Arc<str>>,
}

const NO_CHOOSER_SELECTION: usize = usize::MAX;

#[derive(Debug, Default)]
struct ChooserHitbox {
    x: u16,
    y: u16,
    width: u16,
    indices: Vec<usize>,
}

impl ChooserHitbox {
    fn index_at(&self, column: u16, row: u16) -> Option<usize> {
        let inside_x = column >= self.x && column < self.x.saturating_add(self.width);
        if !inside_x {
            return None;
        }
        let offset = usize::from(row.checked_sub(self.y)?);
        self.indices.get(offset).copied()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UiFlags(u16);

impl Default for UiFlags {
    fn default() -> Self {
        Self(Self::HEADER | Self::LOGO | Self::CRUMBS)
    }
}

impl UiFlags {
    const WIDE: u16 = 1 << 0;
    const HEADER: u16 = 1 << 1;
    const LOGO: u16 = 1 << 2;
    const FAULTS: u16 = 1 << 3;
    const FULLSCREEN: u16 = 1 << 4;
    const WRAP: u16 = 1 << 5;
    const TIMESTAMPS: u16 = 1 << 6;
    const MANAGED_FIELDS: u16 = 1 << 7;
    const SECRET_DECODED: u16 = 1 << 8;
    const CRUMBS: u16 = 1 << 9;
    const INVERT: u16 = 1 << 10;

    fn set(&mut self, flag: u16, enabled: bool) {
        if enabled {
            self.0 |= flag;
        } else {
            self.0 &= !flag;
        }
    }

    #[must_use]
    pub const fn crumbs(self) -> bool {
        self.0 & Self::CRUMBS != 0
    }

    #[must_use]
    pub const fn invert(self) -> bool {
        self.0 & Self::INVERT != 0
    }

    fn toggle(&mut self, flag: u16) {
        self.0 ^= flag;
    }

    #[must_use]
    pub const fn wide(self) -> bool {
        self.0 & Self::WIDE != 0
    }

    #[must_use]
    pub const fn header(self) -> bool {
        self.0 & Self::HEADER != 0
    }

    #[must_use]
    pub const fn logo(self) -> bool {
        self.0 & Self::LOGO != 0
    }

    #[must_use]
    pub const fn faults(self) -> bool {
        self.0 & Self::FAULTS != 0
    }

    #[must_use]
    pub const fn fullscreen(self) -> bool {
        self.0 & Self::FULLSCREEN != 0
    }

    #[must_use]
    pub const fn wrap(self) -> bool {
        self.0 & Self::WRAP != 0
    }

    #[must_use]
    pub const fn timestamps(self) -> bool {
        self.0 & Self::TIMESTAMPS != 0
    }

    #[must_use]
    pub const fn managed_fields(self) -> bool {
        self.0 & Self::MANAGED_FIELDS != 0
    }

    #[must_use]
    const fn secret_decoded(self) -> bool {
        self.0 & Self::SECRET_DECODED != 0
    }
}

fn default_resource_options() -> Vec<ResourceDescriptor> {
    ResourceKind::ALL
        .into_iter()
        .map(ResourceDescriptor::from)
        .chain([
            ResourceDescriptor::workloads(),
            ResourceDescriptor::users(),
            ResourceDescriptor::groups(),
            ResourceDescriptor::helm(),
            ResourceDescriptor::benchmarks(),
            ResourceDescriptor::pulses(),
            ResourceDescriptor::xray(),
            ResourceDescriptor::skins(),
        ])
        .collect()
}

fn normalize_context_options<T: Into<ContextInfo>>(
    contexts: Vec<T>,
    active: &str,
) -> Vec<ContextInfo> {
    let mut contexts = contexts.into_iter().map(Into::into).collect::<Vec<_>>();
    if !contexts.iter().any(|item| item.name == active) {
        contexts.push(ContextInfo::synthetic(active));
    }
    contexts.sort_unstable_by(|left, right| left.name.cmp(&right.name));
    contexts.dedup_by(|left, right| left.name == right.name);
    contexts
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AccessMode {
    ForceReadWrite,
    ReadWrite,
    ReadOnly,
}

#[derive(Debug)]
pub struct App {
    pub instance: Option<ResourceTarget>,
    pub context: String,
    pub server_version: String,
    pub cluster_cpu: String,
    pub cluster_mem: String,
    pub context_options: Vec<ContextInfo>,
    pub namespace: Option<String>,
    pub kind: ResourceDescriptor,
    pub field_selector: Option<String>,
    pub label_selector: Option<String>,
    pub resource_options: Vec<ResourceDescriptor>,
    pub list_mode: ListMode,
    pub generation: u64,
    pub snapshot: ResourceSnapshot,
    pub query: String,
    pub body_query: String,
    pub input: String,
    pub input_mode: InputMode,
    pub overlay: Overlay,
    pub sort_field: SortField,
    pub sort_descending: bool,
    pub selected_column: usize,
    pub(crate) column_sort_override: Option<ColumnSortKey>,
    pub selected: usize,
    pub offset: usize,
    pub list_horizontal_scroll: u16,
    list_max_horizontal_scroll: u16,
    list_column_offsets: Vec<u16>,
    pub viewport_height: usize,
    pub filtered: Vec<usize>,
    pub started_loading_at: Instant,
    pub now_unix: i64,
    pub ui: UiFlags,
    pub marks: HashSet<Arc<str>>,
    pub xray_collapsed: HashSet<Arc<str>>,
    pub mark_anchor: Option<usize>,
    pub history: Vec<ViewState>,
    pub history_cursor: usize,
    pub overlay_selected: usize,
    pub namespace_options: Vec<NamespaceInfo>,
    /// Named namespaces ordered by recency. Key `0` is reserved for all
    /// namespaces, so these entries are addressed by keys `1` through `9`.
    pub recent_namespaces: Vec<String>,
    pub content_title: String,
    pub content_body: String,
    pub content_managed_fields: Option<String>,
    pub content_decoded_body: Option<String>,
    pub content_decoded_managed_fields: Option<String>,
    pub content_kind: Option<ContentKind>,
    pub content_scroll: u16,
    pub content_horizontal_scroll: u16,
    pub log_view: Option<LogView>,
    pub log_view_width: u16,
    pub details_row: Option<ResourceRow>,
    pub notice: Option<String>,
    pub filter_error: Option<String>,
    pub confirm_armed: bool,
    pub pending_delete: Option<PendingDelete>,
    pub port_forward_dialog: Option<PortForwardDialog>,
    pub port_forwards: Vec<PortForwardInfo>,
    pub active_port_forwards: Vec<PortForwardInfo>,
    pub read_only: bool,
    pub user_config: UserConfig,
    pub theme: crate::theme::Skin,
    pub connection: crate::connection::ConnectionOptions,
    pub plugin_dialog: Option<PluginDialog>,
    pub launch: LaunchPreferences,
    pub dumps: ScreenDumpState,
    pub image_editor: ImageEditorState,
    pub rollback: RollbackState,
    pub navigation: NavigationState,
    pub references: ReferencesState,
    pub transfers: TransferState,
    pub sanitize_dialog: Option<SanitizeDialog>,
    cli_access: AccessMode,
    content_max_scroll: u16,
    content_match_cursor: Option<usize>,
    content_target: Option<ResourceTarget>,
    yaml_refresh: LiveRefreshState,
    next_log_id: u64,
    saved_resource_list: Option<SavedResourceList>,
    pending_containers: Option<ResourceTarget>,
    pending_policy: Option<ResourceTarget>,
    pending_child: Option<(ResourceTarget, ResourceRow)>,
    pending_owner: Option<ResourceTarget>,
    column_hitboxes: Vec<ColumnHitbox>,
    resource_rows_hitbox: ResourceRowsHitbox,
    chooser_hitbox: ChooserHitbox,
    completion_seed: Option<String>,
    completion_index: usize,
    input_before_edit: String,
    drain_value_edit_state: DrainValueEditState,
    port_forward_value_edit_state: DrainValueEditState,
}

impl App {
    #[must_use]
    #[allow(clippy::too_many_lines)]
    pub fn new<T>(
        context: impl Into<String>,
        namespace: Option<String>,
        kind: ResourceKind,
        read_only: bool,
        context_options: Vec<T>,
    ) -> Self
    where
        T: Into<ContextInfo>,
    {
        let kind = ResourceDescriptor::from(kind);
        let context = context.into();
        let context_options = normalize_context_options(context_options, &context);
        let recent_namespaces = namespace.iter().cloned().collect();
        let mut app = Self {
            instance: None,
            context,
            server_version: "detecting…".to_owned(),
            cluster_cpu: "n/a".to_owned(),
            cluster_mem: "n/a".to_owned(),
            context_options,
            namespace,
            kind: kind.clone(),
            field_selector: None,
            label_selector: None,
            resource_options: default_resource_options(),
            list_mode: ListMode::Resources,
            generation: 1,
            snapshot: ResourceSnapshot::loading(1, kind),
            query: String::new(),
            body_query: String::new(),
            input: String::new(),
            input_mode: InputMode::Browse,
            overlay: Overlay::None,
            sort_field: SortField::Name,
            sort_descending: false,
            selected_column: 1,
            column_sort_override: None,
            selected: 0,
            offset: 0,
            list_horizontal_scroll: 0,
            list_max_horizontal_scroll: 0,
            list_column_offsets: Vec::new(),
            viewport_height: 1,
            filtered: Vec::new(),
            started_loading_at: Instant::now(),
            now_unix: unix_now(),
            ui: UiFlags::default(),
            marks: HashSet::new(),
            xray_collapsed: HashSet::new(),
            mark_anchor: None,
            history: Vec::new(),
            history_cursor: 0,
            overlay_selected: 0,
            namespace_options: Vec::new(),
            recent_namespaces,
            content_title: String::new(),
            content_body: String::new(),
            content_managed_fields: None,
            content_decoded_body: None,
            content_decoded_managed_fields: None,
            content_kind: None,
            content_scroll: 0,
            content_horizontal_scroll: 0,
            log_view: None,
            log_view_width: 80,
            details_row: None,
            notice: None,
            filter_error: None,
            confirm_armed: false,
            pending_delete: None,
            port_forward_dialog: None,
            port_forwards: Vec::new(),
            active_port_forwards: Vec::new(),
            read_only,
            user_config: UserConfig::default(),
            theme: crate::theme::Skin::default(),
            connection: crate::connection::ConnectionOptions::default(),
            plugin_dialog: None,
            launch: LaunchPreferences::default(),
            dumps: ScreenDumpState::default(),
            image_editor: ImageEditorState::default(),
            rollback: RollbackState::default(),
            navigation: NavigationState::default(),
            references: ReferencesState::default(),
            transfers: TransferState::default(),
            sanitize_dialog: None,
            cli_access: if read_only {
                AccessMode::ReadOnly
            } else {
                AccessMode::ReadWrite
            },
            content_max_scroll: 0,
            content_match_cursor: None,
            content_target: None,
            yaml_refresh: LiveRefreshState::new(),
            next_log_id: 0,
            saved_resource_list: None,
            pending_containers: None,
            pending_policy: None,
            pending_child: None,
            pending_owner: None,
            column_hitboxes: Vec::new(),
            resource_rows_hitbox: ResourceRowsHitbox::default(),
            chooser_hitbox: ChooserHitbox::default(),
            completion_seed: None,
            completion_index: 0,
            input_before_edit: String::new(),
            drain_value_edit_state: DrainValueEditState::Pristine,
            port_forward_value_edit_state: DrainValueEditState::Pristine,
        };
        app.history.push(app.current_view());
        app
    }

    pub fn apply_snapshot(&mut self, snapshot: ResourceSnapshot) {
        if snapshot.generation != self.generation || snapshot.kind != self.kind {
            return;
        }
        if self.list_mode != ListMode::Resources {
            if let Some(saved) = &mut self.saved_resource_list {
                saved.snapshot = snapshot;
            }
            return;
        }
        let selected_key = self
            .restored_workload_selection(&snapshot)
            .or_else(|| self.selected_row().map(|row| Arc::clone(&row.key)));
        self.snapshot = snapshot;
        self.rebuild_filter(selected_key.as_deref());
    }

    pub fn apply_user_config(&mut self, mut config: UserConfig) {
        self.apply_launch_preferences(&mut config);
        self.apply_display_preferences(&config);
        self.theme = config.theme.clone();
        self.read_only = match self.cli_access {
            AccessMode::ForceReadWrite => false,
            AccessMode::ReadOnly => true,
            AccessMode::ReadWrite => config.settings.read_only,
        };
        if config.settings.ui.headless != self.user_config.settings.ui.headless {
            self.ui.0 = (self.ui.0 & !UiFlags::HEADER)
                | if config.settings.ui.headless {
                    0
                } else {
                    UiFlags::HEADER
                };
        }
        if config.settings.ui.logoless != self.user_config.settings.ui.logoless {
            self.set_logo_visible(!config.settings.ui.logoless);
        }
        if self.read_only
            && (matches!(
                self.overlay,
                Overlay::ConfirmDelete | Overlay::PortForwardDialog
            ) || (self.overlay == Overlay::PluginDialog
                && self
                    .plugin_dialog
                    .as_ref()
                    .is_some_and(|dialog| dialog.plugin.dangerous)))
        {
            self.pending_delete = None;
            self.port_forward_dialog = None;
            self.plugin_dialog = None;
            self.confirm_armed = false;
            self.overlay = Overlay::None;
        }
        if self.read_only {
            self.dumps.delete_pending = None;
            self.close_image_editor();
            self.close_rollback();
            self.close_transfer();
            self.close_sanitize_dialog();
        }
        if !config.diagnostics.is_empty() {
            self.show_transient_notice(
                config
                    .diagnostics
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join("; "),
            );
        }
        self.user_config = config;
        self.apply_custom_view_sort();
        self.reset_completion();
        let selected = self.selected_row().map(|row| Arc::clone(&row.key));
        self.rebuild_filter(selected.as_deref());
    }

    fn apply_custom_view_sort(&mut self) {
        let Some((col, asc)) = self
            .user_config
            .custom_view_for(&self.kind, self.namespace.as_deref())
            .and_then(|view| view.sort_column.as_ref())
        else {
            return;
        };
        if let Some(field) = SortField::ALL
            .into_iter()
            .find(|field| field.label().eq_ignore_ascii_case(col))
        {
            self.column_sort_override = None;
            self.sort_field = field;
            self.sort_descending = !*asc;
        }
    }

    pub fn begin_config_load(&mut self) {
        // A new context's write policy must be known before accepting mutations.
        self.read_only = true;
        self.close_image_editor();
        self.close_rollback();
        self.close_transfer();
        self.close_sanitize_dialog();
        self.dumps = ScreenDumpState::default();
        self.user_config.aliases.clear();
        self.user_config.hotkeys.clear();
        self.user_config.views.clear();
        self.user_config.jumps.clear();
        self.user_config.benchmarks = crate::config::BenchmarksConfig::default();
        self.user_config.plugins = crate::plugins::PluginCatalog::default();
        self.pending_delete = None;
        self.port_forward_dialog = None;
        self.plugin_dialog = None;
        self.confirm_armed = false;
        if matches!(
            self.overlay,
            Overlay::ConfirmDelete | Overlay::PortForwardDialog | Overlay::PluginDialog
        ) {
            self.overlay = Overlay::None;
        }
    }

    pub fn run_command(&mut self, command: &str) -> AppCommand {
        command.clone_into(&mut self.input);
        self.execute_command()
    }

    pub fn tick(&mut self) {
        self.now_unix = unix_now();
    }

    #[must_use]
    pub fn namespace_display(&self) -> &str {
        self.namespace.as_deref().unwrap_or("all")
    }

    #[must_use]
    pub fn selected_row(&self) -> Option<&ResourceRow> {
        self.filtered
            .get(self.selected)
            .and_then(|index| self.snapshot.rows.get(*index))
            .map(AsRef::as_ref)
    }

    #[must_use = "iterators are lazy and do nothing unless consumed"]
    pub fn visible_rows(&self) -> impl Iterator<Item = (usize, &ResourceRow)> {
        let end = self
            .offset
            .saturating_add(self.viewport_height)
            .min(self.filtered.len());
        self.filtered[self.offset..end]
            .iter()
            .enumerate()
            .filter_map(|(visible_index, row_index)| {
                self.snapshot
                    .rows
                    .get(*row_index)
                    .map(|row| (self.offset + visible_index, row.as_ref()))
            })
    }

    pub fn set_viewport_height(&mut self, height: usize) {
        self.viewport_height = height.max(1);
        self.ensure_selection_visible();
    }

    pub fn set_logo_visible(&mut self, visible: bool) {
        if self.ui.logo() != visible {
            self.ui.toggle(UiFlags::LOGO);
        }
    }

    /// Returns metadata for the active context.
    ///
    /// # Panics
    ///
    /// Panics if the active context invariant is broken and its metadata is
    /// absent from `context_options`.
    #[must_use]
    pub fn active_context_info(&self) -> &ContextInfo {
        self.context_options
            .iter()
            .find(|item| item.name == self.context)
            .expect("active context is always present in context options")
    }

    pub fn set_context_options(&mut self, mut contexts: Vec<ContextInfo>) {
        if !contexts.iter().any(|item| item.name == self.context) {
            contexts.push(ContextInfo::synthetic(self.context.clone()));
        }
        contexts.sort_unstable_by(|left, right| left.name.cmp(&right.name));
        contexts.dedup_by(|left, right| left.name == right.name);
        self.context_options = contexts;
        self.overlay_selected = self
            .context_options
            .iter()
            .position(|item| item.name == self.context)
            .unwrap_or(0);
    }

    pub fn apply_context_options(
        &mut self,
        context: &str,
        contexts: Vec<ContextInfo>,
        notice: impl Into<String>,
    ) {
        context.clone_into(&mut self.context);
        for entry in &mut self.history {
            if !contexts.iter().any(|c| c.name == entry.context) {
                context.clone_into(&mut entry.context);
            }
        }
        let selected = self.overlay_selected;
        self.set_context_options(contexts);
        self.overlay_selected = selected.min(self.context_options.len().saturating_sub(1));
        self.overlay = Overlay::Contexts;
        self.show_transient_notice(notice);
    }

    pub fn set_namespace_options<T>(&mut self, context: &str, namespaces: Vec<T>)
    where
        T: Into<NamespaceInfo>,
    {
        if self.context != context {
            return;
        }
        let mut options = namespaces
            .into_iter()
            .map(Into::into)
            .filter(|namespace| namespace.name != "<all>")
            .collect::<Vec<_>>();
        options.sort_unstable_by(|left, right| left.name.cmp(&right.name));
        options.dedup_by(|left, right| left.name == right.name);
        self.namespace_options = std::iter::once(NamespaceInfo::all())
            .chain(options)
            .collect();
    }

    pub fn set_server_version(&mut self, version: impl Into<String>) {
        self.server_version = version.into();
    }

    pub fn set_cluster_metrics(&mut self, cpu: impl Into<String>, mem: impl Into<String>) {
        self.cluster_cpu = cpu.into();
        self.cluster_mem = mem.into();
    }

    pub fn set_resource_options(&mut self, mut resources: Vec<ResourceDescriptor>) {
        if resources.is_empty() {
            resources = default_resource_options();
        } else {
            resources.extend([
                ResourceDescriptor::workloads(),
                ResourceDescriptor::users(),
                ResourceDescriptor::groups(),
                ResourceDescriptor::helm(),
                ResourceDescriptor::benchmarks(),
                ResourceDescriptor::pulses(),
                ResourceDescriptor::xray(),
                ResourceDescriptor::skins(),
            ]);
        }
        resources.sort_unstable_by(|left, right| {
            left.plural
                .cmp(&right.plural)
                .then(left.group.cmp(&right.group))
                .then(left.version.cmp(&right.version))
        });
        resources.dedup_by(|left, right| {
            left.group == right.group
                && left.version == right.version
                && left.plural == right.plural
        });
        self.resource_options = resources;
        self.overlay_selected = self
            .resource_options
            .iter()
            .position(|resource| resource == &self.kind)
            .unwrap_or(0);
    }

    #[must_use]
    pub fn is_container_view(&self) -> bool {
        matches!(self.list_mode, ListMode::Containers { .. })
    }

    #[must_use]
    pub fn is_policy_view(&self) -> bool {
        matches!(self.list_mode, ListMode::Policy { .. })
    }

    #[must_use]
    pub fn is_resource_view(&self) -> bool {
        self.list_mode == ListMode::Resources
    }

    #[must_use]
    pub fn is_restartable_workload(&self) -> bool {
        self.is_resource_view()
            && self
                .kind
                .known_kind()
                .is_some_and(ResourceKind::is_restartable)
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> AppCommand {
        if self.overlay == Overlay::RollbackDialog {
            return self.handle_rollback_key(key);
        }
        if self.overlay == Overlay::TransferDialog {
            return self.handle_transfer_key(key);
        }
        if self.overlay == Overlay::SanitizeDialog {
            return self.handle_sanitize_key(key);
        }
        if self.overlay == Overlay::ImageDialog {
            return self.handle_image_key(key);
        }
        if self.overlay == Overlay::PluginDialog {
            return self.handle_plugin_dialog(key);
        }
        if self.overlay == Overlay::ScreenDumps && self.dumps.delete_pending.is_some() {
            return self.handle_screen_dumps_key(key);
        }
        if let Some(plugin) = self.plugin_for_key(key).cloned() {
            return self.activate_plugin(plugin);
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return if self.user_config.settings.no_exit_on_ctrl_c {
                AppCommand::None
            } else {
                AppCommand::Quit
            };
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('e') {
            self.ui.toggle(UiFlags::HEADER);
            return AppCommand::None;
        }
        if self.input_mode != InputMode::Browse {
            return self.handle_input_key(key);
        }
        if self.overlay == Overlay::None
            && let Some(command) = self.reference_key(key)
        {
            return command;
        }
        if key.code == KeyCode::Char('s')
            && key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(
                self.overlay,
                Overlay::None
                    | Overlay::Content
                    | Overlay::Aliases
                    | Overlay::Namespaces
                    | Overlay::Contexts
                    | Overlay::PortForwards
                    | Overlay::ScreenDumps
            )
        {
            return self.export_view();
        }
        if !matches!(
            self.overlay,
            Overlay::ConfirmDelete | Overlay::PortForwardDialog
        ) && let Some(hotkey) = self.user_config.hotkey_for(key)
        {
            self.input = hotkey.command.clone();
            return self.execute_command();
        }
        if !matches!(
            self.overlay,
            Overlay::ConfirmDelete | Overlay::PortForwardDialog | Overlay::PortForwards
        ) && !key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            match key.code {
                KeyCode::Char(':') => return self.start_input(InputMode::Command),
                KeyCode::Char('/') => return self.start_input(InputMode::Filter),
                KeyCode::Char(number) if number.is_ascii_digit() && !self.is_logs_view() => {
                    return self.switch_recent_namespace(number);
                }
                KeyCode::Char('[' | '-') => return self.navigate_history(-1),
                KeyCode::Char(']') => return self.navigate_history(1),
                _ => {}
            }
        }
        if self.overlay != Overlay::None {
            return self.handle_overlay_key(key);
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            return self.handle_control_key(key);
        }
        if key.modifiers.contains(KeyModifiers::SHIFT) {
            let command = self.handle_shift_key(key);
            if command != AppCommand::None {
                return command;
            }
        }

        self.handle_browse_key(key)
    }

    pub fn handle_mouse(&mut self, event: MouseEvent) -> bool {
        if self.input_mode != InputMode::Browse
            || event.kind != MouseEventKind::Down(MouseButton::Left)
        {
            return false;
        }
        if matches!(
            self.overlay,
            Overlay::Aliases | Overlay::Namespaces | Overlay::Contexts | Overlay::ScreenDumps
        ) {
            let Some(index) = self.chooser_hitbox.index_at(event.column, event.row) else {
                return false;
            };
            self.overlay_selected = index;
            return true;
        }
        if let Some(selected) = self.resource_rows_hitbox.index_at(event.column, event.row) {
            self.select(selected);
            return true;
        }
        let Some(hitbox) = self
            .column_hitboxes
            .iter()
            .find(|hitbox| hitbox.contains(event.column, event.row))
            .cloned()
        else {
            return false;
        };
        if matches!(
            self.overlay,
            Overlay::Aliases
                | Overlay::Namespaces
                | Overlay::Contexts
                | Overlay::PortForwards
                | Overlay::ScreenDumps
        ) {
            self.sort_overlay_by_column(&hitbox.key);
            return true;
        }
        if self.overlay != Overlay::None {
            return false;
        }
        self.set_column_sort(hitbox.key);
        true
    }

    pub(crate) fn set_column_hitboxes(&mut self, hitboxes: Vec<ColumnHitbox>) {
        self.column_hitboxes = hitboxes;
    }

    pub(crate) fn set_resource_rows_hitbox(
        &mut self,
        x: u16,
        y: u16,
        width: u16,
        first: usize,
        count: usize,
    ) {
        self.resource_rows_hitbox = ResourceRowsHitbox {
            x,
            y,
            width,
            first,
            count,
        };
    }

    pub(crate) fn set_chooser_hitbox(&mut self, x: u16, y: u16, width: u16, indices: Vec<usize>) {
        self.chooser_hitbox = ChooserHitbox {
            x,
            y,
            width,
            indices,
        };
    }

    pub(crate) fn clear_chooser_hitbox(&mut self) {
        self.chooser_hitbox = ChooserHitbox::default();
    }

    pub(crate) fn set_list_scroll_extent(
        &mut self,
        width: u16,
        viewport_width: u16,
        column_offsets: Vec<u16>,
    ) {
        self.list_max_horizontal_scroll = width.saturating_sub(viewport_width);
        self.list_horizontal_scroll = self
            .list_horizontal_scroll
            .min(self.list_max_horizontal_scroll);
        self.list_column_offsets = column_offsets;
    }

    fn scroll_list_left(&mut self) {
        if self.list_horizontal_scroll == 0 {
            return;
        }
        let target = self
            .list_column_offsets
            .iter()
            .copied()
            .rfind(|offset| *offset < self.list_horizontal_scroll)
            .unwrap_or(0);
        self.list_horizontal_scroll = target;
    }

    fn scroll_list_right(&mut self) {
        if self.list_horizontal_scroll >= self.list_max_horizontal_scroll {
            return;
        }
        let target = self
            .list_column_offsets
            .iter()
            .copied()
            .find(|offset| *offset > self.list_horizontal_scroll)
            .unwrap_or(self.list_max_horizontal_scroll)
            .min(self.list_max_horizontal_scroll);
        self.list_horizontal_scroll = target;
    }

    fn handle_browse_key(&mut self, key: KeyEvent) -> AppCommand {
        if self.kind.is_pulses()
            && self.is_resource_view()
            && let Some(command) = self.handle_pulse_key(key)
        {
            return command;
        }
        match key.code {
            KeyCode::Char('?') => self.open_overlay(Overlay::Help),
            KeyCode::Char(':') => self.start_input(InputMode::Command),
            KeyCode::Char('/') => self.start_input(InputMode::Filter),
            KeyCode::Esc | KeyCode::Char('h') => self.escape_view(),
            KeyCode::Enter => self.open_selected(),
            KeyCode::Left => {
                self.scroll_list_left();
                AppCommand::None
            }
            KeyCode::Right => {
                self.scroll_list_right();
                AppCommand::None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.move_selection_down(1);
                AppCommand::None
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.move_selection_up(1);
                AppCommand::None
            }
            KeyCode::PageDown => {
                self.move_selection_down(self.viewport_height);
                AppCommand::None
            }
            KeyCode::PageUp => {
                self.move_selection_up(self.viewport_height);
                AppCommand::None
            }
            KeyCode::Home | KeyCode::Char('g') => {
                self.select(0);
                AppCommand::None
            }
            KeyCode::End | KeyCode::Char('G') => {
                self.select(self.filtered.len().saturating_sub(1));
                AppCommand::None
            }
            KeyCode::Char('n') if !self.query.is_empty() => {
                self.cycle_filtered_selection(true);
                AppCommand::None
            }
            KeyCode::Char(' ') => {
                self.toggle_mark();
                AppCommand::None
            }
            _ => self.handle_resource_key(key),
        }
    }

    fn handle_resource_key(&mut self, key: KeyEvent) -> AppCommand {
        if self.kind.is_workloads()
            && matches!(key.code, KeyCode::Char('l' | 'p' | 'i' | 's' | 'r' | 'J'))
        {
            return AppCommand::None;
        }
        if self.kind.is_subject() && matches!(key.code, KeyCode::Char('y' | 'd' | 'e' | 'J')) {
            return AppCommand::None;
        }
        if self.is_resource_view()
            && let Some(command) = self.handle_virtual_resource_key(key)
        {
            return command;
        }
        let known_kind = self.kind.known_kind();
        match key.code {
            KeyCode::Char('i') if self.is_resource_view() => self.open_image_editor(),
            KeyCode::Char('t')
                if self.is_resource_view()
                    && self.kind.known_kind() == Some(ResourceKind::Pods) =>
            {
                self.open_transfer()
            }
            KeyCode::Char('z')
                if self.is_resource_view() && known_kind == Some(ResourceKind::Pods) =>
            {
                self.open_sanitize_dialog()
            }
            KeyCode::Char('z')
                if self.is_resource_view() && known_kind == Some(ResourceKind::Deployments) =>
            {
                self.open_deployment_replica_sets()
            }
            KeyCode::Char('c')
                if self.is_resource_view() && known_kind == Some(ResourceKind::Nodes) =>
            {
                self.open_node_cordon_confirmation(true)
            }
            KeyCode::Char('c') => self.copy_name(),
            KeyCode::Char('n') => self.copy_namespace(),
            KeyCode::Char('y') if self.is_resource_view() => self.content_action(ContentKind::Yaml),
            KeyCode::Char('d') if self.is_resource_view() => {
                self.content_action(ContentKind::Describe)
            }
            KeyCode::Char('x') if self.is_resource_view() && self.kind.is_core_v1_secret() => {
                self.content_action(ContentKind::SecretData)
            }
            KeyCode::Char('l') if self.is_container_view() => self.container_log_action(false),
            KeyCode::Char('p') if self.is_container_view() => self.container_log_action(true),
            KeyCode::Char('l') if self.is_resource_view() && supports_logs(&self.kind) => {
                self.resource_log_action(false)
            }
            KeyCode::Char('l') => self.open_selected(),
            KeyCode::Char('p') if self.is_resource_view() && supports_logs(&self.kind) => {
                self.resource_log_action(true)
            }
            KeyCode::Char('o') if known_kind == Some(ResourceKind::Pods) => self.open_pod_node(),
            KeyCode::Char('s') => self.handle_resource_s_key(known_kind),
            KeyCode::Char('a')
                if self.is_container_view() || known_kind == Some(ResourceKind::Pods) =>
            {
                self.attach_action()
            }
            KeyCode::Char('t') if self.is_resource_view() && self.kind.is_cron_job() => {
                self.open_cronjob_trigger_confirmation()
            }
            KeyCode::Char('e') if self.is_resource_view() => self.edit_action(),
            KeyCode::Char('f')
                if self.is_container_view()
                    || matches!(
                        known_kind,
                        Some(ResourceKind::Pods | ResourceKind::Services)
                    ) =>
            {
                AppCommand::ShowPortForwards
            }
            KeyCode::Char('w') if self.namespace.is_none() && self.kind.is_namespaced() => {
                self.warp_to_selected_namespace()
            }
            KeyCode::Char('u') => self.handle_resource_u_key(known_kind),
            KeyCode::Char('b') if known_kind == Some(ResourceKind::Services) => {
                self.benchmark_service_action()
            }
            KeyCode::Char('r') if self.is_restartable_workload() => {
                self.open_workload_restart_confirmation()
            }
            KeyCode::Char('r') if known_kind == Some(ResourceKind::Nodes) => {
                self.open_node_drain_confirmation()
            }
            KeyCode::Char('v') => self.handle_resource_v_key(),
            KeyCode::Char(number) if number.is_ascii_digit() => {
                self.switch_recent_namespace(number)
            }
            _ => AppCommand::None,
        }
    }

    fn handle_resource_u_key(&mut self, known_kind: Option<ResourceKind>) -> AppCommand {
        if known_kind == Some(ResourceKind::Nodes) {
            self.open_node_scheduling_confirmation()
        } else if self.is_resource_view() && self.kind.is_core_v1_namespace() {
            self.use_selected_namespace_row()
        } else if self.is_resource_view() && crate::source::supports_references(&self.kind) {
            self.open_references()
        } else {
            AppCommand::None
        }
    }

    fn handle_resource_v_key(&mut self) -> AppCommand {
        if self.user_config.settings.image_scans.enable
            && (self.is_container_view()
                || (self.is_resource_view() && self.kind.supports_vulnerability_scans()))
        {
            self.open_vulnerability_scans()
        } else {
            AppCommand::None
        }
    }

    fn handle_virtual_resource_key(&mut self, key: KeyEvent) -> Option<AppCommand> {
        if self.kind.is_dir() {
            Some(self.handle_dir_key(key))
        } else if self.kind.is_benchmarks() {
            Some(self.handle_benchmark_key(key))
        } else if self.kind.is_pulses() {
            Some(AppCommand::None)
        } else if self.kind.is_xray() {
            Some(self.handle_xray_key(key))
        } else if self.kind.is_scans() {
            Some(self.handle_scans_key(key))
        } else if self.kind.is_skins() {
            Some(self.handle_skins_key(key))
        } else if self.kind.is_helm() || self.kind.is_helm_history() {
            Some(self.handle_helm_key(key))
        } else {
            None
        }
    }

    fn handle_pulse_key(&mut self, key: KeyEvent) -> Option<AppCommand> {
        let len = self.filtered.len();
        if len == 0 {
            return None;
        }
        match key.code {
            KeyCode::Tab | KeyCode::Right | KeyCode::Char('l') => {
                self.select((self.selected + 1) % len);
                Some(AppCommand::None)
            }
            KeyCode::BackTab | KeyCode::Left => {
                self.select(self.selected.checked_sub(1).unwrap_or(len - 1));
                Some(AppCommand::None)
            }
            KeyCode::Down | KeyCode::Char('j') => {
                let next = if self.selected + 4 < len {
                    self.selected + 4
                } else {
                    len - 1
                };
                self.select(next);
                Some(AppCommand::None)
            }
            KeyCode::Up | KeyCode::Char('k') => {
                let next = self.selected.saturating_sub(4);
                self.select(next);
                Some(AppCommand::None)
            }
            _ => None,
        }
    }

    fn handle_helm_key(&mut self, key: KeyEvent) -> AppCommand {
        match key.code {
            KeyCode::Char('v') => self.content_action(ContentKind::HelmValues),
            KeyCode::Char('y') => self.content_action(ContentKind::Yaml),
            KeyCode::Char('d') => self.content_action(ContentKind::Describe),
            KeyCode::Char('c') => self.copy_name(),
            KeyCode::Char('n') => self.copy_namespace(),
            KeyCode::Char('r') if self.kind.is_helm() => self.open_helm_releases(),
            KeyCode::Char('r') if self.kind.is_helm_history() => {
                self.open_helm_rollback_confirmation()
            }
            KeyCode::Char('l') => self.open_selected(),
            KeyCode::Char('w') if self.namespace.is_none() && self.kind.is_namespaced() => {
                self.warp_to_selected_namespace()
            }
            KeyCode::Char(number) if number.is_ascii_digit() => {
                self.switch_recent_namespace(number)
            }
            _ => AppCommand::None,
        }
    }

    fn handle_dir_key(&mut self, key: KeyEvent) -> AppCommand {
        match key.code {
            KeyCode::Char('y') => self.content_action(ContentKind::Yaml),
            KeyCode::Char('a') => self.dir_apply_action(),
            KeyCode::Char('d') => self.open_dir_delete_confirmation(),
            KeyCode::Char('e') => self.dir_edit_action(),
            KeyCode::Char('c') => self.copy_name(),
            KeyCode::Char('l') => self.open_selected(),
            KeyCode::Char(number) if number.is_ascii_digit() => {
                self.switch_recent_namespace(number)
            }
            _ => AppCommand::None,
        }
    }

    fn handle_benchmark_key(&mut self, key: KeyEvent) -> AppCommand {
        match key.code {
            KeyCode::Char('y' | 'v' | 'l') => self.content_action(ContentKind::Yaml),
            KeyCode::Char('d') => self.open_delete_confirmation(),
            KeyCode::Char('c') => self.copy_name(),
            KeyCode::Char('n') => self.copy_namespace(),
            _ => AppCommand::None,
        }
    }

    fn handle_scans_key(&mut self, key: KeyEvent) -> AppCommand {
        match key.code {
            KeyCode::Char('c') => self.copy_name(),
            _ => AppCommand::None,
        }
    }

    fn handle_skins_key(&mut self, key: KeyEvent) -> AppCommand {
        match key.code {
            KeyCode::Char('c') => self.copy_name(),
            _ => AppCommand::None,
        }
    }

    fn handle_xray_key(&mut self, key: KeyEvent) -> AppCommand {
        match key.code {
            KeyCode::Char('x') => {
                let Some(xray) = self.selected_row().and_then(ResourceRow::xray_data) else {
                    return AppCommand::None;
                };
                if xray.children_count == 0 {
                    return AppCommand::None;
                }
                let path = Arc::clone(&xray.path);
                if !self.xray_collapsed.remove(&path) {
                    self.xray_collapsed.insert(path.clone());
                }
                self.rebuild_filter(Some(path.as_ref()));
                AppCommand::None
            }
            KeyCode::Char('y') => self.content_action(ContentKind::Yaml),
            KeyCode::Char('d') => self.content_action(ContentKind::Describe),
            KeyCode::Char('e') => self.edit_action(),
            KeyCode::Char('c') => self.copy_name(),
            KeyCode::Char('n') => self.copy_namespace(),
            KeyCode::Char('l') => self.xray_log_action(false),
            KeyCode::Char('p') => self.xray_log_action(true),
            KeyCode::Char('s') => self.xray_shell_action(false),
            KeyCode::Char('a') => self.xray_shell_action(true),
            KeyCode::Char(number) if number.is_ascii_digit() => {
                self.switch_recent_namespace(number)
            }
            _ => AppCommand::None,
        }
    }

    fn xray_log_action(&mut self, previous: bool) -> AppCommand {
        let Some(row) = self.selected_row() else {
            return AppCommand::None;
        };
        let Some(xray) = row.xray_data() else {
            return AppCommand::None;
        };
        if xray.is_container {
            let Some(pod) = xray.pod_name.as_ref() else {
                return AppCommand::None;
            };
            self.open_logs(LogTarget::pod(
                row.namespace.to_string(),
                pod.to_string(),
                Some(row.name.to_string()),
                previous,
            ))
        } else if supports_logs(&xray.target_kind) {
            let Some(target) = self.row_target(row) else {
                return AppCommand::None;
            };
            self.open_logs(LogTarget::workload(target, previous))
        } else {
            AppCommand::None
        }
    }

    fn xray_shell_action(&mut self, attach: bool) -> AppCommand {
        if self.read_only {
            return self.unsupported(if attach {
                "Attach is disabled in read-only mode"
            } else {
                "Shell is disabled in read-only mode"
            });
        }
        let Some(row) = self.selected_row() else {
            return AppCommand::None;
        };
        let Some(xray) = row.xray_data() else {
            return AppCommand::None;
        };
        let (pod, container) = if xray.is_container {
            let Some(pod) = xray.pod_name.as_ref() else {
                return AppCommand::None;
            };
            (pod.to_string(), row.name.to_string())
        } else if xray.target_kind.known_kind() == Some(ResourceKind::Pods) {
            let pod_key = xray.path.as_ref();
            let co = self
                .snapshot
                .rows
                .iter()
                .find_map(|r| {
                    let xr = r.xray_data()?;
                    (xr.is_container && xr.parent_path.as_deref() == Some(pod_key))
                        .then(|| r.name.to_string())
                })
                .unwrap_or_else(|| "default".to_owned());
            (row.name.to_string(), co)
        } else {
            return AppCommand::None;
        };
        let target = PodShellTarget {
            namespace: row.namespace.to_string(),
            pod,
            container,
        };
        if attach {
            AppCommand::Attach(target)
        } else {
            AppCommand::Shell(target)
        }
    }

    fn handle_resource_s_key(&mut self, known_kind: Option<ResourceKind>) -> AppCommand {
        if self.is_container_view() {
            self.container_shell_action()
        } else if known_kind == Some(ResourceKind::Pods) {
            self.pod_shell_action()
        } else if self.is_resource_view()
            && known_kind == Some(ResourceKind::Nodes)
            && self.user_config.node_shell_enabled(&self.context)
        {
            self.node_shell_action()
        } else if self.is_resource_view() && self.kind.is_scalable() {
            self.open_scale_dialog()
        } else if self.is_resource_view() && self.kind.is_cron_job() {
            self.open_suspend_toggle_confirmation()
        } else {
            AppCommand::None
        }
    }

    fn handle_control_key(&mut self, key: KeyEvent) -> AppCommand {
        match key.code {
            KeyCode::Char('a') => self.open_aliases(),
            KeyCode::Char('r') => self.request_view(self.current_view(), false),
            KeyCode::Char('f') => {
                self.move_selection_down(self.viewport_height);
                AppCommand::None
            }
            KeyCode::Char('b') => {
                self.move_selection_up(self.viewport_height);
                AppCommand::None
            }
            KeyCode::Char('d')
                if !self.kind.is_subject()
                    && !self.kind.is_helm_history()
                    && !self.kind.is_dir()
                    && !self.kind.is_pulses()
                    && !self.kind.is_scans() =>
            {
                self.open_delete_confirmation()
            }
            KeyCode::Char('k')
                if !self.kind.is_workloads()
                    && !self.kind.is_subject()
                    && !self.kind.is_helm()
                    && !self.kind.is_helm_history()
                    && !self.kind.is_dir()
                    && !self.kind.is_benchmarks()
                    && !self.kind.is_pulses()
                    && !self.kind.is_scans() =>
            {
                self.immediate_delete()
            }
            KeyCode::Char('z') => {
                self.ui.toggle(UiFlags::FAULTS);
                self.rebuild_filter(None);
                AppCommand::None
            }
            KeyCode::Char('w') => {
                self.ui.toggle(UiFlags::WIDE);
                self.select_workload_sort_column(SortField::ALL[self.selected_column]);
                AppCommand::None
            }
            KeyCode::Char('l') if self.is_resource_view() && self.kind.is_apps_v1_replica_set() => {
                self.open_rollback()
            }
            KeyCode::Char('l') => {
                self.ui.toggle(UiFlags::LOGO);
                AppCommand::None
            }
            KeyCode::Char('\\') => {
                self.marks.clear();
                self.mark_anchor = None;
                self.notice = Some("Marks cleared".to_owned());
                AppCommand::None
            }
            KeyCode::Char(' ') => {
                self.mark_range();
                AppCommand::None
            }
            _ => AppCommand::None,
        }
    }

    fn handle_shift_key(&mut self, key: KeyEvent) -> AppCommand {
        let known_kind = self.kind.known_kind();
        match key.code {
            KeyCode::Left if self.kind.is_workloads() => self.move_workload_sort_column(false),
            KeyCode::Right if self.kind.is_workloads() => self.move_workload_sort_column(true),
            KeyCode::Left => {
                self.selected_column = self.selected_column.saturating_sub(1);
            }
            KeyCode::Right => {
                self.selected_column = (self.selected_column + 1).min(SortField::ALL.len() - 1);
            }
            KeyCode::Char('O') => self.set_sort(SortField::ALL[self.selected_column]),
            KeyCode::Char('N') if !self.query.is_empty() => {
                self.cycle_filtered_selection(false);
            }
            KeyCode::Char('N') => self.set_sort(SortField::Name),
            KeyCode::Char('K') if self.kind.is_workloads() => self.set_sort(SortField::Kind),
            KeyCode::Char('R') if self.kind.is_workloads() => self.set_sort(SortField::Ready),
            KeyCode::Char('A') => self.set_sort(SortField::Age),
            KeyCode::Char('P') => self.set_sort(SortField::Namespace),
            KeyCode::Char('S') => self.set_sort(SortField::Status),
            KeyCode::Char('L') if self.kind.is_scans() => self.set_sort(SortField::Details),
            KeyCode::Char('F') if self.kind.is_scans() => self.set_sort(SortField::Ready),
            KeyCode::Char('V') if self.kind.is_scans() => self.set_sort(SortField::Name),
            KeyCode::Char('V')
                if self.user_config.settings.image_scans.enable
                    && (self.is_container_view()
                        || (self.is_resource_view()
                            && self.kind.supports_vulnerability_scans())) =>
            {
                self.set_sort(SortField::VulnScore);
            }
            KeyCode::Char('F')
                if self.is_container_view()
                    || matches!(
                        known_kind,
                        Some(ResourceKind::Pods | ResourceKind::Services)
                    ) =>
            {
                return self.open_port_forward_dialog();
            }
            KeyCode::Char('J') if !self.kind.is_workloads() && !self.kind.is_subject() => {
                return self.open_owner_resource();
            }
            _ => return AppCommand::None,
        }
        AppCommand::None
    }

    fn handle_input_key(&mut self, key: KeyEvent) -> AppCommand {
        match key.code {
            KeyCode::Esc => {
                if self.input_mode == InputMode::Filter {
                    let previous = self.input_before_edit.clone();
                    self.set_active_filter(previous);
                }
                self.input.clear();
                self.input_mode = InputMode::Browse;
                self.reset_completion();
                AppCommand::None
            }
            KeyCode::Enter => {
                if self.input_mode == InputMode::Command {
                    self.execute_command()
                } else {
                    let filter = self.input.clone();
                    self.set_active_filter(filter);
                    self.input_mode = InputMode::Browse;
                    AppCommand::None
                }
            }
            KeyCode::Backspace => {
                self.input.pop();
                self.reset_completion();
                self.preview_filter();
                AppCommand::None
            }
            KeyCode::Char('u')
                if self.input_mode == InputMode::Filter
                    && key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                self.input.clear();
                self.reset_completion();
                self.preview_filter();
                AppCommand::None
            }
            KeyCode::Tab if self.input_mode == InputMode::Command => {
                self.complete_command(key.modifiers.contains(KeyModifiers::SHIFT));
                AppCommand::None
            }
            KeyCode::Tab if self.input_mode == InputMode::Filter && self.input.is_empty() => {
                self.input.clone_from(&self.input_before_edit);
                self.preview_filter();
                AppCommand::None
            }
            KeyCode::BackTab if self.input_mode == InputMode::Command => {
                self.complete_command(true);
                AppCommand::None
            }
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.input.push(character);
                self.reset_completion();
                self.preview_filter();
                AppCommand::None
            }
            _ => AppCommand::None,
        }
    }

    fn handle_overlay_key(&mut self, key: KeyEvent) -> AppCommand {
        match self.overlay {
            Overlay::Help => self.handle_help_overlay(key),
            Overlay::Aliases => self.handle_alias_overlay(key),
            Overlay::Namespaces => self.handle_namespace_overlay(key),
            Overlay::Contexts => self.handle_context_overlay(key),
            Overlay::ConfirmDelete => self.handle_confirm_overlay(key),
            Overlay::Content => self.handle_content_overlay(key),
            Overlay::Details => self.handle_details_overlay(key),
            Overlay::PortForwardDialog => self.handle_port_forward_dialog(key),
            Overlay::PortForwards => self.handle_port_forwards_overlay(key),
            Overlay::PluginDialog => self.handle_plugin_dialog(key),
            Overlay::ScreenDumps => self.handle_screen_dumps_key(key),
            Overlay::ImageDialog => self.handle_image_key(key),
            Overlay::RollbackDialog => self.handle_rollback_key(key),
            Overlay::TransferDialog => self.handle_transfer_key(key),
            Overlay::SanitizeDialog => self.handle_sanitize_key(key),
            Overlay::None => self.close_overlay_on(key),
        }
    }

    fn open_port_forward_dialog(&mut self) -> AppCommand {
        if self.read_only {
            return self.unsupported("Port forwarding is disabled in read-only mode");
        }
        let default_addr = self
            .user_config
            .settings
            .port_forward_address
            .clone()
            .or_else(|| {
                std::env::var("K9S_DEFAULT_PF_ADDRESS")
                    .ok()
                    .filter(|s| !s.trim().is_empty())
            })
            .unwrap_or_else(|| "localhost".to_owned());
        if self.is_container_view() {
            let ListMode::Containers { namespace, pod } = &self.list_mode else {
                return AppCommand::None;
            };
            let Some(row) = self.selected_row() else {
                return AppCommand::None;
            };
            let target = ResourceTarget {
                kind: ResourceDescriptor::from(ResourceKind::Pods),
                namespace: Some(namespace.clone()),
                name: pod.clone(),
            };
            let ports = row.port_forward_ports().to_vec();
            self.port_forward_dialog =
                Some(PortForwardDialog::new(target, ports, &default_addr, None));
            self.overlay_selected = PORT_FORWARD_REMOTE_FIELD;
            self.port_forward_value_edit_state = DrainValueEditState::Pristine;
            self.notice = None;
            self.overlay = Overlay::PortForwardDialog;
            return AppCommand::None;
        }
        let Some(target) = self.selected_target() else {
            return AppCommand::None;
        };
        let (ports, auto_ann, pref_ann) = self.selected_row().map_or_else(
            || (Vec::new(), None, None),
            |row| {
                let ports = row.port_forward_ports().to_vec();
                let auto_ann = row
                    .raw_object
                    .as_deref()
                    .and_then(|v| v.pointer("/metadata/annotations/k9scli.io~1auto-port-forwards"))
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned);
                let pref_ann = row
                    .raw_object
                    .as_deref()
                    .and_then(|v| v.pointer("/metadata/annotations/k9scli.io~1port-forwards"))
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned);
                (ports, auto_ann, pref_ann)
            },
        );
        if let Some(spec) = auto_ann.as_deref()
            && let Some((remote_sel, local_str)) = resolve_pf_annotation(spec, &ports)
        {
            let dialog = PortForwardDialog {
                target: target.clone(),
                ports: ports.clone(),
                remote_port: remote_sel,
                local_port: local_str,
                address: default_addr.clone(),
            };
            if let Ok(req) = dialog.request() {
                return AppCommand::StartPortForward(req);
            }
        }
        self.port_forward_dialog = Some(PortForwardDialog::new(
            target,
            ports,
            &default_addr,
            pref_ann.as_deref(),
        ));
        self.overlay_selected = PORT_FORWARD_REMOTE_FIELD;
        self.port_forward_value_edit_state = DrainValueEditState::Pristine;
        self.notice = None;
        self.overlay = Overlay::PortForwardDialog;
        AppCommand::None
    }

    fn handle_port_forward_dialog(&mut self, key: KeyEvent) -> AppCommand {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q' | '?') => self.cancel_port_forward_dialog(),
            KeyCode::Tab | KeyCode::Down => {
                self.move_port_forward_field(1);
                AppCommand::None
            }
            KeyCode::BackTab | KeyCode::Up => {
                self.move_port_forward_field(-1);
                AppCommand::None
            }
            KeyCode::Backspace if self.editable_port_forward_field() => {
                let field = self.overlay_selected;
                if let Some(dialog) = self.port_forward_dialog.as_mut() {
                    port_forward_field_mut(dialog, field).pop();
                }
                self.port_forward_value_edit_state = DrainValueEditState::Editing;
                self.notice = None;
                AppCommand::None
            }
            KeyCode::Enter => self.activate_port_forward_field(),
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                    && self.edit_port_forward_value(character) =>
            {
                AppCommand::None
            }
            _ => AppCommand::None,
        }
    }

    fn move_port_forward_field(&mut self, direction: isize) {
        self.overlay_selected = if direction < 0 {
            self.overlay_selected
                .checked_sub(1)
                .unwrap_or(PORT_FORWARD_FIELD_COUNT - 1)
        } else {
            (self.overlay_selected + 1) % PORT_FORWARD_FIELD_COUNT
        };
        self.port_forward_value_edit_state = DrainValueEditState::Pristine;
        self.notice = None;
    }

    fn editable_port_forward_field(&self) -> bool {
        matches!(
            self.overlay_selected,
            PORT_FORWARD_REMOTE_FIELD | PORT_FORWARD_LOCAL_FIELD | PORT_FORWARD_ADDRESS_FIELD
        )
    }

    fn edit_port_forward_value(&mut self, character: char) -> bool {
        let field = self.overlay_selected;
        let accepted = match field {
            PORT_FORWARD_REMOTE_FIELD => {
                character.is_ascii_alphanumeric() || matches!(character, ':' | '-' | '_' | '.')
            }
            PORT_FORWARD_LOCAL_FIELD => character.is_ascii_digit(),
            PORT_FORWARD_ADDRESS_FIELD => {
                character.is_ascii_alphanumeric()
                    || matches!(character, '.' | ',' | ':' | '-' | '[' | ']')
            }
            _ => false,
        };
        if !accepted {
            return false;
        }
        let replace = self.port_forward_value_edit_state == DrainValueEditState::Pristine;
        if let Some(dialog) = self.port_forward_dialog.as_mut() {
            let value = port_forward_field_mut(dialog, field);
            if replace {
                value.clear();
            }
            value.push(character);
        }
        self.port_forward_value_edit_state = DrainValueEditState::Editing;
        self.notice = None;
        true
    }

    fn activate_port_forward_field(&mut self) -> AppCommand {
        match self.overlay_selected {
            PORT_FORWARD_REMOTE_FIELD | PORT_FORWARD_LOCAL_FIELD | PORT_FORWARD_ADDRESS_FIELD => {
                self.move_port_forward_field(1);
                AppCommand::None
            }
            PORT_FORWARD_OK_FIELD => {
                let request = self
                    .port_forward_dialog
                    .as_ref()
                    .map(PortForwardDialog::request);
                match request {
                    Some(Ok(request)) => {
                        self.port_forward_dialog = None;
                        self.overlay = Overlay::None;
                        self.notice = None;
                        AppCommand::StartPortForward(request)
                    }
                    Some(Err(error)) => {
                        self.notice = Some(error);
                        AppCommand::None
                    }
                    None => AppCommand::None,
                }
            }
            PORT_FORWARD_CANCEL_FIELD => self.cancel_port_forward_dialog(),
            _ => AppCommand::None,
        }
    }

    fn cancel_port_forward_dialog(&mut self) -> AppCommand {
        self.port_forward_dialog = None;
        self.notice = None;
        self.overlay = Overlay::None;
        AppCommand::None
    }

    fn handle_port_forwards_overlay(&mut self, key: KeyEvent) -> AppCommand {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('d') {
            return self
                .port_forwards
                .get(self.overlay_selected)
                .map_or(AppCommand::None, |forward| {
                    AppCommand::StopPortForward(forward.id)
                });
        }
        match key.code {
            KeyCode::Esc | KeyCode::Char('q' | 'f') => {
                self.port_forwards.clear();
                self.notice = None;
                self.overlay = Overlay::None;
                AppCommand::None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.overlay_selected = self
                    .overlay_selected
                    .saturating_add(1)
                    .min(self.port_forwards.len().saturating_sub(1));
                AppCommand::None
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.overlay_selected = self.overlay_selected.saturating_sub(1);
                AppCommand::None
            }
            KeyCode::Home | KeyCode::Char('g') => {
                self.overlay_selected = 0;
                AppCommand::None
            }
            KeyCode::End | KeyCode::Char('G') => {
                self.overlay_selected = self.port_forwards.len().saturating_sub(1);
                AppCommand::None
            }
            KeyCode::Char('b') => self.benchmark_selected_port_forward(),
            KeyCode::Char('P') if key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.port_forwards
                    .sort_by_key(|f| (f.local_port, f.remote_port));
                AppCommand::None
            }
            KeyCode::Char('U') if key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.port_forwards
                    .sort_by(|a, b| (&a.address, a.local_port).cmp(&(&b.address, b.local_port)));
                AppCommand::None
            }
            KeyCode::Enter => self.open_selected_port_forward_benchmarks(),
            _ => AppCommand::None,
        }
    }

    fn move_chooser_selection(&mut self, visible: &[usize], amount: usize, down: bool) {
        if visible.is_empty() {
            self.overlay_selected = NO_CHOOSER_SELECTION;
            return;
        }
        let position = visible
            .iter()
            .position(|index| *index == self.overlay_selected)
            .unwrap_or(0);
        let position = if down {
            position.saturating_add(amount).min(visible.len() - 1)
        } else {
            position.saturating_sub(amount)
        };
        self.overlay_selected = visible[position];
    }

    fn handle_chooser_navigation(&mut self, key: &KeyEvent, visible: &[usize]) -> bool {
        match key.code {
            KeyCode::Char('n') if !self.body_query.is_empty() => {
                self.cycle_chooser_selection(visible, true);
            }
            KeyCode::Char('N')
                if !self.body_query.is_empty() && key.modifiers.contains(KeyModifiers::SHIFT) =>
            {
                self.cycle_chooser_selection(visible, false);
            }
            KeyCode::Down | KeyCode::Char('j') => self.move_chooser_selection(visible, 1, true),
            KeyCode::Up | KeyCode::Char('k') => self.move_chooser_selection(visible, 1, false),
            KeyCode::PageDown => {
                self.move_chooser_selection(visible, self.viewport_height, true);
            }
            KeyCode::Char('f') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.move_chooser_selection(visible, self.viewport_height, true);
            }
            KeyCode::PageUp => {
                self.move_chooser_selection(visible, self.viewport_height, false);
            }
            KeyCode::Char('b') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.move_chooser_selection(visible, self.viewport_height, false);
            }
            KeyCode::Home | KeyCode::Char('g') => {
                self.overlay_selected = visible.first().copied().unwrap_or(NO_CHOOSER_SELECTION);
            }
            KeyCode::End | KeyCode::Char('G') => {
                self.overlay_selected = visible.last().copied().unwrap_or(NO_CHOOSER_SELECTION);
            }
            _ => return false,
        }
        true
    }

    fn cycle_chooser_selection(&mut self, visible: &[usize], forward: bool) {
        if visible.is_empty() {
            self.overlay_selected = NO_CHOOSER_SELECTION;
            return;
        }
        let position = visible
            .iter()
            .position(|index| *index == self.overlay_selected)
            .unwrap_or(0);
        let next = if forward {
            (position + 1) % visible.len()
        } else {
            position.checked_sub(1).unwrap_or(visible.len() - 1)
        };
        self.overlay_selected = visible[next];
    }

    fn handle_alias_overlay(&mut self, key: KeyEvent) -> AppCommand {
        let visible = self.visible_resource_indices();
        if self.handle_chooser_navigation(&key, &visible) {
            return AppCommand::None;
        }
        match key.code {
            KeyCode::Enter | KeyCode::Char('l') => {
                let Some(index) = visible
                    .iter()
                    .copied()
                    .find(|index| *index == self.overlay_selected)
                else {
                    return AppCommand::None;
                };
                let Some(kind) = self.resource_options.get(index).cloned() else {
                    return AppCommand::None;
                };
                let namespace = kind
                    .is_namespaced()
                    .then(|| self.namespace.clone())
                    .flatten();
                self.overlay = Overlay::None;
                self.request_view(
                    ViewState {
                        instance: None,
                        context: self.context.clone(),
                        kind,
                        namespace,
                        field_selector: None,
                        label_selector: None,
                    },
                    true,
                )
            }
            _ => self.close_overlay_on(key),
        }
    }

    fn handle_namespace_overlay(&mut self, key: KeyEvent) -> AppCommand {
        let visible = self.visible_namespace_indices();
        if self.handle_chooser_navigation(&key, &visible) {
            return AppCommand::None;
        }
        match key.code {
            KeyCode::Char('y') => self.namespace_content_action(&visible),
            KeyCode::Char('e') => self.namespace_edit_action(&visible),
            KeyCode::Char('u') => {
                let Some(index) = visible
                    .iter()
                    .copied()
                    .find(|index| *index == self.overlay_selected)
                else {
                    return AppCommand::None;
                };
                let namespace = self
                    .namespace_options
                    .get(index)
                    .map(|value| value.name.clone())
                    .and_then(|value| (value != "<all>").then_some(value));
                self.use_active_namespace(namespace);
                AppCommand::None
            }
            KeyCode::Enter | KeyCode::Char('l') => {
                let Some(index) = visible
                    .iter()
                    .copied()
                    .find(|index| *index == self.overlay_selected)
                else {
                    return AppCommand::None;
                };
                let namespace = self
                    .namespace_options
                    .get(index)
                    .map(|value| value.name.clone())
                    .and_then(|value| (value != "<all>").then_some(value));
                self.overlay = Overlay::None;
                self.request_view(
                    ViewState {
                        instance: None,
                        context: self.context.clone(),
                        kind: self.kind.clone(),
                        namespace,
                        field_selector: self.field_selector.clone(),
                        label_selector: self.label_selector.clone(),
                    },
                    true,
                )
            }
            _ => self.close_overlay_on(key),
        }
    }

    fn namespace_content_action(&mut self, visible: &[usize]) -> AppCommand {
        let Some(target) = self.selected_namespace_target(visible) else {
            return AppCommand::None;
        };
        self.request_content(target, ContentKind::Yaml)
    }

    fn namespace_edit_action(&mut self, visible: &[usize]) -> AppCommand {
        if self.read_only {
            return self.unsupported("Edit is disabled in read-only mode");
        }
        self.selected_namespace_target(visible)
            .map_or(AppCommand::None, AppCommand::Edit)
    }

    fn selected_namespace_target(&self, visible: &[usize]) -> Option<ResourceTarget> {
        let namespace = visible
            .iter()
            .copied()
            .find(|index| *index == self.overlay_selected)
            .and_then(|index| self.namespace_options.get(index))
            .filter(|namespace| namespace.name != "<all>")?;
        let kind = self
            .resource_options
            .iter()
            .find(|resource| {
                resource.group.is_empty()
                    && resource.version.as_ref() == "v1"
                    && resource.kind.as_ref() == "Namespace"
                    && resource.plural.as_ref() == "namespaces"
            })
            .cloned()
            .unwrap_or_else(|| {
                ResourceDescriptor::new(
                    "",
                    "v1",
                    "v1",
                    "Namespace",
                    "namespaces",
                    false,
                    ResourceOperations::all(),
                )
            });
        Some(ResourceTarget {
            kind,
            namespace: None,
            name: namespace.name.clone(),
        })
    }

    fn handle_context_overlay(&mut self, key: KeyEvent) -> AppCommand {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('d') {
            return self.open_context_delete_confirmation();
        }
        let visible = self.visible_context_indices();
        if self.handle_chooser_navigation(&key, &visible) {
            return AppCommand::None;
        }
        match key.code {
            KeyCode::Char('r') => self.open_context_rename_dialog(&visible),
            KeyCode::Char('u' | 'l') | KeyCode::Enter => {
                let Some(index) = visible
                    .iter()
                    .copied()
                    .find(|index| *index == self.overlay_selected)
                else {
                    return AppCommand::None;
                };
                let Some(context) = self
                    .context_options
                    .get(index)
                    .map(|item| item.name.clone())
                else {
                    return AppCommand::None;
                };
                self.overlay = Overlay::None;
                self.request_view(
                    ViewState {
                        instance: None,
                        context,
                        kind: self.kind.clone(),
                        namespace: self.namespace.clone(),
                        field_selector: self.field_selector.clone(),
                        label_selector: self.label_selector.clone(),
                    },
                    true,
                )
            }
            KeyCode::Char('c') => visible
                .iter()
                .copied()
                .find(|index| *index == self.overlay_selected)
                .and_then(|index| self.context_options.get(index))
                .map_or(AppCommand::None, |context| {
                    AppCommand::Copy(context.name.clone())
                }),
            _ => self.close_overlay_on(key),
        }
    }

    fn handle_confirm_overlay(&mut self, key: KeyEvent) -> AppCommand {
        if self.read_only {
            self.pending_delete = None;
            self.confirm_armed = false;
            self.overlay = Overlay::None;
            return self.unsupported("Mutations are disabled in read-only mode");
        }
        if matches!(self.pending_delete, Some(PendingDelete::DrainNode(_))) {
            return self.handle_drain_confirmation(key);
        }
        if matches!(self.pending_delete, Some(PendingDelete::Scale { .. })) {
            return self.handle_scale_dialog(key);
        }
        if matches!(
            self.pending_delete,
            Some(PendingDelete::RenameContext { .. })
        ) {
            return self.handle_rename_context_dialog(key);
        }
        match key.code {
            KeyCode::Char('h') => {
                self.confirm_armed = false;
                AppCommand::None
            }
            KeyCode::Char('l') => {
                self.confirm_armed = true;
                AppCommand::None
            }
            KeyCode::Tab => {
                self.confirm_armed = !self.confirm_armed;
                AppCommand::None
            }
            KeyCode::Enter if self.confirm_armed => {
                self.confirm_armed = false;
                self.pending_delete
                    .take()
                    .map_or(AppCommand::None, |target| {
                        self.execute_confirmed_action(target)
                    })
            }
            KeyCode::Esc | KeyCode::Char('q' | '?') => {
                let return_to_contexts = matches!(
                    self.pending_delete,
                    Some(PendingDelete::Context { .. } | PendingDelete::RenameContext { .. })
                );
                self.pending_delete = None;
                self.confirm_armed = false;
                self.notice = None;
                self.overlay = if return_to_contexts {
                    Overlay::Contexts
                } else {
                    Overlay::None
                };
                AppCommand::None
            }
            _ => AppCommand::None,
        }
    }

    fn execute_confirmed_action(&mut self, target: PendingDelete) -> AppCommand {
        match target {
            PendingDelete::Resources(targets) => {
                self.overlay = Overlay::None;
                self.marks.clear();
                self.mark_anchor = None;
                AppCommand::Action(ActionIntent::Delete {
                    targets,
                    immediate: false,
                })
            }
            PendingDelete::Restart(targets) => {
                self.overlay = Overlay::None;
                self.marks.clear();
                self.mark_anchor = None;
                AppCommand::Action(ActionIntent::Restart { targets })
            }
            PendingDelete::TriggerCronJob(targets) => {
                self.overlay = Overlay::None;
                self.marks.clear();
                self.mark_anchor = None;
                AppCommand::Action(ActionIntent::TriggerCronJob { targets })
            }
            PendingDelete::ToggleCronJobSuspend { target, .. } => {
                self.overlay = Overlay::None;
                AppCommand::Action(ActionIntent::ToggleCronJobSuspend { target })
            }
            PendingDelete::SetSuspended { targets, suspended } => {
                self.overlay = Overlay::None;
                self.marks.clear();
                self.mark_anchor = None;
                AppCommand::Action(ActionIntent::SetSuspended { targets, suspended })
            }
            PendingDelete::Context { name } => {
                self.overlay = Overlay::Contexts;
                AppCommand::Action(ActionIntent::DeleteContext { name })
            }
            PendingDelete::NodeScheduling {
                names,
                unschedulable,
            } => {
                self.overlay = Overlay::None;
                self.marks.clear();
                self.mark_anchor = None;
                AppCommand::Action(ActionIntent::SetNodeUnschedulable {
                    names,
                    unschedulable,
                })
            }
            PendingDelete::HelmRollback {
                namespace,
                release,
                revision,
            } => {
                self.overlay = Overlay::None;
                AppCommand::Action(ActionIntent::HelmRollback {
                    namespace,
                    release,
                    revision,
                })
            }
            PendingDelete::DirDelete { path } => {
                self.overlay = Overlay::None;
                AppCommand::Action(ActionIntent::DirDelete { path })
            }
            PendingDelete::DrainNode(_)
            | PendingDelete::Scale { .. }
            | PendingDelete::RenameContext { .. } => AppCommand::None,
        }
    }

    fn handle_rename_context_dialog(&mut self, key: KeyEvent) -> AppCommand {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.pending_delete = None;
                self.confirm_armed = false;
                self.notice = None;
                self.overlay = Overlay::Contexts;
                AppCommand::None
            }
            KeyCode::Tab | KeyCode::Down => {
                self.overlay_selected = (self.overlay_selected + 1) % 3;
                AppCommand::None
            }
            KeyCode::BackTab | KeyCode::Up => {
                self.overlay_selected = (self.overlay_selected + 2) % 3;
                AppCommand::None
            }
            KeyCode::Char(character)
                if self.overlay_selected == 0
                    && !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                    && !character.is_whitespace() =>
            {
                if let Some(PendingDelete::RenameContext { new_name, .. }) =
                    self.pending_delete.as_mut()
                {
                    new_name.push(character);
                }
                AppCommand::None
            }
            KeyCode::Backspace if self.overlay_selected == 0 => {
                if let Some(PendingDelete::RenameContext { new_name, .. }) =
                    self.pending_delete.as_mut()
                {
                    new_name.pop();
                }
                AppCommand::None
            }
            KeyCode::Enter if self.overlay_selected == 1 => {
                self.pending_delete = None;
                self.confirm_armed = false;
                self.notice = None;
                self.overlay = Overlay::Contexts;
                AppCommand::None
            }
            KeyCode::Enter if self.overlay_selected == 2 || self.overlay_selected == 0 => {
                if self.read_only {
                    self.pending_delete = None;
                    self.overlay = Overlay::Contexts;
                    return self.unsupported("Rename is disabled in read-only mode");
                }
                let Some(PendingDelete::RenameContext { old_name, new_name }) =
                    self.pending_delete.as_ref()
                else {
                    return AppCommand::None;
                };
                let new_name = new_name.trim().to_owned();
                if new_name.is_empty() {
                    return self.unsupported("Context name cannot be empty");
                }
                let old_name = old_name.clone();
                self.pending_delete = None;
                self.overlay = Overlay::Contexts;
                self.notice = None;
                if old_name == new_name {
                    return AppCommand::None;
                }
                AppCommand::Action(ActionIntent::RenameContext { old_name, new_name })
            }
            _ => AppCommand::None,
        }
    }

    fn handle_scale_dialog(&mut self, key: KeyEvent) -> AppCommand {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => self.cancel_drain_confirmation(),
            KeyCode::Tab | KeyCode::Down => {
                self.overlay_selected = (self.overlay_selected + 1) % 3;
                AppCommand::None
            }
            KeyCode::BackTab | KeyCode::Up => {
                self.overlay_selected = (self.overlay_selected + 2) % 3;
                AppCommand::None
            }
            KeyCode::Char(character)
                if self.overlay_selected == 0 && character.is_ascii_digit() =>
            {
                if let Some(PendingDelete::Scale { replicas, .. }) = self.pending_delete.as_mut()
                    && replicas.len() < 10
                {
                    replicas.push(character);
                }
                AppCommand::None
            }
            KeyCode::Backspace if self.overlay_selected == 0 => {
                if let Some(PendingDelete::Scale { replicas, .. }) = self.pending_delete.as_mut() {
                    replicas.pop();
                }
                AppCommand::None
            }
            KeyCode::Enter if self.overlay_selected == 1 => self.cancel_drain_confirmation(),
            KeyCode::Enter if self.overlay_selected == 2 => {
                if self.read_only {
                    self.cancel_drain_confirmation();
                    return self.unsupported("Scale is disabled in read-only mode");
                }
                let Some(PendingDelete::Scale { targets, replicas }) = self.pending_delete.as_ref()
                else {
                    return AppCommand::None;
                };
                let Ok(replicas) = replicas.parse::<i32>() else {
                    return self
                        .unsupported("Replicas must be a whole number from 0 to 2147483647");
                };
                let targets = targets.clone();
                self.pending_delete = None;
                self.overlay = Overlay::None;
                self.marks.clear();
                self.mark_anchor = None;
                self.notice = None;
                AppCommand::Action(ActionIntent::Scale { targets, replicas })
            }
            _ => AppCommand::None,
        }
    }

    fn handle_drain_confirmation(&mut self, key: KeyEvent) -> AppCommand {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q' | '?') => self.cancel_drain_confirmation(),
            KeyCode::Tab | KeyCode::Down => {
                self.move_drain_field(1);
                AppCommand::None
            }
            KeyCode::BackTab | KeyCode::Up => {
                self.move_drain_field(-1);
                AppCommand::None
            }
            KeyCode::Backspace
                if matches!(
                    self.overlay_selected,
                    DRAIN_GRACE_PERIOD_FIELD | DRAIN_TIMEOUT_FIELD
                ) =>
            {
                let field = self.overlay_selected;
                if let Some(dialog) = self.drain_dialog_mut() {
                    if field == DRAIN_GRACE_PERIOD_FIELD {
                        dialog.grace_period.pop();
                    } else {
                        dialog.timeout.pop();
                    }
                }
                self.drain_value_edit_state = DrainValueEditState::Editing;
                self.notice = None;
                AppCommand::None
            }
            KeyCode::Char(' ') if self.toggle_selected_drain_option() => AppCommand::None,
            KeyCode::Enter => self.activate_selected_drain_field(),
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                    && self.edit_selected_drain_value(character) =>
            {
                AppCommand::None
            }
            _ => AppCommand::None,
        }
    }

    fn move_drain_field(&mut self, direction: isize) {
        self.overlay_selected = if direction < 0 {
            self.overlay_selected
                .checked_sub(1)
                .unwrap_or(DRAIN_FIELD_COUNT - 1)
        } else {
            (self.overlay_selected + 1) % DRAIN_FIELD_COUNT
        };
        self.confirm_armed = self.overlay_selected == DRAIN_OK_FIELD;
        self.drain_value_edit_state = DrainValueEditState::Pristine;
        self.notice = None;
    }

    fn drain_dialog_mut(&mut self) -> Option<&mut DrainDialog> {
        match self.pending_delete.as_mut() {
            Some(PendingDelete::DrainNode(dialog)) => Some(dialog),
            _ => None,
        }
    }

    fn edit_selected_drain_value(&mut self, character: char) -> bool {
        let field = self.overlay_selected;
        let accepted = match field {
            DRAIN_GRACE_PERIOD_FIELD => character.is_ascii_digit() || character == '-',
            DRAIN_TIMEOUT_FIELD => {
                character.is_ascii_digit() || matches!(character, 'm' | 's' | 'h')
            }
            _ => false,
        };
        if !accepted {
            return false;
        }
        let replace = self.drain_value_edit_state == DrainValueEditState::Pristine;
        if let Some(dialog) = self.drain_dialog_mut() {
            let value = if field == DRAIN_GRACE_PERIOD_FIELD {
                &mut dialog.grace_period
            } else {
                &mut dialog.timeout
            };
            if replace {
                value.clear();
            }
            value.push(character);
        }
        self.drain_value_edit_state = DrainValueEditState::Editing;
        self.notice = None;
        true
    }

    fn toggle_selected_drain_option(&mut self) -> bool {
        let field = self.overlay_selected;
        let Some(dialog) = self.drain_dialog_mut() else {
            return false;
        };
        match field {
            DRAIN_IGNORE_DAEMON_SETS_FIELD => {
                dialog.flags.toggle(NodeDrainFlags::IGNORE_DAEMON_SETS);
            }
            DRAIN_DELETE_EMPTYDIR_DATA_FIELD => {
                dialog.flags.toggle(NodeDrainFlags::DELETE_EMPTYDIR_DATA);
            }
            DRAIN_FORCE_FIELD => dialog.flags.toggle(NodeDrainFlags::FORCE),
            DRAIN_DISABLE_EVICTION_FIELD => {
                dialog.flags.toggle(NodeDrainFlags::DISABLE_EVICTION);
            }
            _ => return false,
        }
        self.notice = None;
        true
    }

    fn activate_selected_drain_field(&mut self) -> AppCommand {
        match self.overlay_selected {
            DRAIN_GRACE_PERIOD_FIELD | DRAIN_TIMEOUT_FIELD => {
                self.move_drain_field(1);
                AppCommand::None
            }
            DRAIN_IGNORE_DAEMON_SETS_FIELD
            | DRAIN_DELETE_EMPTYDIR_DATA_FIELD
            | DRAIN_FORCE_FIELD
            | DRAIN_DISABLE_EVICTION_FIELD => {
                self.toggle_selected_drain_option();
                AppCommand::None
            }
            DRAIN_CANCEL_FIELD => self.cancel_drain_confirmation(),
            DRAIN_OK_FIELD => self.confirm_node_drain(),
            _ => AppCommand::None,
        }
    }

    fn cancel_drain_confirmation(&mut self) -> AppCommand {
        self.pending_delete = None;
        self.confirm_armed = false;
        self.notice = None;
        self.overlay = Overlay::None;
        AppCommand::None
    }

    fn confirm_node_drain(&mut self) -> AppCommand {
        let Some(PendingDelete::DrainNode(dialog)) = self.pending_delete.as_ref() else {
            return AppCommand::None;
        };
        let options = match dialog.options() {
            Ok(options) => options,
            Err(error) => {
                self.notice = Some(error);
                return AppCommand::None;
            }
        };
        let names = dialog.names.clone();
        self.pending_delete = None;
        self.confirm_armed = false;
        self.notice = None;
        self.overlay = Overlay::None;
        self.marks.clear();
        self.mark_anchor = None;
        AppCommand::Action(ActionIntent::DrainNode { names, options })
    }

    #[must_use]
    pub fn is_logs_view(&self) -> bool {
        self.overlay == Overlay::Content
            && matches!(
                self.content_kind,
                Some(ContentKind::Logs | ContentKind::PreviousLogs)
            )
    }

    #[must_use]
    pub fn log_display_line<'a>(&self, line: &'a str) -> &'a str {
        if self.ui.timestamps() {
            line
        } else {
            without_log_timestamp(line)
        }
    }

    #[must_use]
    pub fn log_display_line_at<'a>(&self, index: usize, line: &'a str) -> &'a str {
        if self.ui.timestamps() {
            line
        } else {
            self.log_view.as_ref().map_or_else(
                || without_log_timestamp(line),
                |view| view.without_timestamp(index, line),
            )
        }
    }

    #[must_use]
    pub fn can_toggle_all_log_containers(&self) -> bool {
        self.is_logs_view()
            && self.log_view.as_ref().is_some_and(|view| {
                view.request.target.container.is_none()
                    && view.metadata.as_ref().is_some_and(|metadata| {
                        metadata
                            .default_container
                            .as_deref()
                            .is_some_and(|name| !name.is_empty())
                    })
            })
    }

    pub fn show_log_resolution(&mut self, id: u64, metadata: LogSessionMeta) {
        if !self.is_logs_view() {
            return;
        }
        let Some(view) = self.log_view.as_mut().filter(|view| view.request.id == id) else {
            return;
        };
        view.request.all_containers = metadata.all_containers;
        view.metadata = Some(metadata);
        self.content_title = Self::log_title(&view.request, view.metadata.as_ref());
    }

    fn log_title(request: &LogRequest, metadata: Option<&LogSessionMeta>) -> String {
        let label = if request.target.previous {
            "Previous Logs"
        } else {
            "Logs"
        };
        let resolved_container =
            metadata
                .filter(|_| !request.all_containers)
                .and_then(|metadata| {
                    metadata
                        .selected_container
                        .as_deref()
                        .or(metadata.default_container.as_deref())
                });
        let container = request.target.container.as_deref().or(resolved_container);
        let container = container
            .filter(|name| !name.is_empty())
            .map(|name| format!(":{name}"))
            .unwrap_or_default();
        format!(
            "{label}({}/{}{container})",
            request.target.namespace(),
            request.target.name()
        )
    }

    #[must_use]
    pub fn log_status_label(&self) -> String {
        let Some(view) = &self.log_view else {
            return String::new();
        };
        let range = match view.request.range {
            LogRange::Tail => "tail".to_owned(),
            LogRange::Head => "head".to_owned(),
            LogRange::SinceSeconds(3_600) => "1h".to_owned(),
            LogRange::SinceSeconds(seconds) => format!("{}m", seconds / 60),
        };
        let state = if view.finished {
            "complete"
        } else if !view.auto_scroll {
            "paused"
        } else {
            "following"
        };
        format!("{range} · {state}")
    }

    fn open_logs(&mut self, target: LogTarget) -> AppCommand {
        self.next_log_id = self.next_log_id.wrapping_add(1);
        let settings = self.user_config.settings.logger;
        let request = LogRequest::new(self.next_log_id, target, &settings);
        let title = Self::log_title(&request, None);
        self.show_content(
            title,
            "",
            None,
            None,
            None,
            if request.target.previous {
                ContentKind::PreviousLogs
            } else {
                ContentKind::Logs
            },
        );
        self.ui.set(UiFlags::WRAP, settings.text_wrap);
        self.ui.set(UiFlags::TIMESTAMPS, settings.show_time);
        self.content_scroll = if settings.disable_autoscroll {
            0
        } else {
            u16::MAX
        };
        self.log_view = Some(LogView::with_settings(request.clone(), &settings));
        self.notice = Some("Loading logs…".to_owned());
        AppCommand::Action(ActionIntent::StartLogs { request })
    }

    fn reload_logs(&mut self, range: LogRange) -> AppCommand {
        let Some(view) = &mut self.log_view else {
            return AppCommand::None;
        };
        self.next_log_id = self.next_log_id.wrapping_add(1);
        view.request.id = self.next_log_id;
        view.request.range = range;
        view.finished = false;
        view.refresh_pending = true;
        let metadata = view.metadata.take();
        view.clear();
        let request = view.request.clone();
        self.content_title = Self::log_title(&request, metadata.as_ref());
        self.content_body.clear();
        self.reset_body_scroll();
        if self.log_view.as_ref().is_some_and(|view| view.auto_scroll) {
            self.content_scroll = u16::MAX;
        }
        self.notice = Some("Loading logs…".to_owned());
        AppCommand::Action(ActionIntent::StartLogs { request })
    }

    /// Export the complete displayed buffer, respecting timestamps and filtering.
    #[must_use]
    pub fn log_export_text(&self) -> String {
        let matcher = FilterMatcher::new(self.body_filter());
        self.content_body
            .lines()
            .enumerate()
            .map(|(index, line)| self.log_display_line_at(index, line))
            .filter(|line| matcher.matches_text(line))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn toggle_all_log_containers(&mut self) -> AppCommand {
        if !self.can_toggle_all_log_containers() {
            return AppCommand::None;
        }
        let Some(view) = &mut self.log_view else {
            return AppCommand::None;
        };
        if view
            .metadata
            .as_ref()
            .is_some_and(|metadata| metadata.single_container)
        {
            return AppCommand::None;
        }
        view.request.all_containers = !view.request.all_containers;
        let range = view.request.range;
        self.reload_logs(range)
    }

    fn handle_log_key(&mut self, key: KeyEvent) -> Option<AppCommand> {
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            return (key.code == KeyCode::Char('s')).then(|| {
                let name = self.log_view.as_ref().map_or_else(
                    || "pod-logs".to_owned(),
                    |view| {
                        format!(
                            "{}-{}",
                            view.request.target.namespace(),
                            view.request.target.name()
                        )
                    },
                );
                AppCommand::SaveLogs {
                    name,
                    body: self.log_export_text(),
                }
            });
        }
        if key.modifiers.contains(KeyModifiers::ALT) {
            return None;
        }
        let command = match key.code {
            KeyCode::Char('a') => self.toggle_all_log_containers(),
            KeyCode::Char('0') => self.reload_logs(LogRange::Tail),
            KeyCode::Char('1') => self.reload_logs(LogRange::Head),
            KeyCode::Char('2') => self.reload_logs(LogRange::SinceSeconds(60)),
            KeyCode::Char('3') => self.reload_logs(LogRange::SinceSeconds(300)),
            KeyCode::Char('4') => self.reload_logs(LogRange::SinceSeconds(900)),
            KeyCode::Char('5') => self.reload_logs(LogRange::SinceSeconds(1_800)),
            KeyCode::Char('6') => self.reload_logs(LogRange::SinceSeconds(3_600)),
            KeyCode::Char('c') => AppCommand::Copy(self.log_export_text()),
            KeyCode::Char('C') => {
                if let Some(view) = &mut self.log_view {
                    view.clear();
                    view.refresh_pending = false;
                }
                self.content_body.clear();
                self.reset_body_scroll();
                AppCommand::None
            }
            KeyCode::Char('m') => {
                let mark = "─".repeat(usize::from(self.log_view_width.saturating_sub(4).max(1)));
                if !self.content_body.is_empty() && !self.content_body.ends_with('\n') {
                    self.content_body.push('\n');
                }
                if let Some(view) = &mut self.log_view {
                    view.insert_marker(&mut self.content_body, mark);
                } else {
                    append_display_lines(&mut self.content_body, &[mark], 5_000);
                }
                self.content_scroll = u16::MAX;
                AppCommand::None
            }
            KeyCode::Char('s') => {
                if let Some(view) = &mut self.log_view {
                    view.auto_scroll = !view.auto_scroll;
                    if view.auto_scroll {
                        view.show_pending(&mut self.content_body);
                        self.content_scroll = u16::MAX;
                        if !view.column_lock {
                            self.content_horizontal_scroll = 0;
                        }
                    }
                }
                AppCommand::None
            }
            KeyCode::Char('L') => {
                if let Some(view) = &mut self.log_view {
                    view.column_lock = !view.column_lock;
                }
                AppCommand::None
            }
            KeyCode::Char('t') => {
                let keep_bottom = self.content_scroll >= self.content_max_scroll;
                self.ui.toggle(UiFlags::TIMESTAMPS);
                if keep_bottom {
                    self.content_scroll = u16::MAX;
                }
                AppCommand::None
            }
            KeyCode::Esc | KeyCode::Char('q' | '?') => {
                if !self.body_query.is_empty() {
                    self.body_query.clear();
                    self.filter_error = None;
                    self.content_match_cursor = None;
                    return Some(AppCommand::None);
                }
                self.overlay = Overlay::None;
                self.notice = None;
                self.log_view.take().map_or(AppCommand::None, |view| {
                    AppCommand::Action(ActionIntent::StopLogs {
                        id: view.request.id,
                    })
                })
            }
            _ => return None,
        };
        Some(command)
    }

    pub fn append_log_batch(
        &mut self,
        id: u64,
        lines: Vec<LogLine>,
        finished: bool,
        error: Option<String>,
    ) {
        if !self.is_logs_view() {
            return;
        }
        let Some(view) = self.log_view.as_mut().filter(|view| view.request.id == id) else {
            return;
        };
        let lines: Vec<_> = lines
            .into_iter()
            .map(|mut line| {
                line.text = plain_log_line(&line.text);
                if let Some(source) = &mut line.source {
                    source.namespace = plain_log_line(&source.namespace);
                    source.pod = plain_log_line(&source.pod);
                    source.container = plain_log_line(&source.container);
                }
                line
            })
            .collect();
        view.append(&lines);
        view.finished = finished;
        if view.auto_scroll || view.refresh_pending || view.request.range == LogRange::Head {
            view.show_pending(&mut self.content_body);
            if view.auto_scroll && !lines.is_empty() {
                self.content_scroll = u16::MAX;
                if !view.column_lock {
                    self.content_horizontal_scroll = 0;
                }
            }
        }
        if !lines.is_empty() {
            view.refresh_pending = false;
        }
        if let Some(error) = error {
            self.notice = Some(format!("Logs: {error}"));
        } else if (!lines.is_empty() || finished) && self.notice.as_deref() == Some("Loading logs…")
        {
            self.notice = None;
        }
    }

    fn handle_content_overlay(&mut self, key: KeyEvent) -> AppCommand {
        if self.content_kind == Some(ContentKind::ScreenDump)
            && matches!(key.code, KeyCode::Esc | KeyCode::Char('q'))
        {
            return self.leave_screen_dump_content();
        }
        if self.is_logs_view()
            && let Some(command) = self.handle_log_key(key)
        {
            return command;
        }
        if self.handle_body_navigation(&key) {
            return AppCommand::None;
        }
        match key.code {
            KeyCode::Char('r') if self.is_live_content() => self.toggle_content_refresh(),
            KeyCode::Char('e') if self.is_live_content() => self.edit_content_action(),
            KeyCode::Char('c')
                if matches!(
                    self.content_kind,
                    Some(
                        ContentKind::Yaml
                            | ContentKind::SecretData
                            | ContentKind::HelmValues
                            | ContentKind::Logs
                            | ContentKind::PreviousLogs
                            | ContentKind::Describe
                            | ContentKind::ScreenDump
                    )
                ) =>
            {
                AppCommand::Copy(self.active_content_body().to_owned())
            }
            KeyCode::Char('m') if self.content_kind == Some(ContentKind::Yaml) => {
                self.toggle_managed_fields_content()
            }
            KeyCode::Char('x' | 'v')
                if matches!(
                    self.content_kind,
                    Some(ContentKind::Yaml | ContentKind::HelmValues)
                ) && (key.code == KeyCode::Char('x')
                    || self.content_kind == Some(ContentKind::HelmValues))
                    && self.content_decoded_body.is_some() =>
            {
                self.ui.toggle(UiFlags::SECRET_DECODED);
                self.reset_body_scroll();
                self.scroll_to_first_content_match();
                let notice = if self.content_kind == Some(ContentKind::HelmValues) {
                    if self.ui.secret_decoded() {
                        "Showing all values (including defaults)"
                    } else {
                        "Showing user-supplied values"
                    }
                } else if self.ui.secret_decoded() {
                    "Secret values decoded"
                } else {
                    "Secret values encoded"
                };
                self.notice = Some(notice.to_owned());
                AppCommand::None
            }
            KeyCode::Char('f') => {
                self.ui.toggle(UiFlags::FULLSCREEN);
                AppCommand::None
            }
            KeyCode::Char('w') => {
                self.toggle_content_wrap();
                AppCommand::None
            }
            KeyCode::Char('t') => {
                self.ui.toggle(UiFlags::TIMESTAMPS);
                AppCommand::None
            }
            _ => self.close_overlay_on(key),
        }
    }

    fn toggle_managed_fields_content(&mut self) -> AppCommand {
        match self.content_managed_fields.as_deref() {
            Some("") => {
                let Some(target) = self.content_target.clone() else {
                    return AppCommand::None;
                };
                self.yaml_refresh.id = self.yaml_refresh.id.wrapping_add(1);
                self.yaml_refresh.pending = true;
                AppCommand::Action(ActionIntent::ManagedYaml {
                    id: self.yaml_refresh.id,
                    target,
                    body: self.content_body.clone(),
                    decoded_body: self.content_decoded_body.clone(),
                })
            }
            Some(_) => {
                self.ui.toggle(UiFlags::MANAGED_FIELDS);
                AppCommand::None
            }
            None => self.unsupported("Selected resource has no managed fields"),
        }
    }

    fn handle_details_overlay(&mut self, key: KeyEvent) -> AppCommand {
        if self.handle_body_navigation(&key) {
            return AppCommand::None;
        }
        match key.code {
            KeyCode::Char('e') => {
                let command = self.edit_action();
                if matches!(&command, AppCommand::Edit(_)) {
                    self.overlay = Overlay::None;
                }
                command
            }
            KeyCode::Char('y') => {
                self.overlay = Overlay::None;
                self.content_action(ContentKind::Yaml)
            }
            KeyCode::Char('d') => {
                self.overlay = Overlay::None;
                self.content_action(ContentKind::Describe)
            }
            _ => self.close_overlay_on(key),
        }
    }

    fn handle_help_overlay(&mut self, key: KeyEvent) -> AppCommand {
        if self.handle_body_navigation(&key) {
            AppCommand::None
        } else {
            self.close_overlay_on(key)
        }
    }

    fn handle_body_navigation(&mut self, key: &KeyEvent) -> bool {
        match key.code {
            KeyCode::Char('n')
                if self.overlay == Overlay::Content && !self.body_query.is_empty() =>
            {
                self.scroll_to_content_match(true);
            }
            KeyCode::Char('N')
                if self.overlay == Overlay::Content
                    && !self.body_query.is_empty()
                    && key.modifiers.contains(KeyModifiers::SHIFT) =>
            {
                self.scroll_to_content_match(false);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.content_match_cursor = None;
                self.content_scroll = self
                    .content_scroll
                    .saturating_add(1)
                    .min(self.content_max_scroll);
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.content_match_cursor = None;
                self.content_scroll = self.content_scroll.saturating_sub(1);
            }
            KeyCode::PageDown => {
                self.content_match_cursor = None;
                self.content_scroll = self
                    .content_scroll
                    .saturating_add(20)
                    .min(self.content_max_scroll);
            }
            KeyCode::Char('f') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.content_match_cursor = None;
                self.content_scroll = self
                    .content_scroll
                    .saturating_add(20)
                    .min(self.content_max_scroll);
            }
            KeyCode::PageUp => {
                self.content_match_cursor = None;
                self.content_scroll = self.content_scroll.saturating_sub(20);
            }
            KeyCode::Char('b') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.content_match_cursor = None;
                self.content_scroll = self.content_scroll.saturating_sub(20);
            }
            KeyCode::Home | KeyCode::Char('g') => {
                self.content_match_cursor = None;
                self.content_scroll = 0;
            }
            KeyCode::End | KeyCode::Char('G') => {
                self.content_match_cursor = None;
                self.content_scroll = self.content_max_scroll;
            }
            KeyCode::Left | KeyCode::Char('h') => {
                self.content_horizontal_scroll = self.content_horizontal_scroll.saturating_sub(1);
            }
            KeyCode::Right | KeyCode::Char('l') => {
                self.content_horizontal_scroll = self.content_horizontal_scroll.saturating_add(1);
            }
            _ => return false,
        }
        true
    }

    fn reset_body_scroll(&mut self) {
        self.content_scroll = 0;
        self.content_horizontal_scroll = 0;
        self.content_max_scroll = 0;
        self.content_match_cursor = None;
    }

    fn toggle_content_wrap(&mut self) {
        let keep_bottom =
            self.content_max_scroll > 0 && self.content_scroll == self.content_max_scroll;
        self.ui.toggle(UiFlags::WRAP);
        if keep_bottom {
            self.content_scroll = u16::MAX;
        }
    }

    pub(crate) fn set_content_scroll_extent(
        &mut self,
        content_height: usize,
        viewport_height: u16,
    ) {
        let max_scroll = content_height.saturating_sub(usize::from(viewport_height));
        self.content_max_scroll = u16::try_from(max_scroll).unwrap_or(u16::MAX);
        self.content_scroll = self.content_scroll.min(self.content_max_scroll);
    }

    fn close_overlay_on(&mut self, key: KeyEvent) -> AppCommand {
        if matches!(key.code, KeyCode::Esc | KeyCode::Char('h' | 'q' | '?')) {
            self.overlay = Overlay::None;
            self.pending_delete = None;
            self.confirm_armed = false;
            self.notice = None;
        }
        AppCommand::None
    }

    fn start_input(&mut self, mode: InputMode) -> AppCommand {
        self.input_mode = mode;
        self.input_before_edit = self.active_filter().to_owned();
        if self.overlay == Overlay::Help {
            self.overlay = Overlay::None;
        }
        self.input.clear();
        self.reset_completion();
        self.notice = None;
        AppCommand::None
    }

    fn preview_filter(&mut self) {
        if self.input_mode == InputMode::Filter {
            self.set_active_filter(self.input.clone());
        }
    }

    fn body_filter_active(&self) -> bool {
        matches!(
            self.overlay,
            Overlay::Aliases
                | Overlay::Namespaces
                | Overlay::Contexts
                | Overlay::Content
                | Overlay::Details
                | Overlay::ScreenDumps
        )
    }

    fn active_filter(&self) -> &str {
        if self.body_filter_active() {
            &self.body_query
        } else {
            &self.query
        }
    }

    fn set_active_filter(&mut self, filter: String) {
        if self.body_filter_active() {
            self.body_query = filter;
            self.content_match_cursor = None;
            let matcher = FilterMatcher::new(&self.body_query);
            self.filter_error = matcher.error().map(str::to_owned);
            if self.overlay == Overlay::Content
                && matches!(
                    self.content_kind,
                    Some(ContentKind::Yaml | ContentKind::HelmValues)
                )
            {
                self.scroll_to_first_content_match();
            }
            if matches!(
                self.overlay,
                Overlay::Aliases | Overlay::Namespaces | Overlay::Contexts | Overlay::ScreenDumps
            ) {
                self.select_first_visible_chooser_item();
            }
        } else {
            self.query = filter;
            self.rebuild_filter(None);
        }
    }

    fn scroll_to_first_content_match(&mut self) {
        if self.body_query.is_empty() || self.filter_error.is_some() {
            return;
        }
        let matcher = FilterMatcher::new(&self.body_query);
        let match_line = self
            .active_content_body()
            .lines()
            .position(|line| matcher.matches_text(line));
        if let Some(line) = match_line {
            self.content_scroll = u16::try_from(line).unwrap_or(u16::MAX);
            self.content_match_cursor = Some(0);
        }
    }

    fn scroll_to_content_match(&mut self, forward: bool) {
        let matcher = FilterMatcher::new(&self.body_query);
        let positions = if matches!(
            self.content_kind,
            Some(ContentKind::Yaml | ContentKind::HelmValues)
        ) {
            self.active_content_body()
                .lines()
                .enumerate()
                .filter(|(_, line)| matcher.matches_text(line))
                .map(|(index, _)| index)
                .collect::<Vec<_>>()
        } else {
            (0..self
                .active_content_body()
                .lines()
                .enumerate()
                .map(|(index, line)| {
                    if self.is_logs_view() {
                        self.log_display_line_at(index, line)
                    } else {
                        line
                    }
                })
                .filter(|line| matcher.matches_text(line))
                .count())
                .collect::<Vec<_>>()
        };
        if positions.is_empty() {
            return;
        }
        let current = usize::from(self.content_scroll);
        let target_index = if let Some(cursor) = self
            .content_match_cursor
            .filter(|cursor| *cursor < positions.len())
        {
            if forward {
                (cursor + 1) % positions.len()
            } else {
                cursor.checked_sub(1).unwrap_or(positions.len() - 1)
            }
        } else if forward {
            positions
                .iter()
                .position(|position| *position > current)
                .unwrap_or(0)
        } else {
            positions
                .iter()
                .rposition(|position| *position < current)
                .unwrap_or(positions.len() - 1)
        };
        let target = positions[target_index];
        self.content_match_cursor = Some(target_index);
        self.content_scroll = u16::try_from(target).unwrap_or(u16::MAX);
    }

    fn execute_command(&mut self) -> AppCommand {
        let command = self.user_config.expand_command(self.input.trim());
        self.input.clear();
        self.input_mode = InputMode::Browse;
        self.reset_completion();
        let command = match command {
            Ok(command) => command,
            Err(message) => return self.unsupported(&message),
        };
        let mut parts = command.split_whitespace();
        let Some(name) = parts.next() else {
            return AppCommand::None;
        };
        match name.to_ascii_lowercase().as_str() {
            "q" | "quit" => AppCommand::Quit,
            "help" => self.open_overlay(Overlay::Help),
            "alias" | "aliases" => self.open_aliases(),
            "screendump" | "screendumps" | "sd" => self.open_screen_dumps(),
            "ctx" | "context" | "contexts" => {
                if let Some(context) = parts.next() {
                    self.request_view(
                        ViewState {
                            instance: None,
                            context: context.to_owned(),
                            kind: self.kind.clone(),
                            namespace: self.namespace.clone(),
                            field_selector: self.field_selector.clone(),
                            label_selector: self.label_selector.clone(),
                        },
                        true,
                    )
                } else {
                    self.open_contexts()
                }
            }
            "ns" | "namespace" | "namespaces" => {
                if let Some(namespace) = parts.next() {
                    let namespace = (namespace != "all").then(|| namespace.to_owned());
                    self.request_view(
                        ViewState {
                            instance: None,
                            context: self.context.clone(),
                            kind: self.kind.clone(),
                            namespace,
                            field_selector: self.field_selector.clone(),
                            label_selector: self.label_selector.clone(),
                        },
                        true,
                    )
                } else {
                    self.open_namespaces()
                }
            }
            "pf" | "portforward" | "portforwards" => AppCommand::ShowPortForwards,
            "cow" => {
                let says = parts.collect::<Vec<_>>().join(" ");
                "Cow".clone_into(&mut self.content_title);
                self.content_body = cow_talk(&says);
                self.content_managed_fields = None;
                self.content_decoded_body = None;
                self.content_decoded_managed_fields = None;
                self.content_kind = Some(ContentKind::Describe);
                self.reset_body_scroll();
                self.overlay = Overlay::Content;
                AppCommand::None
            }
            "dir" | "ls" => self.execute_dir_command(&parts.collect::<Vec<_>>()),
            "xray" | "xr" => self.execute_xray_command(&parts.collect::<Vec<_>>()),
            "can" => self.execute_can_command(&parts.collect::<Vec<_>>()),
            "suspend" => self.open_suspend_confirmation(true),
            "resume" => self.open_suspend_confirmation(false),
            alias => self.execute_resource_command(alias, &parts.collect::<Vec<_>>()),
        }
    }

    fn execute_dir_command(&mut self, arguments: &[&str]) -> AppCommand {
        let path = arguments
            .first()
            .copied()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or(".");
        let expanded = if let Some(rest) = path.strip_prefix("~/")
            && let Ok(home) = std::env::var("HOME")
        {
            format!("{home}/{rest}")
        } else {
            path.to_owned()
        };
        self.request_view(
            ViewState {
                instance: None,
                context: self.context.clone(),
                kind: ResourceDescriptor::dir(),
                namespace: None,
                field_selector: Some(expanded),
                label_selector: None,
            },
            true,
        )
    }

    fn execute_xray_command(&mut self, arguments: &[&str]) -> AppCommand {
        let Some(raw_target) = arguments.first().copied().filter(|s| !s.trim().is_empty()) else {
            return self.unsupported(
                "You must provide a valid resource type (po, svc, deploy, sts, ds, rs)",
            );
        };
        let Some(target_kind) =
            crate::source::kubernetes::xray::resolve_xray_target_kind(raw_target)
        else {
            return self.unsupported(&format!(
                "`xray` command is not supported on {raw_target}; expected po, svc, deploy, sts, ds, or rs"
            ));
        };
        let mut requested_context = self.context.clone();
        let mut explicit_namespace = None;
        let mut requested_filter = None;
        let mut index = 1;
        while index < arguments.len() {
            let arg = arguments[index];
            if arg == "-n"
                && let Some(ns) = arguments.get(index + 1)
            {
                explicit_namespace = Some(*ns);
                index += 2;
                continue;
            } else if arg == "-f"
                && let Some(f) = arguments.get(index + 1)
            {
                requested_filter = Some((*f).to_owned());
                index += 2;
                continue;
            } else if let Some(ctx) = arg.strip_prefix('@') {
                let ctx = ctx.trim();
                if !ctx.is_empty() {
                    ctx.clone_into(&mut requested_context);
                }
            } else if let Some(f) = arg.strip_prefix('/') {
                requested_filter = Some(f.to_owned());
            } else if explicit_namespace.is_none() {
                explicit_namespace = Some(arg);
            }
            index += 1;
        }
        let namespace = if let Some(ns) = explicit_namespace {
            (!matches!(ns, "all" | "-")).then(|| ns.to_owned())
        } else {
            self.namespace.clone()
        };
        self.xray_collapsed.clear();
        let command = self.request_view(
            ViewState {
                instance: None,
                context: requested_context,
                kind: ResourceDescriptor::xray(),
                namespace,
                field_selector: Some(target_kind.gvr()),
                label_selector: None,
            },
            true,
        );
        if let Some(filter) = requested_filter {
            self.query = filter;
            self.filter_error = None;
        }
        command
    }

    fn execute_can_command(&mut self, arguments: &[&str]) -> AppCommand {
        let Some(subject) = arguments.first() else {
            return self.unsupported(
                "Usage: :can u:<user> | :can g:<group> | :can s:[namespace/]serviceaccount",
            );
        };
        let Some((prefix, raw_name)) = subject.split_once(':') else {
            return self.unsupported(
                "Usage: :can u:<user> | :can g:<group> | :can s:[namespace/]serviceaccount",
            );
        };
        let raw_name = raw_name.trim();
        if raw_name.is_empty() {
            return self.unsupported("Subject name cannot be empty");
        }
        let target = match prefix.to_ascii_lowercase().as_str() {
            "u" | "user" | "usr" => ResourceTarget {
                kind: ResourceDescriptor::users(),
                namespace: None,
                name: raw_name.to_owned(),
            },
            "g" | "group" | "grp" => ResourceTarget {
                kind: ResourceDescriptor::groups(),
                namespace: None,
                name: raw_name.to_owned(),
            },
            "s" | "sa" | "serviceaccount" => {
                let (namespace, name) = if let Some((ns, sa)) = raw_name.split_once('/') {
                    let ns = ns.trim();
                    let sa = sa.trim();
                    if ns.is_empty() || sa.is_empty() {
                        return self.unsupported("ServiceAccount subject must be [namespace/]name");
                    }
                    (ns.to_owned(), sa.to_owned())
                } else {
                    (
                        self.namespace
                            .clone()
                            .unwrap_or_else(|| "default".to_owned()),
                        raw_name.to_owned(),
                    )
                };
                let kind = self
                    .resource_options
                    .iter()
                    .find(|resource| {
                        resource.group.is_empty() && resource.kind.as_ref() == "ServiceAccount"
                    })
                    .cloned()
                    .unwrap_or_else(|| {
                        ResourceDescriptor::new(
                            "",
                            "v1",
                            "v1",
                            "ServiceAccount",
                            "serviceaccounts",
                            true,
                            ResourceOperations::all(),
                        )
                        .with_short_names(["sa"])
                    });
                ResourceTarget {
                    kind,
                    namespace: Some(namespace),
                    name,
                }
            }
            _ => {
                return self.unsupported(
                    "Unknown subject type; expected u:<user>, g:<group>, or s:<serviceaccount>",
                );
            }
        };
        self.overlay = Overlay::None;
        self.pending_policy = Some(target.clone());
        AppCommand::Action(ActionIntent::AuthorizationPolicy { target })
    }

    fn execute_resource_command(&mut self, alias: &str, arguments: &[&str]) -> AppCommand {
        let kind = match self.resolve_resource(alias) {
            Ok(kind) => kind,
            Err(message) => {
                self.notice = Some(message);
                return AppCommand::None;
            }
        };
        let mut requested_context = self.context.clone();
        let mut explicit_namespace = None;
        let mut requested_filter = None;
        let mut label_parts = Vec::new();
        let mut index = 0;
        while index < arguments.len() {
            let argument = arguments[index];
            match argument {
                "-n" => {
                    if let Some(ns) = arguments.get(index + 1) {
                        explicit_namespace = Some(*ns);
                        index += 2;
                        continue;
                    }
                }
                "-l" => {
                    if let Some(label) = arguments.get(index + 1) {
                        label_parts.push(*label);
                        index += 2;
                        continue;
                    }
                }
                "-f" => {
                    if let Some(filter) = arguments.get(index + 1) {
                        requested_filter = Some((*filter).to_owned());
                        index += 2;
                        continue;
                    }
                }
                _ => {
                    if let Some(context) = argument.strip_prefix('@') {
                        let context = context.trim();
                        if !context.is_empty() {
                            context.clone_into(&mut requested_context);
                        }
                    } else if let Some(filter) = argument.strip_prefix('/') {
                        requested_filter = Some(
                            std::iter::once(filter)
                                .chain(arguments.iter().skip(index + 1).copied())
                                .collect::<Vec<_>>()
                                .join(" "),
                        );
                        break;
                    } else if argument.contains('=')
                        || argument.contains('!')
                        || argument.contains('(')
                        || arguments
                            .get(index + 1)
                            .is_some_and(|next| matches!(*next, "in" | "notin"))
                        || !label_parts.is_empty()
                    {
                        label_parts.push(argument);
                    } else if explicit_namespace.is_none() {
                        explicit_namespace = Some(argument);
                    } else {
                        label_parts.push(argument);
                    }
                }
            }
            index += 1;
        }
        let namespace = if kind.is_namespaced() {
            if let Some(ns) = explicit_namespace {
                (ns != "all").then(|| ns.to_owned())
            } else {
                self.namespace.clone()
            }
        } else {
            None
        };
        let label_selector = (!label_parts.is_empty()).then(|| label_parts.join(" "));
        let command = self.request_view(
            ViewState {
                instance: None,
                context: requested_context,
                kind,
                namespace,
                field_selector: None,
                label_selector,
            },
            true,
        );
        if let Some(filter) = requested_filter {
            self.query = filter;
            self.filter_error = None;
        }
        command
    }

    fn current_view(&self) -> ViewState {
        ViewState {
            instance: self.instance.clone(),
            context: self.context.clone(),
            kind: self.kind.clone(),
            namespace: self.namespace.clone(),
            field_selector: self.field_selector.clone(),
            label_selector: self.label_selector.clone(),
        }
    }

    pub fn reload_current_view(&mut self) -> AppCommand {
        let notice = self.notice.take();
        let command = self.request_view(self.current_view(), false);
        self.notice = notice;
        command
    }

    fn request_view(&mut self, view: ViewState, push_history: bool) -> AppCommand {
        self.clear_restored_workload_selection();
        self.references_view_requested(&view);
        self.clear_pending_navigation();
        let resource_kind_changed = self.kind != view.kind
            || ((self.kind.is_dir() || self.kind.is_xray())
                && self.field_selector != view.field_selector);
        if push_history && self.current_view() != view {
            self.save_workloads_history();
            self.truncate_workloads_history();
            self.history.truncate(self.history_cursor + 1);
            self.history.push(view.clone());
            self.history_cursor = self.history.len() - 1;
        }
        if self.context != view.context {
            self.namespace_options.clear();
            self.resource_options = default_resource_options();
            "detecting…".clone_into(&mut self.server_version);
        }
        self.context.clone_from(&view.context);
        if !self
            .context_options
            .iter()
            .any(|item| item.name == self.context)
        {
            self.context_options
                .push(ContextInfo::synthetic(self.context.clone()));
            self.context_options
                .sort_unstable_by(|left, right| left.name.cmp(&right.name));
        }
        self.kind = view.kind.clone();
        self.instance.clone_from(&view.instance);
        self.namespace.clone_from(&view.namespace);
        if resource_kind_changed {
            self.column_sort_override = None;
        }
        if resource_kind_changed && self.kind.is_workloads() {
            self.sort_field = SortField::Kind;
            self.sort_descending = false;
        } else if resource_kind_changed && self.kind.is_helm_history() {
            self.sort_field = SortField::Ready;
            self.sort_descending = true;
        } else if resource_kind_changed && self.kind.is_benchmarks() {
            self.sort_field = SortField::Age;
            self.sort_descending = false;
        } else if resource_kind_changed && self.kind.is_scans() {
            self.sort_field = SortField::Status;
            self.sort_descending = false;
        } else if resource_kind_changed
            && matches!(self.sort_field, SortField::Kind | SortField::Ready)
        {
            self.sort_field = SortField::Name;
            self.sort_descending = false;
        }
        self.apply_custom_view_sort();
        self.select_workload_sort_column(self.sort_field);
        self.field_selector.clone_from(&view.field_selector);
        self.label_selector.clone_from(&view.label_selector);
        self.list_horizontal_scroll = 0;
        self.list_max_horizontal_scroll = 0;
        if view.kind.is_namespaced() {
            self.remember_namespace(view.namespace.clone());
        }
        self.generation = self.generation.wrapping_add(1);
        self.snapshot = ResourceSnapshot::loading(self.generation, view.kind.clone());
        self.list_mode = ListMode::Resources;
        self.saved_resource_list = None;
        self.pending_containers = None;
        self.pending_policy = None;
        self.pending_child = None;
        self.pending_owner = None;
        self.details_row = None;
        self.log_view = None;
        self.body_query.clear();
        if resource_kind_changed {
            self.query.clear();
            self.filter_error = None;
        }
        self.filtered.clear();
        self.selected = 0;
        self.offset = 0;
        self.overlay = Overlay::None;
        self.started_loading_at = Instant::now();
        self.marks.clear();
        self.mark_anchor = None;
        self.notice = None;
        AppCommand::Request(view)
    }

    fn switch_recent_namespace(&mut self, number: char) -> AppCommand {
        let Some(index) = number.to_digit(10).map(|value| value as usize) else {
            return AppCommand::None;
        };
        if !self.kind.is_namespaced() {
            return if index == 0 {
                AppCommand::None
            } else {
                self.unsupported("Namespace shortcuts are unavailable for cluster resources")
            };
        }
        let namespace = if index == 0 {
            None
        } else {
            self.recent_namespaces.get(index - 1).cloned()
        };
        if index != 0 && namespace.is_none() {
            return AppCommand::None;
        }
        self.request_view(
            ViewState {
                instance: None,
                context: self.context.clone(),
                kind: self.kind.clone(),
                namespace,
                field_selector: self.field_selector.clone(),
                label_selector: self.label_selector.clone(),
            },
            true,
        )
    }

    fn remember_namespace(&mut self, namespace: Option<String>) {
        if let Some(namespace) = namespace {
            self.recent_namespaces.retain(|recent| recent != &namespace);
            self.recent_namespaces.insert(0, namespace);
        }
        self.recent_namespaces.truncate(9);
    }

    fn use_selected_namespace_row(&mut self) -> AppCommand {
        let Some(name) = self.selected_row().map(|row| row.name.to_string()) else {
            return AppCommand::None;
        };
        let namespace = (!matches!(name.as_str(), "all" | "<all>")).then_some(name);
        self.use_active_namespace(namespace);
        AppCommand::None
    }

    fn use_active_namespace(&mut self, namespace: Option<String>) {
        if self.namespace == namespace {
            return;
        }
        let label = namespace.as_deref().unwrap_or("all").to_owned();
        self.namespace.clone_from(&namespace);
        self.remember_namespace(namespace);
        self.show_transient_notice(format!("Active namespace set to {label}"));
    }

    fn reset_completion(&mut self) {
        self.completion_seed = None;
        self.completion_index = 0;
    }

    fn complete_command(&mut self, reverse: bool) {
        let first = self.completion_seed.is_none();
        let seed = self
            .completion_seed
            .get_or_insert_with(|| self.input.clone())
            .clone();
        let candidates = self.command_completions_for(&seed);
        if candidates.is_empty() {
            return;
        }
        if first {
            self.completion_index = if reverse { candidates.len() - 1 } else { 0 };
        } else if reverse {
            self.completion_index = self
                .completion_index
                .checked_sub(1)
                .unwrap_or(candidates.len() - 1);
        } else {
            self.completion_index = (self.completion_index + 1) % candidates.len();
        }
        self.input
            .clone_from(&candidates[self.completion_index.min(candidates.len() - 1)]);
    }

    #[must_use]
    pub fn command_suggestions(&self) -> Vec<String> {
        let seed = self.completion_seed.as_deref().unwrap_or(&self.input);
        self.command_completions_for(seed)
    }

    #[must_use]
    pub fn previous_filter_hint(&self) -> Option<&str> {
        (self.input_mode == InputMode::Filter
            && self.input.is_empty()
            && !self.input_before_edit.is_empty())
        .then_some(self.input_before_edit.as_str())
    }

    fn command_completions_for(&self, input: &str) -> Vec<String> {
        let input = input.trim_start();
        let Some(separator) = input.find(char::is_whitespace) else {
            let prefix = input.to_ascii_lowercase();
            if prefix.contains('/') {
                return self
                    .resource_options
                    .iter()
                    .map(ResourceDescriptor::gvr)
                    .filter(|candidate| candidate.starts_with(&prefix))
                    .collect();
            }
            return self
                .command_names()
                .into_iter()
                .filter(|candidate| candidate.to_ascii_lowercase().starts_with(&prefix))
                .collect();
        };
        let command = &input[..separator];
        let argument = input[separator..].trim_start();
        if argument.contains(char::is_whitespace) {
            return Vec::new();
        }
        let Ok(expanded) = self.user_config.expand_command(command) else {
            return Vec::new();
        };
        let mut expanded_parts = expanded.split_whitespace();
        let Some(expanded_command) = expanded_parts.next() else {
            return Vec::new();
        };
        // An alias with an existing namespace, context, or selector has already
        // supplied this argument; offering it again would alter the command.
        if expanded_parts.next().is_some() {
            return Vec::new();
        }
        let candidates = match expanded_command.to_ascii_lowercase().as_str() {
            "ctx" | "context" | "contexts" => self
                .context_options
                .iter()
                .map(|item| item.name.clone())
                .collect(),
            "ns" | "namespace" | "namespaces" => self.namespace_candidates(),
            alias => self.resolve_resource(alias).map_or_else(
                |_| Vec::new(),
                |resource| {
                    if resource.is_namespaced() {
                        self.namespace_candidates()
                    } else {
                        Vec::new()
                    }
                },
            ),
        };
        let argument = argument.to_ascii_lowercase();
        candidates
            .into_iter()
            .filter(|candidate| candidate.to_ascii_lowercase().starts_with(&argument))
            .map(|candidate| format!("{command} {candidate}"))
            .collect()
    }

    fn resolve_resource(&self, alias: &str) -> Result<ResourceDescriptor, String> {
        let alias = alias.to_ascii_lowercase();
        let mut matches: Vec<_> = self
            .resource_options
            .iter()
            .filter(|resource| {
                resource
                    .command_aliases()
                    .iter()
                    .any(|candidate| candidate.eq_ignore_ascii_case(&alias))
                    || resource.gvr().eq_ignore_ascii_case(&alias)
            })
            .cloned()
            .collect();
        matches.dedup();
        if matches.len() == 1 {
            return Ok(matches.remove(0));
        }
        if matches.len() > 1 {
            let mut core = matches.iter().filter(|resource| resource.group.is_empty());
            if let (Some(resource), None) = (core.next(), core.next()) {
                return Ok(resource.clone());
            }
            let choices = matches
                .iter()
                .map(ResourceDescriptor::qualified_name)
                .collect::<Vec<_>>()
                .join(", ");
            return Err(format!(
                "Ambiguous resource {alias:?}; use a qualified name: {choices}"
            ));
        }

        if let Some(kind) = ResourceKind::from_alias(&alias) {
            return Ok(ResourceDescriptor::from(kind));
        }
        Err(format!("Unknown command or resource alias: {alias}"))
    }

    fn command_names(&self) -> Vec<String> {
        let mut commands = BTreeSet::from([
            "screendump".to_owned(),
            "sd".to_owned(),
            "ctx".to_owned(),
            "context".to_owned(),
            "contexts".to_owned(),
            "ns".to_owned(),
            "namespace".to_owned(),
            "namespaces".to_owned(),
            "pf".to_owned(),
            "portforward".to_owned(),
            "portforwards".to_owned(),
            "can".to_owned(),
            "dir".to_owned(),
            "ls".to_owned(),
            "xray".to_owned(),
            "xr".to_owned(),
            "cow".to_owned(),
            "q".to_owned(),
            "quit".to_owned(),
            "help".to_owned(),
            "alias".to_owned(),
            "aliases".to_owned(),
            "suspend".to_owned(),
            "resume".to_owned(),
        ]);
        for resource in &self.resource_options {
            commands.extend(resource.command_aliases());
        }
        commands.extend(self.user_config.aliases.keys().cloned());
        commands.into_iter().collect()
    }

    fn namespace_candidates(&self) -> Vec<String> {
        let mut candidates = BTreeSet::from(["all".to_owned()]);
        candidates.extend(self.recent_namespaces.iter().cloned());
        candidates.extend(
            self.snapshot
                .rows
                .iter()
                .filter(|row| row.namespace.as_ref() != "-")
                .map(|row| row.namespace.to_string()),
        );
        candidates.extend(
            self.namespace_options
                .iter()
                .filter(|namespace| namespace.name != "<all>")
                .map(|namespace| namespace.name.clone()),
        );
        candidates.into_iter().collect()
    }

    fn navigate_history(&mut self, delta: isize) -> AppCommand {
        let next = self
            .history_cursor
            .saturating_add_signed(delta)
            .min(self.history.len().saturating_sub(1));
        if next == self.history_cursor {
            return AppCommand::None;
        }
        self.save_workloads_history();
        self.history_cursor = next;
        let command = self.request_view(self.history[next].clone(), false);
        self.restore_workloads_history();
        command
    }

    fn escape_view(&mut self) -> AppCommand {
        if let Some(command) = self.cancel_initial_reference_scan() {
            return command;
        }
        if let Some(command) = self.restore_references_on_escape() {
            return command;
        }
        if let Some(command) = self.cancel_pending_navigation() {
            return command;
        }
        if !self.is_resource_view() {
            return self.restore_resource_list();
        }
        if self.pending_containers.take().is_some()
            || self.pending_policy.take().is_some()
            || self.pending_child.take().is_some()
            || self.pending_owner.take().is_some()
        {
            self.notice = None;
            return AppCommand::None;
        }
        if self.query.is_empty() {
            self.navigate_history(-1)
        } else {
            self.query.clear();
            self.rebuild_filter(None);
            AppCommand::None
        }
    }

    fn open_overlay(&mut self, overlay: Overlay) -> AppCommand {
        self.overlay = overlay;
        self.overlay_selected = 0;
        self.reset_body_scroll();
        AppCommand::None
    }

    fn open_aliases(&mut self) -> AppCommand {
        self.overlay = Overlay::Aliases;
        self.body_query.clear();
        self.filter_error = None;
        self.overlay_selected = self
            .resource_options
            .iter()
            .position(|resource| resource == &self.kind)
            .unwrap_or(0);
        AppCommand::None
    }

    fn open_contexts(&mut self) -> AppCommand {
        self.overlay = Overlay::Contexts;
        self.body_query.clear();
        self.filter_error = None;
        self.overlay_selected = self
            .context_options
            .iter()
            .position(|context| context.name == self.context)
            .unwrap_or(0);
        AppCommand::None
    }

    #[must_use]
    pub fn visible_context_indices(&self) -> Vec<usize> {
        let matcher = FilterMatcher::new(&self.body_query);
        self.context_options
            .iter()
            .enumerate()
            .filter(|(_, context)| matcher.matches_text(&context.search_text()))
            .map(|(index, _)| index)
            .collect()
    }

    #[must_use]
    pub fn visible_resource_indices(&self) -> Vec<usize> {
        let matcher = FilterMatcher::new(&self.body_query);
        self.resource_options
            .iter()
            .enumerate()
            .filter(|(_, resource)| {
                let mut aliases = resource.command_aliases();
                for (custom_alias, target) in &self.user_config.aliases {
                    if target == &resource.gvr() || target == &resource.qualified_name() {
                        aliases.push(custom_alias.clone());
                    }
                }
                let searchable = format!("{} {}", resource.qualified_name(), aliases.join(" "));
                matcher.matches_text(&searchable)
            })
            .map(|(index, _)| index)
            .collect()
    }

    #[must_use]
    pub fn visible_namespace_indices(&self) -> Vec<usize> {
        let matcher = FilterMatcher::new(&self.body_query);
        self.namespace_options
            .iter()
            .enumerate()
            .filter(|(_, namespace)| matcher.matches_text(&namespace.search_text()))
            .map(|(index, _)| index)
            .collect()
    }

    fn select_first_visible_chooser_item(&mut self) {
        self.overlay_selected = match self.overlay {
            Overlay::Aliases => self.visible_resource_indices().first().copied(),
            Overlay::Namespaces => self.visible_namespace_indices().first().copied(),
            Overlay::Contexts => self.visible_context_indices().first().copied(),
            Overlay::ScreenDumps => self.visible_dump_indices().first().copied(),
            _ => None,
        }
        .unwrap_or(NO_CHOOSER_SELECTION);
    }

    fn open_namespaces(&mut self) -> AppCommand {
        let mut namespaces: BTreeMap<String, NamespaceInfo> = self
            .namespace_options
            .iter()
            .filter(|namespace| namespace.name != "<all>")
            .cloned()
            .map(|namespace| (namespace.name.clone(), namespace))
            .collect();
        for namespace in self
            .snapshot
            .rows
            .iter()
            .filter(|row| row.namespace.as_ref() != "-")
            .map(|row| row.namespace.to_string())
        {
            namespaces
                .entry(namespace.clone())
                .or_insert_with(|| NamespaceInfo::unknown(namespace));
        }
        if let Some(namespace) = &self.namespace {
            namespaces
                .entry(namespace.clone())
                .or_insert_with(|| NamespaceInfo::unknown(namespace.clone()));
        }
        self.namespace_options = std::iter::once(NamespaceInfo::all())
            .chain(namespaces.into_values())
            .collect();
        self.overlay = Overlay::Namespaces;
        self.body_query.clear();
        self.filter_error = None;
        let current = self.namespace.as_deref().unwrap_or("<all>");
        self.overlay_selected = self
            .namespace_options
            .iter()
            .position(|namespace| namespace.name == current)
            .unwrap_or(0);
        AppCommand::None
    }

    fn set_sort(&mut self, field: SortField) {
        self.select_workload_sort_column(field);
        if self.column_sort_override.take().is_none() && self.sort_field == field {
            self.sort_descending = !self.sort_descending;
        } else {
            self.sort_field = field;
            self.sort_descending = false;
        }
        let selected = self.selected_row().map(|row| Arc::clone(&row.key));
        self.rebuild_filter(selected.as_deref());
    }

    fn set_column_sort(&mut self, key: ColumnSortKey) {
        match key {
            ColumnSortKey::Field(field) => {
                self.selected_column = SortField::ALL
                    .iter()
                    .position(|candidate| *candidate == field)
                    .unwrap_or(self.selected_column);
                self.set_sort(field);
            }
            other => {
                if self.column_sort_override.as_ref() == Some(&other) {
                    self.sort_descending = !self.sort_descending;
                } else {
                    self.column_sort_override = Some(other);
                    self.sort_descending = false;
                }
                let selected = self.selected_row().map(|row| Arc::clone(&row.key));
                self.rebuild_filter(selected.as_deref());
            }
        }
    }

    #[allow(clippy::too_many_lines)]
    fn sort_overlay_by_column(&mut self, key: &ColumnSortKey) {
        if self.column_sort_override.as_ref() == Some(key) {
            self.sort_descending = !self.sort_descending;
        } else {
            self.column_sort_override = Some(key.clone());
            self.sort_descending = false;
        }
        let desc = self.sort_descending;
        let ColumnSortKey::Computed(col) = key else {
            return;
        };
        match (self.overlay, *col) {
            (Overlay::Aliases, "RESOURCE") => {
                self.resource_options.sort_by(|a, b| {
                    let ord = a.qualified_name().cmp(&b.qualified_name());
                    if desc { ord.reverse() } else { ord }
                });
                self.select_first_visible_chooser_item();
            }
            (Overlay::Aliases, "ALIASES") => {
                let aliases = &self.user_config.aliases;
                let alias_str = |res: &ResourceDescriptor| {
                    let mut list = res.command_aliases();
                    for (custom_alias, target) in aliases {
                        if target == &res.gvr() || target == &res.qualified_name() {
                            list.push(custom_alias.clone());
                        }
                    }
                    list.join(" ")
                };
                self.resource_options.sort_by(|a, b| {
                    let ord = alias_str(a)
                        .cmp(&alias_str(b))
                        .then_with(|| a.qualified_name().cmp(&b.qualified_name()));
                    if desc { ord.reverse() } else { ord }
                });
                self.select_first_visible_chooser_item();
            }
            (Overlay::Namespaces, "NAME") => {
                self.namespace_options.sort_by(|a, b| {
                    let ord = a.name.cmp(&b.name);
                    if desc { ord.reverse() } else { ord }
                });
                self.select_first_visible_chooser_item();
            }
            (Overlay::Namespaces, "STATUS") => {
                self.namespace_options.sort_by(|a, b| {
                    let ord = a.status.cmp(&b.status).then_with(|| a.name.cmp(&b.name));
                    if desc { ord.reverse() } else { ord }
                });
                self.select_first_visible_chooser_item();
            }
            (Overlay::Namespaces, "AGE") => {
                let now = self.now_unix;
                self.namespace_options.sort_by(|a, b| {
                    let ord = a
                        .age_seconds(now)
                        .cmp(&b.age_seconds(now))
                        .then_with(|| a.name.cmp(&b.name));
                    if desc { ord.reverse() } else { ord }
                });
                self.select_first_visible_chooser_item();
            }
            (Overlay::Contexts, "ACTIVE") => {
                let active = &self.context;
                self.context_options.sort_by(|a, b| {
                    let ord = (b.name == *active)
                        .cmp(&(a.name == *active))
                        .then_with(|| a.name.cmp(&b.name));
                    if desc { ord.reverse() } else { ord }
                });
                self.select_first_visible_chooser_item();
            }
            (Overlay::Contexts, "NAME") => {
                self.context_options.sort_by(|a, b| {
                    let ord = a.name.cmp(&b.name);
                    if desc { ord.reverse() } else { ord }
                });
                self.select_first_visible_chooser_item();
            }
            (Overlay::Contexts, "CLUSTER") => {
                self.context_options.sort_by(|a, b| {
                    let ord = a
                        .cluster_display()
                        .cmp(b.cluster_display())
                        .then_with(|| a.name.cmp(&b.name));
                    if desc { ord.reverse() } else { ord }
                });
                self.select_first_visible_chooser_item();
            }
            (Overlay::Contexts, "USER") => {
                self.context_options.sort_by(|a, b| {
                    let ord = a
                        .user_display()
                        .cmp(b.user_display())
                        .then_with(|| a.name.cmp(&b.name));
                    if desc { ord.reverse() } else { ord }
                });
                self.select_first_visible_chooser_item();
            }
            (Overlay::Contexts, "NAMESPACE") => {
                self.context_options.sort_by(|a, b| {
                    let ord = a
                        .namespace_display()
                        .cmp(b.namespace_display())
                        .then_with(|| a.name.cmp(&b.name));
                    if desc { ord.reverse() } else { ord }
                });
                self.select_first_visible_chooser_item();
            }
            (Overlay::PortForwards, "NAMESPACE") => {
                self.port_forwards.sort_by(|a, b| {
                    let ord = a
                        .target
                        .namespace
                        .as_deref()
                        .unwrap_or("-")
                        .cmp(b.target.namespace.as_deref().unwrap_or("-"))
                        .then_with(|| a.target.name.cmp(&b.target.name));
                    if desc { ord.reverse() } else { ord }
                });
            }
            (Overlay::PortForwards, "KIND") => {
                self.port_forwards.sort_by(|a, b| {
                    let ord = a
                        .target
                        .kind
                        .kind
                        .cmp(&b.target.kind.kind)
                        .then_with(|| a.target.name.cmp(&b.target.name));
                    if desc { ord.reverse() } else { ord }
                });
            }
            (Overlay::PortForwards, "NAME") => {
                self.port_forwards.sort_by(|a, b| {
                    let ord = a.target.name.cmp(&b.target.name);
                    if desc { ord.reverse() } else { ord }
                });
            }
            (Overlay::PortForwards, "LOCAL->REMOTE") => {
                self.port_forwards.sort_by(|a, b| {
                    let ord = (a.local_port, a.remote_port).cmp(&(b.local_port, b.remote_port));
                    if desc { ord.reverse() } else { ord }
                });
            }
            (Overlay::PortForwards, "ADDRESS") => {
                self.port_forwards.sort_by(|a, b| {
                    let ord = (&a.address, a.local_port).cmp(&(&b.address, b.local_port));
                    if desc { ord.reverse() } else { ord }
                });
            }
            (Overlay::PortForwards, "STATUS") => {
                self.port_forwards.sort_by(|a, b| {
                    let ord = a
                        .state
                        .label()
                        .cmp(b.state.label())
                        .then_with(|| a.target.name.cmp(&b.target.name));
                    if desc { ord.reverse() } else { ord }
                });
            }
            (Overlay::ScreenDumps, "NAME") => {
                self.dumps.entries.sort_by(|a, b| {
                    let ord = a.name.cmp(&b.name);
                    if desc { ord.reverse() } else { ord }
                });
                self.select_first_visible_chooser_item();
            }
            (Overlay::ScreenDumps, "DIR") => {
                self.dumps.entries.sort_by(|a, b| {
                    let ord = a
                        .dir_display()
                        .cmp(&b.dir_display())
                        .then_with(|| a.name.cmp(&b.name));
                    if desc { ord.reverse() } else { ord }
                });
                self.select_first_visible_chooser_item();
            }
            (Overlay::ScreenDumps, "SIZE") => {
                self.dumps.entries.sort_by(|a, b| {
                    let ord = a
                        .size_bytes
                        .cmp(&b.size_bytes)
                        .then_with(|| a.name.cmp(&b.name));
                    if desc { ord.reverse() } else { ord }
                });
                self.select_first_visible_chooser_item();
            }
            (Overlay::ScreenDumps, "AGE") => {
                self.dumps.entries.sort_by(|a, b| {
                    let ord = b
                        .modified_unix
                        .cmp(&a.modified_unix)
                        .then_with(|| a.name.cmp(&b.name));
                    if desc { ord.reverse() } else { ord }
                });
                self.select_first_visible_chooser_item();
            }
            _ => {}
        }
    }

    fn rebuild_filter(&mut self, selected_key: Option<&str>) {
        let matcher = FilterMatcher::new(&self.query);
        let faults_only = self.ui.faults() && !self.is_reference_view();
        let custom_paths: Vec<String> = self
            .is_resource_view()
            .then(|| {
                self.user_config
                    .custom_view_for(&self.kind, self.namespace.as_deref())
            })
            .flatten()
            .map(|view| {
                view.columns
                    .iter()
                    .filter_map(|col| col.json_path.clone())
                    .collect()
            })
            .unwrap_or_default();
        self.filter_error = matcher.error().map(str::to_owned);
        self.filtered.clear();
        self.filtered.extend(
            self.snapshot
                .rows
                .iter()
                .enumerate()
                .filter(|(_, row)| matcher.matches_with_paths(row, &custom_paths))
                .filter(|(_, row)| !faults_only || row_is_fault(&self.kind, row))
                .map(|(index, _)| index),
        );

        let rows = &self.snapshot.rows;
        let sort_field = self.sort_field;
        let descending = self.sort_descending;
        let workloads = self.kind.is_workloads();
        let pulses = self.kind.is_pulses();
        let xray = self.kind.is_xray();
        let scans = self.kind.is_scans();
        let scan_settings = &self.user_config.settings.image_scans;
        if xray && !self.xray_collapsed.is_empty() {
            self.filtered.retain(|index| {
                let Some(xr) = self.snapshot.rows[*index].xray_data() else {
                    return true;
                };
                !self.xray_collapsed.iter().any(|collapsed| {
                    xr.path.len() > collapsed.len() + 2
                        && xr.path.starts_with(collapsed.as_ref())
                        && xr.path.as_bytes()[collapsed.len()..].starts_with(b"::")
                })
            });
        }
        let now = self.now_unix;
        let sort_override = self.column_sort_override.as_ref();
        if !pulses && !xray {
            self.filtered.sort_unstable_by(|left, right| {
                let ordering = if let Some(override_key) = sort_override {
                    compare_rows_by_override(&rows[*left], &rows[*right], override_key)
                } else if workloads {
                    navigation::compare_workload_rows(&rows[*left], &rows[*right], sort_field, now)
                } else if scans && sort_field == SortField::Status {
                    crate::scans::sev_rank(&rows[*left].status)
                        .cmp(&crate::scans::sev_rank(&rows[*right].status))
                        .then_with(|| rows[*left].details.cmp(&rows[*right].details))
                        .then_with(|| rows[*left].name.cmp(&rows[*right].name))
                } else if sort_field == SortField::VulnScore {
                    row_vuln_score(&rows[*left], scan_settings)
                        .cmp(&row_vuln_score(&rows[*right], scan_settings))
                        .then_with(|| rows[*left].key.cmp(&rows[*right].key))
                } else {
                    compare_rows(&rows[*left], &rows[*right], sort_field)
                };
                if descending {
                    ordering.reverse()
                } else {
                    ordering
                }
            });
        }

        self.selected = selected_key
            .and_then(|key| {
                self.filtered
                    .iter()
                    .position(|index| rows[*index].key.as_ref() == key)
            })
            .unwrap_or(0)
            .min(self.filtered.len().saturating_sub(1));
        self.ensure_selection_visible();
    }

    fn move_selection_down(&mut self, amount: usize) {
        if !self.filtered.is_empty() {
            self.select(self.selected.saturating_add(amount));
        }
    }

    fn move_selection_up(&mut self, amount: usize) {
        if !self.filtered.is_empty() {
            self.select(self.selected.saturating_sub(amount));
        }
    }

    fn cycle_filtered_selection(&mut self, forward: bool) {
        if self.filtered.is_empty() {
            return;
        }
        let selected = if forward {
            (self.selected + 1) % self.filtered.len()
        } else {
            self.selected
                .checked_sub(1)
                .unwrap_or(self.filtered.len() - 1)
        };
        self.select(selected);
    }

    fn select(&mut self, selected: usize) {
        self.selected = selected.min(self.filtered.len().saturating_sub(1));
        self.ensure_selection_visible();
    }

    fn ensure_selection_visible(&mut self) {
        if self.selected < self.offset {
            self.offset = self.selected;
        } else if self.selected >= self.offset.saturating_add(self.viewport_height) {
            self.offset = self
                .selected
                .saturating_add(1)
                .saturating_sub(self.viewport_height);
        }
        self.offset = self.offset.min(self.filtered.len().saturating_sub(1));
    }

    fn toggle_mark(&mut self) {
        let key = self.selected_row().map(|row| Arc::clone(&row.key));
        if let Some(key) = key {
            if !self.marks.remove(&key) {
                self.marks.insert(key);
            }
            self.mark_anchor = Some(self.selected);
        }
    }

    fn mark_range(&mut self) {
        let start = self.mark_anchor.unwrap_or(self.selected).min(self.selected);
        let end = self.mark_anchor.unwrap_or(self.selected).max(self.selected);
        for position in start..=end {
            if let Some(row) = self
                .filtered
                .get(position)
                .and_then(|index| self.snapshot.rows.get(*index))
            {
                self.marks.insert(Arc::clone(&row.key));
            }
        }
        self.mark_anchor = Some(self.selected);
    }

    fn row_target(&self, row: &ResourceRow) -> Option<ResourceTarget> {
        let kind = if self.kind.is_workloads() {
            let ResourceData::Workload { kind } = &row.data else {
                return None;
            };
            kind.clone()
        } else if self.kind.is_xray() {
            let xray = row.xray_data()?;
            if xray.is_container {
                return None;
            }
            xray.target_kind.clone()
        } else {
            self.kind.clone()
        };
        Some(ResourceTarget {
            kind,
            namespace: (row.namespace.as_ref() != "-").then(|| row.namespace.to_string()),
            name: row.name.to_string(),
        })
    }

    fn selected_target(&self) -> Option<ResourceTarget> {
        if !self.is_resource_view() {
            return None;
        }
        self.selected_row().and_then(|row| self.row_target(row))
    }

    fn selected_or_marked_targets(&self) -> Vec<ResourceTarget> {
        if !self.is_resource_view() {
            return Vec::new();
        }
        let marked = self
            .snapshot
            .rows
            .iter()
            .filter(|row| self.marks.contains(&row.key))
            .filter_map(|row| self.row_target(row))
            .collect::<Vec<_>>();
        if marked.is_empty() {
            self.selected_target().into_iter().collect()
        } else {
            marked
        }
    }

    fn open_virtual_selected(&mut self) -> Option<AppCommand> {
        if !self.is_resource_view() {
            return None;
        }
        if self.kind.is_workloads() {
            Some(self.open_workload_instance())
        } else if self.kind.is_pulses() {
            Some(self.open_pulse_target())
        } else if self.kind.is_helm() {
            Some(self.open_helm_releases())
        } else if self.kind.is_helm_history() {
            Some(self.open_helm_rollback_confirmation())
        } else if self.kind.is_dir() {
            Some(self.open_dir_entry())
        } else if self.kind.is_benchmarks() {
            Some(self.content_action(ContentKind::Yaml))
        } else if self.kind.is_xray() {
            Some(self.open_xray_node())
        } else if self.kind.is_scans() {
            Some(self.open_scan_cve())
        } else if self.kind.is_skins() {
            Some(self.apply_selected_skin())
        } else {
            None
        }
    }

    fn open_selected(&mut self) -> AppCommand {
        if let Some(command) = self.open_virtual_selected() {
            return command;
        }
        if self.is_container_view() {
            return self.container_log_action(false);
        }
        if self.is_policy_view() {
            return AppCommand::None;
        }
        if self.kind.known_kind() == Some(ResourceKind::Deployments) {
            return self.open_deployment_pods();
        }
        if self.kind.is_cron_job() || self.kind.is_job() {
            return self.open_batch_children();
        }
        if self.kind.is_authorization_resource() {
            let Some(target) = self.selected_target() else {
                return AppCommand::None;
            };
            self.pending_policy = Some(target.clone());
            return AppCommand::Action(ActionIntent::AuthorizationPolicy { target });
        }
        if self.kind.is_core_v1_namespace() {
            let Some(name) = self.selected_row().map(|row| row.name.to_string()) else {
                return AppCommand::None;
            };
            let namespace = (!matches!(name.as_str(), "all" | "<all>")).then_some(name);
            self.use_active_namespace(namespace.clone());
            self.query.clear();
            return self.request_view(
                ViewState {
                    instance: None,
                    context: self.context.clone(),
                    kind: ResourceDescriptor::from(ResourceKind::Pods),
                    namespace,
                    field_selector: None,
                    label_selector: None,
                },
                true,
            );
        }
        if let Some(command) = self.open_custom_jump() {
            return command;
        }
        if let Some(command) = self.open_child_resource() {
            return command;
        }
        if let Some(command) = self.open_custom_resource_definition() {
            return command;
        }
        match self.kind.known_kind() {
            Some(ResourceKind::Nodes) => {
                let Some(node) = self.selected_row().map(|row| row.name.to_string()) else {
                    return AppCommand::None;
                };
                self.query.clear();
                self.request_view(
                    ViewState {
                        instance: None,
                        context: self.context.clone(),
                        kind: ResourceDescriptor::from(ResourceKind::Pods),
                        namespace: None,
                        field_selector: Some(format!("spec.nodeName={node}")),
                        label_selector: None,
                    },
                    true,
                )
            }
            Some(ResourceKind::Pods) => {
                let Some(target) = self.selected_target() else {
                    return AppCommand::None;
                };
                self.pending_containers = Some(target.clone());
                AppCommand::Action(ActionIntent::Containers { target })
            }
            _ => {
                let Some(target) = self.selected_target() else {
                    return AppCommand::None;
                };
                let Some(row) = self.selected_row().cloned() else {
                    return AppCommand::None;
                };
                self.pending_child = Some((target.clone(), row));
                AppCommand::Action(ActionIntent::FindChildResource {
                    target,
                    resources: self.resource_options.clone(),
                })
            }
        }
    }

    fn open_pulse_target(&mut self) -> AppCommand {
        let Some(pulse) = self.selected_row().and_then(ResourceRow::pulse_data) else {
            return AppCommand::None;
        };
        let target_kind = pulse.target_kind.clone();
        let namespace = target_kind
            .is_namespaced()
            .then(|| self.namespace.clone())
            .flatten();
        if !self.resource_options.contains(&target_kind) {
            self.resource_options.push(target_kind.clone());
        }
        self.request_view(
            ViewState {
                instance: None,
                context: self.context.clone(),
                kind: target_kind,
                namespace,
                field_selector: None,
                label_selector: None,
            },
            true,
        )
    }

    fn open_xray_node(&mut self) -> AppCommand {
        let Some(row) = self.selected_row() else {
            return AppCommand::None;
        };
        let Some(xray) = row.xray_data() else {
            return AppCommand::None;
        };
        if xray.is_container {
            return self.xray_log_action(false);
        }
        if xray.is_namespace {
            let ns = row.name.to_string();
            return self.request_view(
                ViewState {
                    instance: None,
                    context: self.context.clone(),
                    kind: ResourceDescriptor::from(ResourceKind::Pods),
                    namespace: Some(ns),
                    field_selector: None,
                    label_selector: None,
                },
                true,
            );
        }
        let target_kind = xray.target_kind.clone();
        let ns = (row.namespace.as_ref() != "-").then(|| row.namespace.to_string());
        let name = row.name.to_string();
        if !self.resource_options.contains(&target_kind) {
            self.resource_options.push(target_kind.clone());
        }
        self.request_view(
            ViewState {
                instance: Some(ResourceTarget {
                    kind: target_kind.clone(),
                    namespace: ns.clone(),
                    name,
                }),
                context: self.context.clone(),
                kind: target_kind,
                namespace: ns,
                field_selector: None,
                label_selector: None,
            },
            true,
        )
    }

    fn open_vulnerability_scans(&mut self) -> AppCommand {
        let Some(row) = self.selected_row() else {
            return AppCommand::None;
        };
        if self
            .user_config
            .settings
            .image_scans
            .should_exclude(&row.namespace, &row.labels)
        {
            return self
                .unsupported("Selected resource is excluded from image vulnerability scans");
        }
        let images = row.extract_container_images();
        if images.is_empty() {
            return self.unsupported("Selected resource has no container images to scan");
        }
        self.request_view(
            ViewState {
                instance: None,
                context: self.context.clone(),
                kind: ResourceDescriptor::scans(),
                namespace: None,
                field_selector: Some(images.join("\n")),
                label_selector: None,
            },
            true,
        )
    }

    fn open_scan_cve(&mut self) -> AppCommand {
        let Some(row) = self.selected_row() else {
            return AppCommand::None;
        };
        let url = crate::scans::cve_advisory_url(&row.name);
        self.show_transient_notice(format!("Opening advisory {url}"));
        AppCommand::OpenUrl(url)
    }

    fn apply_selected_skin(&mut self) -> AppCommand {
        let Some(name) = self.selected_row().map(|row| row.name.to_string()) else {
            return AppCommand::None;
        };
        let paths = crate::config::ConfigPaths::from_env().unwrap_or_else(|_| {
            crate::config::ConfigPaths::new("/tmp/kux/config", "/tmp/kux/data")
        });
        match crate::theme::Skin::load_named(&paths, &name) {
            Ok(skin) => {
                self.theme = skin.clone();
                self.user_config.theme = skin;
                self.user_config.settings.ui.skin = Some(name.clone());
                self.show_transient_notice(format!("Applied skin {name}"));
            }
            Err(error) => {
                self.show_transient_notice(error);
            }
        }
        AppCommand::None
    }

    fn open_helm_releases(&mut self) -> AppCommand {
        let Some(row) = self.selected_row() else {
            return AppCommand::None;
        };
        let namespace = if row.namespace.as_ref() == "-" {
            self.namespace
                .clone()
                .unwrap_or_else(|| "default".to_owned())
        } else {
            row.namespace.to_string()
        };
        let release = row.name.to_string();
        self.request_view(
            ViewState {
                instance: None,
                context: self.context.clone(),
                kind: ResourceDescriptor::helm_history(),
                namespace: Some(namespace.clone()),
                field_selector: Some(format!("{namespace}/{release}")),
                label_selector: None,
            },
            true,
        )
    }

    fn open_helm_rollback_confirmation(&mut self) -> AppCommand {
        if self.read_only {
            return self.unsupported("Helm rollback is disabled in read-only mode");
        }
        let Some(row) = self.selected_row() else {
            return AppCommand::None;
        };
        let namespace = if row.namespace.as_ref() == "-" {
            self.namespace
                .clone()
                .unwrap_or_else(|| "default".to_owned())
        } else {
            row.namespace.to_string()
        };
        let (release, revision) = row
            .name
            .split_once(':')
            .map_or((row.name.as_ref(), row.ready.as_ref()), |(r, rev)| (r, rev));
        self.open_action_confirmation(PendingDelete::HelmRollback {
            namespace,
            release: release.to_owned(),
            revision: revision.to_owned(),
        })
    }

    fn open_dir_entry(&mut self) -> AppCommand {
        let Some(row) = self.selected_row() else {
            return AppCommand::None;
        };
        if row.status.as_ref() != "dir" {
            return self.unsupported("You must select a directory");
        }
        let path = row.name.to_string();
        self.request_view(
            ViewState {
                instance: None,
                context: self.context.clone(),
                kind: ResourceDescriptor::dir(),
                namespace: None,
                field_selector: Some(path),
                label_selector: None,
            },
            true,
        )
    }

    fn dir_edit_action(&mut self) -> AppCommand {
        if self.read_only {
            return self.unsupported("Edit is disabled in read-only mode");
        }
        let Some(row) = self.selected_row() else {
            return AppCommand::None;
        };
        if row.status.as_ref() == "dir" {
            return self.unsupported("You must select a manifest");
        }
        AppCommand::EditDump(std::path::PathBuf::from(row.name.as_ref()))
    }

    fn dir_apply_action(&mut self) -> AppCommand {
        if self.read_only {
            return self.unsupported("Apply is disabled in read-only mode");
        }
        let Some(row) = self.selected_row() else {
            return AppCommand::None;
        };
        AppCommand::Action(ActionIntent::DirApply {
            path: row.name.to_string(),
        })
    }

    fn open_dir_delete_confirmation(&mut self) -> AppCommand {
        if self.read_only {
            return self.unsupported("Delete is disabled in read-only mode");
        }
        let Some(row) = self.selected_row() else {
            return AppCommand::None;
        };
        self.open_action_confirmation(PendingDelete::DirDelete {
            path: row.name.to_string(),
        })
    }

    fn benchmark_service_action(&mut self) -> AppCommand {
        let Some(row) = self.selected_row() else {
            return AppCommand::None;
        };
        let svc_type = row.status.as_ref();
        if !matches!(svc_type, "NodePort" | "LoadBalancer") {
            return self
                .unsupported("You must select a reachable service (NodePort or LoadBalancer)");
        }
        let Some(port) = row
            .port_forward_ports
            .as_ref()
            .and_then(|ports| ports.first())
            .map(|p| p.port)
        else {
            return self.unsupported("Service exposes no TCP ports to benchmark");
        };
        let non_empty_json = |path: &str| {
            let val = row.json_path_value(path);
            (!val.is_empty() && val != "<none>" && val != "-").then_some(val)
        };
        let ip = non_empty_json(".status.loadBalancer.ingress[0].ip")
            .or_else(|| non_empty_json(".status.loadBalancer.ingress[0].hostname"))
            .or_else(|| non_empty_json(".spec.externalIPs[0]"))
            .unwrap_or_else(|| {
                if row.details.as_ref() != "-" && !row.details.is_empty() {
                    row.details.to_string()
                } else {
                    "127.0.0.1".to_owned()
                }
            });
        let namespace = if row.namespace.as_ref() == "-" {
            self.namespace
                .clone()
                .unwrap_or_else(|| "default".to_owned())
        } else {
            row.namespace.to_string()
        };
        let name = row.name.to_string();
        let fqn = format!("{namespace}/{name}");
        let cfg = self.user_config.bench_config_for_service(&fqn);
        let path = if cfg.http.path.starts_with('/') {
            cfg.http.path.clone()
        } else {
            format!("/{}", cfg.http.path)
        };
        let base_url = format!("http://{ip}:{port}{path}");
        let cluster = self.active_context_info().cluster.clone();
        AppCommand::RunBenchmark(crate::benchmarks::BenchmarkRunRequest {
            cluster,
            context: self.context.clone(),
            namespace,
            target_name: format!("{name}:{port}"),
            base_url,
            config: cfg,
        })
    }

    fn benchmark_selected_port_forward(&mut self) -> AppCommand {
        let Some(forward) = self.port_forwards.get(self.overlay_selected) else {
            return AppCommand::None;
        };
        let namespace = forward
            .target
            .namespace
            .clone()
            .unwrap_or_else(|| "default".to_owned());
        let name = forward.target.name.clone();
        let remote_port = forward.remote_port;
        let local_port = forward.local_port;
        let address = forward.address.clone();
        let key = format!("{namespace}/{name}:{remote_port}");
        let cfg = self.user_config.bench_config_for_container(&key);
        let path = if cfg.http.path.starts_with('/') {
            cfg.http.path.clone()
        } else {
            format!("/{}", cfg.http.path)
        };
        let base_url = format!("http://{address}:{local_port}{path}");
        let cluster = self.active_context_info().cluster.clone();
        AppCommand::RunBenchmark(crate::benchmarks::BenchmarkRunRequest {
            cluster,
            context: self.context.clone(),
            namespace,
            target_name: format!("{name}:{remote_port}"),
            base_url,
            config: cfg,
        })
    }

    fn open_selected_port_forward_benchmarks(&mut self) -> AppCommand {
        let Some(forward) = self.port_forwards.get(self.overlay_selected) else {
            return AppCommand::None;
        };
        let namespace = forward
            .target
            .namespace
            .clone()
            .unwrap_or_else(|| "default".to_owned());
        let prefix = format!(
            "{namespace}/{}:{}",
            forward.target.name, forward.remote_port
        );
        self.port_forwards.clear();
        self.overlay = Overlay::None;
        self.request_view(
            ViewState {
                instance: None,
                context: self.context.clone(),
                kind: ResourceDescriptor::benchmarks(),
                namespace: None,
                field_selector: Some(prefix),
                label_selector: None,
            },
            true,
        )
    }

    fn open_custom_jump(&mut self) -> Option<AppCommand> {
        let jump = self.user_config.custom_jump_for(&self.kind)?.clone();
        let row = self.selected_row()?;
        let row_name = row.name.to_string();
        let row_namespace = (row.namespace.as_ref() != "-").then(|| row.namespace.to_string());
        let kind = match self.resolve_resource(&jump.resource) {
            Ok(kind) => kind,
            Err(message) => {
                return Some(self.unsupported(&message));
            }
        };
        let namespace = if kind.is_namespaced() {
            if let Some(ns) = &jump.namespace {
                (!matches!(ns.as_str(), "all" | "*" | "-A")).then(|| ns.clone())
            } else {
                row_namespace.or_else(|| self.namespace.clone())
            }
        } else {
            None
        };
        let command = self.request_view(
            ViewState {
                instance: None,
                context: self.context.clone(),
                kind,
                namespace,
                field_selector: None,
                label_selector: None,
            },
            true,
        );
        self.query = row_name;
        self.filter_error = None;
        Some(command)
    }

    fn open_child_resource(&mut self) -> Option<AppCommand> {
        let child = self.selected_row().and_then(|row| {
            row.child_resource_data()
                .map(|data| (row.namespace.to_string(), data.clone()))
        })?;
        let namespace = (child.0 != "-").then_some(child.0);
        Some(self.request_view(
            ViewState {
                instance: None,
                context: self.context.clone(),
                kind: child.1.kind,
                namespace,
                field_selector: None,
                label_selector: Some(child.1.label_selector.to_string()),
            },
            true,
        ))
    }

    fn open_custom_resource_definition(&mut self) -> Option<AppCommand> {
        if !self.kind.is_custom_resource_definition() {
            return None;
        }
        let crd_name = self.selected_row()?.name.as_ref();
        let kind = self
            .resource_options
            .iter()
            .find(|resource| {
                !resource.group.is_empty()
                    && format!("{}.{}", resource.plural, resource.group) == crd_name
            })?
            .clone();
        Some(self.request_view(
            ViewState {
                instance: None,
                context: self.context.clone(),
                kind,
                namespace: None,
                field_selector: None,
                label_selector: None,
            },
            true,
        ))
    }

    pub fn show_discovered_child(
        &mut self,
        parent: &ResourceTarget,
        kind: ResourceDescriptor,
        owner_uid: &str,
    ) -> AppCommand {
        if self.pending_child.as_ref().map(|pending| &pending.0) != Some(parent) {
            return AppCommand::None;
        }
        self.pending_child = None;
        let namespace = kind
            .is_namespaced()
            .then(|| parent.namespace.clone())
            .flatten();
        self.request_view(
            ViewState {
                instance: None,
                context: self.context.clone(),
                kind,
                namespace,
                field_selector: Some(format!("kux.ownerUID={owner_uid}")),
                label_selector: None,
            },
            true,
        )
    }

    pub fn show_no_discovered_child(&mut self, parent: &ResourceTarget) {
        let Some((pending, row)) = self.pending_child.take() else {
            return;
        };
        if &pending != parent {
            return;
        }
        self.details_row = Some(row);
        self.body_query.clear();
        self.reset_body_scroll();
        self.overlay = Overlay::Details;
        self.notice = None;
    }

    fn open_owner_resource(&mut self) -> AppCommand {
        if !self.is_resource_view() {
            return self.unsupported("Synthetic rows do not have Kubernetes owners");
        }
        if let Some((namespace, owner)) = self.selected_row().and_then(|row| {
            row.owner
                .as_ref()
                .map(|owner| (row.namespace.to_string(), owner.clone()))
        }) {
            return self.open_owner_reference((namespace != "-").then_some(namespace), &owner);
        }
        let Some(target) = self.selected_target() else {
            return AppCommand::None;
        };
        self.pending_owner = Some(target.clone());
        AppCommand::Action(ActionIntent::ResolveOwner { target })
    }

    pub fn show_resolved_owner(
        &mut self,
        target: &ResourceTarget,
        owner: Option<OwnerReferenceData>,
    ) -> AppCommand {
        if self.pending_owner.as_ref() != Some(target) {
            return AppCommand::None;
        }
        self.pending_owner = None;
        self.notice = None;
        let Some(owner) = owner else {
            return self.unsupported("Selected resource has no owner");
        };
        self.open_owner_reference(target.namespace.clone(), &owner)
    }

    fn open_owner_reference(
        &mut self,
        namespace: Option<String>,
        owner: &OwnerReferenceData,
    ) -> AppCommand {
        let Some(kind) = self.owner_kind(owner) else {
            let message = format!(
                "Owner kind {}/{} is not available in discovery",
                owner.api_version, owner.kind
            );
            return self.unsupported(&message);
        };
        let namespace = namespace.filter(|_| kind.is_namespaced());
        self.request_view(
            ViewState {
                instance: None,
                context: self.context.clone(),
                kind,
                namespace,
                field_selector: Some(format!("metadata.name={}", owner.name)),
                label_selector: None,
            },
            true,
        )
    }

    fn owner_kind(&self, owner: &OwnerReferenceData) -> Option<ResourceDescriptor> {
        self.resource_options
            .iter()
            .find(|resource| {
                resource.api_version == owner.api_version && resource.kind == owner.kind
            })
            .cloned()
    }

    fn container_log_action(&mut self, previous: bool) -> AppCommand {
        let ListMode::Containers { namespace, pod } = &self.list_mode else {
            return AppCommand::None;
        };
        let Some(row) = self.selected_row() else {
            return AppCommand::None;
        };
        let target = LogTarget::pod(
            namespace.clone(),
            pod.clone(),
            Some(row.name.to_string()),
            previous,
        );
        self.open_logs(target)
    }

    fn resource_log_action(&mut self, previous: bool) -> AppCommand {
        if !self.is_resource_view() || !supports_logs(&self.kind) {
            return AppCommand::None;
        }
        let Some(target) = self.selected_target() else {
            return AppCommand::None;
        };
        if target.namespace.as_deref().is_none_or(str::is_empty) {
            return self.unsupported("Log target namespace is unavailable; refresh and retry");
        }
        self.open_logs(LogTarget::workload(target, previous))
    }

    fn open_pod_node(&mut self) -> AppCommand {
        let Some(node) = self
            .selected_row()
            .and_then(ResourceRow::pod_data)
            .map(|data| data.node.to_string())
            .filter(|node| !node.is_empty() && node != "-")
        else {
            return self.unsupported("Pod is not assigned to a Node");
        };
        self.request_view(
            ViewState {
                instance: None,
                context: self.context.clone(),
                kind: ResourceKind::Nodes.into(),
                namespace: None,
                field_selector: Some(format!("metadata.name={node}")),
                label_selector: None,
            },
            true,
        )
    }

    fn node_shell_action(&mut self) -> AppCommand {
        if self.read_only {
            return self.unsupported("Node shell is disabled in read-only mode");
        }
        let Some(node) = self.selected_row().map(|row| row.name.to_string()) else {
            return AppCommand::None;
        };
        AppCommand::NodeShell(NodeShellTarget {
            node,
            settings: self.user_config.shell_pod_for(&self.context).clone(),
        })
    }

    fn pod_shell_action(&mut self) -> AppCommand {
        if self.read_only {
            return self.unsupported("Shell is disabled in read-only mode");
        }
        let Some(container) = self
            .selected_row()
            .and_then(ResourceRow::pod_data)
            .map(|data| data.default_container.to_string())
            .filter(|container| !container.is_empty())
        else {
            return self.unsupported("Pod has no regular containers");
        };
        let Some(target) = self.selected_target() else {
            return AppCommand::None;
        };
        let Some(namespace) = target.namespace else {
            return self.unsupported("Pod shell requires a namespace");
        };
        AppCommand::Shell(PodShellTarget {
            namespace,
            pod: target.name,
            container,
        })
    }

    fn attach_action(&mut self) -> AppCommand {
        if self.read_only {
            return self.unsupported("Attach is disabled in read-only mode");
        }
        let command = if self.is_container_view() {
            self.container_shell_action()
        } else {
            self.pod_shell_action()
        };
        match command {
            AppCommand::Shell(target) => AppCommand::Attach(target),
            other => other,
        }
    }

    fn container_shell_action(&mut self) -> AppCommand {
        if self.read_only {
            return self.unsupported("Shell is disabled in read-only mode");
        }
        let ListMode::Containers { namespace, pod } = &self.list_mode else {
            return AppCommand::None;
        };
        self.selected_row().map_or(AppCommand::None, |row| {
            AppCommand::Shell(PodShellTarget {
                namespace: namespace.clone(),
                pod: pod.clone(),
                container: row.name.to_string(),
            })
        })
    }

    pub fn show_containers(&mut self, target: ResourceTarget, rows: Vec<Arc<ResourceRow>>) {
        if self.pending_containers.as_ref() != Some(&target)
            || self.kind.known_kind() != Some(ResourceKind::Pods)
        {
            return;
        }
        self.pending_containers = None;
        self.show_child_rows(
            ListMode::Containers {
                namespace: target.namespace.unwrap_or_else(|| "default".to_owned()),
                pod: target.name,
            },
            rows,
        );
    }

    pub fn show_policy(
        &mut self,
        target: &ResourceTarget,
        title: String,
        rows: Vec<Arc<ResourceRow>>,
    ) {
        if self.pending_policy.as_ref() != Some(target) || !target.kind.is_authorization_resource()
        {
            return;
        }
        self.pending_policy = None;
        self.show_child_rows(ListMode::Policy { title }, rows);
    }

    fn show_child_rows(&mut self, mode: ListMode, rows: Vec<Arc<ResourceRow>>) {
        let selected_key = self.selected_row().map(|row| Arc::clone(&row.key));
        let revision = self.snapshot.revision.wrapping_add(1);
        let replacement = ResourceSnapshot {
            generation: self.generation,
            kind: self.kind.clone(),
            rows: Arc::new(rows),
            table_columns: Arc::from([]),
            state: SourceState::Ready,
            revision,
            emitted_at: Instant::now(),
        };
        let parent_snapshot = std::mem::replace(&mut self.snapshot, replacement);
        self.saved_resource_list = Some(SavedResourceList {
            snapshot: parent_snapshot,
            query: std::mem::take(&mut self.query),
            sort_field: self.sort_field,
            sort_descending: self.sort_descending,
            column_sort_override: self.column_sort_override.take(),
            selected_key,
        });
        self.list_mode = mode;
        self.sort_field = SortField::Name;
        self.sort_descending = false;
        self.column_sort_override = None;
        self.selected = 0;
        self.offset = 0;
        self.marks.clear();
        self.rebuild_filter(None);
        self.notice = None;
    }

    fn restore_resource_list(&mut self) -> AppCommand {
        let Some(saved) = self.saved_resource_list.take() else {
            self.list_mode = ListMode::Resources;
            return AppCommand::None;
        };
        self.snapshot = saved.snapshot;
        self.query = saved.query;
        self.sort_field = saved.sort_field;
        self.sort_descending = saved.sort_descending;
        self.column_sort_override = saved.column_sort_override;
        self.list_mode = ListMode::Resources;
        self.selected = 0;
        self.offset = 0;
        self.rebuild_filter(saved.selected_key.as_deref());
        AppCommand::None
    }

    fn open_delete_confirmation(&mut self) -> AppCommand {
        if !self.is_resource_view() {
            return self.unsupported("Synthetic rows cannot be deleted");
        }
        if self.read_only {
            return self.unsupported("Delete is disabled in read-only mode");
        }
        let targets = self.selected_or_marked_targets();
        if targets.is_empty() {
            return AppCommand::None;
        }
        self.pending_delete = Some(PendingDelete::Resources(targets));
        self.confirm_armed = false;
        self.overlay = Overlay::ConfirmDelete;
        AppCommand::None
    }

    fn open_context_delete_confirmation(&mut self) -> AppCommand {
        if self.read_only {
            return self.unsupported("Delete is disabled in read-only mode");
        }
        let Some(context) = self.context_options.get(self.overlay_selected) else {
            return AppCommand::None;
        };
        if context.name == self.context {
            return self.unsupported("Switch context before deleting it");
        }
        if !context.kubeconfig_backed {
            return self.unsupported("Only kubeconfig contexts can be deleted");
        }
        self.pending_delete = Some(PendingDelete::Context {
            name: context.name.clone(),
        });
        self.confirm_armed = false;
        self.notice = None;
        self.overlay = Overlay::ConfirmDelete;
        AppCommand::None
    }

    fn open_context_rename_dialog(&mut self, visible: &[usize]) -> AppCommand {
        if self.read_only {
            return self.unsupported("Rename is disabled in read-only mode");
        }
        let Some(index) = visible
            .iter()
            .copied()
            .find(|index| *index == self.overlay_selected)
        else {
            return AppCommand::None;
        };
        let Some(context) = self.context_options.get(index) else {
            return AppCommand::None;
        };
        if !context.kubeconfig_backed {
            return self.unsupported("Only kubeconfig contexts can be renamed");
        }
        self.pending_delete = Some(PendingDelete::RenameContext {
            old_name: context.name.clone(),
            new_name: context.name.clone(),
        });
        self.confirm_armed = false;
        self.overlay_selected = 0;
        self.notice = None;
        self.overlay = Overlay::ConfirmDelete;
        AppCommand::None
    }

    fn open_workload_restart_confirmation(&mut self) -> AppCommand {
        if self.read_only {
            return self.unsupported("Restart is disabled in read-only mode");
        }
        let targets = self.selected_or_marked_targets();
        if targets.is_empty() {
            return AppCommand::None;
        }
        self.pending_delete = Some(PendingDelete::Restart(targets));
        self.confirm_armed = false;
        self.notice = None;
        self.overlay = Overlay::ConfirmDelete;
        AppCommand::None
    }

    fn open_scale_dialog(&mut self) -> AppCommand {
        if self.read_only {
            return self.unsupported("Scale is disabled in read-only mode");
        }
        let targets = self.selected_or_marked_targets();
        if targets.is_empty() {
            return AppCommand::None;
        }
        let replicas = if targets.len() == 1 {
            self.selected_row()
                .and_then(|row| {
                    let (_, desired) = row.ready.split_once('/')?;
                    let desired = desired.trim_end_matches(['↑', '↓']);
                    desired.parse::<u32>().ok().map(|n| n.to_string())
                })
                .unwrap_or_default()
        } else {
            String::new()
        };
        self.pending_delete = Some(PendingDelete::Scale { targets, replicas });
        self.confirm_armed = false;
        self.overlay_selected = 0;
        self.notice = None;
        self.overlay = Overlay::ConfirmDelete;
        AppCommand::None
    }

    fn open_cronjob_trigger_confirmation(&mut self) -> AppCommand {
        if self.read_only {
            return self.unsupported("Trigger is disabled in read-only mode");
        }
        let targets = self.selected_or_marked_targets();
        if targets.is_empty() {
            return AppCommand::None;
        }
        self.open_action_confirmation(PendingDelete::TriggerCronJob(targets))
    }

    fn open_suspend_toggle_confirmation(&mut self) -> AppCommand {
        if self.read_only {
            return self.unsupported("Suspend/resume is disabled in read-only mode");
        }
        let Some(target) = self.selected_target() else {
            return AppCommand::None;
        };
        let suspend = self.selected_row().and_then(|row| {
            let col_idx = self
                .snapshot
                .table_columns
                .iter()
                .position(|c| c.name.eq_ignore_ascii_case("suspend"));
            let text = col_idx
                .and_then(|i| row.table_cells.get(i))
                .map(|c| c.text.as_ref().trim())
                .or_else(|| {
                    row.raw_object
                        .as_deref()
                        .and_then(|v| v.pointer("/spec/suspend"))
                        .and_then(serde_json::Value::as_bool)
                        .map(|b| if b { "true" } else { "false" })
                })?;
            match text.to_ascii_lowercase().as_str() {
                "true" => Some(false),
                "false" => Some(true),
                _ => None,
            }
        });
        self.open_action_confirmation(PendingDelete::ToggleCronJobSuspend { target, suspend })
    }

    fn open_action_confirmation(&mut self, action: PendingDelete) -> AppCommand {
        self.pending_delete = Some(action);
        self.confirm_armed = false;
        self.notice = None;
        self.overlay = Overlay::ConfirmDelete;
        AppCommand::None
    }

    fn open_suspend_confirmation(&mut self, suspended: bool) -> AppCommand {
        if self.read_only {
            return self.unsupported("Suspend/resume is disabled in read-only mode");
        }
        if !self.is_resource_view() || !self.kind.is_suspendable() {
            return self.unsupported("Suspend/resume requires a CronJob or Job view");
        }
        let targets = self.selected_or_marked_targets();
        if targets.is_empty() {
            return AppCommand::None;
        }
        self.open_action_confirmation(PendingDelete::SetSuspended { targets, suspended })
    }

    fn open_node_drain_confirmation(&mut self) -> AppCommand {
        if self.read_only {
            return self.unsupported("Drain is disabled in read-only mode");
        }
        let names = self
            .selected_or_marked_targets()
            .into_iter()
            .map(|target| target.name)
            .collect::<Vec<_>>();
        if names.is_empty() {
            return AppCommand::None;
        }
        self.pending_delete = Some(PendingDelete::DrainNode(DrainDialog::new(names)));
        self.confirm_armed = false;
        self.overlay_selected = DRAIN_GRACE_PERIOD_FIELD;
        self.drain_value_edit_state = DrainValueEditState::Pristine;
        self.notice = None;
        self.overlay = Overlay::ConfirmDelete;
        AppCommand::None
    }

    fn open_node_scheduling_confirmation(&mut self) -> AppCommand {
        if self.read_only {
            return self.unsupported("Cordon/uncordon is disabled in read-only mode");
        }
        let names = self
            .selected_or_marked_targets()
            .into_iter()
            .map(|target| target.name)
            .collect::<Vec<_>>();
        if names.is_empty() {
            return AppCommand::None;
        }
        let currently_unschedulable = self
            .selected_row()
            .is_some_and(|row| row.node_data().is_some_and(|data| data.unschedulable));
        self.pending_delete = Some(PendingDelete::NodeScheduling {
            names,
            unschedulable: !currently_unschedulable,
        });
        self.confirm_armed = false;
        self.notice = None;
        self.overlay = Overlay::ConfirmDelete;
        AppCommand::None
    }

    fn open_node_cordon_confirmation(&mut self, unschedulable: bool) -> AppCommand {
        if self.read_only {
            return self.unsupported("Cordon/uncordon is disabled in read-only mode");
        }
        let names = self
            .selected_or_marked_targets()
            .into_iter()
            .map(|target| target.name)
            .collect::<Vec<_>>();
        if names.is_empty() {
            return AppCommand::None;
        }
        self.pending_delete = Some(PendingDelete::NodeScheduling {
            names,
            unschedulable,
        });
        self.confirm_armed = false;
        self.notice = None;
        self.overlay = Overlay::ConfirmDelete;
        AppCommand::None
    }

    fn immediate_delete(&mut self) -> AppCommand {
        if !self.is_resource_view() {
            return self.unsupported("Synthetic rows cannot be deleted");
        }
        if self.read_only {
            return self.unsupported("Kill is disabled in read-only mode");
        }
        let targets = self.selected_or_marked_targets();
        if targets.is_empty() {
            return AppCommand::None;
        }
        self.marks.clear();
        self.mark_anchor = None;
        AppCommand::Action(ActionIntent::Delete {
            targets,
            immediate: true,
        })
    }

    fn content_action(&mut self, content: ContentKind) -> AppCommand {
        let Some(target) = self.selected_target() else {
            return AppCommand::None;
        };
        self.request_content(target, content)
    }

    fn edit_action(&mut self) -> AppCommand {
        if self.read_only {
            return self.unsupported("Edit is disabled in read-only mode");
        }
        self.selected_target()
            .map_or(AppCommand::None, AppCommand::Edit)
    }

    fn edit_content_action(&mut self) -> AppCommand {
        if self.read_only {
            return self.unsupported("Edit is disabled in read-only mode");
        }
        self.content_target
            .clone()
            .or_else(|| self.selected_target())
            .map_or(AppCommand::None, AppCommand::Edit)
    }

    fn copy_name(&self) -> AppCommand {
        self.selected_row().map_or(AppCommand::None, |row| {
            AppCommand::Copy(row.name.to_string())
        })
    }

    fn copy_namespace(&self) -> AppCommand {
        self.selected_row().map_or(AppCommand::None, |row| {
            AppCommand::Copy(row.namespace.to_string())
        })
    }

    fn warp_to_selected_namespace(&mut self) -> AppCommand {
        let namespace = self.selected_row().map(|row| row.namespace.to_string());
        namespace.map_or(AppCommand::None, |namespace| {
            self.request_view(
                ViewState {
                    instance: None,
                    context: self.context.clone(),
                    kind: self.kind.clone(),
                    namespace: Some(namespace),
                    field_selector: self.field_selector.clone(),
                    label_selector: self.label_selector.clone(),
                },
                true,
            )
        })
    }

    fn unsupported(&mut self, message: &str) -> AppCommand {
        self.notice = Some(message.to_owned());
        AppCommand::None
    }

    pub fn show_content(
        &mut self,
        title: impl Into<String>,
        body: impl Into<String>,
        managed_fields: Option<String>,
        decoded_body: Option<String>,
        decoded_managed_fields: Option<String>,
        content: ContentKind,
    ) {
        self.reset_live_refresh(content);
        self.ui
            .set(UiFlags::FULLSCREEN, self.user_config.settings.ui.fullscreen);
        self.log_view = None;
        self.content_title = title.into();
        self.content_body = body.into();
        self.content_managed_fields = managed_fields;
        self.content_decoded_body = decoded_body;
        self.content_decoded_managed_fields = decoded_managed_fields;
        if self.ui.secret_decoded() {
            self.ui.toggle(UiFlags::SECRET_DECODED);
        }
        self.content_kind = Some(content);
        if self.ui.managed_fields() {
            self.ui.toggle(UiFlags::MANAGED_FIELDS);
        }
        self.reset_body_scroll();
        self.overlay = Overlay::Content;
        self.body_query.clear();
        self.filter_error = None;
        self.notice = None;
    }

    #[must_use]
    pub fn active_content_body(&self) -> &str {
        if self.ui.secret_decoded() {
            if self.ui.managed_fields() {
                self.content_decoded_managed_fields
                    .as_deref()
                    .or(self.content_decoded_body.as_deref())
                    .or(self.content_managed_fields.as_deref())
                    .unwrap_or(&self.content_body)
            } else {
                self.content_decoded_body
                    .as_deref()
                    .unwrap_or(&self.content_body)
            }
        } else if self.ui.managed_fields() {
            self.content_managed_fields
                .as_deref()
                .unwrap_or(&self.content_body)
        } else {
            &self.content_body
        }
    }

    #[must_use]
    pub fn body_filter(&self) -> &str {
        &self.body_query
    }

    #[must_use]
    pub fn body_line_matches(&self, line: &str) -> bool {
        FilterMatcher::new(&self.body_query).matches_text(line)
    }

    #[must_use]
    pub const fn can_decode_content(&self) -> bool {
        self.content_decoded_body.is_some()
    }

    #[must_use]
    pub const fn is_content_decoded(&self) -> bool {
        self.ui.secret_decoded()
    }

    pub fn show_transient_notice(&mut self, notice: impl Into<String>) {
        self.notice = Some(notice.into());
    }

    pub fn show_port_forwards(&mut self, forwards: Vec<PortForwardInfo>) {
        self.active_port_forwards.clone_from(&forwards);
        self.port_forwards = forwards;
        self.overlay_selected = 0;
        self.notice = None;
        self.overlay = Overlay::PortForwards;
    }

    pub fn refresh_port_forwards(&mut self, forwards: Vec<PortForwardInfo>) {
        self.active_port_forwards.clone_from(&forwards);
        if self.overlay != Overlay::PortForwards {
            return;
        }
        let selected_id = self
            .port_forwards
            .get(self.overlay_selected)
            .map(|forward| forward.id);
        self.port_forwards = forwards;
        if self.port_forwards.is_empty() {
            self.overlay_selected = 0;
            return;
        }
        self.overlay_selected = selected_id
            .and_then(|id| {
                self.port_forwards
                    .iter()
                    .position(|forward| forward.id == id)
            })
            .unwrap_or(0)
            .min(self.port_forwards.len().saturating_sub(1));
    }

    pub fn show_notice(&mut self, notice: impl Into<String>) {
        self.notice = Some(notice.into());
        self.overlay = Overlay::None;
        self.pending_delete = None;
        self.confirm_armed = false;
    }

    #[must_use]
    pub fn source_label(&self) -> &str {
        match &self.snapshot.state {
            SourceState::Loading { .. } => "loading",
            SourceState::Ready => "watching",
            SourceState::Error(_) => "degraded",
        }
    }

    #[must_use]
    pub fn filter_navigation_active(&self) -> bool {
        if self.body_filter_active() {
            !self.body_query.is_empty()
        } else {
            !self.query.is_empty()
        }
    }
}

pub(crate) enum FilterMatcher {
    All,
    Regex(regex::Regex),
    Inverse(regex::Regex),
    Fuzzy(String),
    Labels(Vec<String>),
    Invalid(String),
}

impl FilterMatcher {
    pub(crate) fn new(query: &str) -> Self {
        let query = query.trim();
        if query.is_empty() {
            return Self::All;
        }
        if let Some(value) = query.strip_prefix("-f ") {
            return Self::Fuzzy(value.trim().to_lowercase());
        }
        if let Some(value) = query.strip_prefix("-l ") {
            return Self::Labels(
                value
                    .split(',')
                    .map(|item| item.trim().to_lowercase())
                    .filter(|item| !item.is_empty())
                    .collect(),
            );
        }
        let (inverse, expression) = query
            .strip_prefix('!')
            .map_or((false, query), |value| (true, value.trim()));
        match RegexBuilder::new(expression).case_insensitive(true).build() {
            Ok(regex) if inverse => Self::Inverse(regex),
            Ok(regex) => Self::Regex(regex),
            Err(error) => Self::Invalid(error.to_string()),
        }
    }

    fn matches_with_paths(&self, row: &ResourceRow, custom_paths: &[String]) -> bool {
        let search_text = if custom_paths.is_empty() {
            std::borrow::Cow::Borrowed(row.search.as_ref())
        } else {
            let mut combined = row.search.to_string();
            for path in custom_paths {
                let val = row.json_path_value(path);
                if val != "<none>" && !val.is_empty() {
                    combined.push('\n');
                    combined.push_str(&val.to_lowercase());
                }
            }
            std::borrow::Cow::Owned(combined)
        };
        match self {
            Self::All => true,
            Self::Regex(regex) => regex.is_match(&search_text),
            Self::Inverse(regex) => !regex.is_match(&search_text),
            Self::Fuzzy(needle) => fuzzy_match(&search_text, needle),
            Self::Labels(selectors) => {
                let labels = row.labels.to_lowercase();
                selectors
                    .iter()
                    .all(|selector| labels.split(',').any(|label| label == selector))
            }
            Self::Invalid(_) => false,
        }
    }

    pub(crate) fn matches_text(&self, text: &str) -> bool {
        match self {
            Self::All => true,
            Self::Regex(regex) => regex.is_match(text),
            Self::Inverse(regex) => !regex.is_match(text),
            Self::Fuzzy(needle) => fuzzy_match(&text.to_lowercase(), needle),
            Self::Labels(selectors) => {
                let text = text.to_lowercase();
                selectors.iter().all(|selector| text.contains(selector))
            }
            Self::Invalid(_) => false,
        }
    }

    fn error(&self) -> Option<&str> {
        if let Self::Invalid(error) = self {
            Some(error)
        } else {
            None
        }
    }

    pub(crate) fn match_ranges(&self, text: &str) -> Vec<(usize, usize)> {
        match self {
            Self::Regex(regex) => regex
                .find_iter(text)
                .filter(|matched| !matched.is_empty())
                .map(|matched| (matched.start(), matched.end()))
                .collect(),
            Self::Fuzzy(needle) => fuzzy_match_ranges(text, needle),
            Self::Labels(selectors) => {
                let mut ranges = selectors
                    .iter()
                    .flat_map(|selector| literal_match_ranges(text, selector))
                    .collect::<Vec<_>>();
                ranges.sort_unstable();
                ranges.dedup();
                ranges
            }
            Self::All | Self::Inverse(_) | Self::Invalid(_) => Vec::new(),
        }
    }
}

fn literal_match_ranges(text: &str, needle: &str) -> Vec<(usize, usize)> {
    if needle.is_empty() {
        return Vec::new();
    }
    text.to_ascii_lowercase()
        .match_indices(needle)
        .map(|(start, value)| (start, start + value.len()))
        .collect()
}

fn fuzzy_match_ranges(text: &str, needle: &str) -> Vec<(usize, usize)> {
    if needle.is_empty() {
        return Vec::new();
    }
    let mut expected = needle.chars();
    let mut current = expected.next();
    let mut ranges = Vec::new();
    for (start, character) in text.char_indices() {
        if current.is_some_and(|expected| character.eq_ignore_ascii_case(&expected)) {
            ranges.push((start, start + character.len_utf8()));
            current = expected.next();
            if current.is_none() {
                return ranges;
            }
        }
    }
    Vec::new()
}

fn fuzzy_match(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    let mut expected = needle.chars();
    let mut current = expected.next();
    for character in haystack.chars() {
        if current == Some(character) {
            current = expected.next();
            if current.is_none() {
                return true;
            }
        }
    }
    false
}

fn row_is_fault(kind: &ResourceDescriptor, row: &ResourceRow) -> bool {
    if kind.is_workloads() {
        return row.status.as_ref() == "DEGRADED";
    }
    if kind.is_helm() {
        return row.status.as_ref() != "deployed";
    }
    if kind.is_helm_history() {
        return !matches!(row.status.as_ref(), "deployed" | "superseded");
    }
    if kind.is_benchmarks() {
        return row.status.as_ref() != "pass";
    }
    if kind.is_pulses() {
        return row.status.as_ref() == "DEGRADED";
    }
    if kind.is_xray() {
        return !matches!(row.status.as_ref(), "ok" | "completed");
    }
    if kind.is_dir() || kind.is_subject() || kind.is_skins() {
        return false;
    }
    if row.policy_data().is_some() {
        return false;
    }
    if kind.is_api_service() {
        return row.status.as_ref() != "True";
    }
    if kind.group.is_empty() && kind.kind.as_ref() == "Event" {
        return row.status.as_ref() != "Normal";
    }
    if kind.group.is_empty()
        && matches!(
            kind.kind.as_ref(),
            "PersistentVolume" | "PersistentVolumeClaim"
        )
    {
        return !matches!(
            row.status.as_ref(),
            "-" | "Ready" | "Running" | "Active" | "Bound" | "Available"
        );
    }
    if kind.is_job() {
        return row.status.as_ref() == "Failed"
            || (!row.ready.is_empty()
                && row.ready.as_ref() != "-"
                && row
                    .ready
                    .split_once('/')
                    .is_some_and(|(a, b)| a != b || a == "0"));
    }
    match kind.known_kind() {
        Some(ResourceKind::Services) => false,
        Some(ResourceKind::Nodes) => row.status.as_ref() != "Ready",
        Some(ResourceKind::Pods) => {
            !matches!(row.status.as_ref(), "Running" | "Succeeded" | "Completed")
        }
        Some(ResourceKind::Deployments | ResourceKind::StatefulSets | ResourceKind::DaemonSets) => {
            row.status.as_ref() != "Ready"
        }
        None => !matches!(row.status.as_ref(), "-" | "Ready" | "Running" | "Active"),
    }
}

fn resource_gvr_bytes(resource: &ResourceDescriptor) -> impl Iterator<Item = u8> + '_ {
    resource
        .api_version
        .bytes()
        .chain((!resource.api_version.is_empty()).then_some(b'/'))
        .chain(resource.plural.bytes())
}

#[must_use]
pub(crate) fn row_vuln_score(
    row: &ResourceRow,
    settings: &crate::config::ImageScansSettings,
) -> String {
    if !settings.enable || settings.should_exclude(&row.namespace, &row.labels) {
        return "n/a".to_owned();
    }
    let images = row.extract_container_images();
    if images.is_empty() {
        return "n/a".to_owned();
    }
    crate::scans::score_images(images)
}

fn compare_rows_by_override(
    left: &ResourceRow,
    right: &ResourceRow,
    key: &ColumnSortKey,
) -> Ordering {
    let primary = match key {
        ColumnSortKey::Field(field) => return compare_rows(left, right, *field),
        ColumnSortKey::TableIndex(index) => {
            let l_cell = left.table_cells.get(*index);
            let r_cell = right.table_cells.get(*index);
            if let (Some(l_ts), Some(r_ts)) = (
                l_cell.and_then(|c| c.timestamp_unix),
                r_cell.and_then(|c| c.timestamp_unix),
            ) {
                l_ts.cmp(&r_ts)
            } else {
                let l_str = l_cell.map_or("", |c| c.text.as_ref());
                let r_str = r_cell.map_or("", |c| c.text.as_ref());
                navigation::natural_compare(l_str, r_str)
            }
        }
        ColumnSortKey::JsonPath(path) => {
            let l_str = left.json_path_value(path);
            let r_str = right.json_path_value(path);
            navigation::natural_compare(&l_str, &r_str)
        }
        ColumnSortKey::Computed(label) => match *label {
            "CPU/R:L" => {
                let vals = |r: &ResourceRow| {
                    r.pod_data().map_or_else(
                        || {
                            r.container_data()
                                .map_or((0, 0), |d| (d.cpu_request_millis, d.cpu_limit_millis))
                        },
                        |d| (d.cpu_request_millis, d.cpu_limit_millis),
                    )
                };
                vals(left).cmp(&vals(right))
            }
            "MEM/R:L" => {
                let vals = |r: &ResourceRow| {
                    r.pod_data().map_or_else(
                        || {
                            r.container_data()
                                .map_or((0, 0), |d| (d.memory_request_bytes, d.memory_limit_bytes))
                        },
                        |d| (d.memory_request_bytes, d.memory_limit_bytes),
                    )
                };
                vals(left).cmp(&vals(right))
            }
            _ => {
                let l_str = crate::ui::computed_column_value(left, label);
                let r_str = crate::ui::computed_column_value(right, label);
                navigation::natural_compare(&l_str, &r_str)
            }
        },
    };
    primary.then(left.key.cmp(&right.key))
}

fn compare_rows(left: &ResourceRow, right: &ResourceRow, field: SortField) -> Ordering {
    let primary = match field {
        SortField::Kind => match (&left.data, &right.data) {
            (ResourceData::Workload { kind: left }, ResourceData::Workload { kind: right }) => {
                resource_gvr_bytes(left).cmp(resource_gvr_bytes(right))
            }
            _ => left.details.cmp(&right.details),
        },
        SortField::Namespace => left.namespace.cmp(&right.namespace),
        SortField::Name => left.name.cmp(&right.name),
        SortField::Validity => {
            (left.status.as_ref() == "DEGRADED").cmp(&(right.status.as_ref() == "DEGRADED"))
        }
        SortField::VulnScore => Ordering::Equal,
        SortField::Ready | SortField::Binding => {
            if let (Ok(l), Ok(r)) = (left.ready.parse::<i64>(), right.ready.parse::<i64>()) {
                l.cmp(&r)
            } else {
                left.ready.cmp(&right.ready)
            }
        }
        SortField::Status | SortField::ApiGroup => left.status.cmp(&right.status),
        SortField::Node => row_node(left).cmp(row_node(right)),
        SortField::Version => row_version(left).cmp(row_version(right)),
        SortField::Age => left.created_unix.cmp(&right.created_unix),
        SortField::Details => left.details.cmp(&right.details),
        SortField::Get
        | SortField::List
        | SortField::Watch
        | SortField::Create
        | SortField::Patch
        | SortField::Update
        | SortField::Delete
        | SortField::DeleteCollection => {
            policy_allows(left, field).cmp(&policy_allows(right, field))
        }
        SortField::Restarts
        | SortField::Taints
        | SortField::Cpu
        | SortField::CpuRequestPercent
        | SortField::CpuLimitPercent
        | SortField::Memory
        | SortField::MemoryRequestPercent
        | SortField::MemoryLimitPercent
        | SortField::Pods
        | SortField::CpuAllocatable
        | SortField::CpuPercent
        | SortField::MemoryAllocatable
        | SortField::MemoryPercent => row_numeric(left, field).cmp(&row_numeric(right, field)),
    };
    primary.then(left.key.cmp(&right.key))
}

fn policy_allows(row: &ResourceRow, field: SortField) -> bool {
    let verb = match field {
        SortField::Get => "get",
        SortField::List => "list",
        SortField::Watch => "watch",
        SortField::Create => "create",
        SortField::Patch => "patch",
        SortField::Update => "update",
        SortField::Delete => "delete",
        SortField::DeleteCollection => "deletecollection",
        _ => return false,
    };
    row.policy_data().is_some_and(|data| data.allows(verb))
}

fn row_node(row: &ResourceRow) -> &str {
    row.pod_data().map_or("", |data| data.node.as_ref())
}

fn row_version(row: &ResourceRow) -> &str {
    row.node_data().map_or("", |data| data.version.as_ref())
}

fn row_numeric(row: &ResourceRow, field: SortField) -> Option<u64> {
    let pod = row.pod_data();
    let node = row.node_data();
    match field {
        SortField::Restarts => pod
            .map(|data| u64::from(data.restarts))
            .or_else(|| row.container_data().map(|data| u64::from(data.restarts))),
        SortField::Cpu => pod
            .and_then(|data| data.cpu_millis)
            .or_else(|| node.and_then(|data| data.cpu_millis)),
        SortField::CpuRequestPercent => {
            pod.and_then(|data| percent_of(data.cpu_millis, data.cpu_request_millis))
        }
        SortField::CpuLimitPercent => {
            pod.and_then(|data| percent_of(data.cpu_millis, data.cpu_limit_millis))
        }
        SortField::Memory => pod
            .and_then(|data| data.memory_bytes)
            .or_else(|| node.and_then(|data| data.memory_bytes)),
        SortField::MemoryRequestPercent => {
            pod.and_then(|data| percent_of(data.memory_bytes, data.memory_request_bytes))
        }
        SortField::MemoryLimitPercent => {
            pod.and_then(|data| percent_of(data.memory_bytes, data.memory_limit_bytes))
        }
        SortField::Pods => node.map(|data| u64::from(data.pods)),
        SortField::CpuAllocatable => node.map(|data| data.cpu_allocatable_millis),
        SortField::CpuPercent => {
            node.and_then(|data| percent_of(data.cpu_millis, data.cpu_allocatable_millis))
        }
        SortField::MemoryAllocatable => node.map(|data| data.memory_allocatable_bytes),
        SortField::MemoryPercent => {
            node.and_then(|data| percent_of(data.memory_bytes, data.memory_allocatable_bytes))
        }
        SortField::Taints => node.map(|data| u64::from(data.taint_count)),
        _ => None,
    }
}

fn percent_of(value: Option<u64>, total: u64) -> Option<u64> {
    value.filter(|_| total > 0).map(|value| {
        u64::try_from(u128::from(value).saturating_mul(100) / u128::from(total)).unwrap_or(u64::MAX)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        kubeconfig::ContextInfo,
        model::{NamespaceInfo, NodeData, PodData},
    };

    fn context(
        name: &str,
        cluster: &str,
        user: Option<&str>,
        namespace: Option<&str>,
    ) -> ContextInfo {
        ContextInfo {
            name: name.to_owned(),
            cluster: Some(cluster.to_owned()),
            user: user.map(str::to_owned),
            namespace: namespace.map(str::to_owned),
            kubeconfig_backed: true,
        }
    }

    fn descriptor(
        group: &str,
        version: &str,
        api_version: &str,
        kind: &str,
        plural: &str,
        namespaced: bool,
    ) -> ResourceDescriptor {
        ResourceDescriptor::new(
            group,
            version,
            api_version,
            kind,
            plural,
            namespaced,
            crate::model::ResourceOperations::all(),
        )
    }

    fn app_with_descriptor(
        kind: ResourceDescriptor,
        namespace: Option<&str>,
        row: ResourceRow,
    ) -> App {
        let mut app = App::new(
            "demo",
            namespace.map(str::to_owned),
            ResourceKind::Pods,
            false,
            vec!["demo".to_owned()],
        );
        app.kind = kind.clone();
        app.apply_snapshot(ResourceSnapshot {
            generation: app.generation,
            kind,
            rows: Arc::new(vec![Arc::new(row)]),
            table_columns: Arc::from([]),
            state: SourceState::Ready,
            revision: 1,
            emitted_at: Instant::now(),
        });
        app
    }

    fn snapshot(rows: Vec<ResourceRow>, generation: u64, kind: ResourceKind) -> ResourceSnapshot {
        ResourceSnapshot {
            generation,
            kind: kind.into(),
            rows: Arc::new(rows.into_iter().map(Arc::new).collect()),
            table_columns: Arc::from([]),
            state: SourceState::Ready,
            revision: 1,
            emitted_at: Instant::now(),
        }
    }

    fn app_with_rows() -> App {
        let mut app = App::new(
            "demo",
            None,
            ResourceKind::Pods,
            false,
            vec!["demo".to_owned()],
        );
        app.apply_snapshot(snapshot(
            vec![
                ResourceRow::new("prod", "api-0", "Running", "1/1", "0", 0)
                    .with_labels("app=api,env=prod")
                    .with_pod_data(PodData {
                        node: Arc::from("node-a"),
                        default_container: Arc::from("api"),
                        ..PodData::default()
                    }),
                ResourceRow::new("dev", "worker-0", "CrashLoopBackOff", "0/1", "3", 0)
                    .with_labels("app=worker,env=dev")
                    .with_pod_data(PodData {
                        node: Arc::from("node-b"),
                        default_container: Arc::from("worker"),
                        ..PodData::default()
                    }),
            ],
            1,
            ResourceKind::Pods,
        ));
        app
    }

    fn app_with_contexts(read_only: bool) -> App {
        App::new(
            "prod",
            Some("payments".to_owned()),
            ResourceKind::Pods,
            read_only,
            vec![
                context("dev", "kind-dev", Some("developer"), None),
                context("prod", "gke-prod", Some("alice"), Some("payments")),
            ],
        )
    }

    fn control_d() -> KeyEvent {
        KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL)
    }

    #[test]
    fn ctrl_e_toggles_the_header_from_every_view_and_input_mode() {
        for overlay in [
            Overlay::None,
            Overlay::Help,
            Overlay::Aliases,
            Overlay::Namespaces,
            Overlay::Contexts,
            Overlay::Details,
            Overlay::Content,
            Overlay::ConfirmDelete,
        ] {
            let mut app = app_with_rows();
            app.overlay = overlay;

            assert!(app.ui.header(), "{overlay:?} starts with the header");
            assert_eq!(
                app.handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL)),
                AppCommand::None
            );
            assert!(!app.ui.header(), "{overlay:?} toggles the header");
            assert_eq!(app.overlay, overlay, "{overlay:?} stays open");
        }

        let mut app = app_with_rows();
        app.input_mode = InputMode::Command;
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL)),
            AppCommand::None
        );
        assert!(!app.ui.header());
        assert_eq!(app.input_mode, InputMode::Command);
        assert_eq!(app.input.len(), 0);
    }

    #[test]
    fn vim_navigation_is_contextual_in_lists_and_choosers() {
        let mut list = app_with_rows();
        list.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
        assert_eq!(
            list.selected_row().map(|row| row.name.as_ref()),
            Some("worker-0")
        );
        list.handle_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE));
        assert_eq!(
            list.selected_row().map(|row| row.name.as_ref()),
            Some("api-0")
        );
        list.viewport_height = 1;
        list.handle_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL));
        assert_eq!(
            list.selected_row().map(|row| row.name.as_ref()),
            Some("worker-0")
        );
        list.handle_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL));
        assert_eq!(
            list.selected_row().map(|row| row.name.as_ref()),
            Some("api-0")
        );
        for (key, expected) in [
            (
                KeyEvent::new(KeyCode::Char('G'), KeyModifiers::SHIFT),
                "worker-0",
            ),
            (
                KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE),
                "api-0",
            ),
            (KeyEvent::new(KeyCode::End, KeyModifiers::NONE), "worker-0"),
            (KeyEvent::new(KeyCode::Home, KeyModifiers::NONE), "api-0"),
        ] {
            list.handle_key(key);
            assert_eq!(
                list.selected_row().map(|row| row.name.as_ref()),
                Some(expected)
            );
        }
        list.query = "api".to_owned();
        list.rebuild_filter(None);
        list.handle_key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE));
        assert_eq!(list.query.len(), 0);

        let widget = descriptor(
            "example.io",
            "v1",
            "example.io/v1",
            "Widget",
            "widgets",
            true,
        );
        let mut resource = app_with_descriptor(
            widget,
            Some("prod"),
            ResourceRow::new("prod", "selected", "Ready", "1/1", "-", 0),
        );
        let AppCommand::Action(ActionIntent::FindChildResource { target, .. }) =
            resource.handle_key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE))
        else {
            panic!("generic resources should check for owned children");
        };
        resource.show_no_discovered_child(&target);
        assert_eq!(resource.overlay, Overlay::Details);

        let mut chooser = app_with_rows();
        chooser.open_aliases();
        let visible = chooser.visible_resource_indices();
        chooser.viewport_height = 1;
        chooser.handle_key(KeyEvent::new(KeyCode::Char('G'), KeyModifiers::SHIFT));
        assert_eq!(
            chooser.overlay_selected,
            *visible.last().expect("last alias")
        );
        chooser.handle_key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE));
        assert_eq!(chooser.overlay_selected, visible[0]);
        chooser.handle_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL));
        assert_eq!(chooser.overlay_selected, visible[1]);
        chooser.handle_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL));
        assert_eq!(chooser.overlay_selected, visible[0]);
        assert!(matches!(
            chooser.handle_key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE)),
            AppCommand::Request(_)
        ));
        let mut chooser = app_with_rows();
        chooser.open_aliases();
        chooser.handle_key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE));
        assert_eq!(chooser.overlay, Overlay::None);
    }

    #[test]
    fn unmodified_horizontal_arrows_never_activate_or_leave_non_text_views() {
        let mut list = app_with_rows();
        list.query = "api".to_owned();
        list.rebuild_filter(None);
        list.set_list_scroll_extent(200, 100, vec![24, 60]);
        assert_eq!(
            list.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE)),
            AppCommand::None
        );
        assert_eq!(list.overlay, Overlay::None);
        assert_eq!(list.list_horizontal_scroll, 24);
        assert_eq!(
            list.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE)),
            AppCommand::None
        );
        assert_eq!(list.query, "api", "Left does not clear or leave the view");
        assert_eq!(list.list_horizontal_scroll, 0);

        let mut chooser = app_with_rows();
        chooser.open_aliases();
        let selected = chooser.overlay_selected;
        assert_eq!(
            chooser.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE)),
            AppCommand::None
        );
        assert_eq!(chooser.overlay, Overlay::Aliases);
        assert_eq!(chooser.overlay_selected, selected);
        chooser.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        assert_eq!(chooser.overlay, Overlay::Aliases);

        let mut confirmation = app_with_rows();
        confirmation.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL));
        assert_eq!(confirmation.overlay, Overlay::ConfirmDelete);
        confirmation.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        assert!(!confirmation.confirm_armed);
        confirmation.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        assert_eq!(confirmation.overlay, Overlay::ConfirmDelete);
    }

    #[test]
    fn filter_match_navigation_wraps_lists_choosers_and_content() {
        let mut list = app_with_rows();
        list.query = ".".to_owned();
        list.rebuild_filter(None);
        list.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
        assert_eq!(
            list.selected_row().map(|row| row.name.as_ref()),
            Some("worker-0")
        );
        list.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
        assert_eq!(
            list.selected_row().map(|row| row.name.as_ref()),
            Some("api-0")
        );
        list.handle_key(KeyEvent::new(KeyCode::Char('N'), KeyModifiers::SHIFT));
        assert_eq!(
            list.selected_row().map(|row| row.name.as_ref()),
            Some("worker-0")
        );

        list.query.clear();
        list.rebuild_filter(None);
        assert_eq!(
            list.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE)),
            AppCommand::Copy("prod".to_owned()),
            "n keeps its normal binding when no filter is active"
        );

        let mut chooser = app_with_rows();
        chooser.open_aliases();
        chooser.body_query = ".".to_owned();
        let visible = chooser.visible_resource_indices();
        let first = chooser.overlay_selected;
        chooser.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
        assert_ne!(chooser.overlay_selected, first);
        chooser.handle_key(KeyEvent::new(KeyCode::Char('N'), KeyModifiers::SHIFT));
        assert_eq!(chooser.overlay_selected, first);
        assert!(visible.len() > 1);

        let mut content = app_with_rows();
        content.show_content(
            "YAML(prod/api-0)",
            "match: first\nskip: value\nmatch: second",
            None,
            None,
            None,
            ContentKind::Yaml,
        );
        content.body_query = "match".to_owned();
        content.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
        assert_eq!(content.content_scroll, 2);
        content.set_content_scroll_extent(3, 2);
        assert_eq!(content.content_scroll, 1, "viewport clamps the last match");
        content.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
        assert_eq!(content.content_scroll, 0);
        content.handle_key(KeyEvent::new(KeyCode::Char('N'), KeyModifiers::SHIFT));
        assert_eq!(content.content_scroll, 2);
    }

    #[test]
    fn filter_matcher_reports_regex_fuzzy_and_label_highlight_ranges() {
        assert_eq!(
            FilterMatcher::new("api").match_ranges("prod-api-api"),
            [(5, 8), (9, 12)]
        );
        assert_eq!(
            FilterMatcher::new("-f pi").match_ranges("API"),
            [(1, 2), (2, 3)]
        );
        assert_eq!(
            FilterMatcher::new("-l env=prod").match_ranges("app=api,ENV=PROD"),
            [(8, 16)]
        );
        assert_eq!(FilterMatcher::new("!api").match_ranges("worker").len(), 0);
    }

    #[test]
    fn wrap_toggle_preserves_a_scrolled_bottom_anchor() {
        let mut app = app_with_rows();
        app.show_content(
            "YAML(prod/api-0)",
            "key: value",
            None,
            None,
            None,
            ContentKind::Yaml,
        );
        app.set_content_scroll_extent(100, 10);
        app.handle_key(KeyEvent::new(KeyCode::Char('G'), KeyModifiers::SHIFT));
        assert_eq!(app.content_scroll, 90);

        app.handle_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE));
        assert_eq!(app.content_scroll, u16::MAX);
        app.set_content_scroll_extent(140, 10);
        assert_eq!(app.content_scroll, 130);
    }

    #[test]
    fn pod_log_hotkeys_defer_container_selection_to_the_resolver() {
        let mut pods = app_with_rows();
        assert!(matches!(
            pods.handle_key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE)),
            AppCommand::Action(ActionIntent::StartLogs {
                request: LogRequest { target: LogTarget { ref container, previous: false, .. }, .. },
            }) if container.is_none()
        ));
        pods.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(matches!(
            pods.handle_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE)),
            AppCommand::Action(ActionIntent::StartLogs {
                request: LogRequest { target: LogTarget { ref container, previous: true, .. }, .. },
            }) if container.is_none()
        ));
    }

    #[test]
    fn vim_navigation_scrolls_every_text_body() {
        fn exercise(app: &mut App, label: &str) {
            app.set_content_scroll_extent(65, 20);
            app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
            assert_eq!(app.content_scroll, 1, "{label} handles Down");
            app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
            assert_eq!(app.content_scroll, 0, "{label} handles Up");
            app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
            assert_eq!(app.content_scroll, 1, "{label} scrolls down");
            app.handle_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE));
            assert_eq!(app.content_scroll, 0, "{label} scrolls up");
            app.handle_key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE));
            assert_eq!(app.content_scroll, 20, "{label} handles PageDown");
            app.handle_key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE));
            assert_eq!(app.content_scroll, 0, "{label} handles PageUp");
            app.handle_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL));
            assert_eq!(app.content_scroll, 20, "{label} moves one page down");
            app.handle_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL));
            assert_eq!(app.content_scroll, 0, "{label} moves one page up");
            app.handle_key(KeyEvent::new(KeyCode::Char('G'), KeyModifiers::SHIFT));
            assert_eq!(app.content_scroll, 45, "{label} goes to the bottom");
            app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
            assert_eq!(app.content_scroll, 45, "{label} stays at the bottom");
            app.handle_key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE));
            assert_eq!(app.content_scroll, 0, "{label} goes to the top");
            app.handle_key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
            assert_eq!(app.content_scroll, 45, "{label} handles End");
            app.handle_key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE));
            assert_eq!(app.content_scroll, 0, "{label} handles Home");
            app.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
            assert_eq!(app.content_horizontal_scroll, 1, "{label} handles Right");
            app.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
            assert_eq!(app.content_horizontal_scroll, 0, "{label} handles Left");
            app.handle_key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE));
            assert_eq!(app.content_horizontal_scroll, 1, "{label} scrolls right");
            app.handle_key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE));
            assert_eq!(app.content_horizontal_scroll, 0, "{label} scrolls left");
        }

        let mut content = app_with_rows();
        content.show_content(
            "YAML(prod/api-0)",
            "kind: Pod",
            None,
            None,
            None,
            ContentKind::Yaml,
        );
        exercise(&mut content, "content");

        let mut help = app_with_rows();
        help.open_overlay(Overlay::Help);
        exercise(&mut help, "help");

        let mut details = app_with_rows();
        details.details_row = details.selected_row().cloned();
        details.overlay = Overlay::Details;
        exercise(&mut details, "details");
    }

    #[test]
    fn history_navigation_is_available_from_every_body_view() {
        for overlay in [
            Overlay::Aliases,
            Overlay::Namespaces,
            Overlay::Contexts,
            Overlay::Help,
            Overlay::Content,
            Overlay::Details,
        ] {
            let mut app = app_with_rows();
            app.request_view(
                ViewState {
                    instance: None,
                    context: "demo".to_owned(),
                    kind: ResourceKind::Services.into(),
                    namespace: None,
                    field_selector: None,
                    label_selector: None,
                },
                true,
            );
            app.overlay = overlay;

            assert!(matches!(
                app.handle_key(KeyEvent::new(KeyCode::Char('['), KeyModifiers::NONE)),
                AppCommand::Request(ViewState { kind, .. })
                    if kind.known_kind() == Some(ResourceKind::Pods)
            ));
            assert_eq!(app.overlay, Overlay::None);

            app.overlay = overlay;
            assert!(matches!(
                app.handle_key(KeyEvent::new(KeyCode::Char(']'), KeyModifiers::NONE)),
                AppCommand::Request(ViewState { kind, .. })
                    if kind.known_kind() == Some(ResourceKind::Services)
            ));
            assert_eq!(app.overlay, Overlay::None);
        }
    }

    #[test]
    fn vim_navigation_selects_confirmation_actions() {
        let mut app = app_with_rows();
        app.open_delete_confirmation();
        assert!(!app.confirm_armed);

        app.handle_key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE));
        assert!(app.confirm_armed);
        app.handle_key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE));
        assert!(!app.confirm_armed);
    }

    #[test]
    fn context_filter_matches_every_displayed_metadata_field() {
        let mut app = App::new(
            "prod",
            None,
            ResourceKind::Pods,
            false,
            vec![
                context("prod", "gke-prod", Some("alice"), Some("payments")),
                context("dev", "kind-dev", Some("bob"), None),
            ],
        );
        app.overlay = Overlay::Contexts;
        for query in ["gke-prod", "alice", "payments"] {
            app.body_query = query.to_owned();
            assert_eq!(app.visible_context_indices(), vec![1]);
        }
        app.body_query = "default".to_owned();
        assert_eq!(app.visible_context_indices(), vec![0]);
    }

    #[test]
    fn namespace_results_are_typed_and_scoped_to_the_current_context() {
        let mut app = app_with_rows();
        let prod = vec![NamespaceInfo::discovered("payments", "Active", Some(100))];
        app.set_namespace_options("other", prod.clone());
        assert_eq!(app.namespace_options.len(), 0);
        app.set_namespace_options("demo", prod);
        assert_eq!(app.namespace_options[0], NamespaceInfo::all());
        assert_eq!(app.namespace_options[1].name, "payments");
        assert_eq!(app.namespace_options[1].status, "Active");
    }

    fn type_input(app: &mut App, prefix: char, value: &str) -> AppCommand {
        if prefix == '/' && !app.query.is_empty() {
            app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        }
        app.handle_key(KeyEvent::new(KeyCode::Char(prefix), KeyModifiers::NONE));
        for character in value.chars() {
            app.handle_key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE));
        }
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
    }

    #[test]
    fn supports_regex_inverse_fuzzy_and_label_filters() {
        let mut app = app_with_rows();
        type_input(&mut app, '/', "api|worker");
        assert_eq!(app.filtered.len(), 2);
        type_input(&mut app, '/', "!api");
        assert_eq!(app.filtered.len(), 1);
        assert_eq!(
            app.selected_row().map(|row| row.name.as_ref()),
            Some("worker-0")
        );
        type_input(&mut app, '/', "-f wrkr");
        assert_eq!(app.filtered.len(), 1);
        type_input(&mut app, '/', "-l env=prod");
        assert_eq!(
            app.selected_row().map(|row| row.name.as_ref()),
            Some("api-0")
        );
    }

    #[test]
    fn filter_reopens_empty_with_history_hint_and_tab_restore() {
        let mut app = app_with_rows();
        type_input(&mut app, '/', "api");
        assert_eq!(app.query, "api");

        app.handle_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE));
        assert_eq!(app.input.len(), 0);
        assert_eq!(app.query, "api");
        assert_eq!(app.previous_filter_hint(), Some("api"));

        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(app.input, "api");
        assert_eq!(app.query, "api");

        app.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert_eq!(app.input.len(), 0);
        assert_eq!(app.query.len(), 0);
        assert_eq!(app.previous_filter_hint(), Some("api"));
    }

    #[test]
    fn resource_command_accepts_alias_and_namespace() {
        let mut app = app_with_rows();
        let command = type_input(&mut app, ':', "deploy kube-system");
        assert_eq!(
            command,
            AppCommand::Request(ViewState {
                instance: None,
                context: "demo".to_owned(),
                kind: ResourceKind::Deployments.into(),
                namespace: Some("kube-system".to_owned()),
                field_selector: None,
                label_selector: None,
            })
        );

        let command = type_input(
            &mut app,
            ':',
            "po -n staging -l app=api,tier=web -f worker @prod-ctx",
        );
        assert_eq!(
            command,
            AppCommand::Request(ViewState {
                instance: None,
                context: "prod-ctx".to_owned(),
                kind: ResourceKind::Pods.into(),
                namespace: Some("staging".to_owned()),
                field_selector: None,
                label_selector: Some("app=api,tier=web".to_owned()),
            })
        );
        assert_eq!(app.query, "worker");
    }

    #[test]
    fn resource_command_clears_stale_filter_and_accepts_an_explicit_replacement() {
        let mut app = app_with_rows();
        type_input(&mut app, '/', "api");

        type_input(&mut app, ':', "ns prod");
        assert_eq!(app.query, "api", "namespace changes retain the filter");

        assert!(matches!(
            type_input(&mut app, ':', "svc"),
            AppCommand::Request(ViewState { kind, .. })
                if kind.known_kind() == Some(ResourceKind::Services)
        ));
        assert_eq!(app.query.len(), 0);

        assert!(matches!(
            type_input(&mut app, ':', "pods /worker"),
            AppCommand::Request(ViewState { kind, .. })
                if kind.known_kind() == Some(ResourceKind::Pods)
        ));
        assert_eq!(app.query, "worker");
    }

    #[test]
    fn resource_chooser_clears_the_previous_resource_filter() {
        let mut app = app_with_rows();
        type_input(&mut app, '/', "api");
        app.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL));
        app.overlay_selected = app
            .resource_options
            .iter()
            .position(|resource| resource.known_kind() == Some(ResourceKind::Services))
            .expect("services resource option");

        assert!(matches!(
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            AppCommand::Request(ViewState { kind, .. })
                if kind.known_kind() == Some(ResourceKind::Services)
        ));
        assert_eq!(app.query.len(), 0);
    }

    #[test]
    fn deployment_enter_opens_pods_with_its_label_selector() {
        let deployment = ResourceDescriptor::from(ResourceKind::Deployments);
        let row = ResourceRow::new("prod", "api", "Ready", "3/3", "3 available", 0)
            .with_child_resource_data(crate::model::ChildResourceData {
                kind: ResourceKind::Pods.into(),
                label_selector: Arc::from("app=api,tier=backend"),
            });
        let mut app = app_with_descriptor(deployment, Some("prod"), row);
        app.query = "stale".to_owned();

        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            AppCommand::Request(ViewState {
                instance: None,
                context: "demo".to_owned(),
                kind: ResourceKind::Pods.into(),
                namespace: Some("prod".to_owned()),
                field_selector: None,
                label_selector: Some("app=api,tier=backend".to_owned()),
            })
        );
        assert_eq!(app.query.len(), 0);
        assert_eq!(app.label_selector.as_deref(), Some("app=api,tier=backend"));
        assert_eq!(app.overlay, Overlay::None);
    }

    #[test]
    fn enter_on_a_crd_opens_all_instances_of_its_discovered_resource() {
        let crd = descriptor(
            "apiextensions.k8s.io",
            "v1",
            "apiextensions.k8s.io/v1",
            "CustomResourceDefinition",
            "customresourcedefinitions",
            false,
        );
        let widget = descriptor(
            "example.io",
            "v1",
            "example.io/v1",
            "Widget",
            "widgets",
            true,
        );
        let row = ResourceRow::new("-", "widgets.example.io", "Established", "-", "-", 0);
        let mut app = app_with_descriptor(crd, None, row);
        app.resource_options.push(widget.clone());

        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            AppCommand::Request(ViewState {
                instance: None,
                context: "demo".to_owned(),
                kind: widget,
                namespace: None,
                field_selector: None,
                label_selector: None,
            })
        );
    }

    #[test]
    fn discovered_children_open_with_an_exact_owner_uid_filter() {
        let widget = descriptor(
            "example.io",
            "v1",
            "example.io/v1",
            "Widget",
            "widgets",
            true,
        );
        let gadget = descriptor(
            "example.io",
            "v1",
            "example.io/v1",
            "Gadget",
            "gadgets",
            true,
        );
        let row = ResourceRow::new("prod", "selected", "Ready", "1/1", "-", 0);
        let mut app = app_with_descriptor(widget.clone(), Some("prod"), row);
        let AppCommand::Action(ActionIntent::FindChildResource { target, .. }) =
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        else {
            panic!("generic resources should discover owned children");
        };

        assert_eq!(
            app.show_discovered_child(&target, gadget.clone(), "parent-uid"),
            AppCommand::Request(ViewState {
                instance: None,
                context: "demo".to_owned(),
                kind: gadget,
                namespace: Some("prod".to_owned()),
                field_selector: Some("kux.ownerUID=parent-uid".to_owned()),
                label_selector: None,
            })
        );
    }

    #[test]
    fn shift_j_opens_the_controller_owner_by_name() {
        let replica_set = descriptor("apps", "v1", "apps/v1", "ReplicaSet", "replicasets", true);
        let row = ResourceRow::new("prod", "api-7d9-x2", "Running", "1/1", "0", 0)
            .with_owner_reference_data(OwnerReferenceData {
                api_version: Arc::from("apps/v1"),
                kind: Arc::from("ReplicaSet"),
                name: Arc::from("api-7d9"),
                uid: Arc::from("replicaset-api-7d9"),
            });
        let mut app = app_with_descriptor(ResourceKind::Pods.into(), Some("prod"), row);
        app.resource_options.push(replica_set.clone());

        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Char('J'), KeyModifiers::SHIFT)),
            AppCommand::Request(ViewState {
                instance: None,
                context: "demo".to_owned(),
                kind: replica_set,
                namespace: Some("prod".to_owned()),
                field_selector: Some("metadata.name=api-7d9".to_owned()),
                label_selector: None,
            })
        );
        assert_eq!(app.field_selector.as_deref(), Some("metadata.name=api-7d9"));
    }

    #[test]
    fn shift_j_reports_rows_without_an_owner() {
        let mut app = app_with_rows();
        let AppCommand::Action(ActionIntent::ResolveOwner { target }) =
            app.handle_key(KeyEvent::new(KeyCode::Char('J'), KeyModifiers::SHIFT))
        else {
            panic!("owner lookup should inspect rows without hydrated metadata");
        };
        assert_eq!(app.show_resolved_owner(&target, None), AppCommand::None);
        assert_eq!(
            app.notice.as_deref(),
            Some("Selected resource has no owner")
        );
    }

    #[test]
    fn shift_j_resolves_a_cluster_scoped_node_owner_on_demand() {
        let node_claim = descriptor(
            "karpenter.sh",
            "v1",
            "karpenter.sh/v1",
            "NodeClaim",
            "nodeclaims",
            false,
        );
        let row = ResourceRow::new("-", "worker-0", "Ready", "worker", "v1.34.0", 0)
            .with_node_data(NodeData::default());
        let mut app = app_with_descriptor(ResourceKind::Nodes.into(), None, row);
        app.resource_options.push(node_claim.clone());

        let AppCommand::Action(ActionIntent::ResolveOwner { target }) =
            app.handle_key(KeyEvent::new(KeyCode::Char('J'), KeyModifiers::SHIFT))
        else {
            panic!("Node owner should be resolved from the API");
        };
        assert_eq!(target.kind.known_kind(), Some(ResourceKind::Nodes));
        assert_eq!(target.namespace, None);
        assert_eq!(
            app.show_resolved_owner(
                &target,
                Some(OwnerReferenceData {
                    api_version: Arc::from("karpenter.sh/v1"),
                    kind: Arc::from("NodeClaim"),
                    name: Arc::from("worker-0-claim"),
                    uid: Arc::from("nodeclaim-worker-0"),
                }),
            ),
            AppCommand::Request(ViewState {
                instance: None,
                context: "demo".to_owned(),
                kind: node_claim,
                namespace: None,
                field_selector: Some("metadata.name=worker-0-claim".to_owned()),
                label_selector: None,
            })
        );
    }

    #[test]
    fn quit_command_matches_k9s() {
        let mut app = app_with_rows();
        assert_eq!(type_input(&mut app, ':', "q"), AppCommand::Quit);
    }

    #[test]
    fn context_command_switches_and_participates_in_history() {
        let mut app = app_with_rows();
        let command = type_input(&mut app, ':', "ctx staging");
        assert_eq!(
            command,
            AppCommand::Request(ViewState {
                instance: None,
                context: "staging".to_owned(),
                kind: ResourceKind::Pods.into(),
                namespace: None,
                field_selector: None,
                label_selector: None,
            })
        );
        assert_eq!(app.context, "staging");
        assert!(
            app.context_options
                .iter()
                .any(|context| context.name == "staging")
        );

        let back = app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(
            back,
            AppCommand::Request(ViewState {
                instance: None,
                context: "demo".to_owned(),
                kind: ResourceKind::Pods.into(),
                namespace: None,
                field_selector: None,
                label_selector: None,
            })
        );
    }

    #[test]
    fn escape_navigates_to_previous_view() {
        let mut app = app_with_rows();
        type_input(&mut app, ':', "svc");
        let command = app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(
            command,
            AppCommand::Request(ViewState {
                instance: None,
                context: "demo".to_owned(),
                kind: ResourceKind::Pods.into(),
                namespace: None,
                field_selector: None,
                label_selector: None,
            })
        );
    }

    #[test]
    fn delete_requires_tab_then_enter() {
        let mut app = app_with_rows();
        app.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL));
        assert_eq!(app.overlay, Overlay::ConfirmDelete);
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            AppCommand::None
        );
        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert!(matches!(
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            AppCommand::Action(ActionIntent::Delete {
                immediate: false,
                ..
            })
        ));
    }

    #[test]
    fn restartable_workloads_require_confirmation() {
        for kind in [
            ResourceKind::Deployments,
            ResourceKind::StatefulSets,
            ResourceKind::DaemonSets,
        ] {
            let descriptor = ResourceDescriptor::from(kind);
            let mut app = app_with_descriptor(
                descriptor.clone(),
                Some("prod"),
                ResourceRow::new("prod", "api", "Ready", "1/1", "-", 0),
            );
            let target = ResourceTarget {
                kind: descriptor,
                namespace: Some("prod".to_owned()),
                name: "api".to_owned(),
            };

            assert_eq!(
                app.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE)),
                AppCommand::None
            );
            assert_eq!(
                app.pending_delete,
                Some(PendingDelete::Restart(vec![target.clone()]))
            );
            assert_eq!(
                app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
                AppCommand::None
            );
            app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
            assert_eq!(
                app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
                AppCommand::Action(ActionIntent::Restart {
                    targets: vec![target],
                })
            );
        }
    }

    #[test]
    fn workload_restart_targets_every_marked_resource() {
        let mut app = App::new(
            "demo",
            Some("prod".to_owned()),
            ResourceKind::Deployments,
            false,
            vec!["demo".to_owned()],
        );
        app.apply_snapshot(snapshot(
            vec![
                ResourceRow::new("prod", "api", "Ready", "3/3", "-", 0),
                ResourceRow::new("prod", "worker", "Ready", "2/2", "-", 0),
            ],
            1,
            ResourceKind::Deployments,
        ));
        app.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE));

        app.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE));
        let Some(PendingDelete::Restart(targets)) = app.pending_delete.as_ref() else {
            panic!("marked workloads should be pending restart");
        };
        assert_eq!(
            targets
                .iter()
                .map(|target| target.name.as_str())
                .collect::<Vec<_>>(),
            ["api", "worker"]
        );
    }

    #[test]
    fn read_only_mode_blocks_workload_restart() {
        let mut app = app_with_descriptor(
            ResourceKind::Deployments.into(),
            Some("prod"),
            ResourceRow::new("prod", "api", "Ready", "3/3", "-", 0),
        );
        app.read_only = true;

        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE)),
            AppCommand::None
        );
        assert_eq!(app.overlay, Overlay::None);
        assert_eq!(
            app.notice.as_deref(),
            Some("Restart is disabled in read-only mode")
        );
    }

    #[test]
    fn node_scheduling_toggle_confirms_cordon_and_uncordon() {
        for (currently_unschedulable, requested_unschedulable) in [(false, true), (true, false)] {
            let status = if currently_unschedulable {
                "Ready,Unschedulable"
            } else {
                "Ready"
            };
            let row = ResourceRow::new("-", "worker-0", status, "worker", "v1.32.0", 0)
                .with_node_data(crate::model::NodeData {
                    unschedulable: currently_unschedulable,
                    ..crate::model::NodeData::default()
                });
            let mut app = app_with_descriptor(ResourceKind::Nodes.into(), None, row);

            assert_eq!(
                app.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::NONE)),
                AppCommand::None
            );
            assert_eq!(app.overlay, Overlay::ConfirmDelete);
            assert_eq!(
                app.pending_delete,
                Some(PendingDelete::NodeScheduling {
                    names: vec!["worker-0".to_owned()],
                    unschedulable: requested_unschedulable,
                })
            );
            app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
            assert_eq!(
                app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
                AppCommand::Action(ActionIntent::SetNodeUnschedulable {
                    names: vec!["worker-0".to_owned()],
                    unschedulable: requested_unschedulable,
                })
            );
            assert_eq!(app.overlay, Overlay::None);
        }
    }

    #[test]
    fn read_only_mode_blocks_node_scheduling_toggle() {
        let row = ResourceRow::new("-", "worker-0", "Ready", "worker", "v1.32.0", 0)
            .with_node_data(crate::model::NodeData::default());
        let mut app = app_with_descriptor(ResourceKind::Nodes.into(), None, row);
        app.read_only = true;

        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::NONE)),
            AppCommand::None
        );
        assert_eq!(app.overlay, Overlay::None);
        assert_eq!(
            app.notice.as_deref(),
            Some("Cordon/uncordon is disabled in read-only mode")
        );
    }

    #[test]
    fn node_drain_requires_confirmation_and_targets_the_selected_node() {
        let row = ResourceRow::new("-", "worker-0", "Ready", "worker", "v1.32.0", 0);
        let mut app = app_with_descriptor(ResourceKind::Nodes.into(), None, row);

        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE)),
            AppCommand::None
        );
        assert_eq!(app.overlay, Overlay::ConfirmDelete);
        assert_eq!(
            app.pending_delete,
            Some(PendingDelete::DrainNode(DrainDialog::new(vec![
                "worker-0".to_owned()
            ])))
        );
        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE));
        for _ in 0..5 {
            app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        }
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            AppCommand::Action(ActionIntent::DrainNode {
                names: vec!["worker-0".to_owned()],
                options: NodeDrainOptions {
                    grace_period_seconds: None,
                    timeout: Duration::from_secs(5),
                    flags: NodeDrainFlags::IGNORE_DAEMON_SETS,
                }
            })
        );
    }

    #[test]
    fn drain_dialog_edits_values_toggles_options_and_validates_timeout() {
        let row = ResourceRow::new("-", "worker-0", "Ready", "worker", "v1.32.0", 0);
        let mut app = app_with_descriptor(ResourceKind::Nodes.into(), None, row);
        app.open_node_drain_confirmation();

        app.handle_key(KeyEvent::new(KeyCode::Char('0'), KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        for character in "30s".chars() {
            app.handle_key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE));
        }
        for _ in 0..4 {
            app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
            app.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE));
        }
        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));

        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            AppCommand::Action(ActionIntent::DrainNode {
                names: vec!["worker-0".to_owned()],
                options: NodeDrainOptions {
                    grace_period_seconds: Some(0),
                    timeout: Duration::from_secs(30),
                    flags: NodeDrainFlags::ALL,
                }
            })
        );

        assert_eq!(
            parse_drain_duration("250ms"),
            Ok(Duration::from_millis(250))
        );
        assert_eq!(parse_drain_duration("2m"), Ok(Duration::from_secs(120)));
        assert_eq!(
            parse_drain_duration("0s"),
            Err("Timeout must be greater than zero".to_owned())
        );
    }

    #[test]
    fn read_only_mode_blocks_node_drain() {
        let row = ResourceRow::new("-", "worker-0", "Ready", "worker", "v1.32.0", 0);
        let mut app = app_with_descriptor(ResourceKind::Nodes.into(), None, row);
        app.read_only = true;

        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE)),
            AppCommand::None
        );
        assert_eq!(app.overlay, Overlay::None);
        assert_eq!(
            app.notice.as_deref(),
            Some("Drain is disabled in read-only mode")
        );
    }

    #[test]
    fn active_and_synthetic_contexts_cannot_be_deleted() {
        let mut app = app_with_contexts(false);
        app.overlay = Overlay::Contexts;
        app.overlay_selected = app
            .context_options
            .iter()
            .position(|item| item.name == app.context)
            .expect("active context");
        assert_eq!(app.handle_key(control_d()), AppCommand::None);
        assert_eq!(app.overlay, Overlay::Contexts);
        assert_eq!(
            app.notice.as_deref(),
            Some("Switch context before deleting it")
        );

        app.context_options
            .push(ContextInfo::synthetic("demo-only"));
        app.overlay_selected = app.context_options.len() - 1;
        assert_eq!(app.handle_key(control_d()), AppCommand::None);
        assert_eq!(
            app.notice.as_deref(),
            Some("Only kubeconfig contexts can be deleted")
        );
    }

    #[test]
    fn inactive_context_delete_requires_tab_then_enter_and_returns_to_contexts() {
        let mut app = app_with_contexts(false);
        app.overlay = Overlay::Contexts;
        app.overlay_selected = app
            .context_options
            .iter()
            .position(|item| item.name == "dev")
            .expect("dev context");
        assert_eq!(app.handle_key(control_d()), AppCommand::None);
        assert_eq!(app.overlay, Overlay::ConfirmDelete);
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            AppCommand::None
        );
        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            AppCommand::Action(ActionIntent::DeleteContext {
                name: "dev".to_owned()
            })
        );
        assert_eq!(app.overlay, Overlay::Contexts);
    }

    #[test]
    fn context_chooser_c_copies_the_selected_context_name() {
        let mut app = app_with_contexts(false);
        app.overlay = Overlay::Contexts;
        app.overlay_selected = app
            .context_options
            .iter()
            .position(|item| item.name == "dev")
            .expect("dev context");

        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE)),
            AppCommand::Copy("dev".to_owned())
        );
        assert_eq!(app.overlay, Overlay::Contexts);
    }

    #[test]
    fn read_only_mode_blocks_context_and_resource_delete() {
        let mut app = app_with_contexts(true);
        app.overlay = Overlay::Contexts;
        app.overlay_selected = app
            .context_options
            .iter()
            .position(|item| item.name == "dev")
            .expect("dev context");
        assert_eq!(app.handle_key(control_d()), AppCommand::None);
        assert_eq!(
            app.notice.as_deref(),
            Some("Delete is disabled in read-only mode")
        );

        app.overlay = Overlay::None;
        app.apply_snapshot(snapshot(
            vec![ResourceRow::new(
                "default", "api-0", "Running", "1/1", "0", 0,
            )],
            app.generation,
            ResourceKind::Pods,
        ));
        assert_eq!(app.handle_key(control_d()), AppCommand::None);
        assert_eq!(
            app.notice.as_deref(),
            Some("Delete is disabled in read-only mode")
        );
    }

    #[test]
    fn read_only_mode_blocks_resource_edit() {
        let widget = descriptor(
            "example.io",
            "v1",
            "example.io/v1",
            "Widget",
            "widgets",
            true,
        );
        let mut app = app_with_descriptor(
            widget,
            Some("prod"),
            ResourceRow::new("prod", "selected", "Ready", "1/1", "-", 0),
        );
        app.read_only = true;

        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE)),
            AppCommand::None
        );
        assert_eq!(
            app.notice.as_deref(),
            Some("Edit is disabled in read-only mode")
        );
    }

    #[test]
    fn read_only_edit_keeps_the_details_body_open() {
        let mut app = app_with_rows();
        app.read_only = true;
        app.details_row = app.selected_row().cloned();
        app.overlay = Overlay::Details;

        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE)),
            AppCommand::None
        );
        assert_eq!(app.overlay, Overlay::Details);
        assert_eq!(
            app.notice.as_deref(),
            Some("Edit is disabled in read-only mode")
        );
    }

    #[test]
    fn context_delete_result_preserves_active_context_and_nearest_selection() {
        let mut app = app_with_contexts(false);
        app.overlay = Overlay::Contexts;
        app.overlay_selected = app
            .context_options
            .iter()
            .position(|item| item.name == "dev")
            .expect("dev context");
        let prod = app
            .context_options
            .iter()
            .find(|item| item.name == "prod")
            .cloned()
            .expect("prod context");

        app.apply_context_options("prod", vec![prod], "Deleted context dev");

        assert_eq!(app.context, "prod");
        assert_eq!(app.context_options.len(), 1);
        assert_eq!(app.context_options[0].name, "prod");
        assert_eq!(app.overlay_selected, 0);
        assert_eq!(app.overlay, Overlay::Contexts);
        assert_eq!(app.notice.as_deref(), Some("Deleted context dev"));
    }

    #[test]
    fn ctrl_d_targets_namespaced_and_cluster_scoped_dynamic_resources() {
        let cases = vec![
            (
                descriptor(
                    "example.io",
                    "v1",
                    "example.io/v1",
                    "Widget",
                    "widgets",
                    true,
                ),
                "prod",
                Some("prod".to_owned()),
            ),
            (
                descriptor(
                    "infra.io",
                    "v1",
                    "infra.io/v1",
                    "Machine",
                    "machines",
                    false,
                ),
                "-",
                None,
            ),
        ];

        for (kind, row_namespace, expected_namespace) in cases {
            let mut app = app_with_descriptor(
                kind.clone(),
                expected_namespace.as_deref(),
                ResourceRow::new(row_namespace, "selected", "Ready", "1/1", "-", 0),
            );
            app.handle_key(control_d());
            app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
            let command = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

            assert_eq!(
                command,
                AppCommand::Action(ActionIntent::Delete {
                    targets: vec![ResourceTarget {
                        kind,
                        namespace: expected_namespace,
                        name: "selected".to_owned(),
                    }],
                    immediate: false,
                })
            );
        }
    }

    #[test]
    fn ctrl_d_deletes_every_marked_resource_after_one_confirmation() {
        let mut app = app_with_rows();
        app.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE));

        app.handle_key(control_d());
        let Some(PendingDelete::Resources(targets)) = app.pending_delete.as_ref() else {
            panic!("marked resources should be pending deletion");
        };
        assert_eq!(
            targets
                .iter()
                .map(|target| target.name.as_str())
                .collect::<Vec<_>>(),
            ["api-0", "worker-0"]
        );

        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        let command = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        let AppCommand::Action(ActionIntent::Delete {
            targets,
            immediate: false,
        }) = command
        else {
            panic!("confirmed batch delete should produce one delete action");
        };
        assert_eq!(targets.len(), 2);
        assert_eq!(app.marks.len(), 0);
        assert_eq!(app.overlay, Overlay::None);
    }

    #[test]
    fn y_targets_namespaced_and_cluster_scoped_dynamic_resources() {
        let cases = vec![
            (
                descriptor(
                    "example.io",
                    "v1",
                    "example.io/v1",
                    "Widget",
                    "widgets",
                    true,
                ),
                "prod",
                Some("prod".to_owned()),
            ),
            (
                descriptor("", "v1", "v1", "Namespace", "namespaces", false),
                "-",
                None,
            ),
        ];

        for (kind, row_namespace, expected_namespace) in cases {
            let mut app = app_with_descriptor(
                kind.clone(),
                expected_namespace.as_deref(),
                ResourceRow::new(row_namespace, "selected", "Ready", "1/1", "-", 0),
            );

            assert_eq!(
                app.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE)),
                AppCommand::Action(ActionIntent::Content {
                    id: app.yaml_refresh.id,
                    target: ResourceTarget {
                        kind,
                        namespace: expected_namespace,
                        name: "selected".to_owned(),
                    },
                    content: ContentKind::Yaml,
                })
            );
        }
    }

    #[test]
    fn e_targets_namespaced_and_cluster_scoped_dynamic_resources() {
        let cases = vec![
            (
                descriptor(
                    "example.io",
                    "v1",
                    "example.io/v1",
                    "Widget",
                    "widgets",
                    true,
                ),
                "prod",
                Some("prod".to_owned()),
            ),
            (
                descriptor("", "v1", "v1", "Namespace", "namespaces", false),
                "-",
                None,
            ),
        ];

        for (kind, row_namespace, expected_namespace) in cases {
            let mut app = app_with_descriptor(
                kind.clone(),
                expected_namespace.as_deref(),
                ResourceRow::new(row_namespace, "selected", "Ready", "1/1", "-", 0),
            );

            assert_eq!(
                app.handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE)),
                AppCommand::Edit(ResourceTarget {
                    kind,
                    namespace: expected_namespace,
                    name: "selected".to_owned(),
                })
            );
        }
    }

    #[test]
    fn marks_and_fault_toggle_are_local_index_operations() {
        let mut app = app_with_rows();
        app.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE));
        assert_eq!(app.marks.len(), 1);
        app.handle_key(KeyEvent::new(KeyCode::Char('z'), KeyModifiers::CONTROL));
        assert_eq!(app.filtered.len(), 1);
        assert_eq!(
            app.selected_row().map(|row| row.name.as_ref()),
            Some("worker-0")
        );
    }

    #[test]
    fn pod_fault_filter_excludes_completed_but_retains_unready_and_failed_pods() {
        let mut app = app_with_rows();
        app.apply_snapshot(snapshot(
            [
                ("completed", "Completed"),
                ("succeeded", "Succeeded"),
                ("running", "Running"),
                ("unready", "NoReady"),
                ("crashing", "CrashLoopBackOff"),
            ]
            .into_iter()
            .map(|(name, status)| ResourceRow::new("prod", name, status, "0/1", "-", 0))
            .collect(),
            app.generation,
            ResourceKind::Pods,
        ));
        app.handle_key(KeyEvent::new(KeyCode::Char('z'), KeyModifiers::CONTROL));
        let names = app
            .filtered
            .iter()
            .map(|index| app.snapshot.rows[*index].name.as_ref())
            .collect::<Vec<_>>();
        assert_eq!(names, ["crashing", "unready"]);
        app.handle_key(KeyEvent::new(KeyCode::Char('z'), KeyModifiers::CONTROL));
        assert_eq!(app.filtered.len(), 5);
    }

    #[test]
    fn clicking_a_column_header_sorts_and_reverses_direction() {
        let mut app = app_with_rows();
        app.set_column_hitboxes(vec![
            ColumnHitbox {
                x: 40,
                y: 7,
                width: 10,
                key: ColumnSortKey::Field(SortField::Age),
            },
            ColumnHitbox {
                x: 55,
                y: 7,
                width: 10,
                key: ColumnSortKey::Computed("IP"),
            },
        ]);
        let click = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 44,
            row: 7,
            modifiers: KeyModifiers::NONE,
        };

        assert!(app.handle_mouse(click));
        assert_eq!(app.sort_field, SortField::Age);
        assert_eq!(app.column_sort_override, None);
        assert!(!app.sort_descending);
        assert_eq!(
            SortField::ALL[app.selected_column],
            SortField::Age,
            "mouse sorting should also select the clicked column"
        );

        assert!(app.handle_mouse(click));
        assert!(app.sort_descending);
        assert!(!app.handle_mouse(MouseEvent { row: 8, ..click }));

        // Clicking a computed column (like IP / ARCH / KERNEL / JSONPath / TableIndex) sorts and toggles direction
        let click_ip = MouseEvent {
            column: 58,
            ..click
        };
        assert!(app.handle_mouse(click_ip));
        assert_eq!(
            app.column_sort_override,
            Some(ColumnSortKey::Computed("IP"))
        );
        assert!(!app.sort_descending);
        assert!(app.handle_mouse(click_ip));
        assert!(app.sort_descending);
    }

    #[test]
    fn clicking_a_resource_row_moves_the_cursor_without_changing_marks() {
        let mut app = app_with_rows();
        app.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE));
        app.set_resource_rows_hitbox(10, 8, 80, 1, 1);
        let click = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 24,
            row: 8,
            modifiers: KeyModifiers::NONE,
        };

        assert!(app.handle_mouse(click));
        assert_eq!(
            app.selected_row().map(|row| row.name.as_ref()),
            Some("worker-0")
        );
        assert_eq!(app.marks.len(), 1, "clicking does not alter marked rows");
        assert!(!app.handle_mouse(MouseEvent { row: 9, ..click }));
    }

    #[test]
    fn number_keys_switch_recent_namespaces_and_refresh_recency() {
        let mut app = app_with_rows();
        type_input(&mut app, ':', "ns prod");
        type_input(&mut app, ':', "ns dev");
        assert_eq!(app.recent_namespaces, vec!["dev", "prod"]);

        let command = app.handle_key(KeyEvent::new(KeyCode::Char('0'), KeyModifiers::NONE));
        assert_eq!(
            command,
            AppCommand::Request(ViewState {
                instance: None,
                context: "demo".to_owned(),
                kind: ResourceKind::Pods.into(),
                namespace: None,
                field_selector: None,
                label_selector: None,
            })
        );
        assert_eq!(app.recent_namespaces, vec!["dev", "prod"]);

        let command = app.handle_key(KeyEvent::new(KeyCode::Char('2'), KeyModifiers::NONE));
        assert!(matches!(
            command,
            AppCommand::Request(ViewState {
                namespace: Some(namespace),
                ..
            }) if namespace == "prod"
        ));
        assert_eq!(app.recent_namespaces, vec!["prod", "dev"]);
    }

    #[test]
    fn recent_namespaces_are_capped_at_nine_named_entries() {
        let mut app = app_with_rows();
        for index in 0..12 {
            type_input(&mut app, ':', &format!("ns team-{index}"));
        }
        assert_eq!(app.recent_namespaces.len(), 9);
        assert_eq!(app.recent_namespaces[0], "team-11");
        assert_eq!(app.recent_namespaces[8], "team-3");
    }

    #[test]
    fn initial_named_namespace_starts_at_key_one() {
        let mut app = App::new(
            "demo",
            Some("default".to_owned()),
            ResourceKind::Pods,
            false,
            vec!["demo".to_owned()],
        );
        assert_eq!(app.recent_namespaces, vec!["default"]);
        assert!(matches!(
            app.handle_key(KeyEvent::new(KeyCode::Char('0'), KeyModifiers::NONE)),
            AppCommand::Request(ViewState {
                namespace: None,
                ..
            })
        ));
    }

    #[test]
    fn namespace_chooser_filters_visible_rows_and_selects_the_mapped_namespace() {
        let mut app = App::new(
            "demo",
            Some("prod".to_owned()),
            ResourceKind::Pods,
            false,
            vec!["demo".to_owned()],
        );
        app.set_namespace_options(
            "demo",
            vec![
                NamespaceInfo::discovered("dev", "Terminating", None),
                NamespaceInfo::discovered("prod", "Active", None),
            ],
        );
        assert_eq!(type_input(&mut app, ':', "ns"), AppCommand::None);
        assert_eq!(app.overlay, Overlay::Namespaces);
        assert_eq!(app.namespace_options[app.overlay_selected].name, "prod");

        app.handle_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE));
        for character in "Terminating".chars() {
            app.handle_key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE));
        }
        assert_eq!(app.overlay, Overlay::Namespaces);
        assert_eq!(app.body_query, "Terminating");
        assert_eq!(app.query.len(), 0);
        assert_eq!(app.visible_namespace_indices(), vec![1]);
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            AppCommand::Request(ViewState {
                instance: None,
                context: "demo".to_owned(),
                kind: ResourceKind::Pods.into(),
                namespace: Some("dev".to_owned()),
                field_selector: None,
                label_selector: None,
            })
        );
    }

    #[test]
    fn namespace_chooser_y_requests_yaml_for_the_selected_namespace() {
        let namespace = descriptor("", "v1", "v1", "Namespace", "namespaces", false);
        let mut app = app_with_rows();
        let mut resources = app.resource_options.clone();
        resources.push(namespace.clone());
        app.set_resource_options(resources);
        app.set_namespace_options(
            "demo",
            vec![NamespaceInfo::discovered("prod", "Active", None)],
        );
        assert_eq!(type_input(&mut app, ':', "ns"), AppCommand::None);
        app.overlay_selected = app
            .namespace_options
            .iter()
            .position(|option| option.name == "prod")
            .expect("prod namespace");

        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE)),
            AppCommand::Action(ActionIntent::Content {
                id: app.yaml_refresh.id,
                target: ResourceTarget {
                    kind: namespace,
                    namespace: None,
                    name: "prod".to_owned(),
                },
                content: ContentKind::Yaml,
            })
        );
    }

    #[test]
    fn namespace_chooser_e_edits_the_selected_namespace() {
        let namespace = descriptor("", "v1", "v1", "Namespace", "namespaces", false);
        let mut app = app_with_rows();
        let mut resources = app.resource_options.clone();
        resources.push(namespace.clone());
        app.set_resource_options(resources);
        app.set_namespace_options(
            "demo",
            vec![NamespaceInfo::discovered("prod", "Active", None)],
        );
        assert_eq!(type_input(&mut app, ':', "ns"), AppCommand::None);
        app.overlay_selected = app
            .namespace_options
            .iter()
            .position(|option| option.name == "prod")
            .expect("prod namespace");

        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE)),
            AppCommand::Edit(ResourceTarget {
                kind: namespace,
                namespace: None,
                name: "prod".to_owned(),
            })
        );
    }

    #[test]
    fn namespace_u_uses_selected_namespace_and_enter_on_namespace_resource_opens_pods() {
        let namespace = descriptor("", "v1", "v1", "Namespace", "namespaces", false);
        let mut app = app_with_descriptor(
            namespace.clone(),
            Some("default"),
            ResourceRow::new("-", "prod", "Active", "-", "", 0),
        );
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::NONE)),
            AppCommand::None
        );
        assert_eq!(app.namespace.as_deref(), Some("prod"));
        assert_eq!(
            app.recent_namespaces.first().map(String::as_str),
            Some("prod")
        );
        assert_eq!(app.kind, namespace);

        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            AppCommand::Request(ViewState {
                instance: None,
                context: "demo".to_owned(),
                kind: ResourceKind::Pods.into(),
                namespace: Some("prod".to_owned()),
                field_selector: None,
                label_selector: None,
            })
        );

        app.set_namespace_options(
            "demo",
            vec![
                NamespaceInfo::discovered("dev", "Active", None),
                NamespaceInfo::discovered("prod", "Active", None),
            ],
        );
        assert_eq!(type_input(&mut app, ':', "ns"), AppCommand::None);
        app.overlay_selected = app
            .namespace_options
            .iter()
            .position(|option| option.name == "dev")
            .expect("dev namespace");
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::NONE)),
            AppCommand::None
        );
        assert_eq!(app.namespace.as_deref(), Some("dev"));
        assert_eq!(app.overlay, Overlay::Namespaces);
    }

    #[test]
    fn details_body_e_edits_the_selected_resource() {
        let widget = descriptor(
            "example.io",
            "v1",
            "example.io/v1",
            "Widget",
            "widgets",
            true,
        );
        let mut app = app_with_descriptor(
            widget.clone(),
            Some("prod"),
            ResourceRow::new("prod", "selected", "Ready", "1/1", "-", 0),
        );
        app.details_row = app.selected_row().cloned();
        app.overlay = Overlay::Details;

        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE)),
            AppCommand::Edit(ResourceTarget {
                kind: widget,
                namespace: Some("prod".to_owned()),
                name: "selected".to_owned(),
            })
        );
    }

    #[test]
    fn yaml_body_e_edits_the_resource_that_was_opened() {
        let mut app = app_with_rows();
        let target = ResourceTarget {
            kind: ResourceKind::Pods.into(),
            namespace: Some("prod".to_owned()),
            name: "api-0".to_owned(),
        };
        assert_eq!(
            app.content_action(ContentKind::Yaml),
            AppCommand::Action(ActionIntent::Content {
                id: app.yaml_refresh.id,
                target: target.clone(),
                content: ContentKind::Yaml,
            })
        );
        app.selected = 1;
        app.show_content(
            "YAML(prod/api-0)",
            "kind: Pod",
            None,
            None,
            None,
            ContentKind::Yaml,
        );

        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE)),
            AppCommand::Edit(target)
        );
        assert_eq!(app.overlay, Overlay::Content);
    }

    #[test]
    fn read_only_yaml_edit_keeps_the_body_open() {
        let mut app = app_with_rows();
        app.show_content(
            "YAML(prod/api-0)",
            "kind: Pod",
            None,
            None,
            None,
            ContentKind::Yaml,
        );
        app.read_only = true;

        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE)),
            AppCommand::None
        );
        assert_eq!(app.overlay, Overlay::Content);
        assert_eq!(
            app.notice.as_deref(),
            Some("Edit is disabled in read-only mode")
        );
    }

    #[test]
    fn resource_chooser_filters_qualified_names_and_aliases_without_hidden_selection() {
        let mut app = app_with_rows();
        app.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL));
        assert_eq!(app.overlay, Overlay::Aliases);
        assert_eq!(app.resource_options[app.overlay_selected], app.kind);

        app.viewport_height = 2;
        app.handle_key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE));
        assert_eq!(app.overlay_selected, 2);
        app.handle_key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE));
        assert_eq!(app.overlay_selected, 0);

        app.handle_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE));
        for character in "deployments.apps".chars() {
            app.handle_key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE));
        }
        let visible = app.visible_resource_indices();
        assert_eq!(visible.len(), 1);
        assert_eq!(
            app.resource_options[visible[0]].known_kind(),
            Some(ResourceKind::Deployments)
        );

        app.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        assert_eq!(app.visible_resource_indices().len(), 0);
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.overlay, Overlay::Aliases);

        app.handle_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        for character in "svc".chars() {
            app.handle_key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE));
        }
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            AppCommand::Request(ViewState { kind, .. })
                if kind.known_kind() == Some(ResourceKind::Services)
        ));
    }

    #[test]
    fn escape_leaves_chooser_filter_before_closing_the_chooser() {
        let mut app = app_with_rows();
        app.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL));
        app.handle_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE));

        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.input_mode, InputMode::Browse);
        assert_eq!(app.overlay, Overlay::Aliases);
        assert_eq!(app.body_query.len(), 0);

        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.overlay, Overlay::None);
    }

    #[test]
    fn chooser_mouse_click_maps_the_visible_row_to_its_underlying_option() {
        let mut app = app_with_rows();
        app.overlay = Overlay::Aliases;
        app.set_chooser_hitbox(10, 6, 40, vec![4, 1]);
        let click = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 12,
            row: 7,
            modifiers: KeyModifiers::NONE,
        };

        assert!(app.handle_mouse(click));
        assert_eq!(app.overlay_selected, 1);
    }

    #[test]
    fn command_and_filter_modes_are_available_in_every_body_and_list_view() {
        fn exercise(app: &mut App, label: &str, body_filter: bool) {
            let overlay = app.overlay;
            assert_eq!(
                app.handle_key(KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE)),
                AppCommand::None,
                "{label} command mode"
            );
            assert_eq!(app.input_mode, InputMode::Command, "{label}");
            assert_eq!(app.overlay, overlay, "{label} must stay in place");
            app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

            assert_eq!(
                app.handle_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE)),
                AppCommand::None,
                "{label} filter mode"
            );
            app.handle_key(KeyEvent::new(KeyCode::Char('z'), KeyModifiers::NONE));
            assert_eq!(app.input_mode, InputMode::Filter, "{label}");
            assert_eq!(app.overlay, overlay, "{label} must stay in place");
            if body_filter {
                assert_eq!(app.body_query, "z", "{label} body filter");
                assert!(app.query.is_empty(), "{label} resource filter isolation");
            } else {
                assert_eq!(app.query, "z", "{label} list filter");
                assert!(app.body_query.is_empty(), "{label} body filter isolation");
            }
        }

        let mut content = app_with_rows();
        content.show_content(
            "YAML(prod/api-0)",
            "kind: Pod",
            None,
            None,
            None,
            ContentKind::Yaml,
        );
        exercise(&mut content, "content", true);

        let mut contexts = app_with_rows();
        contexts.overlay = Overlay::Contexts;
        exercise(&mut contexts, "contexts", true);

        let mut aliases = app_with_rows();
        aliases.overlay = Overlay::Aliases;
        exercise(&mut aliases, "resource chooser", true);

        let mut namespaces = app_with_rows();
        namespaces.overlay = Overlay::Namespaces;
        exercise(&mut namespaces, "namespace chooser", true);

        let mut details = app_with_rows();
        details.details_row = details.selected_row().cloned();
        details.overlay = Overlay::Details;
        exercise(&mut details, "details", true);

        let mut containers = app_with_rows();
        containers.list_mode = ListMode::Containers {
            namespace: "prod".to_owned(),
            pod: "api-0".to_owned(),
        };
        exercise(&mut containers, "containers", false);

        let mut policy = app_with_rows();
        policy.list_mode = ListMode::Policy {
            title: "policy(ServiceAccount:prod/api)".to_owned(),
        };
        exercise(&mut policy, "policy", false);
    }

    #[test]
    fn zero_switches_to_all_namespaces_even_from_content() {
        let mut app = App::new(
            "demo",
            Some("prod".to_owned()),
            ResourceKind::Pods,
            false,
            vec!["demo".to_owned()],
        );
        app.show_content(
            "YAML(prod/api-0)",
            "kind: Pod",
            None,
            None,
            None,
            ContentKind::Yaml,
        );

        assert!(matches!(
            app.handle_key(KeyEvent::new(KeyCode::Char('0'), KeyModifiers::NONE)),
            AppCommand::Request(ViewState {
                namespace: None,
                ..
            })
        ));
        assert_eq!(app.namespace, None);
        assert_eq!(app.overlay, Overlay::None);
    }

    #[test]
    fn command_tab_completes_resources_namespaces_and_contexts() {
        let mut app = app_with_rows();
        enter_command(&mut app, "dep");
        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(app.input, "deploy");

        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        enter_command(&mut app, "ns pr");
        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(app.input, "ns prod");

        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        app.set_namespace_options("demo", vec!["kube-system".to_owned(), "prod".to_owned()]);
        enter_command(&mut app, "ns ku");
        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(app.input, "ns kube-system");

        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        enter_command(&mut app, "ctx d");
        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(app.input, "ctx demo");
    }

    #[test]
    fn custom_alias_argument_completion_preserves_alias_and_existing_arguments() {
        let mut app = app_with_rows();
        app.user_config.aliases.extend([
            ("PP".to_owned(), "pods".to_owned()),
            ("chain".to_owned(), "PP".to_owned()),
            ("cx".to_owned(), "ctx".to_owned()),
            ("production".to_owned(), "pods prod".to_owned()),
            ("labeled".to_owned(), "pods app=api".to_owned()),
            ("loop".to_owned(), "loop".to_owned()),
        ]);
        assert!(app.command_completions_for("pp").contains(&"PP".to_owned()));
        assert_eq!(app.command_completions_for("PP pr"), ["PP prod"]);
        assert_eq!(app.command_completions_for("chain pr"), ["chain prod"]);
        assert_eq!(app.command_completions_for("cx de"), ["cx demo"]);
        assert_eq!(
            app.command_completions_for("production "),
            Vec::<String>::new()
        );
        assert_eq!(
            app.command_completions_for("labeled "),
            Vec::<String>::new()
        );
        assert_eq!(app.command_completions_for("loop "), Vec::<String>::new());

        enter_command(&mut app, "PP pr");
        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(app.input, "PP prod");
        assert!(matches!(
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            AppCommand::Request(ViewState { namespace: Some(namespace), .. }) if namespace == "prod"
        ));
    }

    fn enter_command(app: &mut App, value: &str) {
        app.handle_key(KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE));
        for character in value.chars() {
            app.handle_key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE));
        }
    }

    #[test]
    fn logo_can_be_hidden_and_toggled() {
        let mut app = app_with_rows();
        app.set_logo_visible(false);
        assert!(!app.ui.logo());
        app.handle_key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::CONTROL));
        assert!(app.ui.logo());
    }

    #[test]
    fn yaml_content_copies_the_active_encoded_or_decoded_variant() {
        let mut app = app_with_rows();
        app.show_content(
            "YAML(prod/api-0)",
            "data:\n  token: c2VjcmV0",
            Some("metadata:\n  managedFields: []\ndata:\n  token: c2VjcmV0".to_owned()),
            Some("data:\n  token: secret".to_owned()),
            Some("metadata:\n  managedFields: []\ndata:\n  token: secret".to_owned()),
            ContentKind::Yaml,
        );
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE)),
            AppCommand::Copy("data:\n  token: c2VjcmV0".to_owned())
        );
        app.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        assert!(app.is_content_decoded());
        assert_eq!(app.active_content_body(), "data:\n  token: secret");
        app.handle_key(KeyEvent::new(KeyCode::Char('m'), KeyModifiers::NONE));
        assert!(app.ui.managed_fields());
        assert!(matches!(
            app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE)),
            AppCommand::Copy(body) if body.contains("managedFields") && body.contains("secret")
        ));
        app.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        assert!(!app.is_content_decoded());
        assert!(app.active_content_body().contains("c2VjcmV0"));

        app.show_transient_notice("YAML copied to clipboard");
        assert_eq!(app.overlay, Overlay::Content);
        assert_eq!(app.notice.as_deref(), Some("YAML copied to clipboard"));
    }

    #[test]
    fn log_ranges_preserve_pause_and_reject_previous_session_batches() {
        let mut app = app_with_rows();
        app.resource_log_action(false);
        let initial_id = app.log_view.as_ref().unwrap().request.id;
        app.append_log_batch(initial_id, vec!["initial".into()], false, None);
        app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE));
        let generation = app.generation;
        for (key, range) in [
            ('0', LogRange::Tail),
            ('1', LogRange::Head),
            ('2', LogRange::SinceSeconds(60)),
            ('3', LogRange::SinceSeconds(300)),
            ('4', LogRange::SinceSeconds(900)),
            ('5', LogRange::SinceSeconds(1_800)),
            ('6', LogRange::SinceSeconds(3_600)),
        ] {
            let command = app.handle_key(KeyEvent::new(KeyCode::Char(key), KeyModifiers::NONE));
            assert!(
                matches!(command, AppCommand::Action(ActionIntent::StartLogs { request }) if request.range == range)
            );
            assert_eq!(
                app.generation, generation,
                "log range digits must not switch namespaces"
            );
            assert!(!app.log_view.as_ref().unwrap().auto_scroll);
            app.append_log_batch(initial_id, vec!["stale".into()], false, None);
            assert_eq!(app.content_body.len(), 0);
        }
        let id = app.log_view.as_ref().unwrap().request.id;
        app.append_log_batch(id, vec!["refreshed".into()], false, None);
        assert_eq!(
            app.content_body, "refreshed\n",
            "paused ranges get one refresh"
        );
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        app.append_log_batch(id, vec!["late".into()], false, None);
        assert_eq!(app.overlay, Overlay::None);
        assert!(!app.content_body.contains("late"));
    }

    #[test]
    fn log_pause_mark_resume_clear_and_column_lock_preserve_stream_state() {
        let mut app = app_with_rows();
        app.resource_log_action(false);
        let id = app.log_view.as_ref().unwrap().request.id;
        app.append_log_batch(id, vec!["one".into()], false, None);
        app.content_horizontal_scroll = 12;
        app.handle_key(KeyEvent::new(KeyCode::Char('L'), KeyModifiers::SHIFT));
        app.append_log_batch(id, vec!["two".into()], false, None);
        assert_eq!(app.content_horizontal_scroll, 12);
        app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Char('m'), KeyModifiers::NONE));
        assert!(!app.log_view.as_ref().unwrap().auto_scroll);
        app.append_log_batch(id, vec!["three".into()], false, None);
        assert!(!app.content_body.contains("three"));
        assert!(app.content_body.contains("────"));
        app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE));
        assert!(app.content_body.contains("three"));
        assert!(app.content_body.contains("────"));
        app.handle_key(KeyEvent::new(KeyCode::Char('L'), KeyModifiers::SHIFT));
        app.show_transient_notice("Logs saved to example.log");
        app.append_log_batch(id, vec!["four".into()], false, None);
        assert_eq!(app.content_horizontal_scroll, 0);
        assert_eq!(app.notice.as_deref(), Some("Logs saved to example.log"));
        app.handle_key(KeyEvent::new(KeyCode::Char('C'), KeyModifiers::SHIFT));
        assert_eq!(app.content_body.len(), 0);
        assert_eq!(app.log_view.as_ref().unwrap().request.id, id);
        app.append_log_batch(id, vec!["after clear".into()], false, None);
        assert_eq!(app.content_body, "after clear\n");
    }

    #[test]
    fn log_save_exports_filtered_display_and_escape_clears_filter_before_closing() {
        let mut app = app_with_rows();
        app.resource_log_action(false);
        let id = app.log_view.as_ref().unwrap().request.id;
        app.append_log_batch(
            id,
            vec![
                "2026-09-07T00:00:00Z \x1b[31mERROR example\x1b[0m".into(),
                "2026-09-07T00:00:01Z INFO running".into(),
            ],
            false,
            None,
        );
        app.body_query = "ERROR".to_owned();
        assert!(
            matches!(app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL)),
            AppCommand::SaveLogs { body, .. } if body == "ERROR example")
        );
        app.append_log_batch(
            id,
            vec!["2026-09-07T00:00:02Z ERROR second".into()],
            false,
            None,
        );
        app.body_query = "^ERROR".to_owned();
        app.content_scroll = 0;
        app.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
        assert_eq!(app.content_scroll, 1, "matches use timestamp-hidden text");
        app.handle_key(KeyEvent::new(KeyCode::Char('N'), KeyModifiers::SHIFT));
        assert_eq!(app.content_scroll, 0);
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.body_query.len(), 0);
        assert!(app.is_logs_view());
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            AppCommand::Action(ActionIntent::StopLogs { id })
        );
    }

    #[test]
    fn current_and_previous_logs_copy_the_displayed_text() {
        for content in [ContentKind::Logs, ContentKind::PreviousLogs] {
            let mut app = app_with_rows();
            let body = "2026-09-01T01:02:03Z INFO started\nplain second line";
            app.show_content("Logs(prod/api-0)", body, None, None, None, content);

            assert_eq!(
                app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE)),
                AppCommand::Copy("INFO started\nplain second line".to_owned())
            );
            app.handle_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE));
            assert_eq!(app.log_export_text(), body);
            app.body_query = "started".to_owned();
            assert_eq!(app.log_export_text(), "2026-09-01T01:02:03Z INFO started");
            assert_eq!(app.overlay, Overlay::Content);
        }
    }

    #[test]
    fn yaml_auto_refresh_preserves_view_state_and_uses_the_opened_target() {
        let mut app = app_with_rows();
        app.user_config.settings.live_view_auto_refresh = true;
        let AppCommand::Action(ActionIntent::Content { target, .. }) =
            app.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE))
        else {
            panic!("expected YAML action");
        };
        app.show_content(
            "YAML",
            "old",
            Some("old managed".to_owned()),
            Some("old decoded".to_owned()),
            Some("old decoded managed".to_owned()),
            ContentKind::Yaml,
        );
        assert_eq!(app.poll_yaml_refresh(), AppCommand::None);
        app.content_scroll = 7;
        app.content_horizontal_scroll = 3;
        app.body_query = "status".to_owned();
        app.ui.toggle(UiFlags::MANAGED_FIELDS);
        app.ui.toggle(UiFlags::SECRET_DECODED);
        app.ui.toggle(UiFlags::WRAP);
        app.ui.toggle(UiFlags::FULLSCREEN);
        let flags = app.ui;
        app.selected = 1;
        app.yaml_refresh.due = Instant::now();
        assert_eq!(
            app.poll_yaml_refresh(),
            AppCommand::Action(ActionIntent::RefreshYaml {
                id: app.yaml_refresh.id,
                target: target.clone(),
                include_managed_fields: true,
            })
        );
        assert_eq!(
            app.poll_yaml_refresh(),
            AppCommand::None,
            "only one in-flight refresh"
        );
        app.apply_yaml_refresh(
            app.yaml_refresh.id,
            &target,
            Ok(YamlContent {
                body: "new".to_owned(),
                with_managed_fields: Some("new managed".to_owned()),
                decoded_body: Some("new decoded".to_owned()),
                decoded_with_managed_fields: Some("new decoded managed".to_owned()),
            }),
        );
        assert_eq!(app.active_content_body(), "new decoded managed");
        assert_eq!(app.ui, flags);
        assert_eq!(app.content_scroll, 7);
        assert_eq!(app.content_horizontal_scroll, 3);
        assert_eq!(app.body_query, "status");
        assert_eq!(app.overlay, Overlay::Content);
        assert_eq!(
            app.poll_yaml_refresh(),
            AppCommand::None,
            "wait before refreshing again"
        );
    }

    #[test]
    fn yaml_refresh_errors_retry_and_stale_results_cannot_replace_a_reopened_view() {
        let mut app = app_with_rows();
        app.user_config.settings.live_view_auto_refresh = true;
        let AppCommand::Action(ActionIntent::Content { target, .. }) =
            app.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE))
        else {
            panic!("expected YAML action");
        };
        app.show_content(
            "YAML",
            "original",
            Some(String::new()),
            None,
            None,
            ContentKind::Yaml,
        );
        app.yaml_refresh.due = Instant::now();
        let AppCommand::Action(ActionIntent::RefreshYaml {
            id,
            include_managed_fields,
            ..
        }) = app.poll_yaml_refresh()
        else {
            panic!("expected automatic refresh");
        };
        assert!(!include_managed_fields, "managed fields remain lazy");
        app.apply_yaml_refresh(id, &target, Err("unavailable".to_owned()));
        assert_eq!(app.content_body, "original");
        assert_eq!(app.overlay, Overlay::Content);
        assert!(!app.yaml_refresh.pending);
        assert!(
            app.notice
                .as_deref()
                .unwrap()
                .contains("YAML refresh failed:")
        );
        app.yaml_refresh.due = Instant::now();
        assert!(matches!(
            app.poll_yaml_refresh(),
            AppCommand::Action(ActionIntent::RefreshYaml { .. })
        ));
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        app.apply_yaml_refresh(id, &target, Err("late error".to_owned()));
        assert_eq!(app.overlay, Overlay::None);
        assert_eq!(app.poll_yaml_refresh(), AppCommand::None);
        app.show_content("YAML", "reopened", None, None, None, ContentKind::Yaml);
        app.apply_yaml_refresh(
            id,
            &target,
            Ok(YamlContent {
                body: "stale".to_owned(),
                with_managed_fields: None,
                decoded_body: None,
                decoded_with_managed_fields: None,
            }),
        );
        assert_eq!(app.content_body, "reopened");
        app.content_kind = Some(ContentKind::Describe);
        app.yaml_refresh.due = Instant::now();
        assert!(matches!(
            app.poll_yaml_refresh(),
            AppCommand::Action(ActionIntent::RefreshDescribe { .. })
        ));
    }

    #[test]
    fn yaml_managed_fields_load_lazily_and_ignore_stale_results() {
        let mut app = app_with_rows();
        app.user_config.settings.live_view_auto_refresh = true;
        let target = match app.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE)) {
            AppCommand::Action(ActionIntent::Content { target, .. }) => target,
            command => panic!("expected YAML action, got {command:?}"),
        };
        app.show_content(
            "YAML(prod/api-0)",
            "kind: Pod",
            Some(String::new()),
            None,
            None,
            ContentKind::Yaml,
        );

        app.yaml_refresh.due = Instant::now();
        let old_refresh_id = app.yaml_refresh.id;
        assert!(matches!(
            app.poll_yaml_refresh(),
            AppCommand::Action(ActionIntent::RefreshYaml { .. })
        ));

        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Char('m'), KeyModifiers::NONE)),
            AppCommand::Action(ActionIntent::ManagedYaml {
                id: app.yaml_refresh.id,
                target: target.clone(),
                body: "kind: Pod".to_owned(),
                decoded_body: None,
            })
        );
        app.apply_yaml_refresh(old_refresh_id, &target, Err("stale refresh".to_owned()));
        assert_eq!(app.notice, None);
        assert_eq!(
            app.poll_yaml_refresh(),
            AppCommand::None,
            "managed-fields request takes precedence"
        );
        assert!(!app.ui.managed_fields());

        let mut stale = target.clone();
        stale.name.push_str("-stale");
        app.show_managed_yaml(app.yaml_refresh.id, &stale, Ok(("stale".to_owned(), None)));
        assert!(!app.ui.managed_fields());

        app.content_scroll = 7;
        app.content_horizontal_scroll = 3;
        app.body_query = "metadata".to_owned();
        app.show_managed_yaml(
            app.yaml_refresh.id,
            &target,
            Ok((
                "metadata:\n  managedFields:\n  - manager: kubelet".to_owned(),
                None,
            )),
        );
        assert!(app.ui.managed_fields());
        assert!(app.active_content_body().contains("managedFields"));
        assert_eq!(app.content_scroll, 7);
        assert_eq!(app.content_horizontal_scroll, 3);
        assert_eq!(app.body_query, "metadata");
        app.handle_key(KeyEvent::new(KeyCode::Char('m'), KeyModifiers::NONE));
        assert_eq!(app.active_content_body(), "kind: Pod");
        assert_eq!(app.content_scroll, 7);
        assert_eq!(app.content_horizontal_scroll, 3);
        assert_eq!(app.body_query, "metadata");
    }

    #[test]
    fn discovered_resources_participate_in_commands_and_completion() {
        let mut app = app_with_rows();
        let secret = ResourceDescriptor::new(
            "",
            "v1",
            "v1",
            "Secret",
            "secrets",
            true,
            crate::model::ResourceOperations::all(),
        );
        let widget = ResourceDescriptor::new(
            "example.io",
            "v1",
            "example.io/v1",
            "Widget",
            "widgets",
            true,
            crate::model::ResourceOperations::all(),
        )
        .with_short_names(["wdg"]);
        let mut resources: Vec<_> = ResourceKind::ALL
            .into_iter()
            .map(ResourceDescriptor::from)
            .collect();
        resources.extend([secret.clone(), widget.clone()]);
        app.set_resource_options(resources);

        enter_command(&mut app, "sec");
        assert!(
            app.command_suggestions()
                .iter()
                .any(|name| name == "secret")
        );
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

        let command = type_input(&mut app, ':', "wdg prod");
        assert_eq!(
            command,
            AppCommand::Request(ViewState {
                instance: None,
                context: "demo".to_owned(),
                kind: widget,
                namespace: Some("prod".to_owned()),
                field_selector: None,
                label_selector: None,
            })
        );
    }

    #[test]
    fn secret_list_decode_requests_data_only_content() {
        let secret = descriptor("", "v1", "v1", "Secret", "secrets", true);
        let mut app = app_with_descriptor(
            secret.clone(),
            Some("prod"),
            ResourceRow::new("prod", "api-token", "Active", "-", "Opaque", 0),
        );

        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)),
            AppCommand::Action(ActionIntent::Content {
                id: app.yaml_refresh.id,
                target: ResourceTarget {
                    kind: secret,
                    namespace: Some("prod".to_owned()),
                    name: "api-token".to_owned(),
                },
                content: ContentKind::SecretData,
            })
        );
    }

    #[test]
    fn enter_requests_authorization_policy_for_each_rbac_resource() {
        let resources = [
            ("", "v1", "v1", "ServiceAccount", "serviceaccounts", true),
            (
                "rbac.authorization.k8s.io",
                "v1",
                "rbac.authorization.k8s.io/v1",
                "Role",
                "roles",
                true,
            ),
            (
                "rbac.authorization.k8s.io",
                "v1",
                "rbac.authorization.k8s.io/v1",
                "ClusterRole",
                "clusterroles",
                false,
            ),
            (
                "rbac.authorization.k8s.io",
                "v1",
                "rbac.authorization.k8s.io/v1",
                "RoleBinding",
                "rolebindings",
                true,
            ),
            (
                "rbac.authorization.k8s.io",
                "v1",
                "rbac.authorization.k8s.io/v1",
                "ClusterRoleBinding",
                "clusterrolebindings",
                false,
            ),
        ];

        for (group, version, api_version, kind, plural, namespaced) in resources {
            let resource = descriptor(group, version, api_version, kind, plural, namespaced);
            let namespace = namespaced.then_some("prod");
            let row_namespace = namespace.unwrap_or("-");
            let mut app = app_with_descriptor(
                resource.clone(),
                namespace,
                ResourceRow::new(row_namespace, "reader", "Active", "-", "", 0),
            );

            assert_eq!(
                app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
                AppCommand::Action(ActionIntent::AuthorizationPolicy {
                    target: ResourceTarget {
                        kind: resource,
                        namespace: namespace.map(str::to_owned),
                        name: "reader".to_owned(),
                    },
                }),
                "{kind}"
            );
        }
    }

    #[test]
    fn subject_aliases_request_cluster_wide_subject_views() {
        for (alias, subject_kind) in [
            ("users", ResourceDescriptor::users()),
            ("usr", ResourceDescriptor::users()),
            ("user", ResourceDescriptor::users()),
            ("groups", ResourceDescriptor::groups()),
            ("grp", ResourceDescriptor::groups()),
            ("group", ResourceDescriptor::groups()),
        ] {
            let mut app = app_with_rows();
            assert_eq!(
                type_input(&mut app, ':', alias),
                AppCommand::Request(ViewState {
                    instance: None,
                    context: "demo".to_owned(),
                    kind: subject_kind.clone(),
                    namespace: None,
                    field_selector: None,
                    label_selector: None,
                }),
                "command alias {alias}"
            );
        }
    }

    #[test]
    fn policy_child_view_filters_and_escape_restores_the_parent_list() {
        let service_account = descriptor("", "v1", "v1", "ServiceAccount", "serviceaccounts", true);
        let mut app = app_with_descriptor(
            service_account.clone(),
            Some("prod"),
            ResourceRow::new("prod", "api", "Active", "-", "", 0),
        );
        app.query = "api".to_owned();
        app.rebuild_filter(None);
        let AppCommand::Action(ActionIntent::AuthorizationPolicy { target }) =
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        else {
            panic!("expected policy action");
        };
        app.show_policy(
            &target,
            "policy(ServiceAccount:prod/api)".to_owned(),
            vec![Arc::new(
                ResourceRow::new("-", "pods", "core", "Role:reader", "", 0).with_policy_data(
                    crate::model::PolicyData {
                        verbs: vec![Arc::from("get"), Arc::from("list")],
                    },
                ),
            )],
        );

        assert!(app.is_policy_view());
        assert_eq!(app.query, "");
        type_input(&mut app, '/', "pods");
        assert_eq!(app.filtered.len(), 1);
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            AppCommand::None
        );
        assert!(app.is_resource_view());
        assert_eq!(app.query, "api");
        assert_eq!(app.selected_row().map(|row| row.name.as_ref()), Some("api"));
    }

    #[test]
    fn enter_on_node_requests_server_filtered_pods_and_history_returns() {
        let mut app = App::new(
            "demo",
            None,
            ResourceKind::Nodes,
            false,
            vec!["demo".to_owned()],
        );
        app.apply_snapshot(snapshot(
            vec![ResourceRow::new(
                "-",
                "worker-07",
                "Ready",
                "worker",
                "v1.32.0",
                0,
            )],
            1,
            ResourceKind::Nodes,
        ));

        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            AppCommand::Request(ViewState {
                instance: None,
                context: "demo".to_owned(),
                kind: ResourceKind::Pods.into(),
                namespace: None,
                field_selector: Some("spec.nodeName=worker-07".to_owned()),
                label_selector: None,
            })
        );
        assert_eq!(
            app.field_selector.as_deref(),
            Some("spec.nodeName=worker-07")
        );
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            AppCommand::Request(ViewState {
                instance: None,
                context: "demo".to_owned(),
                kind: ResourceKind::Nodes.into(),
                namespace: None,
                field_selector: None,
                label_selector: None,
            })
        );
    }

    #[test]
    fn o_on_pod_opens_its_node_and_history_returns() {
        let mut app = app_with_rows();

        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::NONE)),
            AppCommand::Request(ViewState {
                instance: None,
                context: "demo".to_owned(),
                kind: ResourceKind::Nodes.into(),
                namespace: None,
                field_selector: Some("metadata.name=node-a".to_owned()),
                label_selector: None,
            })
        );
        assert_eq!(app.kind.known_kind(), Some(ResourceKind::Nodes));
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            AppCommand::Request(ViewState {
                instance: None,
                context: "demo".to_owned(),
                kind: ResourceKind::Pods.into(),
                namespace: None,
                field_selector: None,
                label_selector: None,
            })
        );
    }

    #[test]
    fn o_reports_an_unassigned_pod() {
        let mut app = app_with_descriptor(
            ResourceKind::Pods.into(),
            None,
            ResourceRow::new("prod", "pending", "Pending", "0/1", "0", 0)
                .with_pod_data(PodData::default()),
        );

        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::NONE)),
            AppCommand::None
        );
        assert_eq!(app.notice.as_deref(), Some("Pod is not assigned to a Node"));
    }

    #[test]
    fn pod_container_drilldown_opens_selected_container_logs_and_restores_parent() {
        let mut app = app_with_rows();
        let AppCommand::Action(ActionIntent::Containers { target }) =
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        else {
            panic!("expected container request");
        };
        let container = ResourceRow::new("prod", "api", "Running", "true", "example/api:v1", 0)
            .with_container_data(crate::model::ContainerData {
                image: Arc::from("example/api:v1"),
                kind: Arc::from("container"),
                restarts: 2,
                ..crate::model::ContainerData::default()
            });
        app.show_containers(target, vec![Arc::new(container)]);
        assert!(app.is_container_view());
        assert_eq!(app.selected_row().map(|row| row.name.as_ref()), Some("api"));
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            AppCommand::Action(ActionIntent::StartLogs {
                request: LogRequest {
                    id: 1,
                    range: LogRange::Tail,
                    tail_count: 100,
                    target: LogTarget::pod("prod", "api-0", Some("api".to_owned()), false),
                    all_containers: false,
                },
            })
        );

        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            AppCommand::Action(ActionIntent::StopLogs { id: 1 })
        );
        assert!(app.is_container_view());
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            AppCommand::None
        );
        assert!(!app.is_container_view());
        assert_eq!(
            app.selected_row().map(|row| row.name.as_ref()),
            Some("api-0")
        );
    }

    #[test]
    fn pod_shell_targets_the_resolved_or_selected_container() {
        let mut app = app_with_descriptor(
            ResourceKind::Pods.into(),
            None,
            ResourceRow::new("prod", "api-0", "Running", "1/1", "0", 0).with_pod_data(PodData {
                default_container: Arc::from("api"),
                ..PodData::default()
            }),
        );
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE)),
            AppCommand::Shell(PodShellTarget {
                namespace: "prod".to_owned(),
                pod: "api-0".to_owned(),
                container: "api".to_owned(),
            })
        );

        let AppCommand::Action(ActionIntent::Containers { target }) =
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        else {
            panic!("expected container request");
        };
        app.show_containers(
            target,
            vec![Arc::new(ResourceRow::new(
                "prod",
                "sidecar",
                "Running",
                "true",
                "example/sidecar:v1",
                0,
            ))],
        );
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE)),
            AppCommand::Shell(PodShellTarget {
                namespace: "prod".to_owned(),
                pod: "api-0".to_owned(),
                container: "sidecar".to_owned(),
            })
        );
    }

    #[test]
    fn read_only_mode_blocks_pod_shell() {
        let mut app = app_with_rows();
        app.read_only = true;

        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE)),
            AppCommand::None
        );
        assert_eq!(
            app.notice.as_deref(),
            Some("Shell is disabled in read-only mode")
        );
    }

    #[test]
    fn attach_uses_pod_default_or_selected_container_and_respects_read_only() {
        let mut app = app_with_rows();
        let key = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE);
        assert_eq!(
            app.handle_key(key),
            AppCommand::Attach(PodShellTarget {
                namespace: "prod".to_owned(),
                pod: "api-0".to_owned(),
                container: "api".to_owned(),
            })
        );
        let AppCommand::Action(ActionIntent::Containers { target }) = app.open_selected() else {
            panic!("containers")
        };
        app.show_containers(
            target,
            vec![Arc::new(ResourceRow::new(
                "prod", "sidecar", "Running", "true", "image", 0,
            ))],
        );
        assert!(
            matches!(app.handle_key(key), AppCommand::Attach(PodShellTarget { container, .. }) if container == "sidecar")
        );
        app.read_only = true;
        assert_eq!(app.handle_key(key), AppCommand::None);
        assert!(app.notice.as_deref().unwrap().contains("read-only"));
    }

    #[test]
    fn scale_requires_valid_explicit_count_confirmation_and_keeps_marked_targets() {
        let mut app = app_with_rows();
        app.kind = ResourceKind::Deployments.into();
        for row in app.snapshot.rows.iter() {
            app.marks.insert(row.key.clone());
        }
        app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE));
        assert!(
            matches!(&app.pending_delete, Some(PendingDelete::Scale { targets, .. }) if targets.len() == 2)
        );
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            AppCommand::None
        );
        app.overlay_selected = 2;
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            AppCommand::None
        );
        assert!(app.notice.as_deref().unwrap().contains("Replicas"));
        app.overlay_selected = 0;
        app.handle_key(KeyEvent::new(KeyCode::Char('0'), KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert!(
            matches!(app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            AppCommand::Action(ActionIntent::Scale { targets, replicas: 0 }) if targets.len() == 2)
        );
        assert_eq!(app.marks.len(), 0);
    }

    #[test]
    fn cronjob_trigger_batches_but_toggle_uses_only_current_selection() {
        let mut app = app_with_rows();
        app.kind = descriptor("batch", "v1", "batch/v1", "CronJob", "cronjobs", true);
        for row in app.snapshot.rows.iter() {
            app.marks.insert(row.key.clone());
        }
        app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE));
        assert!(
            matches!(&app.pending_delete, Some(PendingDelete::ToggleCronJobSuspend { target, .. }) if target.name == "api-0")
        );
        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert!(matches!(
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            AppCommand::Action(ActionIntent::ToggleCronJobSuspend { .. })
        ));
        app.handle_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE));
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            AppCommand::None
        );
        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert!(
            matches!(app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)), AppCommand::Action(ActionIntent::TriggerCronJob { targets }) if targets.len() == 2)
        );
    }

    #[test]
    fn read_only_blocks_all_new_workload_actions() {
        for (kind, key) in [
            (ResourceKind::Deployments.into(), 's'),
            (
                descriptor("batch", "v1", "batch/v1", "CronJob", "cronjobs", true),
                't',
            ),
            (
                descriptor("batch", "v1", "batch/v1", "CronJob", "cronjobs", true),
                's',
            ),
        ] {
            let mut app = app_with_rows();
            app.kind = kind;
            app.read_only = true;
            assert_eq!(
                app.handle_key(KeyEvent::new(KeyCode::Char(key), KeyModifiers::NONE)),
                AppCommand::None
            );
            assert!(app.pending_delete.is_none());
            assert!(app.notice.as_deref().unwrap().contains("read-only"));
            assert_eq!(type_input(&mut app, ':', "suspend"), AppCommand::None);
            assert!(app.pending_delete.is_none());
        }
    }

    #[test]
    fn custom_aliases_resolve_gvr_and_server_label_selector() {
        let mut app = app_with_rows();
        app.user_config.aliases.insert(
            "backend".to_owned(),
            "apps/v1/deployments prod app in (api,worker),tier=backend".to_owned(),
        );
        let AppCommand::Request(view) = type_input(&mut app, ':', "backend") else {
            panic!("resource request")
        };
        assert_eq!(view.kind, ResourceKind::Deployments.into());
        assert_eq!(view.namespace.as_deref(), Some("prod"));
        assert_eq!(
            view.label_selector.as_deref(),
            Some("app in (api,worker),tier=backend")
        );
        app.user_config
            .aliases
            .insert("cycle".to_owned(), "cycle".to_owned());
        assert_eq!(type_input(&mut app, ':', "cycle"), AppCommand::None);
        assert!(app.notice.is_some());
    }

    #[test]
    fn config_read_only_cancels_pending_actions_and_cli_floor_cannot_be_disabled() {
        let mut app = app_with_rows();
        app.kind = ResourceKind::Deployments.into();
        app.open_scale_dialog();
        let mut config = UserConfig::default();
        config.settings.read_only = true;
        config.settings.no_exit_on_ctrl_c = true;
        app.apply_user_config(config);
        assert!(app.pending_delete.is_none());
        assert_eq!(app.overlay, Overlay::None);
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            AppCommand::None
        );
        assert_eq!(type_input(&mut app, ':', "q"), AppCommand::Quit);
        app.cli_access = AccessMode::ReadOnly;
        app.apply_user_config(UserConfig::default());
        assert!(app.read_only);
    }

    #[test]
    fn shift_f_opens_pod_port_forward_dialog_and_starts_selected_mapping() {
        let row = ResourceRow::new("prod", "api-0", "Running", "1/1", "0", 0)
            .with_port_forward_ports(vec![PortForwardPort {
                container: Some(Arc::from("api")),
                name: Arc::from("http"),
                port: 8080,
            }]);
        let mut app = app_with_descriptor(ResourceKind::Pods.into(), Some("prod"), row);

        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Char('F'), KeyModifiers::SHIFT)),
            AppCommand::None
        );
        assert_eq!(app.overlay, Overlay::PortForwardDialog);
        let dialog = app.port_forward_dialog.as_ref().unwrap();
        assert_eq!(dialog.remote_port, "api::8080");
        assert_eq!(dialog.local_port, "8080");
        assert_eq!(dialog.address, "localhost");

        app.overlay_selected = PORT_FORWARD_OK_FIELD;
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            AppCommand::StartPortForward(PortForwardRequest {
                target: ResourceTarget {
                    kind: ResourceKind::Pods.into(),
                    namespace: Some("prod".to_owned()),
                    name: "api-0".to_owned(),
                },
                remote_port: 8080,
                local_port: 8080,
                address: "localhost".to_owned(),
            })
        );
        assert_eq!(app.overlay, Overlay::None);
    }

    #[test]
    fn port_forward_dialog_validates_values_without_closing() {
        let row = ResourceRow::new("prod", "api", "ClusterIP", "1 ports", "10.0.0.1", 0);
        let mut app = app_with_descriptor(ResourceKind::Services.into(), Some("prod"), row);
        app.handle_key(KeyEvent::new(KeyCode::Char('F'), KeyModifiers::SHIFT));
        app.overlay_selected = PORT_FORWARD_OK_FIELD;

        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            AppCommand::None
        );
        assert_eq!(app.overlay, Overlay::PortForwardDialog);
        assert_eq!(
            app.notice.as_deref(),
            Some("Container/Service port must be between 1 and 65535")
        );
    }

    #[test]
    fn read_only_mode_blocks_new_port_forwards() {
        let mut app = app_with_rows();
        app.read_only = true;

        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Char('F'), KeyModifiers::SHIFT)),
            AppCommand::None
        );
        assert_eq!(app.overlay, Overlay::None);
        assert_eq!(
            app.notice.as_deref(),
            Some("Port forwarding is disabled in read-only mode")
        );
    }

    #[test]
    fn active_port_forward_overlay_stops_selected_session() {
        let mut app = app_with_rows();
        let target = app.selected_target().unwrap();
        app.show_port_forwards(vec![PortForwardInfo {
            id: 7,
            context: "demo".to_owned(),
            target,
            remote_port: 8080,
            local_port: 18080,
            address: "localhost".to_owned(),
            state: PortForwardState::Running,
        }]);

        assert_eq!(app.overlay, Overlay::PortForwards);
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL)),
            AppCommand::StopPortForward(7)
        );
    }

    #[test]
    fn f_and_pf_command_open_the_global_port_forward_list() {
        let mut app = app_with_rows();
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE)),
            AppCommand::ShowPortForwards
        );
        assert_eq!(
            type_input(&mut app, ':', "pf"),
            AppCommand::ShowPortForwards
        );
        assert!(app.command_names().contains(&"portforwards".to_owned()));
        app.show_port_forwards(Vec::new());
        assert_eq!(app.overlay, Overlay::PortForwards);
        assert_eq!(app.port_forwards.len(), 0);
    }

    #[test]
    fn taints_are_sorted_by_count() {
        let low = ResourceRow::new("-", "low", "Ready", "worker", "", 0).with_node_data(
            crate::model::NodeData {
                taints: Arc::from("<none>"),
                taint_count: 0,
                ..crate::model::NodeData::default()
            },
        );
        let high = ResourceRow::new("-", "high", "Ready", "worker", "", 0).with_node_data(
            crate::model::NodeData {
                taints: Arc::from("a:NoSchedule,b:NoExecute"),
                taint_count: 2,
                ..crate::model::NodeData::default()
            },
        );
        assert_eq!(compare_rows(&low, &high, SortField::Taints), Ordering::Less);
    }

    #[test]
    fn can_command_resolves_user_group_and_service_account_policies() {
        let mut app = app_with_rows();
        app.namespace = Some("prod".to_owned());

        assert_eq!(
            type_input(&mut app, ':', "can u:alice@example.com"),
            AppCommand::Action(ActionIntent::AuthorizationPolicy {
                target: ResourceTarget {
                    kind: ResourceDescriptor::users(),
                    namespace: None,
                    name: "alice@example.com".to_owned(),
                },
            })
        );
        assert_eq!(
            type_input(&mut app, ':', "can g:system:masters"),
            AppCommand::Action(ActionIntent::AuthorizationPolicy {
                target: ResourceTarget {
                    kind: ResourceDescriptor::groups(),
                    namespace: None,
                    name: "system:masters".to_owned(),
                },
            })
        );
        let AppCommand::Action(ActionIntent::AuthorizationPolicy { target }) =
            type_input(&mut app, ':', "can s:builder")
        else {
            panic!("service account policy action");
        };
        assert_eq!(target.kind.kind.as_ref(), "ServiceAccount");
        assert_eq!(target.namespace.as_deref(), Some("prod"));
        assert_eq!(target.name, "builder");

        let AppCommand::Action(ActionIntent::AuthorizationPolicy { target }) =
            type_input(&mut app, ':', "can sa:kube-system/coredns")
        else {
            panic!("explicit namespace service account policy action");
        };
        assert_eq!(target.kind.kind.as_ref(), "ServiceAccount");
        assert_eq!(target.namespace.as_deref(), Some("kube-system"));
        assert_eq!(target.name, "coredns");

        assert_eq!(type_input(&mut app, ':', "can"), AppCommand::None);
        assert!(app.notice.as_deref().unwrap().contains("Usage: :can"));
        assert_eq!(type_input(&mut app, ':', "can u:"), AppCommand::None);
        assert!(app.notice.as_deref().unwrap().contains("cannot be empty"));
        assert_eq!(type_input(&mut app, ':', "can x:bob"), AppCommand::None);
        assert!(
            app.notice
                .as_deref()
                .unwrap()
                .contains("Unknown subject type")
        );
    }

    #[test]
    fn subject_views_open_policy_and_suppress_object_mutations() {
        for kind in [ResourceDescriptor::users(), ResourceDescriptor::groups()] {
            let row = crate::source::project_subject_row(
                "alice@example.com".to_owned(),
                "ClusterRoleBinding",
                "cluster-admin".to_owned(),
            );
            let mut app = app_with_descriptor(kind.clone(), None, row);
            assert!(app.command_names().contains(&kind.gvr()));
            for blocked in [
                KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE),
                KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE),
                KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE),
                KeyEvent::new(KeyCode::Char('J'), KeyModifiers::SHIFT),
                KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL),
                KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL),
            ] {
                assert_eq!(app.handle_key(blocked), AppCommand::None);
            }
            let expected = ResourceTarget {
                kind: kind.clone(),
                namespace: None,
                name: "alice@example.com".to_owned(),
            };
            assert_eq!(
                app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
                AppCommand::Action(ActionIntent::AuthorizationPolicy {
                    target: expected.clone(),
                })
            );
            app.show_policy(
                &expected,
                "Policy(User:alice@example.com)".to_owned(),
                Vec::new(),
            );
            assert!(app.is_policy_view());
        }
    }

    #[test]
    fn custom_jumps_context_rename_and_node_shell_gate_behave_like_k9s() {
        let mut app = app_with_rows();
        let mut config = UserConfig::default();
        config.jumps.insert(
            "v1/pods".to_owned(),
            crate::config::CustomJumpSpec {
                resource: "services".to_owned(),
                namespace: Some("kube-system".to_owned()),
            },
        );
        app.apply_user_config(config);
        let selected_name = app.selected_row().unwrap().name.to_string();
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            AppCommand::Request(ViewState {
                instance: None,
                context: "demo".to_owned(),
                kind: ResourceDescriptor::from(ResourceKind::Services),
                namespace: Some("kube-system".to_owned()),
                field_selector: None,
                label_selector: None,
            })
        );
        assert_eq!(app.query, selected_name);

        let node_row = ResourceRow::new("-", "worker-0", "Ready", "worker", "v1.32.0", 0);
        let mut node_app = app_with_descriptor(ResourceKind::Nodes.into(), None, node_row);
        assert_eq!(
            node_app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE)),
            AppCommand::None
        );
        let mut gate_config = UserConfig::default();
        gate_config.settings.feature_gates.insert(
            "default".to_owned(),
            crate::config::FeatureGates { node_shell: true },
        );
        node_app.apply_user_config(gate_config);
        assert_eq!(
            node_app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE)),
            AppCommand::NodeShell(NodeShellTarget {
                node: "worker-0".to_owned(),
                settings: crate::config::ShellPodSettings::default(),
            })
        );

        node_app.set_context_options(vec![ContextInfo {
            name: "demo".to_owned(),
            cluster: Some("demo-cluster".to_owned()),
            user: Some("demo-user".to_owned()),
            namespace: Some("default".to_owned()),
            kubeconfig_backed: true,
        }]);
        type_input(&mut node_app, ':', "ctx");
        assert_eq!(node_app.overlay, Overlay::Contexts);
        assert_eq!(
            node_app.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE)),
            AppCommand::None
        );
        assert_eq!(node_app.overlay, Overlay::ConfirmDelete);
        node_app.handle_key(KeyEvent::new(KeyCode::Char('-'), KeyModifiers::NONE));
        node_app.handle_key(KeyEvent::new(KeyCode::Char('2'), KeyModifiers::NONE));
        assert_eq!(
            node_app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            AppCommand::Action(ActionIntent::RenameContext {
                old_name: "demo".to_owned(),
                new_name: "demo-2".to_owned(),
            })
        );
        node_app.apply_context_options(
            "demo-2",
            vec![ContextInfo {
                name: "demo-2".to_owned(),
                cluster: Some("demo-cluster".to_owned()),
                user: Some("demo-user".to_owned()),
                namespace: Some("default".to_owned()),
                kubeconfig_backed: true,
            }],
            "Renamed context demo to demo-2",
        );
        assert_eq!(node_app.context, "demo-2");
    }

    #[test]
    fn helm_views_support_values_history_and_rollback() {
        let helm_row = crate::source::kubernetes::resource_views::project_helm_row(
            "default".to_owned(),
            "ingress-nginx".to_owned(),
            "2".to_owned(),
            "deployed".to_owned(),
            "ingress-nginx-4.10.0".to_owned(),
            "1.10.0",
            100,
        );
        let mut app = app_with_descriptor(ResourceDescriptor::helm(), Some("default"), helm_row);
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE)),
            AppCommand::Action(ActionIntent::Content {
                id: 1,
                target: ResourceTarget {
                    kind: ResourceDescriptor::helm(),
                    namespace: Some("default".to_owned()),
                    name: "ingress-nginx".to_owned(),
                },
                content: ContentKind::HelmValues,
            })
        );
        app.show_content(
            "Values(default/ingress-nginx)",
            "replicaCount: 2",
            None,
            Some("replicaCount: 2\nimage:\n  tag: latest".to_owned()),
            None,
            ContentKind::HelmValues,
        );
        assert_eq!(app.active_content_body(), "replicaCount: 2");
        app.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        assert!(app.active_content_body().contains("image:"));
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            AppCommand::Request(ViewState {
                instance: None,
                context: "demo".to_owned(),
                kind: ResourceDescriptor::helm_history(),
                namespace: Some("default".to_owned()),
                field_selector: Some("default/ingress-nginx".to_owned()),
                label_selector: None,
            })
        );

        let hist_row = crate::source::kubernetes::resource_views::project_helm_history_row(
            "default".to_owned(),
            "ingress-nginx",
            "1".to_owned(),
            "superseded".to_owned(),
            "ingress-nginx-4.9.0",
            "1.9.0",
            "Install complete".to_owned(),
            50,
        );
        app.apply_snapshot(ResourceSnapshot {
            generation: app.generation,
            kind: ResourceDescriptor::helm_history(),
            rows: Arc::new(vec![Arc::new(hist_row)]),
            table_columns: crate::source::kubernetes::resource_views::helm_history_columns(),
            state: SourceState::Ready,
            revision: 1,
            emitted_at: Instant::now(),
        });
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            AppCommand::None
        );
        assert_eq!(app.overlay, Overlay::ConfirmDelete);
        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            AppCommand::Action(ActionIntent::HelmRollback {
                namespace: "default".to_owned(),
                release: "ingress-nginx".to_owned(),
                revision: "1".to_owned(),
            })
        );
    }

    #[test]
    fn dir_view_supports_navigation_apply_edit_and_delete() {
        let mut app = App::new(
            "demo",
            Some("default".to_owned()),
            ResourceKind::Pods,
            false,
            vec!["demo".to_owned()],
        );
        let cmd = type_input(&mut app, ':', "dir /tmp/manifests");
        assert_eq!(
            cmd,
            AppCommand::Request(ViewState {
                instance: None,
                context: "demo".to_owned(),
                kind: ResourceDescriptor::dir(),
                namespace: None,
                field_selector: Some("/tmp/manifests".to_owned()),
                label_selector: None,
            })
        );
        let dir_row = crate::source::kubernetes::resource_views::project_dir_row(
            "/tmp/manifests/deploy.yaml".to_owned(),
            "deploy.yaml",
            false,
        );
        app.apply_snapshot(ResourceSnapshot {
            generation: app.generation,
            kind: ResourceDescriptor::dir(),
            rows: Arc::new(vec![Arc::new(dir_row)]),
            table_columns: crate::source::kubernetes::resource_views::dir_columns(),
            state: SourceState::Ready,
            revision: 2,
            emitted_at: Instant::now(),
        });
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE)),
            AppCommand::Action(ActionIntent::DirApply {
                path: "/tmp/manifests/deploy.yaml".to_owned(),
            })
        );
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE)),
            AppCommand::EditDump(std::path::PathBuf::from("/tmp/manifests/deploy.yaml"))
        );
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE)),
            AppCommand::None
        );
        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            AppCommand::Action(ActionIntent::DirDelete {
                path: "/tmp/manifests/deploy.yaml".to_owned(),
            })
        );
    }

    #[test]
    fn benchmark_service_command_validates_type_and_builds_request() {
        let mut app = App::new(
            "demo",
            Some("default".to_owned()),
            ResourceKind::Services,
            false,
            vec!["demo".to_owned()],
        );
        app.user_config.benchmarks.services.insert(
            "default/edge".to_owned(),
            crate::config::BenchConfig {
                concurrency: 4,
                requests: 250,
                http: crate::config::BenchHttp {
                    method: "POST".to_owned(),
                    path: "/v1/bench".to_owned(),
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        let port = vec![crate::model::PortForwardPort {
            container: None,
            name: Arc::from("http"),
            port: 8080,
        }];
        let cluster_ip_svc = ResourceRow::new(
            "default",
            "internal",
            "ClusterIP",
            "1 ports",
            "10.96.0.10",
            0,
        )
        .with_port_forward_ports(port.clone());
        let lb_svc = ResourceRow::new(
            "default",
            "edge",
            "LoadBalancer",
            "1 ports",
            "10.96.0.11",
            0,
        )
        .with_port_forward_ports(port)
        .with_raw_object(serde_json::json!({
            "status": {"loadBalancer": {"ingress": [{"ip": "198.51.100.10"}]}}
        }));
        app.apply_snapshot(ResourceSnapshot {
            generation: app.generation,
            kind: ResourceKind::Services.into(),
            rows: Arc::new(vec![Arc::new(cluster_ip_svc), Arc::new(lb_svc)]),
            table_columns: Arc::from([]),
            state: SourceState::Ready,
            revision: 1,
            emitted_at: Instant::now(),
        });
        assert_eq!(app.selected_row().unwrap().name.as_ref(), "edge");
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE)),
            AppCommand::RunBenchmark(crate::benchmarks::BenchmarkRunRequest {
                cluster: None,
                context: "demo".to_owned(),
                namespace: "default".to_owned(),
                target_name: "edge:8080".to_owned(),
                base_url: "http://198.51.100.10:8080/v1/bench".to_owned(),
                config: app.user_config.bench_config_for_service("default/edge"),
            })
        );
        app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE)),
            AppCommand::None
        );
        assert!(
            app.notice
                .as_deref()
                .unwrap_or_default()
                .contains("reachable service")
        );
    }

    #[test]
    fn benchmark_port_forward_commands_work() {
        let mut app = App::new(
            "demo",
            Some("default".to_owned()),
            ResourceKind::Pods,
            false,
            vec!["demo".to_owned()],
        );
        app.show_port_forwards(vec![PortForwardInfo {
            id: 1,
            context: "demo".to_owned(),
            target: ResourceTarget {
                kind: ResourceKind::Pods.into(),
                namespace: Some("default".to_owned()),
                name: "api-0".to_owned(),
            },
            address: "127.0.0.1".to_owned(),
            local_port: 18080,
            remote_port: 8080,
            state: PortForwardState::Running,
        }]);
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE)),
            AppCommand::RunBenchmark(crate::benchmarks::BenchmarkRunRequest {
                cluster: None,
                context: "demo".to_owned(),
                namespace: "default".to_owned(),
                target_name: "api-0:8080".to_owned(),
                base_url: "http://127.0.0.1:18080/".to_owned(),
                config: crate::config::BenchConfig::default(),
            })
        );
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            AppCommand::Request(ViewState {
                instance: None,
                context: "demo".to_owned(),
                kind: ResourceDescriptor::benchmarks(),
                namespace: None,
                field_selector: Some("default/api-0:8080".to_owned()),
                label_selector: None,
            })
        );
    }

    #[test]
    fn pulses_command_grid_navigation_and_drilldown_work() {
        let mut app = App::new(
            "demo",
            Some("default".to_owned()),
            ResourceKind::Pods,
            false,
            vec!["demo".to_owned()],
        );
        for alias in ["pulses", "pu", "hz"] {
            let cmd = type_input(&mut app, ':', alias);
            assert_eq!(
                cmd,
                AppCommand::Request(ViewState {
                    instance: None,
                    context: "demo".to_owned(),
                    kind: ResourceDescriptor::pulses(),
                    namespace: Some("default".to_owned()),
                    field_selector: None,
                    label_selector: None,
                })
            );
        }
        let specs = crate::source::kubernetes::resource_views::pulse_gauge_specs(false);
        assert_eq!(specs.len(), 12);
        let mut rows = Vec::new();
        for (idx, (kind, title)) in specs.into_iter().enumerate() {
            let gvr = kind.gvr();
            rows.push(Arc::new(
                crate::source::kubernetes::resource_views::project_pulse_row(
                    "default",
                    &gvr,
                    title,
                    kind,
                    false,
                    10,
                    u64::from(idx == 0),
                    0,
                    0,
                    0,
                    0,
                    Arc::from([]),
                ),
            ));
        }
        rows.push(Arc::new(
            crate::source::kubernetes::resource_views::project_pulse_row(
                "default",
                "cpu",
                "Cpu",
                ResourceDescriptor::from(ResourceKind::Pods),
                true,
                0,
                0,
                0,
                0,
                500,
                2_000,
                Arc::from([400, 500]),
            ),
        ));
        app.apply_snapshot(ResourceSnapshot {
            generation: app.generation,
            kind: ResourceDescriptor::pulses(),
            rows: Arc::new(rows),
            table_columns: crate::source::kubernetes::resource_views::pulse_columns(),
            state: SourceState::Ready,
            revision: 1,
            emitted_at: Instant::now(),
        });
        assert_eq!(app.selected, 0);
        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(app.selected, 1);
        app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
        assert_eq!(app.selected, 5);
        app.handle_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE));
        assert_eq!(app.selected, 1);
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            AppCommand::Request(ViewState {
                instance: None,
                context: "demo".to_owned(),
                kind: ResourceDescriptor::from(ResourceKind::Deployments),
                namespace: Some("default".to_owned()),
                field_selector: None,
                label_selector: None,
            })
        );
    }

    #[test]
    fn xray_command_collapse_shell_logs_and_drilldown_work() {
        let mut app = App::new(
            "demo",
            None,
            ResourceKind::Pods,
            false,
            vec!["demo".to_owned()],
        );
        let cmd = type_input(&mut app, ':', "xray deploy prod");
        assert_eq!(
            cmd,
            AppCommand::Request(ViewState {
                instance: None,
                context: "demo".to_owned(),
                kind: ResourceDescriptor::xray(),
                namespace: Some("prod".to_owned()),
                field_selector: Some("apps/v1/deployments".to_owned()),
                label_selector: None,
            })
        );
        let rows =
            crate::source::kubernetes::xray::build_demo_xray_rows(&crate::source::WatchRequest {
                instance: None,
                generation: app.generation,
                context: "demo".to_owned(),
                kind: ResourceDescriptor::xray(),
                namespace: Some("prod".to_owned()),
                field_selector: Some("apps/v1/deployments".to_owned()),
                label_selector: None,
            });
        assert_eq!(rows.len(), 7);
        app.apply_snapshot(ResourceSnapshot {
            generation: app.generation,
            kind: ResourceDescriptor::xray(),
            rows: Arc::new(rows),
            table_columns: crate::source::kubernetes::xray::xray_columns(),
            state: SourceState::Ready,
            revision: 1,
            emitted_at: Instant::now(),
        });
        assert_eq!(app.filtered.len(), 7);
        // Move to deployment node (index 1) and collapse it with `x`
        app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
        assert_eq!(app.selected, 1);
        app.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        assert_eq!(app.filtered.len(), 2);
        // Expand again with `x`
        app.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        assert_eq!(app.filtered.len(), 7);
        // Move to container node (index 3) and test `s` shell and `Enter` logs
        app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
        assert_eq!(app.selected, 3);
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE)),
            AppCommand::Shell(PodShellTarget {
                namespace: "prod".to_owned(),
                pod: "pod-0000000".to_owned(),
                container: "app".to_owned(),
            })
        );
        // Move to configmap leaf (index 4) and press Enter to open instance view
        app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
        assert_eq!(app.selected, 4);
        let enter_cmd = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(
            enter_cmd,
            AppCommand::Request(ViewState {
                instance: Some(ResourceTarget { ref name, .. }),
                ..
            }) if name == "app-config"
        ));
    }

    #[test]
    fn vulnerability_scans_extender_and_scan_table_sorting_work() {
        let mut app = App::new(
            "demo",
            Some("default".to_owned()),
            ResourceKind::Pods,
            false,
            vec!["demo".to_owned()],
        );
        let mut cfg = crate::config::UserConfig::default();
        cfg.settings.image_scans.enable = true;
        cfg.settings.image_scans.exclusions.namespaces = vec!["kube-system".to_owned()];
        app.apply_user_config(cfg);

        let pod_row = ResourceRow::new("default", "api-0", "Running", "1/1", "0", 0)
            .with_raw_object(serde_json::json!({
                "spec": {
                    "containers": [
                        { "name": "api", "image": "nginx:1.25" }
                    ]
                }
            }));
        app.apply_snapshot(ResourceSnapshot {
            generation: app.generation,
            kind: ResourceDescriptor::from(ResourceKind::Pods),
            rows: Arc::new(vec![Arc::new(pod_row)]),
            table_columns: Arc::from([]),
            state: SourceState::Ready,
            revision: 1,
            emitted_at: Instant::now(),
        });

        // Sort by VS (`Shift-V`) on Pod table
        app.handle_key(KeyEvent::new(KeyCode::Char('V'), KeyModifiers::SHIFT));
        assert_eq!(app.sort_field, SortField::VulnScore);

        // Press `v` to open `:scans` for the selected Pod's image
        let cmd = app.handle_key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE));
        assert_eq!(
            cmd,
            AppCommand::Request(ViewState {
                instance: None,
                context: "demo".to_owned(),
                kind: ResourceDescriptor::scans(),
                namespace: None,
                field_selector: Some("nginx:1.25".to_owned()),
                label_selector: None,
            })
        );
        let scan_rows = crate::source::kubernetes::resource_views::read_scan_rows("nginx:1.25");
        assert_eq!(scan_rows.len(), 2);
        app.apply_snapshot(ResourceSnapshot {
            generation: app.generation,
            kind: ResourceDescriptor::scans(),
            rows: Arc::new(scan_rows),
            table_columns: crate::scans::scan_columns(),
            state: SourceState::Ready,
            revision: 1,
            emitted_at: Instant::now(),
        });
        assert_eq!(app.sort_field, SortField::Status);
        app.handle_key(KeyEvent::new(KeyCode::Char('L'), KeyModifiers::SHIFT));
        assert_eq!(app.sort_field, SortField::Details);
        app.handle_key(KeyEvent::new(KeyCode::Char('F'), KeyModifiers::SHIFT));
        assert_eq!(app.sort_field, SortField::Ready);
        app.handle_key(KeyEvent::new(KeyCode::Char('V'), KeyModifiers::SHIFT));
        assert_eq!(app.sort_field, SortField::Name);
        assert!(matches!(
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            AppCommand::OpenUrl(url) if url.starts_with("https://")
        ));
    }

    #[test]
    fn cow_command_and_container_port_forward_work() {
        let mut app = App::new(
            "demo",
            Some("default".to_owned()),
            ResourceKind::Pods,
            false,
            vec!["demo".to_owned()],
        );
        app.user_config.settings.port_forward_address = Some("127.0.0.2".to_owned());
        app.run_command("cow Hello Kux");
        assert_eq!(app.overlay, Overlay::Content);
        assert_eq!(app.content_title, "Cow");
        assert!(app.content_body.contains("< Ruroh? Hello Kux >"));
        assert!(app.content_body.contains("(oo)\\_______"));
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.overlay, Overlay::None);

        let container_row = ResourceRow::new("default", "app", "Running", "true", "nginx:1.25", 10)
            .with_container_data(crate::model::ContainerData {
                image: Arc::from("nginx:1.25"),
                kind: Arc::from("container"),
                restarts: 0,
                ..crate::model::ContainerData::default()
            })
            .with_port_forward_ports(vec![PortForwardPort {
                container: Some(Arc::from("app")),
                name: Arc::from("http"),
                port: 8080,
            }]);
        app.apply_snapshot(ResourceSnapshot {
            generation: app.generation,
            kind: ResourceDescriptor::from(ResourceKind::Pods),
            rows: Arc::new(vec![Arc::new(container_row)]),
            table_columns: Arc::from([]),
            state: SourceState::Ready,
            revision: 1,
            emitted_at: Instant::now(),
        });
        app.list_mode = ListMode::Containers {
            namespace: "default".to_owned(),
            pod: "api-0".to_owned(),
        };
        assert!(app.is_container_view());
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE)),
            AppCommand::ShowPortForwards
        );
        app.handle_key(KeyEvent::new(KeyCode::Char('F'), KeyModifiers::SHIFT));
        assert_eq!(app.overlay, Overlay::PortForwardDialog);
        let dialog = app.port_forward_dialog.as_ref().unwrap();
        assert_eq!(dialog.target.name, "api-0");
        assert_eq!(dialog.remote_port, "app::8080");
        assert_eq!(dialog.local_port, "8080");
        assert_eq!(dialog.address, "127.0.0.2");
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn fast_forward_annotations_node_cordon_and_pf_sort_work() {
        let mut app = App::new(
            "demo",
            Some("default".to_owned()),
            ResourceKind::Pods,
            false,
            vec!["demo".to_owned()],
        );
        let pod_row = ResourceRow::new("default", "api-0", "Running", "1/1", "0", 10)
            .with_port_forward_ports(vec![
                PortForwardPort {
                    container: Some(Arc::from("admin")),
                    name: Arc::from("metrics"),
                    port: 9090,
                },
                PortForwardPort {
                    container: Some(Arc::from("app")),
                    name: Arc::from("http"),
                    port: 8080,
                },
            ])
            .with_raw_object(serde_json::json!({
                "metadata": {
                    "annotations": {
                        "k9scli.io/port-forwards": "app::18080:http"
                    }
                }
            }));
        app.apply_snapshot(ResourceSnapshot {
            generation: app.generation,
            kind: ResourceDescriptor::from(ResourceKind::Pods),
            rows: Arc::new(vec![Arc::new(pod_row)]),
            table_columns: Arc::from([]),
            state: SourceState::Ready,
            revision: 1,
            emitted_at: Instant::now(),
        });
        app.handle_key(KeyEvent::new(KeyCode::Char('F'), KeyModifiers::SHIFT));
        assert_eq!(app.overlay, Overlay::PortForwardDialog);
        let dialog = app.port_forward_dialog.as_ref().unwrap();
        assert_eq!(dialog.remote_port, "app::8080");
        assert_eq!(dialog.local_port, "18080");
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

        // Auto port-forward starts immediately without opening dialog
        let auto_pod = ResourceRow::new("default", "api-1", "Running", "1/1", "0", 10)
            .with_port_forward_ports(vec![PortForwardPort {
                container: Some(Arc::from("app")),
                name: Arc::from("http"),
                port: 8080,
            }])
            .with_raw_object(serde_json::json!({
                "metadata": {
                    "annotations": {
                        "k9scli.io/auto-port-forwards": "app::28080:http"
                    }
                }
            }));
        app.apply_snapshot(ResourceSnapshot {
            generation: app.generation,
            kind: ResourceDescriptor::from(ResourceKind::Pods),
            rows: Arc::new(vec![Arc::new(auto_pod)]),
            table_columns: Arc::from([]),
            state: SourceState::Ready,
            revision: 2,
            emitted_at: Instant::now(),
        });
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Char('F'), KeyModifiers::SHIFT)),
            AppCommand::StartPortForward(PortForwardRequest {
                target: ResourceTarget {
                    kind: ResourceDescriptor::from(ResourceKind::Pods),
                    namespace: Some("default".to_owned()),
                    name: "api-1".to_owned(),
                },
                remote_port: 8080,
                local_port: 28080,
                address: "localhost".to_owned(),
            })
        );

        // PortForward overlay sorting (`Shift-P`, `Shift-U`)
        app.show_port_forwards(vec![
            PortForwardInfo {
                id: 1,
                context: "demo".to_owned(),
                target: ResourceTarget {
                    kind: ResourceDescriptor::from(ResourceKind::Pods),
                    namespace: Some("default".to_owned()),
                    name: "b".to_owned(),
                },
                remote_port: 8080,
                local_port: 9000,
                address: "127.0.0.2".to_owned(),
                state: PortForwardState::Running,
            },
            PortForwardInfo {
                id: 2,
                context: "demo".to_owned(),
                target: ResourceTarget {
                    kind: ResourceDescriptor::from(ResourceKind::Pods),
                    namespace: Some("default".to_owned()),
                    name: "a".to_owned(),
                },
                remote_port: 8080,
                local_port: 8000,
                address: "127.0.0.1".to_owned(),
                state: PortForwardState::Running,
            },
        ]);
        app.handle_key(KeyEvent::new(KeyCode::Char('P'), KeyModifiers::SHIFT));
        assert_eq!(app.port_forwards[0].local_port, 8000);
        app.handle_key(KeyEvent::new(KeyCode::Char('U'), KeyModifiers::SHIFT));
        assert_eq!(app.port_forwards[0].address, "127.0.0.1");
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

        // Node `c` explicit Cordon (single and multi-node batch)
        let node_row = ResourceRow::new("-", "node-1", "Ready", "worker", "v1.32.0", 0)
            .with_node_data(crate::model::NodeData::default());
        let node_row2 = ResourceRow::new("-", "node-2", "Ready", "worker", "v1.32.0", 0)
            .with_node_data(crate::model::NodeData::default());
        let mut node_app = app_with_descriptor(ResourceKind::Nodes.into(), None, node_row.clone());
        node_app.apply_snapshot(ResourceSnapshot {
            generation: node_app.generation,
            kind: ResourceKind::Nodes.into(),
            rows: Arc::new(vec![Arc::new(node_row), Arc::new(node_row2)]),
            table_columns: Arc::from([]),
            state: SourceState::Ready,
            revision: 1,
            emitted_at: Instant::now(),
        });
        node_app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE));
        assert_eq!(
            node_app.pending_delete,
            Some(PendingDelete::NodeScheduling {
                names: vec!["node-1".to_owned()],
                unschedulable: true,
            })
        );
        node_app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        node_app.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE));
        node_app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
        node_app.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE));
        node_app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE));
        assert_eq!(
            node_app.pending_delete,
            Some(PendingDelete::NodeScheduling {
                names: vec!["node-1".to_owned(), "node-2".to_owned()],
                unschedulable: true,
            })
        );
        node_app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(
            node_app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            AppCommand::Action(ActionIntent::SetNodeUnschedulable {
                names: vec!["node-1".to_owned(), "node-2".to_owned()],
                unschedulable: true,
            })
        );
        assert!(node_app.marks.is_empty());

        // `:skin` / `:skins` command and interactive skin selection
        let cmd = app.run_command("skin");
        assert!(matches!(cmd, AppCommand::Request(_)));
        assert!(app.kind.is_skins());
        let skin_rows = crate::source::kubernetes::resource_views::read_skin_rows();
        assert!(skin_rows.iter().any(|r| r.name.as_ref() == "default"));
        app.apply_snapshot(ResourceSnapshot {
            generation: app.generation,
            kind: ResourceDescriptor::skins(),
            rows: Arc::new(skin_rows),
            table_columns: crate::source::kubernetes::resource_views::skin_columns(),
            state: SourceState::Ready,
            revision: 1,
            emitted_at: Instant::now(),
        });
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.user_config.settings.ui.skin.as_deref(), Some("default"));

        // Event fault filtering (`Ctrl-Z`)
        let ev_desc = ResourceDescriptor::new(
            "",
            "v1",
            "v1",
            "Event",
            "events",
            true,
            ResourceOperations::all(),
        );
        let normal_ev = ResourceRow::new("default", "ev-1", "Normal", "Scheduled", "ok", 10);
        let warn_ev = ResourceRow::new("default", "ev-2", "Warning", "Failed", "oom", 10);
        let mut ev_app = app_with_descriptor(ev_desc.clone(), None, normal_ev.clone());
        ev_app.apply_snapshot(ResourceSnapshot {
            generation: ev_app.generation,
            kind: ev_desc,
            rows: Arc::new(vec![Arc::new(normal_ev), Arc::new(warn_ev)]),
            table_columns: Arc::from([]),
            state: SourceState::Ready,
            revision: 1,
            emitted_at: Instant::now(),
        });
        assert_eq!(ev_app.filtered.len(), 2);
        ev_app.handle_key(KeyEvent::new(KeyCode::Char('z'), KeyModifiers::CONTROL));
        assert_eq!(ev_app.filtered.len(), 1);
        assert_eq!(ev_app.selected_row().unwrap().name.as_ref(), "ev-2");

        // Scale dialog single-target replica prefill from `READY`
        let deploy_row = ResourceRow::new("default", "web", "Ready", "3/5", "-", 10);
        let mut scale_app = app_with_descriptor(ResourceKind::Deployments.into(), None, deploy_row);
        scale_app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE));
        assert!(matches!(
            &scale_app.pending_delete,
            Some(PendingDelete::Scale { replicas, .. }) if replicas == "5"
        ));

        // CronJob suspend/resume confirmation state preview
        let cj_desc = descriptor("batch", "v1", "batch/v1", "CronJob", "cronjobs", true);
        let cj_row = ResourceRow::new("default", "nightly", "Active", "-", "-", 10)
            .with_raw_object(serde_json::json!({
                "spec": { "suspend": false }
            }));
        let mut cj_app = app_with_descriptor(cj_desc, None, cj_row);
        cj_app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE));
        assert_eq!(
            cj_app.pending_delete,
            Some(PendingDelete::ToggleCronJobSuspend {
                target: ResourceTarget {
                    kind: descriptor("batch", "v1", "batch/v1", "CronJob", "cronjobs", true),
                    namespace: Some("default".to_owned()),
                    name: "nightly".to_owned(),
                },
                suspend: Some(true),
            })
        );
    }
}
