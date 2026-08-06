use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::{Debug, Write as _},
    sync::Arc,
    time::{Duration, Instant},
};

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use futures::StreamExt;
use http::{
    Request as HttpRequest,
    header::{ACCEPT, HeaderValue},
};
use k8s_openapi::api::{
    apps::v1::{DaemonSet, Deployment, ReplicaSet, StatefulSet},
    core::v1::{ContainerStatus, Namespace, Node, Pod, Service},
    rbac::v1::{ClusterRole, ClusterRoleBinding, PolicyRule, Role, RoleBinding, Subject},
};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{LabelSelector, Time};
use kube::{
    Api, Client, Config, Resource, ResourceExt,
    api::{
        DeleteParams, EvictParams, ListParams, LogParams, Patch, PatchParams, Preconditions,
        WatchParams,
    },
    config::KubeConfigOptions,
    core::{ApiResource, DynamicObject, GroupVersionKind, Request, WatchEvent},
    runtime::{WatchStreamExt, watcher},
};
use rustc_hash::FxHashMap;
use serde::{Deserialize, Deserializer, de::DeserializeOwned};
use tokio::{
    process::Command,
    sync::{mpsc, watch},
};

use crate::{
    app::{
        ActionIntent, ContainerTarget, ContentKind, NodeDrainFlags, NodeDrainOptions,
        ResourceTarget,
    },
    model::{
        ChildResourceData, ContainerData, NamespaceInfo, NodeData, OwnerReferenceData, PodData,
        PolicyData, PortForwardPort, ResourceData, ResourceDescriptor, ResourceKind,
        ResourceOperations, ResourceRow, ResourceSnapshot, SourceState, TableCell, TableColumn,
    },
};

use super::{ActionResult, KubernetesSettings, WatchRequest, discovery::discover_resources};

pub async fn run_manager(
    client: Client,
    settings: KubernetesSettings,
    mut requests: mpsc::Receiver<WatchRequest>,
    snapshots: watch::Sender<ResourceSnapshot>,
    mut actions: mpsc::Receiver<ActionIntent>,
    action_results: mpsc::Sender<ActionResult>,
) {
    let mut client = client;
    let mut active_context = settings.initial_context.clone();
    refresh_cluster_metadata(
        client.clone(),
        active_context.clone(),
        action_results.clone(),
    );
    let mut running: Option<tokio::task::JoinHandle<()>> = None;
    loop {
        tokio::select! {
            Some(request) = requests.recv() => {
                if request.context != active_context {
                    match client_for_context(&request.context).await {
                        Ok(next_client) => {
                            client = next_client;
                            active_context.clone_from(&request.context);
                            refresh_cluster_metadata(
                                client.clone(),
                                active_context.clone(),
                                action_results.clone(),
                            );
                        }
                        Err(error) => {
                            let _ = action_results.send(ActionResult::Error(error)).await;
                            continue;
                        }
                    }
                }
                if let Some(task) = running.take() {
                    task.abort();
                }
                let client = client.clone();
                let settings = settings.clone();
                let snapshots = snapshots.clone();
                running = Some(tokio::spawn(async move {
                    run_watch(client, settings, request, snapshots).await;
                }));
            }
            Some(action) = actions.recv() => {
                let client = client.clone();
                let context = active_context.clone();
                let action_results = action_results.clone();
                tokio::spawn(async move {
                    let result = handle_action(client, &context, action).await;
                    let _ = action_results.send(result).await;
                });
            }
            else => break,
        }
    }
    if let Some(task) = running {
        task.abort();
    }
}

fn refresh_cluster_metadata(
    client: Client,
    context: String,
    action_results: mpsc::Sender<ActionResult>,
) {
    tokio::spawn(async move {
        if let Ok(resources) = discover_resources(client.clone()).await {
            let _ = action_results
                .send(ActionResult::ResourceOptions {
                    context: context.clone(),
                    resources,
                })
                .await;
        }
        if let Ok(version) = client.apiserver_version().await {
            let _ = action_results
                .send(ActionResult::ServerVersion(version.git_version))
                .await;
        }
        let Ok(namespaces) = Api::<Namespace>::all(client)
            .list(&ListParams::default())
            .await
        else {
            return;
        };
        let mut namespaces: Vec<NamespaceInfo> =
            namespaces.items.iter().map(namespace_info).collect();
        namespaces.sort_unstable_by(|left, right| left.name.cmp(&right.name));
        namespaces.dedup_by(|left, right| left.name == right.name);
        let _ = action_results
            .send(ActionResult::NamespaceOptions {
                context,
                namespaces,
            })
            .await;
    });
}

fn namespace_info(namespace: &Namespace) -> NamespaceInfo {
    NamespaceInfo::discovered(
        namespace.name_any(),
        namespace
            .status
            .as_ref()
            .and_then(|status| status.phase.clone())
            .unwrap_or_else(|| "Unknown".to_owned()),
        namespace
            .metadata
            .creation_timestamp
            .as_ref()
            .map(|time| time.0.as_second()),
    )
}

async fn client_for_context(context: &str) -> Result<Client, String> {
    let config = Config::from_kubeconfig(&KubeConfigOptions {
        context: Some(context.to_owned()),
        ..KubeConfigOptions::default()
    })
    .await
    .map_err(|error| format!("Unable to load context {context:?}: {error}"))?;
    Client::try_from(config)
        .map_err(|error| format!("Unable to create client for context {context:?}: {error}"))
}

async fn run_watch(
    client: Client,
    settings: KubernetesSettings,
    request: WatchRequest,
    snapshots: watch::Sender<ResourceSnapshot>,
) {
    let _ = snapshots.send(ResourceSnapshot::loading(
        request.generation,
        request.kind.clone(),
    ));
    match request.kind.known_kind() {
        Some(ResourceKind::Pods) => {
            let api = namespaced_api::<Pod>(client.clone(), request.namespace.as_deref());
            watch_api(api, client, settings, request, snapshots, pod_row).await;
        }
        Some(ResourceKind::Deployments) => {
            let api = namespaced_api::<Deployment>(client.clone(), request.namespace.as_deref());
            watch_api(api, client, settings, request, snapshots, deployment_row).await;
        }
        Some(ResourceKind::StatefulSets) => {
            let api = namespaced_api::<StatefulSet>(client.clone(), request.namespace.as_deref());
            watch_api(api, client, settings, request, snapshots, stateful_set_row).await;
        }
        Some(ResourceKind::DaemonSets) => {
            let api = namespaced_api::<DaemonSet>(client.clone(), request.namespace.as_deref());
            watch_api(api, client, settings, request, snapshots, daemon_set_row).await;
        }
        Some(ResourceKind::Services) => {
            let api = namespaced_api::<Service>(client.clone(), request.namespace.as_deref());
            watch_api(api, client, settings, request, snapshots, service_row).await;
        }
        Some(ResourceKind::Nodes) => {
            let api = Api::<Node>::all(client.clone());
            watch_api(api, client, settings, request, snapshots, node_row).await;
        }
        None if request.kind.is_apps_v1_replica_set() => {
            let api = namespaced_api::<ReplicaSet>(client.clone(), request.namespace.as_deref());
            watch_api(api, client, settings, request, snapshots, replica_set_row).await;
        }
        None => {
            let api = dynamic_api(client.clone(), &request.kind, request.namespace.as_deref());
            if request.kind.is_api_service() && request.kind.operations.watchable() {
                watch_api(api, client, settings, request, snapshots, dynamic_row).await;
            } else if request.kind.operations.watchable() {
                watch_table_api(api, client, settings, request, snapshots).await;
            } else {
                poll_table_api(api, client, settings, request, snapshots).await;
            }
        }
    }
}

fn namespaced_api<K>(client: Client, namespace: Option<&str>) -> Api<K>
where
    K: Resource<Scope = kube::core::NamespaceResourceScope>,
    K::DynamicType: Default,
{
    match namespace {
        Some(namespace) => Api::<K>::namespaced(client, namespace),
        None => Api::<K>::all(client),
    }
}

fn api_resource(resource: &ResourceDescriptor) -> ApiResource {
    ApiResource {
        group: resource.group.to_string(),
        version: resource.version.to_string(),
        api_version: resource.api_version.to_string(),
        kind: resource.kind.to_string(),
        plural: resource.plural.to_string(),
    }
}

fn dynamic_api(
    client: Client,
    resource: &ResourceDescriptor,
    namespace: Option<&str>,
) -> Api<DynamicObject> {
    let api_resource = api_resource(resource);
    if resource.is_namespaced() {
        namespace.map_or_else(
            || Api::<DynamicObject>::all_with(client.clone(), &api_resource),
            |namespace| {
                Api::<DynamicObject>::namespaced_with(client.clone(), namespace, &api_resource)
            },
        )
    } else {
        Api::<DynamicObject>::all_with(client, &api_resource)
    }
}

async fn handle_action(client: Client, context: &str, action: ActionIntent) -> ActionResult {
    let result = match action {
        ActionIntent::Delete { targets, immediate } => {
            delete_resources(client, targets, immediate).await
        }
        ActionIntent::DeleteContext { name } => delete_context(context, &name).await,
        ActionIntent::Content { target, content } => {
            fetch_content(client, context, &target, content).await
        }
        ActionIntent::Containers { target } => load_containers(client, target).await,
        ActionIntent::ContainerLogs { target, previous } => {
            fetch_container_logs(client, target, previous).await
        }
        ActionIntent::AuthorizationPolicy { target } => {
            load_authorization_policy(client, target).await
        }
        ActionIntent::Restart { targets } => restart_workloads(client, targets).await,
        ActionIntent::SetNodeUnschedulable {
            name,
            unschedulable,
        } => set_node_unschedulable(client, &name, unschedulable).await,
        ActionIntent::DrainNode { name, options } => drain_node(client, &name, options).await,
    };
    result.unwrap_or_else(ActionResult::Error)
}

async fn delete_resources(
    client: Client,
    targets: Vec<ResourceTarget>,
    immediate: bool,
) -> Result<ActionResult, String> {
    let Some(first) = targets.first() else {
        return Err("No resources selected for deletion".to_owned());
    };
    let total = targets.len();
    let resource = first.kind.plural_name().to_owned();
    let first_identity = resource_identity(first);
    let tasks = targets.into_iter().map(|target| {
        let client = client.clone();
        async move {
            let identity = resource_identity(&target);
            (identity, delete_resource(client, &target, immediate).await)
        }
    });
    let results = futures::stream::iter(tasks)
        .buffer_unordered(16)
        .collect::<Vec<_>>()
        .await;
    let mut failures = results
        .into_iter()
        .filter_map(|(identity, result)| result.err().map(|error| (identity, error)))
        .collect::<Vec<_>>();
    failures.sort_unstable_by(|left, right| left.0.cmp(&right.0));
    deletion_result(immediate, &resource, &first_identity, total, &failures)
}

async fn restart_workloads(
    client: Client,
    targets: Vec<ResourceTarget>,
) -> Result<ActionResult, String> {
    let Some(first) = targets.first() else {
        return Err("No workloads selected for restart".to_owned());
    };
    if targets.iter().any(|target| {
        !target
            .kind
            .known_kind()
            .is_some_and(ResourceKind::is_restartable)
    }) {
        return Err("Restart supports Deployments, StatefulSets, and DaemonSets".to_owned());
    }
    let total = targets.len();
    let resource = first.kind.plural_name().to_owned();
    let first_kind = first.kind.kind.to_string();
    let first_identity = resource_identity(first);
    let restarted_at = k8s_openapi::jiff::Timestamp::now().to_string();
    let tasks = targets.into_iter().map(|target| {
        let client = client.clone();
        let restarted_at = restarted_at.clone();
        async move {
            let identity = resource_identity(&target);
            (
                identity,
                restart_workload(client, &target, &restarted_at).await,
            )
        }
    });
    let results = futures::stream::iter(tasks)
        .buffer_unordered(16)
        .collect::<Vec<_>>()
        .await;
    let mut failures = results
        .into_iter()
        .filter_map(|(identity, result)| result.err().map(|error| (identity, error)))
        .collect::<Vec<_>>();
    failures.sort_unstable_by(|left, right| left.0.cmp(&right.0));
    restart_result(&first_kind, &resource, &first_identity, total, &failures)
}

async fn restart_workload(
    client: Client,
    target: &ResourceTarget,
    restarted_at: &str,
) -> Result<(), String> {
    let namespace = target
        .namespace
        .as_deref()
        .ok_or_else(|| format!("{} requires a namespace", target.kind.kind))?;
    dynamic_api(client, &target.kind, Some(namespace))
        .patch(
            &target.name,
            &PatchParams::default(),
            &Patch::Merge(restart_patch(restarted_at)),
        )
        .await
        .map(|_| ())
        .map_err(|error| error.to_string())
}

fn restart_patch(restarted_at: &str) -> serde_json::Value {
    serde_json::json!({
        "spec": {
            "template": {
                "metadata": {
                    "annotations": {
                        "kubectl.kubernetes.io/restartedAt": restarted_at
                    }
                }
            }
        }
    })
}

