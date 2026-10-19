//! Exact GET polling and the six-resource K9s workload collection.
use std::{
    collections::{BTreeMap, BTreeSet},
    future::Future,
    sync::Arc,
    time::{Duration, Instant},
};

use futures::StreamExt;
use k8s_openapi::api::{
    apps::v1::{DaemonSet, Deployment, ReplicaSet, StatefulSet},
    core::v1::{Node, Pod, Service},
    rbac::v1::{ClusterRoleBinding, RoleBinding},
};
use kube::{
    Api, Client, ResourceExt as _,
    api::{GetParams, ListParams},
    core::{DynamicObject, Request},
};
use tokio::sync::watch;

use crate::model::{
    ResourceDescriptor, ResourceKind, ResourceRow, ResourceSnapshot, SourceState, SubjectKind,
    TableCell, TableColumn,
};

use super::{
    KubernetesSettings, RefreshSchedule, ServerTable, TABLE_MAX_PAGE_SIZE, WatchRequest,
    api_service_row, daemon_set_row, decode_server_table, deployment_row, dynamic_api, dynamic_row,
    dynamic_target_api, node_row, pod_row, replica_set_row, resource_identity,
    server_field_selector, service_row, stateful_set_row, table_request,
};

const MAX_CONTINUATION_BYTES: usize = 16 * 1024;

#[derive(Clone, Copy)]
enum View {
    Instance,
    Workloads,
    Subjects(crate::model::SubjectKind),
    Helm,
    HelmHistory,
    Dir,
    Benchmarks,
    XRay,
    Scans,
    Skins,
}

#[derive(Debug, Default)]
pub(super) struct ViewRows {
    pub columns: Arc<[TableColumn]>,
    pub rows: Vec<Arc<ResourceRow>>,
}

pub(super) async fn run_instance(
    client: Client,
    settings: KubernetesSettings,
    request: WatchRequest,
    snapshots: watch::Sender<ResourceSnapshot>,
) {
    run(client, settings, request, snapshots, View::Instance).await;
}

pub(super) async fn run_workloads(
    client: Client,
    settings: KubernetesSettings,
    request: WatchRequest,
    snapshots: watch::Sender<ResourceSnapshot>,
) {
    run(client, settings, request, snapshots, View::Workloads).await;
}

pub(super) async fn run_subjects(
    client: Client,
    settings: KubernetesSettings,
    request: WatchRequest,
    snapshots: watch::Sender<ResourceSnapshot>,
    kind: crate::model::SubjectKind,
) {
    run(client, settings, request, snapshots, View::Subjects(kind)).await;
}

pub(super) async fn run_helm(
    client: Client,
    settings: KubernetesSettings,
    request: WatchRequest,
    snapshots: watch::Sender<ResourceSnapshot>,
) {
    run(client, settings, request, snapshots, View::Helm).await;
}

pub(super) async fn run_helm_history(
    client: Client,
    settings: KubernetesSettings,
    request: WatchRequest,
    snapshots: watch::Sender<ResourceSnapshot>,
) {
    run(client, settings, request, snapshots, View::HelmHistory).await;
}

pub(super) async fn run_dir(
    client: Client,
    settings: KubernetesSettings,
    request: WatchRequest,
    snapshots: watch::Sender<ResourceSnapshot>,
) {
    run(client, settings, request, snapshots, View::Dir).await;
}

pub(super) async fn run_benchmarks(
    client: Client,
    settings: KubernetesSettings,
    request: WatchRequest,
    snapshots: watch::Sender<ResourceSnapshot>,
) {
    run(client, settings, request, snapshots, View::Benchmarks).await;
}

pub(super) async fn run_xray(
    client: Client,
    settings: KubernetesSettings,
    request: WatchRequest,
    snapshots: watch::Sender<ResourceSnapshot>,
) {
    run(client, settings, request, snapshots, View::XRay).await;
}

pub(super) async fn run_scans(
    client: Client,
    settings: KubernetesSettings,
    request: WatchRequest,
    snapshots: watch::Sender<ResourceSnapshot>,
) {
    run(client, settings, request, snapshots, View::Scans).await;
}

pub(super) async fn run_skins(
    client: Client,
    settings: KubernetesSettings,
    request: WatchRequest,
    snapshots: watch::Sender<ResourceSnapshot>,
) {
    run(client, settings, request, snapshots, View::Skins).await;
}

pub(super) async fn run_pulses(
    client: Client,
    settings: KubernetesSettings,
    request: WatchRequest,
    snapshots: watch::Sender<ResourceSnapshot>,
) {
    let timeout = match settings.connection.api_call_timeout() {
        Ok(Some(timeout)) if tokio::time::Instant::now().checked_add(timeout).is_none() => {
            publish_view(
                &snapshots,
                &request,
                &ViewRows::default(),
                SourceState::Error("Request timeout is not representable".into()),
                1,
            );
            return;
        }
        Ok(timeout) => timeout,
        Err(error) => {
            publish_view(
                &snapshots,
                &request,
                &ViewRows::default(),
                SourceState::Error(settings.connection.redact(&error).into()),
                1,
            );
            return;
        }
    };
    let mut cadence = RefreshSchedule::new(settings.refresh_interval);
    let mut state = PulseHistoryState::default();
    let mut revision = 0_u64;
    loop {
        tokio::select! {
            () = cadence.tick() => {}
            () = snapshots.closed() => return,
        }
        let read = load_pulses(&client, &request, &mut state, timeout);
        let result = tokio::select! {
            result = read => result,
            () = snapshots.closed() => return,
        };
        revision = revision.wrapping_add(1);
        match result {
            Ok(rows) => {
                publish_view(&snapshots, &request, &rows, SourceState::Ready, revision);
            }
            Err(error) => {
                publish_view(
                    &snapshots,
                    &request,
                    &ViewRows::default(),
                    SourceState::Error(settings.connection.redact(&error).into()),
                    revision,
                );
            }
        }
    }
}

async fn run(
    client: Client,
    settings: KubernetesSettings,
    request: WatchRequest,
    snapshots: watch::Sender<ResourceSnapshot>,
    view: View,
) {
    let timeout = match settings.connection.api_call_timeout() {
        Ok(Some(timeout)) if tokio::time::Instant::now().checked_add(timeout).is_none() => {
            publish_view(
                &snapshots,
                &request,
                &ViewRows::default(),
                SourceState::Error("Request timeout is not representable".into()),
                1,
            );
            return;
        }
        Ok(timeout) => timeout,
        Err(error) => {
            publish_view(
                &snapshots,
                &request,
                &ViewRows::default(),
                SourceState::Error(settings.connection.redact(&error).into()),
                1,
            );
            return;
        }
    };
    let mut cadence = RefreshSchedule::new(settings.refresh_interval);
    let mut complete = ViewRows::default();
    let mut revision = 0_u64;
    loop {
        tokio::select! {
            () = cadence.tick() => {}
            () = snapshots.closed() => return,
        }
        let read = async {
            match view {
                View::Instance => load_instance(&client, &request, timeout).await,
                View::Workloads => load_workloads(&client, &request, timeout).await,
                View::Subjects(kind) => load_subjects(&client, &request, kind, timeout).await,
                View::Helm => load_helm(&settings.connection, &request, timeout).await,
                View::HelmHistory => {
                    load_helm_history(&settings.connection, &request, timeout).await
                }
                View::Dir => load_dir(&request).await,
                View::Benchmarks => load_benchmarks(&request).await,
                View::XRay => super::xray::load_xray(&client, &request, timeout).await,
                View::Scans => load_scans(&request).await,
                View::Skins => load_skins().await,
            }
        };
        let result = tokio::select! {
            result = read => result,
            () = snapshots.closed() => return,
        };
        revision = revision.wrapping_add(1);
        let state = match result {
            Ok(rows) => {
                complete = rows;
                SourceState::Ready
            }
            Err(error) => {
                if matches!(view, View::Instance) {
                    complete = ViewRows::default();
                }
                SourceState::Error(settings.connection.redact(&error).into())
            }
        };
        publish_view(&snapshots, &request, &complete, state, revision);
    }
}

