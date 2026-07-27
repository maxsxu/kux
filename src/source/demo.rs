use std::{sync::Arc, time::Instant};

use tokio::sync::{mpsc, watch};

use crate::{
    app::{ActionIntent, ContentKind, ResourceTarget},
    model::{
        ChildResourceData, ContainerData, NamespaceInfo, NodeData, OwnerReferenceData, PodData,
        PolicyData, ResourceDescriptor, ResourceKind, ResourceOperations, ResourceRow,
        ResourceSnapshot, SourceState, unix_now,
    },
};

use super::{ActionResult, WatchRequest};

pub async fn run_manager(
    item_count: usize,
    mut requests: mpsc::Receiver<WatchRequest>,
    snapshots: watch::Sender<ResourceSnapshot>,
    mut actions: mpsc::Receiver<ActionIntent>,
    action_results: mpsc::Sender<ActionResult>,
) {
    let _ = action_results
        .send(ActionResult::NamespaceOptions {
            context: "demo".to_owned(),
            namespaces: (0..250)
                .map(|index| NamespaceInfo::discovered(format!("team-{index:03}"), "Active", None))
                .collect(),
        })
        .await;
    let _ = action_results
        .send(ActionResult::ServerVersion("v1.32.0-demo".to_owned()))
        .await;
    let mut running: Option<tokio::task::JoinHandle<()>> = None;
    loop {
        tokio::select! {
            Some(request) = requests.recv() => {
                if let Some(task) = running.take() {
                    task.abort();
                }
                let snapshots = snapshots.clone();
                running = Some(tokio::spawn(async move {
                    let _ = snapshots.send(ResourceSnapshot::loading(
                        request.generation,
                        request.kind.clone(),
                    ));
                    let generation = request.generation;
                    let kind = request.kind.clone();
                    let built = tokio::task::spawn_blocking(move || build_rows(item_count, &request)).await;
                    if let Ok(rows) = built {
                        let _ = snapshots.send(ResourceSnapshot {
                            generation,
                            kind,
                            rows: Arc::new(rows),
                            table_columns: Arc::from([]),
                            state: SourceState::Ready,
                            revision: 1,
                            emitted_at: Instant::now(),
                        });
                    }
                }));
            }
            Some(action) = actions.recv() => {
                let result = demo_action(action);
                let _ = action_results.send(result).await;
            }
            else => break,
        }
    }
    if let Some(task) = running {
        task.abort();
    }
}

fn demo_action(action: ActionIntent) -> ActionResult {
    match action {
        ActionIntent::Delete { targets, immediate } => demo_delete(&targets, immediate),
        ActionIntent::DeleteContext { name } => ActionResult::Error(format!(
            "Context deletion is unavailable in demo mode: {name}"
        )),
        ActionIntent::DrainNode { name, .. } => {
            ActionResult::Notice(format!("Drained demo node {name}"))
        }
        ActionIntent::Content { target, content } => demo_content(&target, content),
        ActionIntent::Containers { target } => {
            let namespace = target
                .namespace
                .clone()
                .unwrap_or_else(|| "default".to_owned());
            let now = unix_now();
            let rows = vec![
                Arc::new(
                    ResourceRow::new(
                        namespace.clone(),
                        "app",
                        "Running",
                        "true",
                        "ghcr.io/example/kux-demo:v1.0.0",
                        now - 180,
                    )
                    .with_container_data(ContainerData {
                        image: Arc::from("ghcr.io/example/kux-demo:v1.0.0"),
                        kind: Arc::from("container"),
                        restarts: 0,
                    }),
                ),
                Arc::new(
                    ResourceRow::new(
                        namespace,
                        "sidecar",
                        "Running",
                        "true",
                        "ghcr.io/example/metrics-sidecar:v0.8.2",
                        now - 180,
                    )
                    .with_container_data(ContainerData {
                        image: Arc::from("ghcr.io/example/metrics-sidecar:v0.8.2"),
                        kind: Arc::from("container"),
                        restarts: 1,
                    }),
                ),
            ];
            ActionResult::Containers { target, rows }
        }
        ActionIntent::ContainerLogs { target, previous } => {
            let label = if previous { "Previous Logs" } else { "Logs" };
            ActionResult::Content {
                title: format!(
                    "{label}({}/{}/{})",
                    target.namespace, target.pod, target.container
                ),
                body: format!(
                    "2026-07-17T10:00:00Z {} started\n2026-07-17T10:00:01Z demo log output",
                    target.container
                ),
                managed_fields: None,
                decoded_body: None,
                decoded_managed_fields: None,
                content: if previous {
                    ContentKind::PreviousLogs
                } else {
                    ContentKind::Logs
                },
            }
        }
        ActionIntent::AuthorizationPolicy { target } => {
            let title = target.namespace.as_deref().map_or_else(
                || format!("rbac({}:{})", target.kind.kind, target.name),
                |namespace| format!("policy({}:{namespace}/{})", target.kind.kind, target.name),
            );
            let row = ResourceRow::new(
                target.namespace.as_deref().unwrap_or("-"),
                "pods",
                "core",
                "RO:demo-reader",
                "",
                0,
            )
            .with_policy_data(PolicyData {
                verbs: ["get", "list", "watch"]
                    .into_iter()
                    .map(Arc::from)
                    .collect(),
            });
            ActionResult::Policy {
                target,
                title,
                rows: vec![Arc::new(row)],
            }
        }
    }
}