fn restart_result(
    kind: &str,
    resource: &str,
    first_identity: &str,
    total: usize,
    failures: &[(String, String)],
) -> Result<ActionResult, String> {
    if failures.is_empty() {
        return Ok(ActionResult::Notice(if total == 1 {
            format!("Restart in progress for {kind} {first_identity}")
        } else {
            format!("Restart in progress for all {total} {resource}")
        }));
    }
    let succeeded = total.saturating_sub(failures.len());
    let mut details = failures
        .iter()
        .take(3)
        .map(|(identity, error)| format!("{identity}: {error}"))
        .collect::<Vec<_>>()
        .join("; ");
    if failures.len() > 3 {
        let _ = write!(details, "; and {} more", failures.len() - 3);
    }
    Err(format!(
        "Restart requested for {succeeded} of {total} {resource}; failed: {details}"
    ))
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DrainPod {
    namespace: String,
    name: String,
    uid: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PodDrainDisposition {
    Remove(DrainPod),
    Ignore,
    Block(String),
}

async fn drain_node(
    client: Client,
    node_name: &str,
    options: NodeDrainOptions,
) -> Result<ActionResult, String> {
    let timeout = options.timeout;
    match tokio::time::timeout(timeout, drain_node_with_options(client, node_name, options)).await {
        Ok(result) => result,
        Err(_) => Err(format!(
            "Drain of node {node_name} timed out after {}; the node may be cordoned and pods may still be terminating or blocked by a PodDisruptionBudget",
            format_duration(timeout)
        )),
    }
}

async fn drain_node_with_options(
    client: Client,
    node_name: &str,
    options: NodeDrainOptions,
) -> Result<ActionResult, String> {
    let pods = Api::<Pod>::all(client.clone())
        .list(&ListParams::default().fields(&format!("spec.nodeName={node_name}")))
        .await
        .map_err(|error| format!("Unable to list pods on node {node_name}: {error}"))?;
    let mut evictable = Vec::new();
    let mut ignored = 0_usize;
    let mut blockers = Vec::new();
    for pod in &pods.items {
        match pod_drain_disposition(pod, &options) {
            PodDrainDisposition::Remove(target) => evictable.push(target),
            PodDrainDisposition::Ignore => ignored += 1,
            PodDrainDisposition::Block(identity) => blockers.push(identity),
        }
    }
    if !blockers.is_empty() {
        blockers.sort_unstable();
        return Err(format!(
            "Cannot drain node {node_name}: {}",
            summarize_identities(&blockers)
        ));
    }

    patch_node_unschedulable(client.clone(), node_name, true).await?;

    let total = evictable.len();
    let tasks = evictable.iter().cloned().map(|target| {
        let client = client.clone();
        let options = options.clone();
        async move {
            let identity = format!("{}/{}", target.namespace, target.name);
            let result = remove_drain_pod(client, &target, &options).await;
            (identity, result)
        }
    });
    let mut failures = futures::stream::iter(tasks)
        .buffer_unordered(5)
        .filter_map(|(identity, result)| async move {
            result.err().map(|error| format!("{identity}: {error}"))
        })
        .collect::<Vec<_>>()
        .await;
    failures.sort_unstable();
    if !failures.is_empty() {
        return drain_result(node_name, total, ignored, &failures);
    }
    wait_for_drain_targets(&client, node_name, &evictable).await?;
    drain_result(node_name, total, ignored, &[])
}

async fn set_node_unschedulable(
    client: Client,
    node_name: &str,
    unschedulable: bool,
) -> Result<ActionResult, String> {
    patch_node_unschedulable(client, node_name, unschedulable).await?;
    let action = if unschedulable {
        "Cordoned"
    } else {
        "Uncordoned"
    };
    Ok(ActionResult::Notice(format!("{action} node {node_name}")))
}

async fn patch_node_unschedulable(
    client: Client,
    node_name: &str,
    unschedulable: bool,
) -> Result<(), String> {
    let action = if unschedulable { "cordon" } else { "uncordon" };
    Api::<Node>::all(client)
        .patch(
            node_name,
            &PatchParams::default(),
            &Patch::Merge(node_unschedulable_patch(unschedulable)),
        )
        .await
        .map(|_| ())
        .map_err(|error| format!("Unable to {action} node {node_name}: {error}"))
}

fn node_unschedulable_patch(unschedulable: bool) -> serde_json::Value {
    serde_json::json!({ "spec": { "unschedulable": unschedulable } })
}

async fn remove_drain_pod(
    client: Client,
    target: &DrainPod,
    options: &NodeDrainOptions,
) -> Result<(), String> {
    let api = Api::<Pod>::namespaced(client, &target.namespace);
    let delete_params = drain_delete_params(target, options);
    if options.flags.contains(NodeDrainFlags::DISABLE_EVICTION) {
        return match api.delete(&target.name, &delete_params).await {
            Ok(_) => Ok(()),
            Err(kube::Error::Api(status)) if status.code == 404 => Ok(()),
            Err(error) => Err(error.to_string()),
        };
    }
    let eviction_params = EvictParams {
        delete_options: Some(delete_params),
        ..EvictParams::default()
    };
    loop {
        match api.evict(&target.name, &eviction_params).await {
            Ok(_) => return Ok(()),
            Err(kube::Error::Api(status)) if status.code == 404 => return Ok(()),
            Err(kube::Error::Api(status)) if status.code == 429 => {
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
            Err(error) => return Err(error.to_string()),
        }
    }
}

fn drain_delete_params(target: &DrainPod, options: &NodeDrainOptions) -> DeleteParams {
    DeleteParams {
        grace_period_seconds: options.grace_period_seconds,
        preconditions: target.uid.as_ref().map(|uid| Preconditions {
            uid: Some(uid.clone()),
            ..Preconditions::default()
        }),
        ..DeleteParams::default()
    }
}

async fn wait_for_drain_targets(
    client: &Client,
    node_name: &str,
    targets: &[DrainPod],
) -> Result<(), String> {
    let targets = targets
        .iter()
        .map(|target| (target.namespace.clone(), target.name.clone()))
        .collect::<BTreeSet<_>>();
    if targets.is_empty() {
        return Ok(());
    }
    let pods = Api::<Pod>::all(client.clone());
    loop {
        let assigned = pods
            .list(&ListParams::default().fields(&format!("spec.nodeName={node_name}")))
            .await
            .map_err(|error| format!("Unable to verify drain of node {node_name}: {error}"))?;
        let remaining = assigned.items.iter().any(|pod| {
            pod.namespace()
                .map(|namespace| (namespace, pod.name_any()))
                .is_some_and(|identity| targets.contains(&identity))
        });
        if !remaining {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

fn pod_drain_disposition(pod: &Pod, options: &NodeDrainOptions) -> PodDrainDisposition {
    let identity = format!(
        "{}/{}",
        pod.namespace().as_deref().unwrap_or("<unknown>"),
        pod.name_any()
    );
    if pod.meta().deletion_timestamp.is_some()
        || pod
            .meta()
            .annotations
            .as_ref()
            .is_some_and(|annotations| annotations.contains_key("kubernetes.io/config.mirror"))
    {
        return PodDrainDisposition::Ignore;
    }
    let controller = pod
        .meta()
        .owner_references
        .as_ref()
        .and_then(|owners| owners.iter().find(|owner| owner.controller == Some(true)));
    if controller.is_some_and(|owner| owner.kind == "DaemonSet") {
        return if options.flags.contains(NodeDrainFlags::IGNORE_DAEMON_SETS) {
            PodDrainDisposition::Ignore
        } else {
            PodDrainDisposition::Block(format!(
                "{identity} is managed by a DaemonSet; enable Ignore DaemonSets"
            ))
        };
    }
    let uses_emptydir = pod
        .spec
        .as_ref()
        .and_then(|spec| spec.volumes.as_ref())
        .is_some_and(|volumes| volumes.iter().any(|volume| volume.empty_dir.is_some()));
    if uses_emptydir && !options.flags.contains(NodeDrainFlags::DELETE_EMPTYDIR_DATA) {
        return PodDrainDisposition::Block(format!(
            "{identity} uses emptyDir; enable Delete EmptyDir Data"
        ));
    }
    let terminal = pod
        .status
        .as_ref()
        .and_then(|status| status.phase.as_deref())
        .is_some_and(|phase| matches!(phase, "Succeeded" | "Failed"));
    if controller.is_none() && !terminal && !options.flags.contains(NodeDrainFlags::FORCE) {
        return PodDrainDisposition::Block(format!("{identity} is unmanaged; enable Force"));
    }
    let Some(namespace) = pod.namespace() else {
        return PodDrainDisposition::Block(identity);
    };
    PodDrainDisposition::Remove(DrainPod {
        namespace,
        name: pod.name_any(),
        uid: pod.meta().uid.clone(),
    })
}

fn drain_result(
    node_name: &str,
    total: usize,
    ignored: usize,
    failures: &[String],
) -> Result<ActionResult, String> {
    if !failures.is_empty() {
        let accepted = total.saturating_sub(failures.len());
        return Err(format!(
            "Node {node_name} was cordoned; removal started for {accepted} of {total} pods; failed: {}",
            summarize_identities(failures)
        ));
    }
    let ignored_suffix = if ignored == 0 {
        String::new()
    } else {
        format!("; skipped {ignored} DaemonSet, static, or terminating pods")
    };
    Ok(ActionResult::Notice(format!(
        "Drained node {node_name}: cordoned and removed {total} pods{ignored_suffix}"
    )))
}

fn format_duration(duration: Duration) -> String {
    if duration.subsec_millis() == 0 {
        format!("{}s", duration.as_secs())
    } else {
        format!("{}ms", duration.as_millis())
    }
}

fn summarize_identities(identities: &[String]) -> String {
    let mut summary = identities
        .iter()
        .take(3)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    if identities.len() > 3 {
        let _ = write!(summary, ", and {} more", identities.len() - 3);
    }
    summary
}

fn deletion_result(
    immediate: bool,
    resource: &str,
    first_identity: &str,
    total: usize,
    failures: &[(String, String)],
) -> Result<ActionResult, String> {
    let past_tense = if immediate { "Killed" } else { "Deleted" };
    let action = if immediate { "kill" } else { "delete" };
    if failures.is_empty() {
        return Ok(ActionResult::Notice(if total == 1 {
            format!("{past_tense} {first_identity}")
        } else {
            format!("{past_tense} all {total} {resource}")
        }));
    }

    let succeeded = total.saturating_sub(failures.len());
    let mut details = failures
        .iter()
        .take(3)
        .map(|(identity, error)| format!("{identity}: {error}"))
        .collect::<Vec<_>>()
        .join("; ");
    if failures.len() > 3 {
        let _ = write!(details, "; and {} more", failures.len() - 3);
    }
    Err(if succeeded == 0 {
        format!("Failed to {action} all {total} {resource}: {details}")
    } else {
        format!(
            "{past_tense} {succeeded} of {total} {resource}; failed to {action} {}: {details}",
            failures.len()
        )
    })
}

fn resource_identity(target: &ResourceTarget) -> String {
    format!(
        "{}/{}",
        target.namespace.as_deref().unwrap_or("cluster"),
        target.name
    )
}

async fn delete_resource(
    client: Client,
    target: &ResourceTarget,
    immediate: bool,
) -> Result<(), String> {
    ensure_deletable(target)?;
    let mut params = DeleteParams::background();
    if immediate {
        params.grace_period_seconds = Some(0);
    }
    delete_from(dynamic_target_api(client, target)?, target, &params).await
}

fn ensure_deletable(target: &ResourceTarget) -> Result<(), String> {
    if !target.kind.operations.deletable() {
        return Err(format!(
            "{} does not advertise delete support",
            target.kind.qualified_name()
        ));
    }
    Ok(())
}

fn context_delete_args(name: &str) -> [&str; 3] {
    ["config", "delete-context", name]
}

async fn delete_context(active_context: &str, name: &str) -> Result<ActionResult, String> {
    let output = Command::new("kubectl")
        .args(context_delete_args(name))
        .output()
        .await
        .map_err(|error| format!("Unable to run kubectl config delete-context: {error}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return Err(if stderr.is_empty() {
            format!(
                "kubectl config delete-context failed with {}",
                output.status
            )
        } else {
            stderr
        });
    }
    let catalog = crate::kubeconfig::read_contexts(Some(active_context))?;
    Ok(ActionResult::ContextOptions {
        context: active_context.to_owned(),
        contexts: catalog.contexts,
        notice: format!("Deleted context {name}"),
    })
}

async fn delete_from<K>(
    api: Api<K>,
    target: &ResourceTarget,
    params: &DeleteParams,
) -> Result<(), String>
where
    K: Clone + Resource + DeserializeOwned + Debug,
{
    api.delete(&target.name, params)
        .await
        .map(|_| ())
        .map_err(|error| error.to_string())
}

async fn fetch_content(
    client: Client,
    context: &str,
    target: &ResourceTarget,
    content_kind: ContentKind,
) -> Result<ActionResult, String> {
    if content_kind == ContentKind::SecretData {
        return fetch_secret_data(client, target).await;
    }
    if content_kind == ContentKind::Describe {
        return describe_resource(context, target).await;
    }
    if matches!(content_kind, ContentKind::Logs | ContentKind::PreviousLogs) {
        if target.kind.known_kind() != Some(ResourceKind::Pods) {
            return Err("Logs are currently supported for pods only".to_owned());
        }
        let api = namespaced_target_api::<Pod>(client, target)?;
        let body = api
            .logs(
                &target.name,
                &pod_log_params(None, content_kind == ContentKind::PreviousLogs),
            )
            .await
            .map_err(|error| error.to_string())?;
        let label = if content_kind == ContentKind::PreviousLogs {
            "Previous Logs"
        } else {
            "Logs"
        };
        return Ok(ActionResult::Content {
            title: format!("{label} · {}", target.name),
            body,
            managed_fields: None,
            decoded_body: None,
            decoded_managed_fields: None,
            content: content_kind,
        });
    }

    if !target.kind.operations.gettable() {
        return Err(format!(
            "{} does not advertise get support",
            target.kind.qualified_name()
        ));
    }
    let yaml = yaml_from(dynamic_target_api(client, target)?, target).await?;
    Ok(ActionResult::Content {
        title: yaml_title(target),
        body: yaml.body,
        managed_fields: yaml.with_managed_fields,
        decoded_body: yaml.decoded_body,
        decoded_managed_fields: yaml.decoded_with_managed_fields,
        content: content_kind,
    })
}

async fn fetch_secret_data(
    client: Client,
    target: &ResourceTarget,
) -> Result<ActionResult, String> {
    if !target.kind.is_core_v1_secret() {
        return Err("Secret decoding is supported for core/v1 Secrets only".to_owned());
    }
    let object = dynamic_target_api(client, target)?
        .get(&target.name)
        .await
        .map_err(|error| error.to_string())?;
    secret_data_content(&object, target)
}

fn secret_data_content(
    object: &DynamicObject,
    target: &ResourceTarget,
) -> Result<ActionResult, String> {
    let values = decode_secret_data(object)?;
    let body = serde_yaml::to_string(&values).map_err(|error| error.to_string())?;
    Ok(ActionResult::Content {
        title: target.namespace.as_deref().map_or_else(
            || format!("Secret({})", target.name),
            |namespace| format!("Secret({namespace}/{})", target.name),
        ),
        body,
        managed_fields: None,
        decoded_body: None,
        decoded_managed_fields: None,
        content: ContentKind::SecretData,
    })
}

async fn describe_resource(context: &str, target: &ResourceTarget) -> Result<ActionResult, String> {
    let output = Command::new("kubectl")
        .args(describe_args(context, target))
        .output()
        .await
        .map_err(|error| format!("Unable to run kubectl describe: {error}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return Err(if stderr.is_empty() {
            format!("kubectl describe failed with {}", output.status)
        } else {
            stderr
        });
    }
    Ok(ActionResult::Content {
        title: format!("Describe · {}", target.name),
        body: String::from_utf8_lossy(&output.stdout).into_owned(),
        managed_fields: None,
        decoded_body: None,
        decoded_managed_fields: None,
        content: ContentKind::Describe,
    })
}

fn describe_args(context: &str, target: &ResourceTarget) -> Vec<String> {
    let mut args = vec![
        "describe".to_owned(),
        target.kind.qualified_name(),
        target.name.clone(),
        "--context".to_owned(),
        context.to_owned(),
    ];
    if let Some(namespace) = &target.namespace {
        args.extend(["--namespace".to_owned(), namespace.clone()]);
    }
    args
}

struct YamlContent {
    body: String,
    with_managed_fields: Option<String>,
    decoded_body: Option<String>,
    decoded_with_managed_fields: Option<String>,
}

#[derive(Debug, Clone, Copy)]
struct ResourceUsage {
    cpu_millis: Option<u64>,
    memory_bytes: Option<u64>,
}

enum EnrichmentUpdate {
    PodUsage(FxHashMap<Arc<str>, ResourceUsage>),
    NodeUsage(FxHashMap<Arc<str>, ResourceUsage>),
    NodePods(FxHashMap<Arc<str>, u32>),
}

#[derive(Default)]
struct EnrichmentState {
    pod_usage: FxHashMap<Arc<str>, ResourceUsage>,
    node_usage: FxHashMap<Arc<str>, ResourceUsage>,
    node_pods: FxHashMap<Arc<str>, u32>,
}

impl EnrichmentState {
    fn update(&mut self, update: EnrichmentUpdate) {
        match update {
            EnrichmentUpdate::PodUsage(usage) => self.pod_usage = usage,
            EnrichmentUpdate::NodeUsage(usage) => self.node_usage = usage,
            EnrichmentUpdate::NodePods(pods) => self.node_pods = pods,
        }
    }

    fn apply(&self, row: &mut ResourceRow) {
        match &mut row.data {
            ResourceData::Pod(data) => {
                if let Some(usage) = self.pod_usage.get(&row.key) {
                    data.cpu_millis = usage.cpu_millis;
                    data.memory_bytes = usage.memory_bytes;
                }
            }
            ResourceData::Node(data) => {
                if let Some(usage) = self.node_usage.get(row.name.as_ref()) {
                    data.cpu_millis = usage.cpu_millis;
                    data.memory_bytes = usage.memory_bytes;
                }
                data.pods = self.node_pods.get(row.name.as_ref()).copied().unwrap_or(0);
            }
            ResourceData::Generic
            | ResourceData::ChildResource(_)
            | ResourceData::Container(_)
            | ResourceData::Policy(_) => {}
        }
    }

    fn apply_all(&self, rows: &mut FxHashMap<Arc<str>, Arc<ResourceRow>>) {
        for row in rows.values_mut() {
            self.apply(Arc::make_mut(row));
        }
    }
}

struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

fn spawn_enrichment_tasks(
    client: Client,
    request: &WatchRequest,
    settings: &KubernetesSettings,
    updates: mpsc::Sender<EnrichmentUpdate>,
) -> Vec<AbortOnDrop> {
    match request.kind.known_kind() {
        Some(ResourceKind::Pods) if request.field_selector.is_none() => vec![AbortOnDrop(
            tokio::spawn(pod_metrics_loop(client, request.namespace.clone(), updates)),
        )],
        Some(ResourceKind::Nodes) => vec![
            AbortOnDrop(tokio::spawn(node_metrics_loop(
                client.clone(),
                updates.clone(),
            ))),
            AbortOnDrop(tokio::spawn(node_pod_count_loop(
                client,
                settings.streaming_lists,
                updates,
            ))),
        ],
        _ => Vec::new(),
    }
}

async fn pod_metrics_loop(
    client: Client,
    namespace: Option<String>,
    updates: mpsc::Sender<EnrichmentUpdate>,
) {
    let mut ticker = tokio::time::interval(Duration::from_secs(5));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        ticker.tick().await;
        if let Ok(usage) = fetch_pod_metrics(client.clone(), namespace.as_deref()).await {
            let _ = updates.try_send(EnrichmentUpdate::PodUsage(usage));
        }
    }
}

async fn node_metrics_loop(client: Client, updates: mpsc::Sender<EnrichmentUpdate>) {
    let mut ticker = tokio::time::interval(Duration::from_secs(5));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        ticker.tick().await;
        if let Ok(usage) = fetch_node_metrics(client.clone()).await {
            let _ = updates.try_send(EnrichmentUpdate::NodeUsage(usage));
        }
    }
}

async fn fetch_pod_metrics(
    client: Client,
    namespace: Option<&str>,
) -> Result<FxHashMap<Arc<str>, ResourceUsage>, kube::Error> {
    let resource = metrics_resource("PodMetrics", "pods");
    let api = match namespace {
        Some(namespace) => Api::<DynamicObject>::namespaced_with(client, namespace, &resource),
        None => Api::<DynamicObject>::all_with(client, &resource),
    };
    let metrics = api.list(&ListParams::default()).await?;
    let usage = metrics
        .items
        .into_iter()
        .filter_map(|metric| {
            let namespace = metric.namespace()?;
            let name = metric.metadata.name.as_deref()?;
            let containers = metric.data.get("containers")?.as_array()?;
            let mut cpu_millis = 0_u64;
            let mut memory_bytes = 0_u64;
            let mut has_cpu = false;
            let mut has_memory = false;
            for container in containers {
                let container_usage = container.get("usage")?;
                if let Some(value) = container_usage
                    .get("cpu")
                    .and_then(serde_json::Value::as_str)
                    .and_then(parse_cpu_millis)
                {
                    cpu_millis = cpu_millis.saturating_add(value);
                    has_cpu = true;
                }
                if let Some(value) = container_usage
                    .get("memory")
                    .and_then(serde_json::Value::as_str)
                    .and_then(parse_memory_bytes)
                {
                    memory_bytes = memory_bytes.saturating_add(value);
                    has_memory = true;
                }
            }
            Some((
                Arc::from(format!("{namespace}/{name}")),
                ResourceUsage {
                    cpu_millis: has_cpu.then_some(cpu_millis),
                    memory_bytes: has_memory.then_some(memory_bytes),
                },
            ))
        })
        .collect();
    Ok(usage)
}

async fn fetch_node_metrics(
    client: Client,
) -> Result<FxHashMap<Arc<str>, ResourceUsage>, kube::Error> {
    let resource = metrics_resource("NodeMetrics", "nodes");
    let metrics = Api::<DynamicObject>::all_with(client, &resource)
        .list(&ListParams::default())
        .await?;
    Ok(metrics
        .items
        .into_iter()
        .filter_map(|metric| {
            let name = Arc::<str>::from(metric.metadata.name.as_deref()?);
            let usage = metric.data.get("usage")?;
            Some((
                name,
                ResourceUsage {
                    cpu_millis: usage
                        .get("cpu")
                        .and_then(serde_json::Value::as_str)
                        .and_then(parse_cpu_millis),
                    memory_bytes: usage
                        .get("memory")
                        .and_then(serde_json::Value::as_str)
                        .and_then(parse_memory_bytes),
                },
            ))
        })
        .collect())
}

fn metrics_resource(kind: &str, plural: &str) -> ApiResource {
    ApiResource::from_gvk_with_plural(
        &GroupVersionKind::gvk("metrics.k8s.io", "v1beta1", kind),
        plural,
    )
}

async fn node_pod_count_loop(
    client: Client,
    streaming_lists: bool,
    updates: mpsc::Sender<EnrichmentUpdate>,
) {
    let config = if streaming_lists {
        watcher::Config::default().streaming_lists()
    } else {
        watcher::Config::default().page_size(500)
    };
    let stream = watcher::watcher(Api::<Pod>::all(client), config).default_backoff();
    tokio::pin!(stream);
    let mut assignments: FxHashMap<Arc<str>, Arc<str>> = FxHashMap::default();
    let mut initializing: Option<FxHashMap<Arc<str>, Arc<str>>> = None;
    let mut dirty = false;
    let mut ticker = tokio::time::interval(Duration::from_millis(250));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            event = stream.next() => match event {
                Some(Ok(watcher::Event::Init)) => {
                    initializing = Some(FxHashMap::default());
                }
                Some(Ok(watcher::Event::InitApply(pod))) => {
                    if let Some((key, node)) = pod_assignment(&pod) {
                        initializing.get_or_insert_with(FxHashMap::default).insert(key, node);
                    }
                }
                Some(Ok(watcher::Event::InitDone)) => {
                    if let Some(initial) = initializing.take() {
                        assignments = initial;
                    }
                    let counts = assignment_counts(&assignments);
                    let _ = updates.try_send(EnrichmentUpdate::NodePods(counts));
                    dirty = false;
                }
                Some(Ok(watcher::Event::Apply(pod))) => {
                    update_assignment(&mut assignments, &pod);
                    dirty = true;
                }
                Some(Ok(watcher::Event::Delete(pod))) => {
                    assignments.remove(&pod_key(&pod));
                    dirty = true;
                }
                Some(Err(_)) => {}
                None => return,
            },
            _ = ticker.tick(), if dirty && initializing.is_none() => {
                let counts = assignment_counts(&assignments);
                let _ = updates.try_send(EnrichmentUpdate::NodePods(counts));
                dirty = false;
            }
        }
    }
}