fn publish_view(
    snapshots: &watch::Sender<ResourceSnapshot>,
    request: &WatchRequest,
    rows: &ViewRows,
    state: SourceState,
    revision: u64,
) {
    let _ = snapshots.send(ResourceSnapshot {
        generation: request.generation,
        kind: request.kind.clone(),
        rows: Arc::new(rows.rows.clone()),
        table_columns: rows.columns.clone(),
        state,
        revision,
        emitted_at: Instant::now(),
    });
}

pub(super) async fn read_api<T>(
    future: impl Future<Output = Result<T, kube::Error>>,
    timeout: Option<Duration>,
    operation: &str,
) -> Result<T, String> {
    match timeout {
        Some(timeout) => tokio::time::timeout(timeout, future)
            .await
            .map_err(|_| format!("Timed out trying to {operation}"))?,
        None => future.await,
    }
    .map_err(|error| format!("Unable to {operation}: {error}"))
}

async fn load_instance(
    client: &Client,
    request: &WatchRequest,
    timeout: Option<Duration>,
) -> Result<ViewRows, String> {
    let target = request
        .instance
        .as_ref()
        .ok_or("An instance view requires an exact target")?;
    if target.kind.is_workloads()
        || !target.kind.operations.gettable()
        || target.name.trim().is_empty()
        || target.kind.gvr() != request.kind.gvr()
        || target.kind.kind != request.kind.kind
        || (target.kind.is_namespaced()
            && target
                .namespace
                .as_deref()
                .is_none_or(|namespace| namespace.trim().is_empty()))
        || (!target.kind.is_namespaced() && target.namespace.is_some())
    {
        return Err(
            "An instance view requires a readable resource and its exact name and namespace"
                .to_owned(),
        );
    }
    let api = dynamic_target_api(client.clone(), target)?;
    let operation = format!(
        "get {} {}",
        target.kind.qualified_name(),
        resource_identity(target)
    );
    if target.kind.known_kind().is_some()
        || target.kind.is_apps_v1_replica_set()
        || target.kind.is_api_service()
    {
        let object = read_api(api.get(&target.name), timeout, &operation).await?;
        validate_object(
            &object,
            &target.kind,
            target.namespace.as_deref(),
            Some(&target.name),
            false,
        )?;
        let row = instance_row(object, &target.kind)?;
        return Ok(ViewRows {
            columns: super::typed_table_columns(&target.kind),
            rows: vec![Arc::new(row)],
        });
    }
    let raw = Request::new(api.resource_url())
        .get(&target.name, &GetParams::default())
        .map_err(|error| format!("Unable to build instance request: {error}"))?;
    let value = read_api(
        client.request::<serde_json::Value>(table_request(raw)?),
        timeout,
        &operation,
    )
    .await?;
    if value.get("kind").and_then(serde_json::Value::as_str) != Some("Table") {
        let object: DynamicObject =
            serde_json::from_value(value).map_err(|error| error.to_string())?;
        validate_object(
            &object,
            &target.kind,
            target.namespace.as_deref(),
            Some(&target.name),
            false,
        )?;
        return Ok(ViewRows {
            columns: Arc::from([]),
            rows: vec![Arc::new(dynamic_row(&object))],
        });
    }
    let table = parse_table(value)?;
    if table.rows.len() != 1 {
        return Err("The instance Table must contain exactly the selected object".to_owned());
    }
    validate_table_rows(
        &table,
        &target.kind,
        target.namespace.as_deref(),
        Some(&target.name),
    )?;
    let page = decode_server_table(table, &[]);
    Ok(ViewRows {
        columns: page.columns,
        rows: page.rows,
    })
}

fn instance_row(object: DynamicObject, kind: &ResourceDescriptor) -> Result<ResourceRow, String> {
    let invalid = |error| format!("Invalid {} instance: {error}", kind.kind);
    Ok(match kind.known_kind() {
        Some(ResourceKind::Pods) => pod_row(&object.try_parse::<Pod>().map_err(invalid)?),
        Some(ResourceKind::Deployments) => {
            deployment_row(&object.try_parse::<Deployment>().map_err(invalid)?)
        }
        Some(ResourceKind::StatefulSets) => {
            stateful_set_row(&object.try_parse::<StatefulSet>().map_err(invalid)?)
        }
        Some(ResourceKind::DaemonSets) => {
            daemon_set_row(&object.try_parse::<DaemonSet>().map_err(invalid)?)
        }
        Some(ResourceKind::Services) => {
            service_row(&object.try_parse::<Service>().map_err(invalid)?)
        }
        Some(ResourceKind::Nodes) => node_row(&object.try_parse::<Node>().map_err(invalid)?),
        None if kind.is_apps_v1_replica_set() => {
            replica_set_row(&object.try_parse::<ReplicaSet>().map_err(invalid)?)
        }
        None if kind.is_api_service() => api_service_row(&object),
        None => dynamic_row(&object),
    })
}

fn validate_object(
    object: &DynamicObject,
    kind: &ResourceDescriptor,
    namespace: Option<&str>,
    name: Option<&str>,
    table_member: bool,
) -> Result<(), String> {
    let types_match = object.types.as_ref().map_or(table_member, |types| {
        (types.api_version == kind.api_version.as_ref() && types.kind == kind.kind.as_ref())
            || (table_member
                && types.kind == "PartialObjectMetadata"
                && matches!(
                    types.api_version.as_str(),
                    "meta.k8s.io/v1" | "meta.k8s.io/v1beta1"
                ))
    });
    let namespace_matches = if kind.is_namespaced() {
        object.metadata.namespace.as_deref().is_some_and(|actual| {
            !actual.is_empty() && namespace.is_none_or(|expected| actual == expected)
        })
    } else {
        object
            .metadata
            .namespace
            .as_deref()
            .is_none_or(str::is_empty)
    };
    if !types_match
        || !namespace_matches
        || object.metadata.name.as_deref().is_none_or(str::is_empty)
        || name.is_some_and(|expected| object.metadata.name.as_deref() != Some(expected))
    {
        return Err(format!(
            "The returned {} does not match the selected resource scope",
            kind.kind
        ));
    }
    Ok(())
}

