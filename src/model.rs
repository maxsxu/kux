use std::{
    fmt,
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use clap::ValueEnum;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, ValueEnum)]
pub enum ResourceKind {
    #[default]
    Pods,
    Deployments,
    StatefulSets,
    DaemonSets,
    Services,
    Nodes,
}

impl ResourceKind {
    pub const ALL: [Self; 6] = [
        Self::Pods,
        Self::Deployments,
        Self::StatefulSets,
        Self::DaemonSets,
        Self::Services,
        Self::Nodes,
    ];

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Pods => "Pods",
            Self::Deployments => "Deployments",
            Self::StatefulSets => "StatefulSets",
            Self::DaemonSets => "DaemonSets",
            Self::Services => "Services",
            Self::Nodes => "Nodes",
        }
    }

    #[must_use]
    pub const fn plural_name(self) -> &'static str {
        match self {
            Self::Pods => "pods",
            Self::Deployments => "deployments",
            Self::StatefulSets => "statefulsets",
            Self::DaemonSets => "daemonsets",
            Self::Services => "services",
            Self::Nodes => "nodes",
        }
    }

    #[must_use]
    pub const fn short_label(self) -> &'static str {
        match self {
            Self::Pods => "po",
            Self::Deployments => "deploy",
            Self::StatefulSets => "sts",
            Self::DaemonSets => "ds",
            Self::Services => "svc",
            Self::Nodes => "no",
        }
    }

    #[must_use]
    pub const fn index(self) -> usize {
        match self {
            Self::Pods => 0,
            Self::Deployments => 1,
            Self::StatefulSets => 2,
            Self::DaemonSets => 3,
            Self::Services => 4,
            Self::Nodes => 5,
        }
    }

    #[must_use]
    pub fn from_alias(alias: &str) -> Option<Self> {
        match alias.to_ascii_lowercase().as_str() {
            "pod" | "pods" | "po" => Some(Self::Pods),
            "deployment" | "deployments" | "deploy" | "dp" => Some(Self::Deployments),
            "statefulset" | "statefulsets" | "sts" => Some(Self::StatefulSets),
            "daemonset" | "daemonsets" | "ds" => Some(Self::DaemonSets),
            "service" | "services" | "svc" => Some(Self::Services),
            "node" | "nodes" | "no" => Some(Self::Nodes),
            _ => None,
        }
    }

    #[must_use]
    pub const fn aliases(self) -> &'static str {
        match self {
            Self::Pods => "pod pods po",
            Self::Deployments => "deployment deployments deploy dp",
            Self::StatefulSets => "statefulset statefulsets sts",
            Self::DaemonSets => "daemonset daemonsets ds",
            Self::Services => "service services svc",
            Self::Nodes => "node nodes no",
        }
    }

    #[must_use]
    pub const fn is_namespaced(self) -> bool {
        !matches!(self, Self::Nodes)
    }
}

impl fmt::Display for ResourceKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// Runtime identity and capabilities for a resource served by the API server.
///
/// `ResourceKind` remains the small, CLI-facing set of built-in views. This
/// descriptor carries the discovered group/version/resource information needed
/// to address built-in resources, CRDs, and aggregated APIs uniformly.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ResourceDescriptor {
    pub group: Arc<str>,
    pub version: Arc<str>,
    pub api_version: Arc<str>,
    pub kind: Arc<str>,
    pub plural: Arc<str>,
    pub short_names: Arc<[Arc<str>]>,
    pub namespaced: bool,
    pub operations: ResourceOperations,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct ResourceOperations(u8);

impl ResourceOperations {
    const WATCH: u8 = 1 << 0;
    const GET: u8 = 1 << 1;
    const DELETE: u8 = 1 << 2;

    #[must_use]
    pub const fn new(watchable: bool, gettable: bool, deletable: bool) -> Self {
        Self(
            ((watchable as u8) * Self::WATCH)
                | ((gettable as u8) * Self::GET)
                | ((deletable as u8) * Self::DELETE),
        )
    }

