use super::*;
use crate::model::ResourceKind;
use std::{path::PathBuf, sync::atomic::AtomicU64};

fn request() -> TransferRequest {
    TransferRequest {
        target: ResourceTarget {
            kind: ResourceKind::Pods.into(),
            namespace: Some("team".to_owned()),
            name: "pod".to_owned(),
        },
        download: true,
        from: "team/pod:/tmp/from".to_owned(),
        to: std::env::temp_dir().to_string_lossy().into_owned(),
        no_preserve: false,
        container: "main".to_owned(),
        retries: 999,
    }
}

#[test]
fn containers_include_regular_then_restartable_init_and_ephemeral() {
    let pod: Pod = serde_json::from_value(serde_json::json!({
        "metadata":{"annotations":{"kubectl.kubernetes.io/default-container":"preferred"}},
        "spec":{
            "containers":[{"name":"first"},{"name":"preferred"}],
            "initContainers":[{"name":"ordinary"},{"name":"native-sidecar","restartPolicy":"Always"}],
            "ephemeralContainers":[{"name":"debug"}]
        }
    })).unwrap();
    assert_eq!(
        TransferPod::from_pod(&pod).containers,
        ["preferred", "first", "native-sidecar", "debug"]
    );
    assert_eq!(
        TransferPod::from_pod(&Pod::default()).containers,
        Vec::<String>::new()
    );
}

#[test]
fn kubectl_argv_preserves_shared_connection_and_literal_paths() {
    let mut request = request();
    request.from = "team/pod:/tmp/source $(ignored)".to_owned();
    request.to = "./local file".to_owned();
    request.no_preserve = true;
    request.retries = -1;
    let connection = ConnectionOptions {
        kubeconfig: Some("/config path".into()),
        token: Some("secret".to_owned()),
        impersonate: Some("alice".to_owned()),
        ..ConnectionOptions::default()
    };
    let args = request.kubectl_args(&connection, "captured");
    assert_eq!(
        &args[..connection.kubectl_args(Some("captured")).len()],
        connection.kubectl_args(Some("captured"))
    );
    let args: Vec<_> = args.iter().map(|arg| arg.to_string_lossy()).collect();
    assert!(args.ends_with(&[
        "cp".into(),
        "team/pod:/tmp/source $(ignored)".into(),
        "./local file".into(),
        "--no-preserve=true".into(),
        "--retries=-1".into(),
        "-c=main".into()
    ]));
}

