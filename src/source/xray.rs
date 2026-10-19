//! `XRay` dependency tree builder and row projection (`:xray <resource> [namespace]`).

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    time::Duration,
};

use k8s_openapi::api::{
    apps::v1::{DaemonSet, Deployment, ReplicaSet, StatefulSet},
    core::v1::{ConfigMap, PersistentVolumeClaim, Pod, Secret, Service, ServiceAccount},
};
use kube::{Api, Client, Resource as _, ResourceExt as _, api::ListParams};

use crate::model::{
    ResourceDescriptor, ResourceKind, ResourceOperations, ResourceRow, TableCell, TableColumn,
    XRayData,
};

use super::WatchRequest;
use super::resource_views::{ViewRows, read_api};

#[must_use]
pub(crate) fn xray_columns() -> Arc<[TableColumn]> {
    ["NAME", "STATUS", "INFO"]
        .into_iter()
        .map(|name| TableColumn {
            name: name.into(),
            format: "".into(),
            priority: 0,
        })
        .collect::<Vec<_>>()
        .into()
}

#[must_use]
pub(crate) fn resolve_xray_target_kind(gvr_or_alias: &str) -> Option<ResourceDescriptor> {
    match gvr_or_alias.to_ascii_lowercase().as_str() {
        "v1/pods" | "pods" | "pod" | "po" => Some(ResourceDescriptor::from(ResourceKind::Pods)),
        "v1/services" | "services" | "service" | "svc" => {
            Some(ResourceDescriptor::from(ResourceKind::Services))
        }
        "apps/v1/deployments" | "deployments" | "deployment" | "deploy" | "dp" => {
            Some(ResourceDescriptor::from(ResourceKind::Deployments))
        }
        "apps/v1/statefulsets" | "statefulsets" | "statefulset" | "sts" => {
            Some(ResourceDescriptor::from(ResourceKind::StatefulSets))
        }
        "apps/v1/daemonsets" | "daemonsets" | "daemonset" | "ds" => {
            Some(ResourceDescriptor::from(ResourceKind::DaemonSets))
        }
        "apps/v1/replicasets" | "replicasets" | "replicaset" | "rs" => {
            Some(ResourceDescriptor::new(
                "apps",
                "v1",
                "apps/v1",
                "ReplicaSet",
                "replicasets",
                true,
                ResourceOperations::all(),
            ))
        }
        _ => None,
    }
}

fn desc_ns() -> ResourceDescriptor {
    ResourceDescriptor::new(
        "",
        "v1",
        "v1",
        "Namespace",
        "namespaces",
        false,
        ResourceOperations::all(),
    )
}

fn desc_cm() -> ResourceDescriptor {
    ResourceDescriptor::new(
        "",
        "v1",
        "v1",
        "ConfigMap",
        "configmaps",
        true,
        ResourceOperations::all(),
    )
}

fn desc_sec() -> ResourceDescriptor {
    ResourceDescriptor::new(
        "",
        "v1",
        "v1",
        "Secret",
        "secrets",
        true,
        ResourceOperations::all(),
    )
}

fn desc_pvc() -> ResourceDescriptor {
    ResourceDescriptor::new(
        "",
        "v1",
        "v1",
        "PersistentVolumeClaim",
        "persistentvolumeclaims",
        true,
        ResourceOperations::all(),
    )
}

fn desc_sa() -> ResourceDescriptor {
    ResourceDescriptor::new(
        "",
        "v1",
        "v1",
        "ServiceAccount",
        "serviceaccounts",
        true,
        ResourceOperations::all(),
    )
}

fn desc_container() -> ResourceDescriptor {
    ResourceDescriptor::new(
        "",
        "v1",
        "v1",
        "Container",
        "containers",
        true,
        ResourceOperations::new(false, false, false),
    )
}

#[derive(Debug, Clone)]
pub(crate) struct XRayNode {
    pub id: String,
    pub display_name: String,
    pub namespace: String,
    pub target_kind: ResourceDescriptor,
    pub is_container: bool,
    pub is_namespace: bool,
    pub pod_name: Option<String>,
    pub emoji: &'static str,
    pub info: String,
    pub status: &'static str,
    pub children: Vec<Self>,
}

