use std::{sync::Arc, time::Instant};

use tokio::sync::{mpsc, watch};

use crate::{
    app::{ActionIntent, ContentKind, ResourceTarget},
    model::{
        ChildResourceData, ContainerData, NamespaceInfo, NodeData, OwnerReferenceData, PodData,
        PolicyData, PortForwardPort, ResourceDescriptor, ResourceKind, ResourceRow,
        ResourceSnapshot, SourceState, unix_now,
    },
};

use super::{ActionRequest, ActionResult, WatchRequest, logs::LogTask};

pub async fn run_manager(
    item_count: usize,
    mut requests: mpsc::Receiver<WatchRequest>,
    snapshots: watch::Sender<ResourceSnapshot>,
    mut actions: mpsc::Receiver<ActionRequest>,
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
    let _ = action_results
        .send(ActionResult::ClusterMetrics {
            cpu: "31%".to_owned(),
            mem: "25%".to_owned(),
        })
        .await;
    let _ = action_results
        .send(ActionResult::ResourceOptions {
            context: "demo".to_owned(),
            resources: demo_resource_options(),
        })
        .await;
    let mut running: Option<tokio::task::JoinHandle<()>> = None;
    let mut running_logs: Option<LogTask> = None;
    loop {
        tokio::select! {
            Some(request) = requests.recv() => {
                running_logs.take();
                if let Some(task) = running.take() {
                    task.abort();
                }
                running = Some(tokio::spawn(publish_demo_view(
                    item_count, request, snapshots.clone(),
                )));
            }
            Some(action) = actions.recv() => {
                match &action.intent {
                    ActionIntent::StartLogs { request } => {
                        running_logs = Some(LogTask::spawn_demo(
                            action.generation,
                            request.clone(),
                            action_results.clone(),
                        ));
                        continue;
                    }
                    ActionIntent::StopLogs { id } => {
                        LogTask::stop_if_matching(&mut running_logs, *id);
                        continue;
                    }
                    ActionIntent::StopContentRefresh => continue,
                    _ => {}
                }
                let cancelable = action.intent.cancelable_read();
                let scoped = cancelable || matches!(&action.intent, ActionIntent::SetImages { .. } | ActionIntent::SanitizePods { .. } | ActionIntent::Rollback { .. });
                if cancelable {
                    running_logs.take();
                }
                let result = demo_action(action.intent);
                let result = if scoped {
                    ActionResult::Scoped {
                        generation: action.generation,
                        result: Box::new(result),
                    }
                } else {
                    result
                };
                let _ = action_results.send(result).await;
            }
            () = snapshots.closed() => break,
            else => break,
        }
    }
    if let Some(task) = running {
        task.abort();
    }
}

async fn publish_demo_view(
    item_count: usize,
    request: WatchRequest,
    snapshots: watch::Sender<ResourceSnapshot>,
) {
    let _ = snapshots.send(ResourceSnapshot::loading(
        request.generation,
        request.kind.clone(),
    ));
    let generation = request.generation;
    let kind = request.kind.clone();
    let instance = request.instance.clone();
    let table_columns = if kind.is_workloads() {
        super::kubernetes::resource_views::workload_columns()
    } else if kind.is_subject() {
        super::kubernetes::resource_views::subject_columns()
    } else if kind.is_helm() {
        super::kubernetes::resource_views::helm_columns()
    } else if kind.is_helm_history() {
        super::kubernetes::resource_views::helm_history_columns()
    } else if kind.is_dir() {
        super::kubernetes::resource_views::dir_columns()
    } else if kind.is_benchmarks() {
        super::kubernetes::resource_views::benchmark_columns()
    } else if kind.is_pulses() {
        super::kubernetes::resource_views::pulse_columns()
    } else if kind.is_xray() {
        super::kubernetes::xray::xray_columns()
    } else if kind.is_scans() {
        crate::scans::scan_columns()
    } else if kind.is_skins() {
        super::kubernetes::resource_views::skin_columns()
    } else {
        super::kubernetes::typed_table_columns(&kind)
    };
    let built = tokio::task::spawn_blocking(move || build_rows(item_count, &request)).await;
    if let Ok(rows) = built {
        let missing_instance = instance.is_some() && rows.is_empty();
        let _ = snapshots.send(ResourceSnapshot {
            generation,
            kind,
            rows: Arc::new(rows),
            table_columns,
            state: if missing_instance {
                SourceState::Error(Arc::from("Selected demo object no longer exists"))
            } else {
                SourceState::Ready
            },
            revision: 1,
            emitted_at: Instant::now(),
        });
    }
}

fn demo_resource_options() -> Vec<ResourceDescriptor> {
    let mut resources: Vec<_> = ResourceKind::ALL.into_iter().map(Into::into).collect();
    resources.extend([
        ResourceDescriptor::workloads(),
        ResourceDescriptor::users(),
        ResourceDescriptor::groups(),
        ResourceDescriptor::helm(),
        ResourceDescriptor::helm_history(),
        ResourceDescriptor::dir(),
        ResourceDescriptor::benchmarks(),
        ResourceDescriptor::pulses(),
        ResourceDescriptor::xray(),
    ]);
    let additional = [
        ("", "ConfigMap", "configmaps", "cm", true),
        ("", "Secret", "secrets", "sec", true),
        (
            "",
            "PersistentVolumeClaim",
            "persistentvolumeclaims",
            "pvc",
            true,
        ),
        ("", "ServiceAccount", "serviceaccounts", "sa", true),
        (
            "scheduling.k8s.io",
            "PriorityClass",
            "priorityclasses",
            "pc",
            false,
        ),
        ("batch", "Job", "jobs", "job", true),
        ("batch", "CronJob", "cronjobs", "cj", true),
    ];
    resources.extend(additional.map(|(group, kind, plural, alias, namespaced)| {
        ResourceDescriptor::new(
            group,
            "v1",
            if group.is_empty() {
                "v1".to_owned()
            } else {
                format!("{group}/v1")
            },
            kind,
            plural,
            namespaced,
            crate::model::ResourceOperations::all(),
        )
        .with_short_names([alias])
    }));
    resources
}

fn demo_images(id: u64, target: ResourceTarget) -> ActionResult {
    ActionResult::Images {
        id,
        target: target.clone(),
        result: Ok(super::ImageSnapshot {
            target,
            uid: "demo-image-object".to_owned(),
            resource_version: "1".to_owned(),
            images: vec![
                super::ContainerImage {
                    name: "init".to_owned(),
                    image: "busybox:1.37".to_owned(),
                    init: true,
                },
                super::ContainerImage {
                    name: "main".to_owned(),
                    image: "nginx:1.27".to_owned(),
                    init: false,
                },
            ],
        }),
    }
}

fn demo_refresh_describe(id: u64, target: ResourceTarget) -> ActionResult {
    let ActionResult::Content { body, .. } = demo_content(&target, ContentKind::Describe) else {
        unreachable!("demo describe content");
    };
    ActionResult::RefreshedDescribe {
        id,
        target,
        result: Ok(body),
    }
}

