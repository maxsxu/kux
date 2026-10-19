use std::{collections::BTreeSet, future::Future, time::Duration};

use k8s_openapi::api::core::v1::PodSpec;
use kube::{
    Api, Client, ResourceExt,
    api::ListParams,
    core::{ApiResource, DynamicObject},
};

use crate::{
    app::ResourceTarget,
    model::{ResourceDescriptor, ResourceKind},
};

const PAGE_SIZE: u32 = 500;
const MAX_TOKEN_BYTES: usize = 16 * 1024;
const DEFAULT_CONTAINER_ANNOTATION: &str = "kubectl.kubernetes.io/default-container";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogTarget {
    pub resource: ResourceTarget,
    pub container: Option<String>,
    pub previous: bool,
}

impl LogTarget {
    #[must_use]
    pub fn pod(
        namespace: impl Into<String>,
        pod: impl Into<String>,
        container: Option<String>,
        previous: bool,
    ) -> Self {
        Self {
            resource: ResourceTarget {
                kind: ResourceKind::Pods.into(),
                namespace: Some(namespace.into()),
                name: pod.into(),
            },
            container,
            previous,
        }
    }

    #[must_use]
    pub const fn workload(resource: ResourceTarget, previous: bool) -> Self {
        Self {
            resource,
            container: None,
            previous,
        }
    }

    #[must_use]
    pub fn namespace(&self) -> &str {
        self.resource.namespace.as_deref().unwrap_or_default()
    }

