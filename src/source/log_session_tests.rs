use super::*;
use crate::{app::ResourceTarget, model::ResourceKind};
use std::sync::Arc;
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    sync::Notify,
    task::JoinSet,
};

#[derive(Clone)]
enum Reply {
    Json(serde_json::Value),
    Logs(String, bool),
    DelayedLogs(Arc<Notify>, String),
    Hold,
    NotFound,
}

async fn fixture(
    handler: impl Fn(&str) -> Reply + Send + Sync + 'static,
) -> (Client, mpsc::UnboundedReceiver<String>, JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let config = kube::Config::new(
        format!("http://{}", listener.local_addr().unwrap())
            .parse()
            .unwrap(),
    );
    let client = Client::try_from(config).unwrap();
    let handler = Arc::new(handler);
    let (requests, receiver) = mpsc::unbounded_channel();
    let server = tokio::spawn(async move {
        let mut connections = JoinSet::new();
        loop {
            tokio::select! {
                accepted = listener.accept() => {
                    let (mut socket, _) = accepted.unwrap();
                    let handler = handler.clone();
                    let requests = requests.clone();
                    connections.spawn(async move {
                        let mut bytes = Vec::new();
                        let mut chunk = [0;4096];
                        while !bytes.windows(4).any(|window| window == b"\r\n\r\n") {
                            let length = socket.read(&mut chunk).await.unwrap();
                            if length == 0 { return; }
                            bytes.extend_from_slice(&chunk[..length]);
                            assert!(bytes.len() < 65_536);
                        }
                        let wire = String::from_utf8(bytes).unwrap();
                        let path = wire.lines().next().unwrap().split_whitespace().nth(1).unwrap().to_owned();
                        requests.send(path.clone()).unwrap();
                        let reply = handler(&path);
                        let (status, content_type, body, complete) = match reply {
                            Reply::Json(value) => ("200 OK", "application/json", value.to_string(), true),
                            Reply::Logs(body, complete) => ("200 OK", "text/plain", body, complete),
                            Reply::DelayedLogs(gate, body) => {
                                gate.notified().await;
                                ("200 OK", "text/plain", body, true)
                            }
                            Reply::Hold => {
                                // A pending log open must be canceled with the whole session.
                                assert_eq!(socket.read(&mut chunk).await.unwrap(), 0);
                                let _ = requests.send(format!("closed:{path}"));
                                return;
                            }
                            Reply::NotFound => ("404 Not Found", "application/json", r#"{"apiVersion":"v1","kind":"Status","status":"Failure","message":"fixture-token missing","reason":"NotFound","code":404}"#.to_owned(), true),
                        };
                        let mut wire = format!("HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{:x}\r\n{body}\r\n", body.len());
                        if complete { wire.push_str("0\r\n\r\n"); }
                        let _ = socket.write_all(wire.as_bytes()).await;
                    });
                }
                _ = connections.join_next(), if !connections.is_empty() => {}
            }
        }
    });
    (client, receiver, server)
}

fn pod(name: &str, containers: &[&str]) -> serde_json::Value {
    serde_json::json!({
        "apiVersion":"v1","kind":"Pod","metadata":{"name":name,"namespace":"test"},
        "spec":{"containers":containers.iter().map(|name| serde_json::json!({"name":name})).collect::<Vec<_>>()},
        "status":{"phase":"Running"}
    })
}

fn request() -> LogRequest {
    LogRequest::new(
        9,
        LogTarget::pod("test", "demo", None, false),
        &LoggerSettings::default(),
    )
}

async fn next(receiver: &mut mpsc::Receiver<ActionResult>) -> ActionResult {
    let result = tokio::time::timeout(Duration::from_secs(5), receiver.recv())
        .await
        .unwrap()
        .unwrap();
    let ActionResult::Scoped { generation, result } = result else {
        panic!("unscoped log result")
    };
    assert_eq!(generation, 42);
    *result
}

#[tokio::test]
async fn merges_heterogeneous_service_sources_and_only_finishes_after_the_last_source() {
    let gate = Arc::new(Notify::new());
    let waiter = gate.clone();
    let (client, _requests, server) = fixture(move |path| {
        if path == "/api/v1/namespaces/test/services/demo" {
            Reply::Json(serde_json::json!({"apiVersion":"v1","kind":"Service","metadata":{"name":"demo","namespace":"test"},"spec":{"selector":{"app":"demo"}}}))
        } else if path.starts_with("/api/v1/namespaces/test/pods?") {
            Reply::Json(serde_json::json!({"apiVersion":"v1","kind":"PodList","metadata":{},"items":[pod("one", &["app","sidecar"]),pod("two", &["app"])]}))
        } else if path.contains("/pods/two/log?") {
            Reply::DelayedLogs(waiter.clone(), "2026-09-07T10:00:02Z last\n".to_owned())
        } else {
            assert!(path.contains("/pods/one/log?"));
            Reply::Logs("2026-09-07T10:00:00Z same text\n".to_owned(), true)
        }
    }).await;
    let mut request = request();
    request.target = LogTarget::workload(
        ResourceTarget {
            kind: ResourceKind::Services.into(),
            namespace: Some("test".into()),
            name: "demo".into(),
        },
        false,
    );
    let (sender, mut receiver) = mpsc::channel(1);
    let task = LogTask::spawn(client, ConnectionOptions::default(), 42, request, sender);
    let ActionResult::LogResolved { metadata, .. } = next(&mut receiver).await else {
        panic!("missing resolution")
    };
    assert_eq!(metadata.source_count, 3);
    assert!(metadata.multi_pods && metadata.all_containers);
    let mut lines = Vec::new();
    while lines.len() < 2 {
        let ActionResult::LogBatch {
            lines: batch,
            finished,
            error,
            ..
        } = next(&mut receiver).await
        else {
            panic!("unexpected result")
        };
        assert!(!finished);
        assert!(error.is_none());
        lines.extend(batch);
    }
    assert_eq!(
        lines[0].text, lines[1].text,
        "equal messages from different containers must both survive"
    );
    assert_ne!(lines[0].source, lines[1].source);
    assert!(
        lines
            .iter()
            .all(|line| !line.source.as_ref().unwrap().single_container)
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(30), receiver.recv())
            .await
            .is_err()
    );
    gate.notify_one();
    loop {
        let ActionResult::LogBatch {
            lines: batch,
            finished,
            error,
            ..
        } = next(&mut receiver).await
        else {
            panic!("unexpected result")
        };
        assert!(error.is_none());
        lines.extend(batch);
        if finished {
            break;
        }
    }
    assert_eq!(lines.len(), 3);
    assert!(lines[2].source.as_ref().unwrap().single_container);
    drop(task);
    server.abort();
}

