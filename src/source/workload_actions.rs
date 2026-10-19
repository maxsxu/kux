use std::fmt::Write as _;

use futures::StreamExt;
use k8s_openapi::{
    api::batch::v1::{CronJob, Job},
    apimachinery::pkg::apis::meta::v1::{ObjectMeta, OwnerReference},
};
use kube::{
    Api, Client, ResourceExt,
    api::{Patch, PatchParams, PostParams},
};

use crate::{app::ResourceTarget, source::ActionResult};

use super::{dynamic_target_api, resource_identity};

#[derive(Clone, Copy)]
enum WorkloadPatch {
    Scale(i32),
    Suspend(bool),
}

impl WorkloadPatch {
    fn payload(self) -> serde_json::Value {
        match self {
            Self::Scale(replicas) => serde_json::json!({"spec": {"replicas": replicas}}),
            Self::Suspend(suspended) => serde_json::json!({"spec": {"suspend": suspended}}),
        }
    }

    fn description(self) -> String {
        match self {
            Self::Scale(replicas) => format!("Scale to {replicas} replicas"),
            Self::Suspend(true) => "Suspend".to_owned(),
            Self::Suspend(false) => "Resume".to_owned(),
        }
    }
}

pub(super) async fn scale_workloads(
    client: Client,
    targets: Vec<ResourceTarget>,
    replicas: i32,
) -> Result<ActionResult, String> {
    validate_scale_targets(&targets, replicas)?;
    patch_workloads(client, targets, WorkloadPatch::Scale(replicas)).await
}

pub(in crate::source) fn validate_scale_targets(
    targets: &[ResourceTarget],
    replicas: i32,
) -> Result<(), String> {
    if replicas < 0 {
        return Err("Replica count must be zero or greater".to_owned());
    }
    if targets.is_empty() {
        return Err("No workloads selected for scaling".to_owned());
    }
    for target in targets {
        validate_target_identity(target)?;
        if !target.kind.is_scalable() {
            return Err(format!(
                "{} {} does not advertise a supported scale subresource",
                target.kind.kind,
                resource_identity(target)
            ));
        }
    }
    Ok(())
}

pub(super) async fn set_suspended(
    client: Client,
    targets: Vec<ResourceTarget>,
    suspended: bool,
) -> Result<ActionResult, String> {
    validate_suspension_targets(&targets)?;
    patch_workloads(client, targets, WorkloadPatch::Suspend(suspended)).await
}

pub(in crate::source) fn validate_suspension_targets(
    targets: &[ResourceTarget],
) -> Result<(), String> {
    if targets.is_empty() {
        return Err("No Jobs or CronJobs selected".to_owned());
    }
    for target in targets {
        validate_target_identity(target)?;
        if !is_batch_resource(target, "Job", "jobs")
            && !is_batch_resource(target, "CronJob", "cronjobs")
        {
            return Err(format!(
                "Suspend/resume supports batch/v1 Jobs and CronJobs; unsupported {} {}",
                target.kind.kind,
                resource_identity(target)
            ));
        }
    }
    Ok(())
}

fn validate_target_identity(target: &ResourceTarget) -> Result<(), String> {
    if target.kind.is_namespaced() && target.namespace.as_deref().is_none_or(str::is_empty) {
        return Err(format!("{} requires a namespace", target.kind.kind));
    }
    if target.name.is_empty() {
        return Err(format!("{} requires a name", target.kind.kind));
    }
    Ok(())
}

fn is_batch_resource(target: &ResourceTarget, kind: &str, plural: &str) -> bool {
    target.kind.is_namespaced()
        && target.kind.group.as_ref() == "batch"
        && target.kind.version.as_ref() == "v1"
        && target.kind.api_version.as_ref() == "batch/v1"
        && target.kind.kind.as_ref() == kind
        && target.kind.plural.as_ref() == plural
}

