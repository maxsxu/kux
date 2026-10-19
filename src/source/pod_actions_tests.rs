use serde_json::{Value, json};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

use super::*;

fn object(namespace: &str, name: &str, phase: &str) -> Value {
    json!({
        "apiVersion": "v1", "kind": "Pod",
        "metadata": {"namespace": namespace, "name": name, "uid": format!("uid-{name}"), "resourceVersion": "42"},
        "spec": {"containers": [{"name": "main", "image": "main:v1"}]},
        "status": {"phase": phase}
    })
}

fn container(name: &str, state: Value, ready: bool) -> Value {
    let mut result = json!({"name": name, "image": "main:v1", "imageID": "", "ready": ready, "restartCount": 0, "started": false});
    result["state"] = state;
    result
}

fn status(value: &Value) -> String {
    pod_status(&serde_json::from_value(value.clone()).unwrap())
}

fn list(items: Vec<Value>, continuation: &str) -> Value {
    let mut result =
        json!({"apiVersion": "v1", "kind": "PodList", "metadata": {"continue": continuation}});
    result["items"] = Value::Array(items);
    result
}

fn success() -> Value {
    json!({"apiVersion": "v1", "kind": "Status", "status": "Success", "code": 200})
}

fn failure(code: u16, message: &str) -> Value {
    json!({"apiVersion": "v1", "kind": "Status", "status": "Failure", "reason": "Rejected", "code": code, "message": message})
}

#[test]
fn eligibility_is_exact_and_does_not_confuse_raw_phases_with_computed_reasons() {
    for expected in [
        "Completed",
        "Pending",
        "CrashLoopBackOff",
        "Error",
        "ImagePullBackOff",
        "ContainerStatusUnknown",
        "Evicted",
        "OOMKilled",
    ] {
        let mut pod = object("team", "pod", "Running");
        pod["status"]["reason"] = json!(expected);
        assert_eq!(status(&pod), expected);
        assert!(eligible_status(&status(&pod)));
    }
    for excluded in [
        "Succeeded",
        "Failed",
        "Unknown",
        "Running",
        "NoReady",
        "SchedulingGated",
        "ErrImagePull",
        "Init:Error",
        "ExitCode:1",
        "Signal:9",
        "Terminating",
    ] {
        let pod = object("team", "pod", excluded);
        assert_eq!(status(&pod), excluded);
        assert!(!eligible_status(&status(&pod)), "{excluded}");
    }
    let mut pod = object("team", "pod", "Succeeded");
    pod["status"]["containerStatuses"] = json!([container(
        "main",
        json!({"terminated": {"exitCode": 0, "reason": "Completed"}}),
        false
    )]);
    assert_eq!(status(&pod), "Completed");
}

#[test]
fn scheduling_gate_overrides_top_level_reason_but_current_container_reason_wins() {
    let mut pod = object("team", "pod", "Pending");
    pod["status"]["reason"] = json!("Evicted");
    assert_eq!(status(&pod), "Evicted");
    for condition_status in ["False", "True", "Unknown"] {
        pod["status"]["conditions"] = json!([{"type": "PodScheduled", "status": condition_status, "reason": "SchedulingGated"}]);
        assert_eq!(status(&pod), "SchedulingGated");
    }
    pod["status"]["containerStatuses"] = json!([container(
        "main",
        json!({"waiting": {"reason": "Error"}}),
        false
    )]);
    assert_eq!(status(&pod), "Error");
}

#[test]
fn regular_reasons_use_reverse_iteration_and_current_state_only() {
    let mut pod = object("team", "pod", "Running");
    pod["status"]["containerStatuses"] = json!([
        container(
            "first",
            json!({"waiting": {"reason": "ImagePullBackOff"}}),
            false
        ),
        container(
            "last",
            json!({"terminated": {"exitCode": 137, "reason": "OOMKilled"}}),
            false
        )
    ]);
    assert_eq!(status(&pod), "ImagePullBackOff");
    for (state, expected) in [
        (json!({"exitCode": 0, "reason": "Error"}), "Error"),
        (json!({"exitCode": 1, "signal": 9}), "Signal:9"),
        (json!({"exitCode": 2, "signal": 0}), "ExitCode:2"),
        (json!({"exitCode": 0, "reason": ""}), "ExitCode:0"),
    ] {
        pod["status"]["containerStatuses"] =
            json!([container("main", json!({"terminated": state}), false)]);
        assert_eq!(status(&pod), expected);
    }
    let mut main = container("main", json!({"running": {}}), true);
    main["lastState"] = json!({"terminated": {"exitCode": 137, "reason": "OOMKilled"}});
    pod["status"]["containerStatuses"] = json!([main]);
    assert_eq!(status(&pod), "Running");
}

