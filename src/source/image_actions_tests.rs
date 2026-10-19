use std::time::Duration;

use crate::model::{ResourceKind, ResourceOperations};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

use super::*;

#[derive(Debug)]
struct Request {
    method: String,
    path: String,
    content_type: String,
    body: Value,
}

fn target(kind: ResourceKind) -> ResourceTarget {
    ResourceTarget {
        kind: kind.into(),
        namespace: Some("selected-team".to_owned()),
        name: "worker".to_owned(),
    }
}

fn object(target: &ResourceTarget) -> Value {
    let pod_spec = json!({
        "serviceAccountName": "keep-account", "nodeSelector": {"zone": "west"},
        "initContainers": [{"name": "setup", "image": "setup:v1", "command": ["prepare"], "restartPolicy": "Always"}],
        "containers": [
            {"name": "api", "image": "api:v1", "env": [{"name": "KEEP", "value": "yes"}]},
            {"name": "sidecar", "image": "sidecar:v1", "ports": [{"containerPort": 8080}]}
        ]
    });
    json!({
        "apiVersion": target.kind.api_version.as_ref(), "kind": target.kind.kind.as_ref(),
        "metadata": {"name": target.name, "namespace": target.namespace, "uid": "original-uid", "resourceVersion": "1042", "labels": {"keep": "label"}},
        "spec": if target.kind.group.is_empty() { pod_spec } else { json!({"replicas": 3, "template": {"metadata": {"labels": {"app": "worker"}}, "spec": pod_spec}}) }
    })
}

fn snapshot_for(target: &ResourceTarget) -> ImageSnapshot {
    snapshot(target, &serde_json::from_value(object(target)).unwrap()).unwrap()
}

async fn read_request(stream: &mut tokio::net::TcpStream) -> Request {
    let mut bytes = Vec::new();
    let mut buffer = [0; 4096];
    let (header_end, length) = loop {
        let count = stream.read(&mut buffer).await.unwrap();
        assert_ne!(count, 0);
        bytes.extend_from_slice(&buffer[..count]);
        assert!(bytes.len() < 65_536);
        if let Some(end) = bytes.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
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
    let headers = String::from_utf8_lossy(&bytes[..header_end]);
    let mut first = headers.lines().next().unwrap().split_whitespace();
    let method = first.next().unwrap().to_owned();
    let path = first.next().unwrap().split('?').next().unwrap().to_owned();
    let content_type = headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-type")
                .then(|| value.trim().to_owned())
        })
        .unwrap_or_default();
    let body = if length == 0 {
        Value::Null
    } else {
        serde_json::from_slice(&bytes[header_end + 4..header_end + 4 + length]).unwrap()
    };
    Request {
        method,
        path,
        content_type,
        body,
    }
}