async fn patch_workloads(
    client: Client,
    targets: Vec<ResourceTarget>,
    operation: WorkloadPatch,
) -> Result<ActionResult, String> {
    let total = targets.len();
    let tasks = targets.into_iter().map(|target| {
        let client = client.clone();
        async move {
            let identity = format!("{} {}", target.kind.kind, resource_identity(&target));
            let result = patch_workload(client, &target, operation).await;
            (identity, result)
        }
    });
    let results = futures::stream::iter(tasks)
        .buffer_unordered(16)
        .collect::<Vec<_>>()
        .await;
    let mut failures = results
        .into_iter()
        .filter_map(|(identity, result)| result.err().map(|error| (identity, error)))
        .collect::<Vec<_>>();
    failures.sort_unstable_by(|left, right| left.0.cmp(&right.0));
    batch_result(&operation.description(), total, &failures)
}

async fn patch_workload(
    client: Client,
    target: &ResourceTarget,
    operation: WorkloadPatch,
) -> Result<(), String> {
    let api = dynamic_target_api(client, target)?;
    let patch = Patch::Merge(operation.payload());
    let params = PatchParams::default();
    match operation {
        WorkloadPatch::Scale(_) => api
            .patch_scale(&target.name, &params, &patch)
            .await
            .map(|_| ()),
        WorkloadPatch::Suspend(_) => api.patch(&target.name, &params, &patch).await.map(|_| ()),
    }
    .map_err(|error| error.to_string())
}

fn batch_result(
    operation: &str,
    total: usize,
    failures: &[(String, String)],
) -> Result<ActionResult, String> {
    let noun = if total == 1 { "workload" } else { "workloads" };
    if failures.is_empty() {
        return Ok(ActionResult::Notice(format!(
            "{operation} requested for {total} {noun}"
        )));
    }
    let succeeded = total.saturating_sub(failures.len());
    let mut details = failures
        .iter()
        .take(3)
        .map(|(identity, error)| format!("{identity}: {error}"))
        .collect::<Vec<_>>()
        .join("; ");
    if failures.len() > 3 {
        let _ = write!(details, "; and {} more", failures.len() - 3);
    }
    Err(format!(
        "{operation} requested for {succeeded} of {total} {noun}; failed: {details}"
    ))
}

pub(in crate::source) fn validate_cronjob_target(target: &ResourceTarget) -> Result<(), String> {
    validate_target_identity(target)?;
    if !is_batch_resource(target, "CronJob", "cronjobs") {
        return Err("Trigger supports batch/v1 CronJobs".to_owned());
    }
    if !target.kind.operations.gettable() {
        return Err("This CronJob API does not support reading the job template".to_owned());
    }
    Ok(())
}

pub(super) async fn trigger_cronjobs(
    client: Client,
    targets: Vec<ResourceTarget>,
) -> Result<ActionResult, String> {
    validate_cronjob_targets(&targets)?;
    let total = targets.len();
    let tasks = targets.into_iter().map(|target| {
        let client = client.clone();
        async move {
            let identity = resource_identity(&target);
            (identity, trigger_cronjob(client, target).await)
        }
    });
    let results = futures::stream::iter(tasks)
        .buffer_unordered(16)
        .collect::<Vec<_>>()
        .await;
    let mut failures = Vec::new();
    let mut created = Vec::new();
    for (identity, result) in results {
        match result {
            Ok(job) => created.push(job),
            Err(error) => failures.push((identity, error)),
        }
    }
    failures.sort_unstable_by(|left, right| left.0.cmp(&right.0));
    batch_result("Trigger CronJob", total, &failures)?;
    created.sort_unstable();
    let visible = created
        .iter()
        .take(3)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    let remaining = if created.len() > 3 {
        format!("; and {} more", created.len() - 3)
    } else {
        String::new()
    };
    Ok(ActionResult::Notice(format!(
        "Triggered {total} CronJob(s): created Job(s) {visible}{remaining}"
    )))
}

pub(in crate::source) fn validate_cronjob_targets(
    targets: &[ResourceTarget],
) -> Result<(), String> {
    if targets.is_empty() {
        return Err("No CronJobs selected".to_owned());
    }
    targets.iter().try_for_each(validate_cronjob_target)
}

