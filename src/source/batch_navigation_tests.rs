use serde_json::{Value, json};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

use super::*;
use crate::model::{ResourceDescriptor, ResourceKind, ResourceOperations};

fn target(cron_job: bool) -> ResourceTarget {
    let (kind, plural) = if cron_job {
        ("CronJob", "cronjobs")
    } else {
        ("Job", "jobs")
    };
    ResourceTarget {
        kind: ResourceDescriptor::new(
            "batch",
            "v1",
            "batch/v1",
            kind,
            plural,
            true,
            ResourceOperations::all(),
        ),
        namespace: Some("team".to_owned()),
        name: "backup".to_owned(),
    }
}

fn object(cron_job: bool, selector: &Value) -> Value {
    json!({
        "apiVersion": "batch/v1", "kind": if cron_job { "CronJob" } else { "Job" },
        "metadata": {"name": "backup", "namespace": "team", "uid": "new-parent-uid"},
        "spec": {"selector": selector}
    })
}

async fn read_request(stream: &mut tokio::net::TcpStream) -> String {
    let mut bytes = Vec::new();
    let mut chunk = [0; 4096];
    while !bytes.windows(4).any(|window| window == b"\r\n\r\n") {
        let count = stream.read(&mut chunk).await.unwrap();
        assert_ne!(count, 0);
        bytes.extend_from_slice(&chunk[..count]);
    }
    String::from_utf8(bytes).unwrap()
}

async fn listener() -> (Client, tokio::net::TcpListener) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let client = Client::try_from(kube::Config::new(
        format!("http://{}", listener.local_addr().unwrap())
            .parse()
            .unwrap(),
    ))
    .unwrap();
    (client, listener)
}

async fn fixture(code: u16, object: Value) -> (Client, tokio::task::JoinHandle<String>) {
    let (client, listener) = listener().await;
    let task = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let request = read_request(&mut stream).await;
        let body = object.to_string();
        stream.write_all(format!(
            "HTTP/1.1 {code} Response\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len(),
        ).as_bytes()).await.unwrap();
        request
    });
    (client, task)
}

#[tokio::test]
async fn cron_job_navigation_gets_parent_then_filters_by_name_without_uid() {
    let mut cron_job = object(true, &Value::Null);
    cron_job["metadata"].as_object_mut().unwrap().remove("uid");
    cron_job.as_object_mut().unwrap().remove("spec");
    let (client, request) = fixture(200, cron_job).await;
    assert_eq!(
        load_navigation(client, &target(true), None).await.unwrap(),
        BatchNavigationFilter::CronJobJobs {
            owner_name: "backup".to_owned(),
        }
    );
    assert_eq!(
        request.await.unwrap().lines().next(),
        Some("GET /apis/batch/v1/namespaces/team/cronjobs/backup HTTP/1.1")
    );
}

#[tokio::test]
async fn job_navigation_preserves_full_selector_without_owner_filter() {
    let (client, request) = fixture(
        200,
        object(
            false,
            &json!({
                "matchLabels": {"app": "backup", "batch.example.org/run": "scheduled"},
                "matchExpressions": [
                    {"key": "track", "operator": "In", "values": ["stable", "canary", "stable"]},
                    {"key": "env", "operator": "NotIn", "values": ["dev"]},
                    {"key": "release", "operator": "Exists"},
                    {"key": "debug", "operator": "DoesNotExist"}
                ]
            }),
        ),
    )
    .await;
    assert_eq!(
        load_navigation(client, &target(false), Some(Duration::from_secs(1)))
            .await
            .unwrap(),
        BatchNavigationFilter::JobPods {
            label_selector: Some("app=backup,batch.example.org/run=scheduled,track in (canary,stable),env notin (dev),release,!debug".to_owned()),
        }
    );
    assert_eq!(
        request.await.unwrap().lines().next(),
        Some("GET /apis/batch/v1/namespaces/team/jobs/backup HTTP/1.1")
    );
}

#[tokio::test]
async fn missing_or_null_job_selector_matches_nothing() {
    for missing in [false, true] {
        let mut job = object(false, &Value::Null);
        if missing {
            job["spec"].as_object_mut().unwrap().remove("selector");
        }
        let (client, request) = fixture(200, job).await;
        assert_eq!(
            load_navigation(client, &target(false), None).await.unwrap(),
            BatchNavigationFilter::JobPods {
                label_selector: None
            }
        );
        request.await.unwrap();
    }
}

#[tokio::test]
async fn explicitly_empty_job_selector_matches_everything() {
    for selector in [
        json!({}),
        json!({"matchLabels": {}, "matchExpressions": []}),
        json!({"matchLabels": null, "matchExpressions": null}),
    ] {
        let (client, request) = fixture(200, object(false, &selector)).await;
        assert_eq!(
            load_navigation(client, &target(false), None).await.unwrap(),
            BatchNavigationFilter::JobPods {
                label_selector: Some(String::new())
            }
        );
        request.await.unwrap();
    }
}