#[tokio::test]
async fn every_source_opens_concurrently_and_drop_cancels_all_pending_requests() {
    let (client, mut requests, server) = fixture(|path| {
        if path.contains("/log?") {
            Reply::Hold
        } else {
            let names: Vec<_> = (0..33).map(|index| format!("container-{index}")).collect();
            Reply::Json(pod(
                "demo",
                &names.iter().map(String::as_str).collect::<Vec<_>>(),
            ))
        }
    })
    .await;
    let (sender, mut receiver) = mpsc::channel(1);
    let task = LogTask::spawn(client, ConnectionOptions::default(), 42, request(), sender);
    assert!(
        matches!(next(&mut receiver).await, ActionResult::LogResolved { metadata, .. } if metadata.source_count == 33)
    );
    let mut opened = 0;
    while opened < 33 {
        let path = tokio::time::timeout(Duration::from_secs(5), requests.recv())
            .await
            .unwrap()
            .unwrap();
        if path.contains("/log?") {
            opened += 1;
        }
    }
    drop(task);
    let mut closed = 0;
    while closed < 33 {
        let path = tokio::time::timeout(Duration::from_secs(5), requests.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(path.starts_with("closed:"));
        closed += 1;
    }
    assert!(receiver.recv().await.is_none());
    server.abort();
}

#[tokio::test]
async fn a_failed_container_is_redacted_and_does_not_finish_a_healthy_sibling() {
    let gate = Arc::new(Notify::new());
    let waiter = gate.clone();
    let lookups = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (client, _requests, server) = fixture(move |path| {
        if path.contains("container=broken") {
            Reply::NotFound
        } else if path.contains("container=healthy") {
            Reply::DelayedLogs(waiter.clone(), "2026-09-07T10:00:00Z healthy\n".into())
        } else if lookups.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
            Reply::Json(pod("demo", &["broken", "healthy"]))
        } else {
            Reply::NotFound
        }
    })
    .await;
    let (sender, mut receiver) = mpsc::channel(2);
    let connection = ConnectionOptions {
        token: Some("fixture-token".into()),
        ..ConnectionOptions::default()
    };
    let task = LogTask::spawn(client, connection, 42, request(), sender);
    assert!(matches!(
        next(&mut receiver).await,
        ActionResult::LogResolved { .. }
    ));
    let ActionResult::LogBatch {
        finished, error, ..
    } = next(&mut receiver).await
    else {
        panic!("missing source failure")
    };
    assert!(!finished);
    let error = error.unwrap();
    assert!(error.contains("[broken]") && error.contains("Pod no longer exists"));
    assert!(!error.contains("fixture-token"));
    gate.notify_one();
    let mut healthy = Vec::new();
    loop {
        let ActionResult::LogBatch {
            lines,
            finished,
            error,
            ..
        } = next(&mut receiver).await
        else {
            panic!("unexpected result")
        };
        healthy.extend(lines);
        if finished {
            assert!(error.unwrap().contains("1 of 2 log sources failed"));
            break;
        }
    }
    assert_eq!(healthy.len(), 1);
    assert_eq!(healthy[0].source.as_ref().unwrap().container, "healthy");
    drop(task);
    server.abort();
}