#[test]
fn init_statuses_use_forward_order_and_block_regular_reasons() {
    let mut pod = object("team", "pod", "Pending");
    pod["spec"]["initContainers"] = json!([{"name": "first"}, {"name": "second"}]);
    pod["status"]["containerStatuses"] = json!([container(
        "main",
        json!({"waiting": {"reason": "Error"}}),
        false
    )]);
    pod["status"]["initContainerStatuses"] = json!([
        container(
            "first",
            json!({"waiting": {"reason": "ImagePullBackOff"}}),
            false
        ),
        container(
            "second",
            json!({"waiting": {"reason": "CrashLoopBackOff"}}),
            false
        )
    ]);
    assert_eq!(status(&pod), "Init:ImagePullBackOff");
    assert!(!eligible_status(&status(&pod)));
    for (state, expected) in [
        (
            json!({"terminated": {"exitCode": 0, "reason": "Error"}}),
            "Init:CrashLoopBackOff",
        ),
        (
            json!({"terminated": {"exitCode": 1, "reason": "Error"}}),
            "Init:Error",
        ),
        (
            json!({"terminated": {"exitCode": 1, "signal": 9}}),
            "Init:Signal:9",
        ),
        (json!({"terminated": {"exitCode": 2}}), "Init:ExitCode:2"),
        (
            json!({"waiting": {"reason": "PodInitializing"}}),
            "Init:0/2",
        ),
        (json!({"running": {}}), "Init:0/2"),
    ] {
        pod["status"]["initContainerStatuses"][0]["state"] = state;
        assert_eq!(status(&pod), expected);
    }
    pod["status"]["initContainerStatuses"][0]["state"] = json!({"terminated": {"exitCode": 0}});
    pod["status"]["initContainerStatuses"][1]["state"] = json!({});
    assert_eq!(status(&pod), "Init:1/2");
}

#[test]
fn restartable_init_sidecars_are_matched_by_name_and_keep_waiting_failures() {
    let mut pod = object("team", "pod", "Succeeded");
    pod["spec"]["initContainers"] = json!([{"name": "sidecar", "restartPolicy": "Always"}]);
    pod["status"]["containerStatuses"] = json!([container(
        "main",
        json!({"terminated": {"exitCode": 0, "reason": "Completed"}}),
        false
    )]);
    for state in [
        json!({"terminated": {"exitCode": 137, "reason": "Error"}}),
        json!({"running": {}}),
        json!({"waiting": {"reason": "PodInitializing"}}),
        json!({}),
    ] {
        pod["status"]["initContainerStatuses"] = json!([container("sidecar", state, false)]);
        assert_eq!(status(&pod), "Completed");
    }
    pod["status"]["initContainerStatuses"][0]["state"] =
        json!({"waiting": {"reason": "ImagePullBackOff"}});
    assert_eq!(status(&pod), "Init:ImagePullBackOff");
    pod["status"]["initContainerStatuses"][0] = container(
        "different-name",
        json!({"terminated": {"exitCode": 137, "reason": "Error"}}),
        false,
    );
    assert_eq!(status(&pod), "Init:Error");
}

#[test]
fn completed_pods_with_ready_running_containers_use_the_pod_ready_condition() {
    let mut pod = object("team", "pod", "Running");
    pod["status"]["containerStatuses"] = json!([
        container(
            "main",
            json!({"terminated": {"exitCode": 0, "reason": "Completed"}}),
            false
        ),
        container("sidecar", json!({"running": {}}), true)
    ]);
    assert_eq!(status(&pod), "NoReady");
    pod["status"]["conditions"] = json!([{"type": "Ready", "status": "True"}]);
    assert_eq!(status(&pod), "Running");
    pod["status"]["containerStatuses"][1]["ready"] = json!(false);
    assert_eq!(status(&pod), "Completed");
}

