use std::{cmp::Ordering, sync::Arc};

use k8s_openapi::apimachinery::pkg::apis::meta::v1::{
    APIResource as KubernetesApiResource, APIResourceList,
};
use kube::{
    Client,
    core::{
        ApiResource, Version,
        discovery::v2::{APIGroupDiscovery, APIResourceDiscovery},
    },
    discovery::{ApiCapabilities, Scope, verbs},
};

use crate::model::{ResourceDescriptor, ResourceOperations};

/// Discovers every listable primary resource served by the Kubernetes API.
///
/// Aggregated discovery keeps startup to two API requests on modern clusters.
/// Older API servers are supported by falling back to the legacy per-group
/// discovery endpoints.
///
/// # Errors
///
/// Returns an error when both aggregated and legacy discovery fail.
pub async fn discover_resources(client: Client) -> Result<Vec<ResourceDescriptor>, String> {
    let mut resources = match discover_aggregated(&client).await {
        Ok(resources) => resources,
        Err(aggregated_error) => discover_legacy(&client).await.map_err(|legacy_error| {
            format!(
                "Kubernetes API discovery failed (aggregated: {aggregated_error}; legacy: \
                     {legacy_error})"
            )
        })?,
    };
    sort_and_dedup(&mut resources);
    Ok(resources)
}

async fn discover_legacy(client: &Client) -> Result<Vec<ResourceDescriptor>, kube::Error> {
    let (groups, core) =
        tokio::try_join!(client.list_api_groups(), client.list_core_api_versions())?;
    let mut resources = Vec::new();
    for group in groups.groups {
        for version in group.versions {
            let list = client
                .list_api_group_resources(&version.group_version)
                .await?;
            resources.extend(descriptors_from_legacy_list(list));
        }
    }
    for version in core.versions {
        let list = client.list_core_api_resources(&version).await?;
        resources.extend(descriptors_from_legacy_list(list));
    }
    retain_most_stable_versions(&mut resources);
    Ok(resources)
}

fn descriptors_from_legacy_list(list: APIResourceList) -> Vec<ResourceDescriptor> {
    let group_version = list.group_version;
    let scalable = list
        .resources
        .iter()
        .filter_map(|resource| resource.name.strip_suffix("/scale").map(str::to_owned))
        .collect::<std::collections::HashSet<_>>();
    list.resources
        .into_iter()
        .filter(|resource| !resource.name.contains('/'))
        .filter_map(|resource| descriptor_from_legacy_resource(&group_version, resource))
        .map(|mut resource| {
            resource.operations = resource
                .operations
                .with_scale(scalable.contains(resource.plural.as_ref()));
            resource
        })
        .collect()
}

fn descriptor_from_legacy_resource(
    group_version: &str,
    resource: KubernetesApiResource,
) -> Option<ResourceDescriptor> {
    let (list_group, list_version) = group_version
        .rsplit_once('/')
        .map_or(("", group_version), |(group, version)| (group, version));
    let group = resource.group.as_deref().unwrap_or(list_group);
    let version = resource.version.as_deref().unwrap_or(list_version);
    let api_version = if group.is_empty() {
        version.to_owned()
    } else {
        format!("{group}/{version}")
    };
    let short_names = resource.short_names.clone().unwrap_or_default();
    descriptor(
        ApiResource {
            group: group.to_owned(),
            version: version.to_owned(),
            api_version,
            kind: resource.kind,
            plural: resource.name,
        },
        &ApiCapabilities {
            scope: if resource.namespaced {
                Scope::Namespaced
            } else {
                Scope::Cluster
            },
            subresources: Vec::new(),
            operations: resource.verbs,
        },
    )
    .map(|descriptor| descriptor.with_short_names(short_names))
}

async fn discover_aggregated(client: &Client) -> Result<Vec<ResourceDescriptor>, kube::Error> {
    let (groups, core) = tokio::try_join!(
        client.list_api_groups_aggregated(),
        client.list_core_api_versions_aggregated()
    )?;
    let mut resources: Vec<_> = groups
        .items
        .into_iter()
        .chain(core.items)
        .flat_map(descriptors_from_aggregated_group)
        .collect();
    retain_most_stable_versions(&mut resources);
    Ok(resources)
}