#[test]
fn normalization_accepts_editable_remote_targets_and_rejects_ambiguous_paths() {
    let mut copy = request();
    copy.from = " other:/absolute path ".to_owned();
    copy.to = " ./local ".to_owned();
    assert_eq!(
        copy.clone().normalized().unwrap().from,
        "other:/absolute path"
    );
    for bad in [
        "",
        "-flag",
        "pod:",
        "missing-colon",
        "/pod:/tmp",
        "a/b/c:/tmp",
        "pod:/bad\npath",
    ] {
        copy.from = bad.to_owned();
        assert!(copy.clone().normalized().is_err(), "{bad:?}");
    }
    copy.from = "pod:/tmp".to_owned();
    copy.to = "local:ambiguous".to_owned();
    assert!(copy.normalized().is_err());
}

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "kux-transfer-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    #[cfg(unix)]
    fn program(&self, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = self.0.join("fake-kubectl");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        path
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[tokio::test]
async fn upload_validation_checks_source_and_download_parent_without_blocking_executor() {
    let fixture = Fixture::new();
    let mut copy = request();
    copy.to = fixture.0.join("new-file").to_string_lossy().into_owned();
    assert!(validate_local(&copy).await.is_ok());
    copy.to = fixture
        .0
        .join("missing/new-file")
        .to_string_lossy()
        .into_owned();
    assert!(validate_local(&copy).await.unwrap_err().contains("parent"));
    copy.download = false;
    copy.from = fixture
        .0
        .join("missing-source")
        .to_string_lossy()
        .into_owned();
    copy.to = "team/pod:/tmp/to".to_owned();
    assert!(
        validate_local(&copy)
            .await
            .unwrap_err()
            .contains("Cannot access")
    );
    std::fs::write(&copy.from, "input").unwrap();
    assert!(validate_local(&copy).await.is_ok());
}

#[tokio::test]
async fn demo_never_launches_program_and_read_only_blocks() {
    let permitted = AtomicBool::new(true);
    let (_sender, receiver) = watch::channel(false);
    let result = execute_transfer(
        std::ffi::OsStr::new("/never-exists"),
        request(),
        &ConnectionOptions::default(),
        "demo",
        true,
        &permitted,
        receiver,
    )
    .await
    .unwrap();
    assert!(result.starts_with("Demo: would download"));
    permitted.store(false, Ordering::Release);
    let (_sender, receiver) = watch::channel(false);
    let result = execute_transfer(
        std::ffi::OsStr::new("/never-exists"),
        request(),
        &ConnectionOptions::default(),
        "demo",
        true,
        &permitted,
        receiver,
    )
    .await
    .unwrap_err();
    assert!(result.contains("read-only"));
}

#[tokio::test]
async fn output_is_drained_but_memory_is_bounded() {
    let output = vec![b'x'; OUTPUT_LIMIT * 4];
    let result = bounded_output(output.as_slice()).await;
    assert_eq!(result.len(), OUTPUT_LIMIT);
}

#[cfg(unix)]
#[tokio::test]
async fn subprocess_reports_exit_status_and_redacts_connection_credentials() {
    let fixture = Fixture::new();
    let program = fixture.program("printf '%s\\n' \"$@\" >&2; exit 7");
    let connection = ConnectionOptions {
        token: Some("transfer-token-secret".to_owned()),
        ..ConnectionOptions::default()
    };
    let (_sender, receiver) = watch::channel(false);
    let result = execute_transfer(
        program.as_os_str(),
        request(),
        &connection,
        "captured-context",
        false,
        &AtomicBool::new(true),
        receiver,
    )
    .await
    .unwrap_err();
    assert!(result.contains("exit status: 7"));
    assert!(result.contains("captured-context"));
    assert!(result.contains("[REDACTED]"));
    assert!(!result.contains("transfer-token-secret"));
}

#[cfg(unix)]
#[tokio::test]
async fn fake_kubectl_round_trip_copies_files_with_spaces_and_reports_success() {
    let fixture = Fixture::new();
    // The local fixture maps the remote part after ':' to a file under its own
    // temporary directory, then executes cp with two literal argv entries.
    let program = fixture.program("while [ \"$1\" != cp ]; do shift; done\nshift\nfrom=${1#*:}\nto=${2#*:}\ncp \"$from\" \"$to\"");
    let source = fixture.0.join("local source.txt");
    let remote = fixture.0.join("remote copy.txt");
    let downloaded = fixture.0.join("download result.txt");
    let contents = "literal transfer content\n資料\n";
    std::fs::write(&source, contents).unwrap();
    let mut copy = request();
    copy.download = false;
    copy.from = source.to_string_lossy().into_owned();
    copy.to = format!("team/pod:{}", remote.display());
    for download in [false, true] {
        if download {
            copy.download = true;
            copy.from = copy.to.clone();
            copy.to = downloaded.to_string_lossy().into_owned();
        }
        let (_sender, receiver) = watch::channel(false);
        let result = execute_transfer(
            program.as_os_str(),
            copy.clone(),
            &ConnectionOptions::default(),
            "fixture",
            false,
            &AtomicBool::new(true),
            receiver,
        )
        .await
        .unwrap();
        assert!(result.starts_with(if download {
            "Download completed"
        } else {
            "Upload completed"
        }));
    }
    assert_eq!(std::fs::read_to_string(remote).unwrap(), contents);
    assert_eq!(std::fs::read_to_string(downloaded).unwrap(), contents);
}

#[tokio::test]
async fn loading_containers_uses_selected_namespace_and_surfaces_api_errors() {
    use tokio::io::AsyncWriteExt as _;
    for forbidden in [false, true] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let client = kube::Client::try_from(kube::Config::new(
            format!("http://{address}").parse().unwrap(),
        ))
        .unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut headers = vec![];
            let mut buffer = [0; 1024];
            while !headers.windows(4).any(|window| window == b"\r\n\r\n") {
                let count = socket.read(&mut buffer).await.unwrap();
                assert_ne!(count, 0);
                headers.extend_from_slice(&buffer[..count]);
            }
            let body = if forbidden {
                r#"{"kind":"Status","apiVersion":"v1","status":"Failure","message":"forbidden transfer lookup","reason":"Forbidden","code":403}"#
            } else {
                r#"{"kind":"Pod","apiVersion":"v1","metadata":{"name":"pod","namespace":"team"},"spec":{"containers":[{"name":"worker"}]}}"#
            };
            let status = if forbidden { "403 Forbidden" } else { "200 OK" };
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            String::from_utf8(headers).unwrap()
        });
        let mut target = request().target;
        target.namespace = None;
        assert!(
            load_pod(client.clone(), &target)
                .await
                .unwrap_err()
                .contains("namespace")
        );
        target.namespace = Some("team".to_owned());
        let result = load_pod(client, &target).await;
        if forbidden {
            assert!(result.unwrap_err().contains("forbidden transfer lookup"));
        } else {
            assert_eq!(result.unwrap().containers, ["worker"]);
        }
        assert!(
            server
                .await
                .unwrap()
                .starts_with("GET /api/v1/namespaces/team/pods/pod ")
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn subprocess_cancel_kills_and_reaps_running_child() {
    let fixture = Fixture::new();
    let marker = fixture.0.join("started");
    let program = fixture.program(&format!(
        "printf started > '{}'; exec sleep 60",
        marker.display()
    ));
    let (sender, receiver) = watch::channel(false);
    let task = tokio::spawn(async move {
        execute_transfer(
            program.as_os_str(),
            request(),
            &ConnectionOptions::default(),
            "demo",
            false,
            &AtomicBool::new(true),
            receiver,
        )
        .await
    });
    tokio::time::timeout(Duration::from_secs(10), async {
        while !marker.exists() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    sender.send(true).unwrap();
    let result = tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert_eq!(result, "File transfer canceled");
}

#[tokio::test]
async fn runtime_captures_context_and_generation_and_shutdown_finishes() {
    let mut app = App::new(
        "original",
        None,
        ResourceKind::Pods,
        false,
        vec!["original".to_owned()],
    );
    let mut runtime = TransferRuntime::new(true);
    runtime.submit(
        &mut app,
        TransferIntent::Start {
            id: 7,
            request: Box::new(request()),
        },
    );
    let result = tokio::time::timeout(Duration::from_secs(2), runtime.results.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.id, 7);
    assert_eq!(result.context, "original");
    assert_eq!(result.generation, app.generation);
    assert!(result.result.unwrap().starts_with("Demo:"));
    runtime.shutdown().await;
    assert!(runtime.running.is_none());
}

#[cfg(unix)]
#[tokio::test]
async fn runtime_context_or_read_only_change_cancels_active_transfer() {
    for read_only in [false, true] {
        let fixture = Fixture::new();
        let marker = fixture.0.join("started");
        let program = fixture.program(&format!(
            "printf started > '{}'; exec sleep 60",
            marker.display()
        ));
        let mut app = App::new(
            "original",
            None,
            ResourceKind::Pods,
            false,
            Vec::<String>::new(),
        );
        let mut runtime = TransferRuntime::new(false);
        runtime.program = program.into();
        runtime.submit(
            &mut app,
            TransferIntent::Start {
                id: 3,
                request: Box::new(request()),
            },
        );
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if marker.exists() {
                    break;
                }
                if let Ok(result) = runtime.results.try_recv() {
                    panic!("Transfer exited before launching: {:?}", result.result);
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        if read_only {
            app.read_only = true;
        } else {
            app.context = "changed".to_owned();
        }
        runtime.sync(&app);
        let result = tokio::time::timeout(Duration::from_secs(2), runtime.results.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result.result.unwrap_err(), "File transfer canceled");
        runtime.shutdown().await;
    }
}
