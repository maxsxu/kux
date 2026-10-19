use std::time::Duration;

use k8s_openapi::apimachinery::pkg::apis::meta::v1::LabelSelector;
use kube::{
    Api, Client,
    core::{ApiResource, DynamicObject, Selector},
};

use crate::{app::ResourceTarget, model::ResourceKind};

pub(super) async fn load_selector(
    client: Client,
    target: &ResourceTarget,
) -> Result<String, String> {
    if target.kind.known_kind() != Some(ResourceKind::Deployments)
        || target.kind.plural.as_ref() != "deployments"
        || !target.kind.is_namespaced()
        || !target.kind.operations.gettable()
        || target.name.trim().is_empty()
    {
        return Err("Child navigation requires a readable apps/v1 Deployment with an exact name and namespace".to_owned());
    }
    let namespace = target
        .namespace
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| "Deployment child navigation requires an exact namespace".to_owned())?;
    let resource = ApiResource {
        group: "apps".to_owned(),
        version: "v1".to_owned(),
        api_version: "apps/v1".to_owned(),
        kind: "Deployment".to_owned(),
        plural: "deployments".to_owned(),
    };
    let api = Api::<DynamicObject>::namespaced_with(client, namespace, &resource);
    let deployment = tokio::time::timeout(Duration::from_secs(10), api.get(&target.name))
        .await
        .map_err(|_| "Loading the Deployment selector timed out".to_owned())?
        .map_err(|error| format!("Unable to load the Deployment selector: {error}"))?;
    if deployment.metadata.name.as_deref() != Some(target.name.as_str())
        || deployment.metadata.namespace.as_deref() != Some(namespace)
        || deployment
            .types
            .as_ref()
            .is_none_or(|types| types.api_version != "apps/v1" || types.kind != "Deployment")
    {
        return Err("The returned Deployment does not match the selected identity".to_owned());
    }
    let selector: LabelSelector = serde_json::from_value(
        deployment
            .data
            .pointer("/spec/selector")
            .cloned()
            .ok_or_else(|| {
                "The Deployment has no selector; refusing an unfiltered child view".to_owned()
            })?,
    )
    .map_err(|error| format!("Invalid Deployment selector: {error}"))?;
    if selector
        .match_labels
        .as_ref()
        .is_none_or(std::collections::BTreeMap::is_empty)
        && selector
            .match_expressions
            .as_ref()
            .is_none_or(Vec::is_empty)
    {
        return Err(
            "The Deployment selector is empty; refusing an unfiltered child view".to_owned(),
        );
    }
    let selector = Selector::try_from(selector)
        .map_err(|error| format!("Invalid Deployment selector: {error}"))?
        .to_string();
    if selector.trim().is_empty() {
        return Err(
            "The Deployment selector is empty; refusing an unfiltered child view".to_owned(),
        );
    }
    Ok(selector)
}

#[cfg(test)]
#[path = "deployment_navigation_tests.rs"]
mod tests;
