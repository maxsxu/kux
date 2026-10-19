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
    Api, Client, Resource, ResourceExt,
    api::{
        DeleteParams, EvictParams, ListParams, LogParams, Patch, PatchParams, Preconditions,
        WatchParams,
    },
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
    connection::ConnectionOptions,
    model::{
        ChildResourceData, ContainerData, NamespaceInfo, NodeData, OwnerReferenceData, PodData,
        PolicyData, PortForwardPort, ResourceData, ResourceDescriptor, ResourceKind, ResourceRow,
        ResourceSnapshot, SourceState, TableCell, TableColumn,
    },
};

use super::{
    ActionRequest, ActionResult, KubernetesSettings, WatchRequest, YamlContent,
    discovery::discover_resources, logs::LogTask, refresh::RefreshSchedule,
};

#[path = "workload_actions.rs"]
pub(super) mod workload_actions;

#[path = "resource_views.rs"]
pub(crate) mod resource_views;

#[path = "xray.rs"]
pub(crate) mod xray;

struct PodWatchCache {
    context: String,
    receiver: watch::Receiver<ResourceSnapshot>,
    task: tokio::task::JoinHandle<()>,
}

impl PodWatchCache {
    fn spawn(client: Client, settings: KubernetesSettings, context: String) -> Self {
        let request = WatchRequest {
            instance: None,
            generation: 0,
            context: context.clone(),
            kind: ResourceKind::Pods.into(),
            namespace: None,
            field_selector: None,
            label_selector: None,
        };
        let (sender, receiver) = watch::channel(ResourceSnapshot::loading(
            request.generation,
            request.kind.clone(),
        ));
        let task = tokio::spawn(run_watch(client, settings, request, sender));
        Self {
            context,
            receiver,
            task,
        }
    }

    fn matches(&self, request: &WatchRequest) -> bool {
        is_all_namespace_pod_request(request, &self.context)
    }

    fn publish_current(&self, request: &WatchRequest, snapshots: &watch::Sender<ResourceSnapshot>) {
        let mut snapshot = self.receiver.borrow().clone();
        snapshot.generation = request.generation;
        snapshot.kind.clone_from(&request.kind);
        snapshot.emitted_at = Instant::now();
        let _ = snapshots.send(snapshot);
    }
}

fn is_all_namespace_pod_request(request: &WatchRequest, context: &str) -> bool {
    request.context == context
        && request.instance.is_none()
        && request.kind.known_kind() == Some(ResourceKind::Pods)
        && request.namespace.is_none()
        && request.field_selector.is_none()
        && request.label_selector.is_none()
}

