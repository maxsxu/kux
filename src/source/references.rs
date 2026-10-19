//! Read-only workload references for the five K9s `UsedBy` source resources.

use std::{
    collections::{BTreeMap, BTreeSet},
    future::Future,
    sync::Arc,
    time::Duration,
};

use futures::{StreamExt as _, stream};
use kube::{
    Api, Client,
    api::ListParams,
    core::{ApiResource, DynamicObject},
};
use serde_json::Value;
use tokio::sync::Mutex;

use crate::{
    app::ResourceTarget,
    model::{ResourceDescriptor, ResourceOperations},
};

const PAGE_SIZE: u32 = 500;
const MAX_PAGES: usize = 200;
const MAX_ERRORS: usize = 32;
const MAX_SA_CACHE: usize = 5_000;
const MAX_TOKEN_BYTES: usize = 16_384;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReferenceEntry {
    pub target: ResourceTarget,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReferenceScan {
    pub entries: Vec<ReferenceEntry>,
    /// Any error makes the result partial, including permission and pagination failures.
    pub errors: Vec<String>,
    /// Number of workload objects inspected, including objects that did not match.
    pub scanned: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReferenceKind {
    ConfigMap,
    Secret,
    Claim,
    ServiceAccount,
    PriorityClass,
}

fn reference_kind(kind: &ResourceDescriptor) -> Option<ReferenceKind> {
    if kind.version.as_ref() != "v1" {
        return None;
    }
    match (
        kind.group.as_ref(),
        kind.api_version.as_ref(),
        kind.kind.as_ref(),
        kind.plural.as_ref(),
        kind.namespaced,
    ) {
        ("", "v1", "ConfigMap", "configmaps", true) => Some(ReferenceKind::ConfigMap),
        ("", "v1", "Secret", "secrets", true) => Some(ReferenceKind::Secret),
        ("", "v1", "PersistentVolumeClaim", "persistentvolumeclaims", true) => {
            Some(ReferenceKind::Claim)
        }
        ("", "v1", "ServiceAccount", "serviceaccounts", true) => {
            Some(ReferenceKind::ServiceAccount)
        }
        (
            "scheduling.k8s.io",
            "scheduling.k8s.io/v1",
            "PriorityClass",
            "priorityclasses",
            false,
        ) => Some(ReferenceKind::PriorityClass),
        _ => None,
    }
}

#[must_use]
pub fn supports_references(kind: &ResourceDescriptor) -> bool {
    reference_kind(kind).is_some()
}

/// Scan the five registered workload kinds without spawning detached tasks.
/// Dropping this future cancels outstanding list and service-account requests.
///
/// # Errors
/// Rejects unsupported or incomplete source identities and invalid timeouts.
/// Individual API failures are returned in `ReferenceScan::errors` with successful matches.
pub async fn scan_references(
    client: Client,
    target: &ResourceTarget,
    call_timeout: Option<Duration>,
) -> Result<ReferenceScan, String> {
    scan_with_page_limit(client, target, call_timeout, MAX_PAGES).await
}

async fn scan_with_page_limit(
    client: Client,
    target: &ResourceTarget,
    call_timeout: Option<Duration>,
    max_pages: usize,
) -> Result<ReferenceScan, String> {
    let kind = validate(target, call_timeout)?;
    let cache = Arc::new(Mutex::new(BTreeMap::new()));
    let scanners = workloads().into_iter().map(|descriptor| {
        scan_workload(
            client.clone(),
            target,
            kind,
            descriptor,
            call_timeout,
            cache.clone(),
            max_pages,
        )
    });
    // Exactly five scanners; keeping these futures in the stream makes cancellation structured.
    let mut pending = stream::iter(scanners).buffer_unordered(5);
    let mut report = ReferenceScan::default();
    while let Some(part) = pending.next().await {
        report.scanned += part.scanned;
        report.entries.extend(part.entries);
        for error in part.errors {
            add_error(&mut report.errors, error);
        }
    }
    report
        .entries
        .sort_by(|left, right| entry_key(left).cmp(&entry_key(right)));
    report
        .entries
        .dedup_by(|left, right| entry_key(left) == entry_key(right));
    report.errors.sort();
    Ok(report)
}

fn entry_key(entry: &ReferenceEntry) -> (&str, &str, &str, &str, &str) {
    let target = &entry.target;
    (
        target.namespace.as_deref().unwrap_or_default(),
        &target.name,
        &target.kind.group,
        &target.kind.version,
        &target.kind.plural,
    )
}

fn validate(target: &ResourceTarget, timeout: Option<Duration>) -> Result<ReferenceKind, String> {
    let kind = reference_kind(&target.kind).ok_or_else(|| "UsedBy supports only core/v1 ConfigMaps, Secrets, PVCs, ServiceAccounts and scheduling.k8s.io/v1 PriorityClasses".to_owned())?;
    let valid_name = |name: &str| {
        !name.is_empty()
            && !name
                .chars()
                .any(|c| c.is_control() || c.is_whitespace() || matches!(c, '/' | '?' | '#'))
    };
    if !valid_name(&target.name)
        || if target.kind.namespaced {
            target.namespace.as_deref().is_none_or(|ns| !valid_name(ns))
        } else {
            target.namespace.is_some()
        }
    {
        return Err(
            "UsedBy requires an exact source name and its resource namespace scope".to_owned(),
        );
    }
    if let Some(timeout) = timeout
        && (timeout.is_zero() || tokio::time::Instant::now().checked_add(timeout).is_none())
    {
        return Err(
            "UsedBy request timeout must be positive and representable; use None to disable it"
                .to_owned(),
        );
    }
    Ok(kind)
}

fn workloads() -> [ResourceDescriptor; 5] {
    [
        ("apps", "Deployment", "deployments"),
        ("apps", "StatefulSet", "statefulsets"),
        ("apps", "DaemonSet", "daemonsets"),
        ("batch", "Job", "jobs"),
        ("batch", "CronJob", "cronjobs"),
    ]
    .map(|(group, kind, plural)| {
        ResourceDescriptor::new(
            group,
            "v1",
            format!("{group}/v1"),
            kind,
            plural,
            true,
            ResourceOperations::all(),
        )
    })
}

type SaCache = Arc<Mutex<BTreeMap<String, Result<bool, String>>>>;

fn dynamic_api(
    client: Client,
    descriptor: &ResourceDescriptor,
    namespace: Option<&str>,
) -> Api<DynamicObject> {
    let resource = ApiResource {
        group: descriptor.group.to_string(),
        version: descriptor.version.to_string(),
        api_version: descriptor.api_version.to_string(),
        kind: descriptor.kind.to_string(),
        plural: descriptor.plural.to_string(),
    };
    match namespace {
        Some(namespace) => Api::namespaced_with(client, namespace, &resource),
        None => Api::all_with(client, &resource),
    }
}

#[allow(clippy::too_many_arguments)]
async fn scan_workload(
    client: Client,
    source: &ResourceTarget,
    kind: ReferenceKind,
    descriptor: ResourceDescriptor,
    timeout: Option<Duration>,
    cache: SaCache,
    max_pages: usize,
) -> ReferenceScan {
    let api = dynamic_api(client.clone(), &descriptor, source.namespace.as_deref());
    let mut report = ReferenceScan::default();
    let mut continuation = String::new();
    let mut seen = BTreeSet::new();
    for page in 0..max_pages {
        let params = ListParams::default()
            .limit(PAGE_SIZE)
            .continue_token(&continuation);
        let result = request(
            api.list(&params),
            timeout,
            &format!("list {}", descriptor.plural),
        )
        .await;
        let list = match result {
            Ok(list) => list,
            Err(error) => {
                add_error(&mut report.errors, error);
                break;
            }
        };
        // A server ignoring the requested page size must not defeat the processing bound.
        if list.items.len() > PAGE_SIZE as usize {
            add_error(
                &mut report.errors,
                format!(
                    "{} returned more than the requested {PAGE_SIZE} objects; results are truncated",
                    descriptor.plural
                ),
            );
        }
        for object in list.items.into_iter().take(PAGE_SIZE as usize) {
            report.scanned += 1;
            match inspect(&client, source, kind, &descriptor, &object, timeout, &cache).await {
                Ok(Some(entry)) => report.entries.push(entry),
                Ok(None) => {}
                Err(error) => add_error(&mut report.errors, error),
            }
        }
        let next = list.metadata.continue_.unwrap_or_default();
        if next.is_empty() {
            break;
        }
        if next.len() > MAX_TOKEN_BYTES || !seen.insert(next.clone()) {
            add_error(
                &mut report.errors,
                format!(
                    "{} pagination returned an invalid or repeated continuation token; results are partial",
                    descriptor.plural
                ),
            );
            break;
        }
        if page + 1 == max_pages {
            add_error(
                &mut report.errors,
                format!(
                    "{} scan reached the {} object limit; results are partial",
                    descriptor.plural,
                    max_pages * PAGE_SIZE as usize
                ),
            );
        }
        continuation = next;
    }
    report
}

#[allow(clippy::too_many_arguments)]
async fn inspect(
    client: &Client,
    source: &ResourceTarget,
    kind: ReferenceKind,
    descriptor: &ResourceDescriptor,
    object: &DynamicObject,
    timeout: Option<Duration>,
    cache: &SaCache,
) -> Result<Option<ReferenceEntry>, String> {
    let name = object.metadata.name.as_deref().filter(|s| !s.is_empty());
    let namespace = object
        .metadata
        .namespace
        .as_deref()
        .filter(|s| !s.is_empty());
    let (Some(name), Some(namespace)) = (name, namespace) else {
        return Err(format!(
            "{} returned an object without its name or namespace; results are partial",
            descriptor.plural
        ));
    };
    if source
        .namespace
        .as_deref()
        .is_some_and(|ns| ns != namespace)
        || object.types.as_ref().is_some_and(|types| {
            types.api_version != descriptor.api_version.as_ref()
                || types.kind != descriptor.kind.as_ref()
        })
    {
        return Err(format!(
            "{} returned an object outside the requested resource scope; results are partial",
            descriptor.plural
        ));
    }
    let path = if descriptor.kind.as_ref() == "CronJob" {
        "/spec/jobTemplate/spec/template/spec"
    } else {
        "/spec/template/spec"
    };
    let spec = object
        .data
        .pointer(path)
        .filter(|v| v.is_object())
        .ok_or_else(|| {
            format!(
                "{} {namespace}/{name} has no Pod template; results are partial",
                descriptor.kind
            )
        })?;
    let mut matches = matches_spec(spec, kind, &source.name);
    if kind == ReferenceKind::Claim && descriptor.kind.as_ref() == "StatefulSet" {
        matches |= generated_claim(object, name, &source.name);
    }
    if !matches
        && kind == ReferenceKind::Secret
        && let Some(account) = spec
            .get("serviceAccountName")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
    {
        matches =
            account_uses_secret(client, namespace, account, &source.name, timeout, cache).await?;
    }
    Ok(matches.then(|| ReferenceEntry {
        target: ResourceTarget {
            kind: descriptor.clone(),
            namespace: Some(namespace.to_owned()),
            name: name.to_owned(),
        },
    }))
}

fn array<'a>(value: &'a Value, name: &str) -> impl Iterator<Item = &'a Value> {
    value
        .get(name)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
}