fn descriptors_from_aggregated_group(group: APIGroupDiscovery) -> Vec<ResourceDescriptor> {
    let group_name = group
        .metadata
        .and_then(|metadata| metadata.name)
        .unwrap_or_default();
    let mut descriptors = Vec::new();
    for version in group.versions {
        let version_name = version.version.unwrap_or_default();
        descriptors.extend(version.resources.into_iter().filter_map(|resource| {
            descriptor_from_aggregated_resource(&group_name, &version_name, resource)
        }));
    }
    descriptors
}

fn descriptor_from_aggregated_resource(
    group: &str,
    version: &str,
    resource: APIResourceDiscovery,
) -> Option<ResourceDescriptor> {
    if !resource.verbs.iter().any(|verb| verb == verbs::LIST) {
        return None;
    }
    let plural = resource.resource?;
    let kind = resource.response_kind?.kind?;
    let api_version = if group.is_empty() {
        version.to_owned()
    } else {
        format!("{group}/{version}")
    };
    let operations = ResourceOperations::new(
        resource.verbs.iter().any(|verb| verb == verbs::WATCH),
        resource.verbs.iter().any(|verb| verb == verbs::GET),
        resource.verbs.iter().any(|verb| verb == verbs::DELETE),
    )
    .with_scale(
        resource
            .subresources
            .iter()
            .any(|subresource| subresource.subresource.as_deref() == Some("scale")),
    );
    Some(
        ResourceDescriptor::new(
            group,
            version,
            api_version,
            kind,
            plural,
            resource.scope.as_deref() == Some("Namespaced"),
            operations,
        )
        .with_short_names(resource.short_names),
    )
}

fn retain_most_stable_versions(resources: &mut Vec<ResourceDescriptor>) {
    resources.sort_unstable_by(|left, right| {
        left.group
            .cmp(&right.group)
            .then_with(|| left.kind.cmp(&right.kind))
            .then_with(|| {
                Version::parse(&right.version)
                    .priority()
                    .cmp(&Version::parse(&left.version).priority())
            })
    });
    resources.dedup_by(|left, right| left.group == right.group && left.kind == right.kind);
}

fn descriptor(resource: ApiResource, capabilities: &ApiCapabilities) -> Option<ResourceDescriptor> {
    if !capabilities.supports_operation(verbs::LIST) {
        return None;
    }
    Some(ResourceDescriptor::new(
        Arc::from(resource.group),
        Arc::from(resource.version),
        Arc::from(resource.api_version),
        Arc::from(resource.kind),
        Arc::from(resource.plural),
        capabilities.scope == Scope::Namespaced,
        ResourceOperations::new(
            capabilities.supports_operation(verbs::WATCH),
            capabilities.supports_operation(verbs::GET),
            capabilities.supports_operation(verbs::DELETE),
        )
        .with_scale(capabilities.subresources.iter().any(|(subresource, _)| {
            subresource.plural.ends_with("/scale") || subresource.plural == "scale"
        })),
    ))
}

fn sort_and_dedup(resources: &mut Vec<ResourceDescriptor>) {
    resources.sort_unstable_by(compare_resources);
    resources.dedup_by(|left, right| {
        left.group == right.group && left.version == right.version && left.plural == right.plural
    });
}