fn demo_delete(targets: &[ResourceTarget], immediate: bool) -> ActionResult {
    let action = if immediate { "kill" } else { "delete" };
    let notice = if let [target] = targets {
        format!(
            "Demo: would {action} {}/{}",
            target.namespace.as_deref().unwrap_or("cluster"),
            target.name
        )
    } else {
        format!(
            "Demo: would {action} all {} selected resources",
            targets.len()
        )
    };
    ActionResult::Notice(notice)
}

fn demo_content(target: &ResourceTarget, content: ContentKind) -> ActionResult {
    let title = match content {
        ContentKind::Yaml => target.namespace.as_deref().map_or_else(
            || format!("YAML({})", target.name),
            |namespace| format!("YAML({namespace}/{})", target.name),
        ),
        ContentKind::SecretData => target.namespace.as_deref().map_or_else(
            || format!("Secret({})", target.name),
            |namespace| format!("Secret({namespace}/{})", target.name),
        ),
        ContentKind::Describe => "Describe".to_owned(),
        ContentKind::Logs => "Logs".to_owned(),
        ContentKind::PreviousLogs => "Previous Logs".to_owned(),
    };
    let body = if content == ContentKind::Describe {
        format!(
            "Name:             {}\nNamespace:        {}\nLabels:           app=kux-demo\nAnnotations:      <none>\nStatus:           Running\nContainers:\n  kux-demo:\n    Image:        ghcr.io/example/kux-demo:latest\n    Ready:        True\nEvents:\n  Type    Reason   Age   From      Message\n  Normal  Started  1m    kubelet   Started container kux-demo",
            target.name,
            target.namespace.as_deref().unwrap_or("-")
        )
    } else if content == ContentKind::SecretData {
        "password: demo-secret\ntoken: demo-token".to_owned()
    } else {
        format!(
            "# Demo content\nkind: {}\nmetadata:\n  name: {}\n  namespace: {}\nspec:\n  enabled: true\nstatus:\n  phase: generated",
            target.kind,
            target.name,
            target.namespace.as_deref().unwrap_or("-")
        )
    };
    ActionResult::Content {
        title: if matches!(content, ContentKind::Yaml | ContentKind::SecretData) {
            title
        } else {
            format!("{title} · {}", target.name)
        },
        body,
        managed_fields: (content == ContentKind::Yaml).then(|| {
            format!(
                "# Demo content\nkind: {}\nmetadata:\n  name: {}\n  namespace: {}\n  managedFields:\n    - manager: kux-demo\n      operation: Apply\nspec:\n  enabled: true\nstatus:\n  phase: generated",
                target.kind,
                target.name,
                target.namespace.as_deref().unwrap_or("-")
            )
        }),
        decoded_body: None,
        decoded_managed_fields: None,
        content,
    }
}

