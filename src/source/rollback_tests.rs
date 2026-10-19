use super::*;
use crate::model::ResourceDescriptor;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

fn target() -> ResourceTarget {
    ResourceTarget {
        kind: ResourceDescriptor::new(
            "apps",
            "v1",
            "apps/v1",
            "ReplicaSet",
            "replicasets",
            true,
            crate::model::ResourceOperations::new(true, true, true),
        ),
        namespace: Some("team".to_owned()),
        name: "web-old".to_owned(),
    }
}

fn objects() -> (Value, Value) {
    let rs = json!({
        "apiVersion":"apps/v1", "kind":"ReplicaSet",
        "metadata": {"name":"web-old", "namespace":"team", "uid":"rs-uid", "resourceVersion":"100", "labels":{"app":"web"},
            "annotations": {REVISION:"2", "change-cause":"old release", "from-revision":"keep", "deployment.kubernetes.io/desired-replicas":"8"},
            "ownerReferences":[{"apiVersion":"apps/v1", "kind":"Deployment", "name":"web", "uid":"deployment-uid", "controller":true}]},
        "spec":{"replicas":0,"selector":{"matchLabels":{"app":"web","pod-template-hash":"old"}},
            "template":{"metadata":{"labels":{"app":"web","pod-template-hash":"old"},"annotations":{"template-note":"old"}},
                "spec":{"containers":[{"name":"web","image":"web:old","futureContainerField":{"keep":true}}],"futurePodField":{"keep":[1,2]}}}},
        "status":{"replicas":0}
    });
    let deployment = json!({
        "apiVersion":"apps/v1", "kind":"Deployment",
        "metadata":{"name":"web","namespace":"team","uid":"deployment-uid","resourceVersion":"200",
            "annotations":{REVISION:"3","deployment.kubernetes.io/desired-replicas":"4", "kubectl.kubernetes.io/last-applied-configuration":"keep-live", "change-cause":"new release","only-current":"remove"}},
        "spec":{"replicas":4,"selector":{"matchLabels":{"app":"web"}}, "strategy":{"type":"RollingUpdate"},
            "template":{"metadata":{"labels":{"app":"web"}},"spec":{"containers":[{"name":"web","image":"web:new"}]}}}
    });
    (rs, deployment)
}

fn reviewed(rs: &Value, deployment: &Value) -> RollbackSnapshot {
    snapshot(
        &target(),
        &serde_json::from_value(rs.clone()).unwrap(),
        &serde_json::from_value(deployment.clone()).unwrap(),
    )
    .unwrap()
}

#[derive(Debug)]
struct Request {
    method: String,
    path: String,
    content_type: String,
    body: Value,
}

async fn read_request(socket: &mut tokio::net::TcpStream) -> Request {
    let mut bytes = vec![];
    let mut buffer = [0; 4096];
    let (end, length) = loop {
        let count = socket.read(&mut buffer).await.unwrap();
        assert_ne!(count, 0);
        bytes.extend_from_slice(&buffer[..count]);
        if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            let headers = String::from_utf8_lossy(&bytes[..end]);
            let length: usize = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse().unwrap())
                })
                .unwrap_or(0);
            if bytes.len() >= end + 4 + length {
                break (end, length);
            }
        }
    };
    let headers = String::from_utf8_lossy(&bytes[..end]);
    let mut request = headers.lines().next().unwrap().split_whitespace();
    Request {
        method: request.next().unwrap().to_owned(),
        path: request.next().unwrap().to_owned(),
        content_type: headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-type")
                    .then(|| value.trim().to_owned())
            })
            .unwrap_or_default(),
        body: if length == 0 {
            Value::Null
        } else {
            serde_json::from_slice(&bytes[end + 4..end + 4 + length]).unwrap()
        },
    }
}