#[test]
fn replay_memory_is_a_session_budget_even_for_many_sources() {
    for count in [1, 2, 33, 5_001] {
        let mut cursor = ResumeCursor::for_sources(count);
        for index in 0..=cursor.max_lines {
            cursor.accept(&format!("2026-09-07T10:00:00Z unique {index}"));
        }
        assert!(cursor.bytes * count <= MAX_RESUME_BYTES);
        assert!(cursor.lines.len() * count <= MAX_RESUME_LINES);
        assert!(!cursor.resumable);
    }
}

#[tokio::test]
async fn container_reconnects_keep_independent_duplicate_counts_for_identical_timestamps() {
    let attempts = Arc::new(std::sync::Mutex::new(BTreeMap::<String, usize>::new()));
    let recorded = attempts.clone();
    let (client, mut requests, server) = fixture(move |path| {
        if !path.contains("/log?") {
            return Reply::Json(pod("demo", &["alpha", "beta"]));
        }
        let container = if path.contains("container=alpha") {
            "alpha"
        } else {
            assert!(path.contains("container=beta"));
            "beta"
        };
        let mut attempts = attempts.lock().unwrap();
        let attempt = attempts.entry(container.to_owned()).or_default();
        *attempt += 1;
        let body = if *attempt == 1 {
            let count = if container == "alpha" { 2 } else { 1 };
            "2026-09-07T10:00:00Z shared\n".repeat(count)
        } else {
            assert_eq!(*attempt, 2, "clean EOF must end this source without another retry");
            format!(
                "2026-09-07T10:00:00Z shared\n2026-09-07T10:00:00Z shared\n2026-09-07T10:00:00Z only-{container}\n2026-09-07T10:00:01Z next-{container}\n"
            )
        };
        Reply::Logs(body, *attempt != 1)
    })
    .await;
    let (sender, mut receiver) = mpsc::channel(1);
    let task = LogTask::spawn(client, ConnectionOptions::default(), 42, request(), sender);
    assert!(
        matches!(next(&mut receiver).await, ActionResult::LogResolved { metadata, .. } if metadata.source_count == 2)
    );
    let mut collected = Vec::new();
    loop {
        let ActionResult::LogBatch {
            lines,
            finished,
            error,
            ..
        } = next(&mut receiver).await
        else {
            panic!("unexpected log result");
        };
        assert!(error.is_none(), "independent reconnect failed: {error:?}");
        collected.extend(lines);
        if finished {
            break;
        }
    }
    assert_eq!(collected.len(), 8);
    for container in ["alpha", "beta"] {
        let lines: Vec<_> = collected
            .iter()
            .filter(|line| line.source.as_ref().unwrap().container == container)
            .map(|line| line.text.as_str())
            .collect();
        assert_eq!(lines.len(), 4);
        assert_eq!(
            lines
                .iter()
                .filter(|line| **line == "2026-09-07T10:00:00Z shared")
                .count(),
            2
        );
        assert_eq!(lines[2], format!("2026-09-07T10:00:00Z only-{container}"));
        assert_eq!(lines[3], format!("2026-09-07T10:00:01Z next-{container}"));
    }
    assert_eq!(
        *recorded.lock().unwrap(),
        BTreeMap::from([("alpha".to_owned(), 2), ("beta".to_owned(), 2)])
    );
    let mut replay_requests = Vec::new();
    while let Ok(path) = requests.try_recv() {
        if path.contains("/log?") && path.contains("sinceTime=") {
            replay_requests.push(path);
        }
    }
    assert_eq!(replay_requests.len(), 2);
    assert!(
        replay_requests
            .iter()
            .all(|path| !path.contains("tailLines="))
    );
    drop(task);
    server.abort();
}