fn pod_key(pod: &Pod) -> Arc<str> {
    Arc::from(format!(
        "{}/{}",
        pod.namespace().unwrap_or_default(),
        pod.name_any()
    ))
}

fn pod_assignment(pod: &Pod) -> Option<(Arc<str>, Arc<str>)> {
    let node = pod.spec.as_ref()?.node_name.as_deref()?;
    Some((pod_key(pod), Arc::from(node)))
}

fn update_assignment(assignments: &mut FxHashMap<Arc<str>, Arc<str>>, pod: &Pod) {
    let key = pod_key(pod);
    if let Some((_, node)) = pod_assignment(pod) {
        assignments.insert(key, node);
    } else {
        assignments.remove(&key);
    }
}

fn assignment_counts(assignments: &FxHashMap<Arc<str>, Arc<str>>) -> FxHashMap<Arc<str>, u32> {
    let mut counts = FxHashMap::default();
    for node in assignments.values() {
        let count = counts.entry(Arc::clone(node)).or_insert(0_u32);
        *count = count.saturating_add(1);
    }
    counts
}

fn namespaced_target_api<K>(client: Client, target: &ResourceTarget) -> Result<Api<K>, String>
where
    K: Resource<Scope = kube::core::NamespaceResourceScope>,
    K::DynamicType: Default,
{
    target
        .namespace
        .as_deref()
        .map(|namespace| Api::<K>::namespaced(client, namespace))
        .ok_or_else(|| format!("{} requires a namespace", target.name))
}

fn dynamic_target_api(
    client: Client,
    target: &ResourceTarget,
) -> Result<Api<DynamicObject>, String> {
    if target.kind.is_namespaced() && target.namespace.is_none() {
        return Err(format!("{} requires a namespace", target.name));
    }
    Ok(dynamic_api(
        client,
        &target.kind,
        target.namespace.as_deref(),
    ))
}

