//! `ReplicaSet` revision rollback with a reviewed snapshot and atomic Deployment preconditions.

use std::{collections::BTreeMap, time::Duration};

use k8s_openapi::apimachinery::pkg::apis::meta::v1::{LabelSelector, OwnerReference};
use kube::{
    Api, Client,
    api::{Patch, PatchParams},
    core::{ApiResource, DynamicObject, Selector, SelectorExt},
};
use serde_json::{Value, json};

use crate::app::ResourceTarget;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const REVISION: &str = "deployment.kubernetes.io/revision";
const HASH: &str = "pod-template-hash";
const PRESERVED_ANNOTATIONS: [&str; 6] = [
    "kubectl.kubernetes.io/last-applied-configuration",
    REVISION,
    "deployment.kubernetes.io/revision-history",
    "deployment.kubernetes.io/desired-replicas",
    "deployment.kubernetes.io/max-replicas",
    "deprecated.deployment.rollback.to",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RollbackSnapshot {
    pub target: ResourceTarget,
    pub uid: String,
    pub resource_version: String,
    pub deployment: String,
    pub deployment_uid: String,
    pub deployment_resource_version: String,
    pub revision: i64,
    /// Raw JSON retains server-supported fields newer than our typed Kubernetes bindings.
    pub template: Value,
    pub annotations: BTreeMap<String, String>,
    pub already_matches: bool,
}

/// Load a stopped `ReplicaSet` revision and its current owning Deployment.
///
/// # Errors
/// Rejects unreadable, active, ownerless, paused, mismatched, or incomplete objects.
pub async fn load_snapshot(
    client: Client,
    target: &ResourceTarget,
) -> Result<RollbackSnapshot, String> {
    load_with_timeout(client, target, Some(REQUEST_TIMEOUT)).await
}

async fn load_with_timeout(
    client: Client,
    target: &ResourceTarget,
    call_timeout: Option<Duration>,
) -> Result<RollbackSnapshot, String> {
    validate_target(target)?;
    let namespace = target.namespace.as_deref().expect("validated namespace");
    let rs = get(
        &api(client.clone(), namespace, false),
        &target.name,
        "ReplicaSet",
        call_timeout,
    )
    .await?;
    validate_object(&rs, namespace, &target.name, "ReplicaSet")?;
    validate_stopped(&rs)?;
    let owner = deployment_owner(&rs)?;
    let deployment = get(
        &api(client, namespace, true),
        &owner.name,
        "Deployment",
        call_timeout,
    )
    .await?;
    snapshot(target, &rs, &deployment)
}

/// Restore the reviewed `ReplicaSet` template after revalidating both object identities.
///
/// # Errors
/// Rejects changed/recreated objects and API failures. A timed-out PATCH has an unknown
/// outcome and is never automatically retried.
pub async fn rollback(
    client: Client,
    reviewed: &RollbackSnapshot,
    call_timeout: Option<Duration>,
) -> Result<String, String> {
    validate_timeout(call_timeout)?;
    validate_target(&reviewed.target)?;
    if reviewed.uid.is_empty()
        || reviewed.resource_version.is_empty()
        || reviewed.deployment_uid.is_empty()
        || reviewed.deployment_resource_version.is_empty()
        || reviewed.deployment.is_empty()
        || reviewed.revision <= 0
    {
        return Err("Rollback requires complete reviewed identities and a positive revision; reload the confirmation".to_owned());
    }
    let current = load_with_timeout(client.clone(), &reviewed.target, call_timeout).await?;
    if current != *reviewed {
        return Err("The ReplicaSet or Deployment changed since the confirmation was loaded; reload before retrying rollback".to_owned());
    }
    if current.already_matches {
        return Ok(format!(
            "Skipped rollback: Deployment {} already matches revision {}",
            current.deployment, current.revision
        ));
    }
    let patch: Patch<Value> = Patch::Json(serde_json::from_value(json!([
        {"op":"test", "path":"/metadata/uid", "value":current.deployment_uid},
        {"op":"test", "path":"/metadata/resourceVersion", "value":current.deployment_resource_version},
        {"op":"replace", "path":"/spec/template", "value":current.template},
        // JSON Patch add replaces an existing field and also works when metadata
        // has no annotations field, unlike an unconditional replace.
        {"op":"add", "path":"/metadata/annotations", "value":current.annotations}
    ])).map_err(|error| format!("Unable to construct rollback patch: {error}"))?);
    let deployment_api = api(
        client,
        current
            .target
            .namespace
            .as_deref()
            .expect("validated namespace"),
        true,
    );
    let params = PatchParams::default();
    let request = deployment_api.patch(&current.deployment, &params, &patch);
    match call_timeout {
        Some(timeout) => tokio::time::timeout(timeout, request).await.map_err(|_| "Rollback request timed out; its outcome is unknown. Check the Deployment state before retrying".to_owned())?,
        None => request.await,
    }.map_err(|error| match error {
        kube::Error::Api(response) if matches!(response.code, 409 | 422) => format!(
            "{}; the Deployment changed or rejected the patch; reload before retrying rollback",
            response.message
        ),
        kube::Error::Api(response) if (400..500).contains(&response.code) => format!("Deployment {} rollback was rejected (HTTP {}): {}", current.deployment, response.code, response.message),
        error => format!("Unable to confirm rollback of Deployment {}: {error}; its outcome may be unknown. Check the Deployment state before retrying", current.deployment),
    })?;
    Ok(format!(
        "Rolled back Deployment {} to revision {} from ReplicaSet {}",
        current.deployment, current.revision, current.target.name
    ))
}

fn validate_timeout(timeout: Option<Duration>) -> Result<(), String> {
    if let Some(timeout) = timeout {
        if timeout.is_zero() {
            return Err(
                "Rollback request timeout must be positive; use None to disable it".to_owned(),
            );
        }
        if tokio::time::Instant::now().checked_add(timeout).is_none() {
            return Err("Rollback request timeout is too large to represent a deadline".to_owned());
        }
    }
    Ok(())
}

fn validate_target(target: &ResourceTarget) -> Result<(), String> {
    if !target.kind.is_apps_v1_replica_set()
        || !target.kind.is_namespaced()
        || !target.kind.operations.gettable()
        || target.name.is_empty()
        || target.namespace.as_deref().is_none_or(str::is_empty)
    {
        return Err(
            "Rollback requires a readable apps/v1 ReplicaSet with an exact name and namespace"
                .to_owned(),
        );
    }
    Ok(())
}

fn api(client: Client, namespace: &str, deployment: bool) -> Api<DynamicObject> {
    let resource = ApiResource {
        group: "apps".to_owned(),
        version: "v1".to_owned(),
        api_version: "apps/v1".to_owned(),
        kind: if deployment {
            "Deployment"
        } else {
            "ReplicaSet"
        }
        .to_owned(),
        plural: if deployment {
            "deployments"
        } else {
            "replicasets"
        }
        .to_owned(),
    };
    Api::namespaced_with(client, namespace, &resource)
}

async fn get(
    api: &Api<DynamicObject>,
    name: &str,
    kind: &str,
    call_timeout: Option<Duration>,
) -> Result<DynamicObject, String> {
    let request = api.get(name);
    match call_timeout {
        Some(timeout) => tokio::time::timeout(timeout, request)
            .await
            .map_err(|_| format!("Loading {kind} {name} for rollback timed out"))?,
        None => request.await,
    }
    .map_err(|error| format!("Unable to load {kind} {name} for rollback: {error}"))
}

fn validate_object(
    object: &DynamicObject,
    namespace: &str,
    name: &str,
    kind: &str,
) -> Result<(), String> {
    if object.metadata.name.as_deref() != Some(name)
        || object.metadata.namespace.as_deref() != Some(namespace)
        || object
            .types
            .as_ref()
            .is_none_or(|types| types.api_version != "apps/v1" || types.kind != kind)
    {
        return Err(format!(
            "The returned {kind} does not match the selected identity"
        ));
    }
    if object.metadata.deletion_timestamp.is_some() {
        return Err(format!(
            "The {kind} is being deleted; rollback is unavailable"
        ));
    }
    required(object.metadata.uid.as_deref(), kind, "UID")?;
    required(
        object.metadata.resource_version.as_deref(),
        kind,
        "resourceVersion",
    )?;
    Ok(())
}

fn required(value: Option<&str>, kind: &str, field: &str) -> Result<String, String> {
    value
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| format!("The {kind} has no {field}; refusing an unguarded rollback"))
}

