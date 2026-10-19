use std::collections::BTreeSet;

use kube::{
    Api, Client,
    api::{Patch, PatchParams},
    core::{ApiResource, DynamicObject},
};
use serde_json::{Value, json};

use crate::{app::ResourceTarget, model::ResourceDescriptor};

#[cfg(test)]
#[path = "image_actions_tests.rs"]
mod tests;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerImage {
    pub name: String,
    pub image: String,
    pub init: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageSnapshot {
    pub target: ResourceTarget,
    pub uid: String,
    pub resource_version: String,
    pub images: Vec<ContainerImage>,
}

#[must_use]
pub fn supports_images(kind: &ResourceDescriptor) -> bool {
    kind.is_namespaced()
        && kind.operations.gettable()
        && matches!(
            (
                kind.group.as_ref(),
                kind.version.as_ref(),
                kind.kind.as_ref(),
                kind.plural.as_ref(),
                kind.api_version.as_ref()
            ),
            ("", "v1", "Pod", "pods", "v1")
                | ("apps", "v1", "Deployment", "deployments", "apps/v1")
                | ("apps", "v1", "StatefulSet", "statefulsets", "apps/v1")
                | ("apps", "v1", "DaemonSet", "daemonsets", "apps/v1")
        )
}

/// Load init and regular container images from the selected object's current revision.
///
/// # Errors
/// Returns an identity-bearing error for unsupported targets, API failures, or missing metadata/spec.
pub async fn load_images(client: Client, target: &ResourceTarget) -> Result<ImageSnapshot, String> {
    let identity = identity(target);
    let result: Result<ImageSnapshot, String> = async {
        validate_target(target)?;
        let object = api(client, target)
            .get(&target.name)
            .await
            .map_err(|error| error.to_string())?;
        snapshot(target, &object)
    }
    .await;
    result.map_err(|error| format!("Unable to load images for {identity}: {error}"))
}

/// Change only submitted, nonblank images that differ from the loaded snapshot.
///
/// The strategic merge uses container names and preserves their original order and other fields.
/// `resourceVersion` provides optimistic concurrency; a supplied UID is also checked by Kubernetes'
/// immutable-metadata validation, so a recreated resource cannot accept an old snapshot's patch.
/// Pods are checked again before patching, rejecting any owner reference and stale snapshots.
/// No conflict is retried automatically.
///
/// # Errors
/// Returns an identity-bearing error for invalid edits, stale metadata, or API failures.
pub async fn update_images(
    client: Client,
    snapshot: &ImageSnapshot,
    images: &[ContainerImage],
) -> Result<usize, String> {
    let identity = identity(&snapshot.target);
    let result: Result<usize, String> = async {
        let (payload, changed) = image_patch(snapshot, images)?;
        if changed == 0 {
            return Ok(0);
        }
        if snapshot.target.kind.group.is_empty() {
            let current = load_images(client.clone(), &snapshot.target).await?;
            if current.uid != snapshot.uid || current.resource_version != snapshot.resource_version {
                return Err("The Pod changed since its images were loaded; reload the image form before retrying".to_owned());
            }
        }
        api(client, &snapshot.target)
            .patch(
                &snapshot.target.name,
                &PatchParams::default(),
                &Patch::Strategic(payload),
            )
            .await
            .map_err(|error| match error {
                kube::Error::Api(response) if matches!(response.code, 409 | 422) => {
                    format!(
                        "{}; reload the image form before retrying",
                        response.message
                    )
                }
                error => error.to_string(),
            })?;
        Ok(changed)
    }
    .await;
    result.map_err(|error| format!("Unable to set images for {identity}: {error}"))
}

fn identity(target: &ResourceTarget) -> String {
    format!(
        "{} {}/{}",
        target.kind.kind,
        target.namespace.as_deref().unwrap_or("<no namespace>"),
        target.name
    )
}

fn validate_target(target: &ResourceTarget) -> Result<(), String> {
    if !supports_images(&target.kind) {
        return Err("Image editing requires a readable core/v1 Pod or apps/v1 Deployment, StatefulSet, or DaemonSet".to_owned());
    }
    if target.namespace.as_deref().is_none_or(str::is_empty) || target.name.is_empty() {
        return Err("Image editing requires an exact resource name and namespace".to_owned());
    }
    Ok(())
}

fn api(client: Client, target: &ResourceTarget) -> Api<DynamicObject> {
    let resource = ApiResource {
        group: target.kind.group.to_string(),
        version: target.kind.version.to_string(),
        api_version: target.kind.api_version.to_string(),
        kind: target.kind.kind.to_string(),
        plural: target.kind.plural.to_string(),
    };
    Api::namespaced_with(
        client,
        target.namespace.as_deref().expect("validated namespace"),
        &resource,
    )
}

fn snapshot(target: &ResourceTarget, object: &DynamicObject) -> Result<ImageSnapshot, String> {
    if object.metadata.name.as_deref() != Some(target.name.as_str())
        || object.metadata.namespace.as_deref() != target.namespace.as_deref()
    {
        return Err(
            "The returned object does not match the selected name and namespace".to_owned(),
        );
    }
    if target.kind.group.is_empty()
        && let Some(owner) = object
            .metadata
            .owner_references
            .as_ref()
            .and_then(|owners| owners.first())
    {
        return Err(format!(
            "This Pod is managed by {}/{}; set the image on the controller instead",
            owner.kind, owner.name
        ));
    }
    let required = |value: &Option<String>, field: &str| {
        value
            .clone()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| format!("The resource has no {field}; refusing an unguarded image edit"))
    };
    let uid = required(&object.metadata.uid, "UID")?;
    let resource_version = required(&object.metadata.resource_version, "resourceVersion")?;
    let path = if target.kind.group.is_empty() {
        "/spec"
    } else {
        "/spec/template/spec"
    };
    let spec = object
        .data
        .pointer(path)
        .and_then(Value::as_object)
        .ok_or_else(|| format!("The resource has no PodSpec at {path}"))?;
    let mut images = Vec::new();
    for (field, init) in [("initContainers", true), ("containers", false)] {
        match spec.get(field) {
            Some(Value::Array(containers)) => {
                for container in containers {
                    let name = container
                        .get("name")
                        .and_then(Value::as_str)
                        .filter(|name| !name.is_empty())
                        .ok_or_else(|| format!("An entry in {field} has no container name"))?;
                    let image = match container.get("image") {
                        None => "",
                        Some(Value::String(image)) => image,
                        Some(_) => {
                            return Err(format!("Container {name} has an invalid image field"));
                        }
                    };
                    images.push(ContainerImage {
                        name: name.to_owned(),
                        image: image.to_owned(),
                        init,
                    });
                }
            }
            None if init => {}
            _ => return Err(format!("The PodSpec has no valid {field} array")),
        }
    }
    if !images.iter().any(|image| !image.init) {
        return Err("The PodSpec has no regular containers to edit".to_owned());
    }
    validate_image_names(&images)?;
    Ok(ImageSnapshot {
        target: target.clone(),
        uid,
        resource_version,
        images,
    })
}