fn compare_resources(left: &ResourceDescriptor, right: &ResourceDescriptor) -> Ordering {
    left.plural
        .cmp(&right.plural)
        .then_with(|| left.group.cmp(&right.group))
        .then_with(|| left.version.cmp(&right.version))
        .then_with(|| left.kind.cmp(&right.kind))
        .then_with(|| left.api_version.cmp(&right.api_version))
        .then_with(|| left.namespaced.cmp(&right.namespaced))
        .then_with(|| {
            right
                .operations
                .watchable()
                .cmp(&left.operations.watchable())
        })
        .then_with(|| right.operations.gettable().cmp(&left.operations.gettable()))
        .then_with(|| {
            right
                .operations
                .deletable()
                .cmp(&left.operations.deletable())
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn api_resource(group: &str, version: &str, kind: &str, plural: &str) -> ApiResource {
        ApiResource {
            group: group.to_owned(),
            version: version.to_owned(),
            api_version: if group.is_empty() {
                version.to_owned()
            } else {
                format!("{group}/{version}")
            },
            kind: kind.to_owned(),
            plural: plural.to_owned(),
        }
    }

    fn capabilities(scope: Scope, operations: &[&str]) -> ApiCapabilities {
        ApiCapabilities {
            scope,
            subresources: Vec::new(),
            operations: operations.iter().map(ToString::to_string).collect(),
        }
    }

    #[test]
    fn aggregated_discovery_preserves_crd_short_names() {
        let resource = descriptor_from_aggregated_resource(
            "config.streamnative.io",
            "v1alpha1",
            kube::core::discovery::v2::APIResourceDiscovery {
                resource: Some("cloudenvironmentbackendconfigs".to_owned()),
                response_kind: Some(kube::core::discovery::v2::GroupVersionKind {
                    group: Some("config.streamnative.io".to_owned()),
                    version: Some("v1alpha1".to_owned()),
                    kind: Some("CloudEnvironmentBackendConfig".to_owned()),
                }),
                scope: Some("Namespaced".to_owned()),
                verbs: vec![
                    verbs::GET.to_owned(),
                    verbs::LIST.to_owned(),
                    verbs::WATCH.to_owned(),
                ],
                short_names: vec!["cebc".to_owned()],
                ..Default::default()
            },
        )
        .expect("listable CRD");

        assert_eq!(resource.short_names.as_ref(), [Arc::<str>::from("cebc")]);
        assert!(
            resource
                .command_aliases()
                .iter()
                .any(|alias| alias == "cebc")
        );
    }

    #[test]
    fn aggregated_discovery_preserves_scale_subresource_for_custom_resources() {
        let resource: APIResourceDiscovery = serde_json::from_value(serde_json::json!({
            "resource": "widgets", "responseKind": {"group": "example.io", "version": "v1", "kind": "Widget"},
            "scope": "Namespaced", "verbs": ["get", "list", "watch"],
            "subresources": [{"subresource": "scale", "verbs": ["get", "patch", "update"]}]
        })).unwrap();
        let descriptor = descriptor_from_aggregated_resource("example.io", "v1", resource).unwrap();
        assert!(descriptor.is_scalable());
    }

    #[test]
    fn legacy_discovery_pairs_scale_with_parent_without_listing_subresources() {
        let list: APIResourceList = serde_json::from_value(serde_json::json!({
            "apiVersion": "v1", "kind": "APIResourceList", "groupVersion": "example.io/v1",
            "resources": [
                {"name": "widgets", "singularName": "widget", "namespaced": false, "kind": "Widget", "verbs": ["list", "get"]},
                {"name": "widgets/scale", "singularName": "", "namespaced": false, "kind": "Scale", "verbs": ["get", "patch"]},
                {"name": "others", "singularName": "other", "namespaced": false, "kind": "Other", "verbs": ["list", "get"]}
            ]
        })).unwrap();
        let descriptors = descriptors_from_legacy_list(list);
        assert_eq!(descriptors.len(), 2);
        assert!(descriptors[0].is_scalable());
        assert!(!descriptors[1].is_scalable());
    }

    #[test]
    fn legacy_discovery_preserves_crd_short_names() {
        let resource = descriptor_from_legacy_resource(
            "config.streamnative.io/v1alpha1",
            k8s_openapi::apimachinery::pkg::apis::meta::v1::APIResource {
                kind: "CloudEnvironmentBackendConfig".to_owned(),
                name: "cloudenvironmentbackendconfigs".to_owned(),
                namespaced: true,
                short_names: Some(vec!["cebc".to_owned()]),
                verbs: vec![
                    verbs::GET.to_owned(),
                    verbs::LIST.to_owned(),
                    verbs::WATCH.to_owned(),
                ],
                ..Default::default()
            },
        )
        .expect("listable CRD");

        assert_eq!(resource.short_names.as_ref(), [Arc::<str>::from("cebc")]);
        assert!(
            resource
                .command_aliases()
                .iter()
                .any(|alias| alias == "cebc")
        );
    }

    #[test]
    fn converts_scope_and_supported_operations() {
        let resource = descriptor(
            api_resource("example.io", "v1", "Widget", "widgets"),
            &capabilities(
                Scope::Namespaced,
                &[verbs::GET, verbs::LIST, verbs::WATCH, verbs::DELETE],
            ),
        )
        .expect("listable resource");

        assert_eq!(resource.group.as_ref(), "example.io");
        assert_eq!(resource.version.as_ref(), "v1");
        assert_eq!(resource.api_version.as_ref(), "example.io/v1");
        assert_eq!(resource.kind.as_ref(), "Widget");
        assert_eq!(resource.plural.as_ref(), "widgets");
        assert!(resource.namespaced);
        assert!(resource.operations.watchable());
        assert!(resource.operations.gettable());
        assert!(resource.operations.deletable());
    }

    #[test]
    fn keeps_list_only_resources_and_rejects_non_listable_resources() {
        let list_only = descriptor(
            api_resource("metrics.k8s.io", "v1beta1", "NodeMetrics", "nodes"),
            &capabilities(Scope::Cluster, &[verbs::GET, verbs::LIST]),
        )
        .expect("list-only aggregated resource");
        assert!(!list_only.namespaced);
        assert!(!list_only.operations.watchable());
        assert!(list_only.operations.gettable());
        assert!(!list_only.operations.deletable());

        assert!(
            descriptor(
                api_resource("authentication.k8s.io", "v1", "TokenReview", "tokenreviews"),
                &capabilities(Scope::Cluster, &[verbs::CREATE]),
            )
            .is_none()
        );
    }

    #[test]
    fn sorts_by_qualified_name_and_deduplicates_exact_resource_versions() {
        let caps = capabilities(Scope::Namespaced, &[verbs::GET, verbs::LIST, verbs::WATCH]);
        let mut resources = vec![
            descriptor(
                api_resource("z.example.io", "v1", "Widget", "widgets"),
                &caps,
            )
            .unwrap(),
            descriptor(api_resource("", "v1", "Pod", "pods"), &caps).unwrap(),
            descriptor(
                api_resource("a.example.io", "v1", "Widget", "widgets"),
                &caps,
            )
            .unwrap(),
            descriptor(
                api_resource("a.example.io", "v1", "Widget", "widgets"),
                &caps,
            )
            .unwrap(),
        ];

        sort_and_dedup(&mut resources);

        let names: Vec<_> = resources
            .iter()
            .map(ResourceDescriptor::qualified_name)
            .collect();
        assert_eq!(
            names,
            ["pods", "widgets.a.example.io", "widgets.z.example.io"]
        );
    }

    #[test]
    fn retains_the_most_stable_version_of_each_discovered_kind() {
        let caps = capabilities(Scope::Namespaced, &[verbs::GET, verbs::LIST, verbs::WATCH]);
        let mut resources = vec![
            descriptor(
                api_resource("example.io", "v3alpha1", "Widget", "widgets"),
                &caps,
            )
            .unwrap(),
            descriptor(
                api_resource("example.io", "v2beta1", "Widget", "widgets"),
                &caps,
            )
            .unwrap(),
            descriptor(api_resource("example.io", "v1", "Widget", "widgets"), &caps).unwrap(),
        ];

        retain_most_stable_versions(&mut resources);

        assert_eq!(resources.len(), 1);
        assert_eq!(resources[0].version.as_ref(), "v1");
    }

    #[tokio::test]
    #[ignore = "requires access to a Kubernetes cluster"]
    async fn live_cluster_discovers_core_crd_and_aggregated_resources() {
        let client = Client::try_default().await.expect("Kubernetes client");
        let resources = discover_resources(client).await.expect("API discovery");
        assert!(
            resources
                .iter()
                .any(|resource| resource.qualified_name() == "secrets")
        );
        assert!(resources.iter().any(|resource| {
            resource.qualified_name() == "nodes.metrics.k8s.io" && !resource.operations.watchable()
        }));
        assert!(resources.iter().any(|resource| {
            !resource.group.is_empty()
                && resource.group.as_ref() != "metrics.k8s.io"
                && resource.known_kind().is_none()
        }));
        assert!(
            resources
                .iter()
                .any(|resource| !resource.short_names.is_empty())
        );
    }
}