impl XRayNode {
    pub fn sort_children(&mut self) {
        self.children.sort_by(|a, b| a.id.cmp(&b.id));
        for child in &mut self.children {
            child.sort_children();
        }
    }
}

#[must_use]
pub(crate) fn flatten_xray_tree(nodes: &[XRayNode]) -> Vec<Arc<ResourceRow>> {
    let mut out = Vec::new();
    for node in nodes {
        flatten_node(node, None, 0, &mut out);
    }
    out
}

fn flatten_node(
    node: &XRayNode,
    parent_path: Option<&str>,
    depth: usize,
    out: &mut Vec<Arc<ResourceRow>>,
) {
    let path = match parent_path {
        Some(parent) => format!("{parent}::{}", node.id),
        None => node.id.clone(),
    };
    let children_count = node.children.len();
    let cells = [node.display_name.as_str(), node.status, node.info.as_str()]
        .into_iter()
        .map(|text| TableCell {
            text: text.into(),
            timestamp_unix: None,
        })
        .collect();
    let row = ResourceRow::new(
        node.namespace.clone(),
        node.display_name.clone(),
        node.status,
        node.info.clone(),
        path.clone(),
        0,
    )
    .with_table_cells(cells)
    .with_xray_data(XRayData {
        path: Arc::from(path.as_str()),
        parent_path: parent_path.map(Arc::from),
        depth,
        target_kind: node.target_kind.clone(),
        is_container: node.is_container,
        is_namespace: node.is_namespace,
        pod_name: node.pod_name.as_deref().map(Arc::from),
        children_count,
        emoji: Arc::from(node.emoji),
        info: Arc::from(node.info.as_str()),
        status: Arc::from(node.status),
    });
    out.push(Arc::new(row));
    for child in &node.children {
        flatten_node(child, Some(&path), depth + 1, out);
    }
}

struct ClusterState {
    pods: Vec<Pod>,
    cms: BTreeSet<(String, String)>,
    secs: BTreeSet<(String, String)>,
    pvcs: BTreeSet<(String, String)>,
    sas: BTreeMap<(String, String), ServiceAccount>,
}

impl ClusterState {
    async fn load(
        client: &Client,
        namespace: Option<&str>,
        timeout: Option<Duration>,
    ) -> Result<Self, String> {
        let lp = ListParams::default();
        let pod_api = match namespace {
            Some(ns) => Api::<Pod>::namespaced(client.clone(), ns),
            None => Api::<Pod>::all(client.clone()),
        };
        let cm_api = match namespace {
            Some(ns) => Api::<ConfigMap>::namespaced(client.clone(), ns),
            None => Api::<ConfigMap>::all(client.clone()),
        };
        let sec_api = match namespace {
            Some(ns) => Api::<Secret>::namespaced(client.clone(), ns),
            None => Api::<Secret>::all(client.clone()),
        };
        let pvc_api = match namespace {
            Some(ns) => Api::<PersistentVolumeClaim>::namespaced(client.clone(), ns),
            None => Api::<PersistentVolumeClaim>::all(client.clone()),
        };
        let sa_api = match namespace {
            Some(ns) => Api::<ServiceAccount>::namespaced(client.clone(), ns),
            None => Api::<ServiceAccount>::all(client.clone()),
        };
        let pods = read_api(pod_api.list(&lp), timeout, "list pods for xray")
            .await?
            .items;
        let cms = read_api(cm_api.list(&lp), timeout, "list configmaps for xray")
            .await
            .map(|l| {
                l.items
                    .into_iter()
                    .map(|i| (i.namespace().unwrap_or_default(), i.name_any()))
                    .collect()
            })
            .unwrap_or_default();
        let secs = read_api(sec_api.list(&lp), timeout, "list secrets for xray")
            .await
            .map(|l| {
                l.items
                    .into_iter()
                    .map(|i| (i.namespace().unwrap_or_default(), i.name_any()))
                    .collect()
            })
            .unwrap_or_default();
        let pvcs = read_api(pvc_api.list(&lp), timeout, "list pvcs for xray")
            .await
            .map(|l| {
                l.items
                    .into_iter()
                    .map(|i| (i.namespace().unwrap_or_default(), i.name_any()))
                    .collect()
            })
            .unwrap_or_default();
        let sas = read_api(sa_api.list(&lp), timeout, "list serviceaccounts for xray")
            .await
            .map(|l| {
                l.items
                    .into_iter()
                    .map(|i| ((i.namespace().unwrap_or_default(), i.name_any()), i))
                    .collect()
            })
            .unwrap_or_default();
        Ok(Self {
            pods,
            cms,
            secs,
            pvcs,
            sas,
        })
    }