async fn mock_api(responses: Vec<(u16, Value)>) -> (Client, tokio::task::JoinHandle<Vec<Request>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let client = Client::try_from(kube::Config::new(
        format!("http://{address}").parse().unwrap(),
    ))
    .unwrap();
    let server = tokio::spawn(async move {
        let mut requests = vec![];
        for (status, body) in responses {
            let (mut socket, _) = tokio::time::timeout(Duration::from_secs(5), listener.accept())
                .await
                .unwrap()
                .unwrap();
            requests.push(read_request(&mut socket).await);
            let body = body.to_string();
            let response = format!(
                "HTTP/1.1 {status} Response\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        }
        requests
    });
    (client, server)
}

fn failure(code: u16) -> Value {
    json!({"apiVersion":"v1","kind":"Status","status":"Failure","reason":"Rejected","message":format!("fixture rejection {code}"),"code":code})
}

#[tokio::test]
async fn load_reads_exact_selected_rs_and_owner_and_preserves_unknown_fields() {
    let (rs, deployment) = objects();
    let (client, server) = mock_api(vec![(200, rs), (200, deployment)]).await;
    let snapshot = load_snapshot(client, &target()).await.unwrap();
    assert_eq!(snapshot.uid, "rs-uid");
    assert_eq!(snapshot.resource_version, "100");
    assert_eq!(snapshot.deployment, "web");
    assert_eq!(snapshot.deployment_uid, "deployment-uid");
    assert_eq!(snapshot.deployment_resource_version, "200");
    assert_eq!(snapshot.revision, 2);
    assert!(!snapshot.already_matches);
    assert_eq!(
        snapshot.template.pointer("/spec/futurePodField/keep"),
        Some(&json!([1, 2]))
    );
    assert_eq!(
        snapshot
            .template
            .pointer("/spec/containers/0/futureContainerField/keep"),
        Some(&json!(true))
    );
    assert!(
        snapshot
            .template
            .pointer("/metadata/labels/pod-template-hash")
            .is_none()
    );
    let requests = server.await.unwrap();
    assert_eq!(
        requests
            .iter()
            .map(|request| (request.method.as_str(), request.path.as_str()))
            .collect::<Vec<_>>(),
        [
            ("GET", "/apis/apps/v1/namespaces/team/replicasets/web-old"),
            ("GET", "/apis/apps/v1/namespaces/team/deployments/web"),
        ]
    );
}

#[test]
fn annotations_follow_rollbacker_rules_and_replace_live_non_bookkeeping_values() {
    let (mut rs, mut deployment) = objects();
    for key in PRESERVED_ANNOTATIONS {
        rs["metadata"]["annotations"][key] = json!("rs-copy-must-not-win");
        deployment["metadata"]["annotations"][key] = json!("deployment-copy");
    }
    rs["metadata"]["annotations"][REVISION] = json!("2");
    let snapshot = reviewed(&rs, &deployment);
    for key in PRESERVED_ANNOTATIONS {
        assert_eq!(
            snapshot.annotations.get(key).map(String::as_str),
            Some("deployment-copy")
        );
    }
    assert_eq!(snapshot.annotations["change-cause"], "old release");
    assert_eq!(snapshot.annotations["from-revision"], "keep");
    assert!(!snapshot.annotations.contains_key("only-current"));
}

#[tokio::test]
async fn rollback_revalidates_then_sends_one_atomic_json_patch() {
    let (rs, deployment) = objects();
    let snapshot = reviewed(&rs, &deployment);
    let (client, server) = mock_api(vec![
        (200, rs),
        (200, deployment.clone()),
        (200, deployment),
    ])
    .await;
    assert!(
        rollback(client, &snapshot, None)
            .await
            .unwrap()
            .contains("revision 2")
    );
    let requests = server.await.unwrap();
    assert_eq!(requests.len(), 3);
    let patch = &requests[2];
    assert_eq!(patch.method, "PATCH");
    assert_eq!(
        patch.path.trim_end_matches('?'),
        "/apis/apps/v1/namespaces/team/deployments/web"
    );
    assert_eq!(patch.content_type, "application/json-patch+json");
    assert_eq!(
        patch.body,
        json!([
            {"op":"test","path":"/metadata/uid","value":"deployment-uid"},
            {"op":"test","path":"/metadata/resourceVersion","value":"200"},
            {"op":"replace","path":"/spec/template","value":snapshot.template},
            {"op":"add","path":"/metadata/annotations","value":snapshot.annotations},
        ])
    );
}

#[tokio::test]
async fn matching_template_skips_patch_even_when_revision_annotations_differ() {
    let (rs, mut deployment) = objects();
    deployment["spec"]["template"] = rs["spec"]["template"].clone();
    deployment["spec"]["template"]["metadata"]["labels"][HASH] = json!("different");
    let snapshot = reviewed(&rs, &deployment);
    assert!(snapshot.already_matches);
    let (client, server) = mock_api(vec![(200, rs), (200, deployment)]).await;
    assert!(
        rollback(client, &snapshot, None)
            .await
            .unwrap()
            .starts_with("Skipped rollback")
    );
    assert_eq!(server.await.unwrap().len(), 2);
}

#[test]
fn template_comparison_preserves_unknown_fields_and_normalizes_only_known_metadata_maps() {
    assert!(equivalent_template(
        &json!({"metadata":{"labels":{},"annotations":{}},"spec":{}}),
        &json!({"metadata":null,"spec":{}})
    ));
    assert!(!equivalent_template(
        &json!({"spec":{"newField":"old"}}),
        &json!({"spec":{"newField":"new"}})
    ));
    for value in [json!({}), json!([]), Value::Null] {
        assert!(!equivalent_template(
            &json!({"spec":{"unknown":value}}),
            &json!({"spec":{}})
        ));
    }
}

#[tokio::test]
async fn missing_or_active_revision_guards_stop_before_deployment_read() {
    for (path, value, expected) in [
        ("/metadata/uid", json!(""), "UID"),
        ("/metadata/resourceVersion", json!(""), "resourceVersion"),
        ("/metadata/name", json!("wrong"), "identity"),
        ("/metadata/namespace", json!("wrong"), "identity"),
        ("/status/replicas", json!(1), "zero replicas"),
        ("/status/replicas", Value::Null, "zero replicas"),
        (
            "/metadata/annotations/deployment.kubernetes.io~1revision",
            json!("0"),
            "positive",
        ),
        (
            "/metadata/annotations/deployment.kubernetes.io~1revision",
            json!("-1"),
            "positive",
        ),
        (
            "/metadata/annotations/deployment.kubernetes.io~1revision",
            json!("bad"),
            "positive",
        ),
        (
            "/metadata/ownerReferences/0/controller",
            json!(false),
            "controlling",
        ),
        (
            "/metadata/ownerReferences/0/controller",
            Value::Null,
            "controlling",
        ),
        ("/metadata/ownerReferences/0/uid", json!(""), "name and UID"),
        (
            "/metadata/ownerReferences/0/kind",
            json!("StatefulSet"),
            "apps Deployment",
        ),
        (
            "/metadata/ownerReferences/0/apiVersion",
            json!("other/v1"),
            "apps Deployment",
        ),
    ] {
        let (mut rs, _) = objects();
        *rs.pointer_mut(path).unwrap() = value;
        let (client, server) = mock_api(vec![(200, rs)]).await;
        let error = load_snapshot(client, &target()).await.unwrap_err();
        assert!(error.contains(expected), "{path}: {error}");
        assert_eq!(server.await.unwrap().len(), 1);
    }
}

#[tokio::test]
async fn paused_recreated_mismatched_or_incomplete_deployment_cannot_be_loaded() {
    for (path, value, expected) in [
        ("/metadata/uid", json!("recreated"), "recreated"),
        ("/metadata/resourceVersion", json!(""), "resourceVersion"),
        ("/metadata/namespace", json!("wrong"), "identity"),
        (
            "/spec/selector/matchLabels/app",
            json!("unrelated"),
            "selector",
        ),
        ("/spec/template", Value::Null, "template"),
    ] {
        let (rs, mut deployment) = objects();
        *deployment.pointer_mut(path).unwrap() = value;
        let (client, server) = mock_api(vec![(200, rs), (200, deployment)]).await;
        assert!(
            load_snapshot(client, &target())
                .await
                .unwrap_err()
                .contains(expected)
        );
        assert_eq!(server.await.unwrap().len(), 2);
    }
    let (rs, mut deployment) = objects();
    deployment["spec"]["paused"] = json!(true);
    let (client, server) = mock_api(vec![(200, rs), (200, deployment)]).await;
    assert!(
        load_snapshot(client, &target())
            .await
            .unwrap_err()
            .contains("paused")
    );
    assert_eq!(server.await.unwrap().len(), 2);
}

#[tokio::test]
async fn changed_rs_or_deployment_requires_a_new_confirmation_and_never_patches() {
    for (rs_changed, path, value) in [
        (true, "/metadata/uid", json!("new-rs")),
        (true, "/metadata/resourceVersion", json!("101")),
        (
            true,
            "/metadata/annotations/deployment.kubernetes.io~1revision",
            json!("4"),
        ),
        (false, "/metadata/resourceVersion", json!("201")),
    ] {
        let (mut rs, mut deployment) = objects();
        let snapshot = reviewed(&rs, &deployment);
        if rs_changed {
            *rs.pointer_mut(path).unwrap() = value;
        } else {
            *deployment.pointer_mut(path).unwrap() = value;
        }
        let (client, server) = mock_api(vec![(200, rs), (200, deployment)]).await;
        assert!(
            rollback(client, &snapshot, None)
                .await
                .unwrap_err()
                .contains("changed")
        );
        assert_eq!(server.await.unwrap().len(), 2);
    }
}

#[tokio::test]
async fn forged_snapshot_replacement_content_is_rejected_against_live_objects() {
    let (rs, deployment) = objects();
    let mut snapshot = reviewed(&rs, &deployment);
    snapshot.template["spec"]["containers"][0]["image"] = json!("injected");
    let (client, server) = mock_api(vec![(200, rs), (200, deployment)]).await;
    assert!(
        rollback(client, &snapshot, None)
            .await
            .unwrap_err()
            .contains("changed")
    );
    assert_eq!(server.await.unwrap().len(), 2);
}

#[tokio::test]
async fn permission_and_patch_conflicts_report_errors_without_retries() {
    let (rs, deployment) = objects();
    let (client, server) = mock_api(vec![(403, failure(403))]).await;
    assert!(
        load_snapshot(client, &target())
            .await
            .unwrap_err()
            .contains("403")
    );
    assert_eq!(server.await.unwrap().len(), 1);
    for code in [403, 409, 422] {
        let snapshot = reviewed(&rs, &deployment);
        let (client, server) = mock_api(vec![
            (200, rs.clone()),
            (200, deployment.clone()),
            (code, failure(code)),
        ])
        .await;
        let error = rollback(client, &snapshot, None).await.unwrap_err();
        assert!(error.contains(&format!("fixture rejection {code}")));
        if code != 403 {
            assert!(error.contains("reload"));
        }
        let requests = server.await.unwrap();
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.method == "PATCH")
                .count(),
            1
        );
    }
}

