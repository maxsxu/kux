mod batch_navigation;
mod demo;
mod deployment_navigation;
mod discovery;
pub(crate) mod image_actions;
pub(crate) mod kubernetes;
mod log_scope;
mod logs;
mod pod_actions;
mod references;
mod refresh;
mod rollback;
pub use batch_navigation::BatchNavigationFilter;
pub use image_actions::{ContainerImage, ImageSnapshot, supports_images};
pub use pod_actions::SanitizeReport;
pub use references::{ReferenceEntry, ReferenceScan, supports_references};
pub use rollback::RollbackSnapshot;

use std::time::Duration;

use kube::Client;
use tokio::sync::{mpsc, watch};

use crate::{
    app::{ActionIntent, ResourceTarget},
    kubeconfig::ContextInfo,
    model::{NamespaceInfo, OwnerReferenceData, ResourceDescriptor, ResourceRow, ResourceSnapshot},
};

pub use discovery::discover_resources;
#[cfg(test)]
pub(crate) use kubernetes::resource_views::project_subject_row;
pub use log_scope::{LogLine, LogSessionMeta, LogSource, LogTarget, supports_logs};
pub use logs::{LogRange, LogRequest};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchRequest {
    pub generation: u64,
    pub context: String,
    pub kind: ResourceDescriptor,
    pub namespace: Option<String>,
    pub field_selector: Option<String>,
    pub label_selector: Option<String>,
    pub instance: Option<ResourceTarget>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionRequest {
    pub generation: u64,
    pub intent: ActionIntent,
}

impl ActionRequest {
    fn rejected(self, message: String) -> ActionResult {
        let result = match self.intent {
            ActionIntent::LoadBatchNavigation { id, target } => ActionResult::BatchNavigation {
                id,
                target,
                result: Err(message),
            },
            ActionIntent::LoadReferences { id, target } => ActionResult::References {
                id,
                target,
                result: Err(message),
            },
            ActionIntent::LoadDeploymentSelector {
                id,
                target,
                replica_sets,
            } => ActionResult::DeploymentSelector {
                id,
                target,
                replica_sets,
                result: Err(message),
            },
            ActionIntent::LoadRollback { id, target } => ActionResult::RollbackLoaded {
                id,
                target,
                result: Err(message),
            },
            ActionIntent::Rollback { .. } => ActionResult::RolledBack {
                result: Err(message),
            },
            ActionIntent::Content { id, target, .. } => ActionResult::OpenedContent {
                id,
                target,
                result: Box::new(ActionResult::Error(message)),
            },
            ActionIntent::ManagedYaml { id, target, .. } => ActionResult::ManagedYaml {
                id,
                target,
                result: Err(message),
            },
            ActionIntent::LoadImages { id, target } => ActionResult::Images {
                id,
                target,
                result: Err(message),
            },
            ActionIntent::LoadTransferPod { id, target } => ActionResult::TransferPod {
                id,
                target,
                result: Err(message),
            },
            ActionIntent::RefreshYaml { id, target, .. } => ActionResult::RefreshedYaml {
                id,
                target,
                result: Err(message),
            },
            ActionIntent::RefreshDescribe { id, target } => ActionResult::RefreshedDescribe {
                id,
                target,
                result: Err(message),
            },
            ActionIntent::StartLogs { request } => ActionResult::LogBatch {
                id: request.id,
                lines: Vec::new(),
                finished: true,
                error: Some(message),
            },
            ActionIntent::SanitizePods { namespace } => ActionResult::Sanitized {
                namespace,
                result: Err(message),
            },
            ActionIntent::SetImages { snapshot, .. } => ActionResult::ImagesUpdated {
                target: snapshot.target,
                result: Err(message),
            },
            _ => ActionResult::Error(message),
        };
        ActionResult::Scoped {
            generation: self.generation,
            result: Box::new(result),
        }
    }
}

#[derive(Debug, Clone)]
pub struct KubernetesSettings {
    pub connection: crate::connection::ConnectionOptions,
    pub initial_context: String,
    pub publish_interval: Duration,
    pub refresh_interval: watch::Receiver<Duration>,
    pub streaming_lists: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct YamlContent {
    pub body: String,
    pub with_managed_fields: Option<String>,
    pub decoded_body: Option<String>,
    pub decoded_with_managed_fields: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionResult {
    BatchNavigation {
        id: u64,
        target: ResourceTarget,
        result: Result<BatchNavigationFilter, String>,
    },
    References {
        id: u64,
        target: ResourceTarget,
        result: Result<ReferenceScan, String>,
    },
    DeploymentSelector {
        id: u64,
        target: ResourceTarget,
        replica_sets: bool,
        result: Result<String, String>,
    },
    RollbackLoaded {
        id: u64,
        target: ResourceTarget,
        result: Result<RollbackSnapshot, String>,
    },
    RolledBack {
        result: Result<String, String>,
    },
    TransferPod {
        id: u64,
        target: ResourceTarget,
        result: Result<crate::transfer::TransferPod, String>,
    },
    Sanitized {
        namespace: Option<String>,
        result: Result<SanitizeReport, String>,
    },
    OpenedContent {
        id: u64,
        target: ResourceTarget,
        result: Box<ActionResult>,
    },
    ImagesUpdated {
        target: ResourceTarget,
        result: Result<String, String>,
    },
    Images {
        id: u64,
        target: ResourceTarget,
        result: Result<ImageSnapshot, String>,
    },
    RefreshedDescribe {
        id: u64,
        target: ResourceTarget,
        result: Result<String, String>,
    },
    RefreshedYaml {
        id: u64,
        target: ResourceTarget,
        result: Result<YamlContent, String>,
    },
    LogResolved {
        id: u64,
        metadata: LogSessionMeta,
    },
    LogBatch {
        id: u64,
        lines: Vec<LogLine>,
        finished: bool,
        error: Option<String>,
    },
    Scoped {
        generation: u64,
        result: Box<ActionResult>,
    },
    Notice(String),
    TransientNotice(String),
    Content {
        title: String,
        body: String,
        managed_fields: Option<String>,
        decoded_body: Option<String>,
        decoded_managed_fields: Option<String>,
        content: crate::app::ContentKind,
    },
    ManagedYaml {
        id: u64,
        target: crate::app::ResourceTarget,
        result: Result<(String, Option<String>), String>,
    },
    NamespaceOptions {
        context: String,
        namespaces: Vec<NamespaceInfo>,
    },
    ContextOptions {
        context: String,
        contexts: Vec<ContextInfo>,
        notice: String,
    },
    ResourceOptions {
        context: String,
        resources: Vec<ResourceDescriptor>,
    },
    ChildResource {
        parent: crate::app::ResourceTarget,
        kind: ResourceDescriptor,
        owner_uid: String,
    },
    NoChildResource {
        parent: crate::app::ResourceTarget,
    },
    Owner {
        target: ResourceTarget,
        owner: Option<OwnerReferenceData>,
    },
    Containers {
        target: ResourceTarget,
        rows: Vec<std::sync::Arc<ResourceRow>>,
    },
    Policy {
        target: ResourceTarget,
        title: String,
        rows: Vec<std::sync::Arc<ResourceRow>>,
    },
    ServerVersion(String),
    ClusterMetrics {
        cpu: String,
        mem: String,
    },
    Error(String),
}

#[must_use = "the source task should be retained and aborted during shutdown"]
pub fn spawn_kubernetes(
    client: Client,
    settings: KubernetesSettings,
    requests: mpsc::Receiver<WatchRequest>,
    snapshots: watch::Sender<ResourceSnapshot>,
    actions: mpsc::Receiver<ActionRequest>,
    action_results: mpsc::Sender<ActionResult>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(kubernetes::run_manager(
        client,
        settings,
        requests,
        snapshots,
        actions,
        action_results,
    ))
}

#[must_use = "the source task should be retained and aborted during shutdown"]
pub fn spawn_demo(
    item_count: usize,
    requests: mpsc::Receiver<WatchRequest>,
    snapshots: watch::Sender<ResourceSnapshot>,
    actions: mpsc::Receiver<ActionRequest>,
    action_results: mpsc::Sender<ActionResult>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(demo::run_manager(
        item_count,
        requests,
        snapshots,
        actions,
        action_results,
    ))
}