async fn backpressure_fixture() -> (Client, mpsc::UnboundedReceiver<String>, JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let client = Client::try_from(kube::Config::new(
        format!("http://{}", listener.local_addr().unwrap())
            .parse()
            .unwrap(),
    ))
    .unwrap();
    let (requests, received) = mpsc::unbounded_channel();
    let server = tokio::spawn(async move {
        let mut connections = JoinSet::new();
        loop {
            tokio::select! {
                accepted = listener.accept() => {
                    let (mut socket, _) = accepted.unwrap();
                    let requests = requests.clone();
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
                        if !path.contains("/log?") {
                            let body = pod("demo", &["one", "two", "three"]).to_string();
                            socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
                            return;
                        }
                        let body = "2026-09-07T10:00:00Z enough lines to saturate the shared results channel\n".repeat(MAX_BATCH_LINES * 3);
                        socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{:x}\r\n{body}\r\n", body.len()).as_bytes()).await.unwrap();
                        requests.send(format!("written:{path}")).unwrap();
                        match socket.read(&mut chunk).await {
                            Ok(0) => {}
                            Err(error) if matches!(error.kind(), io::ErrorKind::ConnectionReset | io::ErrorKind::ConnectionAborted) => {}
                            result => panic!("expected canceled log connection, got {result:?}"),
                        }
                        requests.send(format!("closed:{path}")).unwrap();
                    });
                }
                _ = connections.join_next(), if !connections.is_empty() => {}
            }
        }
    });
    (client, received, server)
}

#[tokio::test]
async fn dropping_session_cancels_every_stream_while_shared_results_are_backpressured() {
    let (client, mut requests, server) = backpressure_fixture().await;
    let (sender, mut receiver) = mpsc::channel(1);
    let task = LogTask::spawn(client, ConnectionOptions::default(), 42, request(), sender);
    assert!(
        matches!(next(&mut receiver).await, ActionResult::LogResolved { metadata, .. } if metadata.source_count == 3)
    );
    let mut written = std::collections::BTreeSet::new();
    while written.len() < 3 {
        let event = tokio::time::timeout(Duration::from_secs(5), requests.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(event.starts_with("written:"));
        written.insert(event);
    }
    // Keep the receiver alive but stop consuming: cancellation must release
    // every blocked send and HTTP body without relying on receiver closure.
    tokio::time::timeout(Duration::from_secs(5), async {
        while receiver.capacity() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(receiver.len(), 1);
    assert!(!task.task.is_finished());
    drop(task);
    let mut closed = std::collections::BTreeSet::new();
    while closed.len() < 3 {
        let event = tokio::time::timeout(Duration::from_secs(5), requests.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(event.starts_with("closed:"));
        closed.insert(event);
    }
    let ActionResult::LogBatch {
        finished, lines, ..
    } = next(&mut receiver).await
    else {
        panic!("expected the single queued data batch");
    };
    assert!(!finished);
    assert_ne!(lines, Vec::<LogLine>::new());
    assert!(
        tokio::time::timeout(Duration::from_secs(1), receiver.recv())
            .await
            .unwrap()
            .is_none()
    );
    server.abort();
}