fn yaml_title(target: &ResourceTarget) -> String {
    target.namespace.as_deref().map_or_else(
        || format!("YAML({})", target.name),
        |namespace| format!("YAML({namespace}/{})", target.name),
    )
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum PolicyRoleKind {
    Role,
    ClusterRole,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct PolicyReference {
    role_kind: PolicyRoleKind,
    role_namespace: Option<String>,
    role_name: String,
    scope: String,
    binding: String,
}

struct PolicyRules {
    scope: String,
    binding: String,
    rules: Vec<PolicyRule>,
}

async fn load_authorization_policy(
    client: Client,
    target: ResourceTarget,
) -> Result<ActionResult, String> {
    let kind = target.kind.kind.as_ref();
    let namespace = target.namespace.as_deref();
    let (title, sources) = match kind {
        "ServiceAccount" => {
            let namespace = namespace
                .ok_or_else(|| "ServiceAccount authorization requires a namespace".to_owned())?;
            let references =
                service_account_references(client.clone(), namespace, &target.name).await?;
            (
                format!("policy(ServiceAccount:{namespace}/{})", target.name),
                resolve_policy_references(client, references).await?,
            )
        }
        "Role" => {
            let namespace = namespace.ok_or_else(|| "Role requires a namespace".to_owned())?;
            let role = Api::<Role>::namespaced(client, namespace)
                .get(&target.name)
                .await
                .map_err(|error| error.to_string())?;
            (
                format!("rbac(Role:{namespace}/{})", target.name),
                vec![PolicyRules {
                    scope: namespace.to_owned(),
                    binding: format!("RO:{}", target.name),
                    rules: role.rules.unwrap_or_default(),
                }],
            )
        }
        "ClusterRole" => {
            let role = Api::<ClusterRole>::all(client)
                .get(&target.name)
                .await
                .map_err(|error| error.to_string())?;
            (
                format!("rbac(ClusterRole:{})", target.name),
                vec![PolicyRules {
                    scope: "-".to_owned(),
                    binding: format!("CR:{}", target.name),
                    rules: role.rules.unwrap_or_default(),
                }],
            )
        }
        "RoleBinding" => {
            let namespace =
                namespace.ok_or_else(|| "RoleBinding requires a namespace".to_owned())?;
            let binding = Api::<RoleBinding>::namespaced(client.clone(), namespace)
                .get(&target.name)
                .await
                .map_err(|error| error.to_string())?;
            let reference = reference_from_role_binding(&binding, true)?;
            (
                format!("rbac(RoleBinding:{namespace}/{})", target.name),
                resolve_policy_references(client, BTreeSet::from([reference])).await?,
            )
        }
        "ClusterRoleBinding" => {
            let binding = Api::<ClusterRoleBinding>::all(client.clone())
                .get(&target.name)
                .await
                .map_err(|error| error.to_string())?;
            let reference = reference_from_cluster_role_binding(&binding, true)?;
            (
                format!("rbac(ClusterRoleBinding:{})", target.name),
                resolve_policy_references(client, BTreeSet::from([reference])).await?,
            )
        }
        _ => {
            return Err(format!(
                "{} is not an RBAC authorization resource",
                target.kind
            ));
        }
    };
    Ok(ActionResult::Policy {
        target,
        title,
        rows: policy_rows(sources),
    })
}

async fn service_account_references(
    client: Client,
    namespace: &str,
    name: &str,
) -> Result<BTreeSet<PolicyReference>, String> {
    let (mut references, cluster_references) = tokio::try_join!(
        collect_policy_references(Api::<RoleBinding>::all(client.clone()), |binding| {
            let binding_namespace = binding.namespace().unwrap_or_default();
            let matches = binding.subjects.as_deref().is_some_and(|subjects| {
                subjects.iter().any(|subject| {
                    service_account_subject_matches(
                        subject,
                        namespace,
                        name,
                        Some(&binding_namespace),
                    )
                })
            });
            matches
                .then(|| reference_from_role_binding(binding, false))
                .transpose()
        }),
        collect_policy_references(Api::<ClusterRoleBinding>::all(client), |binding| {
            let matches = binding.subjects.as_deref().is_some_and(|subjects| {
                subjects
                    .iter()
                    .any(|subject| service_account_subject_matches(subject, namespace, name, None))
            });
            matches
                .then(|| reference_from_cluster_role_binding(binding, false))
                .transpose()
        })
    )?;
    references.extend(cluster_references);
    Ok(references)
}

async fn collect_policy_references<K, F>(
    api: Api<K>,
    mut project: F,
) -> Result<BTreeSet<PolicyReference>, String>
where
    K: Clone + DeserializeOwned + Debug,
    F: FnMut(&K) -> Result<Option<PolicyReference>, String>,
{
    let mut references = BTreeSet::new();
    let mut token = None;
    loop {
        let params = token.as_deref().map_or_else(
            || ListParams::default().limit(500),
            |token| ListParams::default().limit(500).continue_token(token),
        );
        let page = api.list(&params).await.map_err(|error| error.to_string())?;
        for item in &page.items {
            if let Some(reference) = project(item)? {
                references.insert(reference);
            }
        }
        token = page.metadata.continue_.filter(|token| !token.is_empty());
        if token.is_none() {
            return Ok(references);
        }
    }
}

fn service_account_subject_matches(
    subject: &Subject,
    namespace: &str,
    name: &str,
    binding_namespace: Option<&str>,
) -> bool {
    subject.kind == "ServiceAccount"
        && subject.name == name
        && subject.namespace.as_deref().or(binding_namespace) == Some(namespace)
}

fn reference_from_role_binding(
    binding: &RoleBinding,
    include_binding: bool,
) -> Result<PolicyReference, String> {
    let namespace = binding
        .namespace()
        .ok_or_else(|| format!("RoleBinding {} has no namespace", binding.name_any()))?;
    let role_kind = match binding.role_ref.kind.as_str() {
        "Role" => PolicyRoleKind::Role,
        "ClusterRole" => PolicyRoleKind::ClusterRole,
        kind => return Err(format!("Unsupported RoleBinding roleRef kind {kind:?}")),
    };
    let prefix = if role_kind == PolicyRoleKind::Role {
        "RO"
    } else {
        "CR"
    };
    let role = format!("{prefix}:{}", binding.role_ref.name);
    Ok(PolicyReference {
        role_namespace: (role_kind == PolicyRoleKind::Role).then(|| namespace.clone()),
        role_kind,
        role_name: binding.role_ref.name.clone(),
        scope: namespace,
        binding: if include_binding {
            format!("RB:{}→{role}", binding.name_any())
        } else {
            role
        },
    })
}

fn reference_from_cluster_role_binding(
    binding: &ClusterRoleBinding,
    include_binding: bool,
) -> Result<PolicyReference, String> {
    if binding.role_ref.kind != "ClusterRole" {
        return Err(format!(
            "Unsupported ClusterRoleBinding roleRef kind {:?}",
            binding.role_ref.kind
        ));
    }
    let role = format!("CR:{}", binding.role_ref.name);
    Ok(PolicyReference {
        role_kind: PolicyRoleKind::ClusterRole,
        role_namespace: None,
        role_name: binding.role_ref.name.clone(),
        scope: "-".to_owned(),
        binding: if include_binding {
            format!("CRB:{}→{role}", binding.name_any())
        } else {
            role
        },
    })
}

async fn resolve_policy_references(
    client: Client,
    references: BTreeSet<PolicyReference>,
) -> Result<Vec<PolicyRules>, String> {
    let references: Vec<_> = references.into_iter().collect();
    let role_keys: BTreeSet<_> = references
        .iter()
        .map(|reference| {
            (
                reference.role_kind.clone(),
                reference.role_namespace.clone(),
                reference.role_name.clone(),
            )
        })
        .collect();
    let tasks = role_keys.into_iter().map(|role_key| {
        let client = client.clone();
        async move {
            let rules = match &role_key.0 {
                PolicyRoleKind::Role => {
                    let namespace = role_key
                        .1
                        .as_deref()
                        .ok_or_else(|| "Role reference has no namespace".to_owned())?;
                    Api::<Role>::namespaced(client, namespace)
                        .get(&role_key.2)
                        .await
                        .map_err(|error| error.to_string())?
                        .rules
                        .unwrap_or_default()
                }
                PolicyRoleKind::ClusterRole => Api::<ClusterRole>::all(client)
                    .get(&role_key.2)
                    .await
                    .map_err(|error| error.to_string())?
                    .rules
                    .unwrap_or_default(),
            };
            Ok::<_, String>((role_key, rules))
        }
    });
    let mut resolved = BTreeMap::new();
    for result in futures::stream::iter(tasks)
        .buffer_unordered(16)
        .collect::<Vec<_>>()
        .await
    {
        let (role_key, rules) = result?;
        resolved.insert(role_key, rules);
    }
    references
        .into_iter()
        .map(|reference| {
            let role_key = (
                reference.role_kind,
                reference.role_namespace,
                reference.role_name,
            );
            let rules = resolved.get(&role_key).cloned().ok_or_else(|| {
                "Resolved role disappeared from the authorization cache".to_owned()
            })?;
            Ok(PolicyRules {
                scope: reference.scope,
                binding: reference.binding,
                rules,
            })
        })
        .collect()
}

fn policy_rows(sources: Vec<PolicyRules>) -> Vec<Arc<ResourceRow>> {
    let mut policies: BTreeMap<(String, String, String, String), BTreeSet<String>> =
        BTreeMap::new();
    for source in sources {
        for rule in &source.rules {
            let mut groups: Vec<_> = rule
                .api_groups
                .as_deref()
                .unwrap_or_default()
                .iter()
                .map(String::as_str)
                .collect();
            if groups.is_empty() {
                groups.push("");
            }
            for group in groups {
                let display_group = if group.is_empty() {
                    "core".to_owned()
                } else {
                    group.to_owned()
                };
                for resource in rule.resources.as_deref().unwrap_or_default() {
                    let resource_names = rule.resource_names.as_deref().unwrap_or_default();
                    if resource_names.is_empty() {
                        insert_policy(
                            &mut policies,
                            &source,
                            &display_group,
                            resource,
                            &rule.verbs,
                        );
                    } else {
                        for name in resource_names {
                            insert_policy(
                                &mut policies,
                                &source,
                                &display_group,
                                &format!("{resource}/{name}"),
                                &rule.verbs,
                            );
                        }
                    }
                }
            }
            for url in rule.non_resource_urls.as_deref().unwrap_or_default() {
                insert_policy(&mut policies, &source, "-", url, &rule.verbs);
            }
        }
    }
    policies
        .into_iter()
        .map(|((scope, binding, group, resource), verbs)| {
            let extras = verbs
                .iter()
                .filter(|verb| {
                    !matches!(
                        verb.as_str(),
                        "*" | "get"
                            | "list"
                            | "watch"
                            | "create"
                            | "patch"
                            | "update"
                            | "delete"
                            | "deletecollection"
                    )
                })
                .cloned()
                .collect::<Vec<_>>()
                .join(",");
            let key = format!("{scope}/{group}/{resource}/{binding}");
            let mut row = ResourceRow::new(scope, resource, group, binding, extras, 0)
                .with_policy_data(PolicyData {
                    verbs: verbs.into_iter().map(Arc::from).collect(),
                });
            row.key = Arc::from(key);
            Arc::new(row)
        })
        .collect()
}

fn insert_policy(
    policies: &mut BTreeMap<(String, String, String, String), BTreeSet<String>>,
    source: &PolicyRules,
    group: &str,
    resource: &str,
    verbs: &[String],
) {
    policies
        .entry((
            source.scope.clone(),
            source.binding.clone(),
            group.to_owned(),
            resource.to_owned(),
        ))
        .or_default()
        .extend(verbs.iter().map(|verb| verb.to_ascii_lowercase()));
}

async fn load_containers(client: Client, target: ResourceTarget) -> Result<ActionResult, String> {
    if target.kind.known_kind() != Some(ResourceKind::Pods) {
        return Err("Container navigation is supported for pods only".to_owned());
    }
    let pod = namespaced_target_api::<Pod>(client, &target)?
        .get(&target.name)
        .await
        .map_err(|error| error.to_string())?;
    let rows = container_rows(&pod).into_iter().map(Arc::new).collect();
    Ok(ActionResult::Containers { target, rows })
}

async fn fetch_container_logs(
    client: Client,
    target: ContainerTarget,
    previous: bool,
) -> Result<ActionResult, String> {
    let body = Api::<Pod>::namespaced(client, &target.namespace)
        .logs(
            &target.pod,
            &pod_log_params(Some(target.container.clone()), previous),
        )
        .await
        .map_err(|error| error.to_string())?;
    let label = if previous { "Previous Logs" } else { "Logs" };
    Ok(ActionResult::Content {
        title: format!(
            "{label}({}/{}/{})",
            target.namespace, target.pod, target.container
        ),
        body,
        managed_fields: None,
        decoded_body: None,
        decoded_managed_fields: None,
        content: if previous {
            ContentKind::PreviousLogs
        } else {
            ContentKind::Logs
        },
    })
}

fn pod_log_params(container: Option<String>, previous: bool) -> LogParams {
    LogParams {
        container,
        previous,
        tail_lines: Some(500),
        ..LogParams::default()
    }
}

fn container_rows(pod: &Pod) -> Vec<ResourceRow> {
    let Some(spec) = &pod.spec else {
        return Vec::new();
    };
    let status = pod.status.as_ref();
    let namespace = pod.namespace().unwrap_or_default();
    let created_unix = pod
        .meta()
        .creation_timestamp
        .as_ref()
        .map_or(0, |time| time.0.as_second());
    let mut rows = Vec::new();
    for container in &spec.containers {
        rows.push(container_row(
            &namespace,
            &container.name,
            container.image.as_deref().unwrap_or("-"),
            "container",
            status.and_then(|status| status.container_statuses.as_deref()),
            created_unix,
        ));
    }
    for container in spec.init_containers.as_deref().unwrap_or_default() {
        rows.push(container_row(
            &namespace,
            &container.name,
            container.image.as_deref().unwrap_or("-"),
            "init",
            status.and_then(|status| status.init_container_statuses.as_deref()),
            created_unix,
        ));
    }
    for container in spec.ephemeral_containers.as_deref().unwrap_or_default() {
        rows.push(container_row(
            &namespace,
            &container.name,
            container.image.as_deref().unwrap_or("-"),
            "ephemeral",
            status.and_then(|status| status.ephemeral_container_statuses.as_deref()),
            created_unix,
        ));
    }
    rows.sort_unstable_by(|left, right| left.name.cmp(&right.name));
    rows
}

fn container_row(
    namespace: &str,
    name: &str,
    image: &str,
    kind: &str,
    statuses: Option<&[ContainerStatus]>,
    created_unix: i64,
) -> ResourceRow {
    let status = statuses.and_then(|statuses| statuses.iter().find(|status| status.name == name));
    let ready = status.is_some_and(|status| status.ready);
    let restarts = status.map_or(0, |status| status.restart_count.max(0));
    ResourceRow::new(
        namespace,
        name,
        status.map_or_else(|| "Unknown".to_owned(), container_status),
        if ready { "true" } else { "false" },
        image,
        created_unix,
    )
    .with_container_data(ContainerData {
        image: Arc::from(image),
        kind: Arc::from(kind),
        restarts: u32::try_from(restarts).unwrap_or(0),
    })
}

fn container_status(status: &ContainerStatus) -> String {
    let Some(state) = &status.state else {
        return "Unknown".to_owned();
    };
    if let Some(waiting) = &state.waiting {
        return waiting
            .reason
            .clone()
            .unwrap_or_else(|| "Waiting".to_owned());
    }
    if let Some(terminated) = &state.terminated {
        return terminated
            .reason
            .clone()
            .filter(|reason| !reason.is_empty())
            .unwrap_or_else(|| format!("ExitCode:{}", terminated.exit_code));
    }
    if state.running.is_some() {
        "Running".to_owned()
    } else {
        "Unknown".to_owned()
    }
}

async fn yaml_from(
    api: Api<DynamicObject>,
    target: &ResourceTarget,
) -> Result<YamlContent, String> {
    let object = api
        .get(&target.name)
        .await
        .map_err(|error| error.to_string())?;
    yaml_content_from_object(object, target)
}

fn yaml_content_from_object(
    mut object: DynamicObject,
    target: &ResourceTarget,
) -> Result<YamlContent, String> {
    let mut decoded = is_core_v1_secret(target)
        .then(|| decode_secret_object(&object))
        .flatten();
    let has_managed_fields = object
        .meta()
        .managed_fields
        .as_ref()
        .is_some_and(|fields| !fields.is_empty());
    let with_managed_fields = has_managed_fields
        .then(|| serde_yaml::to_string(&object))
        .transpose()
        .map_err(|error| error.to_string())?;
    let decoded_with_managed_fields = if has_managed_fields {
        decoded
            .as_ref()
            .map(serde_yaml::to_string)
            .transpose()
            .map_err(|error| error.to_string())?
    } else {
        None
    };
    object.meta_mut().managed_fields = None;
    if let Some(decoded) = &mut decoded {
        decoded.meta_mut().managed_fields = None;
    }
    let body = serde_yaml::to_string(&object).map_err(|error| error.to_string())?;
    let decoded_body = decoded
        .as_ref()
        .map(serde_yaml::to_string)
        .transpose()
        .map_err(|error| error.to_string())?;
    Ok(YamlContent {
        body,
        with_managed_fields,
        decoded_body,
        decoded_with_managed_fields,
    })
}

fn is_core_v1_secret(target: &ResourceTarget) -> bool {
    target.kind.is_core_v1_secret()
}

fn decode_secret_object(object: &DynamicObject) -> Option<DynamicObject> {
    let values = decode_secret_data(object).ok()?;
    let mut decoded = object.clone();
    let Some(root) = decoded.data.as_object_mut() else {
        return Some(decoded);
    };
    root.insert(
        "data".to_owned(),
        serde_json::Value::Object(
            values
                .into_iter()
                .map(|(key, value)| (key, serde_json::Value::String(value)))
                .collect(),
        ),
    );
    Some(decoded)
}

fn decode_secret_data(object: &DynamicObject) -> Result<BTreeMap<String, String>, String> {
    let Some(root) = object.data.as_object() else {
        return Ok(BTreeMap::new());
    };
    let Some(data) = root.get("data") else {
        return Ok(BTreeMap::new());
    };
    let Some(data) = data.as_object() else {
        return Ok(BTreeMap::new());
    };
    data.iter()
        .map(|(key, value)| {
            let encoded = value
                .as_str()
                .ok_or_else(|| format!("Secret data key {key:?} is not base64 text"))?;
            let bytes = BASE64_STANDARD
                .decode(encoded)
                .map_err(|error| format!("Unable to decode Secret data key {key:?}: {error}"))?;
            Ok((key.clone(), decoded_secret_value(&bytes)))
        })
        .collect()
}

fn decoded_secret_value(bytes: &[u8]) -> String {
    if let Ok(value) = std::str::from_utf8(bytes) {
        return value.to_owned();
    }
    let mut value = String::with_capacity(2 + bytes.len() * 2);
    value.push_str("0x");
    for byte in bytes {
        let _ = write!(value, "{byte:02x}");
    }
    value
}

async fn watch_api<K, F>(
    api: Api<K>,
    client: Client,
    settings: KubernetesSettings,
    request: WatchRequest,
    snapshots: watch::Sender<ResourceSnapshot>,
    map_row: F,
) where
    K: Clone + Resource + DeserializeOwned + Debug + Send + 'static,
    F: Fn(&K) -> ResourceRow + Copy + Send + 'static,
{
    let (enrichment_sender, mut enrichment_updates) = mpsc::channel(8);
    let _enrichment_tasks =
        spawn_enrichment_tasks(client, &request, &settings, enrichment_sender.clone());
    let _enrichment_sender = enrichment_sender;
    let mut enrichment = EnrichmentState::default();
    let watcher_config = resource_watcher_config(&settings, &request);
    let stream = watcher::watcher(api, watcher_config).default_backoff();
    tokio::pin!(stream);

    let mut rows: FxHashMap<Arc<str>, Arc<ResourceRow>> = FxHashMap::default();
    let mut initializing: Option<FxHashMap<Arc<str>, Arc<ResourceRow>>> = None;
    let mut initial_load_complete = false;
    let mut initial_progress = InitialListProgress::default();
    let mut revision = 0_u64;
    let mut dirty = false;
    let mut ticker = tokio::time::interval(settings.publish_interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            event = stream.next() => {
                match event {
                    Some(Ok(watcher::Event::Init)) => {
                        initializing = Some(FxHashMap::default());
                        initial_progress = InitialListProgress::default();
                    }
                    Some(Ok(watcher::Event::InitApply(object))) => {
                        let mut row = map_row(&object);
                        enrichment.apply(&mut row);
                        apply_initial_row(
                            &mut initializing,
                            Arc::new(row),
                            initial_load_complete,
                            &mut initial_progress,
                            &snapshots,
                            &request,
                            &mut revision,
                        );
                    }
                    Some(Ok(watcher::Event::InitDone)) => {
                        if let Some(initial) = initializing.take() {
                            rows = initial;
                        }
                        initial_load_complete = true;
                        revision = revision.wrapping_add(1);
                        publish(&snapshots, &request, &rows, SourceState::Ready, revision);
                        dirty = false;
                    }
                    Some(Ok(watcher::Event::Apply(object))) => {
                        let mut row = map_row(&object);
                        enrichment.apply(&mut row);
                        let row = Arc::new(row);
                        rows.insert(Arc::clone(&row.key), row);
                        dirty = true;
                    }
                    Some(Ok(watcher::Event::Delete(object))) => {
                        let row = map_row(&object);
                        rows.remove(&row.key);
                        dirty = true;
                    }
                    Some(Err(error)) => {
                        publish_watch_error(
                            &snapshots,
                            &request,
                            &rows,
                            initializing.as_ref(),
                            initial_load_complete,
                            Arc::from(error.to_string()),
                            &mut revision,
                        );
                    }
                    None => {
                        publish_watch_error(
                            &snapshots,
                            &request,
                            &rows,
                            initializing.as_ref(),
                            initial_load_complete,
                            Arc::from("Kubernetes watch ended"),
                            &mut revision,
                        );
                        return;
                    }
                }
            }
            Some(update) = enrichment_updates.recv() => {
                enrichment.update(update);
                enrichment.apply_all(&mut rows);
                if let Some(initializing) = &mut initializing {
                    enrichment.apply_all(initializing);
                }
                dirty = true;
            }
            _ = ticker.tick(), if dirty && initializing.is_none() => {
                revision = revision.wrapping_add(1);
                publish(&snapshots, &request, &rows, SourceState::Ready, revision);
                dirty = false;
            }
        }
    }
}

fn resource_watcher_config(
    settings: &KubernetesSettings,
    request: &WatchRequest,
) -> watcher::Config {
    let mut config = if settings.streaming_lists && request.kind.known_kind().is_some() {
        watcher::Config::default().streaming_lists()
    } else {
        watcher::Config::default().page_size(500)
    };
    if let Some(selector) = &request.field_selector {
        config = config.fields(selector);
    }
    if let Some(selector) = &request.label_selector {
        config = config.labels(selector);
    }
    config
}

fn apply_initial_row(
    initializing: &mut Option<FxHashMap<Arc<str>, Arc<ResourceRow>>>,
    row: Arc<ResourceRow>,
    initial_load_complete: bool,
    progress: &mut InitialListProgress,
    snapshots: &watch::Sender<ResourceSnapshot>,
    request: &WatchRequest,
    revision: &mut u64,
) {
    let initial = initializing.get_or_insert_with(FxHashMap::default);
    initial.insert(Arc::clone(&row.key), row);
    if initial_load_complete || !progress.should_publish(initial.len()) {
        return;
    }
    *revision = revision.wrapping_add(1);
    publish(
        snapshots,
        request,
        initial,
        SourceState::Loading {
            loaded: initial.len(),
        },
        *revision,
    );
}

fn publish_watch_error(
    snapshots: &watch::Sender<ResourceSnapshot>,
    request: &WatchRequest,
    rows: &FxHashMap<Arc<str>, Arc<ResourceRow>>,
    initializing: Option<&FxHashMap<Arc<str>, Arc<ResourceRow>>>,
    initial_load_complete: bool,
    error: Arc<str>,
    revision: &mut u64,
) {
    *revision = revision.wrapping_add(1);
    let visible = if initial_load_complete {
        rows
    } else {
        initializing.unwrap_or(rows)
    };
    publish(
        snapshots,
        request,
        visible,
        SourceState::Error(error),
        *revision,
    );
}

const INITIAL_LIST_PROGRESS_ROWS: usize = 250;
const LIST_PROGRESS_BATCH_ROWS: usize = 5_000;

#[derive(Debug)]
struct InitialListProgress {
    next_rows: usize,
}

impl Default for InitialListProgress {
    fn default() -> Self {
        Self {
            next_rows: INITIAL_LIST_PROGRESS_ROWS,
        }
    }
}

impl InitialListProgress {
    fn should_publish(&mut self, loaded: usize) -> bool {
        if loaded < self.next_rows {
            return false;
        }
        self.next_rows = loaded.saturating_add(LIST_PROGRESS_BATCH_ROWS);
        true
    }
}

const TABLE_ACCEPT: &str = "application/json;as=Table;v=v1;g=meta.k8s.io,\
                           application/json;as=Table;v=v1beta1;g=meta.k8s.io,\
                           application/json";
const TABLE_INITIAL_PAGE_SIZE: u32 = 250;
const TABLE_MAX_PAGE_SIZE: u32 = 5_000;
const TABLE_INCLUDE_OBJECT: &str = "Metadata";

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ServerTableMetadata {
    resource_version: Option<String>,
    #[serde(rename = "continue")]
    continue_token: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ServerTableColumn {
    #[serde(default, deserialize_with = "deserialize_null_default")]
    name: String,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    format: String,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    priority: i32,
}

#[derive(Debug, Clone, Deserialize)]
struct ServerTableRow {
    #[serde(default, deserialize_with = "deserialize_null_default")]
    cells: Vec<serde_json::Value>,
    object: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ServerTable {
    #[serde(default, deserialize_with = "deserialize_null_default")]
    metadata: ServerTableMetadata,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    column_definitions: Vec<ServerTableColumn>,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    rows: Vec<ServerTableRow>,
}

#[derive(Debug, Deserialize)]
struct DynamicObjectList {
    #[serde(default, deserialize_with = "deserialize_null_default")]
    metadata: ServerTableMetadata,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    items: Vec<DynamicObject>,
}

fn deserialize_null_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Option::<T>::deserialize(deserializer).map(Option::unwrap_or_default)
}

struct TableListing {
    columns: Arc<[TableColumn]>,
    rows: FxHashMap<Arc<str>, Arc<ResourceRow>>,
    resource_version: String,
}

async fn watch_table_api(
    api: Api<DynamicObject>,
    client: Client,
    settings: KubernetesSettings,
    request: WatchRequest,
    snapshots: watch::Sender<ResourceSnapshot>,
) {
    let mut revision = 0_u64;
    let mut initial_load_complete = false;
    let mut retry_delay = Duration::from_secs(1);
    loop {
        let mut initial_progress = InitialListProgress::default();
        let mut partial_columns: Arc<[TableColumn]> = Arc::from([]);
        let mut partial_rows = FxHashMap::default();
        match list_table_pages_with_progress(
            &api,
            &client,
            request.field_selector.as_deref(),
            request.label_selector.as_deref(),
            |columns, rows, complete| {
                if initial_load_complete || complete || !initial_progress.should_publish(rows.len())
                {
                    return;
                }
                partial_columns = Arc::clone(columns);
                partial_rows.clone_from(rows);
                revision = revision.wrapping_add(1);
                publish_table(
                    &snapshots,
                    &request,
                    rows,
                    Arc::clone(columns),
                    SourceState::Loading { loaded: rows.len() },
                    revision,
                );
            },
        )
        .await
        {
            Ok(mut listing) => {
                drop(partial_rows);
                drop(partial_columns);
                initial_load_complete = true;
                revision = revision.wrapping_add(1);
                publish_table(
                    &snapshots,
                    &request,
                    &listing.rows,
                    Arc::clone(&listing.columns),
                    SourceState::Ready,
                    revision,
                );
                retry_delay = Duration::from_secs(1);
                match stream_table_watch(
                    &api,
                    &client,
                    &settings,
                    &request,
                    &snapshots,
                    &mut listing,
                    &mut revision,
                )
                .await
                {
                    Ok(()) => {}
                    Err(error) => {
                        revision = revision.wrapping_add(1);
                        publish_table(
                            &snapshots,
                            &request,
                            &listing.rows,
                            Arc::clone(&listing.columns),
                            SourceState::Error(Arc::from(error)),
                            revision,
                        );
                    }
                }
            }
            Err(error) => {
                revision = revision.wrapping_add(1);
                publish_table(
                    &snapshots,
                    &request,
                    &partial_rows,
                    partial_columns,
                    SourceState::Error(Arc::from(error)),
                    revision,
                );
            }
        }
        tokio::time::sleep(retry_delay).await;
        retry_delay = (retry_delay * 2).min(Duration::from_secs(30));
    }
}

async fn stream_table_watch(
    api: &Api<DynamicObject>,
    client: &Client,
    settings: &KubernetesSettings,
    request: &WatchRequest,
    snapshots: &watch::Sender<ResourceSnapshot>,
    listing: &mut TableListing,
    revision: &mut u64,
) -> Result<(), String> {
    let mut params = WatchParams {
        bookmarks: false,
        ..WatchParams::default()
    };
    if let Some(selector) = &request.field_selector {
        params = params.fields(selector);
    }
    if let Some(selector) = &request.label_selector {
        params = params.labels(selector);
    }
    let raw = Request::new(api.resource_url())
        .watch(&params, &listing.resource_version)
        .map_err(|error| format!("Unable to build Kubernetes Table watch: {error}"))?;
    let raw = table_request(raw)?;
    let stream = client
        .request_events::<serde_json::Value>(raw)
        .await
        .map_err(|error| format!("Unable to start Kubernetes Table watch: {error}"))?;
    tokio::pin!(stream);

    let mut dirty = false;
    let mut ticker = tokio::time::interval(settings.publish_interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            event = stream.next() => {
                match event {
                    Some(Ok(WatchEvent::Added(value) | WatchEvent::Modified(value))) => {
                        let update = decode_table_watch_object(value, &listing.columns)?;
                        if update
                            .rows
                            .first()
                            .is_some_and(|row| row.table_cells.is_empty())
                        {
                            listing.columns = Arc::from([]);
                        } else if !update.columns.is_empty() {
                            listing.columns = update.columns;
                        }
                        if !update.resource_version.is_empty() {
                            listing.resource_version = update.resource_version;
                        }
                        for row in update.rows {
                            listing.rows.insert(Arc::clone(&row.key), row);
                        }
                        dirty = true;
                    }
                    Some(Ok(WatchEvent::Deleted(value))) => {
                        let update = decode_table_watch_object(value, &listing.columns)?;
                        if !update.resource_version.is_empty() {
                            listing.resource_version = update.resource_version;
                        }
                        for row in update.rows {
                            listing.rows.remove(&row.key);
                        }
                        dirty = true;
                    }
                    Some(Ok(WatchEvent::Bookmark(bookmark))) => {
                        listing.resource_version = bookmark.metadata.resource_version;
                    }
                    Some(Ok(WatchEvent::Error(error))) => {
                        return Err(error.to_string());
                    }
                    Some(Err(error)) => {
                        return Err(error.to_string());
                    }
                    None => return Ok(()),
                }
            }
            _ = ticker.tick(), if dirty => {
                *revision = revision.wrapping_add(1);
                publish_table(
                    snapshots,
                    request,
                    &listing.rows,
                    Arc::clone(&listing.columns),
                    SourceState::Ready,
                    *revision,
                );
                dirty = false;
            }
        }
    }
}

async fn poll_table_api(
    api: Api<DynamicObject>,
    client: Client,
    settings: KubernetesSettings,
    request: WatchRequest,
    snapshots: watch::Sender<ResourceSnapshot>,
) {
    let mut revision = 0_u64;
    let mut initial_load_complete = false;
    let poll_interval = settings.publish_interval.max(Duration::from_secs(5));
    let mut ticker = tokio::time::interval(poll_interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        ticker.tick().await;
        let mut initial_progress = InitialListProgress::default();
        let mut partial_columns: Arc<[TableColumn]> = Arc::from([]);
        let mut partial_rows = FxHashMap::default();
        match list_table_pages_with_progress(
            &api,
            &client,
            request.field_selector.as_deref(),
            request.label_selector.as_deref(),
            |columns, rows, complete| {
                if initial_load_complete || complete || !initial_progress.should_publish(rows.len())
                {
                    return;
                }
                partial_columns = Arc::clone(columns);
                partial_rows.clone_from(rows);
                revision = revision.wrapping_add(1);
                publish_table(
                    &snapshots,
                    &request,
                    rows,
                    Arc::clone(columns),
                    SourceState::Loading { loaded: rows.len() },
                    revision,
                );
            },
        )
        .await
        {
            Ok(listing) => {
                initial_load_complete = true;
                revision = revision.wrapping_add(1);
                publish_table(
                    &snapshots,
                    &request,
                    &listing.rows,
                    listing.columns,
                    SourceState::Ready,
                    revision,
                );
            }
            Err(error) => {
                revision = revision.wrapping_add(1);
                publish_table(
                    &snapshots,
                    &request,
                    &partial_rows,
                    partial_columns,
                    SourceState::Error(Arc::from(error)),
                    revision,
                );
            }
        }
    }
}

async fn list_table_pages_with_progress<F>(
    api: &Api<DynamicObject>,
    client: &Client,
    field_selector: Option<&str>,
    label_selector: Option<&str>,
    mut on_page: F,
) -> Result<TableListing, String>
where
    F: FnMut(&Arc<[TableColumn]>, &FxHashMap<Arc<str>, Arc<ResourceRow>>, bool),
{
    let mut columns: Arc<[TableColumn]> = Arc::from([]);
    let mut rows = FxHashMap::default();
    let mut resource_version = String::new();
    let mut continue_token: Option<String> = None;
    let mut page_size = TABLE_INITIAL_PAGE_SIZE;
    loop {
        let mut params = ListParams::default().limit(page_size);
        if let Some(selector) = field_selector {
            params = params.fields(selector);
        }
        if let Some(selector) = label_selector {
            params = params.labels(selector);
        }
        if let Some(token) = &continue_token {
            params = params.continue_token(token);
        }
        let raw = Request::new(api.resource_url())
            .list(&params)
            .map_err(|error| format!("Unable to build Kubernetes Table request: {error}"))?;
        let raw = table_request(raw)?;
        let value = client
            .request::<serde_json::Value>(raw)
            .await
            .map_err(|error| format!("Unable to list Kubernetes Table rows: {error}"))?;
        let page = decode_table_list(value)?;
        if columns.is_empty() && !page.columns.is_empty() {
            columns = Arc::clone(&page.columns);
        }
        if !page.resource_version.is_empty() {
            resource_version = page.resource_version;
        }
        for row in page.rows {
            rows.insert(Arc::clone(&row.key), row);
        }
        continue_token = page.continue_token;
        let complete = continue_token.as_deref().is_none_or(str::is_empty);
        on_page(&columns, &rows, complete);
        if complete {
            return Ok(TableListing {
                columns,
                rows,
                resource_version,
            });
        }
        page_size = next_table_page_size(page_size);
    }
}

fn next_table_page_size(current: u32) -> u32 {
    if current < TABLE_MAX_PAGE_SIZE {
        TABLE_MAX_PAGE_SIZE
    } else {
        current
    }
}

fn table_request(mut request: HttpRequest<Vec<u8>>) -> Result<HttpRequest<Vec<u8>>, String> {
    request
        .headers_mut()
        .insert(ACCEPT, HeaderValue::from_static(TABLE_ACCEPT));
    let separator = if request.uri().query().is_some() {
        if request.uri().to_string().ends_with('?') {
            ""
        } else {
            "&"
        }
    } else {
        "?"
    };
    let uri = format!(
        "{}{separator}includeObject={TABLE_INCLUDE_OBJECT}",
        request.uri()
    )
    .parse()
    .map_err(|error| format!("Unable to add Kubernetes Table options: {error}"))?;
    *request.uri_mut() = uri;
    Ok(request)
}

struct DecodedTablePage {
    columns: Arc<[TableColumn]>,
    rows: Vec<Arc<ResourceRow>>,
    resource_version: String,
    continue_token: Option<String>,
}

fn decode_table_list(value: serde_json::Value) -> Result<DecodedTablePage, String> {
    if value.get("kind").and_then(serde_json::Value::as_str) == Some("Table") {
        let table: ServerTable = serde_json::from_value(value)
            .map_err(|error| format!("Unable to decode Kubernetes Table: {error}"))?;
        return Ok(decode_server_table(table, &[]));
    }
    let list: DynamicObjectList = serde_json::from_value(value)
        .map_err(|error| format!("Unable to decode Kubernetes resource list: {error}"))?;
    Ok(DecodedTablePage {
        columns: Arc::from([]),
        rows: list.items.iter().map(dynamic_row).map(Arc::new).collect(),
        resource_version: list.metadata.resource_version.unwrap_or_default(),
        continue_token: list.metadata.continue_token,
    })
}

fn decode_table_watch_object(
    value: serde_json::Value,
    previous_columns: &[TableColumn],
) -> Result<DecodedTablePage, String> {
    if value.get("kind").and_then(serde_json::Value::as_str) == Some("Table") {
        let table: ServerTable = serde_json::from_value(value)
            .map_err(|error| format!("Unable to decode Kubernetes Table event: {error}"))?;
        return Ok(decode_server_table(table, previous_columns));
    }
    let object: DynamicObject = serde_json::from_value(value)
        .map_err(|error| format!("Unable to decode Kubernetes watch object: {error}"))?;
    let resource_version = object.resource_version().unwrap_or_default();
    let row = dynamic_row(&object);
    Ok(DecodedTablePage {
        columns: Arc::from([]),
        rows: vec![Arc::new(row)],
        resource_version,
        continue_token: None,
    })
}

fn decode_server_table(table: ServerTable, previous_columns: &[TableColumn]) -> DecodedTablePage {
    let resource_version = table
        .metadata
        .resource_version
        .clone()
        .or_else(|| {
            table.rows.iter().find_map(|row| {
                row.object
                    .as_ref()?
                    .pointer("/metadata/resourceVersion")?
                    .as_str()
                    .map(str::to_owned)
            })
        })
        .unwrap_or_default();
    let columns: Arc<[TableColumn]> = if table.column_definitions.is_empty() {
        Arc::from(previous_columns)
    } else {
        table
            .column_definitions
            .iter()
            .map(|column| TableColumn {
                name: Arc::from(column.name.as_str()),
                format: Arc::from(column.format.as_str()),
                priority: column.priority,
            })
            .collect::<Vec<_>>()
            .into()
    };
    let rows = table
        .rows
        .into_iter()
        .map(|row| Arc::new(server_table_row(row, &columns)))
        .collect();
    DecodedTablePage {
        columns,
        rows,
        resource_version,
        continue_token: table.metadata.continue_token,
    }
}

fn server_table_row(row: ServerTableRow, columns: &[TableColumn]) -> ResourceRow {
    let object = row
        .object
        .and_then(|object| serde_json::from_value::<DynamicObject>(object).ok());
    let cells: Vec<TableCell> = row
        .cells
        .iter()
        .enumerate()
        .map(|(index, value)| table_cell(value, columns.get(index)))
        .collect();
    let column_text = |name: &str| {
        columns
            .iter()
            .position(|column| column.name.eq_ignore_ascii_case(name))
            .and_then(|index| cells.get(index))
            .map(|cell| cell.text.to_string())
    };
    let namespace = object
        .as_ref()
        .and_then(ResourceExt::namespace)
        .or_else(|| column_text("namespace"))
        .unwrap_or_else(|| "-".to_owned());
    let name = object
        .as_ref()
        .map(ResourceExt::name_any)
        .filter(|name| !name.is_empty())
        .or_else(|| column_text("name"))
        .unwrap_or_else(|| "<unknown>".to_owned());
    let status = column_text("status").unwrap_or_else(|| "-".to_owned());
    let ready = column_text("ready").unwrap_or_else(|| "-".to_owned());
    let details = column_text("details").unwrap_or_else(|| "-".to_owned());
    let created_unix = object
        .as_ref()
        .and_then(|object| object.meta().creation_timestamp.as_ref())
        .map(|time| time.0.as_second())
        .or_else(|| cells.iter().find_map(|cell| cell.timestamp_unix))
        .unwrap_or(0);
    let labels = object
        .as_ref()
        .and_then(|object| object.meta().labels.as_ref())
        .map(|labels| {
            labels
                .iter()
                .map(|(key, value)| format!("{key}={value}"))
                .collect::<Vec<_>>()
                .join(",")
        })
        .unwrap_or_default();
    let owner = object
        .as_ref()
        .and_then(|object| owner_reference_data(object.meta()));
    let row = ResourceRow::new(namespace, name, status, ready, details, created_unix)
        .with_labels(labels)
        .with_table_cells(cells);
    match owner {
        Some(owner) => row.with_owner_reference_data(owner),
        None => row,
    }
}

fn table_cell(value: &serde_json::Value, column: Option<&TableColumn>) -> TableCell {
    let timestamp_unix = column
        .filter(|column| {
            column.format.as_ref() == "date"
                || column.name.eq_ignore_ascii_case("created at")
                || column.name.eq_ignore_ascii_case("creation timestamp")
        })
        .and_then(|_| serde_json::from_value::<Time>(value.clone()).ok())
        .map(|time| time.0.as_second());
    let text = match value {
        serde_json::Value::Null => String::new(),
        serde_json::Value::String(value) => kubectl_table_string(value),
        serde_json::Value::Bool(value) => value.to_string(),
        serde_json::Value::Number(value) => value.to_string(),
        serde_json::Value::Array(values) => format!(
            "[{}]",
            values
                .iter()
                .map(table_scalar_text)
                .collect::<Vec<_>>()
                .join(" ")
        ),
        serde_json::Value::Object(_) => serde_json::to_string(value).unwrap_or_default(),
    };
    TableCell {
        text: Arc::from(text),
        timestamp_unix,
    }
}

fn table_scalar_text(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Null => "<nil>".to_owned(),
        serde_json::Value::String(value) => kubectl_table_string(value),
        _ => value.to_string(),
    }
}