    fn build_pod_node(&self, pod: &Pod) -> XRayNode {
        let ns = pod.namespace().unwrap_or_else(|| "default".to_owned());
        let name = pod.name_any();
        let mut children = Vec::new();
        if let Some(spec) = &pod.spec {
            for co in spec
                .init_containers
                .iter()
                .flatten()
                .chain(spec.containers.iter())
            {
                children.push(self.build_container_node(&ns, &name, co));
            }
            if let Some(volumes) = &spec.volumes {
                for vol in volumes {
                    if let Some(sec) = &vol.secret
                        && let Some(sec_name) = &sec.secret_name
                    {
                        self.add_ref_node(
                            &mut children,
                            desc_sec(),
                            "🔒",
                            &ns,
                            sec_name,
                            self.secs.contains(&(ns.clone(), sec_name.clone())),
                            sec.optional,
                        );
                    }
                    if let Some(cm) = &vol.config_map {
                        self.add_ref_node(
                            &mut children,
                            desc_cm(),
                            "🗺 ",
                            &ns,
                            &cm.name,
                            self.cms.contains(&(ns.clone(), cm.name.clone())),
                            cm.optional,
                        );
                    }
                    if let Some(pvc) = &vol.persistent_volume_claim {
                        self.add_ref_node(
                            &mut children,
                            desc_pvc(),
                            "🎟 ",
                            &ns,
                            &pvc.claim_name,
                            self.pvcs.contains(&(ns.clone(), pvc.claim_name.clone())),
                            None,
                        );
                    }
                }
            }
            let sa_name = spec
                .service_account_name
                .as_deref()
                .filter(|s| !s.is_empty());
            if let Some(sa_name) = sa_name {
                children.push(self.build_sa_node(
                    &ns,
                    sa_name,
                    spec.automount_service_account_token,
                ));
            }
        }
        let statuses = pod
            .status
            .as_ref()
            .and_then(|s| s.container_statuses.as_deref())
            .unwrap_or_default();
        let ready_cnt = statuses.iter().filter(|s| s.ready).count();
        let total_cnt = statuses
            .len()
            .max(pod.spec.as_ref().map_or(0, |s| s.containers.len()));
        let phase = pod
            .status
            .as_ref()
            .and_then(|s| s.phase.as_deref())
            .unwrap_or("Pending");
        let status = if phase == "Succeeded" || phase == "Completed" {
            "completed"
        } else if ready_cnt == total_cnt && phase == "Running" {
            "ok"
        } else {
            "toast"
        };
        XRayNode {
            id: format!("v1/pods/{ns}/{name}"),
            display_name: name,
            namespace: ns,
            target_kind: ResourceDescriptor::from(ResourceKind::Pods),
            is_container: false,
            is_namespace: false,
            pod_name: None,
            emoji: "🚛",
            info: format!("{ready_cnt}/{total_cnt}"),
            status,
            children,
        }
    }