fn named(value: &Value, path: &str, name: &str) -> bool {
    value.pointer(path).and_then(Value::as_str) == Some(name)
}

fn matches_spec(spec: &Value, kind: ReferenceKind, name: &str) -> bool {
    match kind {
        ReferenceKind::ConfigMap | ReferenceKind::Secret => {
            let (env_from, env_key, volume, projected) = if kind == ReferenceKind::ConfigMap {
                (
                    "/configMapRef/name",
                    "/valueFrom/configMapKeyRef/name",
                    "/configMap/name",
                    "/configMap/name",
                )
            } else {
                (
                    "/secretRef/name",
                    "/valueFrom/secretKeyRef/name",
                    "/secret/secretName",
                    "/secret/name",
                )
            };
            ["containers", "initContainers", "ephemeralContainers"]
                .into_iter()
                .flat_map(|field| array(spec, field))
                .any(|container| {
                    array(container, "envFrom").any(|env| named(env, env_from, name))
                        || array(container, "env").any(|env| named(env, env_key, name))
                })
                || array(spec, "volumes").any(|v| {
                    named(v, volume, name)
                        || v.get("projected").is_some_and(|p| {
                            array(p, "sources").any(|source| named(source, projected, name))
                        })
                })
                || (kind == ReferenceKind::Secret
                    && array(spec, "imagePullSecrets").any(|s| named(s, "/name", name)))
        }
        ReferenceKind::Claim => {
            array(spec, "volumes").any(|v| named(v, "/persistentVolumeClaim/claimName", name))
        }
        ReferenceKind::ServiceAccount => {
            spec.get("serviceAccountName")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .unwrap_or("default")
                == name
        }
        ReferenceKind::PriorityClass => named(spec, "/priorityClassName", name),
    }
}

