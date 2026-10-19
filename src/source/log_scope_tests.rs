use serde_json::{Value, json};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

use super::*;
use crate::model::ResourceOperations;

fn target(kind: &str) -> LogTarget {
    let (group, version, plural) = match kind {
        "Pod" => ("", "v1", "pods"),
        "Service" => ("", "v1", "services"),
        "Deployment" => ("apps", "apps/v1", "deployments"),
        "StatefulSet" => ("apps", "apps/v1", "statefulsets"),
        "DaemonSet" => ("apps", "apps/v1", "daemonsets"),
        "Job" => ("batch", "batch/v1", "jobs"),
        _ => panic!("unsupported fixture kind"),
    };
    LogTarget::workload(
        ResourceTarget {
            kind: ResourceDescriptor::new(
                group,
                "v1",
                version,
                kind,
                plural,
                true,
                ResourceOperations::all(),
            ),
            namespace: Some("team".to_owned()),
            name: "selected".to_owned(),
        },
        false,
    )
}

fn spec() -> Value {
    json!({
        "initContainers":[{"name":"init","image":"fixture"}],
        "containers":[{"name":"main","image":"fixture"},{"name":"sidecar","image":"fixture"}],
        "ephemeralContainers":[{"name":"debug","image":"fixture"}]
    })
}

fn pod(name: &str, spec: Value) -> Value {
    let mut pod = json!({"apiVersion":"v1","kind":"Pod","metadata":{
        "name":name,"namespace":"team","labels":{"app":"worker"},
        "ownerReferences":[{"apiVersion":"apps/v1","kind":"Deployment","name":"unrelated","uid":"foreign-owner"}]
    }});
    pod["spec"] = spec;
    pod
}

fn workload(kind: &str) -> Value {
    let target = target(kind);
    let spec = if kind == "Service" {
        json!({"selector":{"app":"worker"}})
    } else {
        json!({
            "selector":{"matchLabels":{"app":"worker"},"matchExpressions":[{"key":"track","operator":"In","values":["canary"]}]},
            "template":{"metadata":{},"spec":spec()}
        })
    };
    json!({"apiVersion":target.resource.kind.api_version.as_ref(),"kind":kind,
        "metadata":{"name":"selected","namespace":"team"},"spec":spec})
}