#[test]
fn deletion_overrides_even_blocking_init_states_and_node_lost_is_unknown() {
    let mut pod = object("team", "pod", "Pending");
    pod["spec"]["initContainers"] = json!([{"name": "setup"}]);
    pod["status"]["initContainerStatuses"] = json!([container(
        "setup",
        json!({"waiting": {"reason": "Error"}}),
        false
    )]);
    pod["metadata"]["deletionTimestamp"] = json!("2026-10-05T00:00:00Z");
    assert_eq!(status(&pod), "Terminating");
    pod["status"]["reason"] = json!("NodeLost");
    assert_eq!(status(&pod), "Unknown");
    assert!(!eligible_status(&status(&pod)));
    assert_eq!(pod_status(&Pod::default()), "");
}

#[derive(Debug)]
struct Request {
    method: String,
    path: String,
    body: Value,
}

async fn read_request(stream: &mut tokio::net::TcpStream) -> Request {
    let mut bytes = Vec::new();
    let mut chunk = [0; 4096];
    let (end, length) = loop {
        let size = stream.read(&mut chunk).await.unwrap();
        assert_ne!(size, 0);
        bytes.extend_from_slice(&chunk[..size]);
        assert!(bytes.len() < 65_536);
        if let Some(end) = bytes.windows(4).position(|value| value == b"\r\n\r\n") {
            let headers = String::from_utf8_lossy(&bytes[..end]);
            let length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().unwrap())
                })
                .unwrap_or(0);
            if bytes.len() >= end + 4 + length {
                break (end, length);
            }
        }
    };
    let header = String::from_utf8_lossy(&bytes[..end]);
    let mut first = header.lines().next().unwrap().split_whitespace();
    Request {
        method: first.next().unwrap().to_owned(),
        path: first.next().unwrap().to_owned(),
        body: if length == 0 {
            Value::Null
        } else {
            serde_json::from_slice(&bytes[end + 4..end + 4 + length]).unwrap()
        },
    }
}

async fn mock_api(responses: Vec<(u16, Value)>) -> (Client, tokio::task::JoinHandle<Vec<Request>>) {
    mock_api_with_delays(
        responses
            .into_iter()
            .map(|(code, body)| (Duration::ZERO, code, body))
            .collect(),
    )
    .await
}

async fn mock_api_with_delays(
    responses: Vec<(Duration, u16, Value)>,
) -> (Client, tokio::task::JoinHandle<Vec<Request>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let client = Client::try_from(kube::Config::new(
        format!("http://{}", listener.local_addr().unwrap())
            .parse()
            .unwrap(),
    ))
    .unwrap();
    let task = tokio::spawn(async move {
        let mut requests = Vec::new();
        for (delay, code, body) in responses {
            let (mut stream, _) = tokio::time::timeout(Duration::from_secs(5), listener.accept())
                .await
                .unwrap()
                .unwrap();
            requests.push(read_request(&mut stream).await);
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            let body = body.to_string();
            let response = format!(
                "HTTP/1.1 {code} Response\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            // A timed-out request may have already closed its connection.
            let _ = stream.write_all(response.as_bytes()).await;
        }
        requests
    });
    (client, task)
}

#[tokio::test]
async fn namespace_sanitize_lists_without_selectors_and_deletes_sequentially_with_preconditions() {
    let (client, server) = mock_api(vec![
        (
            200,
            list(
                vec![
                    object("team", "running", "Running"),
                    object("team", "pending", "Pending"),
                    object("team", "error", "Error"),
                ],
                "",
            ),
        ),
        (200, success()),
        (200, success()),
    ])
    .await;
    assert_eq!(
        sanitize_pods(
            client,
            Some("team".to_owned()),
            Some(Duration::from_secs(120))
        )
        .await
        .unwrap(),
        SanitizeReport {
            deleted: 2,
            eligible: 2,
            failure: None
        }
    );
    let requests = server.await.unwrap();
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[0].method, "GET");
    assert!(
        requests[0]
            .path
            .starts_with("/api/v1/namespaces/team/pods?")
    );
    assert!(!requests[0].path.contains("labelSelector"));
    assert!(!requests[0].path.contains("fieldSelector"));
    for (request, name) in requests[1..].iter().zip(["pending", "error"]) {
        assert_eq!(request.method, "DELETE");
        assert_eq!(
            request.path.split('?').next().unwrap(),
            format!("/api/v1/namespaces/team/pods/{name}")
        );
        assert_eq!(request.body["gracePeriodSeconds"], 0);
        assert_eq!(
            request.body["preconditions"],
            json!({"uid": format!("uid-{name}"), "resourceVersion": "42"})
        );
        assert!(request.body.get("propagationPolicy").is_none());
    }
}