async fn mock_api(responses: Vec<(u16, Value)>) -> (Client, tokio::task::JoinHandle<Vec<Request>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let mut config = kube::Config::new(format!("http://{address}").parse().unwrap());
    config.connect_timeout = Some(Duration::from_secs(5));
    config.read_timeout = Some(Duration::from_secs(5));
    let client = Client::try_from(config).unwrap();
    let server = tokio::spawn(async move {
        let mut requests = Vec::new();
        for (status, body) in responses {
            let (mut stream, _) = tokio::time::timeout(Duration::from_secs(5), listener.accept())
                .await
                .unwrap()
                .unwrap();
            requests.push(read_request(&mut stream).await);
            let body = body.to_string();
            let response = format!(
                "HTTP/1.1 {status} Response\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        }
        requests
    });
    (client, server)
}

#[tokio::test]
async fn loads_both_container_kinds_from_each_supported_exact_api_route() {
    for (kind, path) in [
        (
            ResourceKind::Pods,
            "/api/v1/namespaces/selected-team/pods/worker",
        ),
        (
            ResourceKind::Deployments,
            "/apis/apps/v1/namespaces/selected-team/deployments/worker",
        ),
        (
            ResourceKind::StatefulSets,
            "/apis/apps/v1/namespaces/selected-team/statefulsets/worker",
        ),
        (
            ResourceKind::DaemonSets,
            "/apis/apps/v1/namespaces/selected-team/daemonsets/worker",
        ),
    ] {
        let target = target(kind);
        let (client, server) = mock_api(vec![(200, object(&target))]).await;
        let loaded = load_images(client, &target).await.unwrap();
        assert_eq!(loaded, snapshot_for(&target));
        assert_eq!(
            loaded
                .images
                .iter()
                .map(|image| (image.init, image.name.as_str()))
                .collect::<Vec<_>>(),
            [(true, "setup"), (false, "api"), (false, "sidecar")]
        );
        let requests = server.await.unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, "GET");
        assert_eq!(requests[0].path, path);
    }
}

#[tokio::test]
async fn patches_only_changed_images_with_identity_preconditions_and_preserved_order() {
    for kind in [
        ResourceKind::Pods,
        ResourceKind::Deployments,
        ResourceKind::StatefulSets,
        ResourceKind::DaemonSets,
    ] {
        let target = target(kind);
        let loaded = snapshot_for(&target);
        let responses =
            vec![(200, object(&target)); if kind == ResourceKind::Pods { 2 } else { 1 }];
        let (client, server) = mock_api(responses).await;
        let edits = vec![
            ContainerImage {
                name: "api".to_owned(),
                image: " api:v2 ".to_owned(),
                init: false,
            },
            ContainerImage {
                name: "setup".to_owned(),
                image: "setup:v2".to_owned(),
                init: true,
            },
            ContainerImage {
                name: "sidecar".to_owned(),
                image: "sidecar:v1".to_owned(),
                init: false,
            },
        ];
        assert_eq!(update_images(client, &loaded, &edits).await.unwrap(), 2);
        let requests = server.await.unwrap();
        if kind == ResourceKind::Pods {
            assert_eq!(requests.len(), 2);
            assert_eq!(requests[0].method, "GET");
        } else {
            assert_eq!(requests.len(), 1);
        }
        let request = requests.last().unwrap();
        assert_eq!(request.method, "PATCH");
        assert!(
            request
                .path
                .ends_with(&format!("/{}/worker", target.kind.plural))
        );
        assert_eq!(
            request.content_type,
            "application/strategic-merge-patch+json"
        );
        assert_eq!(
            request.body["metadata"],
            json!({"uid": "original-uid", "resourceVersion": "1042"})
        );
        let path = if kind == ResourceKind::Pods {
            "/spec"
        } else {
            "/spec/template/spec"
        };
        assert_eq!(
            request.body.pointer(path).unwrap(),
            &json!({
                "initContainers": [{"name":"setup","image":"setup:v2"}],
                "$setElementOrder/initContainers": [{"name":"setup"}],
                "containers": [{"name":"api","image":"api:v2"}],
                "$setElementOrder/containers": [{"name":"api"},{"name":"sidecar"}]
            })
        );
    }
}

#[tokio::test]
async fn pods_with_any_owner_reference_are_rejected_on_load_and_update_without_patch() {
    let target = target(ResourceKind::Pods);
    for controller in [Some(true), Some(false), None] {
        let mut managed = object(&target);
        let mut owner = json!({"apiVersion": "apps/v1", "kind": "ReplicaSet", "name": "managed-worker", "uid": "owner-uid"});
        if let Some(controller) = controller {
            owner["controller"] = json!(controller);
        }
        managed["metadata"]["ownerReferences"] = json!([owner]);
        let (client, server) = mock_api(vec![(200, managed.clone())]).await;
        let error = load_images(client, &target).await.unwrap_err();
        assert!(error.contains("Pod selected-team/worker"));
        assert!(error.contains("ReplicaSet/managed-worker"));
        assert!(error.contains("set the image on the controller"));
        let requests = server.await.unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, "GET");

        // A public snapshot with matching UID/revision cannot bypass the ownership check.
        let untrusted = snapshot_for(&target);
        let mut edits = untrusted.images.clone();
        edits[1].image = "api:v2".to_owned();
        let (client, server) = mock_api(vec![(200, managed)]).await;
        let error = update_images(client, &untrusted, &edits).await.unwrap_err();
        assert!(error.contains("Pod selected-team/worker"));
        assert!(error.contains("ReplicaSet/managed-worker"));
        let requests = server.await.unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, "GET");
    }
}