fn parse_table(value: serde_json::Value) -> Result<ServerTable, String> {
    if value.get("kind").and_then(serde_json::Value::as_str) != Some("Table")
        || !matches!(
            value.get("apiVersion").and_then(serde_json::Value::as_str),
            Some("meta.k8s.io/v1" | "meta.k8s.io/v1beta1")
        )
    {
        return Err("The resource endpoint did not return a Kubernetes Table".to_owned());
    }
    serde_json::from_value(value).map_err(|error| format!("Invalid Kubernetes Table: {error}"))
}

fn validate_table_rows(
    table: &ServerTable,
    kind: &ResourceDescriptor,
    namespace: Option<&str>,
    name: Option<&str>,
) -> Result<(), String> {
    for row in &table.rows {
        let object: DynamicObject = serde_json::from_value(
            row.object
                .clone()
                .ok_or("The resource Table is missing object metadata")?,
        )
        .map_err(|error| format!("Invalid Table object metadata: {error}"))?;
        validate_object(&object, kind, namespace, name, true)?;
        if let Some(index) = table
            .column_definitions
            .iter()
            .position(|column| column.name.eq_ignore_ascii_case("name"))
            && row.cells.get(index).and_then(serde_json::Value::as_str)
                != object.metadata.name.as_deref()
        {
            return Err("The Table Name cell does not match its object metadata".to_owned());
        }
    }
    Ok(())
}

fn workload_kinds() -> [ResourceDescriptor; 6] {
    [
        ResourceKind::Pods.into(),
        ResourceKind::Services.into(),
        ResourceKind::DaemonSets.into(),
        ResourceKind::StatefulSets.into(),
        ResourceKind::Deployments.into(),
        ResourceDescriptor::new(
            "apps",
            "v1",
            "apps/v1",
            "ReplicaSet",
            "replicasets",
            true,
            crate::model::ResourceOperations::all(),
        ),
    ]
}

async fn load_workloads(
    client: &Client,
    request: &WatchRequest,
    timeout: Option<Duration>,
) -> Result<ViewRows, String> {
    if !request.kind.is_workloads()
        || request.instance.is_some()
        || request
            .namespace
            .as_deref()
            .is_some_and(|namespace| namespace.trim().is_empty())
    {
        return Err(
            "The aggregate workload view requires its workload collection scope".to_owned(),
        );
    }
    let mut reads = futures::stream::FuturesUnordered::new();
    for kind in workload_kinds() {
        reads.push(load_workload_kind(client, request, kind, timeout));
    }
    let mut rows = BTreeMap::new();
    while let Some(result) = reads.next().await {
        for row in result? {
            rows.insert(row.key.clone(), row);
        }
    }
    Ok(ViewRows {
        columns: workload_columns(),
        rows: rows.into_values().collect(),
    })
}

async fn load_workload_kind(
    client: &Client,
    request: &WatchRequest,
    kind: ResourceDescriptor,
    timeout: Option<Duration>,
) -> Result<Vec<Arc<ResourceRow>>, String> {
    let api = dynamic_api(client.clone(), &kind, request.namespace.as_deref());
    let mut continuation = String::new();
    let mut seen_tokens = BTreeSet::new();
    let mut columns: Arc<[TableColumn]> = Arc::from([]);
    let mut rows = Vec::new();
    loop {
        let mut params = ListParams::default()
            .limit(TABLE_MAX_PAGE_SIZE)
            .continue_token(&continuation);
        if let Some(selector) = request.label_selector.as_deref() {
            params = params.labels(selector);
        }
        if let Some(selector) = server_field_selector(request) {
            params = params.fields(selector);
        }
        let raw = Request::new(api.resource_url())
            .list(&params)
            .map_err(|error| error.to_string())?;
        let value = read_api(
            client.request::<serde_json::Value>(table_request(raw)?),
            timeout,
            &format!("list {} for workloads", kind.qualified_name()),
        )
        .await?;
        let table = parse_table(value)?;
        validate_table_rows(&table, &kind, request.namespace.as_deref(), None)?;
        let page = decode_server_table(table, &columns);
        if !columns.is_empty() && page.columns != columns {
            return Err(format!(
                "{} Table columns changed during pagination",
                kind.kind
            ));
        }
        columns = page.columns;
        for row in page.rows {
            rows.push(Arc::new(project_workload_row(&kind, &row, &columns)?));
        }
        continuation = page.continue_token.unwrap_or_default();
        if continuation.is_empty() {
            return Ok(rows);
        }
        if continuation.len() > MAX_CONTINUATION_BYTES || !seen_tokens.insert(continuation.clone())
        {
            return Err(format!(
                "{} pagination returned an invalid or repeated continuation token",
                kind.kind
            ));
        }
    }
}

pub(crate) fn workload_columns() -> Arc<[TableColumn]> {
    [
        "KIND",
        "NAMESPACE",
        "NAME",
        "STATUS",
        "READY",
        "VALID",
        "AGE",
    ]
    .into_iter()
    .map(|name| TableColumn {
        name: name.into(),
        format: if name == "AGE" { "date" } else { "" }.into(),
        priority: i32::from(name == "VALID"),
    })
    .collect::<Vec<_>>()
    .into()
}

fn workload_cell<'a>(
    row: &'a ResourceRow,
    columns: &[TableColumn],
    name: &str,
) -> Result<&'a str, String> {
    columns
        .iter()
        .position(|column| column.name.eq_ignore_ascii_case(name))
        .and_then(|index| row.table_cells.get(index))
        .map(|cell| cell.text.as_ref())
        .ok_or_else(|| format!("The workload Table is missing its {name} column"))
}

fn is_ready(ready: &str) -> bool {
    let Some((actual, desired)) = ready.split_once('/') else {
        return false;
    };
    match (actual.parse::<i64>(), desired.parse::<i64>()) {
        (Ok(actual), Ok(desired)) => desired == 0 || actual == desired,
        _ => false,
    }
}

pub(crate) fn project_workload_row(
    kind: &ResourceDescriptor,
    row: &ResourceRow,
    columns: &[TableColumn],
) -> Result<ResourceRow, String> {
    let (ready, degraded) = match kind.known_kind() {
        Some(ResourceKind::Services) => (String::new(), false),
        Some(ResourceKind::Pods) => {
            let ready = workload_cell(row, columns, "Ready")?;
            let status = workload_cell(row, columns, "Status")?;
            (
                ready.to_owned(),
                status != "Completed" && (!is_ready(ready) || status != "Running"),
            )
        }
        Some(ResourceKind::Deployments | ResourceKind::StatefulSets) => {
            let ready = workload_cell(row, columns, "Ready")?;
            (ready.to_owned(), !is_ready(ready))
        }
        Some(ResourceKind::DaemonSets) | None
            if kind.is_apps_v1_replica_set()
                || kind.known_kind() == Some(ResourceKind::DaemonSets) =>
        {
            let ready = format!(
                "{}/{}",
                workload_cell(row, columns, "Ready")?,
                workload_cell(row, columns, "Desired")?
            );
            let degraded = !is_ready(&ready);
            (ready, degraded)
        }
        _ => {
            return Err(format!(
                "{} is not a supported aggregate workload",
                kind.kind
            ));
        }
    };
    let status = if degraded { "DEGRADED" } else { "OK" };
    let valid = if degraded { "DEGRADED" } else { "" };
    let gvr = kind.gvr();
    let cells = [
        gvr.as_str(),
        row.namespace.as_ref(),
        row.name.as_ref(),
        status,
        ready.as_str(),
        valid,
        "",
    ]
    .into_iter()
    .enumerate()
    .map(|(index, text)| TableCell {
        text: text.into(),
        timestamp_unix: (index == 6 && row.created_unix != 0).then_some(row.created_unix),
    })
    .collect();
    let mut result = ResourceRow::new(
        row.namespace.to_string(),
        row.name.to_string(),
        status,
        ready,
        gvr,
        row.created_unix,
    )
    .with_labels(row.labels.to_string())
    .with_workload_kind(kind.clone())
    .with_table_cells(cells);
    result.owner.clone_from(&row.owner);
    result.owner_names.clone_from(&row.owner_names);
    Ok(result)
}