#[tokio::test]
async fn all_namespaces_pagination_finishes_listing_before_deleting_exact_pod_namespaces() {
    let (client, server) = mock_api(vec![
        (
            200,
            list(vec![object("team-a", "one", "Pending")], "next-page"),
        ),
        (200, list(vec![object("team-b", "two", "Evicted")], "")),
        (200, success()),
        (200, success()),
    ])
    .await;
    assert_eq!(
        sanitize_pods(client, None, Some(Duration::from_secs(120)))
            .await
            .unwrap(),
        SanitizeReport {
            deleted: 2,
            eligible: 2,
            failure: None
        }
    );
    let requests = server.await.unwrap();
    assert_eq!(requests.len(), 4);
    for request in &requests[..2] {
        assert_eq!(request.method, "GET");
        assert_eq!(request.path.split('?').next(), Some("/api/v1/pods"));
    }
    assert!(requests[1].path.contains("continue=next-page"));
    assert_eq!(
        requests[2].path.split('?').next(),
        Some("/api/v1/namespaces/team-a/pods/one")
    );
    assert_eq!(
        requests[3].path.split('?').next(),
        Some("/api/v1/namespaces/team-b/pods/two")
    );
}

#[tokio::test]
async fn first_delete_failure_returns_partial_counts_and_never_attempts_remaining_pods() {
    let (client, server) = mock_api(vec![
        (
            200,
            list(
                vec![
                    object("team", "one", "Pending"),
                    object("team", "two", "Error"),
                    object("team", "three", "Evicted"),
                ],
                "",
            ),
        ),
        (200, success()),
        (403, failure(403, "delete is forbidden")),
    ])
    .await;
    let report = sanitize_pods(
        client,
        Some("team".to_owned()),
        Some(Duration::from_secs(120)),
    )
    .await
    .unwrap();
    assert_eq!((report.deleted, report.eligible), (1, 3));
    let error = report.failure.unwrap();
    assert!(error.contains("Pod team/two") && error.contains("forbidden"));
    let requests = server.await.unwrap();
    assert_eq!(requests.len(), 3);
    assert!(requests[2].path.contains("/pods/two"));
}

#[tokio::test]
async fn stale_uid_conflicts_stop_without_retry_and_keep_the_listed_preconditions() {
    let (client, server) = mock_api(vec![
        (
            200,
            list(
                vec![
                    object("team", "replaced", "Pending"),
                    object("team", "later", "Pending"),
                ],
                "",
            ),
        ),
        (409, failure(409, "UID precondition failed")),
    ])
    .await;
    let report = sanitize_pods(client, None, Some(Duration::from_secs(120)))
        .await
        .unwrap();
    assert_eq!((report.deleted, report.eligible), (0, 2));
    assert!(report.failure.unwrap().contains("UID precondition failed"));
    let requests = server.await.unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[1].body["preconditions"]["uid"], "uid-replaced");
    assert_eq!(requests[1].body["preconditions"]["resourceVersion"], "42");
}

#[tokio::test]
async fn unknown_identity_or_namespace_mismatch_never_sends_delete() {
    for field in ["namespace", "name", "uid", "resourceVersion"] {
        let mut pod = object("team", "unsafe", "Pending");
        pod["metadata"].as_object_mut().unwrap().remove(field);
        let (client, server) = mock_api(vec![(200, list(vec![pod], ""))]).await;
        let report = sanitize_pods(
            client,
            Some("team".to_owned()),
            Some(Duration::from_secs(120)),
        )
        .await
        .unwrap();
        assert_eq!((report.deleted, report.eligible), (0, 1));
        assert!(
            report
                .failure
                .unwrap()
                .contains("refusing an unguarded deletion")
        );
        assert_eq!(server.await.unwrap().len(), 1);
    }
    let (client, server) = mock_api(vec![(
        200,
        list(vec![object("other", "unsafe", "Pending")], ""),
    )])
    .await;
    let report = sanitize_pods(
        client,
        Some("team".to_owned()),
        Some(Duration::from_secs(120)),
    )
    .await
    .unwrap();
    assert_eq!(report.deleted, 0);
    assert!(
        report
            .failure
            .unwrap()
            .contains("outside the confirmed namespace")
    );
    assert_eq!(server.await.unwrap().len(), 1);
}

