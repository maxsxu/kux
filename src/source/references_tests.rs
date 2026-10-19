use super::*;
use serde_json::json;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

fn source(kind: ReferenceKind, name: &str) -> ResourceTarget {
    let (group, api_version, kind_name, plural, namespace) = match kind {
        ReferenceKind::ConfigMap => ("", "v1", "ConfigMap", "configmaps", Some("team")),
        ReferenceKind::Secret => ("", "v1", "Secret", "secrets", Some("team")),
        ReferenceKind::Claim => (
            "",
            "v1",
            "PersistentVolumeClaim",
            "persistentvolumeclaims",
            Some("team"),
        ),
        ReferenceKind::ServiceAccount => {
            ("", "v1", "ServiceAccount", "serviceaccounts", Some("team"))
        }
        ReferenceKind::PriorityClass => (
            "scheduling.k8s.io",
            "scheduling.k8s.io/v1",
            "PriorityClass",
            "priorityclasses",
            None,
        ),
    };
    ResourceTarget {
        kind: ResourceDescriptor::new(
            group,
            "v1",
            api_version,
            kind_name,
            plural,
            namespace.is_some(),
            ResourceOperations::all(),
        ),
        namespace: namespace.map(str::to_owned),
        name: name.to_owned(),
    }
}

fn object(descriptor: &ResourceDescriptor, namespace: &str, name: &str, spec: Value) -> Value {
    let mut result = json!({"apiVersion":descriptor.api_version.as_ref(),"kind":descriptor.kind.as_ref(),"metadata":{"name":name,"namespace":namespace},"spec":{}});
    if descriptor.kind.as_ref() == "CronJob" {
        result["spec"]["jobTemplate"]["spec"]["template"]["spec"] = spec;
    } else {
        result["spec"]["template"]["spec"] = spec;
    }
    result
}

fn list(items: Vec<Value>, continuation: &str) -> Value {
    let mut result = json!({"apiVersion":"v1","kind":"List","metadata":{"continue":continuation}});
    result["items"] = Value::Array(items);
    result
}

fn failure(code: u16) -> Value {
    json!({"apiVersion":"v1","kind":"Status","status":"Failure","code":code,"reason":"Denied","message":"fixture permission denied"})
}

async fn read_path(socket: &mut tokio::net::TcpStream) -> String {
    let mut bytes = vec![];
    let mut buffer = [0; 2048];
    while !bytes.windows(4).any(|w| w == b"\r\n\r\n") {
        let count = socket.read(&mut buffer).await.unwrap();
        assert_ne!(count, 0);
        bytes.extend_from_slice(&buffer[..count]);
    }
    let text = String::from_utf8(bytes).unwrap();
    let mut words = text.lines().next().unwrap().split_whitespace();
    assert_eq!(words.next(), Some("GET"));
    words.next().unwrap().to_owned()
}