#[allow(clippy::too_many_lines)]
fn demo_action(action: ActionIntent) -> ActionResult {
    match action {
        ActionIntent::LoadBatchNavigation { id, target } => demo_batch_navigation(id, target),
        ActionIntent::LoadReferences { id, target } => demo_references(id, target),
        ActionIntent::LoadDeploymentSelector {
            id,
            target,
            replica_sets,
        } => ActionResult::DeploymentSelector {
            id,
            target,
            replica_sets,
            result: Ok("app=demo".to_owned()),
        },
        ActionIntent::LoadRollback { id, target } => demo_rollback_snapshot(id, target),
        ActionIntent::Rollback { snapshot } => demo_rollback(&snapshot),
        ActionIntent::LoadTransferPod { id, target } => ActionResult::TransferPod {
            id,
            target,
            result: Ok(crate::transfer::TransferPod {
                containers: vec!["main".to_owned(), "sidecar".to_owned(), "debug".to_owned()],
            }),
        },
        ActionIntent::SanitizePods { namespace } => ActionResult::TransientNotice(format!(
            "Demo: would sanitize eligible Pods in {} (filters and marks ignored)",
            namespace.map_or_else(
                || "all namespaces".to_owned(),
                |value| format!("namespace {value}")
            ),
        )),
        ActionIntent::LoadImages { id, target } => demo_images(id, target),
        ActionIntent::SetImages { snapshot, images } => demo_update_images(snapshot, &images),
        ActionIntent::StartLogs { .. } | ActionIntent::StopLogs { .. } => {
            ActionResult::Error("Log sessions are handled by the source manager".to_owned())
        }
        ActionIntent::Delete { targets, immediate } => demo_delete(&targets, immediate),
        ActionIntent::DeleteContext { name } => ActionResult::Error(format!(
            "Context deletion is unavailable in demo mode: {name}"
        )),
        ActionIntent::RenameContext { old_name, new_name } => ActionResult::Error(format!(
            "Context rename is unavailable in demo mode: {old_name} -> {new_name}"
        )),
        ActionIntent::DrainNode { name, .. } => {
            ActionResult::Notice(format!("Drained demo node {name}"))
        }
        ActionIntent::SetNodeUnschedulable {
            name,
            unschedulable,
        } => demo_set_node_unschedulable(&name, unschedulable),
        ActionIntent::Restart { targets } => demo_restart(&targets),
        ActionIntent::Scale { targets, replicas } => demo_scale(&targets, replicas),
        ActionIntent::TriggerCronJob { targets } => demo_trigger_cronjobs(&targets),
        ActionIntent::ToggleCronJobSuspend { target } => demo_toggle_cronjob_suspend(&target),
        ActionIntent::SetSuspended { targets, suspended } => {
            demo_set_suspended(&targets, suspended)
        }
        ActionIntent::Content {
            id,
            target,
            content,
        } => ActionResult::OpenedContent {
            id,
            result: Box::new(demo_content(&target, content)),
            target,
        },
        ActionIntent::RefreshYaml {
            id,
            target,
            include_managed_fields,
        } => demo_refresh_yaml(id, target, include_managed_fields),
        ActionIntent::StopContentRefresh => {
            ActionResult::Notice("Auto-refresh is disabled".to_owned())
        }
        ActionIntent::RefreshDescribe { id, target } => demo_refresh_describe(id, target),
        ActionIntent::ManagedYaml {
            id,
            target,
            body,
            decoded_body,
        } => demo_managed_yaml(id, target, &body, decoded_body.as_deref()),
        ActionIntent::Containers { target } => demo_containers(target),
        ActionIntent::ContainerLogs { target, previous } => demo_container_logs(&target, previous),
        ActionIntent::AuthorizationPolicy { target } => demo_authorization_policy(target),
        ActionIntent::FindChildResource { target, .. } => {
            ActionResult::NoChildResource { parent: target }
        }
        ActionIntent::ResolveOwner { target } => ActionResult::Owner {
            target,
            owner: None,
        },
        ActionIntent::HelmRollback {
            namespace,
            release,
            revision,
        } => ActionResult::Notice(format!(
            "Rolled back Helm release {namespace}/{release} to revision {revision}"
        )),
        ActionIntent::DirApply { path } => ActionResult::Content {
            title: format!("Applied Manifest({path})"),
            body: format!("message:\n  applied {path}"),
            managed_fields: None,
            decoded_body: None,
            decoded_managed_fields: None,
            content: ContentKind::Yaml,
        },
        ActionIntent::DirDelete { path } => ActionResult::Content {
            title: format!("Deleted Manifest({path})"),
            body: format!("message:\n  deleted {path}"),
            managed_fields: None,
            decoded_body: None,
            decoded_managed_fields: None,
            content: ContentKind::Yaml,
        },
    }
}

fn demo_batch_navigation(id: u64, target: ResourceTarget) -> ActionResult {
    let result = if target.kind.is_cron_job() {
        Ok(super::BatchNavigationFilter::CronJobJobs {
            owner_name: target.name.clone(),
        })
    } else if target.kind.is_job() {
        Ok(super::BatchNavigationFilter::JobPods {
            label_selector: Some(format!("batch.kubernetes.io/job-name={}", target.name)),
        })
    } else {
        Err("Batch navigation requires a batch/v1 CronJob or Job".to_owned())
    };
    ActionResult::BatchNavigation { id, target, result }
}

fn demo_references(id: u64, target: ResourceTarget) -> ActionResult {
    let workloads = [
        ("apps", "apps/v1", "Deployment", "deployments"),
        ("apps", "apps/v1", "DaemonSet", "daemonsets"),
        ("apps", "apps/v1", "StatefulSet", "statefulsets"),
        ("batch", "batch/v1", "Job", "jobs"),
        ("batch", "batch/v1", "CronJob", "cronjobs"),
    ];
    let entries = workloads
        .into_iter()
        .enumerate()
        .map(|(index, (group, api_version, kind, plural))| {
            let kind = crate::model::ResourceDescriptor::new(
                group,
                "v1",
                api_version,
                kind,
                plural,
                true,
                crate::model::ResourceOperations::all(),
            );
            let namespace = target
                .namespace
                .clone()
                .unwrap_or_else(|| format!("team-{index:03}"));
            let index = namespace
                .strip_prefix("team-")
                .and_then(|value| value.parse::<usize>().ok())
                .unwrap_or(index);
            super::ReferenceEntry {
                target: ResourceTarget {
                    name: format!("{}-{index:07}", kind.short_label()),
                    kind,
                    namespace: Some(namespace),
                },
            }
        })
        .collect();
    ActionResult::References {
        id,
        target,
        result: Ok(super::ReferenceScan {
            entries,
            errors: Vec::new(),
            scanned: workloads.len(),
        }),
    }
}