    #[must_use]
    pub const fn all() -> Self {
        Self(Self::WATCH | Self::GET | Self::DELETE)
    }

    #[must_use]
    pub const fn watchable(self) -> bool {
        self.0 & Self::WATCH != 0
    }

    #[must_use]
    pub const fn gettable(self) -> bool {
        self.0 & Self::GET != 0
    }

    #[must_use]
    pub const fn deletable(self) -> bool {
        self.0 & Self::DELETE != 0
    }
}

impl ResourceDescriptor {
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        group: impl Into<Arc<str>>,
        version: impl Into<Arc<str>>,
        api_version: impl Into<Arc<str>>,
        kind: impl Into<Arc<str>>,
        plural: impl Into<Arc<str>>,
        namespaced: bool,
        operations: ResourceOperations,
    ) -> Self {
        Self {
            group: group.into(),
            version: version.into(),
            api_version: api_version.into(),
            kind: kind.into(),
            plural: plural.into(),
            short_names: Arc::from([]),
            namespaced,
            operations,
        }
    }

    #[must_use]
    pub fn with_short_names<I, S>(mut self, short_names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<Arc<str>>,
    {
        let mut names = Vec::new();
        for name in short_names {
            let name = name.into();
            if !name.is_empty() && !names.contains(&name) {
                names.push(name);
            }
        }
        self.short_names = names.into();
        self
    }

    /// Returns the specialized built-in kind only for its exact Kubernetes
    /// group, version, and kind tuple.
    #[must_use]
    pub fn known_kind(&self) -> Option<ResourceKind> {
        match (
            self.group.as_ref(),
            self.version.as_ref(),
            self.kind.as_ref(),
        ) {
            ("", "v1", "Pod") => Some(ResourceKind::Pods),
            ("apps", "v1", "Deployment") => Some(ResourceKind::Deployments),
            ("apps", "v1", "StatefulSet") => Some(ResourceKind::StatefulSets),
            ("apps", "v1", "DaemonSet") => Some(ResourceKind::DaemonSets),
            ("", "v1", "Service") => Some(ResourceKind::Services),
            ("", "v1", "Node") => Some(ResourceKind::Nodes),
            _ => None,
        }
    }

    #[must_use]
    pub fn label(&self) -> &str {
        match self.known_kind() {
            Some(kind) => kind.label(),
            None => self.kind.as_ref(),
        }
    }

    #[must_use]
    pub fn plural_name(&self) -> &str {
        self.plural.as_ref()
    }

    /// Returns the kubectl-compatible resource name, qualifying non-core
    /// resources with their API group to avoid ambiguous plurals.
    #[must_use]
    pub fn qualified_name(&self) -> String {
        if self.group.is_empty() {
            self.plural.to_string()
        } else {
            format!("{}.{}", self.plural, self.group)
        }
    }

    #[must_use]
    pub const fn is_namespaced(&self) -> bool {
        self.namespaced
    }

    #[must_use]
    pub fn short_label(&self) -> &str {
        match self.known_kind() {
            Some(kind) => kind.short_label(),
            None => self.plural.as_ref(),
        }
    }

    /// Names accepted by command mode for this resource. Built-ins retain
    /// their K9s-style aliases, while discovered resources include
    /// server-provided short names, lowercase kind, plural, and
    /// group-qualified plural.
    #[must_use]
    pub fn command_aliases(&self) -> Vec<String> {
        let mut aliases: Vec<_> = self.short_names.iter().map(ToString::to_string).collect();
        if let Some(kind) = self.known_kind() {
            for alias in kind.aliases().split_whitespace() {
                if !aliases.iter().any(|candidate| candidate == alias) {
                    aliases.push(alias.to_owned());
                }
            }
        } else {
            for alias in [self.kind.to_ascii_lowercase(), self.plural.to_string()] {
                if !aliases.contains(&alias) {
                    aliases.push(alias);
                }
            }
        }
        let qualified = self.qualified_name();
        if !aliases.contains(&qualified) {
            aliases.push(qualified);
        }
        if self.is_custom_resource_definition() {
            for alias in ["crd", "crds"] {
                if !aliases.iter().any(|candidate| candidate == alias) {
                    aliases.push(alias.to_owned());
                }
            }
        }
        aliases
    }

    #[must_use]
    pub fn is_core_v1_secret(&self) -> bool {
        self.group.is_empty()
            && self.version.as_ref() == "v1"
            && self.kind.as_ref() == "Secret"
            && self.plural.as_ref() == "secrets"
    }

    #[must_use]
    pub fn is_api_service(&self) -> bool {
        self.group.as_ref() == "apiregistration.k8s.io"
            && self.kind.as_ref() == "APIService"
            && self.plural.as_ref() == "apiservices"
    }

    #[must_use]
    pub fn is_apps_v1_replica_set(&self) -> bool {
        self.group.as_ref() == "apps"
            && self.version.as_ref() == "v1"
            && self.kind.as_ref() == "ReplicaSet"
            && self.plural.as_ref() == "replicasets"
    }

    #[must_use]
    pub fn is_custom_resource_definition(&self) -> bool {
        self.group.as_ref() == "apiextensions.k8s.io"
            && self.kind.as_ref() == "CustomResourceDefinition"
            && self.plural.as_ref() == "customresourcedefinitions"
    }

    #[must_use]
    pub fn is_authorization_resource(&self) -> bool {
        (self.group.is_empty() && self.kind.as_ref() == "ServiceAccount")
            || (self.group.as_ref() == "rbac.authorization.k8s.io"
                && matches!(
                    self.kind.as_ref(),
                    "Role" | "ClusterRole" | "RoleBinding" | "ClusterRoleBinding"
                ))
    }
}

