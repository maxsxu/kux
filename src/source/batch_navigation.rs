use std::time::Duration;

use k8s_openapi::apimachinery::pkg::apis::meta::v1::LabelSelector;
use kube::{
    Api, Client,
    core::{ApiResource, DynamicObject, Selector},
};

use crate::app::ResourceTarget;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BatchNavigationFilter {
    CronJobJobs {
        owner_name: String,
    },
    JobPods {
        /// A missing selector matches nothing; an explicitly empty selector matches everything.
        label_selector: Option<String>,
    },
}

pub(super) async fn load_navigation(
    client: Client,
    target: &ResourceTarget,
    call_timeout: Option<Duration>,
) -> Result<BatchNavigationFilter, String> {
    let namespace = validate_target(target, call_timeout)?;
    let resource = ApiResource {
        group: "batch".to_owned(),
        version: "v1".to_owned(),
        api_version: "batch/v1".to_owned(),
        kind: target.kind.kind.to_string(),
        plural: target.kind.plural.to_string(),
    };
    let api = Api::<DynamicObject>::namespaced_with(client, namespace, &resource);
    let request = api.get(&target.name);
    let object = match call_timeout {
        Some(timeout) => tokio::time::timeout(timeout, request).await.map_err(|_| {
            format!(
                "Loading the {} for child navigation timed out",
                resource.kind
            )
        })?,
        None => request.await,
    }
    .map_err(|error| {
        format!(
            "Unable to load the {} for child navigation: {error}",
            resource.kind
        )
    })?;
    if object.metadata.name.as_deref() != Some(target.name.as_str())
        || object.metadata.namespace.as_deref() != Some(namespace)
        || object
            .types
            .as_ref()
            .is_none_or(|types| types.api_version != "batch/v1" || types.kind != resource.kind)
    {
        return Err(format!(
            "The returned {} does not match the selected identity",
            resource.kind
        ));
    }
    if target.kind.is_cron_job() {
        // K9s matches every Job owner reference by name, regardless of UID or controller flag.
        return Ok(BatchNavigationFilter::CronJobJobs {
            owner_name: target.name.clone(),
        });
    }
    let label_selector = job_selector(&object)?;
    Ok(BatchNavigationFilter::JobPods { label_selector })
}

fn validate_target(target: &ResourceTarget, timeout: Option<Duration>) -> Result<&str, String> {
    if target.kind.group.as_ref() != "batch"
        || target.kind.version.as_ref() != "v1"
        || target.kind.api_version.as_ref() != "batch/v1"
        || !target.kind.is_namespaced()
        || !target.kind.operations.gettable()
        || !matches!(
            (target.kind.kind.as_ref(), target.kind.plural.as_ref()),
            ("CronJob", "cronjobs") | ("Job", "jobs")
        )
        || target.name.trim().is_empty()
    {
        return Err("Child navigation requires a readable batch/v1 CronJob or Job with an exact name and namespace".to_owned());
    }
    let namespace = target
        .namespace
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| "Batch child navigation requires an exact namespace".to_owned())?;
    if let Some(timeout) = timeout
        && (timeout.is_zero() || tokio::time::Instant::now().checked_add(timeout).is_none())
    {
        return Err(
            "Batch navigation request timeout must be positive and representable; use None to disable it"
                .to_owned(),
        );
    }
    Ok(namespace)
}

fn job_selector(object: &DynamicObject) -> Result<Option<String>, String> {
    let Some(raw) = object
        .data
        .pointer("/spec/selector")
        .filter(|value| !value.is_null())
    else {
        return Ok(None);
    };
    let selector: LabelSelector = serde_json::from_value(raw.clone())
        .map_err(|error| format!("Invalid Job selector: {error}"))?;
    validate_selector(&selector).map_err(|error| format!("Invalid Job selector: {error}"))?;
    let selector =
        Selector::try_from(selector).map_err(|error| format!("Invalid Job selector: {error}"))?;
    Ok(Some(selector.to_string()))
}

fn validate_selector(selector: &LabelSelector) -> Result<(), String> {
    if let Some(labels) = &selector.match_labels {
        for (key, value) in labels {
            validate_key(key)?;
            validate_value(value)?;
        }
    }
    if let Some(expressions) = &selector.match_expressions {
        for expression in expressions {
            validate_key(&expression.key)?;
            let values = expression.values.as_deref().unwrap_or_default();
            match expression.operator.as_str() {
                "In" | "NotIn" if values.is_empty() => {
                    return Err(format!(
                        "{} requires at least one value",
                        expression.operator
                    ));
                }
                "Exists" | "DoesNotExist" if !values.is_empty() => {
                    return Err(format!("{} does not accept values", expression.operator));
                }
                "In" | "NotIn" | "Exists" | "DoesNotExist" => {}
                _ => return Err(format!("unsupported operator {:?}", expression.operator)),
            }
            for value in values {
                validate_value(value)?;
            }
        }
    }
    Ok(())
}

fn validate_key(key: &str) -> Result<(), String> {
    let name = if let Some((prefix, name)) = key.split_once('/') {
        if prefix.is_empty()
            || prefix.len() > 253
            || !prefix.split('.').all(|label| {
                !label.is_empty()
                    && label.bytes().all(|byte| {
                        byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-'
                    })
                    && label.as_bytes()[0].is_ascii_alphanumeric()
                    && label.as_bytes()[label.len() - 1].is_ascii_alphanumeric()
            })
        {
            return Err(format!("invalid label key {key:?}"));
        }
        name
    } else {
        key
    };
    if !valid_label_name(name) {
        return Err(format!("invalid label key {key:?}"));
    }
    Ok(())
}

fn validate_value(value: &str) -> Result<(), String> {
    if value.is_empty() || valid_label_name(value) {
        Ok(())
    } else {
        Err(format!("invalid label value {value:?}"))
    }
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

#[cfg(test)]
#[path = "batch_navigation_tests.rs"]
mod tests;