    #[must_use]
    pub fn name(&self) -> &str {
        &self.resource.name
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogSource {
    pub namespace: String,
    pub pod: String,
    pub container: String,
    pub single_container: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogLine {
    pub source: Option<LogSource>,
    pub text: String,
}

impl From<String> for LogLine {
    fn from(text: String) -> Self {
        Self { source: None, text }
    }
}

impl From<&str> for LogLine {
    fn from(text: &str) -> Self {
        text.to_owned().into()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)]
pub struct LogSessionMeta {
    pub default_container: Option<String>,
    pub selected_container: Option<String>,
    pub all_containers: bool,
    pub multi_pods: bool,
    pub single_container: bool,
    pub source_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedLogs {
    pub sources: Vec<LogSource>,
    pub metadata: LogSessionMeta,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ScopeKind {
    Pod,
    Service,
    Template,
}

fn scope_kind(resource: &ResourceDescriptor) -> Option<ScopeKind> {
    if resource.version.as_ref() != "v1" || !resource.is_namespaced() {
        return None;
    }
    match (
        resource.group.as_ref(),
        resource.api_version.as_ref(),
        resource.kind.as_ref(),
        resource.plural.as_ref(),
    ) {
        ("", "v1", "Pod", "pods") => Some(ScopeKind::Pod),
        ("", "v1", "Service", "services") => Some(ScopeKind::Service),
        ("apps", "apps/v1", "Deployment", "deployments")
        | ("apps", "apps/v1", "StatefulSet", "statefulsets")
        | ("apps", "apps/v1", "DaemonSet", "daemonsets")
        | ("batch", "batch/v1", "Job", "jobs") => Some(ScopeKind::Template),
        _ => None,
    }
}

#[must_use]
pub fn supports_logs(resource: &ResourceDescriptor) -> bool {
    scope_kind(resource).is_some()
}

/// Resolve the complete Pod/container set for a log session without starting streams.
/// Dropping the future cancels any outstanding API request.
///
/// # Errors
/// Returns invalid scope, timeout, selector, identity and API errors. Failed pagination
/// returns an error instead of opening a silently incomplete set of log streams.
pub async fn resolve(
    client: Client,
    target: &LogTarget,
    all_containers: bool,
    call_timeout: Option<Duration>,
) -> Result<ResolvedLogs, String> {
    let kind = validate(target, call_timeout)?;
    let descriptor = &target.resource.kind;
    let resource = ApiResource {
        group: descriptor.group.to_string(),
        version: descriptor.version.to_string(),
        api_version: descriptor.api_version.to_string(),
        kind: descriptor.kind.to_string(),
        plural: descriptor.plural.to_string(),
    };
    let api = Api::<DynamicObject>::namespaced_with(client.clone(), target.namespace(), &resource);
    let object = request(
        api.get(target.name()),
        call_timeout,
        &format!("read {} for logs", descriptor.kind),
    )
    .await?;
    validate_identity(&object, &resource, target.namespace(), Some(target.name()))?;
    let (mut metadata, selected_container) =
        session_metadata(&object, target, kind, all_containers)?;
    let sources = if kind == ScopeKind::Pod {
        pod_sources(&object, &metadata, selected_container.as_deref())?
    } else {
        let selector = workload_selector(&object, kind)?;
        workload_sources(
            client,
            target.namespace(),
            &selector,
            &metadata,
            selected_container.as_deref(),
            call_timeout,
        )
        .await?
    };
    metadata.source_count = sources.len();
    Ok(ResolvedLogs { sources, metadata })
}

fn validate(target: &LogTarget, timeout: Option<Duration>) -> Result<ScopeKind, String> {
    let kind = scope_kind(&target.resource.kind).ok_or_else(|| {
        "Logs require a Pod, Service, Deployment, StatefulSet, DaemonSet or Job".to_owned()
    })?;
    if !target.resource.kind.operations.gettable()
        || target.namespace().trim().is_empty()
        || target.name().trim().is_empty()
        || target
            .container
            .as_deref()
            .is_some_and(|name| name.trim().is_empty())
        || (kind != ScopeKind::Pod && target.container.is_some())
    {
        return Err("Logs require a readable resource with an exact name and namespace; named containers require a Pod".to_owned());
    }
    if let Some(timeout) = timeout
        && (timeout.is_zero() || tokio::time::Instant::now().checked_add(timeout).is_none())
    {
        return Err(
            "Log request timeout must be positive and representable; use None to disable it"
                .to_owned(),
        );
    }
    Ok(kind)
}

fn session_metadata(
    object: &DynamicObject,
    target: &LogTarget,
    kind: ScopeKind,
    all_containers: bool,
) -> Result<(LogSessionMeta, Option<String>), String> {
    if kind == ScopeKind::Service {
        return Ok((
            LogSessionMeta {
                default_container: None,
                selected_container: None,
                all_containers: true,
                multi_pods: true,
                single_container: false,
                source_count: 0,
            },
            None,
        ));
    }
    let pointer = if kind == ScopeKind::Pod {
        "/spec"
    } else {
        "/spec/template/spec"
    };
    let spec = pod_spec(object, pointer)?;
    let names = container_names(&spec);
    let default_container = if target.container.is_some() {
        None
    } else {
        object
            .metadata
            .annotations
            .as_ref()
            .and_then(|annotations| annotations.get(DEFAULT_CONTAINER_ANNOTATION))
            .filter(|name| {
                spec.containers
                    .iter()
                    .any(|container| &container.name == *name)
            })
            .cloned()
    };
    let selected_container = target
        .container
        .clone()
        .or_else(|| default_container.clone())
        .or_else(|| (names.len() == 1).then(|| names[0].clone()));
    let single_container = target.container.is_some() || names.len() == 1;
    let all_containers = (all_containers && !single_container) || selected_container.is_none();
    Ok((
        LogSessionMeta {
            default_container,
            selected_container: if all_containers {
                None
            } else {
                selected_container.clone()
            },
            all_containers,
            multi_pods: kind != ScopeKind::Pod,
            single_container,
            source_count: 0,
        },
        selected_container,
    ))
}

fn workload_selector(object: &DynamicObject, kind: ScopeKind) -> Result<String, String> {
    let pointer = if kind == ScopeKind::Service {
        "/spec/selector"
    } else {
        "/spec/selector/matchLabels"
    };
    let labels: std::collections::BTreeMap<String, String> = serde_json::from_value(
        object
            .data
            .pointer(pointer)
            .cloned()
            .unwrap_or(serde_json::Value::Null),
    )
    .map_err(|_| {
        format!(
            "No valid label selector found for logs on {}",
            object.name_any()
        )
    })?;
    if labels.is_empty() {
        return Err(format!(
            "No valid label selector found for logs on {}",
            object.name_any()
        ));
    }
    if labels
        .iter()
        .any(|(key, value)| !valid_label_key(key) || !(value.is_empty() || valid_label_name(value)))
    {
        return Err(format!(
            "Invalid label selector for logs on {}",
            object.name_any()
        ));
    }
    Ok(labels
        .into_iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join(","))
}

fn valid_label_key(key: &str) -> bool {
    let name = if let Some((prefix, name)) = key.split_once('/') {
        if prefix.is_empty()
            || prefix.len() > 253
            || !prefix.split('.').all(|part| {
                !part.is_empty()
                    && part.as_bytes()[0].is_ascii_alphanumeric()
                    && part.as_bytes()[part.len() - 1].is_ascii_alphanumeric()
                    && part.bytes().all(|byte| {
                        byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-'
                    })
            })
        {
            return false;
        }
        name
    } else {
        key
    };
    valid_label_name(name)
}

fn valid_label_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 63
        && value.as_bytes()[0].is_ascii_alphanumeric()
        && value.as_bytes()[value.len() - 1].is_ascii_alphanumeric()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

async fn workload_sources(
    client: Client,
    namespace: &str,
    selector: &str,
    metadata: &LogSessionMeta,
    selected_container: Option<&str>,
    timeout: Option<Duration>,
) -> Result<Vec<LogSource>, String> {
    let resource = ApiResource {
        group: String::new(),
        version: "v1".to_owned(),
        api_version: "v1".to_owned(),
        kind: "Pod".to_owned(),
        plural: "pods".to_owned(),
    };
    let api = Api::<DynamicObject>::namespaced_with(client, namespace, &resource);
    let mut continuation = String::new();
    let mut seen_tokens = BTreeSet::new();
    let mut seen_sources = BTreeSet::new();
    let mut sources = Vec::new();
    loop {
        let params = ListParams::default()
            .limit(PAGE_SIZE)
            .labels(selector)
            .continue_token(&continuation);
        let page = request(api.list(&params), timeout, "list matching Pods for logs").await?;
        if page.types.api_version != "v1" || page.types.kind != "PodList" {
            return Err("The returned PodList does not match the selected log scope".to_owned());
        }
        for pod in page.items {
            validate_identity(&pod, &resource, namespace, None)?;
            for source in pod_sources(&pod, metadata, selected_container)? {
                if seen_sources.insert((
                    source.namespace.clone(),
                    source.pod.clone(),
                    source.container.clone(),
                )) {
                    sources.push(source);
                }
            }
        }
        let next = page.metadata.continue_.unwrap_or_default();
        if next.is_empty() {
            return Ok(sources);
        }
        if next.len() > MAX_TOKEN_BYTES || !seen_tokens.insert(next.clone()) {
            return Err("Pod pagination returned an invalid or repeated continuation token; log scope is incomplete".to_owned());
        }
        continuation = next;
    }
}

fn validate_identity(
    object: &DynamicObject,
    resource: &ApiResource,
    namespace: &str,
    name: Option<&str>,
) -> Result<(), String> {
    if object.metadata.namespace.as_deref() != Some(namespace)
        || object.metadata.name.as_deref().is_none_or(str::is_empty)
        || name.is_some_and(|name| object.metadata.name.as_deref() != Some(name))
        // List members commonly omit TypeMeta: the PodList envelope and exact
        // namespaced endpoint establish their type. A selected GET must carry it.
        || object.types.as_ref().map_or(name.is_some(), |types| {
            types.api_version != resource.api_version || types.kind != resource.kind
        })
    {
        return Err(format!(
            "The returned {} does not match the selected log scope",
            resource.kind
        ));
    }
    Ok(())
}

fn pod_spec(object: &DynamicObject, pointer: &str) -> Result<PodSpec, String> {
    serde_json::from_value(
        object
            .data
            .pointer(pointer)
            .cloned()
            .unwrap_or(serde_json::Value::Null),
    )
    .map_err(|error| {
        format!(
            "Invalid Pod spec for logs on {}: {error}",
            object.name_any()
        )
    })
}

fn container_names(spec: &PodSpec) -> Vec<String> {
    spec.init_containers
        .iter()
        .flatten()
        .map(|container| container.name.clone())
        .chain(
            spec.containers
                .iter()
                .map(|container| container.name.clone()),
        )
        .chain(
            spec.ephemeral_containers
                .iter()
                .flatten()
                .map(|container| container.name.clone()),
        )
        .collect()
}

fn pod_sources(
    pod: &DynamicObject,
    metadata: &LogSessionMeta,
    selected_container: Option<&str>,
) -> Result<Vec<LogSource>, String> {
    let spec = pod_spec(pod, "/spec")?;
    let all_names = container_names(&spec);
    let single_container = metadata.single_container || all_names.len() == 1;
    let names = if metadata.all_containers {
        all_names
    } else if let Some(container) = selected_container {
        vec![container.to_owned()]
    } else {
        all_names
    };
    Ok(names
        .into_iter()
        .map(|container| LogSource {
            namespace: pod.namespace().unwrap_or_default(),
            pod: pod.name_any(),
            container,
            single_container,
        })
        .collect())
}

async fn request<T>(
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

#[cfg(test)]
#[path = "log_scope_tests.rs"]
mod tests;