fn validate_stopped(rs: &DynamicObject) -> Result<(), String> {
    if rs.data.pointer("/status/replicas").and_then(Value::as_i64) != Some(0) {
        return Err(
            "The ReplicaSet must report zero replicas before it can be rolled back".to_owned(),
        );
    }
    revision(rs)?;
    Ok(())
}

fn revision(rs: &DynamicObject) -> Result<i64, String> {
    rs.metadata
        .annotations
        .as_ref()
        .and_then(|annotations| annotations.get(REVISION))
        .and_then(|revision| revision.parse::<i64>().ok())
        .filter(|revision| *revision > 0)
        .ok_or_else(|| "The ReplicaSet has no valid positive deployment revision".to_owned())
}

fn deployment_owner(rs: &DynamicObject) -> Result<&OwnerReference, String> {
    let controllers: Vec<_> = rs
        .metadata
        .owner_references
        .iter()
        .flatten()
        .filter(|owner| owner.controller == Some(true))
        .collect();
    let [owner] = controllers.as_slice() else {
        return Err("The ReplicaSet must have one controlling Deployment owner".to_owned());
    };
    if owner.kind != "Deployment"
        || owner
            .api_version
            .split_once('/')
            .is_none_or(|(group, version)| {
                group != "apps" || version.is_empty() || version.contains('/')
            })
        || owner.uid.is_empty()
        || owner.name.is_empty()
    {
        return Err(
            "The ReplicaSet must have an apps Deployment controller with a name and UID".to_owned(),
        );
    }
    Ok(owner)
}