impl From<ResourceKind> for ResourceDescriptor {
    fn from(kind: ResourceKind) -> Self {
        let (group, version, api_version, resource_kind, plural) = match kind {
            ResourceKind::Pods => ("", "v1", "v1", "Pod", "pods"),
            ResourceKind::Deployments => ("apps", "v1", "apps/v1", "Deployment", "deployments"),
            ResourceKind::StatefulSets => ("apps", "v1", "apps/v1", "StatefulSet", "statefulsets"),
            ResourceKind::DaemonSets => ("apps", "v1", "apps/v1", "DaemonSet", "daemonsets"),
            ResourceKind::Services => ("", "v1", "v1", "Service", "services"),
            ResourceKind::Nodes => ("", "v1", "v1", "Node", "nodes"),
        };
        Self::new(
            group,
            version,
            api_version,
            resource_kind,
            plural,
            kind.is_namespaced(),
            ResourceOperations::all(),
        )
    }
}

impl fmt::Display for ResourceDescriptor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceRow {
    pub key: Arc<str>,
    pub namespace: Arc<str>,
    pub name: Arc<str>,
    pub status: Arc<str>,
    pub ready: Arc<str>,
    pub details: Arc<str>,
    pub labels: Arc<str>,
    pub created_unix: i64,
    pub search: Arc<str>,
    pub data: ResourceData,
    pub table_cells: Arc<[TableCell]>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableColumn {
    pub name: Arc<str>,
    pub format: Arc<str>,
    pub priority: i32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableCell {
    pub text: Arc<str>,
    pub timestamp_unix: Option<i64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum ResourceData {
    #[default]
    Generic,
    ChildResource(ChildResourceData),
    Pod(PodData),
    Node(NodeData),
    Container(ContainerData),
    Policy(PolicyData),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChildResourceData {
    pub kind: ResourceDescriptor,
    pub label_selector: Arc<str>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PodData {
    pub restarts: u32,
    pub node: Arc<str>,
    pub cpu_millis: Option<u64>,
    pub cpu_request_millis: u64,
    pub cpu_limit_millis: u64,
    pub memory_bytes: Option<u64>,
    pub memory_request_bytes: u64,
    pub memory_limit_bytes: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NodeData {
    pub taints: Arc<str>,
    pub taint_count: u32,
    pub version: Arc<str>,
    pub pods: u32,
    pub cpu_millis: Option<u64>,
    pub cpu_allocatable_millis: u64,
    pub memory_bytes: Option<u64>,
    pub memory_allocatable_bytes: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContainerData {
    pub image: Arc<str>,
    pub kind: Arc<str>,
    pub restarts: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PolicyData {
    pub verbs: Vec<Arc<str>>,
}

impl PolicyData {
    #[must_use]
    pub fn allows(&self, verb: &str) -> bool {
        self.verbs
            .iter()
            .any(|candidate| candidate.as_ref() == "*" || candidate.as_ref() == verb)
    }
}

impl ResourceRow {
    #[must_use]
    pub fn new(
        namespace: impl Into<String>,
        name: impl Into<String>,
        status: impl Into<String>,
        ready: impl Into<String>,
        details: impl Into<String>,
        created_unix: i64,
    ) -> Self {
        let namespace = namespace.into();
        let name = name.into();
        let status = status.into();
        let ready = ready.into();
        let details = details.into();
        let key = format!("{namespace}/{name}");
        let search = format!("{namespace}\n{name}\n{status}\n{ready}\n{details}").to_lowercase();
        Self {
            key: Arc::from(key),
            namespace: Arc::from(namespace),
            name: Arc::from(name),
            status: Arc::from(status),
            ready: Arc::from(ready),
            details: Arc::from(details),
            labels: Arc::from(""),
            created_unix,
            search: Arc::from(search),
            data: ResourceData::Generic,
            table_cells: Arc::from([]),
        }
    }

    #[must_use]
    pub fn with_labels(mut self, labels: impl Into<String>) -> Self {
        let labels = labels.into();
        self.search = Arc::from(format!("{}\n{labels}", self.search).to_lowercase());
        self.labels = Arc::from(labels);
        self
    }

    #[must_use]
    pub fn with_pod_data(mut self, data: PodData) -> Self {
        self.search = Arc::from(format!("{}\n{}", self.search, data.node).to_lowercase());
        self.data = ResourceData::Pod(data);
        self
    }

    #[must_use]
    pub fn with_child_resource_data(mut self, data: ChildResourceData) -> Self {
        self.data = ResourceData::ChildResource(data);
        self
    }

    #[must_use]
    pub fn with_node_data(mut self, data: NodeData) -> Self {
        self.search =
            Arc::from(format!("{}\n{}\n{}", self.search, data.taints, data.version).to_lowercase());
        self.data = ResourceData::Node(data);
        self
    }

    #[must_use]
    pub fn with_container_data(mut self, data: ContainerData) -> Self {
        self.search =
            Arc::from(format!("{}\n{}\n{}", self.search, data.image, data.kind).to_lowercase());
        self.data = ResourceData::Container(data);
        self
    }

    #[must_use]
    pub fn with_policy_data(mut self, data: PolicyData) -> Self {
        let verbs = data
            .verbs
            .iter()
            .map(AsRef::as_ref)
            .collect::<Vec<_>>()
            .join(",");
        self.search = Arc::from(format!("{}\n{verbs}", self.search).to_lowercase());
        self.data = ResourceData::Policy(data);
        self
    }

    #[must_use]
    pub fn with_table_cells(mut self, cells: Vec<TableCell>) -> Self {
        if !cells.is_empty() {
            let cell_text = cells
                .iter()
                .map(|cell| cell.text.as_ref())
                .collect::<Vec<_>>()
                .join("\n");
            self.search = Arc::from(format!("{}\n{cell_text}", self.search).to_lowercase());
        }
        self.table_cells = cells.into();
        self
    }

    #[must_use]
    pub const fn pod_data(&self) -> Option<&PodData> {
        if let ResourceData::Pod(data) = &self.data {
            Some(data)
        } else {
            None
        }
    }

    #[must_use]
    pub const fn child_resource_data(&self) -> Option<&ChildResourceData> {
        if let ResourceData::ChildResource(data) = &self.data {
            Some(data)
        } else {
            None
        }
    }

    #[must_use]
    pub const fn node_data(&self) -> Option<&NodeData> {
        if let ResourceData::Node(data) = &self.data {
            Some(data)
        } else {
            None
        }
    }

    #[must_use]
    pub const fn container_data(&self) -> Option<&ContainerData> {
        if let ResourceData::Container(data) = &self.data {
            Some(data)
        } else {
            None
        }
    }

    #[must_use]
    pub const fn policy_data(&self) -> Option<&PolicyData> {
        if let ResourceData::Policy(data) = &self.data {
            Some(data)
        } else {
            None
        }
    }

    #[must_use]
    pub fn matches(&self, needle: &str) -> bool {
        needle.is_empty() || self.search.contains(needle)
    }

    #[must_use]
    pub fn age_seconds(&self, now_unix: i64) -> u64 {
        u64::try_from(now_unix.saturating_sub(self.created_unix).max(0)).unwrap_or(0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceState {
    Loading,
    Ready,
    Error(Arc<str>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamespaceInfo {
    pub name: String,
    pub status: String,
    pub created_at_unix: Option<i64>,
    pub synthetic: bool,
}

impl NamespaceInfo {
    #[must_use]
    pub fn all() -> Self {
        Self {
            name: "<all>".to_owned(),
            status: "-".to_owned(),
            created_at_unix: None,
            synthetic: true,
        }
    }

    #[must_use]
    pub fn discovered(
        name: impl Into<String>,
        status: impl Into<String>,
        created_at_unix: Option<i64>,
    ) -> Self {
        Self {
            name: name.into(),
            status: status.into(),
            created_at_unix,
            synthetic: false,
        }
    }

    #[must_use]
    pub fn unknown(name: impl Into<String>) -> Self {
        Self::discovered(name, "Unknown", None)
    }

    #[must_use]
    pub fn age_seconds(&self, now_unix: i64) -> Option<u64> {
        self.created_at_unix
            .and_then(|created| u64::try_from(now_unix.saturating_sub(created).max(0)).ok())
    }

    #[must_use]
    pub fn search_text(&self) -> String {
        format!("{} {}", self.name, self.status)
    }
}

impl From<String> for NamespaceInfo {
    fn from(name: String) -> Self {
        Self::unknown(name)
    }
}

#[derive(Debug, Clone)]
pub struct ResourceSnapshot {
    pub generation: u64,
    pub kind: ResourceDescriptor,
    pub rows: Arc<Vec<Arc<ResourceRow>>>,
    pub table_columns: Arc<[TableColumn]>,
    pub state: SourceState,
    pub revision: u64,
    pub emitted_at: Instant,
}

impl ResourceSnapshot {
    #[must_use]
    pub fn loading(generation: u64, kind: impl Into<ResourceDescriptor>) -> Self {
        Self {
            generation,
            kind: kind.into(),
            rows: Arc::new(Vec::new()),
            table_columns: Arc::from([]),
            state: SourceState::Loading,
            revision: 0,
            emitted_at: Instant::now(),
        }
    }
}

#[must_use]
pub fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs()
        .try_into()
        .unwrap_or(i64::MAX)
}

#[must_use]
pub fn format_age(total_seconds: u64) -> String {
    const MINUTE: u64 = 60;
    const HOUR: u64 = 60 * MINUTE;
    const DAY: u64 = 24 * HOUR;
    const YEAR: u64 = 365 * DAY;

    match total_seconds {
        seconds if seconds < MINUTE => format!("{seconds}s"),
        seconds if seconds < HOUR => format!("{}m", seconds / MINUTE),
        seconds if seconds < DAY => format!("{}h", seconds / HOUR),
        seconds if seconds < YEAR => format!("{}d", seconds / DAY),
        seconds => format!("{}y", seconds / YEAR),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_compact_ages() {
        assert_eq!(format_age(42), "42s");
        assert_eq!(format_age(120), "2m");
        assert_eq!(format_age(7_200), "2h");
        assert_eq!(format_age(172_800), "2d");
    }

    #[test]
    fn row_search_is_case_insensitive_when_query_is_normalized() {
        let row = ResourceRow::new("Prod", "API-0", "Running", "1/1", "0", 0);
        assert!(row.matches("api"));
        assert!(row.matches("prod"));
        assert!(!row.matches("worker"));
    }

    #[test]
    fn builtin_descriptors_preserve_cli_kinds_and_capabilities() {
        let pods = ResourceDescriptor::from(ResourceKind::Pods);
        assert_eq!(pods.known_kind(), Some(ResourceKind::Pods));
        assert_eq!(pods.api_version.as_ref(), "v1");
        assert_eq!(pods.plural_name(), "pods");
        assert_eq!(pods.qualified_name(), "pods");
        assert!(pods.is_namespaced());
        assert!(pods.command_aliases().iter().any(|alias| alias == "po"));

        let nodes = ResourceDescriptor::from(ResourceKind::Nodes);
        assert_eq!(nodes.known_kind(), Some(ResourceKind::Nodes));
        assert!(!nodes.is_namespaced());

        let deployments = ResourceDescriptor::from(ResourceKind::Deployments);
        assert_eq!(deployments.qualified_name(), "deployments.apps");
        assert!(
            deployments
                .command_aliases()
                .iter()
                .any(|alias| alias == "deployments.apps")
        );
    }

    #[test]
    fn discovered_resources_use_exact_gvk_and_dynamic_aliases() {
        let widget = ResourceDescriptor::new(
            "example.io",
            "v1beta1",
            "example.io/v1beta1",
            "Widget",
            "widgets",
            true,
            ResourceOperations::all(),
        )
        .with_short_names(["wdg", "wg"]);
        assert_eq!(widget.known_kind(), None);
        assert_eq!(widget.label(), "Widget");
        assert_eq!(widget.short_label(), "widgets");
        assert_eq!(widget.qualified_name(), "widgets.example.io");
        assert_eq!(
            widget.command_aliases(),
            vec![
                "wdg".to_owned(),
                "wg".to_owned(),
                "widget".to_owned(),
                "widgets".to_owned(),
                "widgets.example.io".to_owned()
            ]
        );

        let pod_named_crd = ResourceDescriptor::new(
            "example.io",
            "v1",
            "example.io/v1",
            "Pod",
            "pods",
            true,
            ResourceOperations::all(),
        );
        assert_eq!(pod_named_crd.known_kind(), None);
    }

    #[test]
    fn crd_aliases_require_the_exact_crd_identity() {
        let crd = ResourceDescriptor::new(
            "apiextensions.k8s.io",
            "v1",
            "apiextensions.k8s.io/v1",
            "CustomResourceDefinition",
            "customresourcedefinitions",
            false,
            ResourceOperations::all(),
        );
        assert!(crd.is_custom_resource_definition());
        assert!(crd.command_aliases().iter().any(|alias| alias == "crd"));
        assert!(crd.command_aliases().iter().any(|alias| alias == "crds"));

        let lookalike = ResourceDescriptor::new(
            "example.io",
            "v1",
            "example.io/v1",
            "CustomResourceDefinition",
            "customresourcedefinitions",
            false,
            ResourceOperations::all(),
        );
        assert!(!lookalike.is_custom_resource_definition());
        assert!(
            !lookalike
                .command_aliases()
                .iter()
                .any(|alias| alias == "crd")
        );
        assert!(
            !lookalike
                .command_aliases()
                .iter()
                .any(|alias| alias == "crds")
        );
    }

    #[test]
    fn container_rows_expose_container_specific_data() {
        let row = ResourceRow::new("prod", "api", "Running", "true", "0", 0).with_container_data(
            ContainerData {
                image: Arc::from("example/api:v1"),
                kind: Arc::from("container"),
                restarts: 2,
            },
        );
        let data = row.container_data().expect("container data");
        assert_eq!(data.image.as_ref(), "example/api:v1");
        assert_eq!(data.kind.as_ref(), "container");
        assert_eq!(data.restarts, 2);
        assert!(row.matches("example/api"));
    }
}