fn kubectl_table_string(value: &str) -> String {
    value.find(['\u{c}', '\n', '\r']).map_or_else(
        || value.to_owned(),
        |breakpoint| format!("{}...", &value[..breakpoint]),
    )
}

fn publish(
    snapshots: &watch::Sender<ResourceSnapshot>,
    request: &WatchRequest,
    rows: &FxHashMap<Arc<str>, Arc<ResourceRow>>,
    state: SourceState,
    revision: u64,
) {
    if snapshots.is_closed() {
        return;
    }
    let mut compact: Vec<_> = rows.values().cloned().collect();
    compact.sort_unstable_by(|left, right| {
        left.namespace
            .cmp(&right.namespace)
            .then(left.name.cmp(&right.name))
    });
    let _ = snapshots.send(ResourceSnapshot {
        generation: request.generation,
        kind: request.kind.clone(),
        rows: Arc::new(compact),
        table_columns: Arc::from([]),
        state,
        revision,
        emitted_at: Instant::now(),
    });
}

fn publish_table(
    snapshots: &watch::Sender<ResourceSnapshot>,
    request: &WatchRequest,
    rows: &FxHashMap<Arc<str>, Arc<ResourceRow>>,
    table_columns: Arc<[TableColumn]>,
    state: SourceState,
    revision: u64,
) {
    if snapshots.is_closed() {
        return;
    }
    let mut compact: Vec<_> = rows.values().cloned().collect();
    compact.sort_unstable_by(|left, right| {
        left.namespace
            .cmp(&right.namespace)
            .then(left.name.cmp(&right.name))
    });
    let _ = snapshots.send(ResourceSnapshot {
        generation: request.generation,
        kind: request.kind.clone(),
        rows: Arc::new(compact),
        table_columns,
        state,
        revision,
        emitted_at: Instant::now(),
    });
}

fn pod_row(pod: &Pod) -> ResourceRow {
    let namespace = pod.namespace().unwrap_or_default();
    let name = pod.name_any();
    let deleting = pod.meta().deletion_timestamp.is_some();
    let status = pod.status.as_ref();
    let container_statuses = status.and_then(|status| status.container_statuses.as_ref());
    let total = pod.spec.as_ref().map_or(0, |spec| spec.containers.len());
    let ready = container_statuses.map_or(0, |statuses| {
        statuses.iter().filter(|container| container.ready).count()
    });
    let restarts: i32 = container_statuses.map_or(0, |statuses| {
        statuses
            .iter()
            .map(|container| container.restart_count)
            .sum()
    });
    let phase = if deleting {
        "Terminating".to_owned()
    } else {
        status
            .and_then(|status| status.phase.clone())
            .unwrap_or_else(|| "Unknown".to_owned())
    };
    let (cpu_request_millis, cpu_limit_millis, memory_request_bytes, memory_limit_bytes) =
        pod_resource_totals(pod);
    let node = pod
        .spec
        .as_ref()
        .and_then(|spec| spec.node_name.clone())
        .unwrap_or_else(|| "-".to_owned());
    let default_container = pod_default_container(pod).unwrap_or_default();
    resource_row(
        pod,
        namespace,
        name,
        phase,
        format!("{ready}/{total}"),
        restarts.to_string(),
    )
    .with_pod_data(PodData {
        restarts: u32::try_from(restarts.max(0)).unwrap_or(0),
        node: Arc::from(node),
        default_container: Arc::from(default_container),
        cpu_millis: None,
        cpu_request_millis,
        cpu_limit_millis,
        memory_bytes: None,
        memory_request_bytes,
        memory_limit_bytes,
    })
    .with_port_forward_ports(pod_forward_ports(pod))
}

fn pod_forward_ports(pod: &Pod) -> Vec<PortForwardPort> {
    pod.spec
        .iter()
        .flat_map(|spec| &spec.containers)
        .flat_map(|container| {
            container.ports.iter().flatten().filter_map(|port| {
                let is_tcp = port.protocol.as_deref().unwrap_or("TCP") == "TCP";
                let port_number = u16::try_from(port.container_port).ok()?;
                (is_tcp && port_number > 0).then(|| PortForwardPort {
                    container: Some(Arc::from(container.name.as_str())),
                    name: Arc::from(port.name.as_deref().unwrap_or("")),
                    port: port_number,
                })
            })
        })
        .collect()
}

fn pod_default_container(pod: &Pod) -> Option<&str> {
    const DEFAULT_CONTAINER_ANNOTATION: &str = "kubectl.kubernetes.io/default-container";

    let spec = pod.spec.as_ref()?;
    let annotated = pod
        .metadata
        .annotations
        .as_ref()
        .and_then(|annotations| annotations.get(DEFAULT_CONTAINER_ANNOTATION))
        .map(String::as_str)
        .filter(|name| {
            spec.containers
                .iter()
                .any(|container| container.name == *name)
        });
    annotated.or_else(|| {
        spec.containers
            .first()
            .map(|container| container.name.as_str())
    })
}

fn deployment_row(deployment: &Deployment) -> ResourceRow {
    let desired = deployment
        .spec
        .as_ref()
        .and_then(|spec| spec.replicas)
        .unwrap_or(1);
    let ready = deployment
        .status
        .as_ref()
        .and_then(|status| status.ready_replicas)
        .unwrap_or(0);
    let available = deployment
        .status
        .as_ref()
        .and_then(|status| status.available_replicas)
        .unwrap_or(0);
    let row = resource_row(
        deployment,
        deployment.namespace().unwrap_or_default(),
        deployment.name_any(),
        workload_status(
            deployment.meta().deletion_timestamp.is_some(),
            ready,
            desired,
        ),
        format!("{ready}/{desired}"),
        format!("{available} available"),
    );
    match deployment.spec.as_ref() {
        Some(spec) => with_child_resource(
            row,
            replica_set_descriptor(),
            label_selector_string(&spec.selector),
        ),
        None => row,
    }
}

fn replica_set_row(replica_set: &ReplicaSet) -> ResourceRow {
    let desired = replica_set
        .spec
        .as_ref()
        .and_then(|spec| spec.replicas)
        .unwrap_or(1);
    let ready = replica_set
        .status
        .as_ref()
        .and_then(|status| status.ready_replicas)
        .unwrap_or(0);
    let row = resource_row(
        replica_set,
        replica_set.namespace().unwrap_or_default(),
        replica_set.name_any(),
        workload_status(
            replica_set.meta().deletion_timestamp.is_some(),
            ready,
            desired,
        ),
        format!("{ready}/{desired}"),
        replica_set
            .status
            .as_ref()
            .and_then(|status| status.available_replicas)
            .map_or_else(
                || "-".to_owned(),
                |available| format!("{available} available"),
            ),
    );
    match replica_set.spec.as_ref() {
        Some(spec) => with_child_resource(
            row,
            ResourceKind::Pods.into(),
            label_selector_string(&spec.selector),
        ),
        None => row,
    }
}

fn stateful_set_row(stateful_set: &StatefulSet) -> ResourceRow {
    let desired = stateful_set
        .spec
        .as_ref()
        .and_then(|spec| spec.replicas)
        .unwrap_or(1);
    let ready = stateful_set
        .status
        .as_ref()
        .and_then(|status| status.ready_replicas)
        .unwrap_or(0);
    let row = resource_row(
        stateful_set,
        stateful_set.namespace().unwrap_or_default(),
        stateful_set.name_any(),
        workload_status(
            stateful_set.meta().deletion_timestamp.is_some(),
            ready,
            desired,
        ),
        format!("{ready}/{desired}"),
        stateful_set
            .status
            .as_ref()
            .and_then(|status| status.current_revision.clone())
            .unwrap_or_else(|| "-".to_owned()),
    );
    match stateful_set.spec.as_ref() {
        Some(spec) => with_child_resource(
            row,
            ResourceKind::Pods.into(),
            label_selector_string(&spec.selector),
        ),
        None => row,
    }
}

fn daemon_set_row(daemon_set: &DaemonSet) -> ResourceRow {
    let desired = daemon_set
        .status
        .as_ref()
        .map_or(0, |status| status.desired_number_scheduled);
    let ready = daemon_set
        .status
        .as_ref()
        .map_or(0, |status| status.number_ready);
    let row = resource_row(
        daemon_set,
        daemon_set.namespace().unwrap_or_default(),
        daemon_set.name_any(),
        workload_status(
            daemon_set.meta().deletion_timestamp.is_some(),
            ready,
            desired,
        ),
        format!("{ready}/{desired}"),
        format!(
            "{} available",
            daemon_set
                .status
                .as_ref()
                .and_then(|status| status.number_available)
                .unwrap_or(0)
        ),
    );
    match daemon_set.spec.as_ref() {
        Some(spec) => with_child_resource(
            row,
            ResourceKind::Pods.into(),
            label_selector_string(&spec.selector),
        ),
        None => row,
    }
}

fn service_row(service: &Service) -> ResourceRow {
    let spec = service.spec.as_ref();
    let service_type = spec
        .and_then(|spec| spec.type_.clone())
        .unwrap_or_else(|| "ClusterIP".to_owned());
    let cluster_ip = spec
        .and_then(|spec| spec.cluster_ip.clone())
        .unwrap_or_else(|| "-".to_owned());
    let ports = spec
        .and_then(|spec| spec.ports.as_ref())
        .map_or(0, Vec::len);
    let row = resource_row(
        service,
        service.namespace().unwrap_or_default(),
        service.name_any(),
        service_type,
        format!("{ports} ports"),
        cluster_ip,
    )
    .with_port_forward_ports(service_forward_ports(service));
    match spec.and_then(|spec| spec.selector.as_ref()) {
        Some(selector) => {
            with_child_resource(row, ResourceKind::Pods.into(), label_map_selector(selector))
        }
        None => row,
    }
}

fn service_forward_ports(service: &Service) -> Vec<PortForwardPort> {
    service
        .spec
        .iter()
        .flat_map(|spec| spec.ports.iter().flatten())
        .filter_map(|port| {
            let is_tcp = port.protocol.as_deref().unwrap_or("TCP") == "TCP";
            let port_number = u16::try_from(port.port).ok()?;
            (is_tcp && port_number > 0).then(|| PortForwardPort {
                container: None,
                name: Arc::from(port.name.as_deref().unwrap_or("")),
                port: port_number,
            })
        })
        .collect()
}