    fn build_container_node(
        &self,
        ns: &str,
        pod_name: &str,
        co: &k8s_openapi::api::core::v1::Container,
    ) -> XRayNode {
        let mut children = Vec::new();
        if let Some(envs) = &co.env {
            for e in envs {
                if let Some(vf) = &e.value_from {
                    if let Some(sec) = &vf.secret_key_ref {
                        self.add_ref_node(
                            &mut children,
                            desc_sec(),
                            "🔒",
                            ns,
                            &sec.name,
                            self.secs.contains(&(ns.to_owned(), sec.name.clone())),
                            sec.optional,
                        );
                    }
                    if let Some(cm) = &vf.config_map_key_ref {
                        self.add_ref_node(
                            &mut children,
                            desc_cm(),
                            "🗺 ",
                            ns,
                            &cm.name,
                            self.cms.contains(&(ns.to_owned(), cm.name.clone())),
                            cm.optional,
                        );
                    }
                }
            }
        }
        if let Some(env_froms) = &co.env_from {
            for ef in env_froms {
                if let Some(cm) = &ef.config_map_ref {
                    self.add_ref_node(
                        &mut children,
                        desc_cm(),
                        "🗺 ",
                        ns,
                        &cm.name,
                        self.cms.contains(&(ns.to_owned(), cm.name.clone())),
                        cm.optional,
                    );
                }
                if let Some(sec) = &ef.secret_ref {
                    self.add_ref_node(
                        &mut children,
                        desc_sec(),
                        "🔒",
                        ns,
                        &sec.name,
                        self.secs.contains(&(ns.to_owned(), sec.name.clone())),
                        sec.optional,
                    );
                }
            }
        }
        XRayNode {
            id: format!("containers/{ns}/{pod_name}/{}", co.name),
            display_name: co.name.clone(),
            namespace: ns.to_owned(),
            target_kind: desc_container(),
            is_container: true,
            is_namespace: false,
            pod_name: Some(pod_name.to_owned()),
            emoji: "🐳",
            info: String::new(),
            status: "ok",
            children,
        }
    }

    fn build_sa_node(&self, ns: &str, sa_name: &str, pod_automount: Option<bool>) -> XRayNode {
        let key = (ns.to_owned(), sa_name.to_owned());
        let Some(sa) = self.sas.get(&key) else {
            return XRayNode {
                id: format!("v1/serviceaccounts/{ns}/{sa_name}"),
                display_name: sa_name.to_owned(),
                namespace: ns.to_owned(),
                target_kind: desc_sa(),
                is_container: false,
                is_namespace: false,
                pod_name: None,
                emoji: "💳",
                info: String::new(),
                status: "noref",
                children: Vec::new(),
            };
        };
        let mut children = Vec::new();
        for sec in sa.secrets.iter().flatten() {
            if let Some(sec_name) = &sec.name {
                self.add_ref_node(
                    &mut children,
                    desc_sec(),
                    "🔒",
                    ns,
                    sec_name,
                    self.secs.contains(&(ns.to_owned(), sec_name.clone())),
                    None,
                );
            }
        }
        for sec in sa.image_pull_secrets.iter().flatten() {
            self.add_ref_node(
                &mut children,
                desc_sec(),
                "🔒",
                ns,
                &sec.name,
                self.secs.contains(&(ns.to_owned(), sec.name.clone())),
                None,
            );
        }
        let auto = pod_automount.or(sa.automount_service_account_token);
        let info = auto.map_or_else(String::new, |b| format!("automount={b}"));
        XRayNode {
            id: format!("v1/serviceaccounts/{ns}/{sa_name}"),
            display_name: sa_name.to_owned(),
            namespace: ns.to_owned(),
            target_kind: desc_sa(),
            is_container: false,
            is_namespace: false,
            pod_name: None,
            emoji: "💳",
            info,
            status: "ok",
            children,
        }
    }

    #[allow(clippy::unused_self, clippy::too_many_arguments)]
    fn add_ref_node(
        &self,
        children: &mut Vec<XRayNode>,
        kind: ResourceDescriptor,
        emoji: &'static str,
        ns: &str,
        name: &str,
        exists: bool,
        optional: Option<bool>,
    ) {
        let id = format!("{}/{ns}/{name}", kind.gvr());
        if children.iter().any(|c| c.id == id) {
            return;
        }
        let status = if exists || optional == Some(true) {
            "ok"
        } else {
            "noref"
        };
        children.push(XRayNode {
            id,
            display_name: name.to_owned(),
            namespace: ns.to_owned(),
            target_kind: kind,
            is_container: false,
            is_namespace: false,
            pod_name: None,
            emoji,
            info: String::new(),
            status,
            children: Vec::new(),
        });
    }

    fn pods_matching_labels(
        &self,
        ns: &str,
        selector: Option<&BTreeMap<String, String>>,
    ) -> Vec<XRayNode> {
        let Some(sel) = selector.filter(|m| !m.is_empty()) else {
            return Vec::new();
        };
        self.pods
            .iter()
            .filter(|pod| pod.namespace().as_deref() == Some(ns))
            .filter(|pod| {
                let labels = pod.meta().labels.as_ref();
                sel.iter()
                    .all(|(k, v)| labels.and_then(|l| l.get(k)) == Some(v))
            })
            .map(|pod| self.build_pod_node(pod))
            .collect()
    }
}