fn demo_container_logs(target: &crate::app::ContainerTarget, previous: bool) -> ActionResult {
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

fn demo_rollback_snapshot(id: u64, target: ResourceTarget) -> ActionResult {
    ActionResult::RollbackLoaded {
        id,
        target: target.clone(),
        result: Ok(super::RollbackSnapshot {
            target,
            uid: "demo-replicaset-uid".to_owned(),
            resource_version: "1".to_owned(),
            deployment: "demo-deployment".to_owned(),
            deployment_uid: "demo-deployment-uid".to_owned(),
            deployment_resource_version: "2".to_owned(),
            revision: 1,
            template: serde_json::json!({
                "metadata": {"labels": {"app": "demo"}},
                "spec": {"containers": [{"name": "main", "image": "nginx:1.26"}]}
            }),
            annotations: std::collections::BTreeMap::from([(
                "kubernetes.io/change-cause".to_owned(),
                "demo revision 1".to_owned(),
            )]),
            already_matches: false,
        }),
    }
}

fn demo_rollback(snapshot: &super::RollbackSnapshot) -> ActionResult {
    ActionResult::RolledBack {
        result: Ok(format!(
            "Demo: would roll back Deployment {}/{} to ReplicaSet {} (revision {})",
            snapshot.target.namespace.as_deref().unwrap_or("default"),
            snapshot.deployment,
            snapshot.target.name,
            snapshot.revision,
        )),
    }
}

fn demo_update_images(
    snapshot: super::ImageSnapshot,
    images: &[super::ContainerImage],
) -> ActionResult {
    let notice = format!(
        "Demo: would update images on {} to {}",
        snapshot.target.name,
        images
            .iter()
            .map(|image| format!("{}={}", image.name, image.image))
            .collect::<Vec<_>>()
            .join(", "),
    );
    ActionResult::ImagesUpdated {
        target: snapshot.target,
        result: Ok(notice),
    }
}

fn demo_managed_yaml(
    id: u64,
    target: ResourceTarget,
    body: &str,
    decoded_body: Option<&str>,
) -> ActionResult {
    let insert = |body: &str| {
        super::kubernetes::insert_managed_fields_yaml(body, "managedFields:\n- manager: demo\n")
    };
    let result = insert(body).and_then(|body| Ok((body, decoded_body.map(insert).transpose()?)));
    ActionResult::ManagedYaml { id, target, result }
}

fn demo_scale(targets: &[ResourceTarget], replicas: i32) -> ActionResult {
    super::kubernetes::workload_actions::validate_scale_targets(targets, replicas).map_or_else(
        ActionResult::Error,
        |()| {
            ActionResult::Notice(format!(
                "Demo: would scale {} workload(s) to {replicas} replicas",
                targets.len()
            ))
        },
    )
}

fn demo_trigger_cronjobs(targets: &[ResourceTarget]) -> ActionResult {
    super::kubernetes::workload_actions::validate_cronjob_targets(targets)
        .map_or_else(ActionResult::Error, |()| {
            ActionResult::Notice(format!("Demo: would trigger {} CronJob(s)", targets.len()))
        })
}

fn demo_toggle_cronjob_suspend(target: &ResourceTarget) -> ActionResult {
    super::kubernetes::workload_actions::validate_cronjob_target(target).map_or_else(
        ActionResult::Error,
        |()| {
            ActionResult::Notice(format!(
                "Demo: would toggle suspension for CronJob {}/{}",
                target.namespace.as_deref().unwrap_or("default"),
                target.name
            ))
        },
    )
}

fn demo_set_suspended(targets: &[ResourceTarget], suspended: bool) -> ActionResult {
    super::kubernetes::workload_actions::validate_suspension_targets(targets).map_or_else(
        ActionResult::Error,
        |()| {
            let action = if suspended { "suspend" } else { "resume" };
            ActionResult::Notice(format!(
                "Demo: would {action} {} workload(s)",
                targets.len()
            ))
        },
    )
}

fn demo_authorization_policy(target: ResourceTarget) -> ActionResult {
    let title = if target.kind.is_subject() || target.kind.kind.as_ref() == "ServiceAccount" {
        target.namespace.as_deref().map_or_else(
            || format!("policy({}:{})", target.kind.kind, target.name),
            |namespace| format!("policy({}:{namespace}/{})", target.kind.kind, target.name),
        )
    } else {
        target.namespace.as_deref().map_or_else(
            || format!("rbac({}:{})", target.kind.kind, target.name),
            |namespace| format!("rbac({}:{namespace}/{})", target.kind.kind, target.name),
        )
    };
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

fn demo_containers(target: ResourceTarget) -> ActionResult {
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
                index: Arc::from("1"),
                init: Arc::from("×"),
                probes: Arc::from("on:on"),
                ports: Arc::from("http:8080"),
                cpu_millis: Some(15),
                cpu_request_millis: 100,
                cpu_limit_millis: 500,
                memory_bytes: Some(32 * 1_048_576),
                memory_request_bytes: 128 * 1_048_576,
                memory_limit_bytes: 512 * 1_048_576,
            })
            .with_port_forward_ports(vec![PortForwardPort {
                container: Some(Arc::from("app")),
                name: Arc::from("http"),
                port: 8080,
            }]),
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
                index: Arc::from("2"),
                init: Arc::from("×"),
                probes: Arc::from("off:on"),
                ports: Arc::from("metrics:9090"),
                cpu_millis: Some(5),
                cpu_request_millis: 50,
                cpu_limit_millis: 200,
                memory_bytes: Some(16 * 1_048_576),
                memory_request_bytes: 64 * 1_048_576,
                memory_limit_bytes: 256 * 1_048_576,
            }),
        ),
    ];
    ActionResult::Containers { target, rows }
}

fn demo_restart(targets: &[ResourceTarget]) -> ActionResult {
    ActionResult::Notice(if targets.len() == 1 {
        format!("Restart in progress for {}", targets[0].name)
    } else {
        format!("Restart in progress for {} demo workloads", targets.len())
    })
}