async fn trigger_cronjob(client: Client, target: ResourceTarget) -> Result<String, String> {
    let namespace = target.namespace.as_deref().expect("validated namespace");
    let identity = resource_identity(&target);
    let cronjob = Api::<CronJob>::namespaced(client.clone(), namespace)
        .get(&target.name)
        .await
        .map_err(|error| format!("Unable to read CronJob {identity}: {error}"))?;
    let job = manual_job(&cronjob, &target)?;
    let created = Api::<Job>::namespaced(client, namespace)
        .create(&PostParams::default(), &job)
        .await
        .map_err(|error| format!("Unable to trigger CronJob {identity}: {error}"))?;
    Ok(format!("{namespace}/{}", created.name_any()))
}

pub(super) async fn toggle_cronjob_suspend(
    client: Client,
    target: ResourceTarget,
) -> Result<ActionResult, String> {
    validate_cronjob_target(&target)?;
    let namespace = target.namespace.as_deref().expect("validated namespace");
    let identity = resource_identity(&target);
    let api = Api::<CronJob>::namespaced(client, namespace);
    let cronjob = api
        .get(&target.name)
        .await
        .map_err(|error| format!("Unable to read CronJob {identity}: {error}"))?;
    let (suspended, patch) = toggle_suspend_patch(&cronjob)?;
    api.patch(&target.name, &PatchParams::default(), &Patch::Merge(patch))
        .await
        .map_err(|error| format!("Unable to change CronJob {identity} suspension: {error}"))?;
    let action = if suspended { "Suspended" } else { "Resumed" };
    Ok(ActionResult::Notice(format!("{action} CronJob {identity}")))
}

fn toggle_suspend_patch(cronjob: &CronJob) -> Result<(bool, serde_json::Value), String> {
    let version = cronjob
        .metadata
        .resource_version
        .as_deref()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            "CronJob has no resourceVersion; unable to safely toggle suspension".to_owned()
        })?;
    let spec = cronjob
        .spec
        .as_ref()
        .ok_or_else(|| "CronJob has no spec".to_owned())?;
    let suspended = !spec.suspend.unwrap_or(false);
    Ok((
        suspended,
        serde_json::json!({
            "metadata": { "resourceVersion": version },
            "spec": { "suspend": suspended },
        }),
    ))
}