fn build_rows(item_count: usize, request: &WatchRequest) -> Vec<Arc<ResourceRow>> {
    let now = unix_now();
    let prefix = request.kind.short_label();
    let known_kind = request.kind.known_kind();
    let mut rows = Vec::with_capacity(item_count);
    for index in 0..item_count {
        let namespace = format!("team-{:03}", index % 250);
        if request
            .namespace
            .as_deref()
            .is_some_and(|wanted| wanted != namespace)
        {
            continue;
        }
        let node = format!("node-{:07}", index % 100);
        if request
            .field_selector
            .as_deref()
            .and_then(|selector| selector.strip_prefix("spec.nodeName="))
            .is_some_and(|wanted| wanted != node)
        {
            continue;
        }
        let name = if known_kind == Some(ResourceKind::Nodes) {
            format!("node-{index:07}")
        } else {
            format!("{prefix}-{index:07}")
        };
        let (status, ready) = if known_kind == Some(ResourceKind::Nodes) {
            if index % 101 == 0 {
                ("NotReady,SchedulingDisabled", "worker")
            } else if index % 17 == 0 {
                ("Ready,SchedulingDisabled", "worker")
            } else {
                ("Ready", "worker")
            }
        } else if index % 101 == 0 {
            ("CrashLoopBackOff", "0/1")
        } else if index % 37 == 0 {
            ("Pending", "0/1")
        } else {
            ("Running", "1/1")
        };
        let labels = request
            .label_selector
            .clone()
            .unwrap_or_else(|| format!("app={prefix},team=team-{:03}", index % 250));
        let row = ResourceRow::new(
            if request.kind.is_namespaced() {
                namespace
            } else {
                "-".to_owned()
            },
            name,
            status,
            ready,
            (index % 13).to_string(),
            now - i64::try_from(index % (86400 * 365)).unwrap_or(i64::MAX),
        )
        .with_labels(labels.clone());
        let row = match known_kind {
            Some(ResourceKind::Pods) => row.with_pod_data(PodData {
                restarts: u32::try_from(index % 13).unwrap_or(0),
                node: Arc::from(node),
                cpu_millis: Some(u64::try_from(index % 500 + 1).unwrap_or(1)),
                cpu_request_millis: 500,
                cpu_limit_millis: 1_000,
                memory_bytes: Some(
                    u64::try_from((index % 512 + 32) * 1_048_576).unwrap_or(u64::MAX),
                ),
                memory_request_bytes: 512 * 1_048_576,
                memory_limit_bytes: 1_024 * 1_048_576,
            }),
            Some(ResourceKind::Nodes) => row.with_node_data(NodeData {
                taints: Arc::from(if index % 10 == 0 {
                    "workload=system:NoSchedule"
                } else {
                    "<none>"
                }),
                taint_count: u32::from(index % 10 == 0),
                version: Arc::from("v1.32.0-demo"),
                pods: u32::try_from(index % 110).unwrap_or(0),
                cpu_millis: Some(u64::try_from(index % 3_000 + 100).unwrap_or(100)),
                cpu_allocatable_millis: 4_000,
                memory_bytes: Some(
                    u64::try_from((index % 16_384 + 1_024) * 1_048_576).unwrap_or(u64::MAX),
                ),
                memory_allocatable_bytes: 32_768 * 1_048_576,
            }),
            _ => row,
        };
        let row = with_demo_child(row, request, labels);
        let row = with_demo_owner(row, request, index);
        rows.push(Arc::new(row));
    }
    rows.sort_unstable_by(|left, right| {
        left.namespace
            .cmp(&right.namespace)
            .then(left.name.cmp(&right.name))
    });
    rows
}

fn with_demo_owner(row: ResourceRow, request: &WatchRequest, index: usize) -> ResourceRow {
    if request.kind.is_apps_v1_replica_set() {
        row.with_owner_reference_data(OwnerReferenceData {
            api_version: Arc::from("apps/v1"),
            kind: Arc::from("Deployment"),
            name: Arc::from(format!("deploy-{index:07}")),
        })
    } else {
        row
    }
}

fn with_demo_child(row: ResourceRow, request: &WatchRequest, labels: String) -> ResourceRow {
    let child_kind = match request.kind.known_kind() {
        Some(ResourceKind::Deployments) => Some(replica_set_descriptor()),
        Some(ResourceKind::StatefulSets | ResourceKind::DaemonSets | ResourceKind::Services) => {
            Some(ResourceKind::Pods.into())
        }
        _ if request.kind.is_apps_v1_replica_set() => Some(ResourceKind::Pods.into()),
        _ => None,
    };
    match child_kind {
        Some(kind) => row.with_child_resource_data(ChildResourceData {
            kind,
            label_selector: Arc::from(labels),
        }),
        None => row,
    }
}

