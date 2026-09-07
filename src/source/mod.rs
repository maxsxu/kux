mod demo;
mod discovery;
mod kubernetes;
mod logs;

use std::time::Duration;

use kube::Client;
use tokio::sync::{mpsc, watch};

use crate::{
    app::{ActionIntent, ResourceTarget},
    kubeconfig::ContextInfo,
    model::{NamespaceInfo, OwnerReferenceData, ResourceDescriptor, ResourceRow, ResourceSnapshot},
};

pub use discovery::discover_resources;
pub use logs::{LogRange, LogRequest, LogTarget};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchRequest {
    pub generation: u64,
    pub context: String,
    pub kind: ResourceDescriptor,
    pub namespace: Option<String>,
    pub field_selector: Option<String>,
    pub label_selector: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionRequest {
    pub generation: u64,
    pub intent: ActionIntent,
}

#[derive(Debug, Clone)]
pub struct KubernetesSettings {
    pub initial_context: String,
    pub publish_interval: Duration,
    pub streaming_lists: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionResult {
    LogBatch {
        id: u64,
        lines: Vec<String>,
        finished: bool,
        error: Option<String>,
    },
    Scoped {
        generation: u64,
        result: Box<ActionResult>,
    },
    Notice(String),
    Content {
        title: String,
        body: String,
        managed_fields: Option<String>,
        decoded_body: Option<String>,
        decoded_managed_fields: Option<String>,
        content: crate::app::ContentKind,
    },
    ManagedYaml {
        target: crate::app::ResourceTarget,
        body: String,
        decoded_body: Option<String>,
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