pub(crate) fn subject_columns() -> Arc<[TableColumn]> {
    ["NAME", "KIND", "FIRST LOCATION"]
        .into_iter()
        .map(|name| TableColumn {
            name: name.into(),
            format: "".into(),
            priority: 0,
        })
        .collect::<Vec<_>>()
        .into()
}

pub(crate) fn project_subject_row(name: String, kind: &str, location: String) -> ResourceRow {
    let cells = [name.as_str(), kind, location.as_str()]
        .into_iter()
        .map(|text| TableCell {
            text: text.into(),
            timestamp_unix: None,
        })
        .collect();
    let mut row =
        ResourceRow::new("-", name.clone(), kind, location, "", 0).with_table_cells(cells);
    row.key = Arc::from(name);
    row
}

async fn load_subjects(
    client: &Client,
    request: &WatchRequest,
    kind: SubjectKind,
    timeout: Option<Duration>,
) -> Result<ViewRows, String> {
    if request.kind.subject_kind() != Some(kind)
        || request.instance.is_some()
        || request.namespace.is_some()
    {
        return Err("The RBAC subject view requires cluster-wide subject scope".to_owned());
    }
    let subject_kind = kind.subject_kind();
    let (cluster_subjects, namespaced_subjects) = tokio::try_join!(
        collect_cluster_subjects(client, request, subject_kind, timeout),
        collect_namespaced_subjects(client, request, subject_kind, timeout),
    )?;
    let mut subjects = BTreeMap::new();
    for (name, (kind, location)) in cluster_subjects {
        subjects.entry(name).or_insert((kind, location));
    }
    for (name, (kind, location)) in namespaced_subjects {
        subjects.entry(name).or_insert((kind, location));
    }
    let rows = subjects
        .into_iter()
        .map(|(name, (kind, location))| Arc::new(project_subject_row(name, kind, location)))
        .collect();
    Ok(ViewRows {
        columns: subject_columns(),
        rows,
    })
}

async fn collect_cluster_subjects(
    client: &Client,
    request: &WatchRequest,
    subject_kind: &str,
    timeout: Option<Duration>,
) -> Result<BTreeMap<String, (&'static str, String)>, String> {
    let api = Api::<ClusterRoleBinding>::all(client.clone());
    let mut continuation = String::new();
    let mut seen_tokens = BTreeSet::new();
    let mut subjects = BTreeMap::new();
    loop {
        let mut params = ListParams::default()
            .limit(500)
            .continue_token(&continuation);
        if let Some(selector) = request.label_selector.as_deref() {
            params = params.labels(selector);
        }
        let page = read_api(
            api.list(&params),
            timeout,
            "list clusterrolebindings for subjects",
        )
        .await?;
        for binding in &page.items {
            let location = binding.name_any();
            for subject in binding.subjects.as_deref().unwrap_or_default() {
                if subject.kind == subject_kind && !subject.name.is_empty() {
                    subjects
                        .entry(subject.name.clone())
                        .or_insert(("ClusterRoleBinding", location.clone()));
                }
            }
        }
        continuation = page.metadata.continue_.unwrap_or_default();
        if continuation.is_empty() {
            return Ok(subjects);
        }
        if continuation.len() > MAX_CONTINUATION_BYTES || !seen_tokens.insert(continuation.clone())
        {
            return Err(
                "ClusterRoleBinding pagination returned an invalid or repeated continuation token"
                    .to_owned(),
            );
        }
    }
}

async fn collect_namespaced_subjects(
    client: &Client,
    request: &WatchRequest,
    subject_kind: &str,
    timeout: Option<Duration>,
) -> Result<BTreeMap<String, (&'static str, String)>, String> {
    let api = Api::<RoleBinding>::all(client.clone());
    let mut continuation = String::new();
    let mut seen_tokens = BTreeSet::new();
    let mut subjects = BTreeMap::new();
    loop {
        let mut params = ListParams::default()
            .limit(500)
            .continue_token(&continuation);
        if let Some(selector) = request.label_selector.as_deref() {
            params = params.labels(selector);
        }
        let page = read_api(api.list(&params), timeout, "list rolebindings for subjects").await?;
        for binding in &page.items {
            let namespace = binding.namespace().unwrap_or_default();
            let name = binding.name_any();
            let location = if namespace.is_empty() {
                name
            } else {
                format!("{namespace}/{name}")
            };
            for subject in binding.subjects.as_deref().unwrap_or_default() {
                if subject.kind == subject_kind && !subject.name.is_empty() {
                    subjects
                        .entry(subject.name.clone())
                        .or_insert(("RoleBinding", location.clone()));
                }
            }
        }
        continuation = page.metadata.continue_.unwrap_or_default();
        if continuation.is_empty() {
            return Ok(subjects);
        }
        if continuation.len() > MAX_CONTINUATION_BYTES || !seen_tokens.insert(continuation.clone())
        {
            return Err(
                "RoleBinding pagination returned an invalid or repeated continuation token"
                    .to_owned(),
            );
        }
    }
}

pub(crate) fn helm_columns() -> Arc<[TableColumn]> {
    [
        "NAMESPACE",
        "NAME",
        "REVISION",
        "STATUS",
        "CHART",
        "APP VERSION",
        "VALID",
        "AGE",
    ]
    .into_iter()
    .map(|name| TableColumn {
        name: name.into(),
        format: if name == "AGE" { "date" } else { "" }.into(),
        priority: i32::from(name == "VALID"),
    })
    .collect::<Vec<_>>()
    .into()
}

pub(crate) fn helm_history_columns() -> Arc<[TableColumn]> {
    [
        "REVISION",
        "STATUS",
        "CHART",
        "APP VERSION",
        "DESCRIPTION",
        "VALID",
    ]
    .into_iter()
    .map(|name| TableColumn {
        name: name.into(),
        format: "".into(),
        priority: i32::from(name == "VALID"),
    })
    .collect::<Vec<_>>()
    .into()
}

pub(crate) fn dir_columns() -> Arc<[TableColumn]> {
    ["NAME"]
        .into_iter()
        .map(|name| TableColumn {
            name: name.into(),
            format: "".into(),
            priority: 0,
        })
        .collect::<Vec<_>>()
        .into()
}