fn replica_set_descriptor() -> ResourceDescriptor {
    ResourceDescriptor::new(
        "apps",
        "v1",
        "apps/v1",
        "ReplicaSet",
        "replicasets",
        true,
        ResourceOperations::all(),
    )
    .with_short_names(["rs"])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn demo_builder_produces_requested_cardinality() {
        let rows = build_rows(
            100_000,
            &WatchRequest {
                generation: 1,
                context: "demo".to_owned(),
                kind: ResourceDescriptor::from(ResourceKind::Pods),
                namespace: None,
                field_selector: None,
                label_selector: None,
            },
        );
        assert_eq!(rows.len(), 100_000);
        assert!(rows.windows(2).all(|pair| pair[0].key <= pair[1].key));
    }

    #[test]
    fn demo_builder_applies_namespace_scope() {
        let rows = build_rows(
            1_000,
            &WatchRequest {
                generation: 1,
                context: "demo".to_owned(),
                kind: ResourceDescriptor::from(ResourceKind::Pods),
                namespace: Some("team-007".to_owned()),
                field_selector: None,
                label_selector: None,
            },
        );
        assert_eq!(rows.len(), 4);
        assert!(rows.iter().all(|row| row.namespace.as_ref() == "team-007"));
    }

    #[test]
    fn demo_builder_applies_node_field_selector() {
        let rows = build_rows(
            1_000,
            &WatchRequest {
                generation: 1,
                context: "demo".to_owned(),
                kind: ResourceDescriptor::from(ResourceKind::Pods),
                namespace: None,
                field_selector: Some("spec.nodeName=node-0000007".to_owned()),
                label_selector: None,
            },
        );
        assert_eq!(rows.len(), 10);
        assert!(rows.iter().all(|row| {
            row.pod_data()
                .is_some_and(|data| data.node.as_ref() == "node-0000007")
        }));
    }

    #[test]
    fn demo_delete_reports_the_full_selected_batch() {
        let target = crate::app::ResourceTarget {
            kind: ResourceDescriptor::from(ResourceKind::Pods),
            namespace: Some("team-007".to_owned()),
            name: "po-0000007".to_owned(),
        };
        assert_eq!(
            demo_action(ActionIntent::Delete {
                targets: vec![
                    target.clone(),
                    crate::app::ResourceTarget {
                        name: "po-0000008".to_owned(),
                        ..target
                    },
                ],
                immediate: false,
            }),
            ActionResult::Notice("Demo: would delete all 2 selected resources".to_owned())
        );
    }

    #[test]
    fn demo_actions_return_container_rows_and_selected_logs() {
        let target = crate::app::ResourceTarget {
            kind: ResourceDescriptor::from(ResourceKind::Pods),
            namespace: Some("team-007".to_owned()),
            name: "po-0000007".to_owned(),
        };
        let ActionResult::Containers { rows, .. } = demo_action(ActionIntent::Containers {
            target: target.clone(),
        }) else {
            panic!("expected container rows");
        };
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].namespace.as_ref(), "team-007");
        assert!(rows.iter().all(|row| row.container_data().is_some()));

        let ActionResult::Content {
            title,
            body,
            content,
            ..
        } = demo_action(ActionIntent::ContainerLogs {
            target: crate::app::ContainerTarget {
                namespace: "team-007".to_owned(),
                pod: target.name,
                container: "sidecar".to_owned(),
            },
            previous: false,
        })
        else {
            panic!("expected container logs");
        };
        assert_eq!(title, "Logs(team-007/po-0000007/sidecar)");
        assert!(body.contains("sidecar"));
        assert_eq!(content, ContentKind::Logs);
    }

    #[test]
    fn demo_yaml_titles_use_resource_identity() {
        let namespaced = crate::app::ResourceTarget {
            kind: ResourceDescriptor::from(ResourceKind::Pods),
            namespace: Some("team-007".to_owned()),
            name: "po-0000007".to_owned(),
        };
        let cluster = crate::app::ResourceTarget {
            kind: ResourceDescriptor::from(ResourceKind::Nodes),
            namespace: None,
            name: "node-007".to_owned(),
        };
        for (target, expected) in [
            (namespaced, "YAML(team-007/po-0000007)"),
            (cluster, "YAML(node-007)"),
        ] {
            let ActionResult::Content { title, .. } = demo_action(ActionIntent::Content {
                target,
                content: ContentKind::Yaml,
            }) else {
                panic!("expected YAML content");
            };
            assert_eq!(title, expected);
        }
    }
}
