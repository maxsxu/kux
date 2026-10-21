use std::sync::atomic::{AtomicBool, Ordering};

use serde_json::{Value, json};
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    sync::mpsc,
    task::JoinSet,
};

use super::*;
use crate::{
    app::ResourceTarget,
    connection::ConnectionOptions,
    model::{ResourceData, ResourceOperations},
};

enum Reply {
    Json(u16, Value),
    Hold,
}

async fn fixture(
    handler: impl Fn(&str, &str) -> Reply + Send + Sync + 'static,
) -> (
    Client,
    mpsc::UnboundedReceiver<String>,
    tokio::task::JoinHandle<()>,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let client = Client::try_from(kube::Config::new(
        format!("http://{}", listener.local_addr().unwrap())
            .parse()
            .unwrap(),
    ))
    .unwrap();
    let handler = Arc::new(handler);
    let (sender, receiver) = mpsc::unbounded_channel();
    let server = tokio::spawn(async move {
        let mut connections = JoinSet::new();
        loop {
            tokio::select! {
                accepted = listener.accept() => {
                    let (mut socket, _) = accepted.unwrap();
                    let handler = handler.clone();
                    let sender = sender.clone();
                    connections.spawn(async move {
                        let mut bytes = Vec::new();
                        let mut chunk = [0; 4096];
                        while !bytes.windows(4).any(|window| window == b"\r\n\r\n") {
                            let count = socket.read(&mut chunk).await.unwrap();
                            if count == 0 { return; }
                            bytes.extend_from_slice(&chunk[..count]);
                        }
                        let request = String::from_utf8(bytes).unwrap();
                        let path = request.lines().next().unwrap().split_whitespace().nth(1).unwrap();
                        let _ = sender.send(path.to_owned());
                        match handler(path, &request) {
                            Reply::Json(code, body) => {
                                let body = body.to_string();
                                socket.write_all(format!("HTTP/1.1 {code} Response\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
                            }
                            Reply::Hold => {
                                assert_eq!(socket.read(&mut chunk).await.unwrap(), 0);
                                let _ = sender.send(format!("closed:{path}"));
                            }
                        }
                    });
                }
                _ = connections.join_next(), if !connections.is_empty() => {}
            }
        }
    });
    (client, receiver, server)
}

fn settings() -> (KubernetesSettings, watch::Sender<Duration>) {
    let (sender, receiver) = watch::channel(Duration::from_secs(600));
    (
        KubernetesSettings {
            connection: ConnectionOptions::default(),
            initial_context: "fixture".to_owned(),
            publish_interval: Duration::from_millis(10),
            refresh_interval: receiver,
            streaming_lists: false,
        },
        sender,
    )
}

fn instance_request(kind: ResourceDescriptor) -> WatchRequest {
    WatchRequest {
        generation: 51,
        context: "fixture".to_owned(),
        kind: kind.clone(),
        namespace: Some("active".to_owned()),
        field_selector: Some("metadata.name=other".to_owned()),
        label_selector: Some("ignored=selector".to_owned()),
        instance: Some(ResourceTarget {
            kind,
            namespace: Some("target".to_owned()),
            name: "selected".to_owned(),
        }),
    }
}

fn workload_request() -> WatchRequest {
    WatchRequest {
        generation: 51,
        context: "fixture".to_owned(),
        kind: ResourceDescriptor::workloads(),
        namespace: Some("target".to_owned()),
        field_selector: None,
        label_selector: Some("app=fixture".to_owned()),
        instance: None,
    }
}

fn pod() -> Value {
    json!({"apiVersion":"v1","kind":"Pod","metadata":{"name":"selected","namespace":"target"},
        "spec":{"containers":[{"name":"main"}]},"status":{"phase":"Running","containerStatuses":[{"name":"main","ready":true,"restartCount":3,"image":"fixture","imageID":"fixture-id"}]}})
}

fn error(code: u16) -> Value {
    json!({"apiVersion":"v1","kind":"Status","status":"Failure","code":code,
        "reason":if code == 403 { "Forbidden" } else { "NotFound" },"message":"fixture-token denied selected object"})
}

fn kind_for_path(path: &str) -> ResourceDescriptor {
    workload_kinds()
        .into_iter()
        .find(|kind| {
            path.split('?')
                .next()
                .unwrap()
                .ends_with(&format!("/{}", kind.plural))
        })
        .unwrap()
}

fn table(kind: &ResourceDescriptor, name: &str) -> Value {
    let (columns, cells) = match kind.kind.as_ref() {
        "Pod" => (
            vec!["Name", "Ready", "Status"],
            vec![json!(name), json!("1/1"), json!("Running")],
        ),
        "Service" => (vec!["Name", "Type"], vec![json!(name), json!("ClusterIP")]),
        "DaemonSet" | "ReplicaSet" => (
            vec!["Name", "Desired", "Ready"],
            vec![json!(name), json!(2), json!(2)],
        ),
        _ => (vec!["Name", "Ready"], vec![json!(name), json!("2/2")]),
    };
    json!({"apiVersion":"meta.k8s.io/v1","kind":"Table","metadata":{"resourceVersion":"8"},
        "columnDefinitions":columns.into_iter().map(|name| json!({"name":name})).collect::<Vec<_>>(),
        "rows":[{"cells":cells,"object":{"apiVersion":"meta.k8s.io/v1","kind":"PartialObjectMetadata","metadata":{"name":name,"namespace":"target","creationTimestamp":"2026-09-07T10:00:00Z","labels":{"app":"fixture"}}}}]})
}

async fn next_snapshot(receiver: &mut watch::Receiver<ResourceSnapshot>) -> ResourceSnapshot {
    tokio::time::timeout(Duration::from_secs(5), receiver.changed())
        .await
        .unwrap()
        .unwrap();
    receiver.borrow_and_update().clone()
}

#[tokio::test]
async fn exact_instance_get_works_when_list_is_forbidden_and_active_namespace_differs() {
    let (client, mut paths, server) = fixture(|path, _| {
        if path == "/api/v1/namespaces/target/pods/selected" {
            Reply::Json(200, pod())
        } else {
            Reply::Json(403, error(403))
        }
    })
    .await;
    let mut kind: ResourceDescriptor = ResourceKind::Pods.into();
    kind.operations = ResourceOperations::new(false, true, false);
    let request = instance_request(kind);
    let rows = load_instance(&client, &request, None).await.unwrap();
    assert_eq!(rows.rows.len(), 1);
    assert_eq!(rows.rows[0].namespace.as_ref(), "target");
    assert_eq!(rows.rows[0].pod_data().unwrap().restarts, 3);
    assert_eq!(
        paths.recv().await.unwrap(),
        "/api/v1/namespaces/target/pods/selected"
    );
    assert!(
        paths.try_recv().is_err(),
        "GET must not be preceded or followed by a LIST/watch"
    );
    server.abort();
}

#[tokio::test]
async fn instance_table_get_preserves_crd_columns_and_metadata_identity() {
    let kind = ResourceDescriptor::new(
        "example.io",
        "v1",
        "example.io/v1",
        "Widget",
        "widgets",
        true,
        ResourceOperations::new(false, true, false),
    );
    let (client, mut paths, server) = fixture(|path, raw| {
        assert_eq!(path, "/apis/example.io/v1/namespaces/target/widgets/selected?includeObject=Object");
        assert!(raw.contains("as=Table"));
        Reply::Json(200, json!({"apiVersion":"meta.k8s.io/v1","kind":"Table",
            "columnDefinitions":[{"name":"Name"},{"name":"Custom Phase","priority":1}],
            "rows":[{"cells":["selected","Reconciling"],"object":{"metadata":{"name":"selected","namespace":"target"}}}]}))
    }).await;
    let rows = load_instance(&client, &instance_request(kind), None)
        .await
        .unwrap();
    assert_eq!(rows.columns[1].name.as_ref(), "Custom Phase");
    assert_eq!(rows.columns[1].priority, 1);
    assert_eq!(rows.rows[0].table_cells[1].text.as_ref(), "Reconciling");
    assert!(paths.recv().await.unwrap().contains("/widgets/selected?"));
    assert!(paths.try_recv().is_err());
    server.abort();
}

#[tokio::test]
async fn instance_permissions_and_not_found_publish_scoped_redacted_errors() {
    for code in [403, 404] {
        let (client, mut paths, server) = fixture(move |_, _| Reply::Json(code, error(code))).await;
        let request = instance_request(ResourceKind::Pods.into());
        let (mut settings, _cadence) = settings();
        settings.connection.token = Some("fixture-token".to_owned());
        let (sender, mut receiver) = watch::channel(ResourceSnapshot::loading(
            request.generation,
            request.kind.clone(),
        ));
        let task = tokio::spawn(run_instance(client, settings, request, sender));
        let snapshot = next_snapshot(&mut receiver).await;
        assert_eq!(snapshot.generation, 51);
        assert!(snapshot.rows.is_empty());
        let SourceState::Error(message) = snapshot.state else {
            panic!("expected read failure");
        };
        assert!(message.contains(if code == 403 { "Forbidden" } else { "NotFound" }));
        assert!(!message.contains("fixture-token"));
        assert!(paths.recv().await.unwrap().ends_with("/pods/selected"));
        assert!(paths.try_recv().is_err());
        drop(receiver);
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap();
        server.abort();
    }
}

#[tokio::test]
async fn instance_never_accepts_a_different_object_or_namespace() {
    for (pointer, value) in [
        ("/metadata/name", "other"),
        ("/metadata/namespace", "active"),
        ("/kind", "Service"),
        ("/apiVersion", "other/v1"),
    ] {
        let mut object = pod();
        *object.pointer_mut(pointer).unwrap() = json!(value);
        let (client, _, server) = fixture(move |_, _| Reply::Json(200, object.clone())).await;
        assert!(
            load_instance(&client, &instance_request(ResourceKind::Pods.into()), None)
                .await
                .unwrap_err()
                .contains("selected resource scope")
        );
        server.abort();
    }
}

#[tokio::test]
async fn invalid_instance_targets_fail_before_any_api_request() {
    let (client, mut paths, server) = fixture(|_, _| Reply::Json(404, error(404))).await;
    for case in 0..6 {
        let mut request = instance_request(ResourceKind::Pods.into());
        match case {
            0 => request.instance = None,
            1 => request.instance.as_mut().unwrap().name.clear(),
            2 => request.instance.as_mut().unwrap().namespace = None,
            3 => {
                request.instance.as_mut().unwrap().kind.operations =
                    ResourceOperations::new(false, false, false);
            }
            4 => request.kind = ResourceKind::Services.into(),
            _ => {
                request.kind = ResourceDescriptor::workloads();
                request.instance.as_mut().unwrap().kind = ResourceDescriptor::workloads();
            }
        }
        assert!(load_instance(&client, &request, None).await.is_err());
    }
    assert!(paths.try_recv().is_err());
    server.abort();
}

#[tokio::test]
async fn cluster_instance_get_and_raw_crd_fallback_keep_their_exact_scopes() {
    let (client, mut paths, server) = fixture(|path, _| {
        if path == "/api/v1/nodes/selected" {
            Reply::Json(200, json!({"apiVersion":"v1","kind":"Node","metadata":{"name":"selected"},"status":{}}))
        } else {
            assert_eq!(path, "/apis/example.io/v1/namespaces/target/widgets/selected?includeObject=Object");
            Reply::Json(200, json!({"apiVersion":"example.io/v1","kind":"Widget","metadata":{"name":"selected","namespace":"target"}}))
        }
    }).await;
    let mut node = instance_request(ResourceKind::Nodes.into());
    node.instance.as_mut().unwrap().namespace = None;
    let rows = load_instance(&client, &node, None).await.unwrap();
    assert_eq!(rows.rows[0].name.as_ref(), "selected");
    assert!(rows.rows[0].node_data().is_some());
    let widget = ResourceDescriptor::new(
        "example.io",
        "v1",
        "example.io/v1",
        "Widget",
        "widgets",
        true,
        ResourceOperations::all(),
    );
    let rows = load_instance(&client, &instance_request(widget), None)
        .await
        .unwrap();
    assert_eq!(rows.rows[0].namespace.as_ref(), "target");
    assert!(rows.columns.is_empty());
    assert_eq!(paths.recv().await.unwrap(), "/api/v1/nodes/selected");
    assert!(paths.recv().await.unwrap().contains("/widgets/selected?"));
    assert!(paths.try_recv().is_err());
    server.abort();
}

#[tokio::test]
async fn instance_tables_cannot_widen_or_substitute_the_selected_identity() {
    let widget = ResourceDescriptor::new(
        "example.io",
        "v1",
        "example.io/v1",
        "Widget",
        "widgets",
        true,
        ResourceOperations::all(),
    );
    for case in 0..5 {
        let (client, _, server) = fixture(move |_, _| {
            let mut value = json!({"apiVersion":"meta.k8s.io/v1","kind":"Table","columnDefinitions":[{"name":"Name"}],
                "rows":[{"cells":["selected"],"object":{"metadata":{"name":"selected","namespace":"target"}}}]});
            match case {
                0 => value["rows"] = json!([]),
                1 => value["rows"][0]["object"]["metadata"]["namespace"] = json!("active"),
                2 => value["rows"][0]["cells"][0] = json!("other"),
                3 => { value["rows"][0].as_object_mut().unwrap().remove("object"); }
                _ => { let duplicate = value["rows"][0].clone(); value["rows"].as_array_mut().unwrap().push(duplicate); }
            }
            Reply::Json(200, value)
        }).await;
        assert!(
            load_instance(&client, &instance_request(widget.clone()), None)
                .await
                .is_err(),
            "case {case}"
        );
        server.abort();
    }
}

#[tokio::test]
async fn workload_aggregate_reads_six_real_tables_and_keeps_same_named_resources_distinct() {
    let (client, mut paths, server) = fixture(|path, raw| {
        assert!(raw.contains("as=Table"));
        assert!(path.contains("labelSelector=app%3Dfixture"));
        assert!(path.contains("includeObject=Object"));
        assert!(path.contains("/namespaces/target/"));
        Reply::Json(200, table(&kind_for_path(path), "same"))
    })
    .await;
    let result = load_workloads(&client, &workload_request(), None)
        .await
        .unwrap();
    assert_eq!(result.rows.len(), 6);
    let keys: BTreeSet<_> = result.rows.iter().map(|row| row.key.as_ref()).collect();
    assert_eq!(keys.len(), 6);
    for row in &result.rows {
        let ResourceData::Workload { kind } = &row.data else {
            panic!("missing actual resource kind");
        };
        assert_eq!(row.key.as_ref(), format!("{}|target|same", kind.gvr()));
        assert_eq!(row.table_cells[0].text.as_ref(), kind.gvr());
    }
    let mut seen = BTreeSet::new();
    for _ in 0..6 {
        seen.insert(kind_for_path(&paths.recv().await.unwrap()).kind.clone());
    }
    assert_eq!(seen.len(), 6);
    assert!(paths.try_recv().is_err());
    assert_eq!(result.columns[5].priority, 1);
    assert_eq!(result.columns[6].name.as_ref(), "AGE");
    server.abort();
}

#[tokio::test]
async fn workload_pagination_keeps_selectors_and_collects_every_page_before_publishing() {
    let (client, mut paths, server) = fixture(|path, _| {
        let kind = kind_for_path(path);
        let second = path.contains("continue=next");
        let mut result = table(&kind, if second { "second" } else { "first" });
        if kind.known_kind() == Some(ResourceKind::Pods) && !second {
            result["metadata"]["continue"] = json!("next");
        }
        if second {
            result["columnDefinitions"] = json!([]);
        }
        Reply::Json(200, result)
    })
    .await;
    let result = load_workloads(&client, &workload_request(), None)
        .await
        .unwrap();
    assert_eq!(result.rows.len(), 7);
    assert!(
        result
            .rows
            .iter()
            .any(|row| row.key.as_ref() == "v1/pods|target|second")
    );
    let mut second = None;
    while let Ok(path) = paths.try_recv() {
        if path.contains("continue=") {
            second = Some(path);
        }
    }
    let second = second.unwrap();
    assert!(second.contains("labelSelector=app%3Dfixture"));
    assert!(second.contains("limit=5000"));
    server.abort();
}

#[tokio::test]
async fn failed_aggregate_refresh_retains_only_the_previous_complete_snapshot() {
    let denied = Arc::new(AtomicBool::new(false));
    let flag = denied.clone();
    let (client, _, server) = fixture(move |path, _| {
        let kind = kind_for_path(path);
        if flag.load(Ordering::SeqCst) && kind.known_kind() == Some(ResourceKind::Services) {
            Reply::Json(403, error(403))
        } else {
            Reply::Json(
                200,
                table(
                    &kind,
                    if flag.load(Ordering::SeqCst) {
                        "partial-new"
                    } else {
                        "complete"
                    },
                ),
            )
        }
    })
    .await;
    let request = workload_request();
    let (settings, cadence) = settings();
    let (sender, mut receiver) = watch::channel(ResourceSnapshot::loading(
        request.generation,
        request.kind.clone(),
    ));
    let task = tokio::spawn(run_workloads(client, settings, request, sender));
    let first = next_snapshot(&mut receiver).await;
    assert!(matches!(first.state, SourceState::Ready));
    assert_eq!(first.rows.len(), 6);
    denied.store(true, Ordering::SeqCst);
    cadence.send(Duration::from_secs(2)).unwrap();
    let failed = next_snapshot(&mut receiver).await;
    assert!(matches!(failed.state, SourceState::Error(_)));
    assert_eq!(failed.generation, 51);
    assert!(failed.revision > first.revision);
    assert_eq!(failed.rows.len(), 6);
    assert!(
        failed
            .rows
            .iter()
            .all(|row| row.name.as_ref() == "complete")
    );
    drop(receiver);
    tokio::time::timeout(Duration::from_secs(1), task)
        .await
        .unwrap()
        .unwrap();
    server.abort();
}

#[tokio::test]
async fn first_aggregate_failure_has_no_partial_rows_and_no_synthetic_api_call() {
    let (client, _, server) = fixture(|path, _| {
        let kind = kind_for_path(path);
        if kind.is_apps_v1_replica_set() {
            Reply::Json(403, error(403))
        } else {
            Reply::Json(200, table(&kind, "partial"))
        }
    })
    .await;
    let result = load_workloads(&client, &workload_request(), None).await;
    assert!(result.unwrap_err().contains("Forbidden"));
    server.abort();
}

#[tokio::test]
async fn aggregate_all_namespace_scope_and_field_selectors_reach_each_real_endpoint() {
    let (client, mut paths, server) = fixture(|path, _| {
        assert!(!path.contains("/namespaces/"));
        assert!(path.contains("fieldSelector=metadata.name%3Dsame"));
        Reply::Json(200, table(&kind_for_path(path), "same"))
    })
    .await;
    let mut request = workload_request();
    request.namespace = None;
    request.field_selector = Some("metadata.name=same".to_owned());
    assert_eq!(
        load_workloads(&client, &request, None)
            .await
            .unwrap()
            .rows
            .len(),
        6
    );
    for _ in 0..6 {
        assert!(!paths.recv().await.unwrap().contains("workloads"));
    }
    server.abort();
}

#[tokio::test]
async fn aggregate_rejects_repeated_tokens_wrong_namespace_and_non_tables() {
    for case in 0..3 {
        let (client, _, server) = fixture(move |path, _| {
            let kind = kind_for_path(path);
            let mut value = table(&kind, "selected");
            if kind.known_kind() == Some(ResourceKind::Pods) {
                match case {
                    0 => value["metadata"]["continue"] = json!("repeat"),
                    1 => value["rows"][0]["object"]["metadata"]["namespace"] = json!("foreign"),
                    _ => value = json!({"apiVersion":"v1","kind":"PodList","items":[]}),
                }
            }
            Reply::Json(200, value)
        })
        .await;
        assert!(
            load_workloads(&client, &workload_request(), None)
                .await
                .is_err()
        );
        server.abort();
    }
}

#[tokio::test]
async fn configured_deadline_bounds_instance_and_aggregate_requests() {
    for instance in [false, true] {
        let (client, _, server) = fixture(|_, _| Reply::Hold).await;
        let request = if instance {
            instance_request(ResourceKind::Pods.into())
        } else {
            workload_request()
        };
        let (mut settings, _cadence) = settings();
        settings.connection.request_timeout = Some("30ms".to_owned());
        let (sender, mut receiver) = watch::channel(ResourceSnapshot::loading(
            request.generation,
            request.kind.clone(),
        ));
        let view = if instance {
            View::Instance
        } else {
            View::Workloads
        };
        let task = tokio::spawn(run(client, settings, request, sender, view));
        let snapshot = next_snapshot(&mut receiver).await;
        let SourceState::Error(message) = snapshot.state else {
            panic!("expected timeout");
        };
        assert!(message.contains("Timed out"));
        assert!(snapshot.rows.is_empty());
        drop(receiver);
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap();
        server.abort();
    }
}

#[tokio::test]
async fn closing_view_cancels_every_unbounded_request_without_detached_tasks() {
    for instance in [false, true] {
        let (client, mut paths, server) = fixture(|_, _| Reply::Hold).await;
        let request = if instance {
            instance_request(ResourceKind::Pods.into())
        } else {
            workload_request()
        };
        let (mut settings, _cadence) = settings();
        settings.connection.request_timeout = Some("0".to_owned());
        let (sender, receiver) = watch::channel(ResourceSnapshot::loading(
            request.generation,
            request.kind.clone(),
        ));
        let view = if instance {
            View::Instance
        } else {
            View::Workloads
        };
        let task = tokio::spawn(run(client, settings, request, sender, view));
        let count = if instance { 1 } else { 6 };
        for _ in 0..count {
            assert!(
                !tokio::time::timeout(Duration::from_secs(2), paths.recv())
                    .await
                    .unwrap()
                    .unwrap()
                    .starts_with("closed:")
            );
        }
        drop(receiver);
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap();
        for _ in 0..count {
            assert!(
                tokio::time::timeout(Duration::from_secs(2), paths.recv())
                    .await
                    .unwrap()
                    .unwrap()
                    .starts_with("closed:")
            );
        }
        server.abort();
    }
}

#[test]
fn projection_matches_pinned_readiness_status_and_validity_rules() {
    let cases = [
        ("Pod", "0/1", "Completed", "OK"),
        ("Pod", "1/1", "Running", "OK"),
        ("Pod", "0/1", "Running", "DEGRADED"),
        ("Pod", "1/1", "Pending", "DEGRADED"),
        ("Deployment", "0/0", "", "OK"),
        ("StatefulSet", "9/0", "", "OK"),
        ("Deployment", "1/2", "", "DEGRADED"),
        ("StatefulSet", "broken", "", "DEGRADED"),
    ];
    for (name, ready, phase, expected) in cases {
        let kind = workload_kinds()
            .into_iter()
            .find(|kind| kind.kind.as_ref() == name)
            .unwrap();
        let columns: Arc<[TableColumn]> = ["Ready", "Status"]
            .map(|name| TableColumn {
                name: name.into(),
                format: "".into(),
                priority: 0,
            })
            .into();
        let row = ResourceRow::new("team", "name", "", "", "", 100).with_table_cells(vec![
            TableCell {
                text: ready.into(),
                timestamp_unix: None,
            },
            TableCell {
                text: phase.into(),
                timestamp_unix: None,
            },
        ]);
        let projected = project_workload_row(&kind, &row, &columns).unwrap();
        assert_eq!(
            projected.status.as_ref(),
            expected,
            "{name} {ready} {phase}"
        );
        assert_eq!(projected.ready.as_ref(), ready);
        assert_eq!(
            projected.table_cells[5].text.as_ref(),
            if expected == "OK" { "" } else { "DEGRADED" }
        );
        assert_eq!(projected.table_cells[6].timestamp_unix, Some(100));
    }
    for kind in workload_kinds()
        .into_iter()
        .filter(|kind| matches!(kind.kind.as_ref(), "DaemonSet" | "ReplicaSet" | "Service"))
    {
        for (actual, desired, expected) in [(2, 2, "OK"), (0, 0, "OK"), (0, 1, "DEGRADED")] {
            let mut value = table(&kind, "name");
            if kind.kind.as_ref() != "Service" {
                value["rows"][0]["cells"] = json!(["name", desired, actual]);
            }
            let decoded = decode_server_table(parse_table(value).unwrap(), &[]);
            let projected =
                project_workload_row(&kind, &decoded.rows[0], &decoded.columns).unwrap();
            assert_eq!(
                projected.status.as_ref(),
                if kind.kind.as_ref() == "Service" {
                    "OK"
                } else {
                    expected
                }
            );
            if kind.kind.as_ref() == "Service" {
                assert_eq!(projected.ready.as_ref(), "");
            } else {
                assert_eq!(projected.ready.as_ref(), format!("{actual}/{desired}"));
            }
        }
    }
}

#[tokio::test]
async fn subject_views_scan_cluster_and_role_bindings_and_prefer_cluster_location() {
    let (client, mut paths, server) = fixture(|path, _| {
        if path.starts_with("/apis/rbac.authorization.k8s.io/v1/clusterrolebindings") {
            Reply::Json(
                200,
                json!({
                    "apiVersion": "rbac.authorization.k8s.io/v1",
                    "kind": "ClusterRoleBindingList",
                    "metadata": {},
                    "items": [{
                        "metadata": {"name": "cluster-admins"},
                        "roleRef": {"apiGroup": "rbac.authorization.k8s.io", "kind": "ClusterRole", "name": "cluster-admin"},
                        "subjects": [
                            {"kind": "User", "name": "alice"},
                            {"kind": "Group", "name": "ops"}
                        ]
                    }]
                }),
            )
        } else if path.starts_with("/apis/rbac.authorization.k8s.io/v1/rolebindings") {
            Reply::Json(
                200,
                json!({
                    "apiVersion": "rbac.authorization.k8s.io/v1",
                    "kind": "RoleBindingList",
                    "metadata": {},
                    "items": [{
                        "metadata": {"name": "dev-edit", "namespace": "dev"},
                        "roleRef": {"apiGroup": "rbac.authorization.k8s.io", "kind": "Role", "name": "edit"},
                        "subjects": [
                            {"kind": "User", "name": "alice"},
                            {"kind": "User", "name": "bob"},
                            {"kind": "Group", "name": "devs"}
                        ]
                    }]
                }),
            )
        } else {
            panic!("unexpected path {path}");
        }
    })
    .await;

    let user_request = WatchRequest {
        generation: 1,
        context: "test".to_owned(),
        kind: ResourceDescriptor::users(),
        namespace: None,
        field_selector: None,
        label_selector: None,
        instance: None,
    };
    let users = load_subjects(&client, &user_request, SubjectKind::User, None)
        .await
        .unwrap();
    assert_eq!(
        users
            .columns
            .iter()
            .map(|col| col.name.as_ref())
            .collect::<Vec<_>>(),
        ["NAME", "KIND", "FIRST LOCATION"]
    );
    assert_eq!(users.rows.len(), 2);
    assert_eq!(users.rows[0].name.as_ref(), "alice");
    assert_eq!(users.rows[0].status.as_ref(), "ClusterRoleBinding");
    assert_eq!(users.rows[0].ready.as_ref(), "cluster-admins");
    assert_eq!(users.rows[1].name.as_ref(), "bob");
    assert_eq!(users.rows[1].status.as_ref(), "RoleBinding");
    assert_eq!(users.rows[1].ready.as_ref(), "dev/dev-edit");

    let group_request = WatchRequest {
        kind: ResourceDescriptor::groups(),
        ..user_request
    };
    let groups = load_subjects(&client, &group_request, SubjectKind::Group, None)
        .await
        .unwrap();
    assert_eq!(groups.rows.len(), 2);
    assert_eq!(groups.rows[0].name.as_ref(), "devs");
    assert_eq!(groups.rows[0].status.as_ref(), "RoleBinding");
    assert_eq!(groups.rows[0].ready.as_ref(), "dev/dev-edit");
    assert_eq!(groups.rows[1].name.as_ref(), "ops");
    assert_eq!(groups.rows[1].status.as_ref(), "ClusterRoleBinding");
    assert_eq!(groups.rows[1].ready.as_ref(), "cluster-admins");

    for _ in 0..4 {
        let _ = paths.recv().await.unwrap();
    }
    server.abort();
}

#[test]
fn helm_rows_and_dir_entries_project_with_expected_columns_and_filters() {
    let ts = parse_helm_timestamp("2025-10-16 12:30:00.000000 +0000 UTC\tmylib-1.0.0");
    assert!(ts > 0);
    let helm_row = project_helm_row(
        "default".to_owned(),
        "redis".to_owned(),
        "3".to_owned(),
        "deployed".to_owned(),
        "redis-19.5.0".to_owned(),
        "7.2.5",
        ts,
    );
    assert_eq!(helm_row.namespace.as_ref(), "default");
    assert_eq!(helm_row.name.as_ref(), "redis");
    assert_eq!(helm_row.ready.as_ref(), "3");
    assert_eq!(helm_row.status.as_ref(), "deployed");
    assert_eq!(helm_row.table_cells[6].text.as_ref(), "");

    let failed_row = project_helm_row(
        "default".to_owned(),
        "payments".to_owned(),
        "2".to_owned(),
        "failed".to_owned(),
        "payments-0.1.0".to_owned(),
        "0.1.0",
        ts,
    );
    assert_eq!(failed_row.table_cells[6].text.as_ref(), "DEGRADED");

    let hist_row = project_helm_history_row(
        "default".to_owned(),
        "redis",
        "2".to_owned(),
        "superseded".to_owned(),
        "redis-19.4.0",
        "7.2.4",
        "Upgrade complete".to_owned(),
        ts,
    );
    assert_eq!(hist_row.name.as_ref(), "redis:2");
    assert_eq!(hist_row.ready.as_ref(), "2");
    assert_eq!(hist_row.table_cells[5].text.as_ref(), "");

    let dir = std::env::temp_dir().join(format!("kux-dir-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("sub")).unwrap();
    std::fs::write(dir.join("deploy.yaml"), "kind: Deployment\n").unwrap();
    std::fs::write(dir.join("service.yml"), "kind: Service\n").unwrap();
    std::fs::write(dir.join("config.json"), "{}\n").unwrap();
    std::fs::write(dir.join("README.md"), "# ignore\n").unwrap();
    std::fs::write(dir.join(".hidden.yaml"), "kind: Pod\n").unwrap();

    let rows = read_dir_rows(dir.to_str().unwrap()).unwrap();
    let names: Vec<_> = rows
        .iter()
        .map(|row| row.table_cells[0].text.as_ref())
        .collect();
    assert_eq!(
        names,
        vec![
            "🦄 config.json",
            "🦄 deploy.yaml",
            "🦄 service.yml",
            "📁 sub"
        ]
    );
    let _ = std::fs::remove_dir_all(&dir);
}