pub(crate) fn parse_helm_timestamp(updated: &str) -> i64 {
    let trimmed = updated.trim();
    if trimmed.is_empty() {
        return 0;
    }
    let tokens: Vec<_> = trimmed.split_whitespace().collect();
    if tokens.len() >= 3
        && tokens[0].len() == 10
        && (tokens[2].starts_with('+') || tokens[2].starts_with('-'))
    {
        let tz = tokens[2];
        let tz_iso = if tz.len() == 5 && !tz.contains(':') {
            format!("{}:{}", &tz[..3], &tz[3..])
        } else {
            tz.to_owned()
        };
        let iso = format!("{}T{}{}", tokens[0], tokens[1], tz_iso);
        if let Ok(ts) = iso.parse::<k8s_openapi::jiff::Timestamp>() {
            return ts.as_second();
        }
    }
    let candidate = tokens.first().copied().unwrap_or(trimmed);
    candidate
        .parse::<k8s_openapi::jiff::Timestamp>()
        .map_or(0, k8s_openapi::jiff::Timestamp::as_second)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn project_helm_row(
    namespace: String,
    name: String,
    revision: String,
    status: String,
    chart: String,
    app_version: &str,
    updated_unix: i64,
) -> ResourceRow {
    let valid = if status == "deployed" { "" } else { "DEGRADED" };
    let cells = [
        namespace.as_str(),
        name.as_str(),
        revision.as_str(),
        status.as_str(),
        chart.as_str(),
        app_version,
        valid,
        "",
    ]
    .into_iter()
    .enumerate()
    .map(|(index, text)| TableCell {
        text: text.into(),
        timestamp_unix: (index == 7 && updated_unix != 0).then_some(updated_unix),
    })
    .collect();
    ResourceRow::new(namespace, name, status, revision, chart, updated_unix).with_table_cells(cells)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn project_helm_history_row(
    namespace: String,
    release_name: &str,
    revision: String,
    status: String,
    chart: &str,
    app_version: &str,
    description: String,
    updated_unix: i64,
) -> ResourceRow {
    let valid = if matches!(status.as_str(), "deployed" | "superseded") {
        ""
    } else {
        "DEGRADED"
    };
    let cells = [
        revision.as_str(),
        status.as_str(),
        chart,
        app_version,
        description.as_str(),
        valid,
    ]
    .into_iter()
    .map(|text| TableCell {
        text: text.into(),
        timestamp_unix: None,
    })
    .collect();
    let row_name = format!("{release_name}:{revision}");
    ResourceRow::new(
        namespace,
        row_name,
        status,
        revision,
        description,
        updated_unix,
    )
    .with_table_cells(cells)
}

pub(crate) fn project_dir_row(path: String, file_name: &str, is_dir: bool) -> ResourceRow {
    let prefix = if is_dir { "📁 " } else { "🦄 " };
    let display = format!("{prefix}{file_name}");
    let cells = [TableCell {
        text: display.clone().into(),
        timestamp_unix: None,
    }]
    .into();
    let kind = if is_dir { "dir" } else { "manifest" };
    let mut row = ResourceRow::new("-", path.clone(), kind, "", display, 0).with_table_cells(cells);
    row.key = Arc::from(path);
    row
}

async fn run_helm_json(
    connection: &crate::connection::ConnectionOptions,
    context: &str,
    subcommand_args: &[&str],
    timeout: Option<Duration>,
) -> Result<serde_json::Value, String> {
    let mut args = connection.helm_args(Some(context));
    args.extend(subcommand_args.iter().map(std::ffi::OsString::from));
    let cmd = tokio::process::Command::new("helm")
        .args(args)
        .kill_on_drop(true)
        .output();
    let output = match timeout {
        Some(timeout) => tokio::time::timeout(timeout, cmd)
            .await
            .map_err(|_| "Timed out waiting for helm CLI".to_owned())?,
        None => cmd.await,
    }
    .map_err(|error| format!("Unable to execute helm CLI: {error}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return Err(if stderr.is_empty() {
            format!("helm exited with {}", output.status)
        } else {
            stderr
        });
    }
    serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("Invalid JSON returned by helm: {error}"))
}

async fn load_helm(
    connection: &crate::connection::ConnectionOptions,
    request: &WatchRequest,
    timeout: Option<Duration>,
) -> Result<ViewRows, String> {
    let mut args = vec!["list", "--all", "-o", "json"];
    if let Some(ns) = request.namespace.as_deref() {
        args.extend(["-n", ns]);
    } else {
        args.push("-A");
    }
    let value = run_helm_json(connection, &request.context, &args, timeout).await?;
    let items = value
        .as_array()
        .ok_or_else(|| "Expected JSON array from helm list".to_owned())?;
    let mut rows = Vec::with_capacity(items.len());
    for item in items {
        let name = item
            .get("name")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned();
        if name.is_empty() {
            continue;
        }
        let namespace = item
            .get("namespace")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("default")
            .to_owned();
        let revision = match item.get("revision") {
            Some(serde_json::Value::String(s)) => s.clone(),
            Some(serde_json::Value::Number(n)) => n.to_string(),
            _ => "1".to_owned(),
        };
        let status = item
            .get("status")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown")
            .to_owned();
        let chart = item
            .get("chart")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let app_version = item
            .get("app_version")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let updated = item
            .get("updated")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        rows.push(Arc::new(project_helm_row(
            namespace,
            name,
            revision,
            status,
            chart,
            &app_version,
            parse_helm_timestamp(updated),
        )));
    }
    Ok(ViewRows {
        columns: helm_columns(),
        rows,
    })
}

async fn load_helm_history(
    connection: &crate::connection::ConnectionOptions,
    request: &WatchRequest,
    timeout: Option<Duration>,
) -> Result<ViewRows, String> {
    let fqn = request
        .field_selector
        .as_deref()
        .ok_or_else(|| "Helm history requires a release namespace/name".to_owned())?;
    let (namespace, release) = fqn
        .split_once('/')
        .unwrap_or((request.namespace.as_deref().unwrap_or("default"), fqn));
    let args = ["history", release, "-n", namespace, "-o", "json"];
    let value = run_helm_json(connection, &request.context, &args, timeout).await?;
    let items = value
        .as_array()
        .ok_or_else(|| "Expected JSON array from helm history".to_owned())?;
    let mut rows = Vec::with_capacity(items.len());
    for item in items {
        let revision = match item.get("revision") {
            Some(serde_json::Value::Number(n)) => n.to_string(),
            Some(serde_json::Value::String(s)) => s.clone(),
            _ => continue,
        };
        let status = item
            .get("status")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown")
            .to_owned();
        let chart = item
            .get("chart")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let app_version = item
            .get("app_version")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let description = item
            .get("description")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let updated = item
            .get("updated")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        rows.push(Arc::new(project_helm_history_row(
            namespace.to_owned(),
            release,
            revision,
            status,
            &chart,
            &app_version,
            description,
            parse_helm_timestamp(updated),
        )));
    }
    Ok(ViewRows {
        columns: helm_history_columns(),
        rows,
    })
}