#[tokio::test]
async fn malformed_public_snapshot_and_wrong_kind_are_rejected_before_network() {
    let (client, server) = mock_api(vec![]).await;
    let (rs, deployment) = objects();
    let mut snapshot = reviewed(&rs, &deployment);
    snapshot.uid.clear();
    assert!(
        rollback(client.clone(), &snapshot, None)
            .await
            .unwrap_err()
            .contains("identities")
    );
    let mut target = target();
    target.kind = crate::model::ResourceKind::Deployments.into();
    assert!(
        load_snapshot(client.clone(), &target)
            .await
            .unwrap_err()
            .contains("ReplicaSet")
    );
    target.kind = snapshot.target.kind;
    target.namespace = None;
    assert!(
        load_snapshot(client, &target)
            .await
            .unwrap_err()
            .contains("namespace")
    );
    assert_eq!(server.await.unwrap().len(), 0);
}

#[tokio::test]
async fn active_or_reparented_rs_after_confirmation_never_patches() {
    for owner_changed in [false, true] {
        let (mut rs, deployment) = objects();
        let snapshot = reviewed(&rs, &deployment);
        let responses = if owner_changed {
            rs["metadata"]["ownerReferences"][0]["uid"] = json!("replacement-deployment");
            vec![(200, rs), (200, deployment)]
        } else {
            rs["status"]["replicas"] = json!(2);
            vec![(200, rs)]
        };
        let (client, server) = mock_api(responses).await;
        let error = rollback(client, &snapshot, None).await.unwrap_err();
        assert!(error.contains(if owner_changed {
            "recreated"
        } else {
            "zero replicas"
        }));
        assert!(
            server
                .await
                .unwrap()
                .iter()
                .all(|request| request.method == "GET")
        );
    }
}