fn replica_set_descriptor() -> ResourceDescriptor {
    ResourceDescriptor::new(
        "apps",
        "v1",
        "apps/v1",
        "ReplicaSet",
        "replicasets",
        true,
        ResourceOperations::all(),
    )
    .with_short_names(["rs"])
}

fn with_child_resource(
    row: ResourceRow,
    kind: ResourceDescriptor,
    label_selector: String,
) -> ResourceRow {
    if label_selector.is_empty() {
        row
    } else {
        row.with_child_resource_data(ChildResourceData {
            kind,
            label_selector: Arc::from(label_selector),
        })
    }
}

fn label_map_selector(labels: &BTreeMap<String, String>) -> String {
    labels
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join(",")
}

fn label_selector_string(selector: &LabelSelector) -> String {
    let mut terms = selector
        .match_labels
        .as_ref()
        .map_or_else(Vec::new, |labels| {
            labels
                .iter()
                .map(|(key, value)| format!("{key}={value}"))
                .collect()
        });
    if let Some(expressions) = &selector.match_expressions {
        terms.extend(expressions.iter().filter_map(|requirement| {
            let values = requirement.values.as_deref().unwrap_or_default().join(",");
            match requirement.operator.as_str() {
                "In" => Some(format!("{} in ({values})", requirement.key)),
                "NotIn" => Some(format!("{} notin ({values})", requirement.key)),
                "Exists" => Some(requirement.key.clone()),
                "DoesNotExist" => Some(format!("!{}", requirement.key)),
                "Gt" => Some(format!("{}>{values}", requirement.key)),
                "Lt" => Some(format!("{}<{values}", requirement.key)),
                _ => None,
            }
        }));
    }
    terms.join(",")
}

fn dynamic_row(object: &DynamicObject) -> ResourceRow {
    if object.types.as_ref().is_some_and(|types| {
        types.api_version.starts_with("apiregistration.k8s.io/") && types.kind == "APIService"
    }) {
        return api_service_row(object);
    }
    let status = dynamic_string(&object.data, &["/status/phase", "/status/state"])
        .or_else(|| dynamic_ready_condition(&object.data))
        .or_else(|| dynamic_string(&object.data, &["/type", "/spec/type"]))
        .unwrap_or_else(|| "-".to_owned());
    let ready = dynamic_ready(&object.data).unwrap_or_else(|| "-".to_owned());
    let details = dynamic_string(
        &object.data,
        &["/status/reason", "/status/message", "/spec/type", "/type"],
    )
    .unwrap_or_else(|| "-".to_owned());
    resource_row(
        object,
        object.namespace().unwrap_or_else(|| "-".to_owned()),
        object.name_any(),
        status,
        ready,
        details,
    )
}

fn api_service_row(object: &DynamicObject) -> ResourceRow {
    let service = object
        .data
        .pointer("/spec/service")
        .filter(|service| !service.is_null())
        .and_then(|service| {
            Some(format!(
                "{}/{}",
                service.get("namespace")?.as_str()?,
                service.get("name")?.as_str()?
            ))
        })
        .unwrap_or_else(|| "Local".to_owned());
    let available = object
        .data
        .pointer("/status/conditions")
        .and_then(serde_json::Value::as_array)
        .and_then(|conditions| {
            conditions.iter().find(|condition| {
                condition.get("type").and_then(serde_json::Value::as_str) == Some("Available")
            })
        })
        .map_or_else(
            || "Unknown".to_owned(),
            |condition| {
                let status = condition
                    .get("status")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("Unknown");
                let reason = condition
                    .get("reason")
                    .and_then(serde_json::Value::as_str)
                    .filter(|reason| !reason.is_empty());
                if status == "True" {
                    status.to_owned()
                } else {
                    reason.map_or_else(
                        || status.to_owned(),
                        |reason| format!("{status} ({reason})"),
                    )
                }
            },
        );
    resource_row(
        object,
        "-".to_owned(),
        object.name_any(),
        available,
        service,
        "-".to_owned(),
    )
}

fn dynamic_string(value: &serde_json::Value, pointers: &[&str]) -> Option<String> {
    pointers.iter().find_map(|pointer| {
        let value = value.pointer(pointer)?;
        value
            .as_str()
            .map(str::to_owned)
            .or_else(|| value.as_bool().map(|value| value.to_string()))
            .or_else(|| value.as_i64().map(|value| value.to_string()))
            .filter(|value| !value.is_empty())
    })
}

fn dynamic_ready_condition(value: &serde_json::Value) -> Option<String> {
    value
        .pointer("/status/conditions")?
        .as_array()?
        .iter()
        .rev()
        .find(|condition| {
            condition.get("status").and_then(serde_json::Value::as_str) == Some("True")
        })?
        .get("type")?
        .as_str()
        .map(str::to_owned)
}

fn dynamic_ready(value: &serde_json::Value) -> Option<String> {
    let ready = value
        .pointer("/status/readyReplicas")
        .and_then(serde_json::Value::as_u64)?;
    let desired = value
        .pointer("/spec/replicas")
        .or_else(|| value.pointer("/status/replicas"))
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(ready);
    Some(format!("{ready}/{desired}"))
}

fn node_row(node: &Node) -> ResourceRow {
    let readiness = node
        .status
        .as_ref()
        .and_then(|status| status.conditions.as_ref())
        .and_then(|conditions| {
            conditions
                .iter()
                .find(|condition| condition.type_ == "Ready")
        })
        .map_or("Unknown", |condition| match condition.status.as_str() {
            "True" => "Ready",
            "False" => "NotReady",
            _ => "Unknown",
        });
    let role = node
        .metadata
        .labels
        .as_ref()
        .and_then(|labels| {
            labels
                .keys()
                .find_map(|key| key.strip_prefix("node-role.kubernetes.io/"))
        })
        .filter(|role| !role.is_empty())
        .unwrap_or("worker");
    let version = node
        .status
        .as_ref()
        .and_then(|status| status.node_info.as_ref())
        .map_or_else(|| "-".to_owned(), |info| info.kubelet_version.clone());
    let taint_count = node
        .spec
        .as_ref()
        .and_then(|spec| spec.taints.as_ref())
        .map_or(0, Vec::len);
    let taints = node
        .spec
        .as_ref()
        .and_then(|spec| spec.taints.as_ref())
        .filter(|taints| !taints.is_empty())
        .map_or_else(
            || "<none>".to_owned(),
            |taints| {
                taints
                    .iter()
                    .map(|taint| {
                        let value = taint
                            .value
                            .as_deref()
                            .map_or_else(String::new, |value| format!("={value}"));
                        format!("{}{}:{}", taint.key, value, taint.effect)
                    })
                    .collect::<Vec<_>>()
                    .join(",")
            },
        );
    let allocatable = node
        .status
        .as_ref()
        .and_then(|status| status.allocatable.as_ref());
    let cpu_allocatable_millis = allocatable
        .and_then(|values| values.get("cpu"))
        .and_then(|quantity| parse_cpu_millis(&quantity.0))
        .unwrap_or(0);
    let memory_allocatable_bytes = allocatable
        .and_then(|values| values.get("memory"))
        .and_then(|quantity| parse_memory_bytes(&quantity.0))
        .unwrap_or(0);
    let unschedulable = node
        .spec
        .as_ref()
        .and_then(|spec| spec.unschedulable)
        .unwrap_or(false);
    let status = if unschedulable {
        format!("{readiness},Unschedulable")
    } else {
        readiness.to_owned()
    };
    resource_row(
        node,
        "-".to_owned(),
        node.name_any(),
        status,
        role.to_owned(),
        version.clone(),
    )
    .with_node_data(NodeData {
        unschedulable,
        taints: Arc::from(taints),
        taint_count: u32::try_from(taint_count).unwrap_or(u32::MAX),
        version: Arc::from(version),
        pods: 0,
        cpu_millis: None,
        cpu_allocatable_millis,
        memory_bytes: None,
        memory_allocatable_bytes,
    })
}

fn pod_resource_totals(pod: &Pod) -> (u64, u64, u64, u64) {
    let mut cpu_requests = 0_u64;
    let mut cpu_limits = 0_u64;
    let mut memory_requests = 0_u64;
    let mut memory_limits = 0_u64;
    let Some(spec) = &pod.spec else {
        return (0, 0, 0, 0);
    };
    for container in &spec.containers {
        let Some(resources) = &container.resources else {
            continue;
        };
        cpu_requests = cpu_requests.saturating_add(
            resources
                .requests
                .as_ref()
                .and_then(|values| values.get("cpu"))
                .and_then(|quantity| parse_cpu_millis(&quantity.0))
                .unwrap_or(0),
        );
        cpu_limits = cpu_limits.saturating_add(
            resources
                .limits
                .as_ref()
                .and_then(|values| values.get("cpu"))
                .and_then(|quantity| parse_cpu_millis(&quantity.0))
                .unwrap_or(0),
        );
        memory_requests = memory_requests.saturating_add(
            resources
                .requests
                .as_ref()
                .and_then(|values| values.get("memory"))
                .and_then(|quantity| parse_memory_bytes(&quantity.0))
                .unwrap_or(0),
        );
        memory_limits = memory_limits.saturating_add(
            resources
                .limits
                .as_ref()
                .and_then(|values| values.get("memory"))
                .and_then(|quantity| parse_memory_bytes(&quantity.0))
                .unwrap_or(0),
        );
    }
    (cpu_requests, cpu_limits, memory_requests, memory_limits)
}

fn parse_cpu_millis(quantity: &str) -> Option<u64> {
    let (number, numerator, denominator) = if let Some(number) = quantity.strip_suffix('n') {
        (number, 1_u128, 1_000_000_u128)
    } else if let Some(number) = quantity.strip_suffix('u') {
        (number, 1, 1_000)
    } else if let Some(number) = quantity.strip_suffix('m') {
        (number, 1, 1)
    } else {
        (quantity, 1_000, 1)
    };
    parse_decimal_scaled(number, numerator, denominator)
}

fn parse_memory_bytes(quantity: &str) -> Option<u64> {
    const SUFFIXES: [(&str, u128); 12] = [
        ("Ki", 1_024),
        ("Mi", 1_048_576),
        ("Gi", 1_073_741_824),
        ("Ti", 1_099_511_627_776),
        ("Pi", 1_125_899_906_842_624),
        ("Ei", 1_152_921_504_606_846_976),
        ("K", 1_000),
        ("M", 1_000_000),
        ("G", 1_000_000_000),
        ("T", 1_000_000_000_000),
        ("P", 1_000_000_000_000_000),
        ("E", 1_000_000_000_000_000_000),
    ];
    let (number, multiplier) = SUFFIXES
        .iter()
        .find_map(|(suffix, multiplier)| {
            quantity
                .strip_suffix(suffix)
                .map(|number| (number, *multiplier))
        })
        .unwrap_or((quantity, 1));
    parse_decimal_scaled(number, multiplier, 1)
}

fn parse_decimal_scaled(number: &str, numerator: u128, denominator: u128) -> Option<u64> {
    let number = number.strip_prefix('+').unwrap_or(number);
    if number.starts_with('-') || denominator == 0 {
        return None;
    }
    let (whole, fraction) = number.split_once('.').unwrap_or((number, ""));
    if whole.is_empty() && fraction.is_empty() {
        return None;
    }
    let scale = 10_u128.checked_pow(u32::try_from(fraction.len()).ok()?)?;
    let whole = if whole.is_empty() {
        0
    } else {
        whole.parse::<u128>().ok()?
    };
    let fraction = if fraction.is_empty() {
        0
    } else {
        fraction.parse::<u128>().ok()?
    };
    let raw = whole.checked_mul(scale)?.checked_add(fraction)?;
    let scaled = raw.checked_mul(numerator)?;
    let divisor = scale.checked_mul(denominator)?;
    let rounded_up = scaled.checked_add(divisor.saturating_sub(1))? / divisor;
    u64::try_from(rounded_up).ok()
}

fn workload_status(deleting: bool, ready: i32, desired: i32) -> String {
    if deleting {
        "Terminating"
    } else if ready >= desired {
        "Ready"
    } else {
        "Progressing"
    }
    .to_owned()
}

fn resource_row<K: ResourceExt>(
    resource: &K,
    namespace: String,
    name: String,
    status: String,
    ready: String,
    details: String,
) -> ResourceRow {
    let created_unix = resource
        .meta()
        .creation_timestamp
        .as_ref()
        .map_or(0, |time| time.0.as_second());
    let labels = resource
        .meta()
        .labels
        .as_ref()
        .map(|labels| {
            labels
                .iter()
                .map(|(key, value)| format!("{key}={value}"))
                .collect::<Vec<_>>()
                .join(",")
        })
        .unwrap_or_default();
    let row =
        ResourceRow::new(namespace, name, status, ready, details, created_unix).with_labels(labels);
    match owner_reference_data(resource.meta()) {
        Some(owner) => row.with_owner_reference_data(owner),
        None => row,
    }
}

fn owner_reference_data(
    metadata: &k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta,
) -> Option<OwnerReferenceData> {
    let owners = metadata.owner_references.as_ref()?;
    let owner = owners
        .iter()
        .find(|owner| owner.controller == Some(true))
        .or_else(|| owners.first())?;
    Some(OwnerReferenceData {
        api_version: Arc::from(owner.api_version.as_str()),
        kind: Arc::from(owner.kind.as_str()),
        name: Arc::from(owner.name.as_str()),
    })
}

#[cfg(test)]
mod tests {
    use k8s_openapi::{
        api::core::v1::{
            Container, ContainerPort, ContainerState, ContainerStateWaiting, ContainerStatus,
            NamespaceStatus, PodSpec, PodStatus,
        },
        apimachinery::pkg::apis::meta::v1::{ManagedFieldsEntry, ObjectMeta, Time},
    };

    use super::*;

    fn default_drain_options() -> NodeDrainOptions {
        NodeDrainOptions::default()
    }

    #[test]
    fn maps_namespace_status_and_creation_time() {
        let namespace = Namespace {
            metadata: ObjectMeta {
                name: Some("payments".to_owned()),
                creation_timestamp: Some(Time(
                    k8s_openapi::jiff::Timestamp::from_second(3_600).expect("valid timestamp"),
                )),
                ..ObjectMeta::default()
            },
            status: Some(NamespaceStatus {
                phase: Some("Active".to_owned()),
                ..NamespaceStatus::default()
            }),
            ..Namespace::default()
        };

        assert_eq!(
            namespace_info(&namespace),
            NamespaceInfo::discovered("payments", "Active", Some(3_600))
        );
    }

    #[test]
    fn node_status_combines_readiness_and_scheduling_state() {
        let mut node: Node = serde_json::from_value(serde_json::json!({
            "apiVersion": "v1",
            "kind": "Node",
            "metadata": { "name": "worker-0" },
            "spec": { "unschedulable": true },
            "status": {
                "conditions": [{
                    "type": "Ready",
                    "status": "True",
                    "lastHeartbeatTime": null,
                    "lastTransitionTime": null,
                    "reason": null,
                    "message": null
                }],
                "nodeInfo": {
                    "architecture": "arm64",
                    "bootID": "boot",
                    "containerRuntimeVersion": "containerd://1.0",
                    "kernelVersion": "1.0",
                    "kubeProxyVersion": "v1.32.0",
                    "kubeletVersion": "v1.32.0",
                    "machineID": "machine",
                    "operatingSystem": "linux",
                    "osImage": "Linux",
                    "systemUUID": "uuid"
                }
            }
        }))
        .expect("valid Node");

        assert_eq!(node_row(&node).status.as_ref(), "Ready,Unschedulable");
        assert!(
            node_row(&node)
                .node_data()
                .is_some_and(|data| data.unschedulable)
        );
        node.status
            .as_mut()
            .and_then(|status| status.conditions.as_mut())
            .and_then(|conditions| conditions.first_mut())
            .expect("Ready condition")
            .status = "False".to_owned();
        assert_eq!(node_row(&node).status.as_ref(), "NotReady,Unschedulable");
    }

    #[test]
    fn node_scheduling_patch_sets_the_requested_state() {
        assert_eq!(
            node_unschedulable_patch(true).pointer("/spec/unschedulable"),
            Some(&serde_json::json!(true))
        );
        assert_eq!(
            node_unschedulable_patch(false).pointer("/spec/unschedulable"),
            Some(&serde_json::json!(false))
        );
    }

    #[test]
    fn restart_patch_changes_the_pod_template_annotation() {
        let patch = restart_patch("2026-07-28T01:02:03Z");
        assert_eq!(
            patch.pointer("/spec/template/metadata/annotations/kubectl.kubernetes.io~1restartedAt"),
            Some(&serde_json::json!("2026-07-28T01:02:03Z"))
        );
    }

    #[test]
    fn restart_result_reports_success_and_partial_failures() {
        assert_eq!(
            restart_result("Deployment", "deployments", "prod/api", 1, &[]),
            Ok(ActionResult::Notice(
                "Restart in progress for Deployment prod/api".to_owned()
            ))
        );
        assert_eq!(
            restart_result(
                "Deployment",
                "deployments",
                "prod/api",
                2,
                &[("prod/worker".to_owned(), "forbidden".to_owned())],
            ),
            Err(
                "Restart requested for 1 of 2 deployments; failed: prod/worker: forbidden"
                    .to_owned()
            )
        );
    }

    #[test]
    fn drain_plan_ignores_daemon_sets_and_blocks_unmanaged_pods() {
        let daemon_set_pod: Pod = serde_json::from_value(serde_json::json!({
            "apiVersion": "v1",
            "kind": "Pod",
            "metadata": {
                "name": "agent-1",
                "namespace": "system",
                "ownerReferences": [{
                    "apiVersion": "apps/v1",
                    "kind": "DaemonSet",
                    "name": "agent",
                    "uid": "agent-uid",
                    "controller": true
                }]
            },
            "spec": { "containers": [] }
        }))
        .expect("valid DaemonSet Pod");
        let unmanaged_pod: Pod = serde_json::from_value(serde_json::json!({
            "apiVersion": "v1",
            "kind": "Pod",
            "metadata": { "name": "debug", "namespace": "default" },
            "spec": { "containers": [] },
            "status": { "phase": "Running" }
        }))
        .expect("valid unmanaged Pod");

        let options = default_drain_options();
        assert_eq!(
            pod_drain_disposition(&daemon_set_pod, &options),
            PodDrainDisposition::Block(
                "system/agent-1 is managed by a DaemonSet; enable Ignore DaemonSets".to_owned()
            )
        );
        assert_eq!(
            pod_drain_disposition(&unmanaged_pod, &options),
            PodDrainDisposition::Block("default/debug is unmanaged; enable Force".to_owned())
        );
        let mut permissive = options;
        permissive.flags = NodeDrainFlags::IGNORE_DAEMON_SETS | NodeDrainFlags::FORCE;
        assert_eq!(
            pod_drain_disposition(&daemon_set_pod, &permissive),
            PodDrainDisposition::Ignore
        );
        assert_eq!(
            pod_drain_disposition(&unmanaged_pod, &permissive),
            PodDrainDisposition::Remove(DrainPod {
                namespace: "default".to_owned(),
                name: "debug".to_owned(),
                uid: None,
            })
        );
    }