impl Drop for PodWatchCache {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub async fn run_manager(
    client: Client,
    settings: KubernetesSettings,
    mut requests: mpsc::Receiver<WatchRequest>,
    snapshots: watch::Sender<ResourceSnapshot>,
    mut actions: mpsc::Receiver<ActionRequest>,
    action_results: mpsc::Sender<ActionResult>,
) {
    let mut client = client;
    let mut active_context = settings.initial_context.clone();
    refresh_cluster_metadata(
        client.clone(),
        active_context.clone(),
        action_results.clone(),
    );
    let mut pod_cache =
        PodWatchCache::spawn(client.clone(), settings.clone(), active_context.clone());
    let mut active_request: Option<WatchRequest> = None;
    let mut running: Option<tokio::task::JoinHandle<()>> = None;
    let mut running_read_action: Option<tokio::task::JoinHandle<()>> = None;
    let mut running_logs: Option<LogTask> = None;
    loop {
        tokio::select! {
            Some(request) = requests.recv() => {
                running_logs.take();
                if let Some(task) = running_read_action.take() {
                    task.abort();
                }
                if request.context != active_context {
                    match client_for_context_with(&settings.connection, &request.context).await {
                        Ok(next_client) => {
                            client = next_client;
                            active_context.clone_from(&request.context);
                            pod_cache = PodWatchCache::spawn(
                                client.clone(),
                                settings.clone(),
                                active_context.clone(),
                            );
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
                active_request = Some(request.clone());
                if pod_cache.matches(&request) {
                    pod_cache.publish_current(&request, &snapshots);
                } else {
                    let client = client.clone();
                    let settings = settings.clone();
                    let snapshots = snapshots.clone();
                    running = Some(tokio::spawn(async move {
                        run_watch(client, settings, request, snapshots).await;
                    }));
                }
            }
            changed = pod_cache.receiver.changed() => {
                if changed.is_err() {
                    pod_cache = PodWatchCache::spawn(
                        client.clone(),
                        settings.clone(),
                        active_context.clone(),
                    );
                } else if let Some(request) = active_request
                    .as_ref()
                    .filter(|request| pod_cache.matches(request))
                {
                    pod_cache.publish_current(request, &snapshots);
                }
            }
            Some(action) = actions.recv() => {
                sync_renamed_active_context(
                    &action.intent,
                    &mut active_context,
                    &mut active_request,
                    &mut pod_cache,
                );
                dispatch_action(
                    client.clone(),
                    active_request.as_ref(),
                    settings.connection.clone(),
                    action,
                    action_results.clone(),
                    &mut running_read_action,
                    &mut running_logs,
                );
            }
            () = snapshots.closed() => break,
            else => break,
        }
    }
    if let Some(task) = running {
        task.abort();
    }
    if let Some(task) = running_read_action {
        task.abort();
    }
}

fn sync_renamed_active_context(
    intent: &ActionIntent,
    active_context: &mut String,
    active_request: &mut Option<WatchRequest>,
    pod_cache: &mut PodWatchCache,
) {
    if let ActionIntent::RenameContext { old_name, new_name } = intent
        && active_context == old_name
    {
        active_context.clone_from(new_name);
        if let Some(req) = active_request.as_mut() {
            req.context.clone_from(new_name);
        }
        pod_cache.context.clone_from(new_name);
    }
}

fn dispatch_action(
    client: Client,
    active_request: Option<&WatchRequest>,
    connection: ConnectionOptions,
    action: ActionRequest,
    results: mpsc::Sender<ActionResult>,
    running_read: &mut Option<tokio::task::JoinHandle<()>>,
    running_logs: &mut Option<LogTask>,
) {
    let Some(request) = active_request.filter(|request| request.generation == action.generation)
    else {
        let message =
            "Action canceled because the active view changed; retry in the current view".to_owned();
        if let Err(mpsc::error::TrySendError::Full(result)) =
            results.try_send(action.rejected(message))
        {
            tokio::spawn(async move {
                let _ = results.send(result).await;
            });
        }
        return;
    };
    let context = request.context.clone();
    match &action.intent {
        ActionIntent::StartLogs { request } => {
            if let Some(task) = running_read.take() {
                task.abort();
            }
            *running_logs = Some(LogTask::spawn(
                client,
                connection,
                action.generation,
                request.clone(),
                results,
            ));
        }
        ActionIntent::StopLogs { id } => LogTask::stop_if_matching(running_logs, *id),
        ActionIntent::StopContentRefresh => {
            if let Some(task) = running_read.take() {
                task.abort();
            }
        }
        _ => {
            let cancelable = action.intent.cancelable_read();
            if cancelable {
                running_logs.take();
            }
            let task = spawn_action_task(client, context, connection, action, results, cancelable);
            if cancelable && let Some(previous) = running_read.replace(task) {
                previous.abort();
            }
        }
    }
}

fn spawn_action_task(
    client: Client,
    context: String,
    connection: ConnectionOptions,
    action: ActionRequest,
    action_results: mpsc::Sender<ActionResult>,
    scoped: bool,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let scoped = scoped
            || matches!(
                &action.intent,
                ActionIntent::SetImages { .. }
                    | ActionIntent::SanitizePods { .. }
                    | ActionIntent::Rollback { .. }
            );
        let result = handle_action(client, &context, &connection, action.intent).await;
        let result = if scoped {
            ActionResult::Scoped {
                generation: action.generation,
                result: Box::new(result),
            }
        } else {
            result
        };
        let _ = action_results.send(result).await;
    })
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
        let Ok(namespaces) = Api::<Namespace>::all(client.clone())
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
        if let (Ok(nodes), Ok(node_metrics)) = (
            Api::<Node>::all(client.clone())
                .list(&ListParams::default())
                .await,
            fetch_node_metrics(client).await,
        ) {
            let mut alloc_cpu = 0_u64;
            let mut alloc_mem = 0_u64;
            for node in &nodes.items {
                let row = node_row(node);
                if let Some(nd) = row.node_data() {
                    alloc_cpu = alloc_cpu.saturating_add(nd.cpu_allocatable_millis);
                    alloc_mem = alloc_mem.saturating_add(nd.memory_allocatable_bytes);
                }
            }
            let mut curr_cpu = 0_u64;
            let mut curr_mem = 0_u64;
            for usage in node_metrics.values() {
                curr_cpu = curr_cpu.saturating_add(usage.cpu_millis.unwrap_or(0));
                curr_mem = curr_mem.saturating_add(usage.memory_bytes.unwrap_or(0));
            }
            let cpu = curr_cpu
                .saturating_mul(100)
                .checked_div(alloc_cpu)
                .map_or_else(|| "n/a".to_owned(), |pct| format!("{pct}%"));
            let mem = curr_mem
                .saturating_mul(100)
                .checked_div(alloc_mem)
                .map_or_else(|| "n/a".to_owned(), |pct| format!("{pct}%"));
            let _ = action_results
                .send(ActionResult::ClusterMetrics { cpu, mem })
                .await;
        }
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

async fn client_for_context_with(
    connection: &ConnectionOptions,
    context: &str,
) -> Result<Client, String> {
    let client = connection.client(Some(context)).await?;
    client.apiserver_version().await.map_err(|error| {
        connection.redact(&format!(
            "Unable to authenticate context {context:?}: {error}"
        ))
    })?;
    Ok(client)
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
    if request.instance.is_some() {
        resource_views::run_instance(client, settings, request, snapshots).await;
        return;
    }
    if request.kind.is_workloads() {
        resource_views::run_workloads(client, settings, request, snapshots).await;
        return;
    }
    if let Some(kind) = request.kind.subject_kind() {
        resource_views::run_subjects(client, settings, request, snapshots, kind).await;
        return;
    }
    if request.kind.is_helm() {
        resource_views::run_helm(client, settings, request, snapshots).await;
        return;
    }
    if request.kind.is_helm_history() {
        resource_views::run_helm_history(client, settings, request, snapshots).await;
        return;
    }
    if request.kind.is_dir() {
        resource_views::run_dir(client, settings, request, snapshots).await;
        return;
    }
    if request.kind.is_benchmarks() {
        resource_views::run_benchmarks(client, settings, request, snapshots).await;
        return;
    }
    if request.kind.is_pulses() {
        resource_views::run_pulses(client, settings, request, snapshots).await;
        return;
    }
    if request.kind.is_xray() {
        resource_views::run_xray(client, settings, request, snapshots).await;
        return;
    }
    if request.kind.is_scans() {
        resource_views::run_scans(client, settings, request, snapshots).await;
        return;
    }
    if request.kind.is_skins() {
        resource_views::run_skins(client, settings, request, snapshots).await;
        return;
    }
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
                // List items may omit TypeMeta. The requested resource, not each
                // object's kind/apiVersion, determines the APIService projection.
                watch_api(api, client, settings, request, snapshots, api_service_row).await;
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

async fn handle_action(
    client: Client,
    context: &str,
    connection: &ConnectionOptions,
    action: ActionIntent,
) -> ActionResult {
    execute_action_intent(client, context, connection, action)
        .await
        .unwrap_or_else(|error| ActionResult::Error(connection.redact(&error)))
}

#[allow(clippy::too_many_lines)]
async fn execute_action_intent(
    client: Client,
    context: &str,
    connection: &ConnectionOptions,
    action: ActionIntent,
) -> Result<ActionResult, String> {
    match action {
        ActionIntent::LoadBatchNavigation { id, target } => {
            Ok(batch_navigation_result(client, connection, id, target).await)
        }
        ActionIntent::LoadReferences { id, target } => {
            Ok(references_result(client, connection, id, target).await)
        }
        ActionIntent::LoadDeploymentSelector {
            id,
            target,
            replica_sets,
        } => Ok(deployment_selector_result(client, connection, id, target, replica_sets).await),
        ActionIntent::LoadRollback { id, target } => {
            Ok(load_rollback_result(client, connection, id, target).await)
        }
        ActionIntent::Rollback { snapshot } => {
            Ok(rollback_result(client, connection, &snapshot).await)
        }
        ActionIntent::LoadTransferPod { id, target } => {
            Ok(transfer_pod_result(client, connection, id, target).await)
        }
        ActionIntent::SanitizePods { namespace } => {
            Ok(sanitize_result(client, connection, namespace).await)
        }
        ActionIntent::LoadImages { id, target } => {
            let result = super::image_actions::load_images(client, &target)
                .await
                .map_err(|error| connection.redact(&error));
            Ok(ActionResult::Images { id, target, result })
        }
        ActionIntent::SetImages { snapshot, images } => {
            Ok(update_images_result(client, connection, snapshot, &images).await)
        }
        ActionIntent::Delete { targets, immediate } => {
            delete_resources(client, connection, context, targets, immediate).await
        }
        ActionIntent::DeleteContext { name } => delete_context(connection, context, &name).await,
        ActionIntent::RenameContext { old_name, new_name } => {
            rename_context(connection, context, &old_name, &new_name).await
        }
        ActionIntent::Content {
            id,
            target,
            content,
        } => Ok(open_content(client, context, connection, id, target, content).await),
        ActionIntent::RefreshYaml {
            id,
            target,
            include_managed_fields,
        } => Ok(refresh_yaml(client, connection, id, target, include_managed_fields).await),
        ActionIntent::ManagedYaml {
            id,
            target,
            body,
            decoded_body,
        } => Ok(managed_yaml_result(client, connection, id, target, body, decoded_body).await),
        ActionIntent::RefreshDescribe { id, target } => {
            Ok(refresh_describe(connection, context, id, target).await)
        }
        ActionIntent::StopContentRefresh => {
            Err("Content refresh cancellation is handled by the source manager".to_owned())
        }
        ActionIntent::Containers { target } => load_containers(client, target).await,
        ActionIntent::ContainerLogs { target, previous } => {
            fetch_container_logs(client, target, previous).await
        }
        ActionIntent::StartLogs { .. } | ActionIntent::StopLogs { .. } => {
            Err("Log sessions are handled by the source manager".to_owned())
        }
        ActionIntent::AuthorizationPolicy { target } => {
            load_authorization_policy(client, target).await
        }
        ActionIntent::FindChildResource { target, resources } => {
            find_child_resource(client, target, resources).await
        }
        ActionIntent::ResolveOwner { target } => resolve_owner(client, target).await,
        ActionIntent::Restart { targets } => restart_workloads(client, targets).await,
        ActionIntent::Scale { targets, replicas } => {
            workload_actions::scale_workloads(client, targets, replicas).await
        }
        ActionIntent::TriggerCronJob { targets } => {
            workload_actions::trigger_cronjobs(client, targets).await
        }
        ActionIntent::ToggleCronJobSuspend { target } => {
            workload_actions::toggle_cronjob_suspend(client, target).await
        }
        ActionIntent::SetSuspended { targets, suspended } => {
            workload_actions::set_suspended(client, targets, suspended).await
        }
        ActionIntent::SetNodeUnschedulable {
            name,
            unschedulable,
        } => set_node_unschedulable(client, &name, unschedulable).await,
        ActionIntent::DrainNode { name, options } => drain_node(client, &name, options).await,
        ActionIntent::HelmRollback {
            namespace,
            release,
            revision,
        } => helm_rollback(connection, context, &namespace, &release, &revision).await,
        ActionIntent::DirApply { path } => {
            dir_kubectl_action(connection, context, "apply", &path).await
        }
        ActionIntent::DirDelete { path } => {
            dir_kubectl_action(connection, context, "delete", &path).await
        }
    }
}

async fn batch_navigation_result(
    client: Client,
    connection: &ConnectionOptions,
    id: u64,
    target: ResourceTarget,
) -> ActionResult {
    let result = match connection.api_call_timeout() {
        Ok(timeout) => super::batch_navigation::load_navigation(client, &target, timeout).await,
        Err(error) => Err(error),
    }
    .map_err(|error| connection.redact(&error));
    ActionResult::BatchNavigation { id, target, result }
}

async fn references_result(
    client: Client,
    connection: &ConnectionOptions,
    id: u64,
    target: ResourceTarget,
) -> ActionResult {
    let result = match connection.api_call_timeout() {
        Ok(timeout) => super::references::scan_references(client, &target, timeout).await,
        Err(error) => Err(error),
    }
    .map(|mut report| {
        report.errors = report
            .errors
            .into_iter()
            .map(|error| connection.redact(&error).chars().take(1_024).collect())
            .collect();
        report
    })
    .map_err(|error| connection.redact(&error));
    ActionResult::References { id, target, result }
}

async fn transfer_pod_result(
    client: Client,
    connection: &ConnectionOptions,
    id: u64,
    target: ResourceTarget,
) -> ActionResult {
    let result = crate::transfer::load_pod(client, &target)
        .await
        .map_err(|error| connection.redact(&error));
    ActionResult::TransferPod { id, target, result }
}

async fn managed_yaml_result(
    client: Client,
    connection: &ConnectionOptions,
    id: u64,
    target: ResourceTarget,
    body: String,
    decoded_body: Option<String>,
) -> ActionResult {
    let result = tokio::time::timeout(
        Duration::from_secs(10),
        fetch_managed_yaml(client, &target, body, decoded_body),
    )
    .await
    .unwrap_or_else(|_| Err("request timed out".to_owned()))
    .map_err(|error| connection.redact(&error));
    ActionResult::ManagedYaml { id, target, result }
}

async fn load_rollback_result(
    client: Client,
    connection: &ConnectionOptions,
    id: u64,
    target: ResourceTarget,
) -> ActionResult {
    let result = super::rollback::load_snapshot(client, &target)
        .await
        .map_err(|error| connection.redact(&error));
    ActionResult::RollbackLoaded { id, target, result }
}

async fn deployment_selector_result(
    client: Client,
    connection: &ConnectionOptions,
    id: u64,
    target: ResourceTarget,
    replica_sets: bool,
) -> ActionResult {
    let result = super::deployment_navigation::load_selector(client, &target)
        .await
        .map_err(|error| connection.redact(&error));
    ActionResult::DeploymentSelector {
        id,
        target,
        replica_sets,
        result,
    }
}

async fn rollback_result(
    client: Client,
    connection: &ConnectionOptions,
    snapshot: &super::RollbackSnapshot,
) -> ActionResult {
    let result = match connection.api_call_timeout() {
        Ok(timeout) => super::rollback::rollback(client, snapshot, timeout).await,
        Err(error) => Err(error),
    }
    .map_err(|error| connection.redact(&error));
    ActionResult::RolledBack { result }
}

async fn update_images_result(
    client: Client,
    connection: &ConnectionOptions,
    snapshot: super::ImageSnapshot,
    images: &[super::ContainerImage],
) -> ActionResult {
    let result = super::image_actions::update_images(client, &snapshot, images)
        .await
        .map(|changed| {
            format!(
                "Updated {changed} container image(s) on {} {}",
                snapshot.target.kind.kind,
                resource_identity(&snapshot.target),
            )
        })
        .map_err(|error| connection.redact(&error));
    ActionResult::ImagesUpdated {
        target: snapshot.target,
        result,
    }
}

async fn sanitize_result(
    client: Client,
    connection: &ConnectionOptions,
    namespace: Option<String>,
) -> ActionResult {
    let result = match connection.api_call_timeout() {
        Ok(timeout) => super::pod_actions::sanitize_pods(client, namespace.clone(), timeout).await,
        Err(error) => Err(error),
    }
    .map(|mut report| {
        report.failure = report.failure.map(|error| connection.redact(&error));
        report
    })
    .map_err(|error| connection.redact(&error));
    ActionResult::Sanitized { namespace, result }
}

async fn open_content(
    client: Client,
    context: &str,
    connection: &ConnectionOptions,
    id: u64,
    target: ResourceTarget,
    kind: ContentKind,
) -> ActionResult {
    let result = tokio::time::timeout(
        Duration::from_secs(10),
        fetch_content(client, context, connection, &target, kind),
    )
    .await
    .unwrap_or_else(|_| Err("request timed out".to_owned()))
    .unwrap_or_else(|error| ActionResult::Error(connection.redact(&error)));
    ActionResult::OpenedContent {
        id,
        target,
        result: Box::new(result),
    }
}

async fn refresh_yaml(
    client: Client,
    connection: &ConnectionOptions,
    id: u64,
    target: ResourceTarget,
    include_managed_fields: bool,
) -> ActionResult {
    let result = tokio::time::timeout(Duration::from_secs(10), async {
        let object = dynamic_target_api(client, &target)?
            .get(&target.name)
            .await
            .map_err(|error| error.to_string())?;
        let target = target.clone();
        tokio::task::spawn_blocking(move || {
            yaml_content_with_managed_fields(object, &target, include_managed_fields)
        })
        .await
        .map_err(|error| format!("Unable to format YAML: {error}"))?
    })
    .await
    .unwrap_or_else(|_| Err("request timed out".to_owned()))
    .map_err(|error| connection.redact(&error));
    ActionResult::RefreshedYaml { id, target, result }
}

async fn resolve_owner(client: Client, target: ResourceTarget) -> Result<ActionResult, String> {
    let object = dynamic_target_api(client, &target)?
        .get(&target.name)
        .await
        .map_err(|error| format!("Unable to inspect {}: {error}", resource_identity(&target)))?;
    Ok(ActionResult::Owner {
        target,
        owner: owner_reference_data(object.meta()),
    })
}

async fn find_child_resource(
    client: Client,
    target: ResourceTarget,
    resources: Vec<ResourceDescriptor>,
) -> Result<ActionResult, String> {
    let parent = dynamic_target_api(client.clone(), &target)?
        .get(&target.name)
        .await
        .map_err(|error| format!("Unable to inspect {}: {error}", resource_identity(&target)))?;
    let Some(owner_uid) = parent.meta().uid.clone() else {
        return Ok(ActionResult::NoChildResource { parent: target });
    };

    let namespace = target.namespace.clone();
    for priority in 0..=2 {
        let candidates = resources
            .iter()
            .filter(|kind| child_candidate_priority(&target.kind, kind) == priority)
            .cloned()
            .collect::<Vec<_>>();
        let tasks = candidates.into_iter().map(|kind| {
            let client = client.clone();
            let namespace = namespace.clone();
            let owner_uid = owner_uid.clone();
            async move {
                resource_has_owner(client, &kind, namespace.as_deref(), &owner_uid)
                    .await
                    .then_some(kind)
            }
        });
        let mut matches = futures::stream::iter(tasks).buffer_unordered(32);
        while let Some(kind) = matches.next().await {
            if let Some(kind) = kind {
                return Ok(ActionResult::ChildResource {
                    parent: target,
                    kind,
                    owner_uid: owner_uid.clone(),
                });
            }
        }
    }
    Ok(ActionResult::NoChildResource { parent: target })
}

fn child_candidate_priority(parent: &ResourceDescriptor, candidate: &ResourceDescriptor) -> u8 {
    if candidate.group == parent.group {
        0
    } else if matches!(candidate.group.as_ref(), "" | "apps" | "batch") {
        1
    } else {
        2
    }
}

async fn resource_has_owner(
    client: Client,
    kind: &ResourceDescriptor,
    parent_namespace: Option<&str>,
    owner_uid: &str,
) -> bool {
    let api = dynamic_api(
        client,
        kind,
        kind.is_namespaced().then_some(parent_namespace).flatten(),
    );
    let mut continue_token: Option<String> = None;
    loop {
        let mut params = ListParams::default().limit(TABLE_MAX_PAGE_SIZE);
        if let Some(token) = &continue_token {
            params = params.continue_token(token);
        }
        let Ok(page) = api.list_metadata(&params).await else {
            return false;
        };
        if page.items.iter().any(|item| {
            item.metadata
                .owner_references
                .as_ref()
                .is_some_and(|owners| owners.iter().any(|owner| owner.uid == owner_uid))
        }) {
            return true;
        }
        continue_token = page.metadata.continue_;
        if continue_token.as_deref().is_none_or(str::is_empty) {
            return false;
        }
    }
}

async fn delete_resources(
    client: Client,
    connection: &ConnectionOptions,
    context: &str,
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
        let connection = connection.clone();
        let context = context.to_owned();
        async move {
            let identity = resource_identity(&target);
            (
                identity,
                delete_resource(client, &connection, &context, &target, immediate).await,
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
    connection: &ConnectionOptions,
    context: &str,
    target: &ResourceTarget,
    immediate: bool,
) -> Result<(), String> {
    if target.kind.is_helm() {
        let ns = target.namespace.as_deref().unwrap_or("default");
        let mut args = connection.helm_args(Some(context));
        args.extend(
            ["uninstall", &target.name, "-n", ns]
                .into_iter()
                .map(std::ffi::OsString::from),
        );
        let output = Command::new("helm")
            .args(args)
            .kill_on_drop(true)
            .output()
            .await
            .map_err(|error| format!("Unable to run helm uninstall: {error}"))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            return Err(if stderr.is_empty() {
                format!("helm uninstall failed with {}", output.status)
            } else {
                connection.redact(&stderr)
            });
        }
        return Ok(());
    }
    if target.kind.is_benchmarks() {
        let path = target.name.clone();
        tokio::task::spawn_blocking(move || {
            std::fs::remove_file(&path)
                .map_err(|error| format!("Unable to delete benchmark report {path}: {error}"))
        })
        .await
        .map_err(|error| format!("Benchmark delete task failed: {error}"))??;
        return Ok(());
    }
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

fn context_delete_args_with(connection: &ConnectionOptions, name: &str) -> Vec<std::ffi::OsString> {
    let mut args = connection.kubectl_args(None);
    args.extend(context_delete_args(name).map(std::ffi::OsString::from));
    args
}

fn context_rename_args<'a>(old_name: &'a str, new_name: &'a str) -> [&'a str; 4] {
    ["config", "rename-context", old_name, new_name]
}

fn context_rename_args_with(
    connection: &ConnectionOptions,
    old_name: &str,
    new_name: &str,
) -> Vec<std::ffi::OsString> {
    let mut args = connection.kubectl_args(None);
    args.extend(context_rename_args(old_name, new_name).map(std::ffi::OsString::from));
    args
}

async fn rename_context(
    connection: &ConnectionOptions,
    active_context: &str,
    old_name: &str,
    new_name: &str,
) -> Result<ActionResult, String> {
    let output = Command::new("kubectl")
        .args(context_rename_args_with(connection, old_name, new_name))
        .output()
        .await
        .map_err(|error| format!("Unable to run kubectl config rename-context: {error}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return Err(if stderr.is_empty() {
            format!(
                "kubectl config rename-context failed with {}",
                output.status
            )
        } else {
            connection.redact(&stderr)
        });
    }
    let next_active = if active_context == old_name {
        new_name
    } else {
        active_context
    };
    let catalog = crate::kubeconfig::read_contexts_with(connection, Some(next_active))?;
    Ok(ActionResult::ContextOptions {
        context: next_active.to_owned(),
        contexts: catalog.contexts,
        notice: format!("Renamed context {old_name} to {new_name}"),
    })
}

async fn delete_context(
    connection: &ConnectionOptions,
    active_context: &str,
    name: &str,
) -> Result<ActionResult, String> {
    let output = Command::new("kubectl")
        .args(context_delete_args_with(connection, name))
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
            connection.redact(&stderr)
        });
    }
    let catalog = crate::kubeconfig::read_contexts_with(connection, Some(active_context))?;
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
    connection: &ConnectionOptions,
    target: &ResourceTarget,
    content_kind: ContentKind,
) -> Result<ActionResult, String> {
    if target.kind.is_helm() || target.kind.is_helm_history() {
        return fetch_helm_content(connection, context, target, content_kind).await;
    }
    if target.kind.is_dir() {
        return fetch_dir_content(target, content_kind).await;
    }
    if target.kind.is_benchmarks() {
        return fetch_benchmark_content(target, content_kind).await;
    }
    if content_kind == ContentKind::SecretData {
        return fetch_secret_data(client, target).await;
    }
    if content_kind == ContentKind::Describe {
        return describe_resource(connection, context, target).await;
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

async fn fetch_managed_yaml(
    client: Client,
    target: &ResourceTarget,
    body: String,
    decoded_body: Option<String>,
) -> Result<(String, Option<String>), String> {
    let object = dynamic_target_api(client, target)?
        .get(&target.name)
        .await
        .map_err(|error| error.to_string())?;
    tokio::task::spawn_blocking(move || {
        managed_yaml_content_from_object(&object, &body, decoded_body.as_deref())
    })
    .await
    .map_err(|error| format!("Unable to format managed-fields YAML: {error}"))?
}

fn managed_yaml_content_from_object(
    object: &DynamicObject,
    body: &str,
    decoded_body: Option<&str>,
) -> Result<(String, Option<String>), String> {
    let Some(fields) = object.metadata.managed_fields.as_ref() else {
        return Ok((body.to_owned(), decoded_body.map(str::to_owned)));
    };
    let fragment = serde_yaml::to_string(&BTreeMap::from([("managedFields", fields)]))
        .map_err(|error| error.to_string())?;
    Ok((
        insert_managed_fields_yaml(body, &fragment)?,
        decoded_body
            .map(|body| insert_managed_fields_yaml(body, &fragment))
            .transpose()?,
    ))
}

/// Add only the managed-fields entry to the already displayed snapshot. Reusing
/// its text preserves every other value, including encoded/decoded Secret data.
pub(super) fn insert_managed_fields_yaml(body: &str, fragment: &str) -> Result<String, String> {
    let header = "metadata:\n";
    let mut offset = body
        .match_indices(header)
        .find(|(index, _)| *index == 0 || body.as_bytes()[index - 1] == b'\n')
        .map(|(index, _)| index + header.len())
        .ok_or_else(|| "Displayed YAML has no metadata mapping".to_owned())?;
    // ObjectMeta keys are serialized in dictionary order. Skip complete earlier
    // entries, including their nested mappings and indentless YAML sequences.
    for line in body[offset..].split_inclusive('\n') {
        if !line.starts_with(' ') && !line.trim().is_empty() {
            break;
        }
        let key = line
            .strip_prefix("  ")
            .and_then(|entry| entry.split_once(':'));
        if key.is_some_and(|(key, _)| !key.starts_with([' ', '-']) && key > "managedFields") {
            break;
        }
        offset += line.len();
    }
    let mut result = String::with_capacity(body.len() + fragment.len());
    result.push_str(&body[..offset]);
    for line in fragment.lines() {
        let _ = writeln!(result, "  {line}");
    }
    result.push_str(&body[offset..]);
    Ok(result)
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

async fn refresh_describe(
    connection: &ConnectionOptions,
    context: &str,
    id: u64,
    target: ResourceTarget,
) -> ActionResult {
    let result = tokio::time::timeout(
        Duration::from_secs(10),
        describe_resource(connection, context, &target),
    )
    .await
    .unwrap_or_else(|_| Err("request timed out".to_owned()))
    .and_then(|result| match result {
        ActionResult::Content { body, .. } => Ok(body),
        _ => Err("Describe returned no content".to_owned()),
    })
    .map_err(|error| connection.redact(&error));
    ActionResult::RefreshedDescribe { id, target, result }
}

async fn describe_resource(
    connection: &ConnectionOptions,
    context: &str,
    target: &ResourceTarget,
) -> Result<ActionResult, String> {
    let output = Command::new("kubectl")
        .args(describe_args_with(connection, context, target))
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|error| format!("Unable to run kubectl describe: {error}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return Err(if stderr.is_empty() {
            format!("kubectl describe failed with {}", output.status)
        } else {
            connection.redact(&stderr)
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

fn describe_args_with(
    connection: &ConnectionOptions,
    context: &str,
    target: &ResourceTarget,
) -> Vec<std::ffi::OsString> {
    let mut args = connection.kubectl_args(None);
    args.extend(
        describe_args(context, target)
            .into_iter()
            .map(std::ffi::OsString::from),
    );
    args
}

async fn run_helm_stdout(
    connection: &ConnectionOptions,
    context: &str,
    sub_args: &[&str],
) -> Result<String, String> {
    let mut args = connection.helm_args(Some(context));
    args.extend(sub_args.iter().map(std::ffi::OsString::from));
    let output = Command::new("helm")
        .args(args)
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|error| format!("Unable to run helm: {error}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return Err(if stderr.is_empty() {
            format!("helm failed with {}", output.status)
        } else {
            connection.redact(&stderr)
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn split_helm_target(target: &ResourceTarget) -> (&str, &str, Option<&str>) {
    let namespace = target.namespace.as_deref().unwrap_or("default");
    if target.kind.is_helm_history()
        && let Some((release, rev)) = target.name.split_once(':')
    {
        (namespace, release, Some(rev))
    } else {
        (namespace, &target.name, None)
    }
}

async fn fetch_helm_content(
    connection: &ConnectionOptions,
    context: &str,
    target: &ResourceTarget,
    content_kind: ContentKind,
) -> Result<ActionResult, String> {
    let (namespace, release, revision) = split_helm_target(target);
    let rev_suffix = revision.map_or_else(String::new, |rev| format!(":{rev}"));
    match content_kind {
        ContentKind::Yaml => {
            let mut args = vec!["get", "manifest", release, "-n", namespace];
            if let Some(rev) = revision {
                args.extend(["--revision", rev]);
            }
            let body = run_helm_stdout(connection, context, &args).await?;
            Ok(ActionResult::Content {
                title: format!("YAML({namespace}/{release}{rev_suffix})"),
                body,
                managed_fields: None,
                decoded_body: None,
                decoded_managed_fields: None,
                content: ContentKind::Yaml,
            })
        }
        ContentKind::Describe => {
            let mut args = vec!["get", "notes", release, "-n", namespace];
            if let Some(rev) = revision {
                args.extend(["--revision", rev]);
            }
            let body = run_helm_stdout(connection, context, &args).await?;
            Ok(ActionResult::Content {
                title: format!("Describe({namespace}/{release}{rev_suffix})"),
                body,
                managed_fields: None,
                decoded_body: None,
                decoded_managed_fields: None,
                content: ContentKind::Describe,
            })
        }
        ContentKind::HelmValues => {
            let mut user_args = vec!["get", "values", release, "-n", namespace, "-o", "yaml"];
            let mut all_args = vec![
                "get", "values", release, "-n", namespace, "--all", "-o", "yaml",
            ];
            if let Some(rev) = revision {
                user_args.extend(["--revision", rev]);
                all_args.extend(["--revision", rev]);
            }
            let (user_values, all_values) = tokio::try_join!(
                run_helm_stdout(connection, context, &user_args),
                run_helm_stdout(connection, context, &all_args),
            )?;
            Ok(ActionResult::Content {
                title: format!("Values({namespace}/{release}{rev_suffix})"),
                body: user_values,
                managed_fields: None,
                decoded_body: Some(all_values),
                decoded_managed_fields: None,
                content: ContentKind::HelmValues,
            })
        }
        _ => Err("Unsupported content view for Helm release".to_owned()),
    }
}

async fn fetch_dir_content(
    target: &ResourceTarget,
    content_kind: ContentKind,
) -> Result<ActionResult, String> {
    if content_kind != ContentKind::Yaml {
        return Err("Only YAML view (`y`) is supported for local directory entries".to_owned());
    }
    let path = target.name.clone();
    if std::path::Path::new(&path).extension().is_none() {
        return Err("Select a manifest file to view YAML".to_owned());
    }
    let read_path = path.clone();
    let body = tokio::task::spawn_blocking(move || {
        std::fs::read_to_string(&read_path)
            .map_err(|error| format!("Unable to read {read_path}: {error}"))
    })
    .await
    .map_err(|error| format!("File reader task failed: {error}"))??;
    Ok(ActionResult::Content {
        title: format!("YAML({path})"),
        body,
        managed_fields: None,
        decoded_body: None,
        decoded_managed_fields: None,
        content: ContentKind::Yaml,
    })
}

async fn fetch_benchmark_content(
    target: &ResourceTarget,
    content_kind: ContentKind,
) -> Result<ActionResult, String> {
    if content_kind != ContentKind::Yaml {
        return Err("Only report view (`Enter` / `y`) is supported for benchmarks".to_owned());
    }
    let path = target.name.clone();
    let read_path = path.clone();
    let body = tokio::task::spawn_blocking(move || {
        std::fs::read_to_string(&read_path)
            .map_err(|error| format!("Unable to load bench file {read_path}: {error}"))
    })
    .await
    .map_err(|error| format!("Benchmark reader task failed: {error}"))??;
    let file_name = std::path::Path::new(&path)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(&path);
    let tokens: Vec<_> = file_name.split('_').collect();
    let subject = if tokens.len() >= 2 {
        format!("{}/{}", tokens[0], tokens[1])
    } else {
        file_name.to_owned()
    };
    Ok(ActionResult::Content {
        title: format!("Results({subject})"),
        body,
        managed_fields: None,
        decoded_body: None,
        decoded_managed_fields: None,
        content: ContentKind::Yaml,
    })
}

async fn helm_rollback(
    connection: &ConnectionOptions,
    context: &str,
    namespace: &str,
    release: &str,
    revision: &str,
) -> Result<ActionResult, String> {
    let args = ["rollback", release, revision, "-n", namespace];
    run_helm_stdout(connection, context, &args).await?;
    Ok(ActionResult::Notice(format!(
        "Rolled back Helm release {namespace}/{release} to revision {revision}"
    )))
}

pub(crate) fn is_kustomized_dir(path: &std::path::Path) -> bool {
    if !path.is_dir() {
        return false;
    }
    let Ok(entries) = std::fs::read_dir(path) else {
        return false;
    };
    entries.flatten().any(|entry| {
        matches!(
            entry.file_name().to_str(),
            Some("Kustomization" | "kustomization.yaml" | "kustomization.yml")
        )
    })
}

pub(crate) fn contains_sub_dir(path: &std::path::Path) -> bool {
    if !path.is_dir() {
        return false;
    }
    let Ok(entries) = std::fs::read_dir(path) else {
        return false;
    };
    entries
        .flatten()
        .any(|entry| entry.file_type().is_ok_and(|ft| ft.is_dir()))
}

pub(crate) fn dir_kubectl_flags(path: &str) -> Vec<&'static str> {
    let p = std::path::Path::new(path);
    if is_kustomized_dir(p) {
        vec!["-k"]
    } else if contains_sub_dir(p) {
        vec!["-f", "-R"]
    } else {
        vec!["-f"]
    }
}

fn format_dir_command_results(output: &str) -> String {
    output
        .trim()
        .lines()
        .map(|line| format!("  {line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

async fn dir_kubectl_action(
    connection: &ConnectionOptions,
    context: &str,
    verb: &str,
    path: &str,
) -> Result<ActionResult, String> {
    let mut args = connection.kubectl_args(Some(context));
    args.push(std::ffi::OsString::from(verb));
    for flag in dir_kubectl_flags(path) {
        args.push(std::ffi::OsString::from(flag));
    }
    args.push(std::ffi::OsString::from(path));
    let output = Command::new("kubectl")
        .args(args)
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|error| format!("Unable to run kubectl {verb}: {error}"))?;
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let body = if output.status.success() {
        format!("message:\n{}", format_dir_command_results(&combined))
    } else {
        format!(
            "status:\n  {}\nmessage:\n{}",
            output.status,
            format_dir_command_results(&combined)
        )
    };
    let label = if verb == "apply" {
        "Applied Manifest"
    } else {
        "Deleted Manifest"
    };
    Ok(ActionResult::Content {
        title: format!("{label}({path})"),
        body: connection.redact(&body),
        managed_fields: None,
        decoded_body: None,
        decoded_managed_fields: None,
        content: ContentKind::Yaml,
    })
}

#[derive(Debug, Clone, Copy)]
pub(super) struct ResourceUsage {
    pub cpu_millis: Option<u64>,
    pub memory_bytes: Option<u64>,
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
            | ResourceData::Workload { .. }
            | ResourceData::Reference { .. }
            | ResourceData::ChildResource(_)
            | ResourceData::Container(_)
            | ResourceData::Policy(_)
            | ResourceData::Pulse(_)
            | ResourceData::XRay(_) => {}
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

struct FastInitialList {
    columns: Arc<[TableColumn]>,
    rows: FxHashMap<Arc<str>, Arc<ResourceRow>>,
    hydrated: bool,
    quality: u8,
}

const FAST_INITIAL_LIST_HEAD_START: Duration = Duration::from_secs(5);
const OWNER_UID_FIELD_SELECTOR_PREFIX: &str = "kux.ownerUID=";
const OWNER_NAME_FIELD_SELECTOR_PREFIX: &str = "kux.ownerName=";
const MATCH_NONE_FIELD_SELECTOR: &str = "kux.matchNone=true";

fn owner_name_filter(request: &WatchRequest) -> Option<&str> {
    request
        .field_selector
        .as_deref()
        .and_then(|selector| selector.strip_prefix(OWNER_NAME_FIELD_SELECTOR_PREFIX))
}

fn has_local_field_filter(request: &WatchRequest) -> bool {
    owner_uid_filter(request).is_some()
        || owner_name_filter(request).is_some()
        || request.field_selector.as_deref() == Some(MATCH_NONE_FIELD_SELECTOR)
}

fn owner_uid_filter(request: &WatchRequest) -> Option<&str> {
    request
        .field_selector
        .as_deref()
        .and_then(|selector| selector.strip_prefix(OWNER_UID_FIELD_SELECTOR_PREFIX))
}

fn server_field_selector(request: &WatchRequest) -> Option<&str> {
    request
        .field_selector
        .as_deref()
        .filter(|_| !has_local_field_filter(request))
}

fn spawn_fast_initial_list(
    client: Client,
    request: &WatchRequest,
) -> (mpsc::UnboundedReceiver<FastInitialList>, AbortOnDrop) {
    let (updates, receiver) = mpsc::unbounded_channel();
    let api = dynamic_api(client.clone(), &request.kind, request.namespace.as_deref());
    let field_selector = server_field_selector(request).map(str::to_owned);
    let label_selector = request.label_selector.clone();
    let metadata_free_head = request.namespace.is_none()
        && request.kind.known_kind() == Some(ResourceKind::Pods)
        && !has_local_field_filter(request);
    let task = tokio::spawn(async move {
        if metadata_free_head {
            if let Ok(page) = list_table_head(
                &api,
                &client,
                field_selector.as_deref(),
                label_selector.as_deref(),
                TABLE_FAST_INCLUDE_OBJECT,
            )
            .await
            {
                let rows = page
                    .rows
                    .into_iter()
                    .map(|row| (Arc::clone(&row.key), row))
                    .collect();
                let _ = updates.send(FastInitialList {
                    columns: page.columns,
                    rows,
                    hydrated: false,
                    quality: 0,
                });
            }
            list_pod_pages(
                Api::<Pod>::all(client.clone()),
                field_selector.as_deref(),
                label_selector.as_deref(),
                |rows| {
                    let _ = updates.send(FastInitialList {
                        columns: Arc::from([]),
                        rows: rows.clone(),
                        hydrated: true,
                        quality: 1,
                    });
                },
            )
            .await;
            return;
        }
        let mut sent_first = false;
        let _ = list_table_pages_with_include(
            &api,
            &client,
            field_selector.as_deref(),
            label_selector.as_deref(),
            TABLE_INCLUDE_OBJECT,
            |columns, rows, complete| {
                if sent_first && !complete {
                    return;
                }
                sent_first = true;
                let _ = updates.send(FastInitialList {
                    columns: Arc::clone(columns),
                    rows: rows.clone(),
                    hydrated: false,
                    quality: 1,
                });
            },
        )
        .await;
    });
    (receiver, AbortOnDrop(task))
}

async fn list_table_head(
    api: &Api<DynamicObject>,
    client: &Client,
    field_selector: Option<&str>,
    label_selector: Option<&str>,
    include_object: &str,
) -> Result<DecodedTablePage, String> {
    let mut params = ListParams::default().limit(TABLE_INITIAL_PAGE_SIZE);
    if let Some(selector) = field_selector {
        params = params.fields(selector);
    }
    if let Some(selector) = label_selector {
        params = params.labels(selector);
    }
    let raw = Request::new(api.resource_url())
        .list(&params)
        .map_err(|error| format!("Unable to build Kubernetes Table request: {error}"))?;
    let raw = table_request_with_include(raw, include_object)?;
    let value = client
        .request::<serde_json::Value>(raw)
        .await
        .map_err(|error| format!("Unable to list Kubernetes Table rows: {error}"))?;
    decode_table_list(value)
}

async fn list_pod_pages<F>(
    api: Api<Pod>,
    field_selector: Option<&str>,
    label_selector: Option<&str>,
    mut on_page: F,
) where
    F: FnMut(&FxHashMap<Arc<str>, Arc<ResourceRow>>),
{
    let mut rows = FxHashMap::default();
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
        let Ok(page) = api.list(&params).await else {
            return;
        };
        for pod in &page.items {
            let row = Arc::new(pod_row(pod));
            rows.insert(Arc::clone(&row.key), row);
        }
        on_page(&rows);
        continue_token = page.metadata.continue_;
        if continue_token.as_deref().is_none_or(str::is_empty) {
            return;
        }
        page_size = next_table_page_size(page_size);
    }
}

fn hydrate_fast_initial_rows(
    kind: &ResourceDescriptor,
    columns: &[TableColumn],
    rows: &mut FxHashMap<Arc<str>, Arc<ResourceRow>>,
) {
    for row in rows.values_mut() {
        *row = Arc::new(hydrate_fast_initial_row(
            kind,
            columns,
            row.as_ref().clone(),
        ));
    }
}

fn hydrate_fast_initial_row(
    kind: &ResourceDescriptor,
    columns: &[TableColumn],
    mut row: ResourceRow,
) -> ResourceRow {
    if row.created_unix == 0
        && let Some(age_seconds) = table_cell_text(columns, &row, "age").and_then(parse_table_age)
    {
        row.created_unix =
            crate::model::unix_now().saturating_sub(i64::try_from(age_seconds).unwrap_or(i64::MAX));
    }
    match kind.known_kind() {
        Some(ResourceKind::Pods) => {
            let restarts = table_cell_text(columns, &row, "restarts")
                .and_then(|value| value.split_whitespace().next())
                .and_then(|value| value.parse().ok())
                .unwrap_or(0);
            let node =
                table_cell_text(columns, &row, "node").map_or_else(|| Arc::from("-"), Arc::from);
            row.with_pod_data(PodData {
                restarts,
                node,
                ..PodData::default()
            })
        }
        Some(ResourceKind::Nodes) => {
            let status = row.status.replace("SchedulingDisabled", "Unschedulable");
            let unschedulable = status.contains("Unschedulable");
            let version =
                table_cell_text(columns, &row, "version").map_or_else(|| Arc::from("-"), Arc::from);
            row.status = Arc::from(status);
            row.with_node_data(NodeData {
                unschedulable,
                taints: Arc::from("<none>"),
                version,
                ..NodeData::default()
            })
        }
        Some(
            ResourceKind::Deployments
            | ResourceKind::StatefulSets
            | ResourceKind::DaemonSets
            | ResourceKind::Services,
        ) => {
            row = remap_fast_table_cells(kind, columns, row);
            if let Some(selector) = table_selector(&typed_table_columns(kind), &row) {
                with_child_resource(row, ResourceKind::Pods.into(), selector)
            } else {
                row
            }
        }
        None if kind.is_apps_v1_replica_set() => {
            row = remap_fast_table_cells(kind, columns, row);
            if let Some(selector) = table_selector(&typed_table_columns(kind), &row) {
                with_child_resource(row, ResourceKind::Pods.into(), selector)
            } else {
                row
            }
        }
        _ => row,
    }
}

fn remap_fast_table_cells(
    kind: &ResourceDescriptor,
    server_columns: &[TableColumn],
    mut row: ResourceRow,
) -> ResourceRow {
    let target_cols = typed_table_columns(kind);
    if target_cols.is_empty() || server_columns.is_empty() {
        return row;
    }
    let cells = target_cols
        .iter()
        .map(|target| {
            if target.name.eq_ignore_ascii_case("age") {
                return age_cell(row.created_unix);
            }
            let text = table_cell_text(server_columns, &row, target.name.as_ref())
                .map(|s| {
                    if s == "<none>" {
                        String::new()
                    } else {
                        s.to_owned()
                    }
                })
                .unwrap_or_default();
            plain_cell(text)
        })
        .collect();
    row.table_cells = cells;
    row
}

fn parse_table_age(value: &str) -> Option<u64> {
    let split = value.find(|character: char| !character.is_ascii_digit())?;
    let amount = value[..split].parse::<u64>().ok()?;
    let unit = value[split..].chars().next()?;
    let multiplier = match unit {
        's' => 1,
        'm' => 60,
        'h' => 60 * 60,
        'd' => 24 * 60 * 60,
        'y' => 365 * 24 * 60 * 60,
        _ => return None,
    };
    amount.checked_mul(multiplier)
}

fn table_cell_text<'a>(
    columns: &[TableColumn],
    row: &'a ResourceRow,
    name: &str,
) -> Option<&'a str> {
    columns
        .iter()
        .position(|column| column.name.eq_ignore_ascii_case(name))
        .and_then(|index| row.table_cells.get(index))
        .map(|cell| cell.text.as_ref())
}

fn table_selector(columns: &[TableColumn], row: &ResourceRow) -> Option<String> {
    table_cell_text(columns, row, "selector")
        .filter(|selector| !selector.is_empty() && *selector != "<none>")
        .map(str::to_owned)
}

fn spawn_enrichment_tasks(
    client: Client,
    request: &WatchRequest,
    settings: &KubernetesSettings,
    updates: mpsc::Sender<EnrichmentUpdate>,
) -> Vec<AbortOnDrop> {
    match request.kind.known_kind() {
        // Pod field selectors scope the resource watch, not the Metrics API.
        // Fetch namespace-scoped metrics and join only matching visible pod keys.
        Some(ResourceKind::Pods) => vec![AbortOnDrop(tokio::spawn(pod_metrics_loop(
            client,
            request.namespace.clone(),
            settings.refresh_interval.clone(),
            updates,
        )))],
        Some(ResourceKind::Nodes) => vec![
            AbortOnDrop(tokio::spawn(node_metrics_loop(
                client.clone(),
                settings.refresh_interval.clone(),
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
    refresh: watch::Receiver<Duration>,
    updates: mpsc::Sender<EnrichmentUpdate>,
) {
    let mut ticker = RefreshSchedule::new(refresh);
    loop {
        ticker.tick().await;
        if let Ok(usage) = fetch_pod_metrics(client.clone(), namespace.as_deref()).await {
            let _ = updates.try_send(EnrichmentUpdate::PodUsage(usage));
        }
    }
}

async fn node_metrics_loop(
    client: Client,
    refresh: watch::Receiver<Duration>,
    updates: mpsc::Sender<EnrichmentUpdate>,
) {
    let mut ticker = RefreshSchedule::new(refresh);
    loop {
        ticker.tick().await;
        if let Ok(usage) = fetch_node_metrics(client.clone()).await {
            let _ = updates.try_send(EnrichmentUpdate::NodeUsage(usage));
        }
    }
}

pub(super) async fn fetch_pod_metrics(
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

pub(super) async fn fetch_node_metrics(
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
        "User" | "Group" if target.kind.is_subject() => {
            let references = subject_references(client.clone(), kind, &target.name).await?;
            (
                format!("policy({kind}:{})", target.name),
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

async fn subject_references(
    client: Client,
    subject_kind: &str,
    name: &str,
) -> Result<BTreeSet<PolicyReference>, String> {
    let (mut references, cluster_references) = tokio::try_join!(
        collect_policy_references(Api::<RoleBinding>::all(client.clone()), |binding| {
            let matches = binding.subjects.as_deref().is_some_and(|subjects| {
                subjects
                    .iter()
                    .any(|subject| subject_matches(subject, subject_kind, name))
            });
            matches
                .then(|| reference_from_role_binding(binding, false))
                .transpose()
        }),
        collect_policy_references(Api::<ClusterRoleBinding>::all(client), |binding| {
            let matches = binding.subjects.as_deref().is_some_and(|subjects| {
                subjects
                    .iter()
                    .any(|subject| subject_matches(subject, subject_kind, name))
            });
            matches
                .then(|| reference_from_cluster_role_binding(binding, false))
                .transpose()
        })
    )?;
    references.extend(cluster_references);
    Ok(references)
}

fn subject_matches(subject: &Subject, kind: &str, name: &str) -> bool {
    subject.kind == kind && subject.name == name
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
    if subject.kind == "ServiceAccount" {
        subject.name == name
            && subject.namespace.as_deref().or(binding_namespace) == Some(namespace)
    } else if subject.kind == "Group" {
        let group = subject.name.as_str();
        group == "system:authenticated"
            || group == "system:serviceaccounts"
            || group
                .strip_prefix("system:serviceaccounts:")
                .is_some_and(|ns| ns == namespace)
    } else {
        false
    }
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
    let pod = namespaced_target_api::<Pod>(client.clone(), &target)?
        .get(&target.name)
        .await
        .map_err(|error| error.to_string())?;
    let usage = if let Some(ns) = target.namespace.as_deref() {
        fetch_pod_container_metrics(client, ns, &target.name)
            .await
            .ok()
    } else {
        None
    };
    let rows = load_containers_rows(&pod, usage.as_ref())
        .into_iter()
        .map(Arc::new)
        .collect();
    Ok(ActionResult::Containers { target, rows })
}

async fn fetch_pod_container_metrics(
    client: Client,
    namespace: &str,
    pod_name: &str,
) -> Result<FxHashMap<String, ResourceUsage>, kube::Error> {
    let resource = metrics_resource("PodMetrics", "pods");
    let api = Api::<DynamicObject>::namespaced_with(client, namespace, &resource);
    let metric = api.get(pod_name).await?;
    let Some(containers) = metric
        .data
        .get("containers")
        .and_then(serde_json::Value::as_array)
    else {
        return Ok(FxHashMap::default());
    };
    let mut map = FxHashMap::default();
    for container in containers {
        let Some(name) = container.get("name").and_then(serde_json::Value::as_str) else {
            continue;
        };
        let Some(usage) = container.get("usage") else {
            continue;
        };
        let cpu_millis = usage
            .get("cpu")
            .and_then(serde_json::Value::as_str)
            .and_then(parse_cpu_millis);
        let memory_bytes = usage
            .get("memory")
            .and_then(serde_json::Value::as_str)
            .and_then(parse_memory_bytes);
        map.insert(
            name.to_owned(),
            ResourceUsage {
                cpu_millis,
                memory_bytes,
            },
        );
    }
    Ok(map)
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

fn load_containers_rows(
    pod: &Pod,
    usage: Option<&FxHashMap<String, ResourceUsage>>,
) -> Vec<ResourceRow> {
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
    for (idx, container) in spec
        .init_containers
        .as_deref()
        .unwrap_or_default()
        .iter()
        .enumerate()
    {
        rows.push(container_row(
            &namespace,
            container,
            (idx + 1).to_string(),
            "✓",
            "init",
            status.and_then(|status| status.init_container_statuses.as_deref()),
            created_unix,
            usage.and_then(|map| map.get(&container.name)).copied(),
        ));
    }
    for (idx, container) in spec.containers.iter().enumerate() {
        rows.push(container_row(
            &namespace,
            container,
            (idx + 1).to_string(),
            "×",
            "container",
            status.and_then(|status| status.container_statuses.as_deref()),
            created_unix,
            usage.and_then(|map| map.get(&container.name)).copied(),
        ));
    }
    for (idx, container) in spec
        .ephemeral_containers
        .as_deref()
        .unwrap_or_default()
        .iter()
        .enumerate()
    {
        let ephemeral_as_container = k8s_openapi::api::core::v1::Container {
            name: container.name.clone(),
            image: container.image.clone(),
            liveness_probe: container.liveness_probe.clone(),
            readiness_probe: container.readiness_probe.clone(),
            ports: container.ports.clone(),
            resources: container.resources.clone(),
            ..k8s_openapi::api::core::v1::Container::default()
        };
        rows.push(container_row(
            &namespace,
            &ephemeral_as_container,
            (idx + 1).to_string(),
            "×",
            "ephemeral",
            status.and_then(|status| status.ephemeral_container_statuses.as_deref()),
            created_unix,
            usage.and_then(|map| map.get(&container.name)).copied(),
        ));
    }
    rows
}

#[cfg(test)]
fn container_rows(pod: &Pod) -> Vec<ResourceRow> {
    load_containers_rows(pod, None)
}

fn container_forward_ports(
    container: &k8s_openapi::api::core::v1::Container,
) -> Vec<PortForwardPort> {
    container
        .ports
        .iter()
        .flatten()
        .filter_map(|port| {
            let is_tcp = port.protocol.as_deref().unwrap_or("TCP") == "TCP";
            let port_number = u16::try_from(port.container_port).ok()?;
            (is_tcp && port_number > 0).then(|| PortForwardPort {
                container: Some(Arc::from(container.name.as_str())),
                name: Arc::from(port.name.as_deref().unwrap_or("")),
                port: port_number,
            })
        })
        .collect()
}

fn container_ports_display(container: &k8s_openapi::api::core::v1::Container) -> String {
    container
        .ports
        .as_deref()
        .unwrap_or_default()
        .iter()
        .map(|p| {
            let mut s = String::new();
            if let Some(name) = p.name.as_deref().filter(|n| !n.is_empty()) {
                let _ = write!(s, "{name}:");
            }
            let _ = write!(s, "{}", p.container_port);
            if let Some(proto) = p.protocol.as_deref().filter(|pr| *pr != "TCP") {
                let _ = write!(s, "/{proto}");
            }
            s
        })
        .collect::<Vec<_>>()
        .join(",")
}

#[allow(clippy::too_many_arguments)]
fn container_row(
    namespace: &str,
    container: &k8s_openapi::api::core::v1::Container,
    index: String,
    init: &str,
    kind: &str,
    statuses: Option<&[ContainerStatus]>,
    created_unix: i64,
    usage: Option<ResourceUsage>,
) -> ResourceRow {
    let name = &container.name;
    let image = container.image.as_deref().unwrap_or("-");
    let status = statuses.and_then(|statuses| statuses.iter().find(|status| status.name == *name));
    let ready = status.is_some_and(|status| status.ready);
    let restarts = status.map_or(0, |status| status.restart_count.max(0));
    let liveness = if container.liveness_probe.is_some() {
        "on"
    } else {
        "off"
    };
    let readiness = if container.readiness_probe.is_some() {
        "on"
    } else {
        "off"
    };
    let probes = format!("{liveness}:{readiness}");
    let ports_str = container_ports_display(container);
    let (cpu_req, cpu_lim, mem_req, mem_lim) = container_resource_totals(container);
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
        index: Arc::from(index),
        init: Arc::from(init),
        probes: Arc::from(probes),
        ports: Arc::from(ports_str),
        cpu_millis: usage.and_then(|u| u.cpu_millis),
        cpu_request_millis: cpu_req,
        cpu_limit_millis: cpu_lim,
        memory_bytes: usage.and_then(|u| u.memory_bytes),
        memory_request_bytes: mem_req,
        memory_limit_bytes: mem_lim,
    })
    .with_port_forward_ports(container_forward_ports(container))
}

fn container_resource_totals(
    container: &k8s_openapi::api::core::v1::Container,
) -> (u64, u64, u64, u64) {
    let Some(resources) = &container.resources else {
        return (0, 0, 0, 0);
    };
    let cpu_req = resources
        .requests
        .as_ref()
        .and_then(|v| v.get("cpu"))
        .and_then(|q| parse_cpu_millis(&q.0))
        .unwrap_or(0);
    let cpu_lim = resources
        .limits
        .as_ref()
        .and_then(|v| v.get("cpu"))
        .and_then(|q| parse_cpu_millis(&q.0))
        .unwrap_or(0);
    let mem_req = resources
        .requests
        .as_ref()
        .and_then(|v| v.get("memory"))
        .and_then(|q| parse_memory_bytes(&q.0))
        .unwrap_or(0);
    let mem_lim = resources
        .limits
        .as_ref()
        .and_then(|v| v.get("memory"))
        .and_then(|q| parse_memory_bytes(&q.0))
        .unwrap_or(0);
    (cpu_req, cpu_lim, mem_req, mem_lim)
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
    let target = target.clone();
    tokio::task::spawn_blocking(move || yaml_content_from_object(object, &target))
        .await
        .map_err(|error| format!("Unable to format YAML: {error}"))?
}

fn yaml_content_from_object(
    object: DynamicObject,
    target: &ResourceTarget,
) -> Result<YamlContent, String> {
    yaml_content_with_managed_fields(object, target, false)
}

fn yaml_content_with_managed_fields(
    mut object: DynamicObject,
    target: &ResourceTarget,
    include_managed_fields: bool,
) -> Result<YamlContent, String> {
    let mut decoded = is_core_v1_secret(target)
        .then(|| decode_secret_object(&object))
        .flatten();
    let has_managed_fields = object
        .meta()
        .managed_fields
        .as_ref()
        .is_some_and(|fields| !fields.is_empty());
    let managed_body = |object: &DynamicObject| {
        if include_managed_fields {
            serde_yaml::to_string(object).map_err(|error| error.to_string())
        } else {
            Ok(String::new())
        }
    };
    let with_managed_fields = has_managed_fields
        .then(|| managed_body(&object))
        .transpose()?;
    let decoded_with_managed_fields = decoded
        .as_ref()
        .filter(|_| has_managed_fields)
        .map(managed_body)
        .transpose()?;
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

#[derive(Default)]
struct TypedWatchState {
    table_columns: Arc<[TableColumn]>,
    rows: FxHashMap<Arc<str>, Arc<ResourceRow>>,
    initializing: Option<FxHashMap<Arc<str>, Arc<ResourceRow>>>,
    initial_load_complete: bool,
    initial_progress: InitialListProgress,
    revision: u64,
    dirty: bool,
    fast_initial_published: bool,
    fast_initial_rows: Option<FxHashMap<Arc<str>, Arc<ResourceRow>>>,
    fast_initial_quality: u8,
}

impl TypedWatchState {
    fn new(table_columns: Arc<[TableColumn]>) -> Self {
        Self {
            table_columns,
            ..Self::default()
        }
    }

    fn apply_fast_initial(
        &mut self,
        mut update: FastInitialList,
        snapshots: &watch::Sender<ResourceSnapshot>,
        request: &WatchRequest,
    ) {
        if self.initial_load_complete {
            return;
        }
        if self.fast_initial_published && update.quality < self.fast_initial_quality {
            return;
        }
        if !update.hydrated {
            hydrate_fast_initial_rows(&request.kind, &update.columns, &mut update.rows);
        }
        if let Some(initializing) = &self.initializing {
            overlay_typed_rows(&mut update.rows, initializing);
        }
        self.fast_initial_published = true;
        self.fast_initial_quality = update.quality;
        self.revision = self.revision.wrapping_add(1);
        let state = SourceState::Loading {
            loaded: update.rows.len(),
        };
        publish_table(
            snapshots,
            request,
            &update.rows,
            Arc::clone(&self.table_columns),
            state,
            self.revision,
        );
        self.fast_initial_rows = Some(update.rows);
    }

    fn apply_event<K, F>(
        &mut self,
        event: watcher::Event<K>,
        map_row: F,
        enrichment: &EnrichmentState,
        snapshots: &watch::Sender<ResourceSnapshot>,
        request: &WatchRequest,
    ) where
        F: Fn(&K) -> ResourceRow,
    {
        match event {
            watcher::Event::Init => {
                self.initializing = Some(FxHashMap::default());
                self.initial_progress = InitialListProgress::default();
            }
            watcher::Event::InitApply(object) => {
                let mut row = map_row(&object);
                enrichment.apply(&mut row);
                apply_initial_row(
                    &mut self.initializing,
                    Arc::new(row),
                    self.initial_load_complete || self.fast_initial_published,
                    &mut self.initial_progress,
                    snapshots,
                    request,
                    &self.table_columns,
                    &mut self.revision,
                );
                self.dirty = true;
            }
            watcher::Event::InitDone => {
                if let Some(initial) = self.initializing.take() {
                    self.rows = initial;
                }
                self.initial_load_complete = true;
                self.fast_initial_rows = None;
                self.revision = self.revision.wrapping_add(1);
                publish_table(
                    snapshots,
                    request,
                    &self.rows,
                    Arc::clone(&self.table_columns),
                    SourceState::Ready,
                    self.revision,
                );
                self.dirty = false;
            }
            watcher::Event::Apply(object) => {
                let mut row = map_row(&object);
                enrichment.apply(&mut row);
                let row = Arc::new(row);
                self.rows.insert(Arc::clone(&row.key), row);
                self.dirty = true;
            }
            watcher::Event::Delete(object) => {
                let row = map_row(&object);
                self.rows.remove(&row.key);
                self.dirty = true;
            }
        }
    }

    fn apply_enrichment(&mut self, enrichment: &EnrichmentState) {
        enrichment.apply_all(&mut self.rows);
        if let Some(initializing) = &mut self.initializing {
            enrichment.apply_all(initializing);
        }
        self.dirty = true;
    }

    fn publish_ready(
        &mut self,
        snapshots: &watch::Sender<ResourceSnapshot>,
        request: &WatchRequest,
    ) {
        self.revision = self.revision.wrapping_add(1);
        if let Some(initializing) = &self.initializing {
            let mut visible = self.fast_initial_rows.clone().unwrap_or_default();
            overlay_typed_rows(&mut visible, initializing);
            publish_table(
                snapshots,
                request,
                &visible,
                Arc::clone(&self.table_columns),
                SourceState::Loading {
                    loaded: initializing.len(),
                },
                self.revision,
            );
        } else {
            publish_table(
                snapshots,
                request,
                &self.rows,
                Arc::clone(&self.table_columns),
                SourceState::Ready,
                self.revision,
            );
        }
        self.dirty = false;
    }

    fn publish_error(
        &mut self,
        snapshots: &watch::Sender<ResourceSnapshot>,
        request: &WatchRequest,
        error: Arc<str>,
    ) {
        self.revision = self.revision.wrapping_add(1);
        let visible = if self.initial_load_complete {
            &self.rows
        } else {
            self.fast_initial_rows
                .as_ref()
                .or(self.initializing.as_ref())
                .unwrap_or(&self.rows)
        };
        publish_table(
            snapshots,
            request,
            visible,
            Arc::clone(&self.table_columns),
            SourceState::Error(error),
            self.revision,
        );
    }
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
    let (mut fast_initial_updates, _fast_initial_task) =
        spawn_fast_initial_list(client.clone(), &request);
    let mut state = TypedWatchState::new(typed_table_columns(&request.kind));
    let mut fast_initial_finished = false;
    if let Ok(update) =
        tokio::time::timeout(FAST_INITIAL_LIST_HEAD_START, fast_initial_updates.recv()).await
    {
        match update {
            Some(update) => state.apply_fast_initial(update, &snapshots, &request),
            None => fast_initial_finished = true,
        }
    }

    let (enrichment_sender, mut enrichment_updates) = mpsc::channel(8);
    let _enrichment_tasks = spawn_enrichment_tasks(
        client.clone(),
        &request,
        &settings,
        enrichment_sender.clone(),
    );
    let _enrichment_sender = enrichment_sender;
    let mut enrichment = EnrichmentState::default();
    let watcher_config = resource_watcher_config(&settings, &request);
    let stream = watcher::watcher(api, watcher_config).default_backoff();
    tokio::pin!(stream);

    let mut ticker = tokio::time::interval(settings.publish_interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            event = stream.next() => {
                match event {
                    Some(Ok(event)) => state.apply_event(
                        event,
                        map_row,
                        &enrichment,
                        &snapshots,
                        &request,
                    ),
                    Some(Err(error)) => state.publish_error(
                        &snapshots,
                        &request,
                        Arc::from(error.to_string()),
                    ),
                    None => {
                        state.publish_error(
                            &snapshots,
                            &request,
                            Arc::from("Kubernetes watch ended"),
                        );
                        return;
                    }
                }
            }
            update = fast_initial_updates.recv(), if !fast_initial_finished => {
                match update {
                    Some(update) => state.apply_fast_initial(update, &snapshots, &request),
                    None => fast_initial_finished = true,
                }
            }
            Some(update) = enrichment_updates.recv() => {
                enrichment.update(update);
                state.apply_enrichment(&enrichment);
            }
            _ = ticker.tick(), if state.dirty => {
                state.publish_ready(&snapshots, &request);
            }
        }
    }
}

fn overlay_typed_rows(
    visible: &mut FxHashMap<Arc<str>, Arc<ResourceRow>>,
    typed: &FxHashMap<Arc<str>, Arc<ResourceRow>>,
) {
    for row in typed.values() {
        visible.remove(format!("-/{}", row.name).as_str());
        visible.insert(Arc::clone(&row.key), Arc::clone(row));
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
    if let Some(selector) = server_field_selector(request) {
        config = config.fields(selector);
    }
    if let Some(selector) = &request.label_selector {
        config = config.labels(selector);
    }
    config
}

#[allow(clippy::too_many_arguments)]
fn apply_initial_row(
    initializing: &mut Option<FxHashMap<Arc<str>, Arc<ResourceRow>>>,
    row: Arc<ResourceRow>,
    suppress_progress: bool,
    progress: &mut InitialListProgress,
    snapshots: &watch::Sender<ResourceSnapshot>,
    request: &WatchRequest,
    table_columns: &Arc<[TableColumn]>,
    revision: &mut u64,
) {
    let initial = initializing.get_or_insert_with(FxHashMap::default);
    initial.insert(Arc::clone(&row.key), row);
    if suppress_progress || !progress.should_publish(initial.len()) {
        return;
    }
    *revision = revision.wrapping_add(1);
    publish_table(
        snapshots,
        request,
        initial,
        Arc::clone(table_columns),
        SourceState::Loading {
            loaded: initial.len(),
        },
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
const TABLE_FAST_INCLUDE_OBJECT: &str = "None";

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
            server_field_selector(&request),
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
    if let Some(selector) = server_field_selector(request) {
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
    let mut ticker = RefreshSchedule::new(settings.refresh_interval);
    loop {
        ticker.tick().await;
        let mut initial_progress = InitialListProgress::default();
        let mut partial_columns: Arc<[TableColumn]> = Arc::from([]);
        let mut partial_rows = FxHashMap::default();
        match list_table_pages_with_progress(
            &api,
            &client,
            server_field_selector(&request),
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
    on_page: F,
) -> Result<TableListing, String>
where
    F: FnMut(&Arc<[TableColumn]>, &FxHashMap<Arc<str>, Arc<ResourceRow>>, bool),
{
    list_table_pages_with_include(
        api,
        client,
        field_selector,
        label_selector,
        TABLE_INCLUDE_OBJECT,
        on_page,
    )
    .await
}

async fn list_table_pages_with_include<F>(
    api: &Api<DynamicObject>,
    client: &Client,
    field_selector: Option<&str>,
    label_selector: Option<&str>,
    include_object: &str,
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
        let raw = table_request_with_include(raw, include_object)?;
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

fn table_request(request: HttpRequest<Vec<u8>>) -> Result<HttpRequest<Vec<u8>>, String> {
    table_request_with_include(request, TABLE_INCLUDE_OBJECT)
}

fn table_request_with_include(
    mut request: HttpRequest<Vec<u8>>,
    include_object: &str,
) -> Result<HttpRequest<Vec<u8>>, String> {
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
    let uri = format!("{}{separator}includeObject={include_object}", request.uri())
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
    let raw_value = row.object.clone();
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
    let status = column_text("status")
        .or_else(|| column_text("available"))
        .or_else(|| column_text("type"))
        .or_else(|| column_text("phase"))
        .unwrap_or_else(|| "-".to_owned());
    let ready = column_text("ready")
        .or_else(|| column_text("service"))
        .or_else(|| column_text("completions"))
        .or_else(|| column_text("reason"))
        .unwrap_or_else(|| "-".to_owned());
    let details = column_text("details")
        .or_else(|| column_text("message"))
        .or_else(|| column_text("claim"))
        .or_else(|| column_text("volume"))
        .or_else(|| column_text("data"))
        .unwrap_or_else(|| "-".to_owned());
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
    let mut row = match owner {
        Some(owner) => row.with_owner_reference_data(owner),
        None => row,
    };
    if let Some(object) = object.as_ref() {
        row.owner_names = owner_reference_names(object.meta());
    }
    if let Some(raw_value) = raw_value {
        row = row.with_raw_object(raw_value);
    }
    row
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

#[cfg(test)]
fn publish(
    snapshots: &watch::Sender<ResourceSnapshot>,
    request: &WatchRequest,
    rows: &FxHashMap<Arc<str>, Arc<ResourceRow>>,
    state: SourceState,
    revision: u64,
) {
    publish_table(snapshots, request, rows, Arc::from([]), state, revision);
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
    let mut compact: Vec<_> = rows
        .values()
        .filter(|row| row_matches_owner_filter(row, request))
        .cloned()
        .collect();
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

pub(super) fn row_matches_owner_filter(row: &ResourceRow, request: &WatchRequest) -> bool {
    request.field_selector.as_deref() != Some(MATCH_NONE_FIELD_SELECTOR)
        && owner_name_filter(request)
            .is_none_or(|name| row.owner_names.iter().any(|owner| owner.as_ref() == name))
        && owner_uid_filter(request).is_none_or(|uid| {
            row.owner
                .as_ref()
                .is_some_and(|owner| owner.uid.as_ref() == uid)
        })
}

fn make_table_col(name: &str, priority: i32) -> TableColumn {
    TableColumn {
        name: Arc::from(name),
        format: if name.eq_ignore_ascii_case("age") {
            Arc::from("date")
        } else {
            Arc::from("")
        },
        priority,
    }
}

pub(crate) fn typed_table_columns(kind: &ResourceDescriptor) -> Arc<[TableColumn]> {
    match kind.known_kind() {
        Some(ResourceKind::Deployments) => Arc::from([
            make_table_col("NAME", 0),
            make_table_col("READY", 0),
            make_table_col("UP-TO-DATE", 0),
            make_table_col("AVAILABLE", 0),
            make_table_col("CONTAINERS", 1),
            make_table_col("IMAGES", 1),
            make_table_col("SELECTOR", 1),
            make_table_col("LABELS", 1),
            make_table_col("VALID", 1),
            make_table_col("AGE", 0),
        ]),
        Some(ResourceKind::StatefulSets) => Arc::from([
            make_table_col("NAME", 0),
            make_table_col("READY", 0),
            make_table_col("SELECTOR", 1),
            make_table_col("CONTAINERS", 1),
            make_table_col("IMAGES", 1),
            make_table_col("LABELS", 1),
            make_table_col("VALID", 1),
            make_table_col("AGE", 0),
        ]),
        Some(ResourceKind::DaemonSets) => Arc::from([
            make_table_col("NAME", 0),
            make_table_col("DESIRED", 0),
            make_table_col("CURRENT", 0),
            make_table_col("READY", 0),
            make_table_col("UP-TO-DATE", 0),
            make_table_col("AVAILABLE", 0),
            make_table_col("NODE SELECTOR", 0),
            make_table_col("CONTAINERS", 1),
            make_table_col("IMAGES", 1),
            make_table_col("SELECTOR", 1),
            make_table_col("LABELS", 1),
            make_table_col("VALID", 1),
            make_table_col("AGE", 0),
        ]),
        Some(ResourceKind::Services) => Arc::from([
            make_table_col("NAME", 0),
            make_table_col("TYPE", 0),
            make_table_col("CLUSTER-IP", 0),
            make_table_col("EXTERNAL-IP", 0),
            make_table_col("SELECTOR", 1),
            make_table_col("PORTS", 0),
            make_table_col("LABELS", 1),
            make_table_col("VALID", 1),
            make_table_col("AGE", 0),
        ]),
        None if kind.is_apps_v1_replica_set() => Arc::from([
            make_table_col("NAME", 0),
            make_table_col("DESIRED", 0),
            make_table_col("CURRENT", 0),
            make_table_col("READY", 0),
            make_table_col("CONTAINERS", 1),
            make_table_col("IMAGES", 1),
            make_table_col("SELECTOR", 1),
            make_table_col("LABELS", 1),
            make_table_col("VALID", 1),
            make_table_col("AGE", 0),
        ]),
        _ => Arc::from([]),
    }
}

fn plain_cell(text: impl Into<String>) -> TableCell {
    TableCell {
        text: Arc::from(text.into()),
        timestamp_unix: None,
    }
}

fn age_cell(created_unix: i64) -> TableCell {
    let age_secs =
        u64::try_from(crate::model::unix_now().saturating_sub(created_unix).max(0)).unwrap_or(0);
    TableCell {
        text: Arc::from(crate::model::format_age(age_secs)),
        timestamp_unix: Some(created_unix),
    }
}

fn pod_spec_containers_and_images(
    spec: Option<&k8s_openapi::api::core::v1::PodSpec>,
) -> (String, String) {
    let mut names = Vec::new();
    let mut images = Vec::new();
    if let Some(spec) = spec {
        for c in spec.init_containers.as_deref().unwrap_or_default() {
            names.push(c.name.as_str());
            images.push(c.image.as_deref().unwrap_or(""));
        }
        for c in &spec.containers {
            names.push(c.name.as_str());
            images.push(c.image.as_deref().unwrap_or(""));
        }
    }
    (names.join(","), images.join(","))
}

fn pod_readiness_gates(pod: &Pod) -> String {
    let Some(gates) = pod
        .spec
        .as_ref()
        .and_then(|s| s.readiness_gates.as_ref())
        .filter(|g| !g.is_empty())
    else {
        return "<none>".to_owned();
    };
    let conditions = pod
        .status
        .as_ref()
        .and_then(|s| s.conditions.as_deref())
        .unwrap_or_default();
    let mut true_gates = 0_usize;
    for gate in gates {
        if conditions
            .iter()
            .any(|c| c.type_ == gate.condition_type && c.status == "True")
        {
            true_gates += 1;
        }
    }
    format!("{true_gates}/{}", gates.len())
}

fn pod_row(pod: &Pod) -> ResourceRow {
    let namespace = pod.namespace().unwrap_or_default();
    let name = pod.name_any();
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
    let phase = super::pod_actions::pod_status(pod);
    let (cpu_request_millis, cpu_limit_millis, memory_request_bytes, memory_limit_bytes) =
        pod_resource_totals(pod);
    let node = pod
        .spec
        .as_ref()
        .and_then(|spec| spec.node_name.clone())
        .unwrap_or_else(|| "-".to_owned());
    let qos = status
        .and_then(|s| s.qos_class.clone())
        .unwrap_or_else(|| "-".to_owned());
    let ip = status
        .and_then(|s| s.pod_ip.clone())
        .unwrap_or_else(|| "<none>".to_owned());
    let nominated_node = status
        .and_then(|s| s.nominated_node_name.clone())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "<none>".to_owned());
    let readiness_gates = pod_readiness_gates(pod);
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
        qos: Arc::from(qos),
        ip: Arc::from(ip),
        nominated_node: Arc::from(nominated_node),
        readiness_gates: Arc::from(readiness_gates),
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
    let up_to_date = deployment
        .status
        .as_ref()
        .and_then(|status| status.updated_replicas)
        .unwrap_or(0);
    let available = deployment
        .status
        .as_ref()
        .and_then(|status| status.available_replicas)
        .unwrap_or(0);
    let selector = deployment
        .spec
        .as_ref()
        .map(|spec| label_selector_string(&spec.selector))
        .unwrap_or_default();
    let (containers, images) = pod_spec_containers_and_images(
        deployment
            .spec
            .as_ref()
            .and_then(|s| s.template.spec.as_ref()),
    );
    let mut row = resource_row(
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
    let valid = if row.status.as_ref() == "Ready" {
        String::new()
    } else {
        row.status.to_string()
    };
    let created_unix = row.created_unix;
    let row_name = row.name.to_string();
    let row_ready = row.ready.to_string();
    let labels = row.labels.to_string();
    row = row.with_table_cells(vec![
        plain_cell(row_name),
        plain_cell(row_ready),
        plain_cell(up_to_date.to_string()),
        plain_cell(available.to_string()),
        plain_cell(containers),
        plain_cell(images),
        plain_cell(selector.clone()),
        plain_cell(labels),
        plain_cell(valid),
        age_cell(created_unix),
    ]);
    with_child_resource(row, ResourceKind::Pods.into(), selector)
}

fn replica_set_row(replica_set: &ReplicaSet) -> ResourceRow {
    let desired = replica_set
        .spec
        .as_ref()
        .and_then(|spec| spec.replicas)
        .unwrap_or(1);
    let current = replica_set
        .status
        .as_ref()
        .map_or(0, |status| status.replicas);
    let ready = replica_set
        .status
        .as_ref()
        .and_then(|status| status.ready_replicas)
        .unwrap_or(0);
    let selector = replica_set
        .spec
        .as_ref()
        .map(|spec| label_selector_string(&spec.selector))
        .unwrap_or_default();
    let (containers, images) = pod_spec_containers_and_images(
        replica_set
            .spec
            .as_ref()
            .and_then(|s| s.template.as_ref())
            .and_then(|t| t.spec.as_ref()),
    );
    let mut row = resource_row(
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
    let valid = if row.status.as_ref() == "Ready" {
        String::new()
    } else {
        row.status.to_string()
    };
    let created_unix = row.created_unix;
    let row_name = row.name.to_string();
    let labels = row.labels.to_string();
    row = row.with_table_cells(vec![
        plain_cell(row_name),
        plain_cell(desired.to_string()),
        plain_cell(current.to_string()),
        plain_cell(ready.to_string()),
        plain_cell(containers),
        plain_cell(images),
        plain_cell(selector.clone()),
        plain_cell(labels),
        plain_cell(valid),
        age_cell(created_unix),
    ]);
    with_child_resource(row, ResourceKind::Pods.into(), selector)
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
    let selector = stateful_set
        .spec
        .as_ref()
        .map(|spec| label_selector_string(&spec.selector))
        .unwrap_or_default();
    let (containers, images) = pod_spec_containers_and_images(
        stateful_set
            .spec
            .as_ref()
            .and_then(|s| s.template.spec.as_ref()),
    );
    let mut row = resource_row(
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
    let valid = if row.status.as_ref() == "Ready" {
        String::new()
    } else {
        row.status.to_string()
    };
    let created_unix = row.created_unix;
    let row_name = row.name.to_string();
    let row_ready = row.ready.to_string();
    let labels = row.labels.to_string();
    row = row.with_table_cells(vec![
        plain_cell(row_name),
        plain_cell(row_ready),
        plain_cell(selector.clone()),
        plain_cell(containers),
        plain_cell(images),
        plain_cell(labels),
        plain_cell(valid),
        age_cell(created_unix),
    ]);
    with_child_resource(row, ResourceKind::Pods.into(), selector)
}

fn daemon_set_row(daemon_set: &DaemonSet) -> ResourceRow {
    let desired = daemon_set
        .status
        .as_ref()
        .map_or(0, |status| status.desired_number_scheduled);
    let current = daemon_set
        .status
        .as_ref()
        .map_or(0, |status| status.current_number_scheduled);
    let ready = daemon_set
        .status
        .as_ref()
        .map_or(0, |status| status.number_ready);
    let up_to_date = daemon_set
        .status
        .as_ref()
        .and_then(|status| status.updated_number_scheduled)
        .unwrap_or(0);
    let available = daemon_set
        .status
        .as_ref()
        .and_then(|status| status.number_available)
        .unwrap_or(0);
    let selector = daemon_set
        .spec
        .as_ref()
        .map(|spec| label_selector_string(&spec.selector))
        .unwrap_or_default();
    let pod_spec = daemon_set
        .spec
        .as_ref()
        .and_then(|s| s.template.spec.as_ref());
    let node_selector = pod_spec
        .and_then(|s| s.node_selector.as_ref())
        .map(label_map_selector)
        .unwrap_or_default();
    let (containers, images) = pod_spec_containers_and_images(pod_spec);
    let mut row = resource_row(
        daemon_set,
        daemon_set.namespace().unwrap_or_default(),
        daemon_set.name_any(),
        workload_status(
            daemon_set.meta().deletion_timestamp.is_some(),
            ready,
            desired,
        ),
        format!("{ready}/{desired}"),
        format!("{available} available"),
    );
    let valid = if row.status.as_ref() == "Ready" {
        String::new()
    } else {
        row.status.to_string()
    };
    let created_unix = row.created_unix;
    let row_name = row.name.to_string();
    let labels = row.labels.to_string();
    row = row.with_table_cells(vec![
        plain_cell(row_name),
        plain_cell(desired.to_string()),
        plain_cell(current.to_string()),
        plain_cell(ready.to_string()),
        plain_cell(up_to_date.to_string()),
        plain_cell(available.to_string()),
        plain_cell(node_selector),
        plain_cell(containers),
        plain_cell(images),
        plain_cell(selector.clone()),
        plain_cell(labels),
        plain_cell(valid),
        age_cell(created_unix),
    ]);
    with_child_resource(row, ResourceKind::Pods.into(), selector)
}

fn service_external_ip(service: &Service) -> String {
    let Some(spec) = service.spec.as_ref() else {
        return String::new();
    };
    let svc_type = spec.type_.as_deref().unwrap_or("ClusterIP");
    let ext_ips = spec
        .external_ips
        .as_ref()
        .filter(|ips| !ips.is_empty())
        .map(|ips| ips.join(","));
    match svc_type {
        "ClusterIP" | "NodePort" => ext_ips.unwrap_or_default(),
        "LoadBalancer" => {
            let lb_ips = service
                .status
                .as_ref()
                .and_then(|s| s.load_balancer.as_ref())
                .and_then(|lb| lb.ingress.as_ref())
                .map(|ingresses| {
                    ingresses
                        .iter()
                        .filter_map(|ing| ing.ip.as_deref().or(ing.hostname.as_deref()))
                        .collect::<Vec<_>>()
                        .join(",")
                })
                .filter(|s| !s.is_empty());
            match (lb_ips, ext_ips) {
                (Some(lb), Some(ext)) => format!("{lb},{ext}"),
                (Some(lb), None) => lb,
                (None, Some(ext)) => ext,
                (None, None) => "<pending>".to_owned(),
            }
        }
        "ExternalName" => spec.external_name.clone().unwrap_or_default(),
        _ => "<unknown>".to_owned(),
    }
}

fn service_ports_display(service: &Service) -> String {
    service
        .spec
        .as_ref()
        .and_then(|s| s.ports.as_ref())
        .map(|ports| {
            ports
                .iter()
                .map(|p| {
                    let mut s = String::new();
                    if let Some(name) = p.name.as_deref().filter(|n| !n.is_empty()) {
                        let _ = write!(s, "{name}:");
                    }
                    let _ = write!(s, "{}", p.port);
                    if let Some(np) = p.node_port.filter(|np| *np > 0) {
                        let _ = write!(s, "►{np}");
                    }
                    if let Some(proto) = p.protocol.as_deref().filter(|pr| *pr != "TCP") {
                        let _ = write!(s, "/{proto}");
                    }
                    s
                })
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default()
}

fn service_row(service: &Service) -> ResourceRow {
    let spec = service.spec.as_ref();
    let service_type = spec
        .and_then(|spec| spec.type_.clone())
        .unwrap_or_else(|| "ClusterIP".to_owned());
    let cluster_ip = spec
        .and_then(|spec| spec.cluster_ip.clone())
        .filter(|ip| ip != "None")
        .unwrap_or_default();
    let external_ip = service_external_ip(service);
    let selector = spec
        .and_then(|spec| spec.selector.as_ref())
        .map(label_map_selector)
        .unwrap_or_default();
    let ports_display = service_ports_display(service);
    let ports_count = spec
        .and_then(|spec| spec.ports.as_ref())
        .map_or(0, Vec::len);
    let raw_value = serde_json::to_value(service).ok();
    let mut row = resource_row(
        service,
        service.namespace().unwrap_or_default(),
        service.name_any(),
        service_type.clone(),
        format!("{ports_count} ports"),
        if cluster_ip.is_empty() {
            "-".to_owned()
        } else {
            cluster_ip.clone()
        },
    )
    .with_port_forward_ports(service_forward_ports(service));
    if let Some(raw_value) = raw_value {
        row = row.with_raw_object(raw_value);
    }
    let created_unix = row.created_unix;
    let row_name = row.name.to_string();
    let labels = row.labels.to_string();
    row = row.with_table_cells(vec![
        plain_cell(row_name),
        plain_cell(service_type),
        plain_cell(cluster_ip),
        plain_cell(external_ip),
        plain_cell(selector.clone()),
        plain_cell(ports_display),
        plain_cell(labels),
        plain_cell(String::new()),
        age_cell(created_unix),
    ]);
    with_child_resource(row, ResourceKind::Pods.into(), selector)
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

fn node_roles(node: &Node) -> String {
    let Some(labels) = node.metadata.labels.as_ref() else {
        return "<none>".to_owned();
    };
    let mut roles = Vec::new();
    for (k, v) in labels {
        if let Some(role) = k.strip_prefix("node-role.kubernetes.io/")
            && !role.is_empty()
            && !roles.contains(&role)
        {
            roles.push(role);
        } else if (k == "kubernetes.io/role" || k == "kops.k8s.io/instancegroup")
            && !v.is_empty()
            && !roles.contains(&v.as_str())
        {
            roles.push(v.as_str());
        }
    }
    if roles.is_empty() {
        "<none>".to_owned()
    } else {
        roles.join(",")
    }
}

fn node_ips(node: &Node) -> (String, String) {
    let Some(addrs) = node.status.as_ref().and_then(|s| s.addresses.as_ref()) else {
        return ("<none>".to_owned(), "<none>".to_owned());
    };
    let mut i_ip = "<none>".to_owned();
    let mut e_ip = "<none>".to_owned();
    for a in addrs {
        match a.type_.as_str() {
            "InternalIP" => i_ip.clone_from(&a.address),
            "ExternalIP" => e_ip.clone_from(&a.address),
            _ => {}
        }
    }
    (i_ip, e_ip)
}

#[allow(clippy::too_many_lines)]
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
    let role = node_roles(node);
    let node_info = node
        .status
        .as_ref()
        .and_then(|status| status.node_info.as_ref());
    let version = node_info.map_or_else(|| "-".to_owned(), |info| info.kubelet_version.clone());
    let kernel = node_info.map_or_else(|| "-".to_owned(), |info| info.kernel_version.clone());
    let os_image = node_info.map_or_else(|| "-".to_owned(), |info| info.os_image.clone());
    let arch = node_info.map_or_else(
        || "-".to_owned(),
        |info| format!("{}/{}", info.operating_system, info.architecture),
    );
    let runtime = node_info.map_or_else(
        || "-".to_owned(),
        |info| info.container_runtime_version.clone(),
    );
    let (internal_ip, external_ip) = node_ips(node);
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
        role,
        version.clone(),
    )
    .with_node_data(NodeData {
        unschedulable,
        taints: Arc::from(taints),
        taint_count: u32::try_from(taint_count).unwrap_or(u32::MAX),
        version: Arc::from(version),
        kernel: Arc::from(kernel),
        os_image: Arc::from(os_image),
        arch: Arc::from(arch),
        runtime: Arc::from(runtime),
        internal_ip: Arc::from(internal_ip),
        external_ip: Arc::from(external_ip),
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

fn resource_row<K: ResourceExt + serde::Serialize>(
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
    let mut row = match owner_reference_data(resource.meta()) {
        Some(owner) => row.with_owner_reference_data(owner),
        None => row,
    };
    row.owner_names = owner_reference_names(resource.meta());
    if let Ok(raw_value) = serde_json::to_value(resource) {
        row = row.with_raw_object(raw_value);
    }
    row
}

fn owner_reference_names(
    metadata: &k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta,
) -> Arc<[Arc<str>]> {
    metadata
        .owner_references
        .as_ref()
        .map(|owners| {
            owners
                .iter()
                .map(|owner| Arc::from(owner.name.as_str()))
                .collect()
        })
        .unwrap_or_default()
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
        uid: Arc::from(owner.uid.as_str()),
    })
}

#[cfg(test)]
mod tests {
    use kube::Config;

    async fn client_for_context(context: &str) -> Result<Client, String> {
        client_for_context_with(&ConnectionOptions::default(), context).await
    }
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

    async fn scale_client_fixture() -> (Client, tokio::task::JoinHandle<String>) {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let config = Config::new(format!("http://{address}").parse().unwrap());
        let client = Client::try_from(config).unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            let mut chunk = [0; 4096];
            loop {
                let size = socket.read(&mut chunk).await.unwrap();
                assert_ne!(size, 0);
                bytes.extend_from_slice(&chunk[..size]);
                if let Some(end) = bytes.windows(4).position(|value| value == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&bytes[..end]);
                    let length = headers
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().unwrap())
                        })
                        .unwrap_or(0);
                    if bytes.len() >= end + 4 + length {
                        break;
                    }
                }
                assert!(bytes.len() < 65_536);
            }
            let body = r#"{"apiVersion":"autoscaling/v1","kind":"Scale","metadata":{"name":"worker","namespace":"team"},"spec":{"replicas":0}}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            String::from_utf8(bytes).unwrap()
        });
        (client, server)
    }

    fn assert_scoped_rejection(result: ActionResult, expected_generation: u64) {
        let ActionResult::Scoped { generation, result } = result else {
            panic!("rejection must retain the submitting generation");
        };
        assert_eq!(generation, expected_generation);
        assert!(
            matches!(*result, ActionResult::Error(message) if message.contains("active view changed"))
        );
    }

    fn references_target() -> ResourceTarget {
        ResourceTarget {
            kind: ResourceDescriptor::new(
                "",
                "v1",
                "v1",
                "ConfigMap",
                "configmaps",
                true,
                crate::model::ResourceOperations::all(),
            ),
            namespace: Some("team".to_owned()),
            name: "settings".to_owned(),
        }
    }

    async fn references_partial_client_fixture(
        token: &str,
    ) -> (Client, tokio::task::JoinHandle<()>) {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = Client::try_from(Config::new(
            format!("http://{}", listener.local_addr().unwrap())
                .parse()
                .unwrap(),
        ))
        .unwrap();
        let token = token.to_owned();
        let server = tokio::spawn(async move {
            for _ in 0..5 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                let mut chunk = [0; 4096];
                while !request.windows(4).any(|value| value == b"\r\n\r\n") {
                    let size = socket.read(&mut chunk).await.unwrap();
                    assert_ne!(size, 0);
                    request.extend_from_slice(&chunk[..size]);
                    assert!(request.len() < 65_536);
                }
                let request = String::from_utf8(request).unwrap();
                let (status, body) = if request.contains("/daemonsets") {
                    (
                        "403 Forbidden",
                        serde_json::json!({
                            "apiVersion": "v1", "kind": "Status", "status": "Failure",
                            "reason": "Forbidden", "code": 403,
                            "message": format!("do not expose {token} in this scan error"),
                        }),
                    )
                } else {
                    let items = if request.contains("/deployments") {
                        vec![serde_json::json!({
                            "apiVersion": "apps/v1", "kind": "Deployment",
                            "metadata": {"name": "worker", "namespace": "team"},
                            "spec": {"selector": {"matchLabels": {"app": "worker"}},
                                "template": {"spec": {"containers": [{"name": "main",
                                    "image": "example", "envFrom": [{"configMapRef": {"name": "settings"}}]}]}}},
                        })]
                    } else {
                        Vec::new()
                    };
                    (
                        "200 OK",
                        serde_json::json!({
                            "apiVersion": "v1", "kind": "List", "metadata": {}, "items": items,
                        }),
                    )
                };
                let body = body.to_string();
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len(),
                );
                socket.write_all(response.as_bytes()).await.unwrap();
            }
        });
        (client, server)
    }

    #[tokio::test]
    async fn references_source_preserves_partial_results_and_redacts_scan_errors() {
        for token in [
            "fixture-token".to_owned(),
            format!("fixture-token-{}", "x".repeat(2_048)),
        ] {
            let (client, server) = references_partial_client_fixture(&token).await;
            let target = references_target();
            let action = ActionIntent::LoadReferences {
                id: 56,
                target: target.clone(),
            };
            let result = tokio::time::timeout(
                Duration::from_secs(3),
                handle_action(
                    client,
                    "active-context",
                    &ConnectionOptions {
                        token: Some(token),
                        ..ConnectionOptions::default()
                    },
                    action,
                ),
            )
            .await
            .unwrap();
            let ActionResult::References {
                id,
                target: actual,
                result: Ok(report),
            } = result
            else {
                panic!("a partial reference scan must retain its typed report");
            };
            assert_eq!(id, 56);
            assert_eq!(actual, target);
            assert_eq!(report.entries.len(), 1);
            assert_eq!(
                report.entries[0].target.kind,
                ResourceKind::Deployments.into()
            );
            assert_eq!(report.entries[0].target.namespace.as_deref(), Some("team"));
            assert_eq!(report.entries[0].target.name, "worker");
            assert_eq!(report.scanned, 1);
            assert_eq!(report.errors.len(), 1);
            assert!(report.errors[0].contains("[REDACTED]"));
            assert!(!report.errors[0].contains("fixture-token"));
            assert!(report.errors[0].chars().count() <= 1_024);
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn references_invalid_connection_timeout_is_scoped_and_never_reaches_api() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = Client::try_from(Config::new(
            format!("http://{}", listener.local_addr().unwrap())
                .parse()
                .unwrap(),
        ))
        .unwrap();
        let target = references_target();
        let action = ActionIntent::LoadReferences {
            id: 57,
            target: target.clone(),
        };
        assert!(action.cancelable_read());
        let (results, mut received) = mpsc::channel(1);
        spawn_action_task(
            client,
            "active-context".to_owned(),
            ConnectionOptions {
                request_timeout: Some("invalid".to_owned()),
                ..ConnectionOptions::default()
            },
            ActionRequest {
                generation: 9,
                intent: action,
            },
            results,
            true,
        )
        .await
        .unwrap();
        let ActionResult::Scoped { generation, result } = received.try_recv().unwrap() else {
            panic!("reference completion must retain its generation");
        };
        assert_eq!(generation, 9);
        let ActionResult::References {
            id,
            target: actual,
            result: Err(message),
        } = *result
        else {
            panic!("reference timeout rejection must retain its request identity");
        };
        assert_eq!(id, 57);
        assert_eq!(actual, target);
        assert!(message.contains("Request timeout"));
        assert!(
            tokio::time::timeout(Duration::from_millis(20), listener.accept())
                .await
                .is_err()
        );
    }

    fn identified_read_actions(target: &ResourceTarget) -> [ActionIntent; 12] {
        [
            ActionIntent::Content {
                id: 41,
                target: target.clone(),
                content: ContentKind::Yaml,
            },
            ActionIntent::ManagedYaml {
                id: 42,
                target: target.clone(),
                body: "metadata: {}".to_owned(),
                decoded_body: None,
            },
            ActionIntent::LoadImages {
                id: 43,
                target: target.clone(),
            },
            ActionIntent::LoadTransferPod {
                id: 44,
                target: target.clone(),
            },
            ActionIntent::RefreshYaml {
                id: 45,
                target: target.clone(),
                include_managed_fields: true,
            },
            ActionIntent::RefreshDescribe {
                id: 46,
                target: target.clone(),
            },
            ActionIntent::StartLogs {
                request: super::super::LogRequest {
                    id: 47,
                    target: super::super::LogTarget::pod(
                        "team",
                        target.name.clone(),
                        Some("main".to_owned()),
                        false,
                    ),
                    range: super::super::LogRange::Tail,
                    tail_count: 100,
                    all_containers: false,
                },
            },
            ActionIntent::LoadRollback {
                id: 48,
                target: target.clone(),
            },
            ActionIntent::LoadDeploymentSelector {
                id: 49,
                target: target.clone(),
                replica_sets: false,
            },
            ActionIntent::LoadDeploymentSelector {
                id: 50,
                target: target.clone(),
                replica_sets: true,
            },
            ActionIntent::LoadReferences {
                id: 51,
                target: target.clone(),
            },
            ActionIntent::LoadBatchNavigation {
                id: 52,
                target: target.clone(),
            },
        ]
    }

    fn assert_identified_read_rejection(
        result: ActionResult,
        case: usize,
        target: &ResourceTarget,
    ) {
        let (actual_case, id, actual_target, message) = match result {
            ActionResult::OpenedContent { id, target, result } => {
                let ActionResult::Error(message) = *result else {
                    panic!("opening rejection must carry its terminal error");
                };
                (0, id, Some(target), message)
            }
            ActionResult::ManagedYaml {
                id,
                target,
                result: Err(message),
            } => (1, id, Some(target), message),
            ActionResult::Images {
                id,
                target,
                result: Err(message),
            } => (2, id, Some(target), message),
            ActionResult::TransferPod {
                id,
                target,
                result: Err(message),
            } => (3, id, Some(target), message),
            ActionResult::RefreshedYaml {
                id,
                target,
                result: Err(message),
            } => (4, id, Some(target), message),
            ActionResult::RefreshedDescribe {
                id,
                target,
                result: Err(message),
            } => (5, id, Some(target), message),
            ActionResult::LogBatch {
                id,
                lines,
                finished,
                error: Some(message),
            } => {
                assert_eq!(lines, Vec::<super::super::LogLine>::new());
                assert!(finished);
                (6, id, None, message)
            }
            ActionResult::RollbackLoaded {
                id,
                target,
                result: Err(message),
            } => (7, id, Some(target), message),
            ActionResult::DeploymentSelector {
                id,
                target,
                replica_sets,
                result: Err(message),
            } => (8 + usize::from(replica_sets), id, Some(target), message),
            ActionResult::References {
                id,
                target,
                result: Err(message),
            } => (10, id, Some(target), message),
            ActionResult::BatchNavigation {
                id,
                target,
                result: Err(message),
            } => (11, id, Some(target), message),
            other => panic!("read rejection must retain its result type: {other:?}"),
        };
        assert_eq!(actual_case, case);
        assert_eq!(id, 41 + u64::try_from(case).unwrap());
        assert_eq!(actual_target.as_ref(), (case != 6).then_some(target));
        assert!(message.contains("active view changed"));
    }

    #[tokio::test]
    async fn rejected_identified_reads_finish_with_original_ids_and_targets_without_api_calls() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = Client::try_from(Config::new(
            format!("http://{}", listener.local_addr().unwrap())
                .parse()
                .unwrap(),
        ))
        .unwrap();
        let current = WatchRequest {
            instance: None,
            generation: 8,
            context: "active-context".to_owned(),
            kind: ResourceKind::Pods.into(),
            namespace: Some("team".to_owned()),
            field_selector: None,
            label_selector: None,
        };
        let target = ResourceTarget {
            kind: current.kind.clone(),
            namespace: current.namespace.clone(),
            name: "loading-pod".to_owned(),
        };
        let task = tokio::spawn(std::future::pending::<()>());
        let handle = task.abort_handle();
        let mut running_read = Some(task);
        let mut running_logs = None;
        let (results, mut received) = mpsc::channel(1);
        for (scope, generation) in [(None, 8), (Some(&current), 7), (Some(&current), 9)] {
            for (case, intent) in identified_read_actions(&target).into_iter().enumerate() {
                dispatch_action(
                    client.clone(),
                    scope,
                    ConnectionOptions::default(),
                    ActionRequest { generation, intent },
                    results.clone(),
                    &mut running_read,
                    &mut running_logs,
                );
                let ActionResult::Scoped {
                    generation: actual,
                    result,
                } = received.try_recv().unwrap()
                else {
                    panic!("read rejection must retain the submitting generation");
                };
                assert_eq!(actual, generation);
                assert_identified_read_rejection(*result, case, &target);
                assert_eq!(running_read.as_ref().unwrap().id(), handle.id());
                assert!(!handle.is_finished());
                assert!(running_logs.is_none());
            }
        }
        running_read.take().unwrap().abort();
        assert!(
            tokio::time::timeout(Duration::from_millis(20), listener.accept())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn full_result_channel_delivers_rejection_after_capacity_frees_and_keeps_active_read() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = Client::try_from(Config::new(
            format!("http://{}", listener.local_addr().unwrap())
                .parse()
                .unwrap(),
        ))
        .unwrap();
        let target = ResourceTarget {
            kind: ResourceKind::Pods.into(),
            namespace: Some("team".to_owned()),
            name: "loading-pod".to_owned(),
        };
        let task = tokio::spawn(std::future::pending::<()>());
        let handle = task.abort_handle();
        let mut running_read = Some(task);
        let mut running_logs = None;
        let (results, mut received) = mpsc::channel(1);
        let earlier = ActionResult::Notice("earlier result fills the channel".to_owned());
        results.try_send(earlier.clone()).unwrap();
        dispatch_action(
            client,
            None,
            ConnectionOptions::default(),
            ActionRequest {
                generation: 8,
                intent: identified_read_actions(&target).into_iter().next().unwrap(),
            },
            results,
            &mut running_read,
            &mut running_logs,
        );
        assert_eq!(running_read.as_ref().unwrap().id(), handle.id());
        assert!(!handle.is_finished());
        assert!(running_logs.is_none());
        assert_eq!(received.try_recv().unwrap(), earlier);
        let result = tokio::time::timeout(Duration::from_secs(5), received.recv())
            .await
            .unwrap()
            .unwrap();
        let ActionResult::Scoped { generation, result } = result else {
            panic!("delayed rejection must retain its submitting generation");
        };
        assert_eq!(generation, 8);
        assert_identified_read_rejection(*result, 0, &target);
        assert_eq!(running_read.as_ref().unwrap().id(), handle.id());
        assert!(!handle.is_finished());
        running_read.take().unwrap().abort();
        assert!(
            tokio::time::timeout(Duration::from_millis(20), listener.accept())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn stale_generation_actions_never_reach_the_new_context_client() {
        let (client, server) = scale_client_fixture().await;
        let current = WatchRequest {
            instance: None,
            generation: 8,
            context: "new-context".to_owned(),
            kind: ResourceKind::Deployments.into(),
            namespace: Some("team".to_owned()),
            field_selector: None,
            label_selector: None,
        };
        let action = |generation, replicas| ActionRequest {
            generation,
            intent: ActionIntent::Scale {
                targets: vec![ResourceTarget {
                    kind: ResourceKind::Deployments.into(),
                    namespace: Some("team".to_owned()),
                    name: "worker".to_owned(),
                }],
                replicas,
            },
        };
        let (results, mut received) = mpsc::channel(4);
        let (mut running_read, mut running_logs) = (None, None);
        for (scope, generation) in [(Some(&current), 7), (Some(&current), 9), (None, 8)] {
            dispatch_action(
                client.clone(),
                scope,
                ConnectionOptions::default(),
                action(generation, 9),
                results.clone(),
                &mut running_read,
                &mut running_logs,
            );
            assert_scoped_rejection(received.try_recv().unwrap(), generation);
            assert!(running_read.is_none());
            assert!(running_logs.is_none());
        }
        // A current request still reaches the same fake API, proving the guard
        // does not merely disable action execution or use a disconnected client.
        dispatch_action(
            client,
            Some(&current),
            ConnectionOptions::default(),
            action(8, 0),
            results,
            &mut running_read,
            &mut running_logs,
        );
        let observed = tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .unwrap()
            .unwrap();
        let mut request_line = observed.lines().next().unwrap().split_whitespace();
        assert_eq!(request_line.next(), Some("PATCH"));
        assert_eq!(
            request_line.next().unwrap().split('?').next(),
            Some("/apis/apps/v1/namespaces/team/deployments/worker/scale")
        );
        let (_, body) = observed.split_once("\r\n\r\n").unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(body).unwrap()["spec"]["replicas"],
            0
        );
        let result = tokio::time::timeout(Duration::from_secs(5), received.recv())
            .await
            .unwrap();
        assert!(matches!(result, Some(ActionResult::Notice(_))));
    }

    #[tokio::test]
    async fn stale_generation_image_updates_are_scoped_and_never_call_the_api() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = Client::try_from(Config::new(
            format!("http://{}", listener.local_addr().unwrap())
                .parse()
                .unwrap(),
        ))
        .unwrap();
        let current = WatchRequest {
            instance: None,
            generation: 8,
            context: "new-context".to_owned(),
            kind: ResourceKind::Deployments.into(),
            namespace: Some("team".to_owned()),
            field_selector: None,
            label_selector: None,
        };
        let loaded = super::super::ImageSnapshot {
            target: ResourceTarget {
                kind: current.kind.clone(),
                namespace: current.namespace.clone(),
                name: "worker".to_owned(),
            },
            uid: "old-context-uid".to_owned(),
            resource_version: "100".to_owned(),
            images: vec![super::super::ContainerImage {
                name: "worker".to_owned(),
                image: "worker:v1".to_owned(),
                init: false,
            }],
        };
        let mut images = loaded.images.clone();
        images[0].image = "worker:v2".to_owned();
        let (results, mut received) = mpsc::channel(3);
        let (mut running_read, mut running_logs) = (None, None);
        for (scope, generation) in [(Some(&current), 7), (Some(&current), 9), (None, 8)] {
            dispatch_action(
                client.clone(),
                scope,
                ConnectionOptions::default(),
                ActionRequest {
                    generation,
                    intent: ActionIntent::SetImages {
                        snapshot: loaded.clone(),
                        images: images.clone(),
                    },
                },
                results.clone(),
                &mut running_read,
                &mut running_logs,
            );
            let ActionResult::Scoped {
                generation: actual,
                result,
            } = received.try_recv().unwrap()
            else {
                panic!("image rejection must retain the submitting generation");
            };
            assert_eq!(actual, generation);
            assert!(matches!(
                *result,
                ActionResult::ImagesUpdated { target, result: Err(message) }
                    if target == loaded.target && message.contains("active view changed")
            ));
            assert!(running_read.is_none());
            assert!(running_logs.is_none());
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(20), listener.accept())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn no_active_watch_sanitize_rejection_preserves_typed_scope_and_sends_no_api_request() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = Client::try_from(Config::new(
            format!("http://{}", listener.local_addr().unwrap())
                .parse()
                .unwrap(),
        ))
        .unwrap();
        let (results, mut received) = mpsc::channel(2);
        let (mut running_read, mut running_logs) = (None, None);
        for namespace in [None, Some("confirmed-team".to_owned())] {
            dispatch_action(
                client.clone(),
                None,
                ConnectionOptions::default(),
                ActionRequest {
                    generation: 8,
                    intent: ActionIntent::SanitizePods {
                        namespace: namespace.clone(),
                    },
                },
                results.clone(),
                &mut running_read,
                &mut running_logs,
            );
            let ActionResult::Scoped { generation, result } = received.try_recv().unwrap() else {
                panic!("sanitize rejection must retain the submitting generation");
            };
            assert_eq!(generation, 8);
            assert!(matches!(
                *result,
                ActionResult::Sanitized { namespace: actual, result: Err(message) }
                    if actual == namespace && message.contains("active view changed")
            ));
            assert!(running_read.is_none());
            assert!(running_logs.is_none());
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(20), listener.accept())
                .await
                .is_err()
        );
    }

    fn rollback_fixture() -> super::super::RollbackSnapshot {
        super::super::RollbackSnapshot {
            target: ResourceTarget {
                kind: ResourceDescriptor::new(
                    "apps",
                    "v1",
                    "apps/v1",
                    "ReplicaSet",
                    "replicasets",
                    true,
                    crate::model::ResourceOperations::all(),
                ),
                namespace: Some("team".to_owned()),
                name: "worker-old".to_owned(),
            },
            uid: "rs-uid".to_owned(),
            resource_version: "10".to_owned(),
            deployment: "worker".to_owned(),
            deployment_uid: "deployment-uid".to_owned(),
            deployment_resource_version: "20".to_owned(),
            revision: 1,
            template: serde_json::json!({"spec": {"containers": [{"name": "main", "image": "old"}]}}),
            annotations: std::collections::BTreeMap::new(),
            already_matches: false,
        }
    }

    #[tokio::test]
    async fn rollback_mutations_are_scoped_and_rejected_before_api_on_changed_views() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = Client::try_from(Config::new(
            format!("http://{}", listener.local_addr().unwrap())
                .parse()
                .unwrap(),
        ))
        .unwrap();
        let snapshot = rollback_fixture();
        let current = WatchRequest {
            instance: None,
            generation: 8,
            context: "active-context".to_owned(),
            kind: snapshot.target.kind.clone(),
            namespace: snapshot.target.namespace.clone(),
            field_selector: None,
            label_selector: None,
        };
        let (results, mut received) = mpsc::channel(1);
        let (mut running_read, mut running_logs) = (None, None);
        for (scope, generation) in [(None, 8), (Some(&current), 7), (Some(&current), 9)] {
            let intent = ActionIntent::Rollback {
                snapshot: Box::new(snapshot.clone()),
            };
            assert!(!intent.cancelable_read());
            dispatch_action(
                client.clone(),
                scope,
                ConnectionOptions::default(),
                ActionRequest { generation, intent },
                results.clone(),
                &mut running_read,
                &mut running_logs,
            );
            let ActionResult::Scoped {
                generation: actual,
                result,
            } = received.try_recv().unwrap()
            else {
                panic!("rollback rejection must remain scoped");
            };
            assert_eq!(actual, generation);
            assert!(
                matches!(*result, ActionResult::RolledBack { result: Err(message) }
                if message.contains("active view changed"))
            );
            assert!(running_read.is_none());
            assert!(running_logs.is_none());
        }
        // A noncancelable mutation completion still carries its originating generation.
        let mut invalid = snapshot;
        invalid.target.kind = ResourceKind::Pods.into();
        let task = spawn_action_task(
            client,
            "active-context".to_owned(),
            ConnectionOptions::default(),
            ActionRequest {
                generation: 8,
                intent: ActionIntent::Rollback {
                    snapshot: Box::new(invalid),
                },
            },
            results,
            false,
        );
        task.await.unwrap();
        let ActionResult::Scoped { generation, result } = received.try_recv().unwrap() else {
            panic!("rollback completion must remain scoped even though it is not cancelable");
        };
        assert_eq!(generation, 8);
        assert!(matches!(
            *result,
            ActionResult::RolledBack { result: Err(_) }
        ));
        assert!(
            tokio::time::timeout(Duration::from_millis(20), listener.accept())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn stale_generation_cancellation_preserves_current_read_but_matching_stop_aborts_it() {
        let config = Config::new("http://127.0.0.1:1".parse().unwrap());
        let client = Client::try_from(config).unwrap();
        let current = WatchRequest {
            instance: None,
            generation: 8,
            context: "new-context".to_owned(),
            kind: ResourceKind::Pods.into(),
            namespace: Some("team".to_owned()),
            field_selector: None,
            label_selector: None,
        };
        let task = tokio::spawn(std::future::pending::<()>());
        let handle = task.abort_handle();
        let mut running_read = Some(task);
        let mut running_logs = None;
        let (results, mut received) = mpsc::channel(3);
        for (scope, generation) in [(Some(&current), 7), (Some(&current), 9), (None, 8)] {
            dispatch_action(
                client.clone(),
                scope,
                ConnectionOptions::default(),
                ActionRequest {
                    generation,
                    intent: ActionIntent::StopContentRefresh,
                },
                results.clone(),
                &mut running_read,
                &mut running_logs,
            );
            assert_scoped_rejection(received.try_recv().unwrap(), generation);
            assert_eq!(running_read.as_ref().unwrap().id(), handle.id());
            assert!(!handle.is_finished());
        }
        dispatch_action(
            client,
            Some(&current),
            ConnectionOptions::default(),
            ActionRequest {
                generation: current.generation,
                intent: ActionIntent::StopContentRefresh,
            },
            results,
            &mut running_read,
            &mut running_logs,
        );
        assert!(running_read.is_none());
        tokio::time::timeout(Duration::from_secs(5), async {
            while !handle.is_finished() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            received.try_recv(),
            Err(mpsc::error::TryRecvError::Disconnected)
        );
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
            "metadata": {
                "name": "worker-0",
                "ownerReferences": [{
                    "apiVersion": "karpenter.sh/v1",
                    "kind": "NodeClaim",
                    "name": "worker-0-claim",
                    "uid": "nodeclaim-worker-0",
                    "controller": true
                }]
            },
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
        let owner = node_row(&node).owner.expect("NodeClaim owner");
        assert_eq!(owner.api_version.as_ref(), "karpenter.sh/v1");
        assert_eq!(owner.kind.as_ref(), "NodeClaim");
        assert_eq!(owner.name.as_ref(), "worker-0-claim");
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
    fn deployment_row_declares_pods_as_label_selected_children() {
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
        assert_eq!(child.kind.known_kind(), Some(ResourceKind::Pods));
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
        let connection = ConnectionOptions {
            kubeconfig: Some("/tmp/explicit config".into()),
            ..ConnectionOptions::default()
        };
        let args = context_delete_args_with(&connection, "inactive");
        assert_eq!(
            args,
            [
                "--kubeconfig",
                "/tmp/explicit config",
                "config",
                "delete-context",
                "inactive"
            ]
            .map(std::ffi::OsString::from)
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
        let connection = ConnectionOptions {
            kubeconfig: Some("/tmp/explicit config".into()),
            impersonate: Some("reader".to_owned()),
            ..ConnectionOptions::default()
        };
        let args = describe_args_with(&connection, "staging", &pod);
        for (flag, value) in [
            ("--kubeconfig", "/tmp/explicit config"),
            ("--as", "reader"),
            ("--context", "staging"),
            ("--namespace", "prod"),
        ] {
            assert!(
                args.windows(2)
                    .any(|pair| pair[0] == flag && pair[1] == value)
            );
        }
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
    fn projects_api_service_list_items_without_type_meta_like_kubectl() {
        let local: DynamicObject = serde_json::from_value(serde_json::json!({
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
        let row = api_service_row(&local);
        assert_eq!(row.namespace.as_ref(), "-");
        assert_eq!(row.name.as_ref(), "v1.");
        assert_eq!(row.ready.as_ref(), "Local");
        assert_eq!(row.status.as_ref(), "True");
        assert_eq!(row.created_unix, 1_704_067_200);

        let remote: DynamicObject = serde_json::from_value(serde_json::json!({
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
        let row = api_service_row(&remote);
        assert_eq!(row.name.as_ref(), "v1beta1.metrics.k8s.io");
        assert_eq!(row.ready.as_ref(), "kube-system/metrics-server");
        assert_eq!(row.status.as_ref(), "False (MissingEndpoints)");

        let unknown: DynamicObject = serde_json::from_value(serde_json::json!({
            "metadata": {"name": "v1.example.io"},
            "spec": {"service": null}
        }))
        .expect("APIService without a condition");
        let row = api_service_row(&unknown);
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

        let yaml = yaml_content_from_object(object.clone(), &target).expect("secret yaml");
        assert!(yaml.body.contains("c2VjcmV0Cg=="));
        assert!(!yaml.body.contains("managedFields"));
        let decoded = yaml.decoded_body.expect("decoded body");
        let decoded_value: serde_json::Value =
            serde_yaml::from_str(&decoded).expect("decoded yaml value");
        assert_eq!(decoded_value["data"]["token"], "secret\n");
        assert_eq!(decoded_value["data"]["binary"], "0x00ff10");
        assert_eq!(yaml.with_managed_fields.as_deref(), Some(""));
        assert_eq!(yaml.decoded_with_managed_fields.as_deref(), Some(""));

        let (managed, decoded_managed) =
            managed_yaml_content_from_object(&object, &yaml.body, Some(&decoded))
                .expect("managed-fields yaml");
        assert!(managed.contains("managedFields"));
        let decoded_managed = decoded_managed.expect("decoded managed body");
        assert!(decoded_managed.contains("managedFields"));
        assert!(decoded_managed.contains("0x00ff10"));

        let refreshed = yaml_content_with_managed_fields(object.clone(), &target, true).unwrap();
        assert_eq!(refreshed.body, yaml.body);
        assert_eq!(refreshed.decoded_body.as_deref(), Some(decoded.as_str()));
        assert_eq!(
            refreshed.with_managed_fields.as_deref(),
            Some(managed.as_str())
        );
        assert_eq!(
            refreshed.decoded_with_managed_fields.as_deref(),
            Some(decoded_managed.as_str())
        );
        object.metadata.managed_fields = None;
        let refreshed = yaml_content_with_managed_fields(object, &target, true).unwrap();
        assert_eq!(refreshed.with_managed_fields, None);
        assert_eq!(refreshed.decoded_with_managed_fields, None);
    }

    #[tokio::test]
    async fn initial_content_keeps_request_identity_on_success_and_redacted_failure() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        for (status, body, succeeds) in [
            (
                "200 OK",
                r#"{"kind":"Pod","apiVersion":"v1","metadata":{"name":"worker","namespace":"team"},"spec":{"containers":[]}}"#,
                true,
            ),
            (
                "403 Forbidden",
                r#"{"kind":"Status","apiVersion":"v1","status":"Failure","reason":"Forbidden","message":"denied fixture-sensitive-token","code":403}"#,
                false,
            ),
        ] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let client = Client::try_from(Config::new(
                format!("http://{}", listener.local_addr().unwrap())
                    .parse()
                    .unwrap(),
            ))
            .unwrap();
            let server = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                let mut chunk = [0; 1024];
                while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                    let length = socket.read(&mut chunk).await.unwrap();
                    assert_ne!(length, 0);
                    request.extend_from_slice(&chunk[..length]);
                }
                assert!(
                    String::from_utf8_lossy(&request)
                        .starts_with("GET /api/v1/namespaces/team/pods/worker")
                );
                socket.write_all(format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()
                ).as_bytes()).await.unwrap();
            });
            let target = ResourceTarget {
                kind: ResourceKind::Pods.into(),
                namespace: Some("team".to_owned()),
                name: "worker".to_owned(),
            };
            let connection = ConnectionOptions {
                token: Some("fixture-sensitive-token".to_owned()),
                ..ConnectionOptions::default()
            };
            let result = handle_action(
                client,
                "fixture",
                &connection,
                ActionIntent::Content {
                    id: 91,
                    target: target.clone(),
                    content: ContentKind::Yaml,
                },
            )
            .await;
            let ActionResult::OpenedContent {
                id,
                target: returned,
                result,
            } = result
            else {
                panic!("initial content must retain its request identity");
            };
            assert_eq!(id, 91);
            assert_eq!(returned, target);
            if succeeds {
                let ActionResult::Content { body, content, .. } = *result else {
                    panic!("expected YAML content");
                };
                assert_eq!(content, ContentKind::Yaml);
                assert!(body.contains("name: worker"));
            } else {
                let ActionResult::Error(error) = *result else {
                    panic!("expected scoped opening failure");
                };
                assert!(error.contains("Forbidden"));
                assert!(!error.contains("fixture-sensitive-token"));
            }
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn managed_fields_api_failure_keeps_request_identity_and_redacts_credentials() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = Client::try_from(Config::new(
            format!("http://{}", listener.local_addr().unwrap())
                .parse()
                .unwrap(),
        ))
        .unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut chunk = [0; 1024];
            while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                let length = socket.read(&mut chunk).await.unwrap();
                assert_ne!(length, 0);
                request.extend_from_slice(&chunk[..length]);
            }
            assert!(
                String::from_utf8_lossy(&request)
                    .starts_with("GET /api/v1/namespaces/team/pods/worker")
            );
            let body = r#"{"kind":"Status","apiVersion":"v1","status":"Failure","reason":"Forbidden","message":"denied fixture-sensitive-token","code":403}"#;
            socket.write_all(format!(
                "HTTP/1.1 403 Forbidden\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()
            ).as_bytes()).await.unwrap();
        });
        let target = ResourceTarget {
            kind: ResourceKind::Pods.into(),
            namespace: Some("team".to_owned()),
            name: "worker".to_owned(),
        };
        let connection = ConnectionOptions {
            token: Some("fixture-sensitive-token".to_owned()),
            ..ConnectionOptions::default()
        };
        let result = handle_action(
            client,
            "fixture",
            &connection,
            ActionIntent::ManagedYaml {
                id: 73,
                target: target.clone(),
                body: "kind: Pod".to_owned(),
                decoded_body: None,
            },
        )
        .await;
        let ActionResult::ManagedYaml {
            id,
            target: returned,
            result: Err(error),
        } = result
        else {
            panic!("managed-fields errors must retain their request identity");
        };
        assert_eq!(id, 73);
        assert_eq!(returned, target);
        assert!(error.contains("Forbidden"));
        assert!(!error.contains("fixture-sensitive-token"));
        server.await.unwrap();
    }

    #[test]
    fn managed_fields_toggle_preserves_the_displayed_snapshot() {
        let original = "apiVersion: v1\nkind: Secret\nmetadata:\n  annotations:\n    keep: original\n  labels:\n    z-last: unchanged\n  name: example\n  resourceVersion: '1'\ndata:\n  token: b2xk\nspec:\n  managedFields: keep-this\nstatus:\n  phase: Old\n";
        let decoded = original.replace("b2xk", "old");
        let latest: DynamicObject = serde_json::from_value(serde_json::json!({
            "apiVersion": "v1", "kind": "Secret",
            "metadata": {
                "name": "example", "resourceVersion": "2",
                "annotations": {"keep": "changed"},
                "managedFields": [{"manager": "controller", "operation": "Update", "apiVersion": "v1"}]
            },
            "data": {"token": "bmV3"},
            "spec": {"managedFields": "changed"},
            "status": {"phase": "New"}
        })).unwrap();
        let (body, decoded_body) =
            managed_yaml_content_from_object(&latest, original, Some(&decoded)).unwrap();
        for (expanded, baseline) in [(body, original), (decoded_body.unwrap(), decoded.as_str())] {
            let mut value: serde_json::Value = serde_yaml::from_str(&expanded).unwrap();
            assert_eq!(
                value["metadata"]["managedFields"][0]["manager"],
                "controller"
            );
            value["metadata"]
                .as_object_mut()
                .unwrap()
                .remove("managedFields");
            assert_eq!(
                value,
                serde_yaml::from_str::<serde_json::Value>(baseline).unwrap()
            );
            let (prefix, suffix) = baseline.split_once("  name:").unwrap();
            assert!(expanded.starts_with(&format!("{prefix}  managedFields:\n")));
            assert!(expanded.ends_with(suffix));
        }
    }

    #[test]
    fn inserts_managed_fields_in_dictionary_order_within_metadata() {
        let fragment = "managedFields:\n- manager: controller\n";
        let inserted = "  managedFields:\n  - manager: controller\n";
        for (before, after) in [
            (
                "metadata:\n  annotations:\n    z-key: value\n  finalizers:\n  - z-finalizer\n  labels:\n    z-label: value\n",
                "  name: example\n  namespace: default\n  resourceVersion: '1'\n",
            ),
            ("metadata:\n", "  name: example\n"),
            (
                "metadata:\n  labels:\n    app: example\n",
                "spec:\n  value: keep\n",
            ),
            ("metadata:\n  labels:\n    app: example\n", ""),
        ] {
            assert_eq!(
                insert_managed_fields_yaml(&format!("{before}{after}"), fragment).unwrap(),
                format!("{before}{inserted}{after}"),
            );
        }
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
        let group_ns: Subject = serde_json::from_value(serde_json::json!({
            "kind": "Group",
            "name": "system:serviceaccounts:argo"
        }))
        .expect("group subject");
        assert!(service_account_subject_matches(
            &group_ns,
            "argo",
            "apiserver-admin",
            None,
        ));
        assert!(!service_account_subject_matches(
            &group_ns,
            "default",
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
    fn kubernetes_table_projects_apiservice_service_and_availability() {
        let page = decode_table_list(serde_json::json!({
            "kind": "Table",
            "apiVersion": "meta.k8s.io/v1",
            "metadata": {"resourceVersion": "42"},
            "columnDefinitions": [
                {"name": "Name", "format": "name", "priority": 0},
                {"name": "Service", "format": "string", "priority": 0},
                {"name": "Available", "format": "string", "priority": 0},
                {"name": "Age", "format": "string", "priority": 0}
            ],
            "rows": [{
                "cells": [
                    "v1alpha2.cloud.streamnative.io",
                    "sn-system/cloud-api-server-apiservice",
                    "True",
                    "4y194d"
                ],
                "object": {
                    "apiVersion": "meta.k8s.io/v1",
                    "kind": "PartialObjectMetadata",
                    "metadata": {
                        "name": "v1alpha2.cloud.streamnative.io",
                        "resourceVersion": "42",
                        "creationTimestamp": "2021-01-01T00:00:00Z"
                    }
                }
            }]
        }))
        .expect("APIService table");

        let row = page.rows.first().expect("APIService row");
        assert_eq!(row.name.as_ref(), "v1alpha2.cloud.streamnative.io");
        assert_eq!(row.ready.as_ref(), "sn-system/cloud-api-server-apiservice");
        assert_eq!(row.status.as_ref(), "True");
        assert_eq!(row.created_unix, 1_609_459_200);
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
    fn only_obsolete_read_actions_are_safe_to_cancel() {
        let target = ResourceTarget {
            kind: ResourceKind::Pods.into(),
            namespace: Some("prod".to_owned()),
            name: "api-0".to_owned(),
        };
        assert!(
            ActionIntent::Content {
                id: 1,
                target: target.clone(),
                content: ContentKind::Yaml,
            }
            .cancelable_read()
        );
        assert!(
            ActionIntent::Containers {
                target: target.clone(),
            }
            .cancelable_read()
        );
        assert!(
            !ActionIntent::Delete {
                targets: vec![target],
                immediate: false,
            }
            .cancelable_read()
        );
    }

    #[test]
    fn typed_initial_list_publishes_rows_before_init_done() {
        let kind: ResourceDescriptor = ResourceKind::Pods.into();
        let request = WatchRequest {
            instance: None,
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

        let columns: Arc<[TableColumn]> = Arc::from([]);
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
                &columns,
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
    fn typed_rows_progressively_replace_fast_rows_during_initialization() {
        let kind: ResourceDescriptor = ResourceKind::Pods.into();
        let request = WatchRequest {
            instance: None,
            generation: 8,
            context: "demo".to_owned(),
            kind: kind.clone(),
            namespace: None,
            field_selector: None,
            label_selector: None,
        };
        let (snapshots, receiver) = watch::channel(ResourceSnapshot::loading(8, kind));
        let mut state = TypedWatchState::default();
        let rows = (0..3)
            .map(|index| {
                let row = Arc::new(ResourceRow::new(
                    "-",
                    format!("pod-{index}"),
                    "Running",
                    "1/1",
                    "",
                    0,
                ));
                (Arc::clone(&row.key), row)
            })
            .collect();

        state.apply_fast_initial(
            FastInitialList {
                columns: Arc::from([]),
                rows,
                hydrated: false,
                quality: 0,
            },
            &snapshots,
            &request,
        );
        apply_initial_row(
            &mut state.initializing,
            Arc::new(ResourceRow::new("prod", "pod-0", "Running", "1/1", "", 0)),
            state.fast_initial_published,
            &mut state.initial_progress,
            &snapshots,
            &request,
            &state.table_columns,
            &mut state.revision,
        );
        state.dirty = true;
        state.publish_ready(&snapshots, &request);

        let snapshot = receiver.borrow();
        assert_eq!(snapshot.rows.len(), 3);
        assert_eq!(
            snapshot
                .rows
                .iter()
                .find(|row| row.name.as_ref() == "pod-0")
                .expect("typed row")
                .namespace
                .as_ref(),
            "prod"
        );
        assert_eq!(snapshot.state, SourceState::Loading { loaded: 1 });
        assert_eq!(snapshot.revision, 2);
    }

    #[test]
    fn pod_cache_matches_only_unfiltered_all_namespace_pod_views() {
        let request = WatchRequest {
            instance: None,
            generation: 1,
            context: "test".to_owned(),
            kind: ResourceKind::Pods.into(),
            namespace: None,
            field_selector: None,
            label_selector: None,
        };
        assert!(is_all_namespace_pod_request(&request, "test"));

        let mut scoped = request.clone();
        scoped.namespace = Some("default".to_owned());
        assert!(!is_all_namespace_pod_request(&scoped, "test"));

        let mut filtered = request.clone();
        filtered.label_selector = Some("app=api".to_owned());
        assert!(!is_all_namespace_pod_request(&filtered, "test"));
        let mut instance = request.clone();
        instance.instance = Some(ResourceTarget {
            kind: ResourceKind::Pods.into(),
            namespace: Some("default".to_owned()),
            name: "api-0".to_owned(),
        });
        assert!(!is_all_namespace_pod_request(&instance, "test"));
        assert!(!is_all_namespace_pod_request(&request, "production"));
    }

    #[tokio::test]
    async fn pod_metrics_are_started_for_filtered_views() {
        let client = Client::try_from(Config::new("http://127.0.0.1:1".parse().unwrap())).unwrap();
        let settings = KubernetesSettings {
            connection: ConnectionOptions::default(),
            initial_context: "test".to_owned(),
            publish_interval: Duration::from_millis(100),
            refresh_interval: watch::channel(Duration::from_secs(5)).1,
            streaming_lists: false,
        };
        for field_selector in [
            None,
            Some("spec.nodeName=worker-07"),
            Some("metadata.name=api-0"),
            Some("kux.ownerUID=parent-123"),
        ] {
            for namespace in [None, Some("default")] {
                let request = WatchRequest {
                    instance: None,
                    generation: 1,
                    context: "test".to_owned(),
                    kind: ResourceKind::Pods.into(),
                    namespace: namespace.map(str::to_owned),
                    field_selector: field_selector.map(str::to_owned),
                    label_selector: Some("app=api".to_owned()),
                };
                let (sender, _receiver) = mpsc::channel(8);
                let tasks = spawn_enrichment_tasks(client.clone(), &request, &settings, sender);
                assert_eq!(tasks.len(), 1, "pod metrics task for {request:?}");
                // Abort before yielding; this unit test performs no network calls.
                drop(tasks);
            }
        }
    }

    #[test]
    fn owner_uid_filters_are_local_and_match_exact_controller_uids() {
        let mut request = WatchRequest {
            instance: None,
            generation: 1,
            context: "test".to_owned(),
            kind: ResourceKind::Pods.into(),
            namespace: Some("prod".to_owned()),
            field_selector: Some("kux.ownerUID=parent-123".to_owned()),
            label_selector: None,
        };
        assert_eq!(server_field_selector(&request), None);
        let matching = ResourceRow::new("prod", "child", "Running", "1/1", "", 0)
            .with_owner_reference_data(OwnerReferenceData {
                api_version: Arc::from("example.io/v1"),
                kind: Arc::from("Widget"),
                name: Arc::from("parent"),
                uid: Arc::from("parent-123"),
            });
        assert!(row_matches_owner_filter(&matching, &request));

        request.field_selector = Some("metadata.name=child".to_owned());
        assert_eq!(server_field_selector(&request), Some("metadata.name=child"));
        assert!(row_matches_owner_filter(&matching, &request));
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
    fn hydrates_fast_pod_rows_for_the_regular_list_layout() {
        let columns = vec![
            TableColumn {
                name: Arc::from("Name"),
                format: Arc::from("name"),
                priority: 0,
            },
            TableColumn {
                name: Arc::from("Restarts"),
                format: Arc::from(""),
                priority: 0,
            },
            TableColumn {
                name: Arc::from("Node"),
                format: Arc::from(""),
                priority: 1,
            },
        ];
        let mut row = ResourceRow::new("prod", "api-0", "Running", "1/1", "", 0);
        row.table_cells = Arc::from([
            TableCell {
                text: Arc::from("api-0"),
                timestamp_unix: None,
            },
            TableCell {
                text: Arc::from("3 (2m ago)"),
                timestamp_unix: None,
            },
            TableCell {
                text: Arc::from("worker-1"),
                timestamp_unix: None,
            },
        ]);

        let row = hydrate_fast_initial_row(&ResourceKind::Pods.into(), &columns, row);

        let pod = row.pod_data().expect("fast row has Pod data");
        assert_eq!(pod.restarts, 3);
        assert_eq!(pod.node.as_ref(), "worker-1");
    }

    #[test]
    fn progressive_deployment_rows_preserve_selector_for_pod_and_replica_set_navigation() {
        let columns = vec![TableColumn {
            name: Arc::from("Selector"),
            format: Arc::from(""),
            priority: 1,
        }];
        let row = ResourceRow::new("team", "api", "Ready", "1/1", "", 0).with_table_cells(vec![
            TableCell {
                text: Arc::from("app=api,tier in (web,worker)"),
                timestamp_unix: None,
            },
        ]);
        let row = hydrate_fast_initial_row(&ResourceKind::Deployments.into(), &columns, row);
        let child = row
            .child_resource_data()
            .expect("progressive Deployment selector");
        assert_eq!(child.kind.known_kind(), Some(ResourceKind::Pods));
        assert_eq!(
            child.label_selector.as_ref(),
            "app=api,tier in (web,worker)"
        );
        assert_eq!(row.namespace.as_ref(), "team");
    }

    #[test]
    fn parses_server_table_age_units_for_metadata_free_rows() {
        assert_eq!(parse_table_age("42s"), Some(42));
        assert_eq!(parse_table_age("7m"), Some(420));
        assert_eq!(parse_table_age("3h"), Some(10_800));
        assert_eq!(parse_table_age("2d"), Some(172_800));
        assert_eq!(parse_table_age("1y"), Some(31_536_000));
        assert_eq!(parse_table_age("<unknown>"), None);
    }

    #[test]
    fn hydrates_fast_node_status_and_version_like_the_typed_row() {
        let columns = vec![
            TableColumn {
                name: Arc::from("Name"),
                format: Arc::from("name"),
                priority: 0,
            },
            TableColumn {
                name: Arc::from("Version"),
                format: Arc::from(""),
                priority: 0,
            },
        ];
        let mut row = ResourceRow::new("", "worker-1", "NotReady,SchedulingDisabled", "", "", 0);
        row.table_cells = Arc::from([
            TableCell {
                text: Arc::from("worker-1"),
                timestamp_unix: None,
            },
            TableCell {
                text: Arc::from("v1.34.8"),
                timestamp_unix: None,
            },
        ]);

        let row = hydrate_fast_initial_row(&ResourceKind::Nodes.into(), &columns, row);

        assert_eq!(row.status.as_ref(), "NotReady,Unschedulable");
        let node = row.node_data().expect("fast row has Node data");
        assert!(node.unschedulable);
        assert_eq!(node.version.as_ref(), "v1.34.8");
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
    #[ignore = "requires KUX_LIVE_CONTEXT and access to a Kubernetes cluster"]
    async fn live_api_service_watch_values_match_kubectl() {
        let context = std::env::var("KUX_LIVE_CONTEXT").expect("KUX_LIVE_CONTEXT");
        let output = tokio::process::Command::new("kubectl")
            .args(["--context", &context, "get", "apiservice", "--no-headers"])
            .output()
            .await
            .expect("kubectl APIService list");
        assert!(output.status.success());
        let expected: Vec<_> = String::from_utf8(output.stdout)
            .expect("kubectl text")
            .lines()
            .map(|line| {
                let cells: Vec<_> = line.split_whitespace().collect();
                (
                    cells[0].to_owned(),
                    cells[1].to_owned(),
                    cells[2..cells.len() - 1].join(" "),
                )
            })
            .collect();
        assert_ne!(expected.len(), 0);
        let kind = ResourceDescriptor::new(
            "apiregistration.k8s.io",
            "v1",
            "apiregistration.k8s.io/v1",
            "APIService",
            "apiservices",
            false,
            crate::model::ResourceOperations::all(),
        );
        let request = WatchRequest {
            instance: None,
            generation: 1,
            context: context.clone(),
            kind: kind.clone(),
            namespace: None,
            field_selector: None,
            label_selector: None,
        };
        let (snapshots, mut receiver) = watch::channel(ResourceSnapshot::loading(1, kind));
        let client = client_for_context(&context)
            .await
            .expect("Kubernetes client");
        let task = tokio::spawn(run_watch(
            client,
            KubernetesSettings {
                connection: ConnectionOptions::default(),
                initial_context: context,
                publish_interval: Duration::from_millis(100),
                refresh_interval: watch::channel(Duration::from_secs(5)).1,
                streaming_lists: false,
            },
            request,
            snapshots,
        ));
        let result = tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                receiver.changed().await.expect("APIService watch active");
                let snapshot = receiver.borrow_and_update().clone();
                // Table bootstrap must have been replaced by the completed object list.
                if snapshot.state == SourceState::Ready
                    && snapshot.rows.iter().all(|row| row.table_cells.is_empty())
                {
                    return snapshot;
                }
            }
        })
        .await;
        task.abort();
        let snapshot = result.expect("completed APIService list within 30 seconds");
        assert_eq!(snapshot.rows.len(), expected.len());
        for (name, service, available) in expected {
            let row = snapshot
                .rows
                .iter()
                .find(|row| row.name.as_ref() == name)
                .unwrap_or_else(|| panic!("missing APIService {name}"));
            assert_eq!(row.ready.as_ref(), service, "{name} SERVICE");
            assert_eq!(row.status.as_ref(), available, "{name} AVAILABLE");
        }
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
    #[ignore = "requires KUX_LIVE_CONTEXT and access to a Kubernetes cluster"]
    async fn live_generic_child_discovery_finds_a_deployment_replica_set() {
        let context = std::env::var("KUX_LIVE_CONTEXT").expect("KUX_LIVE_CONTEXT");
        let client = client_for_context(&context)
            .await
            .expect("Kubernetes client");
        let deployment = Api::<Deployment>::all(client.clone())
            .list(&ListParams::default().limit(1))
            .await
            .expect("Deployment list")
            .items
            .into_iter()
            .next()
            .expect("at least one Deployment");
        let target = ResourceTarget {
            kind: ResourceKind::Deployments.into(),
            namespace: deployment.namespace(),
            name: deployment.name_any(),
        };
        let resources = discover_resources(client.clone())
            .await
            .expect("API discovery");

        let result = find_child_resource(client, target.clone(), resources)
            .await
            .expect("child discovery");
        assert!(matches!(
            result,
            ActionResult::ChildResource { parent, kind, .. }
                if parent == target && kind.is_apps_v1_replica_set()
        ));
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

    #[tokio::test]
    #[ignore = "requires KUX_LIVE_CONTEXT and access to a Kubernetes cluster"]
    async fn live_unpaged_compact_table_list_reports_elapsed_time() {
        let context = std::env::var("KUX_LIVE_CONTEXT").expect("KUX_LIVE_CONTEXT");
        let client = client_for_context(&context)
            .await
            .expect("Kubernetes client");
        let api = dynamic_api(client.clone(), &ResourceKind::Pods.into(), None);
        let raw = Request::new(api.resource_url())
            .list(&ListParams::default())
            .expect("Table list request");
        let raw =
            table_request_with_include(raw, TABLE_FAST_INCLUDE_OBJECT).expect("Table request");
        let started = Instant::now();
        let value = client
            .request::<serde_json::Value>(raw)
            .await
            .expect("Table response");
        let elapsed = started.elapsed();
        let page = decode_table_list(value).expect("decoded Table response");
        eprintln!("compact table rows={} elapsed={elapsed:?}", page.rows.len());
    }

    #[tokio::test]
    #[ignore = "requires KUX_LIVE_CONTEXT, KUX_LIVE_NODE and access to pod metrics"]
    async fn live_node_pods_include_metrics() {
        let context = std::env::var("KUX_LIVE_CONTEXT").expect("KUX_LIVE_CONTEXT");
        let node = std::env::var("KUX_LIVE_NODE").expect("KUX_LIVE_NODE");
        let client = client_for_context(&context)
            .await
            .expect("Kubernetes client");
        let request = WatchRequest {
            instance: None,
            generation: 1,
            context: context.clone(),
            kind: ResourceKind::Pods.into(),
            namespace: None,
            field_selector: Some(format!("spec.nodeName={node}")),
            label_selector: None,
        };
        let (snapshots, mut receiver) =
            watch::channel(ResourceSnapshot::loading(1, request.kind.clone()));
        let task = tokio::spawn(run_watch(
            client,
            KubernetesSettings {
                connection: ConnectionOptions::default(),
                initial_context: context,
                publish_interval: Duration::from_millis(100),
                refresh_interval: watch::channel(Duration::from_secs(5)).1,
                streaming_lists: false,
            },
            request,
            snapshots,
        ));
        let result = tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                receiver.changed().await.expect("pod watch active");
                let snapshot = receiver.borrow_and_update().clone();
                if snapshot.state == SourceState::Ready
                    && snapshot.rows.iter().any(|row| {
                        row.pod_data().is_some_and(|pod| {
                            pod.cpu_millis.is_some() && pod.memory_bytes.is_some()
                        })
                    })
                {
                    return snapshot;
                }
            }
        })
        .await;
        task.abort();
        let snapshot = result.expect("node-scoped pod metrics arrive within 30 seconds");
        assert!(
            snapshot
                .rows
                .iter()
                .all(|row| row.pod_data().is_some_and(|pod| pod.node.as_ref() == node))
        );
        let with_metrics = snapshot
            .rows
            .iter()
            .filter(|row| {
                row.pod_data()
                    .is_some_and(|pod| pod.cpu_millis.is_some() && pod.memory_bytes.is_some())
            })
            .count();
        eprintln!(
            "node-scoped pods={} with CPU and memory metrics={with_metrics}",
            snapshot.rows.len()
        );
    }

    #[tokio::test]
    #[ignore = "requires KUX_LIVE_CONTEXT and access to a Kubernetes cluster"]
    async fn live_all_namespace_pod_watch_reports_first_rows_and_ready_time() {
        let context = std::env::var("KUX_LIVE_CONTEXT").expect("KUX_LIVE_CONTEXT");
        let client = client_for_context(&context)
            .await
            .expect("Kubernetes client");
        let request = WatchRequest {
            instance: None,
            generation: 1,
            context: context.clone(),
            kind: ResourceKind::Pods.into(),
            namespace: None,
            field_selector: None,
            label_selector: None,
        };
        let (snapshots, mut receiver) = watch::channel(ResourceSnapshot::loading(
            request.generation,
            request.kind.clone(),
        ));
        let started = Instant::now();
        let task = tokio::spawn(run_watch(
            client,
            KubernetesSettings {
                connection: ConnectionOptions::default(),
                initial_context: context,
                publish_interval: Duration::from_millis(100),
                refresh_interval: watch::channel(Duration::from_secs(5)).1,
                streaming_lists: true,
            },
            request,
            snapshots,
        ));
        let mut first_rows = None;
        let mut complete_columns = None;
        let mut largest_loading = None;
        let (ready, rows, missing_namespaces) =
            tokio::time::timeout(Duration::from_secs(30), async {
                loop {
                    receiver.changed().await.expect("Pod watch remains active");
                    let snapshot = receiver.borrow_and_update().clone();
                    if first_rows.is_none() && !snapshot.rows.is_empty() {
                        first_rows = Some(started.elapsed());
                    }
                    if complete_columns.is_none()
                        && snapshot.rows.len() > INITIAL_LIST_PROGRESS_ROWS
                        && snapshot.rows.iter().all(|row| {
                            !matches!(row.namespace.as_ref(), "" | "-")
                                && row
                                    .pod_data()
                                    .is_some_and(|pod| !pod.default_container.is_empty())
                        })
                    {
                        complete_columns = Some(started.elapsed());
                    }
                    if matches!(snapshot.state, SourceState::Loading { .. })
                        && largest_loading
                            .as_ref()
                            .is_none_or(|(rows, _)| snapshot.rows.len() > *rows)
                    {
                        largest_loading = Some((snapshot.rows.len(), started.elapsed()));
                    }
                    if snapshot.state == SourceState::Ready {
                        let missing_namespaces = snapshot
                            .rows
                            .iter()
                            .filter(|row| matches!(row.namespace.as_ref(), "" | "-"))
                            .count();
                        break (started.elapsed(), snapshot.rows.len(), missing_namespaces);
                    }
                }
            })
            .await
            .expect("Pod watch becomes ready within 30 seconds");
        task.abort();
        eprintln!(
            "first_rows={first_rows:?}, complete_columns={complete_columns:?}, \
             largest_loading={largest_loading:?}, ready={ready:?}, rows={rows}"
        );
        assert!(
            first_rows.is_some_and(|elapsed| elapsed < Duration::from_secs(1)),
            "the first Pod page must appear within one second"
        );
        assert!(
            complete_columns.is_some_and(|elapsed| elapsed < Duration::from_secs(10)),
            "complete Pod columns must arrive before watch initialization finishes"
        );
        assert_eq!(missing_namespaces, 0, "ready Pods retain namespaces");
    }

    #[tokio::test]
    #[ignore = "requires KUX_LIVE_CONTEXT and access to a Kubernetes cluster"]
    async fn live_warm_pod_cache_publishes_250_rows_within_one_second() {
        let context = std::env::var("KUX_LIVE_CONTEXT").expect("KUX_LIVE_CONTEXT");
        let client = client_for_context(&context)
            .await
            .expect("Kubernetes client");
        let mut cache = PodWatchCache::spawn(
            client,
            KubernetesSettings {
                connection: ConnectionOptions::default(),
                initial_context: context.clone(),
                publish_interval: Duration::from_millis(100),
                refresh_interval: watch::channel(Duration::from_secs(5)).1,
                streaming_lists: true,
            },
            context.clone(),
        );
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                let snapshot = cache.receiver.borrow_and_update().clone();
                if snapshot.rows.len() >= 250 && snapshot.state == SourceState::Ready {
                    break;
                }
                cache
                    .receiver
                    .changed()
                    .await
                    .expect("Pod cache remains active");
            }
        })
        .await
        .expect("Pod cache warms within 15 seconds");

        let request = WatchRequest {
            instance: None,
            generation: 42,
            context,
            kind: ResourceKind::Pods.into(),
            namespace: None,
            field_selector: None,
            label_selector: None,
        };
        let (sender, receiver) = watch::channel(ResourceSnapshot::loading(
            request.generation,
            request.kind.clone(),
        ));
        let started = Instant::now();
        cache.publish_current(&request, &sender);
        let elapsed = started.elapsed();
        let snapshot = receiver.borrow();
        eprintln!(
            "warm-cache rows={} elapsed={elapsed:?}",
            snapshot.rows.len()
        );
        assert!(snapshot.rows.len() >= 250);
        assert_eq!(snapshot.generation, request.generation);
        assert!(elapsed < Duration::from_secs(1));
    }

    #[tokio::test]
    #[ignore = "requires KUX_LIVE_CONTEXT and access to a Kubernetes cluster"]
    async fn live_manager_opens_prewarmed_pods_within_one_second() {
        let context = std::env::var("KUX_LIVE_CONTEXT").expect("KUX_LIVE_CONTEXT");
        let client = client_for_context(&context)
            .await
            .expect("Kubernetes client");
        let (requests, request_rx) = mpsc::channel(2);
        let kind: ResourceDescriptor = ResourceKind::Pods.into();
        let (snapshot_tx, mut snapshots) =
            watch::channel(ResourceSnapshot::loading(1, kind.clone()));
        let (_actions, action_rx) = mpsc::channel(1);
        let (action_results, _action_result_rx) = mpsc::channel(8);
        let manager = tokio::spawn(run_manager(
            client,
            KubernetesSettings {
                connection: ConnectionOptions::default(),
                initial_context: context.clone(),
                publish_interval: Duration::from_millis(100),
                refresh_interval: watch::channel(Duration::from_secs(5)).1,
                streaming_lists: true,
            },
            request_rx,
            snapshot_tx,
            action_rx,
            action_results,
        ));
        tokio::time::sleep(Duration::from_secs(5)).await;

        let request = WatchRequest {
            instance: None,
            generation: 77,
            context,
            kind,
            namespace: None,
            field_selector: None,
            label_selector: None,
        };
        let started = Instant::now();
        requests.send(request).await.expect("manager is active");
        let rows = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                snapshots
                    .changed()
                    .await
                    .expect("snapshot stream is active");
                let snapshot = snapshots.borrow_and_update();
                if snapshot.generation == 77 && snapshot.rows.len() >= 250 {
                    break snapshot.rows.len();
                }
            }
        })
        .await
        .expect("prewarmed Pods appear within one second");
        let elapsed = started.elapsed();
        manager.abort();
        eprintln!("manager-cache rows={rows} elapsed={elapsed:?}");
    }

    #[tokio::test]
    #[ignore = "requires KUX_LIVE_CONTEXT and access to a Kubernetes cluster"]
    async fn live_dynamic_yaml_fetch_completes_without_blocking_the_async_worker() {
        let context = std::env::var("KUX_LIVE_CONTEXT").expect("KUX_LIVE_CONTEXT");
        let qualified_name = std::env::var("KUX_LIVE_RESOURCE")
            .unwrap_or_else(|_| "applications.argoproj.io".to_owned());
        let namespace = std::env::var("KUX_LIVE_NAMESPACE").unwrap_or_else(|_| "argo".to_owned());
        let name = std::env::var("KUX_LIVE_NAME")
            .unwrap_or_else(|_| "cloud-controlplane-resources".to_owned());
        let client = client_for_context(&context)
            .await
            .expect("Kubernetes client");
        let resources = discover_resources(client.clone())
            .await
            .expect("API discovery");
        let kind = resources
            .iter()
            .find(|resource| resource.qualified_name() == qualified_name)
            .cloned()
            .unwrap_or_else(|| panic!("missing {qualified_name}"));
        let target = ResourceTarget {
            kind,
            namespace: Some(namespace),
            name,
        };
        let started = Instant::now();
        let result = fetch_content(
            client,
            &context,
            &ConnectionOptions::default(),
            &target,
            ContentKind::Yaml,
        )
        .await
        .expect("YAML content");
        let elapsed = started.elapsed();
        let ActionResult::Content { body, .. } = result else {
            panic!("expected YAML content");
        };
        eprintln!("fetched {} YAML bytes in {elapsed:?}", body.len());
        assert!(body.contains("kind: Application"));
        assert!(elapsed < Duration::from_secs(30), "YAML took {elapsed:?}");
    }
}

#[cfg(test)]
#[path = "batch_navigation_integration_tests.rs"]
mod batch_navigation_integration_tests;