fn demo_set_node_unschedulable(name: &str, unschedulable: bool) -> ActionResult {
    let action = if unschedulable {
        "Cordoned"
    } else {
        "Uncordoned"
    };
    ActionResult::Notice(format!("{action} demo node {name}"))
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

fn demo_refresh_yaml(
    id: u64,
    target: ResourceTarget,
    include_managed_fields: bool,
) -> ActionResult {
    let ActionResult::Content {
        body,
        managed_fields,
        decoded_body,
        decoded_managed_fields,
        ..
    } = demo_content(&target, ContentKind::Yaml)
    else {
        unreachable!("demo YAML returns content");
    };
    let managed = |value: Option<String>| {
        value.map(|body| {
            if include_managed_fields {
                body
            } else {
                String::new()
            }
        })
    };
    ActionResult::RefreshedYaml {
        id,
        target,
        result: Ok(super::YamlContent {
            body,
            with_managed_fields: managed(managed_fields),
            decoded_body,
            decoded_with_managed_fields: managed(decoded_managed_fields),
        }),
    }
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
        ContentKind::HelmValues => target.namespace.as_deref().map_or_else(
            || format!("Values({})", target.name),
            |namespace| format!("Values({namespace}/{})", target.name),
        ),
        ContentKind::Describe => "Describe".to_owned(),
        ContentKind::Logs => "Logs".to_owned(),
        ContentKind::PreviousLogs => "Previous Logs".to_owned(),
        ContentKind::ScreenDump => {
            return ActionResult::Error("Screen dumps are local files".to_owned());
        }
    };
    let body = if content == ContentKind::Describe {
        format!(
            "Name:             {}\nNamespace:        {}\nLabels:           app=kux-demo\nAnnotations:      <none>\nStatus:           Running\nContainers:\n  kux-demo:\n    Image:        ghcr.io/example/kux-demo:latest\n    Ready:        True\nEvents:\n  Type    Reason   Age   From      Message\n  Normal  Started  1m    kubelet   Started container kux-demo",
            target.name,
            target.namespace.as_deref().unwrap_or("-")
        )
    } else if content == ContentKind::SecretData {
        "password: demo-secret\ntoken: demo-token".to_owned()
    } else if content == ContentKind::HelmValues {
        "replicaCount: 2\nimage:\n  tag: \"v1.2.0\"".to_owned()
    } else {
        format!(
            "# Demo content\nkind: {}\nmetadata:\n  name: {}\n  namespace: {}\nspec:\n  enabled: true\nstatus:\n  phase: generated",
            target.kind,
            target.name,
            target.namespace.as_deref().unwrap_or("-")
        )
    };
    ActionResult::Content {
        title: if matches!(
            content,
            ContentKind::Yaml | ContentKind::SecretData | ContentKind::HelmValues
        ) {
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
        decoded_body: (content == ContentKind::HelmValues).then(|| {
            "replicaCount: 2\nimage:\n  repository: nginx\n  tag: \"v1.2.0\"\n  pullPolicy: IfNotPresent\nservice:\n  type: ClusterIP\n  port: 80".to_owned()
        }),
        decoded_managed_fields: None,
        content,
    }
}

fn build_virtual_rows(item_count: usize, request: &WatchRequest) -> Option<Vec<Arc<ResourceRow>>> {
    if request.kind.is_workloads() {
        Some(build_workload_rows(item_count, request))
    } else if let Some(kind) = request.kind.subject_kind() {
        Some(build_subject_rows(kind))
    } else if request.kind.is_helm() {
        Some(build_helm_rows(request))
    } else if request.kind.is_helm_history() {
        Some(build_helm_history_rows(request))
    } else if request.kind.is_dir() {
        Some(build_dir_rows(request))
    } else if request.kind.is_benchmarks() {
        Some(build_benchmark_rows(request))
    } else if request.kind.is_pulses() {
        Some(build_pulse_rows(item_count, request))
    } else if request.kind.is_xray() {
        Some(super::kubernetes::xray::build_demo_xray_rows(request))
    } else if request.kind.is_scans() {
        Some(super::kubernetes::resource_views::read_scan_rows(
            request.field_selector.as_deref().unwrap_or_default(),
        ))
    } else if request.kind.is_skins() {
        Some(super::kubernetes::resource_views::read_skin_rows())
    } else {
        None
    }
}

fn build_rows(item_count: usize, request: &WatchRequest) -> Vec<Arc<ResourceRow>> {
    if request
        .instance
        .as_ref()
        .is_some_and(|target| target.kind != request.kind || target.kind.is_workloads())
    {
        return Vec::new();
    }
    if let Some(rows) = build_virtual_rows(item_count, request) {
        return rows;
    }
    let namespace_scope = request
        .instance
        .as_ref()
        .map_or(request.namespace.as_deref(), |target| {
            target.namespace.as_deref()
        });
    let now = unix_now();
    let prefix = request.kind.short_label();
    let known_kind = request.kind.known_kind();
    let mut rows = Vec::with_capacity(item_count);
    for index in 0..item_count {
        let namespace = format!("team-{:03}", index % 250);
        if namespace_scope.is_some_and(|wanted| wanted != namespace) {
            continue;
        }
        let node = format!("node-{:07}", index % 100);
        if request.instance.is_none()
            && request
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
        if request
            .instance
            .as_ref()
            .is_some_and(|target| target.name != name)
        {
            continue;
        }
        let (status, ready, unschedulable) = demo_status(known_kind, index);
        let labels = request
            .label_selector
            .clone()
            .unwrap_or_else(|| format!("app={prefix},team=team-{:03}", index % 250));
        let row = ResourceRow::new(
            if request.kind.is_namespaced() {
                namespace.clone()
            } else {
                "-".to_owned()
            },
            name.clone(),
            status,
            ready,
            (index % 13).to_string(),
            now - i64::try_from(index % (86400 * 365)).unwrap_or(i64::MAX),
        )
        .with_labels(labels.clone())
        .with_port_forward_ports(demo_port_forward_ports(known_kind))
        .with_raw_object(demo_raw_object(
            request, &name, &namespace, prefix, index, &node, status,
        ));
        let row = with_demo_kind_data(row, known_kind, index, node, unschedulable);
        let row = with_demo_child(row, request, labels);
        let row = with_demo_owner(row, request, index);
        if request.instance.is_some() || super::kubernetes::row_matches_owner_filter(&row, request)
        {
            rows.push(Arc::new(row));
        }
    }
    rows.sort_unstable_by(|left, right| {
        left.namespace
            .cmp(&right.namespace)
            .then(left.name.cmp(&right.name))
    });
    rows
}

fn demo_raw_object(
    request: &WatchRequest,
    name: &str,
    namespace: &str,
    prefix: &str,
    index: usize,
    node: &str,
    status: &str,
) -> serde_json::Value {
    serde_json::json!({
        "apiVersion": request.kind.api_version.as_ref(),
        "kind": request.kind.kind.as_ref(),
        "metadata": {
            "name": name,
            "namespace": if request.kind.is_namespaced() { serde_json::Value::String(namespace.to_owned()) } else { serde_json::Value::Null },
            "labels": {
                "app": prefix,
                "team": format!("team-{:03}", index % 250),
            },
        },
        "spec": {
            "nodeName": node,
            "containers": [
                {
                    "name": "app",
                    "image": "ghcr.io/example/kux-demo:v1.0.0",
                }
            ]
        },
        "status": {
            "phase": status,
            "podIP": format!("10.244.{}.{}", (index / 254) % 255, (index % 254) + 1),
            "loadBalancer": {
                "ingress": [
                    {
                        "ip": "127.0.0.1",
                        "hostname": "localhost"
                    }
                ]
            }
        }
    })
}

#[allow(clippy::too_many_lines)]
fn with_demo_kind_data(
    row: ResourceRow,
    known_kind: Option<ResourceKind>,
    index: usize,
    node: String,
    unschedulable: bool,
) -> ResourceRow {
    match known_kind {
        Some(ResourceKind::Pods) => row.with_pod_data(PodData {
            restarts: u32::try_from(index % 13).unwrap_or(0),
            node: Arc::from(node),
            default_container: Arc::from("app"),
            qos: Arc::from("Burstable"),
            ip: Arc::from(format!(
                "10.244.{}.{}",
                (index / 254) % 255,
                (index % 254) + 1
            )),
            nominated_node: Arc::from("<none>"),
            readiness_gates: Arc::from("<none>"),
            cpu_millis: Some(u64::try_from(index % 500 + 1).unwrap_or(1)),
            cpu_request_millis: 500,
            cpu_limit_millis: 1_000,
            memory_bytes: Some(u64::try_from((index % 512 + 32) * 1_048_576).unwrap_or(u64::MAX)),
            memory_request_bytes: 512 * 1_048_576,
            memory_limit_bytes: 1_024 * 1_048_576,
        }),
        Some(ResourceKind::Nodes) => row.with_node_data(NodeData {
            unschedulable,
            taints: Arc::from(if index.is_multiple_of(10) {
                "workload=system:NoSchedule"
            } else {
                "<none>"
            }),
            taint_count: u32::from(index.is_multiple_of(10)),
            version: Arc::from("v1.32.0-demo"),
            kernel: Arc::from("6.8.0-demo"),
            os_image: Arc::from("Ubuntu 24.04 LTS"),
            arch: Arc::from("linux/amd64"),
            runtime: Arc::from("containerd://1.7.20"),
            internal_ip: Arc::from(format!(
                "10.0.{}.{}",
                (index / 254) % 255,
                (index % 254) + 1
            )),
            external_ip: Arc::from("<none>"),
            pods: u32::try_from(index % 110).unwrap_or(0),
            cpu_millis: Some(u64::try_from(index % 3_000 + 100).unwrap_or(100)),
            cpu_allocatable_millis: 4_000,
            memory_bytes: Some(
                u64::try_from((index % 16_384 + 1_024) * 1_048_576).unwrap_or(u64::MAX),
            ),
            memory_allocatable_bytes: 32_768 * 1_048_576,
        }),
        Some(ResourceKind::Deployments) => {
            let counts = row.ready.split_once('/').unwrap_or(("1", "1"));
            row.clone().with_table_cells(vec![
                crate::model::TableCell {
                    text: Arc::clone(&row.name),
                    timestamp_unix: None,
                },
                crate::model::TableCell {
                    text: Arc::clone(&row.ready),
                    timestamp_unix: None,
                },
                crate::model::TableCell {
                    text: Arc::from(counts.0),
                    timestamp_unix: None,
                },
                crate::model::TableCell {
                    text: Arc::from(counts.0),
                    timestamp_unix: None,
                },
                crate::model::TableCell {
                    text: Arc::from("app"),
                    timestamp_unix: None,
                },
                crate::model::TableCell {
                    text: Arc::from("ghcr.io/example/kux-demo:v1.0.0"),
                    timestamp_unix: None,
                },
                crate::model::TableCell {
                    text: Arc::clone(&row.labels),
                    timestamp_unix: None,
                },
                crate::model::TableCell {
                    text: Arc::clone(&row.labels),
                    timestamp_unix: None,
                },
                crate::model::TableCell {
                    text: Arc::from(""),
                    timestamp_unix: None,
                },
                crate::model::TableCell {
                    text: Arc::from("1m"),
                    timestamp_unix: Some(row.created_unix),
                },
            ])
        }
        Some(ResourceKind::StatefulSets) => row.clone().with_table_cells(vec![
            crate::model::TableCell {
                text: Arc::clone(&row.name),
                timestamp_unix: None,
            },
            crate::model::TableCell {
                text: Arc::clone(&row.ready),
                timestamp_unix: None,
            },
            crate::model::TableCell {
                text: Arc::clone(&row.labels),
                timestamp_unix: None,
            },
            crate::model::TableCell {
                text: Arc::from("app"),
                timestamp_unix: None,
            },
            crate::model::TableCell {
                text: Arc::from("ghcr.io/example/kux-demo:v1.0.0"),
                timestamp_unix: None,
            },
            crate::model::TableCell {
                text: Arc::clone(&row.labels),
                timestamp_unix: None,
            },
            crate::model::TableCell {
                text: Arc::from(""),
                timestamp_unix: None,
            },
            crate::model::TableCell {
                text: Arc::from("1m"),
                timestamp_unix: Some(row.created_unix),
            },
        ]),
        Some(ResourceKind::DaemonSets) => {
            let counts = row.ready.split_once('/').unwrap_or(("1", "1"));
            row.clone().with_table_cells(vec![
                crate::model::TableCell {
                    text: Arc::clone(&row.name),
                    timestamp_unix: None,
                },
                crate::model::TableCell {
                    text: Arc::from(counts.1),
                    timestamp_unix: None,
                },
                crate::model::TableCell {
                    text: Arc::from(counts.1),
                    timestamp_unix: None,
                },
                crate::model::TableCell {
                    text: Arc::from(counts.0),
                    timestamp_unix: None,
                },
                crate::model::TableCell {
                    text: Arc::from(counts.0),
                    timestamp_unix: None,
                },
                crate::model::TableCell {
                    text: Arc::from(counts.0),
                    timestamp_unix: None,
                },
                crate::model::TableCell {
                    text: Arc::from(""),
                    timestamp_unix: None,
                },
                crate::model::TableCell {
                    text: Arc::from("app"),
                    timestamp_unix: None,
                },
                crate::model::TableCell {
                    text: Arc::from("ghcr.io/example/kux-demo:v1.0.0"),
                    timestamp_unix: None,
                },
                crate::model::TableCell {
                    text: Arc::clone(&row.labels),
                    timestamp_unix: None,
                },
                crate::model::TableCell {
                    text: Arc::clone(&row.labels),
                    timestamp_unix: None,
                },
                crate::model::TableCell {
                    text: Arc::from(""),
                    timestamp_unix: None,
                },
                crate::model::TableCell {
                    text: Arc::from("1m"),
                    timestamp_unix: Some(row.created_unix),
                },
            ])
        }
        Some(ResourceKind::Services) => {
            let cluster_ip = format!("10.96.{}.{}", (index / 254) % 255, (index % 254) + 1);
            row.clone().with_table_cells(vec![
                crate::model::TableCell {
                    text: Arc::clone(&row.name),
                    timestamp_unix: None,
                },
                crate::model::TableCell {
                    text: Arc::from("ClusterIP"),
                    timestamp_unix: None,
                },
                crate::model::TableCell {
                    text: Arc::from(cluster_ip),
                    timestamp_unix: None,
                },
                crate::model::TableCell {
                    text: Arc::from(""),
                    timestamp_unix: None,
                },
                crate::model::TableCell {
                    text: Arc::clone(&row.labels),
                    timestamp_unix: None,
                },
                crate::model::TableCell {
                    text: Arc::from("http:8080"),
                    timestamp_unix: None,
                },
                crate::model::TableCell {
                    text: Arc::clone(&row.labels),
                    timestamp_unix: None,
                },
                crate::model::TableCell {
                    text: Arc::from(""),
                    timestamp_unix: None,
                },
                crate::model::TableCell {
                    text: Arc::from("1m"),
                    timestamp_unix: Some(row.created_unix),
                },
            ])
        }
        _ => row,
    }
}

fn build_workload_rows(item_count: usize, request: &WatchRequest) -> Vec<Arc<ResourceRow>> {
    use super::kubernetes::resource_views::project_workload_row;
    use crate::model::{TableCell, TableColumn};
    let kinds: Vec<_> = [
        ResourceKind::Pods,
        ResourceKind::Services,
        ResourceKind::DaemonSets,
        ResourceKind::StatefulSets,
        ResourceKind::Deployments,
    ]
    .into_iter()
    .map(ResourceDescriptor::from)
    .chain(std::iter::once(ResourceDescriptor::new(
        "apps",
        "v1",
        "apps/v1",
        "ReplicaSet",
        "replicasets",
        true,
        crate::model::ResourceOperations::all(),
    )))
    .collect();
    let columns: Vec<_> = ["Name", "Ready", "Status", "Desired", "Age"]
        .into_iter()
        .map(|name| TableColumn {
            name: Arc::from(name),
            format: Arc::from(""),
            priority: 0,
        })
        .collect();
    let mut result = Vec::new();
    for kind in kinds {
        let child = WatchRequest {
            kind: kind.clone(),
            instance: None,
            ..request.clone()
        };
        for row in build_rows(item_count, &child) {
            let counts = row.ready.split_once('/').unwrap_or(("0", "0"));
            let ready = if kind.is_apps_v1_replica_set()
                || kind.known_kind() == Some(ResourceKind::DaemonSets)
            {
                counts.0
            } else {
                row.ready.as_ref()
            };
            let mut row = row.as_ref().clone();
            let cells = [row.name.as_ref(), ready, row.status.as_ref(), counts.1, ""]
                .into_iter()
                .map(|text| TableCell {
                    text: Arc::from(text),
                    timestamp_unix: None,
                })
                .collect();
            row = row.with_table_cells(cells);
            let projected = project_workload_row(&kind, &row, &columns)
                .expect("generated workload columns are valid");
            result.push(Arc::new(projected));
        }
    }
    result
}

fn build_subject_rows(kind: crate::model::SubjectKind) -> Vec<Arc<ResourceRow>> {
    use super::kubernetes::resource_views::project_subject_row;
    let entries = match kind {
        crate::model::SubjectKind::User => [
            ("alice@example.com", "RoleBinding", "team-000/developer"),
            (
                "system:kube-controller-manager",
                "ClusterRoleBinding",
                "system:kube-controller-manager",
            ),
            (
                "system:kube-scheduler",
                "ClusterRoleBinding",
                "system:kube-scheduler",
            ),
        ],
        crate::model::SubjectKind::Group => [
            (
                "system:authenticated",
                "ClusterRoleBinding",
                "system:public-info-viewer",
            ),
            ("system:masters", "ClusterRoleBinding", "cluster-admin"),
            ("team-developers", "RoleBinding", "team-000/developer"),
        ],
    };
    entries
        .into_iter()
        .map(|(name, binding_kind, location)| {
            Arc::new(project_subject_row(
                name.to_owned(),
                binding_kind,
                location.to_owned(),
            ))
        })
        .collect()
}

fn build_helm_rows(request: &WatchRequest) -> Vec<Arc<ResourceRow>> {
    let now = unix_now();
    let entries = [
        (
            "team-000",
            "ingress-nginx",
            "1",
            "deployed",
            "ingress-nginx-4.10.0",
            "1.10.0",
            now - 172_800,
        ),
        (
            "team-000",
            "redis",
            "3",
            "deployed",
            "redis-19.5.0",
            "7.2.5",
            now - 86_400,
        ),
        (
            "team-001",
            "payments-api",
            "2",
            "failed",
            "payments-api-0.4.2",
            "0.4.2",
            now - 3_600,
        ),
    ];
    entries
        .into_iter()
        .filter(|(ns, _, _, _, _, _, _)| {
            request
                .namespace
                .as_deref()
                .is_none_or(|wanted| wanted == *ns)
        })
        .map(
            |(namespace, name, revision, status, chart, app_version, updated_unix)| {
                Arc::new(super::kubernetes::resource_views::project_helm_row(
                    namespace.to_owned(),
                    name.to_owned(),
                    revision.to_owned(),
                    status.to_owned(),
                    chart.to_owned(),
                    app_version,
                    updated_unix,
                ))
            },
        )
        .collect()
}

fn build_helm_history_rows(request: &WatchRequest) -> Vec<Arc<ResourceRow>> {
    let now = unix_now();
    let selector = request
        .field_selector
        .as_deref()
        .unwrap_or("team-000/ingress-nginx");
    let (ns, release) = selector.split_once('/').unwrap_or((
        "team-000",
        selector.strip_prefix("release=").unwrap_or(selector),
    ));
    let entries = [
        (
            "3",
            "deployed",
            format!("{release}-1.2.0"),
            "1.2.0",
            "Upgrade complete",
            now - 86_400,
        ),
        (
            "2",
            "superseded",
            format!("{release}-1.1.0"),
            "1.1.0",
            "Upgrade complete",
            now - 259_200,
        ),
        (
            "1",
            "superseded",
            format!("{release}-1.0.0"),
            "1.0.0",
            "Install complete",
            now - 432_000,
        ),
    ];
    entries
        .into_iter()
        .map(
            |(rev, status, chart, app_version, description, updated_unix)| {
                Arc::new(super::kubernetes::resource_views::project_helm_history_row(
                    ns.to_owned(),
                    release,
                    rev.to_owned(),
                    status.to_owned(),
                    &chart,
                    app_version,
                    description.to_owned(),
                    updated_unix,
                ))
            },
        )
        .collect()
}

fn build_dir_rows(request: &WatchRequest) -> Vec<Arc<ResourceRow>> {
    let dir = request.field_selector.as_deref().unwrap_or(".");
    super::kubernetes::resource_views::read_dir_rows(dir).unwrap_or_default()
}

fn build_benchmark_rows(request: &WatchRequest) -> Vec<Arc<ResourceRow>> {
    super::kubernetes::resource_views::read_benchmark_rows(
        &request.context,
        request.field_selector.as_deref(),
    )
    .unwrap_or_default()
}

fn build_pulse_rows(item_count: usize, request: &WatchRequest) -> Vec<Arc<ResourceRow>> {
    use super::kubernetes::resource_views::{project_pulse_row, pulse_gauge_specs};
    let all_namespaces = request.namespace.is_none();
    let ns_label = request.namespace.as_deref().unwrap_or("-");
    let specs = pulse_gauge_specs(all_namespaces);
    let mut rows = Vec::with_capacity(specs.len() + 2);
    for (idx, (kind, title)) in specs.into_iter().enumerate() {
        let gvr = kind.gvr();
        let total = u64::try_from((item_count / 10).max(1) + idx * 3).unwrap_or(10);
        let faults = u64::from(idx % 3 == 0);
        rows.push(Arc::new(project_pulse_row(
            ns_label,
            &gvr,
            title,
            kind,
            false,
            total,
            faults,
            0,
            0,
            0,
            0,
            Arc::from([]),
        )));
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
        1_250,
        4_000,
        Arc::from([800, 950, 1_100, 1_050, 1_200, 1_250]),
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
        4_096,
        16_384,
        Arc::from([3_200, 3_500, 3_800, 3_950, 4_000, 4_096]),
    )));
    rows
}