#[tokio::test]
async fn invalid_job_selector_shape_keys_values_and_operators_fail_before_navigation() {
    for selector in [
        json!("app=backup"),
        json!({"matchLabels": {"bad/key/again":"backup"}}),
        json!({"matchLabels": {"app":"bad value"}}),
        json!({"matchExpressions": [{"key":"", "operator":"Exists"}]}),
        json!({"matchExpressions": [{"key":"app", "operator":"Gt", "values":["3"]}]}),
        json!({"matchExpressions": [{"key":"app", "operator":"In", "values":[]}]}),
        json!({"matchExpressions": [{"key":"app", "operator":"NotIn"}]}),
        json!({"matchExpressions": [{"key":"app", "operator":"Exists", "values":["backup"]}]}),
        json!({"matchExpressions": [{"key":"app", "operator":"DoesNotExist", "values":["backup"]}]}),
    ] {
        let (client, request) = fixture(200, object(false, &selector)).await;
        assert!(
            load_navigation(client, &target(false), None)
                .await
                .unwrap_err()
                .contains("Invalid Job selector"),
            "invalid selector {selector}"
        );
        request.await.unwrap();
    }
}

#[tokio::test]
async fn returned_batch_object_must_match_selected_identity() {
    for cron_job in [false, true] {
        for (path, value) in [
            ("/metadata/name", "other"),
            ("/metadata/namespace", "other"),
            ("/kind", "Deployment"),
            ("/apiVersion", "batch/v1beta1"),
        ] {
            let mut job = object(cron_job, &json!({}));
            *job.pointer_mut(path).unwrap() = json!(value);
            let (client, request) = fixture(200, job).await;
            assert!(
                load_navigation(client, &target(cron_job), None)
                    .await
                    .unwrap_err()
                    .contains("selected identity")
            );
            request.await.unwrap();
        }
    }
}

#[tokio::test]
async fn invalid_target_and_timeouts_do_not_make_an_api_request() {
    let (client, listener) = listener().await;
    for cron_job in [false, true] {
        for case in 0..9 {
            let mut target = target(cron_job);
            match case {
                0 => target.kind = ResourceKind::Pods.into(),
                1 => target.namespace = None,
                2 => target.namespace = Some(" ".to_owned()),
                3 => target.name.clear(),
                4 => target.kind.api_version = "batch/v2".into(),
                5 => target.kind.plural = "cronjobs".into(),
                6 => target.kind.version = "v1beta1".into(),
                7 => target.kind.namespaced = false,
                _ => target.kind.operations = ResourceOperations::new(false, false, false),
            }
            if case == 5 && cron_job {
                target.kind.plural = "jobs".into();
            }
            assert!(
                load_navigation(client.clone(), &target, None)
                    .await
                    .is_err()
            );
        }
        for timeout in [Duration::ZERO, Duration::MAX] {
            assert!(
                load_navigation(client.clone(), &target(cron_job), Some(timeout))
                    .await
                    .unwrap_err()
                    .contains("timeout")
            );
        }
    }
    assert!(
        tokio::time::timeout(Duration::from_millis(20), listener.accept())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn forbidden_and_missing_parents_report_get_failure() {
    for cron_job in [false, true] {
        for (code, reason) in [(403, "Forbidden"), (404, "NotFound")] {
            let (client, request) = fixture(
                code,
                json!({
                    "apiVersion":"v1", "kind":"Status", "status":"Failure",
                    "reason":reason, "message":"parent read failed", "code":code,
                }),
            )
            .await;
            assert!(
                load_navigation(client, &target(cron_job), None)
                    .await
                    .unwrap_err()
                    .contains("parent read failed")
            );
            request.await.unwrap();
        }
    }
}

#[tokio::test]
async fn configured_timeout_bounds_a_stalled_parent_read() {
    let (client, listener) = listener().await;
    let (sender, receiver) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let request = read_request(&mut stream).await;
        sender.send(request).unwrap();
        std::future::pending::<()>().await;
    });
    assert!(
        load_navigation(client, &target(false), Some(Duration::from_millis(50)))
            .await
            .unwrap_err()
            .contains("timed out")
    );
    assert!(receiver.await.unwrap().contains("/jobs/backup"));
    server.abort();
    assert!(server.await.unwrap_err().is_cancelled());
}

#[tokio::test]
async fn dropping_navigation_future_cancels_an_unbounded_parent_read() {
    let (client, listener) = listener().await;
    let (sender, receiver) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        sender.send(read_request(&mut stream).await).unwrap();
        let mut byte = [0];
        stream.read(&mut byte).await
    });
    let task = tokio::spawn(async move { load_navigation(client, &target(true), None).await });
    assert!(receiver.await.unwrap().contains("/cronjobs/backup"));
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), server)
            .await
            .unwrap()
            .unwrap()
            .unwrap(),
        0
    );
}

#[test]
fn qualified_label_keys_and_values_match_kubernetes_validation_boundaries() {
    for key in [
        "app",
        "App_1",
        "batch.example.org/job-name",
        "domain/key.with-dots",
    ] {
        assert!(validate_key(key).is_ok(), "valid key {key}");
    }
    assert!(validate_key(&format!("example.org/{}", "a".repeat(63))).is_ok());
    assert!(validate_key(&format!("example.org/{}", "a".repeat(64))).is_err());
    for key in [
        "",
        "/name",
        "EXAMPLE.org/name",
        "domain/",
        "a/b/c",
        "-bad",
        "bad_",
        "a..b/name",
        "例子",
    ] {
        assert!(validate_key(key).is_err(), "invalid key {key}");
    }
    for value in ["", "A_1", "a.b-c"] {
        assert!(validate_value(value).is_ok(), "valid value {value}");
    }
    for value in ["bad/value", "bad value", "-bad", "bad.", "值"] {
        assert!(validate_value(value).is_err(), "invalid value {value}");
    }
    assert!(validate_value(&"a".repeat(63)).is_ok());
    assert!(validate_value(&"a".repeat(64)).is_err());
}