pub(crate) fn is_manifest_or_json_file(file_name: &str) -> bool {
    std::path::Path::new(file_name)
        .extension()
        .and_then(std::ffi::OsStr::to_str)
        .is_some_and(|ext| {
            ext.eq_ignore_ascii_case("yaml")
                || ext.eq_ignore_ascii_case("yml")
                || ext.eq_ignore_ascii_case("json")
        })
}

async fn load_dir(request: &WatchRequest) -> Result<ViewRows, String> {
    let dir = request
        .field_selector
        .clone()
        .unwrap_or_else(|| ".".to_owned());
    let rows = tokio::task::spawn_blocking(move || read_dir_rows(&dir))
        .await
        .map_err(|error| format!("Directory reader task failed: {error}"))??;
    Ok(ViewRows {
        columns: dir_columns(),
        rows,
    })
}

pub(crate) fn read_dir_rows(dir: &str) -> Result<Vec<Arc<ResourceRow>>, String> {
    let entries = std::fs::read_dir(dir)
        .map_err(|error| format!("Unable to read directory {dir}: {error}"))?;
    let mut collected = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| format!("Unable to read entry in {dir}: {error}"))?;
        let file_name = entry.file_name().to_string_lossy().into_owned();
        if file_name.starts_with('.') {
            continue;
        }
        let file_type = entry
            .file_type()
            .map_err(|error| format!("Unable to inspect {file_name}: {error}"))?;
        let is_dir = file_type.is_dir();
        if !is_dir && !is_manifest_or_json_file(&file_name) {
            continue;
        }
        let full_path = entry.path().to_string_lossy().into_owned();
        collected.push((file_name, full_path, is_dir));
    }
    collected.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    Ok(collected
        .into_iter()
        .map(|(file_name, full_path, is_dir)| {
            Arc::new(project_dir_row(full_path, &file_name, is_dir))
        })
        .collect())
}

pub(crate) fn benchmark_columns() -> Arc<[TableColumn]> {
    [
        "NAMESPACE",
        "NAME",
        "STATUS",
        "TIME",
        "REQ/S",
        "2XX",
        "4XX/5XX",
        "REPORT",
        "VALID",
        "AGE",
    ]
    .into_iter()
    .map(|name| TableColumn {
        name: name.into(),
        format: if name == "AGE" { "date" } else { "" }.into(),
        priority: i32::from(name == "VALID"),
    })
    .collect::<Vec<_>>()
    .into()
}

pub(crate) fn project_benchmark_row(
    full_path: String,
    file_name: &str,
    modified_unix: i64,
    report_data: &str,
) -> Option<ResourceRow> {
    let tokens: Vec<_> = file_name.split('_').collect();
    if tokens.len() < 3 {
        return None;
    }
    let namespace = tokens[0].to_owned();
    let target_name = format!("{}:{}", tokens[1], tokens[2]);
    let metrics = crate::benchmarks::parse_benchmark_report(report_data);
    let valid = if metrics.status == "pass" {
        ""
    } else {
        "DEGRADED"
    };
    let cells = [
        namespace.as_str(),
        target_name.as_str(),
        metrics.status.as_str(),
        metrics.total_time.as_str(),
        metrics.req_per_sec.as_str(),
        metrics.ok_2xx.as_str(),
        metrics.err_4xx_5xx.as_str(),
        file_name,
        valid,
        "",
    ]
    .into_iter()
    .enumerate()
    .map(|(index, text)| TableCell {
        text: text.into(),
        timestamp_unix: (index == 9 && modified_unix != 0).then_some(modified_unix),
    })
    .collect();
    let mut row = ResourceRow::new(
        namespace,
        full_path.clone(),
        metrics.status,
        metrics.req_per_sec,
        file_name,
        modified_unix,
    )
    .with_table_cells(cells);
    row.key = Arc::from(full_path);
    Some(row)
}

async fn load_benchmarks(request: &WatchRequest) -> Result<ViewRows, String> {
    let context = request.context.clone();
    let prefix_filter = request.field_selector.clone();
    let rows = tokio::task::spawn_blocking(move || {
        read_benchmark_rows(&context, prefix_filter.as_deref())
    })
    .await
    .map_err(|error| format!("Benchmark reader task failed: {error}"))??;
    Ok(ViewRows {
        columns: benchmark_columns(),
        rows,
    })
}

pub(crate) fn read_benchmark_rows(
    context: &str,
    prefix_filter: Option<&str>,
) -> Result<Vec<Arc<ResourceRow>>, String> {
    let dir = crate::benchmarks::benchmarks_dir(None, context)
        .map_err(|error| format!("Unable to resolve benchmark directory: {error}"))?;
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(format!(
                "Unable to read benchmark directory {}: {error}",
                dir.display()
            ));
        }
    };
    let match_prefix = prefix_filter
        .filter(|s| !s.is_empty())
        .map(crate::benchmarks::sanitize_bench_target);
    let mut rows = Vec::new();
    for entry in entries.flatten() {
        let file_name = entry.file_name().to_string_lossy().into_owned();
        if !std::path::Path::new(&file_name)
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("txt"))
        {
            continue;
        }
        if let Some(prefix) = &match_prefix
            && !file_name.starts_with(prefix)
        {
            continue;
        }
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }
        let modified_unix = metadata
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .and_then(|d| i64::try_from(d.as_secs()).ok())
            .unwrap_or(0);
        let full_path = entry.path().to_string_lossy().into_owned();
        let Ok(data) = std::fs::read_to_string(entry.path()) else {
            continue;
        };
        if let Some(row) = project_benchmark_row(full_path, &file_name, modified_unix, &data) {
            rows.push(Arc::new(row));
        }
    }
    rows.sort_unstable_by(|a, b| {
        b.created_unix
            .cmp(&a.created_unix)
            .then_with(|| a.name.cmp(&b.name))
    });
    Ok(rows)
}

async fn load_scans(request: &WatchRequest) -> Result<ViewRows, String> {
    let raw_images = request.field_selector.clone().unwrap_or_default();
    let rows = tokio::task::spawn_blocking(move || read_scan_rows(&raw_images))
        .await
        .map_err(|error| format!("Scan reader task failed: {error}"))?;
    Ok(ViewRows {
        columns: crate::scans::scan_columns(),
        rows,
    })
}

#[must_use]
pub(crate) fn read_scan_rows(raw_images: &str) -> Vec<Arc<ResourceRow>> {
    let images: Vec<_> = raw_images
        .split('\n')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    crate::scans::project_scan_rows(images)
}

#[must_use]
pub(crate) fn skin_columns() -> Arc<[TableColumn]> {
    ["NAME", "ORIGIN"]
        .into_iter()
        .map(|name| TableColumn {
            name: name.into(),
            format: "".into(),
            priority: 0,
        })
        .collect::<Vec<_>>()
        .into()
}

