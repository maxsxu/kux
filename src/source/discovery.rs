use std::{cmp::Ordering, sync::Arc};

use kube::{
    Client,
    core::ApiResource,
    discovery::{ApiCapabilities, Discovery, Scope, verbs},
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
    let discovery = match Discovery::new(client.clone()).run_aggregated().await {
        Ok(discovery) => discovery,
        Err(aggregated_error) => Discovery::new(client).run().await.map_err(|legacy_error| {
            format!(
                "Kubernetes API discovery failed (aggregated: {aggregated_error}; legacy: \
                 {legacy_error})"
            )
        })?,
    };

    let mut resources = discovery
        .groups_alphabetical()
        .into_iter()
        .flat_map(kube::discovery::ApiGroup::resources_by_stability)
        .filter_map(|(resource, capabilities)| descriptor(resource, &capabilities))
        .collect();
    sort_and_dedup(&mut resources);
    Ok(resources)
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
        ),
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
    }
}