fn group_under_namespaces(nodes: Vec<XRayNode>) -> Vec<XRayNode> {
    let mut by_ns: BTreeMap<String, Vec<XRayNode>> = BTreeMap::new();
    for node in nodes {
        by_ns.entry(node.namespace.clone()).or_default().push(node);
    }
    let mut ns_nodes = Vec::with_capacity(by_ns.len());
    for (ns, children) in by_ns {
        let mut ns_node = XRayNode {
            id: format!("v1/namespaces/-/{ns}"),
            display_name: ns.clone(),
            namespace: "-".to_owned(),
            target_kind: desc_ns(),
            is_container: false,
            is_namespace: true,
            pod_name: None,
            emoji: "🗂 ",
            info: String::new(),
            status: "ok",
            children,
        };
        ns_node.sort_children();
        ns_nodes.push(ns_node);
    }
    ns_nodes
}

#[allow(clippy::too_many_lines)]
pub(super) async fn load_xray(
    client: &Client,
    request: &WatchRequest,
    timeout: Option<Duration>,
) -> Result<ViewRows, String> {
    let target_gvr = request.field_selector.as_deref().unwrap_or("v1/pods");
    let target_kind = resolve_xray_target_kind(target_gvr)
        .ok_or_else(|| format!("Unsupported XRay resource {target_gvr:?}"))?;
    let ns = request.namespace.as_deref();
    let state = ClusterState::load(client, ns, timeout).await?;
    let lp = ListParams::default();

    let mut top_nodes = Vec::new();
    match (target_kind.group.as_ref(), target_kind.kind.as_ref()) {
        ("", "Pod") => {
            for pod in &state.pods {
                top_nodes.push(state.build_pod_node(pod));
            }
        }
        ("", "Service") => {
            let api = match ns {
                Some(ns) => Api::<Service>::namespaced(client.clone(), ns),
                None => Api::<Service>::all(client.clone()),
            };
            let list = read_api(api.list(&lp), timeout, "list services for xray").await?;
            for svc in list.items {
                let svc_ns = svc.namespace().unwrap_or_else(|| "default".to_owned());
                let name = svc.name_any();
                let sel = svc.spec.as_ref().and_then(|s| s.selector.as_ref());
                let children = state.pods_matching_labels(&svc_ns, sel);
                if children.is_empty() {
                    continue;
                }
                top_nodes.push(XRayNode {
                    id: format!("v1/services/{svc_ns}/{name}"),
                    display_name: name,
                    namespace: svc_ns,
                    target_kind: target_kind.clone(),
                    is_container: false,
                    is_namespace: false,
                    pod_name: None,
                    emoji: "💁‍♀️",
                    info: String::new(),
                    status: "ok",
                    children,
                });
            }
        }
        ("apps", "Deployment") => {
            let api = match ns {
                Some(ns) => Api::<Deployment>::namespaced(client.clone(), ns),
                None => Api::<Deployment>::all(client.clone()),
            };
            let list = read_api(api.list(&lp), timeout, "list deployments for xray").await?;
            for dp in list.items {
                let dp_ns = dp.namespace().unwrap_or_else(|| "default".to_owned());
                let name = dp.name_any();
                let sel = dp
                    .spec
                    .as_ref()
                    .and_then(|s| s.selector.match_labels.as_ref());
                let children = state.pods_matching_labels(&dp_ns, sel);
                if children.is_empty() {
                    continue;
                }
                let r = dp.spec.as_ref().and_then(|s| s.replicas).unwrap_or(1);
                let a = dp
                    .status
                    .as_ref()
                    .and_then(|s| s.available_replicas)
                    .unwrap_or(0);
                let u = dp
                    .status
                    .as_ref()
                    .and_then(|s| s.unavailable_replicas)
                    .unwrap_or(0);
                let status = if a == r && u == 0 { "ok" } else { "toast" };
                top_nodes.push(XRayNode {
                    id: format!("apps/v1/deployments/{dp_ns}/{name}"),
                    display_name: name,
                    namespace: dp_ns,
                    target_kind: target_kind.clone(),
                    is_container: false,
                    is_namespace: false,
                    pod_name: None,
                    emoji: "🪂",
                    info: format!("{a}/{r}/{u}"),
                    status,
                    children,
                });
            }
        }
        ("apps", "StatefulSet") => {
            let api = match ns {
                Some(ns) => Api::<StatefulSet>::namespaced(client.clone(), ns),
                None => Api::<StatefulSet>::all(client.clone()),
            };
            let list = read_api(api.list(&lp), timeout, "list statefulsets for xray").await?;
            for sts in list.items {
                let sts_ns = sts.namespace().unwrap_or_else(|| "default".to_owned());
                let name = sts.name_any();
                let sel = sts
                    .spec
                    .as_ref()
                    .and_then(|s| s.selector.match_labels.as_ref());
                let children = state.pods_matching_labels(&sts_ns, sel);
                if children.is_empty() {
                    continue;
                }
                let r = sts.spec.as_ref().and_then(|s| s.replicas).unwrap_or(1);
                let a = sts
                    .status
                    .as_ref()
                    .and_then(|s| s.current_replicas)
                    .unwrap_or(0);
                let status = if a == r { "ok" } else { "toast" };
                top_nodes.push(XRayNode {
                    id: format!("apps/v1/statefulsets/{sts_ns}/{name}"),
                    display_name: name,
                    namespace: sts_ns,
                    target_kind: target_kind.clone(),
                    is_container: false,
                    is_namespace: false,
                    pod_name: None,
                    emoji: "🎎",
                    info: format!("{a}/{r}"),
                    status,
                    children,
                });
            }
        }
        ("apps", "DaemonSet") => {
            let api = match ns {
                Some(ns) => Api::<DaemonSet>::namespaced(client.clone(), ns),
                None => Api::<DaemonSet>::all(client.clone()),
            };
            let list = read_api(api.list(&lp), timeout, "list daemonsets for xray").await?;
            for ds in list.items {
                let ds_ns = ds.namespace().unwrap_or_else(|| "default".to_owned());
                let name = ds.name_any();
                let sel = ds
                    .spec
                    .as_ref()
                    .and_then(|s| s.selector.match_labels.as_ref());
                let children = state.pods_matching_labels(&ds_ns, sel);
                if children.is_empty() {
                    continue;
                }
                let d = ds.status.as_ref().map_or(0, |s| s.desired_number_scheduled);
                let a = ds
                    .status
                    .as_ref()
                    .and_then(|s| s.number_available)
                    .unwrap_or(0);
                let status = if a == d { "ok" } else { "toast" };
                top_nodes.push(XRayNode {
                    id: format!("apps/v1/daemonsets/{ds_ns}/{name}"),
                    display_name: name,
                    namespace: ds_ns,
                    target_kind: target_kind.clone(),
                    is_container: false,
                    is_namespace: false,
                    pod_name: None,
                    emoji: "😈",
                    info: format!("{a}/{d}"),
                    status,
                    children,
                });
            }
        }
        ("apps", "ReplicaSet") => {
            let api = match ns {
                Some(ns) => Api::<ReplicaSet>::namespaced(client.clone(), ns),
                None => Api::<ReplicaSet>::all(client.clone()),
            };
            let list = read_api(api.list(&lp), timeout, "list replicasets for xray").await?;
            for rs in list.items {
                let rs_ns = rs.namespace().unwrap_or_else(|| "default".to_owned());
                let name = rs.name_any();
                let sel = rs
                    .spec
                    .as_ref()
                    .and_then(|s| s.selector.match_labels.as_ref());
                let children = state.pods_matching_labels(&rs_ns, sel);
                if children.is_empty() {
                    continue;
                }
                let r = rs.spec.as_ref().and_then(|s| s.replicas).unwrap_or(1);
                let a = rs.status.as_ref().map_or(0, |s| s.replicas);
                let status = if a == r { "ok" } else { "toast" };
                top_nodes.push(XRayNode {
                    id: format!("apps/v1/replicasets/{rs_ns}/{name}"),
                    display_name: name,
                    namespace: rs_ns,
                    target_kind: target_kind.clone(),
                    is_container: false,
                    is_namespace: false,
                    pod_name: None,
                    emoji: "👯‍♂️",
                    info: format!("{a}/{r}"),
                    status,
                    children,
                });
            }
        }
        _ => {}
    }

    let ns_nodes = group_under_namespaces(top_nodes);
    Ok(ViewRows {
        columns: xray_columns(),
        rows: flatten_xray_tree(&ns_nodes),
    })
}