#[tokio::test]
async fn missing_annotations_are_supported_and_template_fields_are_preserved() {
    let (rs, mut deployment) = objects();
    deployment["metadata"]
        .as_object_mut()
        .unwrap()
        .remove("annotations");
    let snapshot = reviewed(&rs, &deployment);
    assert!(!snapshot.annotations.contains_key(REVISION));
    let (client, server) = mock_api(vec![
        (200, rs),
        (200, deployment.clone()),
        (200, deployment),
    ])
    .await;
    rollback(client, &snapshot, None).await.unwrap();
    let requests = server.await.unwrap();
    assert_eq!(requests[2].body[3]["op"], "add");
    assert_eq!(
        requests[2].body[2]["value"]["spec"]["futurePodField"],
        json!({"keep":[1,2]})
    );
}

#[tokio::test]
async fn mutation_timeout_reports_unknown_outcome_and_does_not_retry() {
    let (rs, deployment) = objects();
    let snapshot = reviewed(&rs, &deployment);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let client = Client::try_from(kube::Config::new(
        format!("http://{address}").parse().unwrap(),
    ))
    .unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let mut requests = vec![];
        for response in [Some(rs), Some(deployment), None] {
            let (mut socket, _) = listener.accept().await.unwrap();
            requests.push(read_request(&mut socket).await);
            if let Some(response) = response {
                let body = response.to_string();
                socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
            } else {
                let _ = stopped.await;
                break;
            }
        }
        requests
    });
    let error = rollback(client, &snapshot, Some(Duration::from_secs(1)))
        .await
        .unwrap_err();
    let _ = stop.send(());
    assert!(error.contains("outcome is unknown"), "{error}");
    assert!(error.contains("Check the Deployment state"));
    let requests = server.await.unwrap();
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[2].method, "PATCH");
}