fn list(pods: Vec<Value>, continuation: &str) -> Value {
    let mut list = json!({"apiVersion":"v1","kind":"PodList","metadata":{"continue":continuation}});
    list["items"] = Value::Array(pods);
    list
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

async fn read_request(socket: &mut tokio::net::TcpStream) -> String {
    let mut bytes = Vec::new();
    let mut chunk = [0; 4096];
    while !bytes.windows(4).any(|window| window == b"\r\n\r\n") {
        let count = socket.read(&mut chunk).await.unwrap();
        assert_ne!(count, 0);
        bytes.extend_from_slice(&chunk[..count]);
    }
    String::from_utf8(bytes).unwrap()
}

async fn reply(socket: &mut tokio::net::TcpStream, code: u16, value: &Value) {
    let body = value.to_string();
    socket.write_all(format!("HTTP/1.1 {code} Response\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
}

async fn fixture(replies: Vec<(u16, Value)>) -> (Client, tokio::task::JoinHandle<Vec<String>>) {
    let (client, listener) = listener().await;
    let server = tokio::spawn(async move {
        let mut requests = Vec::new();
        for (code, value) in replies {
            let (mut socket, _) = listener.accept().await.unwrap();
            requests.push(read_request(&mut socket).await);
            reply(&mut socket, code, &value).await;
        }
        requests
    });
    (client, server)
}

fn containers(resolved: &ResolvedLogs) -> Vec<&str> {
    resolved
        .sources
        .iter()
        .map(|source| source.container.as_str())
        .collect()
}

#[tokio::test]
async fn unannotated_pod_resolves_all_init_regular_and_ephemeral_containers() {
    let (client, server) = fixture(vec![(200, pod("selected", spec()))]).await;
    let resolved = resolve(client, &target("Pod"), false, None).await.unwrap();
    assert_eq!(containers(&resolved), ["init", "main", "sidecar", "debug"]);
    assert_eq!(
        resolved.metadata,
        LogSessionMeta {
            default_container: None,
            selected_container: None,
            all_containers: true,
            multi_pods: false,
            single_container: false,
            source_count: 4,
        }
    );
    assert!(
        resolved
            .sources
            .iter()
            .all(|source| source.namespace == "team"
                && source.pod == "selected"
                && !source.single_container)
    );
    assert!(server.await.unwrap()[0].starts_with("GET /api/v1/namespaces/team/pods/selected "));
}

#[tokio::test]
async fn valid_regular_default_container_can_expand_to_all_containers() {
    for all in [false, true] {
        let mut pod = pod("selected", spec());
        pod["metadata"]["annotations"] = json!({DEFAULT_CONTAINER_ANNOTATION:"sidecar"});
        let (client, server) = fixture(vec![(200, pod)]).await;
        let resolved = resolve(client, &target("Pod"), all, None).await.unwrap();
        assert_eq!(
            resolved.metadata.default_container.as_deref(),
            Some("sidecar")
        );
        assert_eq!(resolved.metadata.all_containers, all);
        assert_eq!(
            resolved.metadata.selected_container.as_deref(),
            (!all).then_some("sidecar")
        );
        assert_eq!(
            containers(&resolved),
            if all {
                vec!["init", "main", "sidecar", "debug"]
            } else {
                vec!["sidecar"]
            }
        );
        assert!(!resolved.metadata.single_container);
        server.await.unwrap();
    }
}

#[tokio::test]
async fn invalid_or_nonregular_annotation_does_not_choose_a_container() {
    for annotation in ["missing", "init", "debug", ""] {
        let mut pod = pod("selected", spec());
        pod["metadata"]["annotations"] = json!({DEFAULT_CONTAINER_ANNOTATION:annotation});
        let (client, server) = fixture(vec![(200, pod)]).await;
        let resolved = resolve(client, &target("Pod"), false, None).await.unwrap();
        assert_eq!(resolved.metadata.default_container, None);
        assert_eq!(resolved.sources.len(), 4);
        server.await.unwrap();
    }
}

#[tokio::test]
async fn one_container_pod_is_single_even_when_all_is_requested() {
    for all in [false, true] {
        let (client, server) = fixture(vec![(
            200,
            pod("selected", json!({"containers":[{"name":"only"}]})),
        )])
        .await;
        let resolved = resolve(client, &target("Pod"), all, None).await.unwrap();
        assert_eq!(containers(&resolved), ["only"]);
        assert!(resolved.metadata.single_container);
        assert!(resolved.sources[0].single_container);
        assert!(!resolved.metadata.all_containers);
        assert_eq!(
            resolved.metadata.selected_container.as_deref(),
            Some("only")
        );
        server.await.unwrap();
    }
}

#[tokio::test]
async fn explicit_container_logs_stay_single_and_preserve_previous_target() {
    for selected in ["init", "main", "debug"] {
        let mut object = pod("selected", spec());
        object["metadata"]["annotations"] = json!({DEFAULT_CONTAINER_ANNOTATION:"sidecar"});
        let (client, server) = fixture(vec![(200, object)]).await;
        let target = LogTarget::pod("team", "selected", Some(selected.to_owned()), true);
        assert!(target.previous);
        let resolved = resolve(client, &target, false, None).await.unwrap();
        assert_eq!(containers(&resolved), [selected]);
        assert!(resolved.metadata.single_container);
        assert_eq!(resolved.metadata.default_container, None);
        assert_eq!(
            resolved.metadata.selected_container.as_deref(),
            Some(selected)
        );
        server.await.unwrap();
    }
}

#[tokio::test]
async fn every_workload_route_lists_all_matching_namespace_pods_without_owner_filter() {
    for kind in ["Service", "Deployment", "StatefulSet", "DaemonSet", "Job"] {
        let (client, server) = fixture(vec![
            (200, workload(kind)),
            (200, list(vec![pod("one", spec()), pod("two", spec())], "")),
        ])
        .await;
        let resolved = resolve(client, &target(kind), false, None).await.unwrap();
        assert_eq!(resolved.sources.len(), 8, "route {kind}");
        assert!(resolved.metadata.multi_pods);
        assert!(resolved.metadata.all_containers);
        assert!(resolved.sources.iter().any(|source| source.pod == "one"));
        assert!(resolved.sources.iter().any(|source| source.pod == "two"));
        let requests = server.await.unwrap();
        assert!(requests[0].contains(&format!("/{}/selected ", target(kind).resource.kind.plural)));
        assert!(requests[1].starts_with("GET /api/v1/namespaces/team/pods?"));
        assert!(requests[1].contains("labelSelector=app%3Dworker"));
        assert!(requests[1].contains("limit=500"));
        assert!(
            !requests[1].contains("track"),
            "log selection intentionally ignores expressions"
        );
        assert!(!requests[1].contains("fieldSelector"));
    }
}

#[tokio::test]
async fn workload_defaults_use_workload_metadata_not_template_metadata() {
    for top_level in [false, true] {
        let mut workload = workload("Deployment");
        workload["spec"]["template"]["metadata"]["annotations"] =
            json!({DEFAULT_CONTAINER_ANNOTATION:"main"});
        if top_level {
            workload["metadata"]["annotations"] = json!({DEFAULT_CONTAINER_ANNOTATION:"sidecar"});
        }
        let (client, server) = fixture(vec![
            (200, workload),
            (200, list(vec![pod("one", spec())], "")),
        ])
        .await;
        let resolved = resolve(client, &target("Deployment"), false, None)
            .await
            .unwrap();
        assert_eq!(
            resolved.metadata.default_container.as_deref(),
            top_level.then_some("sidecar")
        );
        assert_eq!(resolved.sources.len(), if top_level { 1 } else { 4 });
        if top_level {
            assert_eq!(containers(&resolved), ["sidecar"]);
        }
        server.await.unwrap();
    }
}

#[tokio::test]
async fn service_prefix_policy_is_per_pod_and_ignores_pod_default_annotations() {
    let mut multiple = pod("multiple", spec());
    multiple["metadata"]["annotations"] = json!({DEFAULT_CONTAINER_ANNOTATION:"main"});
    let (client, server) = fixture(vec![
        (200, workload("Service")),
        (
            200,
            list(
                vec![
                    pod("single", json!({"containers":[{"name":"only"}]})),
                    multiple,
                ],
                "",
            ),
        ),
    ])
    .await;
    let resolved = resolve(client, &target("Service"), false, None)
        .await
        .unwrap();
    assert_eq!(resolved.sources.len(), 5);
    assert!(!resolved.metadata.single_container);
    assert!(resolved.sources[0].single_container);
    assert!(
        resolved.sources[1..]
            .iter()
            .all(|source| !source.single_container)
    );
    server.await.unwrap();
}

#[tokio::test]
async fn one_template_container_remains_selected_across_pods_with_injected_sidecars() {
    let mut workload = workload("Job");
    workload["spec"]["template"]["spec"] = json!({"containers":[{"name":"main"}]});
    let (client, server) = fixture(vec![
        (200, workload),
        (200, list(vec![pod("one", spec())], "")),
    ])
    .await;
    let resolved = resolve(client, &target("Job"), false, None).await.unwrap();
    assert_eq!(containers(&resolved), ["main"]);
    assert!(resolved.metadata.single_container);
    assert!(resolved.sources[0].single_container);
    server.await.unwrap();
}

#[tokio::test]
async fn absent_empty_and_expression_only_workload_selectors_fail_before_pod_listing() {
    for kind in ["Service", "Deployment", "StatefulSet", "DaemonSet", "Job"] {
        for selector in [
            Value::Null,
            json!({}),
            json!({"matchExpressions":[{"key":"app","operator":"Exists"}]}),
        ] {
            let mut workload = workload(kind);
            workload["spec"]["selector"] = selector;
            let (client, server) = fixture(vec![(200, workload)]).await;
            assert!(
                resolve(client, &target(kind), false, None)
                    .await
                    .unwrap_err()
                    .contains("selector")
            );
            assert_eq!(server.await.unwrap().len(), 1);
        }
    }
}

#[tokio::test]
async fn invalid_label_tokens_cannot_expand_the_log_selector() {
    for labels in [
        json!({"app":"worker,other=value"}),
        json!({"bad/key/again":"worker"}),
        json!({"app":"bad value"}),
    ] {
        let mut workload = workload("Service");
        workload["spec"]["selector"] = labels;
        let (client, server) = fixture(vec![(200, workload)]).await;
        assert!(
            resolve(client, &target("Service"), false, None)
                .await
                .unwrap_err()
                .contains("selector")
        );
        server.await.unwrap();
    }
}

#[tokio::test]
async fn pod_pages_preserve_selection_and_deduplicate_source_identities() {
    let (client, server) = fixture(vec![
        (200, workload("Service")),
        (200, list(vec![pod("one", spec())], "next-page")),
        (200, list(vec![pod("one", spec()), pod("two", spec())], "")),
    ])
    .await;
    let resolved = resolve(client, &target("Service"), false, None)
        .await
        .unwrap();
    assert_eq!(resolved.sources.len(), 8);
    let requests = server.await.unwrap();
    assert!(requests[2].contains("continue=next-page"));
    assert!(requests[2].contains("labelSelector=app%3Dworker"));
}

#[tokio::test]
async fn server_page_size_does_not_silently_limit_matching_pods() {
    let pods = (0..501)
        .map(|index| {
            pod(
                &format!("pod-{index}"),
                json!({"containers":[{"name":"main"}]}),
            )
        })
        .collect();
    let (client, server) = fixture(vec![(200, workload("Service")), (200, list(pods, ""))]).await;
    let resolved = resolve(client, &target("Service"), false, None)
        .await
        .unwrap();
    assert_eq!(resolved.sources.len(), 501);
    server.await.unwrap();
}

#[tokio::test]
async fn repeated_or_oversized_pagination_token_rejects_incomplete_scope() {
    for oversized in [false, true] {
        let token = if oversized {
            "a".repeat(MAX_TOKEN_BYTES + 1)
        } else {
            "repeat".to_owned()
        };
        let mut replies = vec![
            (200, workload("Service")),
            (200, list(vec![pod("one", spec())], &token)),
        ];
        if !oversized {
            replies.push((200, list(vec![pod("two", spec())], &token)));
        }
        let (client, server) = fixture(replies).await;
        assert!(
            resolve(client, &target("Service"), false, None)
                .await
                .unwrap_err()
                .contains("scope is incomplete")
        );
        server.await.unwrap();
    }
}

#[tokio::test]
async fn zero_matching_pods_resolves_a_completed_empty_scope() {
    let (client, server) = fixture(vec![
        (200, workload("Service")),
        (200, list(Vec::new(), "")),
    ])
    .await;
    let resolved = resolve(client, &target("Service"), false, None)
        .await
        .unwrap();
    assert_eq!(resolved.sources, Vec::<LogSource>::new());
    assert_eq!(resolved.metadata.source_count, 0);
    assert!(resolved.metadata.multi_pods);
    server.await.unwrap();
}

#[tokio::test]
async fn returned_source_and_listed_pods_must_stay_in_selected_identity() {
    for listed in [false, true] {
        for (path, value) in [
            ("/metadata/namespace", "other"),
            ("/kind", "Service"),
            ("/apiVersion", "other/v1"),
        ] {
            let mut bad = pod("selected", spec());
            *bad.pointer_mut(path).unwrap() = json!(value);
            let replies = if listed {
                vec![(200, workload("Service")), (200, list(vec![bad], ""))]
            } else {
                vec![(200, bad)]
            };
            let (client, server) = fixture(replies).await;
            assert!(
                resolve(
                    client,
                    &target(if listed { "Service" } else { "Pod" }),
                    false,
                    None
                )
                .await
                .unwrap_err()
                .contains("selected log scope")
            );
            server.await.unwrap();
        }
    }
}

#[tokio::test]
async fn workload_pod_list_members_can_omit_type_meta() {
    for kind in ["Service", "Deployment", "StatefulSet", "DaemonSet", "Job"] {
        let mut member = pod("member", spec());
        let object = member.as_object_mut().unwrap();
        object.remove("apiVersion");
        object.remove("kind");
        let (client, server) =
            fixture(vec![(200, workload(kind)), (200, list(vec![member], ""))]).await;
        let resolved = resolve(client, &target(kind), false, None).await.unwrap();
        assert_eq!(containers(&resolved), ["init", "main", "sidecar", "debug"]);
        assert!(
            resolved
                .sources
                .iter()
                .all(|source| source.namespace == "team" && source.pod == "member")
        );
        assert_eq!(resolved.metadata.selected_container, None);
        server.await.unwrap();
    }
}

#[tokio::test]
async fn listed_pods_require_the_pod_list_envelope_and_named_namespace_identity() {
    for case in 0..6 {
        let mut page = list(vec![pod("member", spec())], "");
        match case {
            0 => page["kind"] = json!("ServiceList"),
            1 => page["apiVersion"] = json!("other/v1"),
            2 => {
                page.as_object_mut().unwrap().remove("apiVersion");
                page.as_object_mut().unwrap().remove("kind");
            }
            3 => page["items"][0]["metadata"]["name"] = json!(""),
            4 => {
                page["items"][0]["metadata"]
                    .as_object_mut()
                    .unwrap()
                    .remove("name");
            }
            _ => {
                page["items"][0]["metadata"]
                    .as_object_mut()
                    .unwrap()
                    .remove("namespace");
            }
        }
        let (client, server) = fixture(vec![(200, workload("Service")), (200, page)]).await;
        assert!(
            resolve(client, &target("Service"), false, None)
                .await
                .unwrap_err()
                .contains("selected log scope"),
            "case {case}"
        );
        server.await.unwrap();
    }
}

#[tokio::test]
async fn selected_get_still_requires_type_meta_and_exact_name() {
    for omit_type in [false, true] {
        let mut selected = pod("selected", spec());
        if omit_type {
            let object = selected.as_object_mut().unwrap();
            object.remove("apiVersion");
            object.remove("kind");
        } else {
            selected["metadata"]["name"] = json!("other");
        }
        let (client, server) = fixture(vec![(200, selected)]).await;
        assert!(
            resolve(client, &target("Pod"), false, None)
                .await
                .unwrap_err()
                .contains("selected log scope")
        );
        server.await.unwrap();
    }
}

#[tokio::test]
async fn api_failure_in_parent_or_later_pod_page_rejects_scope_atomically() {
    let error = json!({"apiVersion":"v1","kind":"Status","status":"Failure","code":403,"reason":"Forbidden","message":"fixture access denied"});
    for listing in [false, true] {
        let replies = if listing {
            vec![
                (200, workload("Service")),
                (200, list(vec![pod("one", spec())], "next")),
                (403, error.clone()),
            ]
        } else {
            vec![(403, error.clone())]
        };
        let (client, server) = fixture(replies).await;
        assert!(
            resolve(client, &target("Service"), false, None)
                .await
                .unwrap_err()
                .contains("fixture access denied")
        );
        server.await.unwrap();
    }
}

#[tokio::test]
async fn invalid_scope_identity_and_timeout_fail_without_any_request() {
    let (client, listener) = listener().await;
    for case in 0..8 {
        let mut target = target("Pod");
        match case {
            0 => target.resource.namespace = None,
            1 => target.resource.name.clear(),
            2 => target.container = Some(" ".to_owned()),
            3 => target.resource.kind.operations = ResourceOperations::new(false, false, false),
            4 => target.resource.kind.api_version = "other/v1".into(),
            5 => target.resource.kind.namespaced = false,
            6 => target.resource.kind = ResourceKind::Nodes.into(),
            _ => {
                target.resource.kind = ResourceKind::Services.into();
                target.container = Some("main".to_owned());
            }
        }
        assert!(resolve(client.clone(), &target, false, None).await.is_err());
    }
    for timeout in [Duration::ZERO, Duration::MAX] {
        assert!(
            resolve(client.clone(), &target("Pod"), false, Some(timeout))
                .await
                .unwrap_err()
                .contains("timeout")
        );
    }
    assert!(
        tokio::time::timeout(Duration::from_millis(20), listener.accept())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn configured_deadline_bounds_parent_and_pod_list_requests() {
    for listing in [false, true] {
        let (client, listener) = listener().await;
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            if listing {
                let (mut socket, _) = listener.accept().await.unwrap();
                read_request(&mut socket).await;
                reply(&mut socket, 200, &workload("Service")).await;
            }
            let (mut socket, _) = listener.accept().await.unwrap();
            sender.send(read_request(&mut socket).await).unwrap();
            std::future::pending::<()>().await;
        });
        assert!(
            resolve(
                client,
                &target("Service"),
                false,
                Some(Duration::from_millis(50))
            )
            .await
            .unwrap_err()
            .contains("Timed out")
        );
        let request = receiver.await.unwrap();
        assert!(request.contains(if listing {
            "/pods?"
        } else {
            "/services/selected "
        }));
        server.abort();
        assert!(server.await.unwrap_err().is_cancelled());
    }
}

#[tokio::test]
async fn canceling_scope_resolution_closes_an_unbounded_pod_list_request() {
    let (client, listener) = listener().await;
    let (sender, receiver) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        {
            let (mut socket, _) = listener.accept().await.unwrap();
            read_request(&mut socket).await;
            reply(&mut socket, 200, &workload("Service")).await;
        }
        let (mut socket, _) = listener.accept().await.unwrap();
        sender.send(read_request(&mut socket).await).unwrap();
        let mut byte = [0];
        socket.read(&mut byte).await
    });
    let resolver =
        tokio::spawn(async move { resolve(client, &target("Service"), false, None).await });
    assert!(receiver.await.unwrap().contains("/pods?"));
    resolver.abort();
    assert!(resolver.await.unwrap_err().is_cancelled());
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
fn exact_supported_log_resources_exclude_cronjobs_replicasets_and_custom_lookalikes() {
    for kind in [
        "Pod",
        "Service",
        "Deployment",
        "StatefulSet",
        "DaemonSet",
        "Job",
    ] {
        let mut resource = target(kind).resource.kind;
        assert!(supports_logs(&resource));
        resource.version = "v2".into();
        assert!(!supports_logs(&resource));
    }
    for (kind, plural) in [("CronJob", "cronjobs"), ("ReplicaSet", "replicasets")] {
        let resource = ResourceDescriptor::new(
            "batch",
            "v1",
            "batch/v1",
            kind,
            plural,
            true,
            ResourceOperations::all(),
        );
        assert!(!supports_logs(&resource));
    }
    assert_eq!(
        LogLine::from("local marker"),
        LogLine {
            source: None,
            text: "local marker".to_owned()
        }
    );
}