#[must_use]
pub(crate) fn read_skin_rows() -> Vec<Arc<ResourceRow>> {
    let paths = crate::config::ConfigPaths::from_env()
        .unwrap_or_else(|_| crate::config::ConfigPaths::new("/tmp/kux/config", "/tmp/kux/data"));
    crate::theme::Skin::names(&paths)
        .into_iter()
        .map(|name| {
            let origin = if name == "default" {
                "stock".to_owned()
            } else {
                crate::theme::Skin::candidate_paths(&paths, &name)
                    .ok()
                    .and_then(|candidates| candidates.into_iter().find(|p| p.is_file()))
                    .map_or_else(|| "custom".to_owned(), |p| p.display().to_string())
            };
            let cells = [name.as_str(), origin.as_str()]
                .into_iter()
                .map(|text| TableCell {
                    text: text.into(),
                    timestamp_unix: None,
                })
                .collect();
            let mut row =
                ResourceRow::new("-", name.clone(), "ok", "", origin, 0).with_table_cells(cells);
            row.key = Arc::from(name);
            Arc::new(row)
        })
        .collect()
}

async fn load_skins() -> Result<ViewRows, String> {
    let rows = tokio::task::spawn_blocking(read_skin_rows)
        .await
        .map_err(|error| format!("Skin reader task failed: {error}"))?;
    Ok(ViewRows {
        columns: skin_columns(),
        rows,
    })
}

#[derive(Default)]
struct PulseHistoryState {
    prev_counts: BTreeMap<String, (u64, u64)>,
    cpu_series: Vec<u64>,
    mem_series: Vec<u64>,
}

#[must_use]
pub(crate) fn pulse_columns() -> Arc<[TableColumn]> {
    ["NAME", "OK", "FAULTS", "STATUS"]
        .into_iter()
        .map(|name| TableColumn {
            name: name.into(),
            format: "".into(),
            priority: 0,
        })
        .collect::<Vec<_>>()
        .into()
}

fn pulse_desc(
    group: &'static str,
    kind: &'static str,
    plural: &'static str,
    namespaced: bool,
    short: &'static [&'static str],
) -> ResourceDescriptor {
    let version = if group == "autoscaling" { "v2" } else { "v1" };
    let api_version = match group {
        "batch" => "batch/v1",
        "autoscaling" => "autoscaling/v2",
        "networking.k8s.io" => "networking.k8s.io/v1",
        _ => "v1",
    };
    ResourceDescriptor::new(
        group,
        version,
        api_version,
        kind,
        plural,
        namespaced,
        crate::model::ResourceOperations::all(),
    )
    .with_short_names(short.iter().copied())
}

#[must_use]
pub(crate) fn pulse_gauge_specs(all_namespaces: bool) -> Vec<(ResourceDescriptor, &'static str)> {
    let mut specs = Vec::new();
    if all_namespaces {
        specs.extend([
            (ResourceDescriptor::from(ResourceKind::Nodes), "Nodes"),
            (
                pulse_desc("", "Namespace", "namespaces", false, &["ns"]),
                "Namespaces",
            ),
            (ResourceDescriptor::from(ResourceKind::Services), "Services"),
            (pulse_desc("", "Event", "events", true, &["ev"]), "Events"),
        ]);
    }
    specs.extend([
        (ResourceDescriptor::from(ResourceKind::Pods), "Pods"),
        (
            ResourceDescriptor::from(ResourceKind::Deployments),
            "Deployments",
        ),
        (
            ResourceDescriptor::from(ResourceKind::StatefulSets),
            "Statefulsets",
        ),
        (
            ResourceDescriptor::from(ResourceKind::DaemonSets),
            "Daemonsets",
        ),
        (pulse_desc("batch", "Job", "jobs", true, &[]), "Jobs"),
        (
            pulse_desc("batch", "CronJob", "cronjobs", true, &["cj"]),
            "Cronjobs",
        ),
        (
            pulse_desc("", "PersistentVolume", "persistentvolumes", false, &["pv"]),
            "Persistentvolumes",
        ),
        (
            pulse_desc(
                "",
                "PersistentVolumeClaim",
                "persistentvolumeclaims",
                true,
                &["pvc"],
            ),
            "Persistentvolumeclaims",
        ),
        (
            pulse_desc(
                "autoscaling",
                "HorizontalPodAutoscaler",
                "horizontalpodautoscalers",
                true,
                &["hpa"],
            ),
            "Horizontalpodautoscalers",
        ),
        (
            pulse_desc("networking.k8s.io", "Ingress", "ingresses", true, &["ing"]),
            "Ingresses",
        ),
        (
            pulse_desc(
                "networking.k8s.io",
                "NetworkPolicy",
                "networkpolicies",
                true,
                &["netpol", "np"],
            ),
            "Networkpolicies",
        ),
        (
            pulse_desc("", "ServiceAccount", "serviceaccounts", true, &["sa"]),
            "Serviceaccounts",
        ),
    ]);
    specs
}

#[must_use]
#[allow(clippy::too_many_arguments)]
pub(crate) fn project_pulse_row(
    namespace: &str,
    gvr: &str,
    title: &str,
    target_kind: ResourceDescriptor,
    is_sparkline: bool,
    total: u64,
    faults: u64,
    delta_total: i8,
    delta_faults: i8,
    current: u64,
    capacity: u64,
    series: Arc<[u64]>,
) -> ResourceRow {
    let status = if is_sparkline {
        "METRIC"
    } else if faults > 0 {
        "DEGRADED"
    } else {
        "OK"
    };
    let ok_str = if is_sparkline {
        format!("{current}/{capacity}")
    } else {
        total.to_string()
    };
    let fault_str = if is_sparkline {
        String::new()
    } else {
        faults.to_string()
    };
    let cells = [title, ok_str.as_str(), fault_str.as_str(), status]
        .into_iter()
        .map(|text| TableCell {
            text: text.into(),
            timestamp_unix: None,
        })
        .collect();
    ResourceRow::new(namespace, title, status, ok_str, fault_str, 0)
        .with_table_cells(cells)
        .with_pulse_data(crate::model::PulseData {
            gvr: Arc::from(gvr),
            title: Arc::from(title),
            target_kind,
            is_sparkline,
            total,
            faults,
            delta_total,
            delta_faults,
            current,
            capacity,
            series,
        })
}

