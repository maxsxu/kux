use super::*;
use serde_json::json;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

fn job_kind() -> ResourceDescriptor {
    ResourceDescriptor::new(
        "batch",
        "v1",
        "batch/v1",
        "Job",
        "jobs",
        true,
        crate::model::ResourceOperations::all(),
    )
}

fn request(selector: &str) -> WatchRequest {
    WatchRequest {
        instance: None,
        generation: 17,
        context: "test".to_owned(),
        kind: job_kind(),
        namespace: Some("team".to_owned()),
        field_selector: Some(selector.to_owned()),
        label_selector: None,
    }
}

fn job() -> serde_json::Value {
    json!({"apiVersion":"batch/v1","kind":"Job","metadata":{
        "name":"run","namespace":"team","ownerReferences":[
            {"apiVersion":"example.com/v1","kind":"Other","name":"controller","uid":"one","controller":true},
            {"apiVersion":"example.com/v2","kind":"Foreign","name":"selected-cronjob","uid":"different-incarnation","controller":false}
        ]
    },"spec":{}})
}

#[test]
fn cronjob_local_filter_matches_any_exact_owner_name_in_typed_and_table_rows() {
    let object: DynamicObject = serde_json::from_value(job()).unwrap();
    let dynamic = dynamic_row(&object);
    let mut metadata = job();
    metadata["apiVersion"] = json!("meta.k8s.io/v1");
    metadata["kind"] = json!("PartialObjectMetadata");
    metadata.as_object_mut().unwrap().remove("spec");
    let table_rows = [job(), metadata].map(|object| {
        decode_table_list(json!({
            "apiVersion":"meta.k8s.io/v1","kind":"Table","metadata":{},
            "columnDefinitions":[{"name":"Name","type":"string","format":"name"}],
            "rows":[{"cells":["run"],"object":object}]
        }))
        .unwrap()
        .rows
        .remove(0)
    });
    for row in std::iter::once(&dynamic).chain(table_rows.iter().map(AsRef::as_ref)) {
        assert_eq!(row.owner.as_ref().unwrap().name.as_ref(), "controller");
        assert_eq!(row.owner_names.len(), 2);
        assert!(row_matches_owner_filter(
            row,
            &request("kux.ownerName=selected-cronjob")
        ));
        assert!(row_matches_owner_filter(
            row,
            &request("kux.ownerName=controller")
        ));
        assert!(!row_matches_owner_filter(
            row,
            &request("kux.ownerName=selected")
        ));
        assert!(!row_matches_owner_filter(
            row,
            &request("kux.ownerName=Selected-cronjob")
        ));
    }
    assert_eq!(
        server_field_selector(&request("kux.ownerName=selected-cronjob")),
        None
    );
    assert!(!row_matches_owner_filter(
        &ResourceRow::new("team", "unowned", "-", "-", "", 0),
        &request("kux.ownerName=selected-cronjob")
    ));
}

#[test]
fn progressive_and_table_publication_remove_jobs_when_owner_changes() {
    let object: DynamicObject = serde_json::from_value(job()).unwrap();
    let row = Arc::new(dynamic_row(&object));
    let mut rows = FxHashMap::from_iter([(row.key.clone(), row.clone())]);
    let request = request("kux.ownerName=selected-cronjob");
    let (send, received) = watch::channel(ResourceSnapshot::loading(17, request.kind.clone()));
    publish(
        &send,
        &request,
        &rows,
        SourceState::Loading { loaded: 1 },
        1,
    );
    assert_eq!(received.borrow().rows.len(), 1);
    publish_table(&send, &request, &rows, Arc::from([]), SourceState::Ready, 2);
    assert_eq!(received.borrow().rows.len(), 1);
    let mut changed = row.as_ref().clone();
    changed.owner_names = Arc::from([Arc::from("another-cronjob")]);
    rows.insert(changed.key.clone(), Arc::new(changed));
    publish_table(&send, &request, &rows, Arc::from([]), SourceState::Ready, 3);
    assert!(received.borrow().rows.is_empty());
    assert_eq!(received.borrow().generation, 17);
    assert_eq!(received.borrow().state, SourceState::Ready);
}

#[test]
fn nil_job_selector_matches_no_rows_without_sending_internal_fields_to_server() {
    let mut request = request("kux.matchNone=true");
    request.kind = ResourceKind::Pods.into();
    let row = Arc::new(ResourceRow::new("team", "pod", "Running", "1/1", "", 0));
    assert_eq!(server_field_selector(&request), None);
    assert!(!row_matches_owner_filter(&row, &request));
    let (send, received) = watch::channel(ResourceSnapshot::loading(17, request.kind.clone()));
    let rows = FxHashMap::from_iter([(row.key.clone(), row)]);
    publish(&send, &request, &rows, SourceState::Ready, 1);
    assert!(received.borrow().rows.is_empty());
    assert_eq!(received.borrow().state, SourceState::Ready);
    request.field_selector = None;
    request.label_selector = Some(String::new());
    publish(&send, &request, &rows, SourceState::Ready, 2);
    assert_eq!(received.borrow().rows.len(), 1);
}

#[tokio::test]
async fn batch_navigation_completion_is_scoped_and_redacts_api_errors() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let client = Client::try_from(kube::Config::new(
        format!("http://{}", listener.local_addr().unwrap())
            .parse()
            .unwrap(),
    ))
    .unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut bytes = Vec::new();
        let mut buffer = [0; 2048];
        while !bytes.windows(4).any(|b| b == b"\r\n\r\n") {
            let count = socket.read(&mut buffer).await.unwrap();
            assert_ne!(count, 0);
            bytes.extend_from_slice(&buffer[..count]);
        }
        assert!(
            String::from_utf8(bytes)
                .unwrap()
                .starts_with("GET /apis/batch/v1/namespaces/team/jobs/selected ")
        );
        let body = json!({"apiVersion":"v1","kind":"Status","status":"Failure","code":403,"reason":"Forbidden","message":"fixture-private-token access denied"}).to_string();
        socket.write_all(format!("HTTP/1.1 403 Forbidden\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
    });
    let target = ResourceTarget {
        kind: job_kind(),
        namespace: Some("team".to_owned()),
        name: "selected".to_owned(),
    };
    let action = ActionIntent::LoadBatchNavigation {
        id: 64,
        target: target.clone(),
    };
    assert!(action.cancelable_read());
    let (sender, mut received) = mpsc::channel(1);
    spawn_action_task(
        client,
        "demo".to_owned(),
        ConnectionOptions {
            token: Some("fixture-private-token".to_owned()),
            ..ConnectionOptions::default()
        },
        ActionRequest {
            generation: 23,
            intent: action,
        },
        sender,
        true,
    )
    .await
    .unwrap();
    let ActionResult::Scoped { generation, result } = received.try_recv().unwrap() else {
        panic!("scoped result");
    };
    assert_eq!(generation, 23);
    let ActionResult::BatchNavigation {
        id,
        target: actual,
        result: Err(error),
    } = *result
    else {
        panic!("typed error");
    };
    assert_eq!(id, 64);
    assert_eq!(target, actual);
    assert!(error.contains("[REDACTED]"));
    assert!(!error.contains("fixture-private-token"));
    server.await.unwrap();
}
