use std::{collections::BTreeSet, future::Future, time::Duration};

use k8s_openapi::api::core::v1::Pod;
use kube::{
    Api, Client,
    api::{DeleteParams, ListParams, Preconditions},
};
use tokio::time::Instant;

#[cfg(test)]
#[path = "pod_actions_tests.rs"]
mod tests;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SanitizeReport {
    pub deleted: usize,
    pub eligible: usize,
    pub failure: Option<String>,
}

#[derive(Clone, Copy)]
struct Timeouts {
    request: Duration,
    total: Duration,
}

impl Timeouts {
    fn from_call_timeout(request: Duration) -> Result<Self, String> {
        if request.is_zero() {
            return Err("Sanitize call timeout must be positive or disabled".to_owned());
        }
        let total = request
            .checked_mul(5)
            .ok_or_else(|| "Sanitize call timeout is too large".to_owned())?;
        Ok(Self { request, total })
    }

    fn start(self) -> Result<(Instant, Duration), String> {
        let now = Instant::now();
        // Validate both clocks before constructing any API request.
        now.checked_add(self.request)
            .ok_or_else(|| "Sanitize call timeout is too large".to_owned())?;
        let deadline = now
            .checked_add(self.total)
            .ok_or_else(|| "Sanitize overall timeout is too large".to_owned())?;
        Ok((deadline, self.request))
    }
}

/// Delete eligible Pods in the selected namespace, stopping at the first failure.
/// Listing completes before any deletion; each delete requires the listed UID and revision.
pub(super) async fn sanitize_pods(
    client: Client,
    namespace: Option<String>,
    call_timeout: Option<Duration>,
) -> Result<SanitizeReport, String> {
    let timeouts = call_timeout.map(Timeouts::from_call_timeout).transpose()?;
    sanitize_with_timeouts(client, namespace, timeouts).await
}

async fn sanitize_with_timeouts(
    client: Client,
    namespace: Option<String>,
    timeouts: Option<Timeouts>,
) -> Result<SanitizeReport, String> {
    if namespace
        .as_deref()
        .is_some_and(|namespace| namespace.trim().is_empty())
    {
        return Err(
            "Sanitize requires an explicit namespace or the all-namespaces scope".to_owned(),
        );
    }
    let deadline = timeouts.map(Timeouts::start).transpose()?;
    let api = namespace.as_deref().map_or_else(
        || Api::<Pod>::all(client.clone()),
        |namespace| Api::<Pod>::namespaced(client.clone(), namespace),
    );
    let pods = eligible_pods(&api, deadline).await.map_err(|error| {
        format!(
            "Unable to list Pods for sanitize in {}: {error}",
            namespace.as_deref().unwrap_or("all namespaces")
        )
    })?;
    let mut report = SanitizeReport {
        deleted: 0,
        eligible: pods.len(),
        failure: None,
    };
    for pod in pods {
        let identity = format!(
            "Pod {}/{}",
            pod.metadata
                .namespace
                .as_deref()
                .unwrap_or("<no namespace>"),
            pod.metadata.name.as_deref().unwrap_or("<no name>")
        );
        let result = async {
            let (namespace, name, params) = delete_identity(&pod, namespace.as_deref())?;
            let api = Api::<Pod>::namespaced(client.clone(), namespace);
            bounded(deadline, api.delete(name, &params)).await?;
            Ok::<_, String>(())
        }
        .await;
        match result {
            Ok(()) => report.deleted += 1,
            Err(error) => {
                report.failure = Some(format!("{identity}: {error}"));
                break;
            }
        }
    }
    Ok(report)
}

async fn eligible_pods(
    api: &Api<Pod>,
    deadline: Option<(Instant, Duration)>,
) -> Result<Vec<Pod>, String> {
    let mut pods = Vec::new();
    let mut params = ListParams::default().limit(500);
    let mut continuations = BTreeSet::new();
    loop {
        let page = bounded(deadline, api.list(&params)).await?;
        pods.extend(
            page.items
                .into_iter()
                .filter(|pod| eligible_status(&pod_status(pod))),
        );
        let Some(continuation) = page.metadata.continue_.filter(|token| !token.is_empty()) else {
            break;
        };
        if !continuations.insert(continuation.clone()) {
            return Err("The API repeated a list continuation token".to_owned());
        }
        params = params.continue_token(&continuation);
    }
    Ok(pods)
}

async fn bounded<T>(
    limits: Option<(Instant, Duration)>,
    request: impl Future<Output = Result<T, kube::Error>>,
) -> Result<T, String> {
    let Some((deadline, timeout)) = limits else {
        return request.await.map_err(|error| error.to_string());
    };
    let now = Instant::now();
    if now >= deadline {
        return Err("sanitize deadline exceeded".to_owned());
    }
    let request_deadline = now
        .checked_add(timeout)
        .ok_or_else(|| "Sanitize call timeout is too large".to_owned())?
        .min(deadline);
    tokio::time::timeout_at(request_deadline, request)
        .await
        .map_err(|_| {
            if request_deadline == deadline {
                "sanitize deadline exceeded"
            } else {
                "request timed out"
            }
            .to_owned()
        })?
        .map_err(|error| error.to_string())
}