    #[test]
    fn drain_plan_evicts_managed_and_completed_pods() {
        let managed_pod: Pod = serde_json::from_value(serde_json::json!({
            "apiVersion": "v1",
            "kind": "Pod",
            "metadata": {
                "name": "api-7d9-x2",
                "namespace": "prod",
                "ownerReferences": [{
                    "apiVersion": "apps/v1",
                    "kind": "ReplicaSet",
                    "name": "api-7d9",
                    "uid": "replica-set-uid",
                    "controller": true
                }]
            },
            "spec": { "containers": [] }
        }))
        .expect("valid managed Pod");
        let completed_pod: Pod = serde_json::from_value(serde_json::json!({
            "apiVersion": "v1",
            "kind": "Pod",
            "metadata": { "name": "done", "namespace": "jobs" },
            "spec": { "containers": [] },
            "status": { "phase": "Succeeded" }
        }))
        .expect("valid completed Pod");

        assert_eq!(
            pod_drain_disposition(&managed_pod, &default_drain_options()),
            PodDrainDisposition::Remove(DrainPod {
                namespace: "prod".to_owned(),
                name: "api-7d9-x2".to_owned(),
                uid: None,
            })
        );
        assert_eq!(
            pod_drain_disposition(&completed_pod, &default_drain_options()),
            PodDrainDisposition::Remove(DrainPod {
                namespace: "jobs".to_owned(),
                name: "done".to_owned(),
                uid: None,
            })
        );
    }

    #[test]
    fn drain_plan_requires_explicit_emptydir_data_deletion() {
        let pod: Pod = serde_json::from_value(serde_json::json!({
            "apiVersion": "v1",
            "kind": "Pod",
            "metadata": {
                "name": "cache-0",
                "namespace": "prod",
                "uid": "cache-uid",
                "ownerReferences": [{
                    "apiVersion": "apps/v1",
                    "kind": "StatefulSet",
                    "name": "cache",
                    "uid": "stateful-set-uid",
                    "controller": true
                }]
            },
            "spec": {
                "containers": [],
                "volumes": [{ "name": "cache", "emptyDir": {} }]
            }
        }))
        .expect("valid emptyDir Pod");
        let mut options = default_drain_options();

        assert_eq!(
            pod_drain_disposition(&pod, &options),
            PodDrainDisposition::Block(
                "prod/cache-0 uses emptyDir; enable Delete EmptyDir Data".to_owned()
            )
        );
        options.flags = NodeDrainFlags::DELETE_EMPTYDIR_DATA;
        assert_eq!(
            pod_drain_disposition(&pod, &options),
            PodDrainDisposition::Remove(DrainPod {
                namespace: "prod".to_owned(),
                name: "cache-0".to_owned(),
                uid: Some("cache-uid".to_owned()),
            })
        );
    }

    #[test]
    fn drain_delete_parameters_include_grace_period_and_uid_precondition() {
        let target = DrainPod {
            namespace: "prod".to_owned(),
            name: "api-0".to_owned(),
            uid: Some("pod-uid".to_owned()),
        };
        let mut options = default_drain_options();
        options.grace_period_seconds = Some(7);

        let params = drain_delete_params(&target, &options);
        assert_eq!(params.grace_period_seconds, Some(7));
        assert_eq!(
            params
                .preconditions
                .and_then(|preconditions| preconditions.uid),
            Some("pod-uid".to_owned())
        );
    }

    #[test]
    fn drain_result_reports_partial_eviction_after_cordon() {
        assert_eq!(
            drain_result(
                "worker-0",
                3,
                2,
                &["prod/api-1: disruption budget".to_owned()]
            ),
            Err("Node worker-0 was cordoned; removal started for 2 of 3 pods; failed: prod/api-1: disruption budget".to_owned())
        );
    }

    #[test]
    fn deployment_row_declares_replica_sets_as_label_selected_children() {
        let deployment: Deployment = serde_json::from_value(serde_json::json!({
            "apiVersion": "apps/v1",
            "kind": "Deployment",
            "metadata": { "name": "api", "namespace": "prod" },
            "spec": {
                "replicas": 3,
                "selector": { "matchLabels": { "app": "api", "tier": "backend" } },
                "template": {
                    "metadata": { "labels": { "app": "api", "tier": "backend" } },
                    "spec": { "containers": [] }
                }
            },
            "status": { "readyReplicas": 3, "availableReplicas": 3 }
        }))
        .expect("valid Deployment");

        let row = deployment_row(&deployment);
        let child = row.child_resource_data().expect("Deployment child data");
        assert!(child.kind.is_apps_v1_replica_set());
        assert_eq!(child.label_selector.as_ref(), "app=api,tier=backend");
    }

    #[test]
    fn pod_row_preserves_the_controller_owner_reference() {
        let pod: Pod = serde_json::from_value(serde_json::json!({
            "apiVersion": "v1",
            "kind": "Pod",
            "metadata": {
                "name": "api-7d9-x2",
                "namespace": "prod",
                "ownerReferences": [
                    {
                        "apiVersion": "example.io/v1",
                        "kind": "SideOwner",
                        "name": "side-owner",
                        "uid": "side-owner-uid"
                    },
                    {
                        "apiVersion": "apps/v1",
                        "kind": "ReplicaSet",
                        "name": "api-7d9",
                        "uid": "replica-set-uid",
                        "controller": true
                    }
                ]
            },
            "spec": { "containers": [] }
        }))
        .expect("valid Pod");

        let row = pod_row(&pod);
        let owner = row.owner.as_ref().expect("Pod owner data");
        assert_eq!(owner.api_version.as_ref(), "apps/v1");
        assert_eq!(owner.kind.as_ref(), "ReplicaSet");
        assert_eq!(owner.name.as_ref(), "api-7d9");
    }

    #[test]
    fn replica_set_row_declares_pods_as_label_selected_children() {
        let replica_set: ReplicaSet = serde_json::from_value(serde_json::json!({
            "apiVersion": "apps/v1",
            "kind": "ReplicaSet",
            "metadata": { "name": "api-7d9", "namespace": "prod" },
            "spec": {
                "replicas": 3,
                "selector": { "matchLabels": { "pod-template-hash": "7d9" } },
                "template": {
                    "metadata": { "labels": { "pod-template-hash": "7d9" } },
                    "spec": { "containers": [] }
                }
            },
            "status": {
                "replicas": 3,
                "readyReplicas": 3,
                "availableReplicas": 3,
                "fullyLabeledReplicas": 3,
                "observedGeneration": 1
            }
        }))
        .expect("valid ReplicaSet");

        let row = replica_set_row(&replica_set);
        let child = row.child_resource_data().expect("ReplicaSet child data");
        assert_eq!(child.kind.known_kind(), Some(ResourceKind::Pods));
        assert_eq!(child.label_selector.as_ref(), "pod-template-hash=7d9");
    }

    #[test]
    fn builds_explicit_kubectl_context_delete_arguments() {
        assert_eq!(
            context_delete_args("prod; touch /tmp/nope"),
            ["config", "delete-context", "prod; touch /tmp/nope"]
        );
    }

    #[test]
    fn refuses_resources_without_discovery_delete_capability() {
        let target = ResourceTarget {
            kind: ResourceDescriptor::new(
                "example.io",
                "v1",
                "example.io/v1",
                "Widget",
                "widgets",
                true,
                crate::model::ResourceOperations::default(),
            ),
            namespace: Some("prod".to_owned()),
            name: "one".to_owned(),
        };
        assert_eq!(
            ensure_deletable(&target).expect_err("delete must be refused"),
            "widgets.example.io does not advertise delete support"
        );
    }

    #[test]
    fn summarizes_complete_and_partial_batch_deletes() {
        assert_eq!(
            deletion_result(false, "pods", "prod/api-0", 2, &[]),
            Ok(ActionResult::Notice("Deleted all 2 pods".to_owned()))
        );
        assert_eq!(
            deletion_result(
                false,
                "pods",
                "prod/api-0",
                3,
                &[("prod/api-1".to_owned(), "forbidden".to_owned())],
            ),
            Err("Deleted 2 of 3 pods; failed to delete 1: prod/api-1: forbidden".to_owned())
        );
    }

    #[test]
    fn compacts_pod_to_display_row() {
        let pod = Pod {
            metadata: ObjectMeta {
                name: Some("api-0".into()),
                namespace: Some("prod".into()),
                ..ObjectMeta::default()
            },
            spec: Some(PodSpec {
                containers: vec![Container {
                    name: "api".into(),
                    ports: Some(vec![
                        ContainerPort {
                            container_port: 8080,
                            name: Some("http".to_owned()),
                            protocol: Some("TCP".to_owned()),
                            ..ContainerPort::default()
                        },
                        ContainerPort {
                            container_port: 5353,
                            protocol: Some("UDP".to_owned()),
                            ..ContainerPort::default()
                        },
                    ]),
                    ..Container::default()
                }],
                ..PodSpec::default()
            }),
            status: Some(PodStatus {
                phase: Some("Running".into()),
                container_statuses: Some(vec![ContainerStatus {
                    name: "api".into(),
                    ready: true,
                    restart_count: 3,
                    image: "api:latest".into(),
                    image_id: String::new(),
                    ..ContainerStatus::default()
                }]),
                ..PodStatus::default()
            }),
        };
        let row = pod_row(&pod);
        assert_eq!(row.key.as_ref(), "prod/api-0");
        assert_eq!(row.ready.as_ref(), "1/1");
        assert_eq!(row.details.as_ref(), "3");
        let data = row.pod_data().expect("pod data");
        assert_eq!(data.restarts, 3);
        assert_eq!(data.node.as_ref(), "-");
        assert_eq!(data.default_container.as_ref(), "api");
        assert_eq!(row.port_forward_ports().len(), 1);
        assert_eq!(row.port_forward_ports()[0].display(), "api::8080(http)");
    }

    #[test]
    fn service_row_exposes_only_tcp_service_ports() {
        let service: Service = serde_json::from_value(serde_json::json!({
            "apiVersion": "v1",
            "kind": "Service",
            "metadata": { "name": "api", "namespace": "prod" },
            "spec": {
                "selector": { "app": "api" },
                "ports": [
                    { "name": "http", "port": 80, "targetPort": 8080, "protocol": "TCP" },
                    { "name": "dns", "port": 53, "protocol": "UDP" }
                ]
            }
        }))
        .expect("valid Service");

        let row = service_row(&service);
        assert_eq!(row.port_forward_ports().len(), 1);
        assert_eq!(row.port_forward_ports()[0].display(), "80(http)");
        assert_eq!(row.port_forward_ports()[0].port, 80);
    }

    #[test]
    fn previous_log_params_keep_the_selected_container() {
        let params = pod_log_params(Some("sidecar".to_owned()), true);

        assert_eq!(params.container.as_deref(), Some("sidecar"));
        assert!(params.previous);
        assert_eq!(params.tail_lines, Some(500));
    }

    #[test]
    fn pod_default_container_honors_valid_annotation_and_falls_back_to_first() {
        let mut pod = Pod {
            metadata: ObjectMeta {
                annotations: Some(BTreeMap::from([(
                    "kubectl.kubernetes.io/default-container".to_owned(),
                    "sidecar".to_owned(),
                )])),
                ..ObjectMeta::default()
            },
            spec: Some(PodSpec {
                containers: vec![
                    Container {
                        name: "api".to_owned(),
                        ..Container::default()
                    },
                    Container {
                        name: "sidecar".to_owned(),
                        ..Container::default()
                    },
                ],
                ..PodSpec::default()
            }),
            ..Pod::default()
        };

        assert_eq!(pod_default_container(&pod), Some("sidecar"));
        pod.metadata
            .annotations
            .as_mut()
            .expect("annotations")
            .insert(
                "kubectl.kubernetes.io/default-container".to_owned(),
                "missing".to_owned(),
            );
        assert_eq!(pod_default_container(&pod), Some("api"));
    }

    #[test]
    fn parses_kubernetes_cpu_and_memory_quantities() {
        assert_eq!(parse_cpu_millis("250m"), Some(250));
        assert_eq!(parse_cpu_millis("1.5"), Some(1_500));
        assert_eq!(parse_cpu_millis("1500000n"), Some(2));
        assert_eq!(parse_memory_bytes("64Mi"), Some(64 * 1_048_576));
        assert_eq!(parse_memory_bytes("1.5Gi"), Some(1_610_612_736));
    }

    #[test]
    fn enrichment_updates_compact_rows_without_kubernetes_objects() {
        let mut row =
            ResourceRow::new("prod", "api-0", "Running", "1/1", "0", 0).with_pod_data(PodData {
                restarts: 0,
                node: Arc::from("worker-0"),
                cpu_request_millis: 100,
                cpu_limit_millis: 200,
                memory_request_bytes: 128,
                memory_limit_bytes: 256,
                ..PodData::default()
            });
        let mut state = EnrichmentState::default();
        state.update(EnrichmentUpdate::PodUsage(FxHashMap::from_iter([(
            Arc::clone(&row.key),
            ResourceUsage {
                cpu_millis: Some(25),
                memory_bytes: Some(64),
            },
        )])));
        state.apply(&mut row);
        let data = row.pod_data().expect("pod metrics");
        assert_eq!(data.cpu_millis, Some(25));
        assert_eq!(data.memory_bytes, Some(64));
    }

    #[test]
    fn builds_explicit_kubectl_describe_arguments() {
        let pod = ResourceTarget {
            kind: ResourceKind::Pods.into(),
            namespace: Some("prod".to_owned()),
            name: "api-0".to_owned(),
        };
        assert_eq!(
            describe_args("staging", &pod),
            [
                "describe",
                "pods",
                "api-0",
                "--context",
                "staging",
                "--namespace",
                "prod",
            ]
        );

        let node = ResourceTarget {
            kind: ResourceKind::Nodes.into(),
            namespace: None,
            name: "worker-0".to_owned(),
        };
        assert_eq!(
            describe_args("staging", &node),
            ["describe", "nodes", "worker-0", "--context", "staging"]
        );

        let widget = ResourceTarget {
            kind: ResourceDescriptor::new(
                "example.io",
                "v1",
                "example.io/v1",
                "Widget",
                "widgets",
                true,
                crate::model::ResourceOperations::all(),
            ),
            namespace: Some("prod".to_owned()),
            name: "primary".to_owned(),
        };
        assert_eq!(describe_args("staging", &widget)[1], "widgets.example.io");
    }

    #[test]
    fn compacts_secret_like_dynamic_objects() {
        let resource = ApiResource::from_gvk_with_plural(
            &GroupVersionKind::gvk("", "v1", "Secret"),
            "secrets",
        );
        let object = DynamicObject::new("api-token", &resource)
            .within("prod")
            .data(serde_json::json!({
                "type": "Opaque",
                "data": {"token": "c2VjcmV0"}
            }));
        let row = dynamic_row(&object);
        assert_eq!(row.key.as_ref(), "prod/api-token");
        assert_eq!(row.status.as_ref(), "Opaque");
        assert!(row.search.contains("api-token"));
    }

    #[test]
    fn projects_local_and_remote_api_services_to_k9s_columns() {
        let local: DynamicObject = serde_json::from_value(serde_json::json!({
            "apiVersion": "apiregistration.k8s.io/v1",
            "kind": "APIService",
            "metadata": {
                "name": "v1.",
                "creationTimestamp": "2024-01-01T00:00:00Z"
            },
            "spec": {},
            "status": {
                "conditions": [{"type": "Available", "status": "True"}]
            }
        }))
        .expect("local APIService");
        let row = dynamic_row(&local);
        assert_eq!(row.namespace.as_ref(), "-");
        assert_eq!(row.name.as_ref(), "v1.");
        assert_eq!(row.ready.as_ref(), "Local");
        assert_eq!(row.status.as_ref(), "True");
        assert_eq!(row.created_unix, 1_704_067_200);

        let remote: DynamicObject = serde_json::from_value(serde_json::json!({
            "apiVersion": "apiregistration.k8s.io/v1",
            "kind": "APIService",
            "metadata": {"name": "v1beta1.metrics.k8s.io"},
            "spec": {
                "service": {"namespace": "kube-system", "name": "metrics-server"}
            },
            "status": {
                "conditions": [{
                    "type": "Available",
                    "status": "False",
                    "reason": "MissingEndpoints"
                }]
            }
        }))
        .expect("remote APIService");
        let row = dynamic_row(&remote);
        assert_eq!(row.name.as_ref(), "v1beta1.metrics.k8s.io");
        assert_eq!(row.ready.as_ref(), "kube-system/metrics-server");
        assert_eq!(row.status.as_ref(), "False (MissingEndpoints)");

        let unknown: DynamicObject = serde_json::from_value(serde_json::json!({
            "apiVersion": "apiregistration.k8s.io/v1",
            "kind": "APIService",
            "metadata": {"name": "v1.example.io"},
            "spec": {"service": null}
        }))
        .expect("APIService without a condition");
        let row = dynamic_row(&unknown);
        assert_eq!(row.ready.as_ref(), "Local");
        assert_eq!(row.status.as_ref(), "Unknown");
    }

    #[test]
    fn secret_yaml_has_encoded_and_lossless_decoded_variants() {
        let resource = ApiResource::from_gvk_with_plural(
            &GroupVersionKind::gvk("", "v1", "Secret"),
            "secrets",
        );
        let mut object = DynamicObject::new("api-token", &resource)
            .within("prod")
            .data(serde_json::json!({
                "type": "Opaque",
                "data": {
                    "token": "c2VjcmV0Cg==",
                    "binary": "AP8Q"
                }
            }));
        object.metadata.managed_fields = Some(vec![ManagedFieldsEntry::default()]);
        let target = ResourceTarget {
            kind: ResourceDescriptor::new(
                "",
                "v1",
                "v1",
                "Secret",
                "secrets",
                true,
                crate::model::ResourceOperations::all(),
            ),
            namespace: Some("prod".to_owned()),
            name: "api-token".to_owned(),
        };

        let yaml = yaml_content_from_object(object, &target).expect("secret yaml");
        assert!(yaml.body.contains("c2VjcmV0Cg=="));
        assert!(!yaml.body.contains("managedFields"));
        let decoded = yaml.decoded_body.expect("decoded body");
        let decoded_value: serde_json::Value =
            serde_yaml::from_str(&decoded).expect("decoded yaml value");
        assert_eq!(decoded_value["data"]["token"], "secret\n");
        assert_eq!(decoded_value["data"]["binary"], "0x00ff10");
        let decoded_managed = yaml
            .decoded_with_managed_fields
            .expect("decoded managed body");
        assert!(decoded_managed.contains("managedFields"));
        assert!(decoded_managed.contains("0x00ff10"));
    }