#[tokio::test]
async fn no_eligible_pods_succeeds_without_delete_and_list_failures_delete_nothing() {
    let (client, server) = mock_api(vec![(
        200,
        list(
            vec![
                object("team", "healthy", "Running"),
                object("team", "finished", "Succeeded"),
            ],
            "",
        ),
    )])
    .await;
    assert_eq!(
        sanitize_pods(client, None, Some(Duration::from_secs(120)))
            .await
            .unwrap(),
        SanitizeReport {
            deleted: 0,
            eligible: 0,
            failure: None
        }
    );
    assert_eq!(server.await.unwrap().len(), 1);
    let (client, server) = mock_api(vec![
        (
            200,
            list(vec![object("team", "eligible", "Pending")], "next"),
        ),
        (403, failure(403, "list is forbidden")),
    ])
    .await;
    let error = sanitize_pods(client, None, Some(Duration::from_secs(120)))
        .await
        .unwrap_err();
    assert!(error.contains("Unable to list Pods") && error.contains("forbidden"));
    let requests = server.await.unwrap();
    assert_eq!(requests.len(), 2);
    assert!(requests.iter().all(|request| request.method == "GET"));
}

#[tokio::test]
async fn request_timeout_and_overall_deadline_are_independently_bounded() {
    let never = || std::future::pending::<Result<(), kube::Error>>();
    let error = bounded(
        Some((
            Instant::now() + Duration::from_secs(5),
            Duration::from_millis(10),
        )),
        never(),
    )
    .await
    .unwrap_err();
    assert_eq!(error, "request timed out");
    let error = bounded(
        Some((
            Instant::now() + Duration::from_millis(10),
            Duration::from_secs(5),
        )),
        never(),
    )
    .await
    .unwrap_err();
    assert_eq!(error, "sanitize deadline exceeded");
    let error = bounded(Some((Instant::now(), Duration::from_secs(5))), never())
        .await
        .unwrap_err();
    assert_eq!(error, "sanitize deadline exceeded");
}

#[test]
fn call_timeout_derives_the_five_times_overall_budget() {
    let timeouts = Timeouts::from_call_timeout(Duration::from_secs(120)).unwrap();
    assert_eq!(timeouts.request, Duration::from_secs(120));
    assert_eq!(timeouts.total, Duration::from_secs(600));
}

#[tokio::test]
async fn invalid_timeouts_return_before_any_api_request() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let client = Client::try_from(kube::Config::new(
        format!("http://{}", listener.local_addr().unwrap())
            .parse()
            .unwrap(),
    ))
    .unwrap();
    for (duration, expected) in [
        (Duration::ZERO, "must be positive or disabled"),
        (Duration::MAX, "too large"),
        (Duration::from_secs(u64::MAX / 5), "too large"),
    ] {
        let error = sanitize_pods(client.clone(), None, Some(duration))
            .await
            .unwrap_err();
        assert!(error.contains(expected), "{error}");
    }
    assert!(
        tokio::time::timeout(Duration::from_millis(20), listener.accept())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn overall_deadline_reports_completed_deletes_and_stops_remaining_pods() {
    let (client, server) = mock_api_with_delays(vec![
        (
            Duration::ZERO,
            200,
            list(
                vec![
                    object("team", "first", "Pending"),
                    object("team", "second", "Pending"),
                    object("team", "third", "Pending"),
                ],
                "",
            ),
        ),
        (Duration::ZERO, 200, success()),
        (Duration::from_millis(200), 200, success()),
    ])
    .await;
    let report = sanitize_with_timeouts(
        client,
        Some("team".to_owned()),
        Some(Timeouts {
            request: Duration::from_millis(500),
            total: Duration::from_millis(100),
        }),
    )
    .await
    .unwrap();
    assert_eq!((report.deleted, report.eligible), (1, 3));
    assert_eq!(
        report.failure.as_deref(),
        Some("Pod team/second: sanitize deadline exceeded")
    );
    let requests = server.await.unwrap();
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[0].method, "GET");
    assert!(requests[1].path.contains("/pods/first"));
    assert!(requests[2].path.contains("/pods/second"));
}

#[tokio::test]
async fn disabled_timeouts_allow_delayed_list_and_delete_responses() {
    let (client, server) = mock_api_with_delays(vec![
        (
            Duration::from_millis(30),
            200,
            list(vec![object("team", "pending", "Pending")], ""),
        ),
        (Duration::from_millis(30), 200, success()),
    ])
    .await;
    assert_eq!(
        sanitize_pods(client, None, None).await.unwrap(),
        SanitizeReport {
            deleted: 1,
            eligible: 1,
            failure: None,
        }
    );
    assert_eq!(server.await.unwrap().len(), 2);
}