#[must_use]
#[allow(clippy::too_many_lines)]
pub(crate) fn build_demo_xray_rows(request: &WatchRequest) -> Vec<Arc<ResourceRow>> {
    let target_gvr = request.field_selector.as_deref().unwrap_or("v1/pods");
    let Some(target_kind) = resolve_xray_target_kind(target_gvr) else {
        return Vec::new();
    };
    let namespaces = if let Some(ns) = request.namespace.as_deref() {
        vec![ns.to_owned()]
    } else {
        vec!["team-000".to_owned(), "team-001".to_owned()]
    };
    let mut top_nodes = Vec::new();
    for (idx, ns) in namespaces.into_iter().enumerate() {
        let pod_name = format!("pod-{idx:07}");
        let co_node = XRayNode {
            id: format!("containers/{ns}/{pod_name}/app"),
            display_name: "app".to_owned(),
            namespace: ns.clone(),
            target_kind: desc_container(),
            is_container: true,
            is_namespace: false,
            pod_name: Some(pod_name.clone()),
            emoji: "🐳",
            info: String::new(),
            status: "ok",
            children: vec![
                XRayNode {
                    id: format!("v1/configmaps/{ns}/app-config"),
                    display_name: "app-config".to_owned(),
                    namespace: ns.clone(),
                    target_kind: desc_cm(),
                    is_container: false,
                    is_namespace: false,
                    pod_name: None,
                    emoji: "🗺 ",
                    info: String::new(),
                    status: "ok",
                    children: Vec::new(),
                },
                XRayNode {
                    id: format!("v1/secrets/{ns}/app-secret"),
                    display_name: "app-secret".to_owned(),
                    namespace: ns.clone(),
                    target_kind: desc_sec(),
                    is_container: false,
                    is_namespace: false,
                    pod_name: None,
                    emoji: "🔒",
                    info: String::new(),
                    status: "ok",
                    children: Vec::new(),
                },
            ],
        };
        let sa_node = XRayNode {
            id: format!("v1/serviceaccounts/{ns}/default"),
            display_name: "default".to_owned(),
            namespace: ns.clone(),
            target_kind: desc_sa(),
            is_container: false,
            is_namespace: false,
            pod_name: None,
            emoji: "💳",
            info: "automount=true".to_owned(),
            status: "ok",
            children: Vec::new(),
        };
        let pod_node = XRayNode {
            id: format!("v1/pods/{ns}/{pod_name}"),
            display_name: pod_name,
            namespace: ns.clone(),
            target_kind: ResourceDescriptor::from(ResourceKind::Pods),
            is_container: false,
            is_namespace: false,
            pod_name: None,
            emoji: "🚛",
            info: "1/1".to_owned(),
            status: "ok",
            children: vec![co_node, sa_node],
        };
        if target_kind.kind.as_ref() == "Pod" {
            top_nodes.push(pod_node);
        } else {
            let (emoji, info) = match target_kind.kind.as_ref() {
                "Service" => ("💁‍♀️", ""),
                "Deployment" => ("🪂", "1/1/0"),
                "StatefulSet" => ("🎎", "1/1"),
                "DaemonSet" => ("😈", "1/1"),
                _ => ("👯‍♂️", "1/1"),
            };
            let wl_name = format!("{}-{idx:07}", target_kind.short_label());
            top_nodes.push(XRayNode {
                id: format!("{}/{ns}/{wl_name}", target_kind.gvr()),
                display_name: wl_name,
                namespace: ns,
                target_kind: target_kind.clone(),
                is_container: false,
                is_namespace: false,
                pod_name: None,
                emoji,
                info: info.to_owned(),
                status: "ok",
                children: vec![pod_node],
            });
        }
    }
    let ns_nodes = group_under_namespaces(top_nodes);
    flatten_xray_tree(&ns_nodes)
}