#[tokio::test]
async fn pod_update_rejects_changed_identity_or_revision_before_patching() {
    let target = target(ResourceKind::Pods);
    let loaded = snapshot_for(&target);
    let mut edits = loaded.images.clone();
    edits[1].image = "api:v2".to_owned();
    for field in ["uid", "resourceVersion"] {
        let mut changed = object(&target);
        changed["metadata"][field] = json!("replacement");
        let (client, server) = mock_api(vec![(200, changed)]).await;
        let error = update_images(client, &loaded, &edits).await.unwrap_err();
        assert!(error.contains("Pod selected-team/worker"));
        assert!(error.contains("reload the image form"));
        let requests = server.await.unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, "GET");
    }
}

#[tokio::test]
async fn pod_patch_keeps_preconditions_after_ownership_check_and_does_not_retry_conflicts() {
    let target = target(ResourceKind::Pods);
    let loaded = snapshot_for(&target);
    let mut edits = loaded.images.clone();
    edits[1].image = "api:v2".to_owned();
    let (client, server) = mock_api(vec![
        (200, object(&target)),
        (409, failure(409, "resourceVersion conflict")),
    ])
    .await;
    let error = update_images(client, &loaded, &edits).await.unwrap_err();
    assert!(error.contains("reload the image form"));
    let requests = server.await.unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].method, "GET");
    assert_eq!(requests[1].method, "PATCH");
    assert_eq!(requests[0].path, requests[1].path);
    assert_eq!(
        requests[1].body["metadata"],
        json!({"uid": "original-uid", "resourceVersion": "1042"})
    );
}

#[tokio::test]
async fn workload_owners_do_not_prevent_editing_their_pod_template() {
    let target = target(ResourceKind::Deployments);
    let mut managed = object(&target);
    managed["metadata"]["ownerReferences"] = json!([{"apiVersion": "example.io/v1", "kind": "Application", "name": "worker-app", "uid": "app-uid", "controller": true}]);
    let (client, server) = mock_api(vec![(200, managed.clone()), (200, managed)]).await;
    let loaded = load_images(client.clone(), &target).await.unwrap();
    let mut edits = loaded.images.clone();
    edits[1].image = "api:v2".to_owned();
    assert_eq!(update_images(client, &loaded, &edits).await.unwrap(), 1);
    let requests = server.await.unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].method, "GET");
    assert_eq!(requests[1].method, "PATCH");
}

#[tokio::test]
async fn blank_and_unchanged_images_do_not_send_any_request() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let client = Client::try_from(kube::Config::new(
        format!("http://{}", listener.local_addr().unwrap())
            .parse()
            .unwrap(),
    ))
    .unwrap();
    let loaded = snapshot_for(&target(ResourceKind::Deployments));
    let mut edits = loaded.images.clone();
    edits[0].image = " \t ".to_owned();
    edits[1].image = " api:v1 ".to_owned();
    assert_eq!(update_images(client, &loaded, &edits).await.unwrap(), 0);
    assert!(
        tokio::time::timeout(Duration::from_millis(20), listener.accept())
            .await
            .is_err()
    );
}

fn failure(code: u16, message: &str) -> Value {
    json!({"apiVersion":"v1","kind":"Status","status":"Failure","reason":"Rejected","code":code,"message":message})
}