async fn respond(socket: &mut tokio::net::TcpStream, code: u16, body: Value) {
    let body = body.to_string();
    let response = format!(
        "HTTP/1.1 {code} Response\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    socket.write_all(response.as_bytes()).await.unwrap();
}

async fn mock_api(
    count: usize,
    mut handler: impl FnMut(&str) -> (u16, Value) + Send + 'static,
) -> (Client, tokio::task::JoinHandle<Vec<String>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let client = Client::try_from(kube::Config::new(
        format!("http://{}", listener.local_addr().unwrap())
            .parse()
            .unwrap(),
    ))
    .unwrap();
    let server = tokio::spawn(async move {
        let mut paths = vec![];
        for _ in 0..count {
            let (mut socket, _) = tokio::time::timeout(Duration::from_secs(10), listener.accept())
                .await
                .unwrap()
                .unwrap();
            let path = read_path(&mut socket).await;
            let (code, body) = handler(&path);
            respond(&mut socket, code, body).await;
            paths.push(path);
        }
        paths
    });
    (client, server)
}

fn kind_for_path(path: &str) -> ResourceDescriptor {
    let path = path.split('?').next().unwrap();
    workloads()
        .into_iter()
        .find(|kind| path.ends_with(&format!("/{}", kind.plural)))
        .unwrap()
}

#[test]
fn exact_supported_source_identities_only() {
    for kind in [
        ReferenceKind::ConfigMap,
        ReferenceKind::Secret,
        ReferenceKind::Claim,
        ReferenceKind::ServiceAccount,
        ReferenceKind::PriorityClass,
    ] {
        let target = source(kind, "selected");
        assert!(supports_references(&target.kind));
        assert!(validate(&target, None).is_ok());
        let mut forged = target.clone();
        forged.kind.group = "example.com".into();
        assert!(!supports_references(&forged.kind));
        forged = target.clone();
        forged.kind.version = "v2".into();
        assert!(!supports_references(&forged.kind));
        forged = target.clone();
        forged.kind.api_version = "example.com/v1".into();
        assert!(!supports_references(&forged.kind));
        forged = target.clone();
        forged.kind.plural = "other".into();
        assert!(!supports_references(&forged.kind));
        forged = target;
        forged.kind.namespaced = !forged.kind.namespaced;
        assert!(!supports_references(&forged.kind));
    }
    for kind in workloads() {
        assert!(!supports_references(&kind));
    }
}

#[test]
fn configmap_and_secret_container_and_volume_paths() {
    for kind in [ReferenceKind::ConfigMap, ReferenceKind::Secret] {
        let (from, key) = if kind == ReferenceKind::ConfigMap {
            ("configMapRef", "configMapKeyRef")
        } else {
            ("secretRef", "secretKeyRef")
        };
        for container_field in ["containers", "initContainers", "ephemeralContainers"] {
            for env in [
                json!({"envFrom":[{from:{"name":"selected"}}]}),
                json!({"env":[{"valueFrom":{key:{"name":"selected","key":"private-value-must-not-be-read"}}}]}),
            ] {
                let spec = json!({container_field:[env]});
                assert!(matches_spec(&spec, kind, "selected"));
                assert!(!matches_spec(&spec, kind, "unrelated"));
            }
        }
        let volume = if kind == ReferenceKind::ConfigMap {
            json!({"configMap":{"name":"selected"}})
        } else {
            json!({"secret":{"secretName":"selected"}})
        };
        assert!(matches_spec(&json!({"volumes":[volume]}), kind, "selected"));
        let projection = if kind == ReferenceKind::ConfigMap {
            "configMap"
        } else {
            "secret"
        };
        assert!(matches_spec(
            &json!({"volumes":[{"projected":{"sources":[{projection:{"name":"selected"}}]}}]}),
            kind,
            "selected"
        ));
        assert!(!matches_spec(&json!({}), kind, "selected"));
    }
    assert!(matches_spec(
        &json!({"imagePullSecrets":[{"name":"selected"}]}),
        ReferenceKind::Secret,
        "selected"
    ));
    assert!(!matches_spec(
        &json!({"containers":[{"env":[{"value":"selected"}]}]}),
        ReferenceKind::Secret,
        "selected"
    ));
}

#[test]
fn serviceaccount_default_priority_and_claim_matches() {
    for spec in [
        json!({}),
        json!({"serviceAccountName":""}),
        json!({"serviceAccountName":"default"}),
    ] {
        assert!(matches_spec(
            &spec,
            ReferenceKind::ServiceAccount,
            "default"
        ));
        assert!(!matches_spec(&spec, ReferenceKind::ServiceAccount, "other"));
    }
    assert!(matches_spec(
        &json!({"serviceAccountName":"worker"}),
        ReferenceKind::ServiceAccount,
        "worker"
    ));
    assert!(matches_spec(
        &json!({"priorityClassName":"urgent"}),
        ReferenceKind::PriorityClass,
        "urgent"
    ));
    assert!(!matches_spec(
        &json!({}),
        ReferenceKind::PriorityClass,
        "urgent"
    ));
    assert!(matches_spec(
        &json!({"volumes":[{"persistentVolumeClaim":{"claimName":"data"}}]}),
        ReferenceKind::Claim,
        "data"
    ));
}

#[test]
fn generated_claim_requires_full_statefulset_identity_and_ordinal() {
    let object: DynamicObject = serde_json::from_value(
        json!({"spec":{"volumeClaimTemplates":[{"metadata":{"name":"data"}}]}}),
    )
    .unwrap();
    for claim in ["data-web-0", "data-web-12"] {
        assert!(generated_claim(&object, "web", claim));
    }
    for claim in [
        "data-web",
        "data-web-",
        "data-web-old-0",
        "data-web-0suffix",
        "data-other-0",
        "other-web-0",
    ] {
        assert!(!generated_claim(&object, "web", claim));
    }
}

#[tokio::test]
async fn five_workload_kinds_use_exact_namespace_and_include_job_cronjob_pvcs() {
    let (client, server) = mock_api(5, |path| {
        assert!(path.contains("/namespaces/team/"));
        assert!(path.contains("limit=500"));
        let descriptor = kind_for_path(path);
        (
            200,
            list(
                vec![object(
                    &descriptor,
                    "team",
                    "same-name",
                    json!({"volumes":[{"persistentVolumeClaim":{"claimName":"data"}}]}),
                )],
                "",
            ),
        )
    })
    .await;
    let report = scan_references(client, &source(ReferenceKind::Claim, "data"), None)
        .await
        .unwrap();
    assert_eq!(report.entries.len(), 5);
    assert_eq!(report.scanned, 5);
    assert_eq!(report.errors, [] as [String; 0]);
    let paths = server.await.unwrap();
    let mut expected: Vec<_> = workloads()
        .iter()
        .map(|kind| format!("/apis/{}/v1/namespaces/team/{}", kind.group, kind.plural))
        .collect();
    expected.sort();
    let mut actual: Vec<_> = paths
        .iter()
        .map(|path| path.split('?').next().unwrap().to_owned())
        .collect();
    actual.sort();
    assert_eq!(actual, expected);
    assert!(
        report
            .entries
            .iter()
            .all(|entry| entry.target.namespace.as_deref() == Some("team"))
    );
}

#[tokio::test]
async fn priorityclass_scans_all_namespaces_and_preserves_cross_kind_identities() {
    let (client, server) = mock_api(5, |path| {
        assert!(!path.contains("/namespaces/"));
        let kind = kind_for_path(path);
        (
            200,
            list(
                vec![
                    object(
                        &kind,
                        "alpha",
                        "shared",
                        json!({"priorityClassName":"critical"}),
                    ),
                    object(
                        &kind,
                        "beta",
                        "shared",
                        json!({"priorityClassName":"critical"}),
                    ),
                    object(
                        &kind,
                        "gamma",
                        "other",
                        json!({"priorityClassName":"normal"}),
                    ),
                ],
                "",
            ),
        )
    })
    .await;
    let report = scan_references(
        client,
        &source(ReferenceKind::PriorityClass, "critical"),
        None,
    )
    .await
    .unwrap();
    assert_eq!(report.entries.len(), 10);
    assert_eq!(report.scanned, 15);
    assert_eq!(report.errors, [] as [String; 0]);
    assert_eq!(
        report
            .entries
            .iter()
            .filter(|entry| entry.target.namespace.as_deref() == Some("alpha"))
            .count(),
        5
    );
    assert_eq!(
        report
            .entries
            .iter()
            .filter(|entry| entry.target.namespace.as_deref() == Some("beta"))
            .count(),
        5
    );
    server.await.unwrap();
}

#[tokio::test]
async fn explicit_serviceaccount_secret_lookup_is_cached_across_workload_kinds() {
    for field in ["secrets", "imagePullSecrets"] {
        let (client, server) = mock_api(6, move |path| {
            if path == "/api/v1/namespaces/team/serviceaccounts/worker" {
                return (200, json!({"apiVersion":"v1","kind":"ServiceAccount","metadata":{"name":"worker","namespace":"team"},field:[{"name":"selected"}]}));
            }
            let kind = kind_for_path(path);
            (200, list(vec![object(&kind, "team", "workload", json!({"serviceAccountName":"worker"}))], ""))
        }).await;
        let report = scan_references(client, &source(ReferenceKind::Secret, "selected"), None)
            .await
            .unwrap();
        assert_eq!(report.entries.len(), 5, "{field}");
        assert_eq!(report.errors, [] as [String; 0]);
        assert_eq!(
            server
                .await
                .unwrap()
                .iter()
                .filter(|path| path.contains("serviceaccounts"))
                .count(),
            1
        );
    }
}

#[tokio::test]
async fn secret_lookup_failure_is_partial_and_keeps_direct_volume_matches() {
    let (client, server) = mock_api(6, |path| {
        if path.contains("/serviceaccounts/") {
            return (403, failure(403));
        }
        let kind = kind_for_path(path);
        let spec = if kind.kind.as_ref() == "Deployment" {
            json!({"serviceAccountName":"denied","volumes":[{"secret":{"secretName":"selected"}}]})
        } else {
            json!({"serviceAccountName":"denied"})
        };
        (200, list(vec![object(&kind, "team", "workload", spec)], ""))
    })
    .await;
    let report = scan_references(client, &source(ReferenceKind::Secret, "selected"), None)
        .await
        .unwrap();
    assert_eq!(report.entries.len(), 1);
    assert_eq!(report.entries[0].target.kind.kind.as_ref(), "Deployment");
    assert_eq!(report.errors.len(), 1);
    assert!(report.errors[0].contains("ServiceAccount team/denied"));
    assert_eq!(report.scanned, 5);
    server.await.unwrap();
}

#[tokio::test]
async fn list_denial_and_unserved_api_are_explicit_partial_results() {
    let (client, server) = mock_api(5, |path| {
        let kind = kind_for_path(path);
        if kind.kind.as_ref() == "DaemonSet" {
            return (403, failure(403));
        }
        if kind.kind.as_ref() == "Job" {
            return (404, failure(404));
        }
        (
            200,
            list(vec![object(&kind, "team", "workload", json!({}))], ""),
        )
    })
    .await;
    let report = scan_references(
        client,
        &source(ReferenceKind::ServiceAccount, "default"),
        None,
    )
    .await
    .unwrap();
    assert_eq!(report.entries.len(), 3);
    assert_eq!(report.scanned, 3);
    assert_eq!(report.errors.len(), 2);
    server.await.unwrap();
    let (client, server) = mock_api(5, |_| (403, failure(403))).await;
    let report = scan_references(client, &source(ReferenceKind::ConfigMap, "selected"), None)
        .await
        .unwrap();
    assert_eq!(report.entries, [] as [ReferenceEntry; 0]);
    assert_eq!(report.errors.len(), 5);
    server.await.unwrap();
}

#[tokio::test]
async fn pagination_uses_server_token_and_deduplicates_full_identity() {
    let (client, server) = mock_api(6, |path| {
        let kind = kind_for_path(path);
        if kind.kind.as_ref() != "Deployment" {
            return (200, list(vec![], ""));
        }
        let next = if path.contains("continue=next") {
            ""
        } else {
            "next"
        };
        let mut items = vec![object(&kind, "team", "same", json!({}))];
        if next.is_empty() {
            items.push(object(&kind, "team", "second", json!({})));
        }
        (200, list(items, next))
    })
    .await;
    let report = scan_references(
        client,
        &source(ReferenceKind::ServiceAccount, "default"),
        None,
    )
    .await
    .unwrap();
    assert_eq!(report.entries.len(), 2);
    assert_eq!(report.scanned, 3);
    assert_eq!(report.errors, [] as [String; 0]);
    assert_eq!(
        server
            .await
            .unwrap()
            .iter()
            .filter(|path| path.contains("continue=next"))
            .count(),
        1
    );
}

#[tokio::test]
async fn repeated_tokens_and_page_cap_stop_with_partial_diagnostic() {
    let (client, server) = mock_api(6, |path| {
        let kind = kind_for_path(path);
        (
            200,
            list(
                vec![],
                if kind.kind.as_ref() == "Deployment" {
                    "repeat"
                } else {
                    ""
                },
            ),
        )
    })
    .await;
    let report = scan_references(client, &source(ReferenceKind::ConfigMap, "selected"), None)
        .await
        .unwrap();
    assert!(report.errors[0].contains("repeated continuation"));
    server.await.unwrap();
    let (client, server) = mock_api(5, |_| (200, list(vec![], "more"))).await;
    let report = scan_with_page_limit(
        client,
        &source(ReferenceKind::ConfigMap, "selected"),
        None,
        1,
    )
    .await
    .unwrap();
    assert_eq!(report.errors.len(), 5);
    assert!(
        report
            .errors
            .iter()
            .all(|error| error.contains("500 object limit"))
    );
    server.await.unwrap();
}

#[tokio::test]
async fn returned_wrong_namespace_kind_or_missing_template_is_partial() {
    let (client, server) = mock_api(5, |path| {
        let kind = kind_for_path(path);
        let mut wrong_kind = object(&kind, "team", "wrong-kind", json!({}));
        wrong_kind["kind"] = json!("Unrelated");
        (
            200,
            list(
                vec![
                    object(&kind, "elsewhere", "wrong-scope", json!({})),
                    wrong_kind,
                    json!({"metadata":{"name":"no-template","namespace":"team"}}),
                    json!({"metadata":{"name":"no-namespace"}}),
                ],
                "",
            ),
        )
    })
    .await;
    let report = scan_references(
        client,
        &source(ReferenceKind::ServiceAccount, "default"),
        None,
    )
    .await
    .unwrap();
    assert_eq!(report.entries, [] as [ReferenceEntry; 0]);
    assert_eq!(report.scanned, 20);
    assert_eq!(report.errors.len(), 15);
    server.await.unwrap();
}

#[tokio::test]
async fn invalid_target_and_timeout_make_no_requests() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let client = Client::try_from(kube::Config::new(
        format!("http://{}", listener.local_addr().unwrap())
            .parse()
            .unwrap(),
    ))
    .unwrap();
    let valid = source(ReferenceKind::ConfigMap, "selected");
    for timeout in [Duration::ZERO, Duration::MAX] {
        assert!(
            scan_references(client.clone(), &valid, Some(timeout))
                .await
                .is_err()
        );
    }
    let mut targets = vec![];
    let mut invalid = valid.clone();
    invalid.namespace = None;
    targets.push(invalid);
    let mut invalid = valid.clone();
    invalid.name = String::new();
    targets.push(invalid);
    let mut invalid = valid;
    invalid.kind.group = "spoofed".into();
    targets.push(invalid);
    let mut invalid = source(ReferenceKind::PriorityClass, "selected");
    invalid.namespace = Some("team".to_owned());
    targets.push(invalid);
    for target in targets {
        assert!(
            scan_references(client.clone(), &target, None)
                .await
                .is_err()
        );
    }
    assert!(
        tokio::time::timeout(Duration::from_millis(30), listener.accept())
            .await
            .is_err()
    );
}