#[tokio::test]
async fn invalid_timeouts_are_rejected_before_http_without_panicking() {
    let (rs, deployment) = objects();
    let snapshot = reviewed(&rs, &deployment);
    let (client, server) = mock_api(vec![]).await;
    for timeout in [Duration::ZERO, Duration::MAX] {
        let error = rollback(client.clone(), &snapshot, Some(timeout))
            .await
            .unwrap_err();
        assert!(error.contains("request timeout"));
    }
    assert_eq!(server.await.unwrap().len(), 0);
}

#[tokio::test]
async fn dropped_patch_response_reports_unknown_outcome_without_retrying() {
    let (rs, deployment) = objects();
    let snapshot = reviewed(&rs, &deployment);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let client = Client::try_from(kube::Config::new(
        format!("http://{address}").parse().unwrap(),
    ))
    .unwrap();
    let server = tokio::spawn(async move {
        let mut requests = vec![];
        for response in [Some(rs), Some(deployment), None] {
            let (mut socket, _) = listener.accept().await.unwrap();
            requests.push(read_request(&mut socket).await);
            if let Some(response) = response {
                let body = response.to_string();
                socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
            }
            // After reading the PATCH body, drop the connection without a
            // response: the client cannot determine whether it was applied.
        }
        requests
    });
    let error = rollback(client, &snapshot, Some(Duration::from_secs(5)))
        .await
        .unwrap_err();
    assert!(error.contains("outcome may be unknown"), "{error}");
    assert!(error.contains("Check the Deployment state"));
    let requests = server.await.unwrap();
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[2].method, "PATCH");
}