async fn load_pulses(
    client: &Client,
    request: &WatchRequest,
    state: &mut PulseHistoryState,
    timeout: Option<Duration>,
) -> Result<ViewRows, String> {
    let all_namespaces = request.namespace.is_none();
    let ns_label = request.namespace.as_deref().unwrap_or("-");
    let specs = pulse_gauge_specs(all_namespaces);
    let mut reads = futures::stream::FuturesUnordered::new();
    for (index, (kind, title)) in specs.into_iter().enumerate() {
        let client_clone = client.clone();
        let ns = request.namespace.clone();
        reads.push(async move {
            let (total, faults) =
                check_pulse_kind(&client_clone, &kind, ns.as_deref(), timeout).await;
            (index, kind, title, total, faults)
        });
    }
    let mut results = Vec::new();
    while let Some(item) = reads.next().await {
        results.push(item);
    }
    results.sort_unstable_by_key(|item| item.0);

    let mut rows = Vec::with_capacity(results.len() + 2);
    for (_, kind, title, total, faults) in results {
        let gvr = kind.gvr();
        let (delta_total, delta_faults) =
            if let Some(&(prev_total, prev_faults)) = state.prev_counts.get(&gvr) {
                (
                    match total.cmp(&prev_total) {
                        std::cmp::Ordering::Greater => 1,
                        std::cmp::Ordering::Less => -1,
                        std::cmp::Ordering::Equal => 0,
                    },
                    match faults.cmp(&prev_faults) {
                        std::cmp::Ordering::Greater => 1,
                        std::cmp::Ordering::Less => -1,
                        std::cmp::Ordering::Equal => 0,
                    },
                )
            } else {
                (0, 0)
            };
        state.prev_counts.insert(gvr.clone(), (total, faults));
        rows.push(Arc::new(project_pulse_row(
            ns_label,
            &gvr,
            title,
            kind,
            false,
            total,
            faults,
            delta_total,
            delta_faults,
            0,
            0,
            Arc::from([]),
        )));
    }

    let (curr_cpu, alloc_cpu, curr_mem_mib, alloc_mem_mib) =
        load_pulse_cluster_metrics(client, request.namespace.as_deref(), timeout).await;
    state.cpu_series.push(curr_cpu);
    if state.cpu_series.len() > 120 {
        state.cpu_series.remove(0);
    }
    state.mem_series.push(curr_mem_mib);
    if state.mem_series.len() > 120 {
        state.mem_series.remove(0);
    }

    let pod_kind = ResourceDescriptor::from(ResourceKind::Pods);
    rows.push(Arc::new(project_pulse_row(
        ns_label,
        "cpu",
        "Cpu",
        pod_kind.clone(),
        true,
        0,
        0,
        0,
        0,
        curr_cpu,
        alloc_cpu,
        Arc::from(state.cpu_series.as_slice()),
    )));
    rows.push(Arc::new(project_pulse_row(
        ns_label,
        "memory",
        "Memory",
        pod_kind,
        true,
        0,
        0,
        0,
        0,
        curr_mem_mib,
        alloc_mem_mib,
        Arc::from(state.mem_series.as_slice()),
    )));

    Ok(ViewRows {
        columns: pulse_columns(),
        rows,
    })
}

async fn check_pulse_kind(
    client: &Client,
    kind: &ResourceDescriptor,
    namespace: Option<&str>,
    timeout: Option<Duration>,
) -> (u64, u64) {
    let effective_ns = kind.is_namespaced().then_some(namespace).flatten();
    let api = dynamic_api(client.clone(), kind, effective_ns);
    let params = ListParams::default().limit(TABLE_MAX_PAGE_SIZE);
    let Ok(raw) = Request::new(api.resource_url()).list(&params) else {
        return (0, 0);
    };
    let Ok(req) = table_request(raw) else {
        return (0, 0);
    };
    let Ok(value) = read_api(
        client.request::<serde_json::Value>(req),
        timeout,
        &format!("check pulse for {}", kind.qualified_name()),
    )
    .await
    else {
        return (0, 0);
    };
    let Ok(table) = parse_table(value) else {
        return (0, 0);
    };
    let page = decode_server_table(table, &[]);
    let total = u64::try_from(page.rows.len()).unwrap_or(u64::MAX);
    let mut faults = 0_u64;
    for row in &page.rows {
        if pulse_table_row_is_fault(kind, row, &page.columns) {
            faults = faults.saturating_add(1);
        }
    }
    (total, faults)
}

#[must_use]
pub(crate) fn pulse_table_row_is_fault(
    kind: &ResourceDescriptor,
    row: &ResourceRow,
    columns: &[TableColumn],
) -> bool {
    let cell = |name: &str| {
        columns
            .iter()
            .position(|col| col.name.eq_ignore_ascii_case(name))
            .and_then(|idx| row.table_cells.get(idx))
            .map_or("", |c| c.text.as_ref())
    };
    match (kind.group.as_ref(), kind.kind.as_ref()) {
        ("", "Node") => {
            let status = cell("Status");
            !status.is_empty() && !status.starts_with("Ready")
        }
        ("", "Namespace") => {
            let status = cell("Status");
            !status.is_empty() && status != "Active"
        }
        ("", "Event") => {
            let ev_type = cell("Type");
            !ev_type.is_empty() && ev_type != "Normal"
        }
        ("", "Pod") => {
            let status = cell("Status");
            let ready = cell("Ready");
            !status.is_empty() && status != "Completed" && (!is_ready(ready) || status != "Running")
        }
        ("apps", "Deployment" | "StatefulSet") => {
            let ready = cell("Ready");
            !ready.is_empty() && !is_ready(ready)
        }
        ("apps", "DaemonSet") => {
            let ready = cell("Ready");
            let desired = cell("Desired");
            if !ready.is_empty() && !desired.is_empty() {
                !is_ready(&format!("{ready}/{desired}"))
            } else {
                !row.ready.is_empty() && row.ready.as_ref() != "-" && !is_ready(&row.ready)
            }
        }
        ("batch", "Job") => {
            let status = cell("Status");
            let completions = cell("Completions");
            status == "Failed" || (!completions.is_empty() && !is_ready(completions))
        }
        ("", "PersistentVolume" | "PersistentVolumeClaim") => {
            let status = cell("Status");
            !status.is_empty() && !matches!(status, "Bound" | "Available")
        }
        _ => false,
    }
}

async fn load_pulse_cluster_metrics(
    client: &Client,
    namespace: Option<&str>,
    timeout: Option<Duration>,
) -> (u64, u64, u64, u64) {
    let node_api = Api::<Node>::all(client.clone());
    let list_params = ListParams::default();
    let mut alloc_cpu = 0_u64;
    let mut alloc_mem_bytes = 0_u64;
    if let Ok(nodes) = read_api(
        node_api.list(&list_params),
        timeout,
        "list nodes for pulse capacity",
    )
    .await
    {
        for node in &nodes.items {
            let row = node_row(node);
            if let Some(nd) = row.node_data() {
                alloc_cpu = alloc_cpu.saturating_add(nd.cpu_allocatable_millis);
                alloc_mem_bytes = alloc_mem_bytes.saturating_add(nd.memory_allocatable_bytes);
            }
        }
    }
    let mut curr_cpu = 0_u64;
    let mut curr_mem_bytes = 0_u64;
    if namespace.is_some() {
        if let Ok(pod_metrics) = super::fetch_pod_metrics(client.clone(), namespace).await {
            for usage in pod_metrics.values() {
                curr_cpu = curr_cpu.saturating_add(usage.cpu_millis.unwrap_or(0));
                curr_mem_bytes = curr_mem_bytes.saturating_add(usage.memory_bytes.unwrap_or(0));
            }
        }
    } else if let Ok(node_metrics) = super::fetch_node_metrics(client.clone()).await {
        for usage in node_metrics.values() {
            curr_cpu = curr_cpu.saturating_add(usage.cpu_millis.unwrap_or(0));
            curr_mem_bytes = curr_mem_bytes.saturating_add(usage.memory_bytes.unwrap_or(0));
        }
    }
    (
        curr_cpu,
        alloc_cpu,
        curr_mem_bytes / 1_048_576,
        alloc_mem_bytes / 1_048_576,
    )
}

#[cfg(test)]
#[path = "resource_views_tests.rs"]
mod tests;
