use std::{
    cmp::Ordering,
    collections::{BTreeMap, BTreeSet, HashSet},
    sync::Arc,
    time::{Duration, Instant},
};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use regex::RegexBuilder;

use crate::kubeconfig::ContextInfo;
use crate::log_view::{LogView, append_display_lines, plain_log_line, without_log_timestamp};
use crate::model::{
    NamespaceInfo, OwnerReferenceData, PortForwardPort, ResourceDescriptor, ResourceKind,
    ResourceOperations, ResourceRow, ResourceSnapshot, SourceState, unix_now,
};
use crate::source::{LogRange, LogRequest, LogTarget};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SortField {
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
}

impl SortField {
    pub const ALL: [Self; 31] = [
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
    ];

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
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
    pub context: String,
    pub kind: ResourceDescriptor,
    pub namespace: Option<String>,
    pub field_selector: Option<String>,
    pub label_selector: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ColumnHitbox {
    pub(crate) x: u16,
    pub(crate) y: u16,
    pub(crate) width: u16,
    pub(crate) field: SortField,
}

impl ColumnHitbox {
    const fn contains(self, column: u16, row: u16) -> bool {
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
    Describe,
    Logs,
    PreviousLogs,
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
    fn new(target: ResourceTarget, ports: Vec<PortForwardPort>) -> Self {
        let remote_port = ports
            .first()
            .map_or_else(String::new, PortForwardPort::selector);
        let local_port = ports
            .first()
            .map_or_else(String::new, |port| port.port.to_string());
        Self {
            target,
            ports,
            remote_port,
            local_port,
            address: "localhost".to_owned(),
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
    Delete {
        targets: Vec<ResourceTarget>,
        immediate: bool,
    },
    DeleteContext {
        name: String,
    },
    Content {
        target: ResourceTarget,
        content: ContentKind,
    },
    ManagedYaml {
        target: ResourceTarget,
    },
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
    SetNodeUnschedulable {
        name: String,
        unschedulable: bool,
    },
    DrainNode {
        name: String,
        options: NodeDrainOptions,
    },
}

impl ActionIntent {
    #[must_use]
    pub(crate) const fn cancelable_read(&self) -> bool {
        matches!(
            self,
            Self::Content { .. }
                | Self::ManagedYaml { .. }
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
    pub name: String,
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
    pub(crate) fn new(name: String) -> Self {
        Self {
            name,
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
    None,
    Quit,
    Request(ViewState),
    Copy(String),
    SaveLogs { name: String, body: String },
    Edit(ResourceTarget),
    Shell(PodShellTarget),
    StartPortForward(PortForwardRequest),
    ShowPortForwards,
    StopPortForward(u64),
    Action(ActionIntent),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PendingDelete {
    Resources(Vec<ResourceTarget>),
    Restart(Vec<ResourceTarget>),
    Context { name: String },
    NodeScheduling { name: String, unschedulable: bool },
    DrainNode(DrainDialog),
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
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum ListMode {
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
        Self(Self::HEADER | Self::LOGO)
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

#[derive(Debug)]
pub struct App {
    pub context: String,
    pub server_version: String,
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
    pub selected: usize,
    pub offset: usize,
    pub viewport_height: usize,
    pub filtered: Vec<usize>,
    pub started_loading_at: Instant,
    pub now_unix: i64,
    pub ui: UiFlags,
    pub marks: HashSet<Arc<str>>,
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
    pub read_only: bool,
    content_max_scroll: u16,
    content_match_cursor: Option<usize>,
    content_target: Option<ResourceTarget>,
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
        let generation = 1;
        let kind = ResourceDescriptor::from(kind);
        let context = context.into();
        let mut context_options = context_options
            .into_iter()
            .map(Into::into)
            .collect::<Vec<_>>();
        if !context_options.iter().any(|item| item.name == context) {
            context_options.push(ContextInfo::synthetic(context.clone()));
        }
        context_options.sort_unstable_by(|left, right| left.name.cmp(&right.name));
        context_options.dedup_by(|left, right| left.name == right.name);
        let initial_view = ViewState {
            context: context.clone(),
            kind: kind.clone(),
            namespace: namespace.clone(),
            field_selector: None,
            label_selector: None,
        };
        let recent_namespaces = namespace.iter().cloned().collect();
        Self {
            context,
            server_version: "detecting…".to_owned(),
            context_options,
            namespace,
            kind: kind.clone(),
            field_selector: None,
            label_selector: None,
            resource_options: ResourceKind::ALL
                .into_iter()
                .map(ResourceDescriptor::from)
                .collect(),
            list_mode: ListMode::Resources,
            generation,
            snapshot: ResourceSnapshot::loading(generation, kind),
            query: String::new(),
            body_query: String::new(),
            input: String::new(),
            input_mode: InputMode::Browse,
            overlay: Overlay::None,
            sort_field: SortField::Name,
            sort_descending: false,
            selected_column: 1,
            selected: 0,
            offset: 0,
            viewport_height: 1,
            filtered: Vec::new(),
            started_loading_at: Instant::now(),
            now_unix: unix_now(),
            ui: UiFlags::default(),
            marks: HashSet::new(),
            mark_anchor: None,
            history: vec![initial_view],
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
            read_only,
            content_max_scroll: 0,
            content_match_cursor: None,
            content_target: None,
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
        }
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
        let selected_key = self.selected_row().map(|row| Arc::clone(&row.key));
        self.snapshot = snapshot;
        self.rebuild_filter(selected_key.as_deref());
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
        if self.context != context {
            return;
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

    pub fn set_resource_options(&mut self, mut resources: Vec<ResourceDescriptor>) {
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
        if resources.is_empty() {
            resources.extend(ResourceKind::ALL.into_iter().map(ResourceDescriptor::from));
        }
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
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return AppCommand::Quit;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('e') {
            self.ui.toggle(UiFlags::HEADER);
            return AppCommand::None;
        }
        if self.input_mode != InputMode::Browse {
            return self.handle_input_key(key);
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
            Overlay::Aliases | Overlay::Namespaces | Overlay::Contexts
        ) {
            let Some(index) = self.chooser_hitbox.index_at(event.column, event.row) else {
                return false;
            };
            self.overlay_selected = index;
            return true;
        }
        if self.overlay != Overlay::None {
            return false;
        }
        if let Some(selected) = self.resource_rows_hitbox.index_at(event.column, event.row) {
            self.select(selected);
            return true;
        }
        let Some(hitbox) = self
            .column_hitboxes
            .iter()
            .copied()
            .find(|hitbox| hitbox.contains(event.column, event.row))
        else {
            return false;
        };
        self.selected_column = SortField::ALL
            .iter()
            .position(|field| *field == hitbox.field)
            .unwrap_or(self.selected_column);
        self.set_sort(hitbox.field);
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

    fn handle_browse_key(&mut self, key: KeyEvent) -> AppCommand {
        match key.code {
            KeyCode::Char('?') => self.open_overlay(Overlay::Help),
            KeyCode::Char(':') => self.start_input(InputMode::Command),
            KeyCode::Char('/') => self.start_input(InputMode::Filter),
            KeyCode::Esc | KeyCode::Char('h') => self.escape_view(),
            KeyCode::Enter => self.open_selected(),
            KeyCode::Left | KeyCode::Right => AppCommand::None,
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
        let known_kind = self.kind.known_kind();
        match key.code {
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
            KeyCode::Char('l') if known_kind == Some(ResourceKind::Pods) => {
                self.pod_log_action(false)
            }
            KeyCode::Char('l') => self.open_selected(),
            KeyCode::Char('p') if known_kind == Some(ResourceKind::Pods) => {
                self.pod_log_action(true)
            }
            KeyCode::Char('o') if known_kind == Some(ResourceKind::Pods) => self.open_pod_node(),
            KeyCode::Char('s') if self.is_container_view() => self.container_shell_action(),
            KeyCode::Char('s') if known_kind == Some(ResourceKind::Pods) => self.pod_shell_action(),
            KeyCode::Char('a') if known_kind == Some(ResourceKind::Pods) => {
                self.unsupported("Attach is not implemented yet")
            }
            KeyCode::Char('e') if self.is_resource_view() => self.edit_action(),
            KeyCode::Char('f')
                if matches!(
                    known_kind,
                    Some(ResourceKind::Pods | ResourceKind::Services)
                ) =>
            {
                AppCommand::ShowPortForwards
            }
            KeyCode::Char('w') if self.namespace.is_none() && self.kind.is_namespaced() => {
                self.warp_to_selected_namespace()
            }
            KeyCode::Char('u') if known_kind == Some(ResourceKind::Nodes) => {
                self.open_node_scheduling_confirmation()
            }
            KeyCode::Char('b') if known_kind == Some(ResourceKind::Services) => {
                self.unsupported("Benchmarking is not implemented yet")
            }
            KeyCode::Char('r') if self.is_restartable_workload() => {
                self.open_workload_restart_confirmation()
            }
            KeyCode::Char('r') if known_kind == Some(ResourceKind::Nodes) => {
                self.open_node_drain_confirmation()
            }
            KeyCode::Char(number) if number.is_ascii_digit() => {
                self.switch_recent_namespace(number)
            }
            _ => AppCommand::None,
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
            KeyCode::Char('d') => self.open_delete_confirmation(),
            KeyCode::Char('k') => self.immediate_delete(),
            KeyCode::Char('z') => {
                self.ui.toggle(UiFlags::FAULTS);
                self.rebuild_filter(None);
                AppCommand::None
            }
            KeyCode::Char('w') => {
                self.ui.toggle(UiFlags::WIDE);
                AppCommand::None
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
            KeyCode::Char('A') => self.set_sort(SortField::Age),
            KeyCode::Char('P') => self.set_sort(SortField::Namespace),
            KeyCode::Char('S') => self.set_sort(SortField::Status),
            KeyCode::Char('F')
                if matches!(
                    known_kind,
                    Some(ResourceKind::Pods | ResourceKind::Services)
                ) =>
            {
                return self.open_port_forward_dialog();
            }
            KeyCode::Char('J') => {
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
            Overlay::None => self.close_overlay_on(key),
        }
    }

    fn open_port_forward_dialog(&mut self) -> AppCommand {
        if self.read_only {
            return self.unsupported("Port forwarding is disabled in read-only mode");
        }
        let Some(target) = self.selected_target() else {
            return AppCommand::None;
        };
        let ports = self
            .selected_row()
            .map_or_else(Vec::new, |row| row.port_forward_ports().to_vec());
        self.port_forward_dialog = Some(PortForwardDialog::new(target, ports));
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
        self.content_target = Some(target.clone());
        AppCommand::Action(ActionIntent::Content {
            target,
            content: ContentKind::Yaml,
        })
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
            KeyCode::Char('c') => visible
                .iter()
                .copied()
                .find(|index| *index == self.overlay_selected)
                .and_then(|index| self.context_options.get(index))
                .map_or(AppCommand::None, |context| {
                    AppCommand::Copy(context.name.clone())
                }),
            KeyCode::Enter | KeyCode::Char('l') => {
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
                        context,
                        kind: self.kind.clone(),
                        namespace: self.namespace.clone(),
                        field_selector: self.field_selector.clone(),
                        label_selector: self.label_selector.clone(),
                    },
                    true,
                )
            }
            _ => self.close_overlay_on(key),
        }
    }

    fn handle_confirm_overlay(&mut self, key: KeyEvent) -> AppCommand {
        if matches!(self.pending_delete, Some(PendingDelete::DrainNode(_))) {
            return self.handle_drain_confirmation(key);
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
                    .map_or(AppCommand::None, |target| match target {
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
                        PendingDelete::Context { name } => {
                            self.overlay = Overlay::Contexts;
                            AppCommand::Action(ActionIntent::DeleteContext { name })
                        }
                        PendingDelete::NodeScheduling {
                            name,
                            unschedulable,
                        } => {
                            self.overlay = Overlay::None;
                            AppCommand::Action(ActionIntent::SetNodeUnschedulable {
                                name,
                                unschedulable,
                            })
                        }
                        PendingDelete::DrainNode(_) => AppCommand::None,
                    })
            }
            KeyCode::Esc | KeyCode::Char('q' | '?') => {
                let return_to_contexts =
                    matches!(self.pending_delete, Some(PendingDelete::Context { .. }));
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
        let name = dialog.name.clone();
        self.pending_delete = None;
        self.confirm_armed = false;
        self.notice = None;
        self.overlay = Overlay::None;
        AppCommand::Action(ActionIntent::DrainNode { name, options })
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
        let request = LogRequest {
            id: self.next_log_id,
            target,
            range: LogRange::Tail,
        };
        let label = if request.target.previous {
            "Previous Logs"
        } else {
            "Logs"
        };
        let container = request
            .target
            .container
            .as_ref()
            .map(|name| format!("/{name}"))
            .unwrap_or_default();
        let title = format!(
            "{label}({}/{}{container})",
            request.target.namespace, request.target.pod
        );
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
        self.log_view = Some(LogView::new(request.clone()));
        self.content_scroll = u16::MAX;
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
        view.clear();
        let request = view.request.clone();
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
            .map(|line| self.log_display_line(line))
            .filter(|line| matcher.matches_text(line))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn handle_log_key(&mut self, key: KeyEvent) -> Option<AppCommand> {
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            return (key.code == KeyCode::Char('s')).then(|| {
                let name = self.log_view.as_ref().map_or_else(
                    || "pod-logs".to_owned(),
                    |view| {
                        format!(
                            "{}-{}",
                            view.request.target.namespace, view.request.target.pod
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
                append_display_lines(&mut self.content_body, &[mark]);
                self.content_scroll = u16::MAX;
                AppCommand::None
            }
            KeyCode::Char('s') => {
                if let Some(view) = &mut self.log_view {
                    view.auto_scroll = !view.auto_scroll;
                    if view.auto_scroll {
                        append_display_lines(&mut self.content_body, &view.take_pending());
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
        lines: Vec<String>,
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
            .map(|line| plain_log_line(&line))
            .collect();
        view.append(&lines);
        view.finished = finished;
        if view.auto_scroll || view.refresh_pending || view.request.range == LogRange::Head {
            append_display_lines(&mut self.content_body, &view.take_pending());
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
        if self.is_logs_view()
            && let Some(command) = self.handle_log_key(key)
        {
            return command;
        }
        if self.handle_body_navigation(&key) {
            return AppCommand::None;
        }
        match key.code {
            KeyCode::Char('e') if self.content_kind == Some(ContentKind::Yaml) => {
                let command = self.edit_content_action();
                if matches!(&command, AppCommand::Edit(_)) {
                    self.overlay = Overlay::None;
                }
                command
            }
            KeyCode::Char('c')
                if matches!(
                    self.content_kind,
                    Some(
                        ContentKind::Yaml
                            | ContentKind::SecretData
                            | ContentKind::Logs
                            | ContentKind::PreviousLogs
                    )
                ) =>
            {
                AppCommand::Copy(self.active_content_body().to_owned())
            }
            KeyCode::Char('m') if self.content_kind == Some(ContentKind::Yaml) => {
                match self.content_managed_fields.as_deref() {
                    Some("") => {
                        let Some(target) = self.content_target.clone() else {
                            return AppCommand::None;
                        };
                        AppCommand::Action(ActionIntent::ManagedYaml { target })
                    }
                    Some(_) => {
                        self.ui.toggle(UiFlags::MANAGED_FIELDS);
                        self.reset_body_scroll();
                        self.scroll_to_first_content_match();
                        AppCommand::None
                    }
                    None => self.unsupported("Selected resource has no managed fields"),
                }
            }
            KeyCode::Char('x')
                if self.content_kind == Some(ContentKind::Yaml)
                    && self.content_decoded_body.is_some() =>
            {
                self.ui.toggle(UiFlags::SECRET_DECODED);
                self.reset_body_scroll();
                self.scroll_to_first_content_match();
                self.notice = Some(
                    if self.ui.secret_decoded() {
                        "Secret values decoded"
                    } else {
                        "Secret values encoded"
                    }
                    .to_owned(),
                );
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
            if self.overlay == Overlay::Content && self.content_kind == Some(ContentKind::Yaml) {
                self.scroll_to_first_content_match();
            }
            if matches!(
                self.overlay,
                Overlay::Aliases | Overlay::Namespaces | Overlay::Contexts
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
        let positions = if self.content_kind == Some(ContentKind::Yaml) {
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
                .map(|line| {
                    if self.is_logs_view() {
                        self.log_display_line(line)
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
        let command = self.input.trim().to_owned();
        self.input.clear();
        self.input_mode = InputMode::Browse;
        self.reset_completion();
        let mut parts = command.split_whitespace();
        let Some(name) = parts.next() else {
            return AppCommand::None;
        };
        match name.to_ascii_lowercase().as_str() {
            "q" | "quit" => AppCommand::Quit,
            "ctx" | "context" | "contexts" => {
                if let Some(context) = parts.next() {
                    self.request_view(
                        ViewState {
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
            alias => {
                let kind = match self.resolve_resource(alias) {
                    Ok(kind) => kind,
                    Err(message) => {
                        self.notice = Some(message);
                        return AppCommand::None;
                    }
                };
                let mut namespace = if kind.is_namespaced() {
                    self.namespace.clone()
                } else {
                    None
                };
                let mut requested_filter = None;
                if let Some(argument) = parts.next() {
                    if let Some(filter) = argument.strip_prefix('/') {
                        requested_filter = Some(filter.to_owned());
                    } else {
                        namespace = (argument != "all").then(|| argument.to_owned());
                    }
                }
                let command = self.request_view(
                    ViewState {
                        context: self.context.clone(),
                        kind,
                        namespace,
                        field_selector: None,
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
        }
    }

    fn current_view(&self) -> ViewState {
        ViewState {
            context: self.context.clone(),
            kind: self.kind.clone(),
            namespace: self.namespace.clone(),
            field_selector: self.field_selector.clone(),
            label_selector: self.label_selector.clone(),
        }
    }

    fn request_view(&mut self, view: ViewState, push_history: bool) -> AppCommand {
        let resource_kind_changed = self.kind != view.kind;
        if push_history && self.current_view() != view {
            self.history.truncate(self.history_cursor + 1);
            self.history.push(view.clone());
            self.history_cursor = self.history.len() - 1;
        }
        if self.context != view.context {
            self.namespace_options.clear();
            self.resource_options = ResourceKind::ALL
                .into_iter()
                .map(ResourceDescriptor::from)
                .collect();
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
        self.namespace.clone_from(&view.namespace);
        self.field_selector.clone_from(&view.field_selector);
        self.label_selector.clone_from(&view.label_selector);
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
            return self
                .command_names()
                .into_iter()
                .filter(|candidate| candidate.starts_with(&prefix))
                .collect();
        };
        let command = &input[..separator];
        let argument = input[separator..].trim_start();
        if argument.contains(char::is_whitespace) {
            return Vec::new();
        }
        let candidates = match command.to_ascii_lowercase().as_str() {
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
            "ctx".to_owned(),
            "context".to_owned(),
            "contexts".to_owned(),
            "ns".to_owned(),
            "namespace".to_owned(),
            "namespaces".to_owned(),
            "pf".to_owned(),
            "portforward".to_owned(),
            "portforwards".to_owned(),
            "q".to_owned(),
            "quit".to_owned(),
        ]);
        for resource in &self.resource_options {
            commands.extend(resource.command_aliases());
        }
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
        self.history_cursor = next;
        self.request_view(self.history[next].clone(), false)
    }

    fn escape_view(&mut self) -> AppCommand {
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
                let searchable = format!(
                    "{} {}",
                    resource.qualified_name(),
                    resource.command_aliases().join(" ")
                );
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
        if self.sort_field == field {
            self.sort_descending = !self.sort_descending;
        } else {
            self.sort_field = field;
            self.sort_descending = false;
        }
        let selected = self.selected_row().map(|row| Arc::clone(&row.key));
        self.rebuild_filter(selected.as_deref());
    }

    fn rebuild_filter(&mut self, selected_key: Option<&str>) {
        let matcher = FilterMatcher::new(&self.query);
        self.filter_error = matcher.error().map(str::to_owned);
        self.filtered.clear();
        self.filtered.extend(
            self.snapshot
                .rows
                .iter()
                .enumerate()
                .filter(|(_, row)| matcher.matches(row))
                .filter(|(_, row)| !self.ui.faults() || row_is_fault(&self.kind, row))
                .map(|(index, _)| index),
        );

        let rows = &self.snapshot.rows;
        let sort_field = self.sort_field;
        let descending = self.sort_descending;
        self.filtered.sort_unstable_by(|left, right| {
            let ordering = compare_rows(&rows[*left], &rows[*right], sort_field);
            if descending {
                ordering.reverse()
            } else {
                ordering
            }
        });

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

    fn selected_target(&self) -> Option<ResourceTarget> {
        if !self.is_resource_view() {
            return None;
        }
        self.selected_row().map(|row| ResourceTarget {
            kind: self.kind.clone(),
            namespace: (row.namespace.as_ref() != "-").then(|| row.namespace.to_string()),
            name: row.name.to_string(),
        })
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
            .map(|row| ResourceTarget {
                kind: self.kind.clone(),
                namespace: (row.namespace.as_ref() != "-").then(|| row.namespace.to_string()),
                name: row.name.to_string(),
            })
            .collect::<Vec<_>>();
        if marked.is_empty() {
            self.selected_target().into_iter().collect()
        } else {
            marked
        }
    }

    fn open_selected(&mut self) -> AppCommand {
        if self.is_container_view() {
            return self.container_log_action(false);
        }
        if self.is_policy_view() {
            return AppCommand::None;
        }
        if self.kind.is_authorization_resource() {
            let Some(target) = self.selected_target() else {
                return AppCommand::None;
            };
            self.pending_policy = Some(target.clone());
            return AppCommand::Action(ActionIntent::AuthorizationPolicy { target });
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

    fn open_child_resource(&mut self) -> Option<AppCommand> {
        let child = self.selected_row().and_then(|row| {
            row.child_resource_data()
                .map(|data| (row.namespace.to_string(), data.clone()))
        })?;
        let namespace = (child.0 != "-").then_some(child.0);
        Some(self.request_view(
            ViewState {
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
        let target = LogTarget {
            namespace: namespace.clone(),
            pod: pod.clone(),
            container: Some(row.name.to_string()),
            previous,
        };
        self.open_logs(target)
    }

    fn pod_log_action(&mut self, previous: bool) -> AppCommand {
        let Some(row) = self.selected_row() else {
            return AppCommand::None;
        };
        let Some(container) = row
            .pod_data()
            .map(|data| data.default_container.to_string())
            .filter(|container| !container.is_empty())
        else {
            return self.unsupported("Pod has no regular containers");
        };
        let namespace = row.namespace.to_string();
        let pod = row.name.to_string();
        self.open_logs(LogTarget {
            namespace,
            pod,
            container: Some(container),
            previous,
        })
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
                context: self.context.clone(),
                kind: ResourceKind::Nodes.into(),
                namespace: None,
                field_selector: Some(format!("metadata.name={node}")),
                label_selector: None,
            },
            true,
        )
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
            selected_key,
        });
        self.list_mode = mode;
        self.sort_field = SortField::Name;
        self.sort_descending = false;
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

    fn open_node_drain_confirmation(&mut self) -> AppCommand {
        if self.read_only {
            return self.unsupported("Drain is disabled in read-only mode");
        }
        let Some(name) = self.selected_row().map(|row| row.name.to_string()) else {
            return AppCommand::None;
        };
        self.pending_delete = Some(PendingDelete::DrainNode(DrainDialog::new(name)));
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
        let Some((name, currently_unschedulable)) = self.selected_row().map(|row| {
            (
                row.name.to_string(),
                row.node_data().is_some_and(|data| data.unschedulable),
            )
        }) else {
            return AppCommand::None;
        };
        self.pending_delete = Some(PendingDelete::NodeScheduling {
            name,
            unschedulable: !currently_unschedulable,
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
        self.selected_target().map_or(AppCommand::None, |target| {
            AppCommand::Action(ActionIntent::Delete {
                targets: vec![target],
                immediate: true,
            })
        })
    }

    fn content_action(&mut self, content: ContentKind) -> AppCommand {
        let Some(target) = self.selected_target() else {
            return AppCommand::None;
        };
        self.content_target = Some(target.clone());
        AppCommand::Action(ActionIntent::Content { target, content })
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

    pub fn show_managed_yaml(
        &mut self,
        target: &ResourceTarget,
        body: String,
        decoded_body: Option<String>,
    ) {
        if self.overlay != Overlay::Content
            || self.content_kind != Some(ContentKind::Yaml)
            || self.content_target.as_ref() != Some(target)
        {
            return;
        }
        self.content_managed_fields = Some(body);
        self.content_decoded_managed_fields = decoded_body;
        if !self.ui.managed_fields() {
            self.ui.toggle(UiFlags::MANAGED_FIELDS);
        }
        self.reset_body_scroll();
        self.scroll_to_first_content_match();
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
        self.port_forwards = forwards;
        self.overlay_selected = 0;
        self.notice = None;
        self.overlay = Overlay::PortForwards;
    }

    pub fn refresh_port_forwards(&mut self, forwards: Vec<PortForwardInfo>) {
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

    fn matches(&self, row: &ResourceRow) -> bool {
        match self {
            Self::All => true,
            Self::Regex(regex) => regex.is_match(&row.search),
            Self::Inverse(regex) => !regex.is_match(&row.search),
            Self::Fuzzy(needle) => fuzzy_match(&row.search, needle),
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
    if row.policy_data().is_some() {
        return false;
    }
    if kind.is_api_service() {
        return row.status.as_ref() != "True";
    }
    match kind.known_kind() {
        Some(ResourceKind::Services) => false,
        Some(ResourceKind::Nodes) => row.status.as_ref() != "Ready",
        Some(ResourceKind::Pods) => !matches!(row.status.as_ref(), "Running" | "Succeeded"),
        Some(ResourceKind::Deployments | ResourceKind::StatefulSets | ResourceKind::DaemonSets) => {
            row.status.as_ref() != "Ready"
        }
        None => !matches!(row.status.as_ref(), "-" | "Ready" | "Running" | "Active"),
    }
}

fn compare_rows(left: &ResourceRow, right: &ResourceRow, field: SortField) -> Ordering {
    let primary = match field {
        SortField::Namespace => left.namespace.cmp(&right.namespace),
        SortField::Name => left.name.cmp(&right.name),
        SortField::Ready | SortField::Binding => left.ready.cmp(&right.ready),
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
        assert!(app.input.is_empty());
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
        assert!(list.query.is_empty());

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
        assert_eq!(
            list.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE)),
            AppCommand::None
        );
        assert_eq!(list.overlay, Overlay::None);
        assert_eq!(
            list.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE)),
            AppCommand::None
        );
        assert_eq!(list.query, "api", "Left does not clear or leave the view");

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
        assert!(FilterMatcher::new("!api").match_ranges("worker").is_empty());
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
    fn pod_log_hotkeys_target_the_resolved_container_for_current_and_previous_logs() {
        let mut pods = app_with_rows();
        assert!(matches!(
            pods.handle_key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE)),
            AppCommand::Action(ActionIntent::StartLogs {
                request: LogRequest { target: LogTarget { ref container, previous: false, .. }, .. },
            }) if container.as_deref() == Some("api")
        ));
        pods.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(matches!(
            pods.handle_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE)),
            AppCommand::Action(ActionIntent::StartLogs {
                request: LogRequest { target: LogTarget { ref container, previous: true, .. }, .. },
            }) if container.as_deref() == Some("api")
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
        assert!(app.namespace_options.is_empty());
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
        assert!(app.input.is_empty());
        assert_eq!(app.query, "api");
        assert_eq!(app.previous_filter_hint(), Some("api"));

        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(app.input, "api");
        assert_eq!(app.query, "api");

        app.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert!(app.input.is_empty());
        assert!(app.query.is_empty());
        assert_eq!(app.previous_filter_hint(), Some("api"));
    }

    #[test]
    fn resource_command_accepts_alias_and_namespace() {
        let mut app = app_with_rows();
        let command = type_input(&mut app, ':', "deploy kube-system");
        assert_eq!(
            command,
            AppCommand::Request(ViewState {
                context: "demo".to_owned(),
                kind: ResourceKind::Deployments.into(),
                namespace: Some("kube-system".to_owned()),
                field_selector: None,
                label_selector: None,
            })
        );
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
        assert!(app.query.is_empty());

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
        assert!(app.query.is_empty());
    }

    #[test]
    fn enter_on_a_parent_resource_opens_its_label_selected_child_view() {
        let deployment = ResourceDescriptor::from(ResourceKind::Deployments);
        let replica_set = descriptor("apps", "v1", "apps/v1", "ReplicaSet", "replicasets", true);
        let row = ResourceRow::new("prod", "api", "Ready", "3/3", "3 available", 0)
            .with_child_resource_data(crate::model::ChildResourceData {
                kind: replica_set.clone(),
                label_selector: Arc::from("app=api,tier=backend"),
            });
        let mut app = app_with_descriptor(deployment, Some("prod"), row);
        app.query = "stale".to_owned();

        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            AppCommand::Request(ViewState {
                context: "demo".to_owned(),
                kind: replica_set,
                namespace: Some("prod".to_owned()),
                field_selector: None,
                label_selector: Some("app=api,tier=backend".to_owned()),
            })
        );
        assert!(app.query.is_empty());
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
                    name: "worker-0".to_owned(),
                    unschedulable: requested_unschedulable,
                })
            );
            app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
            assert_eq!(
                app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
                AppCommand::Action(ActionIntent::SetNodeUnschedulable {
                    name: "worker-0".to_owned(),
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
            Some(PendingDelete::DrainNode(DrainDialog::new(
                "worker-0".to_owned()
            )))
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
                name: "worker-0".to_owned(),
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
                name: "worker-0".to_owned(),
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
        assert!(app.marks.is_empty());
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
    fn clicking_a_column_header_sorts_and_reverses_direction() {
        let mut app = app_with_rows();
        app.set_column_hitboxes(vec![ColumnHitbox {
            x: 40,
            y: 7,
            width: 10,
            field: SortField::Age,
        }]);
        let click = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 44,
            row: 7,
            modifiers: KeyModifiers::NONE,
        };

        assert!(app.handle_mouse(click));
        assert_eq!(app.sort_field, SortField::Age);
        assert!(!app.sort_descending);
        assert_eq!(
            SortField::ALL[app.selected_column],
            SortField::Age,
            "mouse sorting should also select the clicked column"
        );

        assert!(app.handle_mouse(click));
        assert!(app.sort_descending);
        assert!(!app.handle_mouse(MouseEvent { row: 8, ..click }));
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
        assert!(app.query.is_empty());
        assert_eq!(app.visible_namespace_indices(), vec![1]);
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            AppCommand::Request(ViewState {
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
        assert_eq!(app.overlay, Overlay::None);
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
        assert!(app.visible_resource_indices().is_empty());
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
        assert!(app.body_query.is_empty());

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
        app.pod_log_action(false);
        let initial_id = app.log_view.as_ref().unwrap().request.id;
        app.append_log_batch(initial_id, vec!["initial".to_owned()], false, None);
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
            app.append_log_batch(initial_id, vec!["stale".to_owned()], false, None);
            assert!(app.content_body.is_empty());
        }
        let id = app.log_view.as_ref().unwrap().request.id;
        app.append_log_batch(id, vec!["refreshed".to_owned()], false, None);
        assert_eq!(
            app.content_body, "refreshed\n",
            "paused ranges get one refresh"
        );
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        app.append_log_batch(id, vec!["late".to_owned()], false, None);
        assert_eq!(app.overlay, Overlay::None);
        assert!(!app.content_body.contains("late"));
    }

    #[test]
    fn log_pause_mark_resume_clear_and_column_lock_preserve_stream_state() {
        let mut app = app_with_rows();
        app.pod_log_action(false);
        let id = app.log_view.as_ref().unwrap().request.id;
        app.append_log_batch(id, vec!["one".to_owned()], false, None);
        app.content_horizontal_scroll = 12;
        app.handle_key(KeyEvent::new(KeyCode::Char('L'), KeyModifiers::SHIFT));
        app.append_log_batch(id, vec!["two".to_owned()], false, None);
        assert_eq!(app.content_horizontal_scroll, 12);
        app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Char('m'), KeyModifiers::NONE));
        assert!(!app.log_view.as_ref().unwrap().auto_scroll);
        app.append_log_batch(id, vec!["three".to_owned()], false, None);
        assert!(!app.content_body.contains("three"));
        assert!(app.content_body.contains("────"));
        app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE));
        assert!(app.content_body.contains("three"));
        assert!(app.content_body.contains("────"));
        app.handle_key(KeyEvent::new(KeyCode::Char('L'), KeyModifiers::SHIFT));
        app.show_transient_notice("Logs saved to example.log");
        app.append_log_batch(id, vec!["four".to_owned()], false, None);
        assert_eq!(app.content_horizontal_scroll, 0);
        assert_eq!(app.notice.as_deref(), Some("Logs saved to example.log"));
        app.handle_key(KeyEvent::new(KeyCode::Char('C'), KeyModifiers::SHIFT));
        assert!(app.content_body.is_empty());
        assert_eq!(app.log_view.as_ref().unwrap().request.id, id);
        app.append_log_batch(id, vec!["after clear".to_owned()], false, None);
        assert_eq!(app.content_body, "after clear\n");
    }

    #[test]
    fn log_save_exports_filtered_display_and_escape_clears_filter_before_closing() {
        let mut app = app_with_rows();
        app.pod_log_action(false);
        let id = app.log_view.as_ref().unwrap().request.id;
        app.append_log_batch(
            id,
            vec![
                "2026-09-07T00:00:00Z \x1b[31mERROR example\x1b[0m".to_owned(),
                "2026-09-07T00:00:01Z INFO running".to_owned(),
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
            vec!["2026-09-07T00:00:02Z ERROR second".to_owned()],
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
        assert!(app.body_query.is_empty());
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
    fn yaml_managed_fields_load_lazily_and_ignore_stale_results() {
        let mut app = app_with_rows();
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

        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Char('m'), KeyModifiers::NONE)),
            AppCommand::Action(ActionIntent::ManagedYaml {
                target: target.clone()
            })
        );
        assert!(!app.ui.managed_fields());

        let mut stale = target.clone();
        stale.name.push_str("-stale");
        app.show_managed_yaml(&stale, "stale".to_owned(), None);
        assert!(!app.ui.managed_fields());

        app.show_managed_yaml(
            &target,
            "metadata:\n  managedFields:\n  - manager: kubelet".to_owned(),
            None,
        );
        assert!(app.ui.managed_fields());
        assert!(app.active_content_body().contains("managedFields"));
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
                    target: LogTarget {
                        namespace: "prod".to_owned(),
                        pod: "api-0".to_owned(),
                        container: Some("api".to_owned()),
                        previous: false,
                    },
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
        assert!(app.port_forwards.is_empty());
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
}