fn demo_port_forward_ports(kind: Option<ResourceKind>) -> Vec<PortForwardPort> {
    match kind {
        Some(ResourceKind::Pods) => vec![
            PortForwardPort {
                container: Some(Arc::from("app")),
                name: Arc::from("http"),
                port: 8080,
            },
            PortForwardPort {
                container: Some(Arc::from("sidecar")),
                name: Arc::from("metrics"),
                port: 9090,
            },
        ],
        Some(ResourceKind::Services) => vec![PortForwardPort {
            container: None,
            name: Arc::from("http"),
            port: 8080,
        }],
        _ => Vec::new(),
    }
}

fn demo_status(
    known_kind: Option<ResourceKind>,
    index: usize,
) -> (&'static str, &'static str, bool) {
    if known_kind == Some(ResourceKind::Nodes) {
        if index.is_multiple_of(101) {
            ("NotReady,Unschedulable", "worker", true)
        } else if index.is_multiple_of(17) {
            ("Ready,Unschedulable", "worker", true)
        } else {
            ("Ready", "worker", false)
        }
    } else if index.is_multiple_of(101) {
        ("CrashLoopBackOff", "0/1", false)
    } else if index.is_multiple_of(37) {
        ("Pending", "0/1", false)
    } else {
        ("Running", "1/1", false)
    }
}