fn validate_image_names(images: &[ContainerImage]) -> Result<(), String> {
    let mut names = BTreeSet::new();
    for image in images {
        if image.name.is_empty() || !names.insert((image.init, &image.name)) {
            return Err(format!(
                "Duplicate or empty container name {:?}",
                image.name
            ));
        }
    }
    Ok(())
}

fn image_patch(
    snapshot: &ImageSnapshot,
    images: &[ContainerImage],
) -> Result<(Value, usize), String> {
    validate_target(&snapshot.target)?;
    if snapshot.uid.is_empty() || snapshot.resource_version.is_empty() {
        return Err("The image snapshot requires UID and resourceVersion preconditions".to_owned());
    }
    validate_image_names(&snapshot.images)?;
    validate_image_names(images)?;
    let mut changed = Vec::new();
    for image in images {
        let original = snapshot
            .images
            .iter()
            .find(|original| original.name == image.name && original.init == image.init)
            .ok_or_else(|| {
                format!(
                    "Container {:?} is not present in the loaded image snapshot",
                    image.name
                )
            })?;
        let trimmed = image.image.trim();
        if !trimmed.is_empty() && trimmed != original.image {
            changed.push(ContainerImage {
                name: image.name.clone(),
                image: trimmed.to_owned(),
                init: image.init,
            });
        }
    }
    let mut spec = serde_json::Map::new();
    for (field, init) in [("initContainers", true), ("containers", false)] {
        let updates: Vec<_> = snapshot
            .images
            .iter()
            .filter(|image| image.init == init)
            .filter_map(|original| {
                changed
                    .iter()
                    .find(|image| image.init == init && image.name == original.name)
            })
            .map(|image| json!({"name": image.name, "image": image.image}))
            .collect();
        if !updates.is_empty() {
            spec.insert(field.to_owned(), Value::Array(updates));
            spec.insert(
                format!("$setElementOrder/{field}"),
                Value::Array(
                    snapshot
                        .images
                        .iter()
                        .filter(|image| image.init == init)
                        .map(|image| json!({"name": image.name}))
                        .collect(),
                ),
            );
        }
    }
    let spec = if snapshot.target.kind.group.is_empty() {
        Value::Object(spec)
    } else {
        json!({"template": {"spec": spec}})
    };
    Ok((
        json!({"metadata": {"uid": snapshot.uid, "resourceVersion": snapshot.resource_version}, "spec": spec}),
        changed.len(),
    ))
}