fn manual_job(cronjob: &CronJob, target: &ResourceTarget) -> Result<Job, String> {
    let identity = resource_identity(target);
    let uid = cronjob
        .metadata
        .uid
        .clone()
        .filter(|uid| !uid.is_empty())
        .ok_or_else(|| format!("CronJob {identity} has no UID"))?;
    let template = &cronjob
        .spec
        .as_ref()
        .ok_or_else(|| format!("CronJob {identity} has no spec"))?
        .job_template;
    let spec = template
        .spec
        .clone()
        .ok_or_else(|| format!("CronJob {identity} has no job template spec"))?;
    let mut annotations = template
        .metadata
        .as_ref()
        .and_then(|metadata| metadata.annotations.clone())
        .unwrap_or_default();
    annotations.insert(
        "cronjob.kubernetes.io/instantiate".to_owned(),
        "manual".to_owned(),
    );
    // Kubernetes appends five characters. Keep the resulting Job name within 63 characters.
    let prefix = target.name.chars().take(50).collect::<String>();
    let prefix = prefix.trim_end_matches(['-', '.']);
    Ok(Job {
        metadata: ObjectMeta {
            generate_name: Some(format!("{prefix}-manual-")),
            namespace: target.namespace.clone(),
            labels: template
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.labels.clone()),
            annotations: Some(annotations),
            owner_references: Some(vec![OwnerReference {
                api_version: "batch/v1".to_owned(),
                kind: "CronJob".to_owned(),
                name: target.name.clone(),
                uid,
                controller: Some(true),
                // A blocking owner reference additionally requires CronJob finalizer permissions.
                block_owner_deletion: None,
            }]),
            ..ObjectMeta::default()
        },
        spec: Some(spec),
        ..Job::default()
    })
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use kube::{Resource, core::Request};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use crate::model::{ResourceDescriptor, ResourceKind, ResourceOperations};

    use super::*;

    fn target(group: &str, kind: &str, plural: &str) -> ResourceTarget {
        ResourceTarget {
            kind: ResourceDescriptor::new(
                group,
                "v1",
                if group.is_empty() {
                    "v1".to_owned()
                } else {
                    format!("{group}/v1")
                },
                kind,
                plural,
                true,
                ResourceOperations::all(),
            ),
            namespace: Some("payments".to_owned()),
            name: "worker".to_owned(),
        }
    }

    #[derive(Debug)]
    struct ObservedRequest {
        method: String,
        path: String,
        body: serde_json::Value,
    }

    async fn mock_api<F>(
        count: usize,
        response: F,
    ) -> (Client, tokio::task::JoinHandle<Vec<ObservedRequest>>)
    where
        F: Fn(&ObservedRequest) -> (u16, serde_json::Value) + Send + 'static,
    {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("mock listener");
        let config = kube::Config::new(
            format!("http://{}", listener.local_addr().unwrap())
                .parse()
                .unwrap(),
        );
        let client = Client::try_from(config).expect("mock client");
        let server = tokio::spawn(async move {
            let mut requests = Vec::new();
            for _ in 0..count {
                let (mut stream, _) =
                    tokio::time::timeout(Duration::from_secs(5), listener.accept())
                        .await
                        .expect("expected request within timeout")
                        .expect("mock connection");
                let request = read_mock_request(&mut stream).await;
                let (status, body) = response(&request);
                let body = serde_json::to_vec(&body).unwrap();
                let headers = format!(
                    "HTTP/1.1 {status} Mock\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                stream.write_all(headers.as_bytes()).await.unwrap();
                stream.write_all(&body).await.unwrap();
                stream.shutdown().await.unwrap();
                requests.push(request);
            }
            requests
        });
        (client, server)
    }

    async fn read_mock_request(stream: &mut tokio::net::TcpStream) -> ObservedRequest {
        let mut bytes = Vec::new();
        let header_end = loop {
            let mut chunk = [0_u8; 4096];
            let size = stream.read(&mut chunk).await.unwrap();
            assert_ne!(size, 0, "request ended before headers");
            bytes.extend_from_slice(&chunk[..size]);
            if let Some(offset) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                break offset + 4;
            }
        };
        let headers = String::from_utf8(bytes[..header_end].to_vec()).unwrap();
        let length = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().unwrap())
            })
            .unwrap_or(0);
        while bytes.len() - header_end < length {
            let mut chunk = [0_u8; 4096];
            let size = stream.read(&mut chunk).await.unwrap();
            assert_ne!(size, 0, "request ended before body");
            bytes.extend_from_slice(&chunk[..size]);
        }
        let mut parts = headers.lines().next().unwrap().split_whitespace();
        let method = parts.next().unwrap().to_owned();
        let path = parts.next().unwrap().split('?').next().unwrap().to_owned();
        ObservedRequest {
            method,
            path,
            body: if length == 0 {
                serde_json::Value::Null
            } else {
                serde_json::from_slice(&bytes[header_end..header_end + length]).unwrap()
            },
        }
    }

    #[tokio::test]
    async fn real_client_scales_through_subresource_and_reports_partial_forbidden() {
        let (client, server) = mock_api(2, |request| {
            assert_eq!(request.method, "PATCH");
            assert!(request.path.ends_with("/scale"));
            assert_eq!(request.body, serde_json::json!({"spec": {"replicas": 0}}));
            if request.path.contains("/denied/") {
                (
                    403,
                    serde_json::json!({"kind": "Status", "apiVersion": "v1", "status": "Failure",
                    "reason": "Forbidden", "message": "patch scale denied", "code": 403}),
                )
            } else {
                (
                    200,
                    serde_json::json!({"kind": "Scale", "apiVersion": "autoscaling/v1",
                    "metadata": {"name": "worker"}, "spec": {"replicas": 0}}),
                )
            }
        })
        .await;
        let resource = target("apps", "Deployment", "deployments");
        let denied = ResourceTarget {
            name: "denied".to_owned(),
            ..resource.clone()
        };
        let result = scale_workloads(client, vec![resource, denied], 0)
            .await
            .unwrap_err();
        assert!(result.contains("1 of 2 workloads"));
        assert!(result.contains("Deployment payments/denied"));
        assert!(result.contains("patch scale denied"));
        let requests = server.await.unwrap();
        assert!(
            requests.iter().any(|request| request.path
                == "/apis/apps/v1/namespaces/payments/deployments/worker/scale")
        );
    }

    #[tokio::test]
    async fn real_client_does_not_retry_conflicting_cronjob_toggle() {
        let (client, server) = mock_api(2, |request| {
            assert_eq!(
                request.path,
                "/apis/batch/v1/namespaces/payments/cronjobs/worker"
            );
            if request.method == "GET" {
                (
                    200,
                    serde_json::json!({"kind": "CronJob", "apiVersion": "batch/v1",
                    "metadata": {"name": "worker", "resourceVersion": "17"},
                    "spec": {"schedule": "* * * * *", "jobTemplate": {}, "suspend": true}}),
                )
            } else {
                assert_eq!(request.method, "PATCH");
                assert_eq!(
                    request.body,
                    serde_json::json!({
                    "metadata": {"resourceVersion": "17"}, "spec": {"suspend": false}})
                );
                (
                    409,
                    serde_json::json!({"kind": "Status", "apiVersion": "v1", "status": "Failure",
                    "reason": "Conflict", "message": "resourceVersion changed", "code": 409}),
                )
            }
        })
        .await;
        let result = toggle_cronjob_suspend(client, target("batch", "CronJob", "cronjobs"))
            .await
            .unwrap_err();
        assert!(result.contains("CronJob payments/worker"));
        assert!(result.contains("resourceVersion changed"));
        assert_eq!(server.await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn real_client_creates_job_in_selected_cronjob_namespace() {
        let (client, server) = mock_api(2, |request| {
            if request.method == "GET" {
                assert_eq!(request.path, "/apis/batch/v1/namespaces/payments/cronjobs/worker");
                (200, serde_json::json!({"kind": "CronJob", "apiVersion": "batch/v1",
                    "metadata": {"name": "worker", "namespace": "payments", "uid": "owner-uid"},
                    "spec": {"schedule": "* * * * *", "jobTemplate": {"spec": {"template": {"spec": {
                        "restartPolicy": "Never", "containers": [{"name": "app", "image": "app:v1"}]}}}}}}))
            } else {
                assert_eq!(request.method, "POST");
                assert_eq!(request.path, "/apis/batch/v1/namespaces/payments/jobs");
                assert_eq!(request.body["metadata"]["namespace"], "payments");
                assert_eq!(request.body["metadata"]["ownerReferences"][0]["uid"], "owner-uid");
                assert_eq!(request.body["spec"]["template"]["spec"]["containers"][0]["image"], "app:v1");
                (201, serde_json::json!({"kind": "Job", "apiVersion": "batch/v1",
                    "metadata": {"name": "worker-manual-abcde", "namespace": "payments"}}))
            }
        }).await;
        let result = trigger_cronjobs(client, vec![target("batch", "CronJob", "cronjobs")])
            .await
            .unwrap();
        let ActionResult::Notice(notice) = result else {
            panic!("trigger notice");
        };
        assert!(notice.contains("payments/worker-manual-abcde"));
        assert_eq!(server.await.unwrap().len(), 2);
    }

    #[test]
    fn scaling_accepts_zero_and_only_exact_supported_resource_descriptors() {
        for (group, kind, plural) in [
            ("apps", "Deployment", "deployments"),
            ("apps", "StatefulSet", "statefulsets"),
            ("apps", "ReplicaSet", "replicasets"),
            ("", "ReplicationController", "replicationcontrollers"),
        ] {
            let resource = target(group, kind, plural);
            assert!(validate_scale_targets(std::slice::from_ref(&resource), 0).is_ok());
            assert!(validate_scale_targets(std::slice::from_ref(&resource), i32::MAX).is_ok());
            assert!(validate_scale_targets(&[resource], -1).is_err());
        }
        for resource in [
            target("apps", "DaemonSet", "daemonsets"),
            target("example.com", "Deployment", "deployments"),
            target("apps", "Deployment", "widgets"),
            ResourceTarget {
                namespace: None,
                ..target("apps", "Deployment", "deployments")
            },
            ResourceTarget {
                namespace: Some(String::new()),
                ..target("apps", "Deployment", "deployments")
            },
        ] {
            assert!(validate_scale_targets(&[resource], 3).is_err());
        }
        assert!(validate_scale_targets(&[], 3).is_err());
    }

    #[test]
    fn scale_patch_routes_through_scale_subresource_and_encodes_zero() {
        for (resource, path) in [
            (
                target("apps", "Deployment", "deployments"),
                "/apis/apps/v1/namespaces/payments/deployments/worker/scale",
            ),
            (
                target("", "ReplicationController", "replicationcontrollers"),
                "/api/v1/namespaces/payments/replicationcontrollers/worker/scale",
            ),
        ] {
            let ar = super::super::api_resource(&resource.kind);
            let path_prefix =
                kube::core::DynamicObject::url_path(&ar, resource.namespace.as_deref());
            let request = Request::new(&path_prefix)
                .patch_subresource(
                    "scale",
                    &resource.name,
                    &PatchParams::default(),
                    &Patch::Merge(WorkloadPatch::Scale(0).payload()),
                )
                .expect("valid request");
            assert_eq!(request.method(), http::Method::PATCH);
            assert_eq!(request.uri().path(), path);
            assert_eq!(
                request.headers()[http::header::CONTENT_TYPE],
                "application/merge-patch+json"
            );
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(request.body()).unwrap(),
                serde_json::json!({"spec": {"replicas": 0}})
            );
        }
    }

    #[test]
    fn scale_supports_discovered_custom_resources_with_scale_capability() {
        let mut resource = target("workloads.example.com", "WorkerSet", "workersets");
        assert!(validate_scale_targets(std::slice::from_ref(&resource), 4).is_err());
        resource.kind.operations = resource.kind.operations.with_scale(true);
        assert!(validate_scale_targets(std::slice::from_ref(&resource), 4).is_ok());
        resource.kind.namespaced = false;
        resource.namespace = None;
        assert!(validate_scale_targets(&[resource], 4).is_ok());
    }

    #[test]
    fn suspend_resume_supports_only_batch_jobs_and_cronjobs() {
        assert!(
            validate_suspension_targets(&[
                target("batch", "Job", "jobs"),
                target("batch", "CronJob", "cronjobs"),
            ])
            .is_ok()
        );
        for resource in [
            target("example.com", "Job", "jobs"),
            target("batch", "Job", "otherjobs"),
            ResourceTarget {
                kind: ResourceKind::Deployments.into(),
                ..target("batch", "Job", "jobs")
            },
        ] {
            assert!(validate_suspension_targets(&[resource]).is_err());
        }
        for suspended in [true, false] {
            assert_eq!(
                WorkloadPatch::Suspend(suspended).payload(),
                serde_json::json!({"spec": {"suspend": suspended}})
            );
        }
        assert!(validate_suspension_targets(&[]).is_err());
    }

    #[test]
    fn manual_job_preserves_template_and_discards_server_metadata() {
        let cronjob: CronJob = serde_json::from_value(serde_json::json!({
            "metadata": {"name": "worker", "namespace": "payments", "uid": "source-uid"},
            "spec": {
                "schedule": "*/5 * * * *",
                "suspend": true,
                "jobTemplate": {
                    "metadata": {
                        "name": "must-not-reuse", "uid": "template-uid", "resourceVersion": "10",
                        "labels": {"team": "payments"},
                        "annotations": {"example.com/purpose": "billing"},
                        "finalizers": ["example.com/keep"]
                    },
                    "spec": {
                        "backoffLimit": 2, "ttlSecondsAfterFinished": 90,
                        "template": {
                            "metadata": {"labels": {"app": "worker"}},
                            "spec": {
                                "containers": [{"name": "worker", "image": "worker:v3"}],
                                "restartPolicy": "Never"
                            }
                        }
                    }
                }
            }
        }))
        .expect("valid CronJob");
        let resource = target("batch", "CronJob", "cronjobs");
        let job = manual_job(&cronjob, &resource).expect("manual job");
        assert_eq!(job.spec, cronjob.spec.as_ref().unwrap().job_template.spec);
        assert_eq!(
            job.metadata.generate_name.as_deref(),
            Some("worker-manual-")
        );
        assert_eq!(job.metadata.namespace.as_deref(), Some("payments"));
        assert!(job.metadata.name.is_none());
        assert!(job.metadata.uid.is_none());
        assert!(job.metadata.resource_version.is_none());
        assert!(job.metadata.finalizers.is_none());
        let annotations = job.metadata.annotations.as_ref().unwrap();
        assert_eq!(annotations["cronjob.kubernetes.io/instantiate"], "manual");
        assert_eq!(annotations["example.com/purpose"], "billing");
        assert_eq!(job.metadata.labels.as_ref().unwrap()["team"], "payments");
        let owner = &job.metadata.owner_references.as_ref().unwrap()[0];
        assert_eq!(owner.uid, "source-uid");
        assert_eq!(owner.name, "worker");
        assert_eq!(owner.controller, Some(true));
        assert_ne!(owner.block_owner_deletion, Some(true));
        let request = Request::new(Job::url_path(&(), Some("payments")))
            .create(&PostParams::default(), serde_json::to_vec(&job).unwrap())
            .expect("create job request");
        assert_eq!(request.method(), http::Method::POST);
        assert_eq!(
            request.uri().path(),
            "/apis/batch/v1/namespaces/payments/jobs"
        );
        let long_target = ResourceTarget {
            name: "a".repeat(52),
            ..resource
        };
        let long_job = manual_job(&cronjob, &long_target).unwrap();
        assert!(long_job.metadata.generate_name.unwrap().len() + 5 <= 63);
    }

    #[test]
    fn triggering_requires_readable_cronjob_and_complete_template() {
        assert!(validate_cronjob_target(&target("batch", "Job", "jobs")).is_err());
        let mut resource = target("batch", "CronJob", "cronjobs");
        resource.kind.operations = ResourceOperations::new(true, false, true);
        assert!(validate_cronjob_target(&resource).is_err());
        assert!(
            manual_job(&CronJob::default(), &resource)
                .unwrap_err()
                .contains("UID")
        );
        assert!(validate_cronjob_targets(&[]).is_err());
    }

    #[test]
    fn toggle_uses_fetched_state_and_resource_version_precondition() {
        for previous in [None, Some(false), Some(true)] {
            let cronjob = CronJob {
                metadata: ObjectMeta {
                    resource_version: Some("1042".to_owned()),
                    ..ObjectMeta::default()
                },
                spec: Some(k8s_openapi::api::batch::v1::CronJobSpec {
                    suspend: previous,
                    ..Default::default()
                }),
                ..CronJob::default()
            };
            let expected = !previous.unwrap_or(false);
            let (suspended, patch) = toggle_suspend_patch(&cronjob).unwrap();
            assert_eq!(suspended, expected);
            assert_eq!(
                patch,
                serde_json::json!({
                    "metadata": { "resourceVersion": "1042" },
                    "spec": { "suspend": expected },
                })
            );
        }
        assert!(
            toggle_suspend_patch(&CronJob::default())
                .unwrap_err()
                .contains("resourceVersion")
        );
    }

    #[test]
    fn batch_errors_report_success_count_and_cap_identity_details() {
        assert_eq!(
            batch_result("Scale to 2 replicas", 2, &[]),
            Ok(ActionResult::Notice(
                "Scale to 2 replicas requested for 2 workloads".to_owned()
            ))
        );
        let failures = (0..4)
            .map(|index| (format!("Job payments/job-{index}"), "forbidden".to_owned()))
            .collect::<Vec<_>>();
        let error = batch_result("Suspend", 5, &failures).unwrap_err();
        assert!(error.contains("1 of 5 workloads"));
        assert!(error.contains("Job payments/job-0: forbidden"));
        assert!(error.contains("and 1 more"));
        assert!(!error.contains("job-3"));
    }
}