#[test]
fn diagnostic_count_is_bounded_without_truncating_before_credential_redaction() {
    let mut errors = vec![];
    for index in 0..100 {
        add_error(&mut errors, format!("{index}: {}", "🦀".repeat(2000)));
    }
    assert_eq!(errors.len(), MAX_ERRORS + 1);
    assert!(errors[0].chars().count() > 2000);
    let error = errors[0].clone();
    add_error(&mut errors, error);
    assert_eq!(errors.len(), MAX_ERRORS + 1);
}

#[tokio::test]
async fn secret_serviceaccount_wrong_kind_is_partial_and_foreign_secret_is_not_a_match() {
    for forged in [false, true] {
        let (client, server) = mock_api(6, move |path| {
            if path.contains("/serviceaccounts/") {
                return (200, json!({"apiVersion":"v1","kind":if forged {"Secret"} else {"ServiceAccount"},"metadata":{"name":"worker","namespace":"team"},"secrets":[{"name":"selected","namespace":"foreign"}]}));
            }
            let kind = kind_for_path(path);
            (200, list(vec![object(&kind, "team", "workload", json!({"serviceAccountName":"worker"}))], ""))
        }).await;
        let report = scan_references(client, &source(ReferenceKind::Secret, "selected"), None)
            .await
            .unwrap();
        assert_eq!(report.entries, [] as [ReferenceEntry; 0]);
        assert_eq!(report.errors.is_empty(), !forged);
        server.await.unwrap();
    }
}