fn with_demo_owner(row: ResourceRow, request: &WatchRequest, index: usize) -> ResourceRow {
    if request.kind.is_job() {
        row.with_owner_reference_data(OwnerReferenceData {
            api_version: Arc::from("batch/v1"),
            kind: Arc::from("CronJob"),
            name: Arc::from(format!("cronjobs-{index:07}")),
            uid: Arc::from(format!("demo-cronjob-{index:07}")),
        })
    } else if request.kind.is_apps_v1_replica_set() {
        row.with_owner_reference_data(OwnerReferenceData {
            api_version: Arc::from("apps/v1"),
            kind: Arc::from("Deployment"),
            name: Arc::from(format!("deploy-{index:07}")),
            uid: Arc::from(format!("demo-deployment-{index:07}")),
        })
    } else {
        row
    }
}

fn with_demo_child(row: ResourceRow, request: &WatchRequest, labels: String) -> ResourceRow {
    let child_kind = match request.kind.known_kind() {
        Some(
            ResourceKind::Deployments
            | ResourceKind::StatefulSets
            | ResourceKind::DaemonSets
            | ResourceKind::Services,
        ) => Some(ResourceKind::Pods.into()),
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

#[cfg(test)]
fn replica_set_descriptor() -> ResourceDescriptor {
    ResourceDescriptor::new(
        "apps",
        "v1",
        "apps/v1",
        "ReplicaSet",
        "replicasets",
        true,
        crate::model::ResourceOperations::all(),
    )
    .with_short_names(["rs"])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn demo_workloads_keep_all_six_kinds_and_round_trip_to_single_objects() {
        let request = WatchRequest {
            generation: 1,
            context: "demo".to_owned(),
            kind: ResourceDescriptor::workloads(),
            namespace: None,
            instance: None,
            field_selector: None,
            label_selector: None,
        };
        let rows = build_rows(300, &request);
        assert_eq!(rows.len(), 1_800);
        assert_eq!(
            rows.iter()
                .map(|row| &row.key)
                .collect::<std::collections::HashSet<_>>()
                .len(),
            rows.len()
        );
        let mut kinds = std::collections::BTreeSet::new();
        for row in rows
            .iter()
            .filter(|row| row.namespace.as_ref() == "team-000" && row.name.ends_with("0000000"))
        {
            let crate::model::ResourceData::Workload { kind } = &row.data else {
                panic!("Workload row must retain its concrete resource kind");
            };
            kinds.insert(kind.gvr());
            let instance = ResourceTarget {
                kind: kind.clone(),
                namespace: Some(row.namespace.to_string()),
                name: row.name.to_string(),
            };
            let single = build_rows(
                300,
                &WatchRequest {
                    kind: kind.clone(),
                    instance: Some(instance),
                    // The active scope may differ from the object's namespace.
                    namespace: Some("team-099".to_owned()),
                    field_selector: Some("spec.nodeName=unrelated".to_owned()),
                    ..request.clone()
                },
            );
            assert_eq!(single.len(), 1);
            assert_eq!(single[0].namespace, row.namespace);
            assert_eq!(single[0].name, row.name);
        }
        assert_eq!(kinds.len(), 6);
        let scoped = build_rows(
            300,
            &WatchRequest {
                namespace: Some("team-000".to_owned()),
                ..request.clone()
            },
        );
        assert_eq!(scoped.len(), 12);
        assert!(
            scoped
                .iter()
                .all(|row| row.namespace.as_ref() == "team-000")
        );
        assert_eq!(
            build_rows(
                300,
                &WatchRequest {
                    instance: Some(ResourceTarget {
                        kind: request.kind.clone(),
                        namespace: Some("team-000".to_owned()),
                        name: "not-an-api-resource".to_owned(),
                    }),
                    ..request.clone()
                }
            )
            .len(),
            0
        );
        assert_eq!(
            build_rows(
                300,
                &WatchRequest {
                    kind: ResourceKind::Pods.into(),
                    instance: Some(ResourceTarget {
                        kind: ResourceKind::Pods.into(),
                        namespace: Some("team-000".to_owned()),
                        name: "missing".to_owned(),
                    }),
                    ..request
                }
            )
            .len(),
            0
        );
    }

    #[test]
    fn demo_deployment_and_replica_set_rows_both_open_label_selected_pods() {
        for kind in [ResourceKind::Deployments.into(), replica_set_descriptor()] {
            let rows = build_rows(
                1,
                &WatchRequest {
                    instance: None,
                    generation: 1,
                    context: "demo".to_owned(),
                    kind,
                    namespace: None,
                    field_selector: None,
                    label_selector: None,
                },
            );
            let row = rows.first().expect("demo workload row");
            let child = row.child_resource_data().expect("workload child metadata");
            assert_eq!(child.kind.known_kind(), Some(ResourceKind::Pods));
            assert!(!child.label_selector.is_empty());
            assert_eq!(child.label_selector, row.labels);
        }
    }

    #[test]
    fn demo_builder_produces_requested_cardinality() {
        let rows = build_rows(
            100_000,
            &WatchRequest {
                instance: None,
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
                instance: None,
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
                instance: None,
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
    fn demo_node_scheduling_actions_report_the_requested_operation() {
        for (unschedulable, expected) in [
            (true, "Cordoned demo node worker-0"),
            (false, "Uncordoned demo node worker-0"),
        ] {
            assert_eq!(
                demo_action(ActionIntent::SetNodeUnschedulable {
                    name: "worker-0".to_owned(),
                    unschedulable,
                }),
                ActionResult::Notice(expected.to_owned())
            );
        }
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
    fn demo_rollback_loads_a_frozen_snapshot_and_reports_the_selected_revision() {
        let target = ResourceTarget {
            kind: ResourceDescriptor::new(
                "apps",
                "v1",
                "apps/v1",
                "ReplicaSet",
                "replicasets",
                true,
                crate::model::ResourceOperations::all(),
            ),
            namespace: Some("team-007".to_owned()),
            name: "worker-previous".to_owned(),
        };
        let ActionResult::RollbackLoaded {
            id,
            target: loaded_target,
            result,
        } = demo_action(ActionIntent::LoadRollback {
            id: 49,
            target: target.clone(),
        })
        else {
            panic!("demo must return a request-identified rollback snapshot");
        };
        assert_eq!(id, 49);
        assert_eq!(loaded_target, target);
        let snapshot = result.unwrap();
        assert_eq!(snapshot.target, target);
        assert_ne!(snapshot.uid, "");
        assert_ne!(snapshot.resource_version, "");
        assert_ne!(snapshot.deployment_uid, "");
        assert_ne!(snapshot.deployment_resource_version, "");
        assert_eq!(snapshot.revision, 1);
        assert!(!snapshot.already_matches);
        assert_eq!(
            snapshot.template["spec"]["containers"][0]["image"],
            "nginx:1.26"
        );
        assert!(
            snapshot.template["metadata"]["labels"]
                .get("pod-template-hash")
                .is_none()
        );
        let original = snapshot.clone();
        let ActionResult::RolledBack { result } = demo_action(ActionIntent::Rollback {
            snapshot: Box::new(snapshot),
        }) else {
            panic!("demo must retain a typed rollback completion");
        };
        assert_eq!(
            result.unwrap(),
            format!(
                "Demo: would roll back Deployment team-007/{} to ReplicaSet worker-previous (revision 1)",
                original.deployment,
            )
        );
    }

    #[test]
    fn demo_catalog_includes_usedby_sources_and_all_workload_destinations() {
        let resources = demo_resource_options();
        for alias in ["cm", "sec", "pvc", "sa", "pc"] {
            let source = resources
                .iter()
                .find(|kind| kind.command_aliases().iter().any(|name| name == alias))
                .unwrap();
            assert!(super::super::supports_references(source));
        }
        for alias in ["deploy", "ds", "sts", "job", "cj"] {
            assert!(
                resources
                    .iter()
                    .any(|kind| kind.command_aliases().iter().any(|name| name == alias))
            );
        }
        for kind in ResourceKind::ALL {
            assert!(
                resources
                    .iter()
                    .any(|resource| resource.known_kind() == Some(kind))
            );
        }
    }

    #[test]
    fn demo_references_retain_request_identity_and_actual_workload_targets() {
        for namespace in [Some("team-007".to_owned()), None] {
            let target = ResourceTarget {
                kind: ResourceDescriptor::new(
                    if namespace.is_some() {
                        ""
                    } else {
                        "scheduling.k8s.io"
                    },
                    "v1",
                    if namespace.is_some() {
                        "v1"
                    } else {
                        "scheduling.k8s.io/v1"
                    },
                    if namespace.is_some() {
                        "ConfigMap"
                    } else {
                        "PriorityClass"
                    },
                    if namespace.is_some() {
                        "configmaps"
                    } else {
                        "priorityclasses"
                    },
                    namespace.is_some(),
                    crate::model::ResourceOperations::all(),
                ),
                namespace: namespace.clone(),
                name: "settings".to_owned(),
            };
            let ActionResult::References {
                id,
                target: loaded,
                result,
            } = demo_action(ActionIntent::LoadReferences {
                id: 52,
                target: target.clone(),
            })
            else {
                panic!("demo must retain reference request identity");
            };
            assert_eq!(id, 52);
            assert_eq!(loaded, target);
            let scan = result.unwrap();
            assert_eq!(scan.scanned, 5);
            assert_eq!(scan.errors, Vec::<String>::new());
            assert_eq!(scan.entries.len(), 5);
            assert_eq!(
                scan.entries
                    .iter()
                    .map(|entry| entry.target.kind.kind.as_ref())
                    .collect::<Vec<_>>(),
                vec!["Deployment", "DaemonSet", "StatefulSet", "Job", "CronJob"],
            );
            for entry in scan.entries {
                if let Some(namespace) = &namespace {
                    assert_eq!(entry.target.namespace.as_ref(), Some(namespace));
                }
                let request = WatchRequest {
                    instance: None,
                    generation: 1,
                    context: "demo".to_owned(),
                    kind: entry.target.kind.clone(),
                    namespace: entry.target.namespace.clone(),
                    field_selector: None,
                    label_selector: None,
                };
                let rows = build_rows(10, &request);
                assert!(
                    rows.iter()
                        .any(|row| row.name.as_ref() == entry.target.name)
                );
            }
        }
    }

    #[test]
    fn demo_deployment_selector_retains_request_identity_and_child_mode() {
        let target = ResourceTarget {
            kind: ResourceKind::Deployments.into(),
            namespace: Some("team-007".to_owned()),
            name: "worker".to_owned(),
        };
        for replica_sets in [false, true] {
            assert_eq!(
                demo_action(ActionIntent::LoadDeploymentSelector {
                    id: 51,
                    target: target.clone(),
                    replica_sets
                }),
                ActionResult::DeploymentSelector {
                    id: 51,
                    target: target.clone(),
                    replica_sets,
                    result: Ok("app=demo".to_owned()),
                },
            );
        }
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
            let ActionResult::OpenedContent {
                id,
                target: returned_target,
                result,
            } = demo_action(ActionIntent::Content {
                id: 42,
                target: target.clone(),
                content: ContentKind::Yaml,
            })
            else {
                panic!("expected tagged YAML content");
            };
            assert_eq!(id, 42);
            assert_eq!(returned_target, target);
            let ActionResult::Content { title, .. } = *result else {
                panic!("expected raw YAML payload");
            };
            assert_eq!(title, expected);
        }
    }
}