fn delete_identity<'a>(
    pod: &'a Pod,
    scope: Option<&str>,
) -> Result<(&'a str, &'a str, DeleteParams), String> {
    let required = |field: Option<&'a str>, name| {
        field
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| format!("missing {name}; refusing an unguarded deletion"))
    };
    let namespace = required(pod.metadata.namespace.as_deref(), "namespace")?;
    let name = required(pod.metadata.name.as_deref(), "name")?;
    if scope.is_some_and(|scope| scope != namespace) {
        return Err("the returned Pod is outside the confirmed namespace".to_owned());
    }
    let uid = required(pod.metadata.uid.as_deref(), "UID")?;
    let resource_version = required(pod.metadata.resource_version.as_deref(), "resourceVersion")?;
    Ok((
        namespace,
        name,
        DeleteParams {
            grace_period_seconds: Some(0),
            preconditions: Some(Preconditions {
                uid: Some(uid.to_owned()),
                resource_version: Some(resource_version.to_owned()),
            }),
            ..DeleteParams::default()
        },
    ))
}

fn eligible_status(status: &str) -> bool {
    matches!(
        status,
        "Completed"
            | "Pending"
            | "CrashLoopBackOff"
            | "Error"
            | "ImagePullBackOff"
            | "ContainerStatusUnknown"
            | "Evicted"
            | "OOMKilled"
    )
}

/// Match K9s v0.51.0 render.PodStatus, including init and container reason precedence.
pub(super) fn pod_status(pod: &Pod) -> String {
    let Some(status) = &pod.status else {
        return if pod.metadata.deletion_timestamp.is_some() {
            "Terminating"
        } else {
            ""
        }
        .to_owned();
    };
    let mut reason = status
        .reason
        .as_deref()
        .filter(|reason| !reason.is_empty())
        .or(status.phase.as_deref())
        .unwrap_or_default()
        .to_owned();
    if status
        .conditions
        .as_deref()
        .unwrap_or_default()
        .iter()
        .any(|condition| {
            condition.type_ == "PodScheduled"
                && condition.reason.as_deref() == Some("SchedulingGated")
        })
    {
        "SchedulingGated".clone_into(&mut reason);
    }
    if let Some(initializing) = init_status(pod) {
        reason = initializing;
    } else {
        let mut has_running = false;
        for container in status
            .container_statuses
            .as_deref()
            .unwrap_or_default()
            .iter()
            .rev()
        {
            let Some(state) = &container.state else {
                continue;
            };
            if let Some(waiting) = state
                .waiting
                .as_ref()
                .and_then(|state| state.reason.as_deref())
                .filter(|reason| !reason.is_empty())
            {
                waiting.clone_into(&mut reason);
            } else if let Some(terminated) = &state.terminated {
                reason = termination_reason(terminated);
            } else if container.ready && state.running.is_some() {
                has_running = true;
            }
        }
        if reason == "Completed" && has_running {
            let ready = status
                .conditions
                .as_deref()
                .unwrap_or_default()
                .iter()
                .any(|condition| condition.type_ == "Ready" && condition.status == "True");
            if ready { "Running" } else { "NoReady" }.clone_into(&mut reason);
        }
    }
    if pod.metadata.deletion_timestamp.is_some() {
        if status.reason.as_deref() == Some("NodeLost") {
            "Unknown"
        } else {
            "Terminating"
        }
        .clone_into(&mut reason);
    }
    reason
}

fn termination_reason(state: &k8s_openapi::api::core::v1::ContainerStateTerminated) -> String {
    if let Some(reason) = state.reason.as_ref().filter(|reason| !reason.is_empty()) {
        reason.clone()
    } else if let Some(signal) = state.signal.filter(|signal| *signal != 0) {
        format!("Signal:{signal}")
    } else {
        format!("ExitCode:{}", state.exit_code)
    }
}

fn init_status(pod: &Pod) -> Option<String> {
    let spec = pod.spec.as_ref();
    let containers = spec
        .and_then(|spec| spec.init_containers.as_deref())
        .unwrap_or_default();
    let statuses = pod
        .status
        .as_ref()?
        .init_container_statuses
        .as_deref()
        .unwrap_or_default();
    for (index, container) in statuses.iter().enumerate() {
        let sidecar = containers.iter().any(|spec| {
            spec.name == container.name && spec.restart_policy.as_deref() == Some("Always")
        });
        if let Some(terminated) = container
            .state
            .as_ref()
            .and_then(|state| state.terminated.as_ref())
        {
            if terminated.exit_code == 0 || sidecar {
                continue;
            }
            return Some(format!("Init:{}", termination_reason(terminated)));
        }
        if let Some(waiting) = container
            .state
            .as_ref()
            .and_then(|state| state.waiting.as_ref())
            .and_then(|state| state.reason.as_deref())
            .filter(|reason| !reason.is_empty() && *reason != "PodInitializing")
        {
            return Some(format!("Init:{waiting}"));
        }
        if !sidecar {
            return Some(format!("Init:{index}/{}", containers.len()));
        }
    }
    None
}
