use serde_json::{Value, json};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

use super::*;

fn target() -> ResourceTarget {
    ResourceTarget {
        kind: ResourceKind::Deployments.into(),
        namespace: Some("team".to_owned()),
        name: "web".to_owned(),
    }
}

fn object(selector: &Value) -> Value {
    json!({
        "apiVersion": "apps/v1", "kind": "Deployment",
        "metadata": {"name": "web", "namespace": "team"},
        "spec": {"selector": selector}
    })
}

async fn fixture(code: u16, object: Value) -> (Client, tokio::task::JoinHandle<String>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let client = Client::try_from(kube::Config::new(
        format!("http://{}", listener.local_addr().unwrap())
            .parse()
            .unwrap(),
    ))
    .unwrap();
    let task = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut bytes = Vec::new();
        let mut chunk = [0; 4096];
        while !bytes.windows(4).any(|window| window == b"\r\n\r\n") {
            let count = stream.read(&mut chunk).await.unwrap();
            assert_ne!(count, 0);
            bytes.extend_from_slice(&chunk[..count]);
        }
        let body = object.to_string();
        stream.write_all(format!(
            "HTTP/1.1 {code} Response\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len(),
        ).as_bytes()).await.unwrap();
        String::from_utf8(bytes).unwrap()
    });
    (client, task)
}

#[tokio::test]
async fn complete_selector_preserves_labels_and_every_expression_operator() {
    let (client, request) = fixture(
        200,
        object(&json!({
            "matchLabels": {"app": "web", "tier": "worker"},
            "matchExpressions": [
                {"key": "track", "operator": "In", "values": ["stable", "canary"]},
                {"key": "env", "operator": "NotIn", "values": ["dev"]},
                {"key": "release", "operator": "Exists"},
                {"key": "debug", "operator": "DoesNotExist"}
            ]
        })),
    )
    .await;
    let selector = load_selector(client, &target()).await.unwrap();
    assert_eq!(
        selector,
        "app=web,tier=worker,track in (canary,stable),env notin (dev),release,!debug"
    );
    let request = request.await.unwrap();
    assert_eq!(
        request.lines().next(),
        Some("GET /apis/apps/v1/namespaces/team/deployments/web HTTP/1.1")
    );
}

#[tokio::test]
async fn absent_empty_and_invalid_selectors_never_produce_an_unfiltered_view() {
    for selector in [
        Value::Null,
        json!({}),
        json!({"matchLabels": {}}),
        json!({"matchExpressions": [{"key":"", "operator":"Exists"}]}),
        json!({"matchExpressions": [{"key":"app", "operator":"Unsupported"}]}),
    ] {
        let (client, request) = fixture(200, object(&selector)).await;
        assert!(
            load_selector(client, &target())
                .await
                .unwrap_err()
                .contains("selector")
        );
        request.await.unwrap();
    }
    let mut deployment = object(&json!({"matchLabels": {"app":"web"}}));
    deployment["spec"]
        .as_object_mut()
        .unwrap()
        .remove("selector");
    let (client, request) = fixture(200, deployment).await;
    assert!(
        load_selector(client, &target())
            .await
            .unwrap_err()
            .contains("no selector")
    );
    request.await.unwrap();
}

#[tokio::test]
async fn mismatched_returned_deployment_identity_is_rejected() {
    for (path, value) in [
        ("/metadata/name", "other"),
        ("/metadata/namespace", "other"),
        ("/kind", "StatefulSet"),
        ("/apiVersion", "apps/v1beta2"),
    ] {
        let mut deployment = object(&json!({"matchLabels": {"app":"web"}}));
        *deployment.pointer_mut(path).unwrap() = json!(value);
        let (client, request) = fixture(200, deployment).await;
        assert!(
            load_selector(client, &target())
                .await
                .unwrap_err()
                .contains("selected identity")
        );
        request.await.unwrap();
    }
}

#[tokio::test]
async fn invalid_target_fails_before_any_api_request() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let client = Client::try_from(kube::Config::new(
        format!("http://{}", listener.local_addr().unwrap())
            .parse()
            .unwrap(),
    ))
    .unwrap();
    for case in 0..5 {
        let mut target = target();
        match case {
            0 => target.kind = ResourceKind::Pods.into(),
            1 => target.namespace = None,
            2 => target.namespace = Some(" ".to_owned()),
            3 => target.name.clear(),
            _ => {
                target.kind.operations = crate::model::ResourceOperations::new(false, false, false);
            }
        }
        assert!(load_selector(client.clone(), &target).await.is_err());
    }
    assert!(
        tokio::time::timeout(Duration::from_millis(20), listener.accept())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn forbidden_deployment_read_returns_the_api_failure() {
    let (client, request) = fixture(
        403,
        json!({
            "apiVersion":"v1", "kind":"Status", "status":"Failure",
            "reason":"Forbidden", "message":"deployment read denied", "code":403,
        }),
    )
    .await;
    assert!(
        load_selector(client, &target())
            .await
            .unwrap_err()
            .contains("deployment read denied")
    );
    request.await.unwrap();
}