#[tokio::test]
async fn forbidden_load_and_conflicting_patches_report_identity_without_retries() {
    let target = target(ResourceKind::Deployments);
    let (client, server) = mock_api(vec![(403, failure(403, "get is forbidden"))]).await;
    let error = load_images(client, &target).await.unwrap_err();
    assert!(error.contains("Deployment selected-team/worker") && error.contains("forbidden"));
    assert_eq!(server.await.unwrap().len(), 1);
    for (code, reason) in [
        (409, "resourceVersion conflict"),
        (422, "metadata.uid is immutable"),
    ] {
        let loaded = snapshot_for(&target);
        let mut edits = loaded.images.clone();
        edits[1].image = "api:v2".to_owned();
        let (client, server) = mock_api(vec![(code, failure(code, reason))]).await;
        let error = update_images(client, &loaded, &edits).await.unwrap_err();
        assert!(error.contains("Deployment selected-team/worker") && error.contains(reason));
        assert!(error.contains("reload the image form"));
        assert_eq!(server.await.unwrap().len(), 1);
    }
}

#[test]
fn patch_validation_rejects_unknown_duplicate_or_unversioned_edits() {
    let loaded = snapshot_for(&target(ResourceKind::Pods));
    let mut invalid = loaded.clone();
    invalid.uid.clear();
    assert!(
        image_patch(&invalid, &loaded.images)
            .unwrap_err()
            .contains("UID")
    );
    invalid = loaded.clone();
    invalid.resource_version.clear();
    assert!(
        image_patch(&invalid, &loaded.images)
            .unwrap_err()
            .contains("resourceVersion")
    );
    let unknown = ContainerImage {
        name: "other".to_owned(),
        image: "other:v2".to_owned(),
        init: false,
    };
    assert!(
        image_patch(&loaded, &[unknown])
            .unwrap_err()
            .contains("not present")
    );
    assert!(
        image_patch(
            &loaded,
            &[loaded.images[0].clone(), loaded.images[0].clone()]
        )
        .unwrap_err()
        .contains("Duplicate")
    );
}

#[test]
fn container_order_is_preserved_even_when_edits_arrive_in_reverse_order() {
    let loaded = snapshot_for(&target(ResourceKind::Pods));
    let edits = [
        ContainerImage {
            name: "sidecar".to_owned(),
            image: "sidecar:v2".to_owned(),
            init: false,
        },
        ContainerImage {
            name: "api".to_owned(),
            image: "api:v2".to_owned(),
            init: false,
        },
    ];
    let (patch, count) = image_patch(&loaded, &edits).unwrap();
    assert_eq!(count, 2);
    assert_eq!(patch["spec"]["containers"][0]["name"], "api");
    assert_eq!(patch["spec"]["containers"][1]["name"], "sidecar");
    assert!(patch["spec"].get("initContainers").is_none());
}

#[test]
fn snapshot_requires_matching_identity_and_complete_concurrency_metadata() {
    let target = target(ResourceKind::Deployments);
    for field in ["uid", "resourceVersion", "namespace", "name"] {
        let mut object = object(&target);
        object["metadata"].as_object_mut().unwrap().remove(field);
        assert!(snapshot(&target, &serde_json::from_value(object).unwrap()).is_err());
    }
    let mut object = object(&target);
    object["spec"]["template"]["spec"]["containers"] = Value::Array(Vec::new());
    assert!(
        snapshot(&target, &serde_json::from_value(object).unwrap())
            .unwrap_err()
            .contains("regular containers")
    );
}

#[test]
fn only_exact_supported_gettable_namespaced_resources_are_accepted() {
    let mut selected = target(ResourceKind::Deployments);
    assert!(validate_target(&selected).is_ok());
    selected.namespace = None;
    assert!(validate_target(&selected).is_err());
    let mut unsupported = target(ResourceKind::Deployments).kind;
    unsupported.group = "example.io".into();
    assert!(!supports_images(&unsupported));
    unsupported = target(ResourceKind::Deployments).kind;
    unsupported.version = "v2".into();
    assert!(!supports_images(&unsupported));
    unsupported = target(ResourceKind::Deployments).kind;
    unsupported.plural = "others".into();
    assert!(!supports_images(&unsupported));
    let mut kind: ResourceDescriptor = ResourceKind::Pods.into();
    kind.operations = ResourceOperations::new(true, false, true);
    assert!(!supports_images(&kind));
}