fn snapshot(
    target: &ResourceTarget,
    rs: &DynamicObject,
    deployment: &DynamicObject,
) -> Result<RollbackSnapshot, String> {
    let namespace = target.namespace.as_deref().expect("validated namespace");
    let owner = deployment_owner(rs)?;
    validate_object(deployment, namespace, &owner.name, "Deployment")?;
    let deployment_uid = required(deployment.metadata.uid.as_deref(), "Deployment", "UID")?;
    if deployment_uid != owner.uid {
        return Err(
            "The controlling Deployment was recreated; the ReplicaSet owner UID no longer matches"
                .to_owned(),
        );
    }
    if deployment
        .data
        .pointer("/spec/paused")
        .and_then(Value::as_bool)
        == Some(true)
    {
        return Err("The Deployment is paused; resume it before rolling back".to_owned());
    }
    validate_selector(rs, deployment)?;
    let desired_template = template(rs)?;
    let already_matches = equivalent_template(&desired_template, &template(deployment)?);
    let mut annotations = deployment.metadata.annotations.clone().unwrap_or_default();
    annotations.retain(|name, _| PRESERVED_ANNOTATIONS.contains(&name.as_str()));
    annotations.extend(
        rs.metadata
            .annotations
            .iter()
            .flatten()
            .filter(|(name, _)| !PRESERVED_ANNOTATIONS.contains(&name.as_str()))
            .map(|(name, value)| (name.clone(), value.clone())),
    );
    Ok(RollbackSnapshot {
        target: target.clone(),
        uid: required(rs.metadata.uid.as_deref(), "ReplicaSet", "UID")?,
        resource_version: required(
            rs.metadata.resource_version.as_deref(),
            "ReplicaSet",
            "resourceVersion",
        )?,
        deployment: owner.name.clone(),
        deployment_uid,
        deployment_resource_version: required(
            deployment.metadata.resource_version.as_deref(),
            "Deployment",
            "resourceVersion",
        )?,
        revision: revision(rs)?,
        template: desired_template,
        annotations,
        already_matches,
    })
}

fn validate_selector(rs: &DynamicObject, deployment: &DynamicObject) -> Result<(), String> {
    let selector: LabelSelector = serde_json::from_value(
        deployment
            .data
            .pointer("/spec/selector")
            .cloned()
            .ok_or_else(|| "The Deployment has no selector".to_owned())?,
    )
    .map_err(|error| format!("Invalid Deployment selector: {error}"))?;
    let selector = Selector::try_from(selector)
        .map_err(|error| format!("Invalid Deployment selector: {error}"))?;
    if !selector.matches(&rs.metadata.labels.clone().unwrap_or_default()) {
        return Err("The selected ReplicaSet does not match its Deployment selector".to_owned());
    }
    Ok(())
}

fn template(object: &DynamicObject) -> Result<Value, String> {
    let mut template = object
        .data
        .pointer("/spec/template")
        .filter(|value| value.is_object() && value.get("spec").is_some_and(Value::is_object))
        .cloned()
        .ok_or_else(|| "The resource has no valid Pod template".to_owned())?;
    if let Some(labels) = template
        .pointer_mut("/metadata/labels")
        .and_then(Value::as_object_mut)
    {
        labels.remove(HASH);
    }
    Ok(template)
}

fn equivalent_template(left: &Value, right: &Value) -> bool {
    fn normalize(value: &Value) -> Value {
        let mut value = value.clone();
        if let Some(metadata) = value.get_mut("metadata").and_then(Value::as_object_mut) {
            // Removing the hash can leave an empty labels map. Normalize only
            // these known optional metadata maps; unknown empty fields may be
            // meaningful and must not make a changed template look identical.
            for field in ["labels", "annotations"] {
                if metadata.get(field).is_some_and(|value| {
                    value.is_null() || value.as_object().is_some_and(serde_json::Map::is_empty)
                }) {
                    metadata.remove(field);
                }
            }
        }
        if value.get("metadata").is_some_and(|value| {
            value.is_null() || value.as_object().is_some_and(serde_json::Map::is_empty)
        }) && let Some(fields) = value.as_object_mut()
        {
            fields.remove("metadata");
        }
        value
    }
    normalize(left) == normalize(right)
}

#[cfg(test)]
#[path = "rollback_tests.rs"]
mod tests;