fn generated_claim(object: &DynamicObject, workload: &str, claim: &str) -> bool {
    object.data.get("spec").is_some_and(|spec| {
        array(spec, "volumeClaimTemplates").any(|template| {
            template
                .pointer("/metadata/name")
                .and_then(Value::as_str)
                .filter(|name| !name.is_empty())
                .is_some_and(|name| {
                    claim
                        .strip_prefix(&format!("{name}-{workload}-"))
                        .is_some_and(|ordinal| {
                            !ordinal.is_empty() && ordinal.bytes().all(|byte| byte.is_ascii_digit())
                        })
                })
        })
    })
}

async fn account_uses_secret(
    client: &Client,
    namespace: &str,
    name: &str,
    secret: &str,
    timeout: Option<Duration>,
    cache: &SaCache,
) -> Result<bool, String> {
    // Each scan has one namespace and one Secret. Cache failures too so a denied
    // account produces one diagnostic and one request across all workload kinds.
    let mut cache = cache.lock().await;
    if let Some(result) = cache.get(name) {
        return result.clone();
    }
    if cache.len() >= MAX_SA_CACHE {
        return Err(
            "ServiceAccount lookup limit reached; Secret references are partial".to_owned(),
        );
    }
    let descriptor = ResourceDescriptor::new(
        "",
        "v1",
        "v1",
        "ServiceAccount",
        "serviceaccounts",
        true,
        ResourceOperations::all(),
    );
    let api = dynamic_api(client.clone(), &descriptor, Some(namespace));
    let result = request(api.get(name), timeout, &format!("get ServiceAccount {namespace}/{name} for Secret references")).await.and_then(|account| {
        if account.metadata.name.as_deref() != Some(name) || account.metadata.namespace.as_deref() != Some(namespace) || account.types.as_ref().is_some_and(|types| types.api_version != "v1" || types.kind != "ServiceAccount") {
            return Err(format!("ServiceAccount {namespace}/{name} returned an unexpected identity; Secret references are partial"));
        }
        Ok(array(&account.data, "secrets").any(|reference| named(reference, "/name", secret) && reference.get("namespace").and_then(Value::as_str).is_none_or(|ns| ns.is_empty() || ns == namespace)) || array(&account.data, "imagePullSecrets").any(|reference| named(reference, "/name", secret)))
    });
    cache.insert(name.to_owned(), result.clone());
    result
}

async fn request<T>(
    future: impl Future<Output = Result<T, kube::Error>>,
    timeout: Option<Duration>,
    operation: &str,
) -> Result<T, String> {
    match timeout {
        Some(timeout) => tokio::time::timeout(timeout, future)
            .await
            .map_err(|_| format!("UsedBy {operation} timed out; results are partial"))?,
        None => future.await,
    }
    .map_err(|error| format!("Unable to {operation}: {error}; UsedBy results are partial"))
}

fn add_error(errors: &mut Vec<String>, error: String) {
    if errors.contains(&error) {
        return;
    }
    if errors.len() < MAX_ERRORS {
        errors.push(error);
    } else if errors.len() == MAX_ERRORS {
        errors.push("Additional UsedBy errors omitted; results are partial".to_owned());
    }
}

#[cfg(test)]
#[path = "references_tests.rs"]
mod tests;