#[tokio::test]
async fn oversized_page_is_truncated_with_explicit_partial_error() {
    let (client, server) = mock_api(5, |path| {
        let kind = kind_for_path(path);
        let items = if kind.kind.as_ref() == "Deployment" {
            (0..=PAGE_SIZE)
                .map(|index| object(&kind, "team", &format!("workload-{index}"), json!({})))
                .collect()
        } else {
            vec![]
        };
        (200, list(items, ""))
    })
    .await;
    let report = scan_references(
        client,
        &source(ReferenceKind::ServiceAccount, "default"),
        None,
    )
    .await
    .unwrap();
    assert_eq!(report.scanned, PAGE_SIZE as usize);
    assert_eq!(report.entries.len(), PAGE_SIZE as usize);
    assert_eq!(report.errors.len(), 1);
    assert!(report.errors[0].contains("truncated"));
    server.await.unwrap();
}

#[tokio::test]
async fn list_response_timeout_is_reported_for_every_unfinished_kind() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let client = Client::try_from(kube::Config::new(
        format!("http://{}", listener.local_addr().unwrap())
            .parse()
            .unwrap(),
    ))
    .unwrap();
    let server = tokio::spawn(async move {
        let mut sockets = vec![];
        for _ in 0..5 {
            let (mut socket, _) = listener.accept().await.unwrap();
            read_path(&mut socket).await;
            socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 10000\r\n\r\n{").await.unwrap();
            sockets.push(socket);
        }
        std::future::pending::<()>().await;
        drop(sockets);
    });
    let report = scan_references(
        client,
        &source(ReferenceKind::ConfigMap, "selected"),
        Some(Duration::from_millis(100)),
    )
    .await
    .unwrap();
    assert_eq!(report.entries, [] as [ReferenceEntry; 0]);
    assert_eq!(report.errors.len(), 5);
    assert!(
        report
            .errors
            .iter()
            .all(|error| error.contains("timed out"))
    );
    server.abort();
    assert!(server.await.unwrap_err().is_cancelled());
}

#[tokio::test]
async fn aborting_scan_does_not_start_continuation_requests() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let client = Client::try_from(kube::Config::new(
        format!("http://{}", listener.local_addr().unwrap())
            .parse()
            .unwrap(),
    ))
    .unwrap();
    let scanner = tokio::spawn(async move {
        scan_references(
            client,
            &source(ReferenceKind::ServiceAccount, "default"),
            None,
        )
        .await
    });
    let mut sockets = vec![];
    for _ in 0..5 {
        let (mut socket, _) = tokio::time::timeout(Duration::from_secs(5), listener.accept())
            .await
            .unwrap()
            .unwrap();
        let path = read_path(&mut socket).await;
        assert!(!path.contains("continue=after-cancel"));
        sockets.push(socket);
    }
    scanner.abort();
    assert!(scanner.await.unwrap_err().is_cancelled());
    let body = list(vec![], "after-cancel").to_string();
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    for mut socket in sockets {
        let _ = socket.write_all(response.as_bytes()).await;
    }
    assert!(
        tokio::time::timeout(Duration::from_millis(50), listener.accept())
            .await
            .is_err()
    );
}