    #[test]
    fn secret_data_content_contains_only_decoded_data() {
        let resource = ApiResource::from_gvk_with_plural(
            &GroupVersionKind::gvk("", "v1", "Secret"),
            "secrets",
        );
        let mut object = DynamicObject::new("api-token", &resource)
            .within("prod")
            .data(serde_json::json!({
                "type": "Opaque",
                "data": {
                    "token": "c2VjcmV0Cg==",
                    "binary": "AP8Q"
                }
            }));
        object.metadata.labels = Some(BTreeMap::from([(
            "app".to_owned(),
            "must-not-leak".to_owned(),
        )]));
        let target = ResourceTarget {
            kind: ResourceDescriptor::new(
                "",
                "v1",
                "v1",
                "Secret",
                "secrets",
                true,
                crate::model::ResourceOperations::all(),
            ),
            namespace: Some("prod".to_owned()),
            name: "api-token".to_owned(),
        };

        let ActionResult::Content {
            title,
            body,
            managed_fields,
            decoded_body,
            decoded_managed_fields,
            content,
        } = secret_data_content(&object, &target).expect("decoded Secret data")
        else {
            panic!("expected content result");
        };
        assert_eq!(title, "Secret(prod/api-token)");
        assert_eq!(content, ContentKind::SecretData);
        assert!(managed_fields.is_none());
        assert!(decoded_body.is_none());
        assert!(decoded_managed_fields.is_none());

        let values: BTreeMap<String, String> = serde_yaml::from_str(&body).expect("data-only YAML");
        assert_eq!(
            values,
            BTreeMap::from([
                ("binary".to_owned(), "0x00ff10".to_owned()),
                ("token".to_owned(), "secret\n".to_owned()),
            ])
        );
        for forbidden in ["apiVersion", "kind", "metadata", "must-not-leak", "Opaque"] {
            assert!(
                !body.contains(forbidden),
                "unexpected {forbidden} in {body}"
            );
        }
    }

    #[test]
    fn secret_decode_requires_exact_core_identity_and_valid_base64() {
        let resource = ApiResource::from_gvk_with_plural(
            &GroupVersionKind::gvk("", "v1", "Secret"),
            "secrets",
        );
        let invalid = DynamicObject::new("broken", &resource)
            .within("prod")
            .data(serde_json::json!({"data": {"token": "not base64!"}}));
        assert!(decode_secret_object(&invalid).is_none());

        let target = ResourceTarget {
            kind: ResourceDescriptor::new(
                "example.io",
                "v1",
                "example.io/v1",
                "Secret",
                "secrets",
                true,
                crate::model::ResourceOperations::all(),
            ),
            namespace: Some("prod".to_owned()),
            name: "custom".to_owned(),
        };
        assert!(!is_core_v1_secret(&target));
        let yaml = yaml_content_from_object(invalid, &target).expect("custom secret yaml");
        assert!(yaml.decoded_body.is_none());
    }

    #[test]
    fn service_account_subject_matching_obeys_namespace_and_kind() {
        let explicit: Subject = serde_json::from_value(serde_json::json!({
            "kind": "ServiceAccount",
            "name": "apiserver-admin",
            "namespace": "argo"
        }))
        .expect("subject");
        assert!(service_account_subject_matches(
            &explicit,
            "argo",
            "apiserver-admin",
            None,
        ));
        assert!(!service_account_subject_matches(
            &explicit,
            "other",
            "apiserver-admin",
            None,
        ));

        let implicit: Subject = serde_json::from_value(serde_json::json!({
            "kind": "ServiceAccount",
            "name": "apiserver-admin"
        }))
        .expect("subject");
        assert!(service_account_subject_matches(
            &implicit,
            "argo",
            "apiserver-admin",
            Some("argo"),
        ));
        assert!(!service_account_subject_matches(
            &implicit,
            "argo",
            "apiserver-admin",
            Some("default"),
        ));

        let user: Subject = serde_json::from_value(serde_json::json!({
            "kind": "User",
            "name": "apiserver-admin",
            "namespace": "argo"
        }))
        .expect("subject");
        assert!(!service_account_subject_matches(
            &user,
            "argo",
            "apiserver-admin",
            None,
        ));
    }

    #[test]
    fn policy_rows_expand_resources_names_and_non_resource_urls() {
        let rules: Vec<PolicyRule> = serde_json::from_value(serde_json::json!([
            {
                "apiGroups": [""],
                "resources": ["secrets"],
                "resourceNames": ["workflow-auth0-creds"],
                "verbs": ["get", "list", "impersonate"]
            },
            {
                "apiGroups": ["apps"],
                "resources": ["deployments"],
                "verbs": ["*"]
            },
            {
                "nonResourceURLs": ["/healthz"],
                "verbs": ["get"]
            }
        ]))
        .expect("policy rules");
        let rows = policy_rows(vec![PolicyRules {
            scope: "argo".to_owned(),
            binding: "RO:apiserver-admin-reader".to_owned(),
            rules,
        }]);

        let named = rows
            .iter()
            .find(|row| row.name.as_ref() == "secrets/workflow-auth0-creds")
            .expect("resourceNames policy");
        assert_eq!(named.namespace.as_ref(), "argo");
        assert_eq!(named.status.as_ref(), "core");
        assert_eq!(named.ready.as_ref(), "RO:apiserver-admin-reader");
        assert_eq!(named.details.as_ref(), "impersonate");
        let policy = named.policy_data().expect("verb matrix");
        assert!(policy.allows("get"));
        assert!(policy.allows("list"));
        assert!(!policy.allows("watch"));
        assert!(
            rows.iter().all(|row| row.name.as_ref() != "secrets"),
            "resourceNames must not be presented as access to every Secret"
        );

        let deployments = rows
            .iter()
            .find(|row| row.name.as_ref() == "deployments")
            .expect("deployments policy");
        let policy = deployments.policy_data().expect("wildcard verb matrix");
        for verb in [
            "get",
            "list",
            "watch",
            "create",
            "patch",
            "update",
            "delete",
            "deletecollection",
        ] {
            assert!(policy.allows(verb), "wildcard should allow {verb}");
        }

        let health = rows
            .iter()
            .find(|row| row.name.as_ref() == "/healthz")
            .expect("non-resource URL policy");
        assert_eq!(health.status.as_ref(), "-");
        assert!(health.policy_data().expect("verbs").allows("get"));
    }

    #[test]
    fn builds_regular_and_init_container_rows_with_runtime_status() {
        let pod = Pod {
            metadata: ObjectMeta {
                name: Some("api-0".to_owned()),
                namespace: Some("prod".to_owned()),
                ..ObjectMeta::default()
            },
            spec: Some(PodSpec {
                containers: vec![Container {
                    name: "api".to_owned(),
                    image: Some("example/api:v1".to_owned()),
                    ..Container::default()
                }],
                init_containers: Some(vec![Container {
                    name: "migrate".to_owned(),
                    image: Some("example/migrate:v1".to_owned()),
                    ..Container::default()
                }]),
                ..PodSpec::default()
            }),
            status: Some(PodStatus {
                container_statuses: Some(vec![ContainerStatus {
                    name: "api".to_owned(),
                    ready: false,
                    restart_count: 4,
                    image: "example/api:v1".to_owned(),
                    image_id: String::new(),
                    state: Some(ContainerState {
                        waiting: Some(ContainerStateWaiting {
                            reason: Some("CrashLoopBackOff".to_owned()),
                            ..ContainerStateWaiting::default()
                        }),
                        ..ContainerState::default()
                    }),
                    ..ContainerStatus::default()
                }]),
                init_container_statuses: Some(vec![ContainerStatus {
                    name: "migrate".to_owned(),
                    ready: true,
                    restart_count: 0,
                    image: "example/migrate:v1".to_owned(),
                    image_id: String::new(),
                    ..ContainerStatus::default()
                }]),
                ..PodStatus::default()
            }),
        };
        let rows = container_rows(&pod);
        assert_eq!(rows.len(), 2);
        let api = rows.iter().find(|row| row.name.as_ref() == "api").unwrap();
        assert_eq!(api.status.as_ref(), "CrashLoopBackOff");
        assert_eq!(api.container_data().unwrap().restarts, 4);
        let init = rows
            .iter()
            .find(|row| row.name.as_ref() == "migrate")
            .unwrap();
        assert_eq!(init.container_data().unwrap().kind.as_ref(), "init");
    }

    #[test]
    fn yaml_titles_include_namespace_and_name() {
        let pod = ResourceTarget {
            kind: ResourceKind::Pods.into(),
            namespace: Some("prod".to_owned()),
            name: "api-0".to_owned(),
        };
        let node = ResourceTarget {
            kind: ResourceKind::Nodes.into(),
            namespace: None,
            name: "worker-0".to_owned(),
        };
        assert_eq!(yaml_title(&pod), "YAML(prod/api-0)");
        assert_eq!(yaml_title(&node), "YAML(worker-0)");
    }

    #[test]
    fn requests_kubectl_table_pages_with_metadata_only() {
        let request = Request::new("/apis/example.io/v1/widgets")
            .list(&ListParams::default().limit(TABLE_INITIAL_PAGE_SIZE))
            .expect("list request");
        let request = table_request(request).expect("table request");

        assert_eq!(
            request.headers().get(ACCEPT).expect("accept header"),
            TABLE_ACCEPT
        );
        let query = request.uri().query().expect("query");
        assert!(query.contains("limit=250"));
        assert!(query.contains("includeObject=Metadata"));
        assert!(!query.contains("includeObject=Object"));
    }

    #[test]
    fn grows_dynamic_table_pages_to_a_bounded_maximum_after_first_paint() {
        let mut size = TABLE_INITIAL_PAGE_SIZE;
        let mut sizes = vec![size];
        for _ in 0..2 {
            size = next_table_page_size(size);
            sizes.push(size);
        }
        assert_eq!(sizes, [250, 5_000, 5_000]);
        assert_eq!(
            next_table_page_size(TABLE_MAX_PAGE_SIZE),
            TABLE_MAX_PAGE_SIZE
        );
    }

    #[test]
    fn initial_list_progress_publishes_at_250_then_every_5000_rows() {
        let mut progress = InitialListProgress::default();
        let published = [1, 249, 250, 251, 5_249, 5_250, 5_251, 10_249, 10_250]
            .into_iter()
            .filter(|loaded| progress.should_publish(*loaded))
            .collect::<Vec<_>>();
        assert_eq!(published, [250, 5_250, 10_250]);
    }

    #[test]
    fn typed_initial_list_publishes_rows_before_init_done() {
        let kind: ResourceDescriptor = ResourceKind::Pods.into();
        let request = WatchRequest {
            generation: 7,
            context: "demo".to_owned(),
            kind: kind.clone(),
            namespace: None,
            field_selector: None,
            label_selector: None,
        };
        let (snapshots, receiver) = watch::channel(ResourceSnapshot::loading(7, kind));
        let mut initializing = None;
        let mut progress = InitialListProgress::default();
        let mut revision = 0;

        for index in 0..INITIAL_LIST_PROGRESS_ROWS {
            apply_initial_row(
                &mut initializing,
                Arc::new(ResourceRow::new(
                    "prod",
                    format!("pod-{index}"),
                    "Running",
                    "1/1",
                    "0",
                    0,
                )),
                false,
                &mut progress,
                &snapshots,
                &request,
                &mut revision,
            );
        }

        let snapshot = receiver.borrow();
        assert_eq!(snapshot.rows.len(), INITIAL_LIST_PROGRESS_ROWS);
        assert_eq!(
            snapshot.state,
            SourceState::Loading {
                loaded: INITIAL_LIST_PROGRESS_ROWS
            }
        );
        assert_eq!(snapshot.revision, 1);
    }

    #[test]
    fn decodes_server_printer_columns_cells_and_dates() {
        let page = decode_table_list(serde_json::json!({
            "apiVersion": "meta.k8s.io/v1",
            "kind": "Table",
            "metadata": {"resourceVersion": "42"},
            "columnDefinitions": [
                {"name": "Name", "type": "string", "format": "name", "priority": 0},
                {
                    "name": "Region",
                    "type": "string",
                    "format": null,
                    "priority": null
                },
                {"name": "Endpoint", "type": "string", "priority": 1},
                {"name": "Age", "type": "string", "format": "date", "priority": 0}
            ],
            "rows": [{
                "cells": [
                    "example",
                    "us-west-1",
                    "https://example.invalid",
                    "1970-01-01T01:00:00Z"
                ],
                "object": {
                    "apiVersion": "meta.k8s.io/v1",
                    "kind": "PartialObjectMetadata",
                    "metadata": {
                        "name": "example",
                        "namespace": "demo",
                        "resourceVersion": "42",
                        "creationTimestamp": "1970-01-01T01:00:00Z"
                    }
                }
            }]
        }))
        .expect("table");

        assert_eq!(page.resource_version, "42");
        assert_eq!(page.columns.len(), 4);
        assert_eq!(page.columns[1].format.as_ref(), "");
        assert_eq!(page.columns[1].priority, 0);
        assert_eq!(page.columns[2].name.as_ref(), "Endpoint");
        assert_eq!(page.columns[2].priority, 1);
        assert_eq!(page.rows[0].namespace.as_ref(), "demo");
        assert_eq!(page.rows[0].name.as_ref(), "example");
        assert_eq!(page.rows[0].table_cells[1].text.as_ref(), "us-west-1");
        assert_eq!(page.rows[0].table_cells[3].timestamp_unix, Some(3_600));
    }

    #[test]
    fn table_watch_events_reuse_initial_column_definitions() {
        let columns = vec![
            TableColumn {
                name: Arc::from("Name"),
                format: Arc::from("name"),
                priority: 0,
            },
            TableColumn {
                name: Arc::from("Phase"),
                format: Arc::from(""),
                priority: 0,
            },
        ];
        let event = decode_table_watch_object(
            serde_json::json!({
                "apiVersion": "meta.k8s.io/v1",
                "kind": "Table",
                "metadata": {},
                "rows": [{
                    "cells": ["example", "Ready"],
                    "object": {
                        "apiVersion": "example.io/v1",
                        "kind": "Widget",
                        "metadata": {
                            "name": "example",
                            "namespace": "demo",
                            "resourceVersion": "43"
                        }
                    }
                }]
            }),
            &columns,
        )
        .expect("watch table");

        assert_eq!(event.columns.as_ref(), columns);
        assert_eq!(event.resource_version, "43");
        assert_eq!(event.rows[0].name.as_ref(), "example");
        assert_eq!(event.rows[0].table_cells[1].text.as_ref(), "Ready");
    }

    #[test]
    fn formats_table_cells_like_kubectl() {
        assert_eq!(table_cell(&serde_json::Value::Null, None).text.as_ref(), "");
        assert_eq!(
            table_cell(&serde_json::json!("first\nsecond"), None)
                .text
                .as_ref(),
            "first..."
        );
        assert_eq!(
            table_cell(&serde_json::json!(["one", "two"]), None)
                .text
                .as_ref(),
            "[one two]"
        );
        let created_at = TableColumn {
            name: Arc::from("Created At"),
            format: Arc::from(""),
            priority: 0,
        };
        assert_eq!(
            table_cell(
                &serde_json::json!("1970-01-01T01:00:00Z"),
                Some(&created_at)
            )
            .timestamp_unix,
            Some(3_600)
        );
    }

    #[tokio::test]
    #[ignore = "requires access to a Kubernetes cluster"]
    async fn live_dynamic_api_uses_server_tables_for_native_crd_and_aggregated_resources() {
        let client = Client::try_default().await.expect("Kubernetes client");
        let resources = discover_resources(client.clone())
            .await
            .expect("API discovery");
        for qualified_name in [
            "secrets",
            "customresourcedefinitions.apiextensions.k8s.io",
            "nodes.metrics.k8s.io",
        ] {
            let resource = resources
                .iter()
                .find(|resource| resource.qualified_name() == qualified_name)
                .unwrap_or_else(|| panic!("missing {qualified_name}"));
            let api = dynamic_api(client.clone(), resource, None);
            let listing = list_table_pages_with_progress(&api, &client, None, None, |_, _, _| {})
                .await
                .unwrap_or_else(|error| panic!("table list {qualified_name}: {error}"));
            assert!(
                !listing.columns.is_empty(),
                "no Table columns for {qualified_name}"
            );
            assert!(!listing.rows.is_empty(), "no {qualified_name} rows");
        }
    }

    #[tokio::test]
    #[ignore = "requires KUX_LIVE_CONTEXT and access to a large Kubernetes cluster"]
    async fn live_large_dynamic_resource_table_list() {
        let context = std::env::var("KUX_LIVE_CONTEXT").expect("KUX_LIVE_CONTEXT");
        let qualified_name = std::env::var("KUX_LIVE_RESOURCE")
            .unwrap_or_else(|_| "applications.argoproj.io".to_owned());
        let client = client_for_context(&context)
            .await
            .expect("Kubernetes client");
        let resources = discover_resources(client.clone())
            .await
            .expect("API discovery");
        let resource = resources
            .iter()
            .find(|resource| resource.qualified_name() == qualified_name)
            .unwrap_or_else(|| panic!("missing {qualified_name}"));
        let api = dynamic_api(client.clone(), resource, None);
        let started = Instant::now();
        let mut progress = Vec::new();
        let listing =
            list_table_pages_with_progress(&api, &client, None, None, |_, rows, complete| {
                progress.push((rows.len(), complete, started.elapsed()));
            })
            .await
            .unwrap_or_else(|error| panic!("table list {qualified_name}: {error}"));
        eprintln!(
            "listed {} {qualified_name} rows in {:?}",
            listing.rows.len(),
            started.elapsed()
        );
        eprintln!("progressive pages: {progress:?}");
        assert!(!listing.columns.is_empty(), "no Table columns");
        assert!(!listing.rows.is_empty(), "no {qualified_name} rows");
        assert!(
            listing
                .rows
                .values()
                .any(|row| row.namespace.as_ref() != "-")
        );
        assert!(progress.first().is_some_and(|(loaded, _, _)| *loaded > 0));
        assert!(progress.last().is_some_and(|(_, complete, _)| *complete));
    }
}
