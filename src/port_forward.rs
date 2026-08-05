use std::{collections::BTreeMap, ffi::OsString, process::Stdio};

use tokio::{
    io::{AsyncBufReadExt, AsyncRead, BufReader},
    process::Command,
    sync::{mpsc, oneshot},
};

use crate::{
    app::{PortForwardInfo, PortForwardRequest, PortForwardState, ResourceTarget},
    model::ResourceKind,
};

const MAX_ACTIVE_FORWARDS: usize = 32;

#[derive(Debug)]
pub enum PortForwardEvent {
    Output { id: u64, line: String },
    Exited { id: u64, success: bool },
}

#[derive(Debug)]
struct Session {
    info: PortForwardInfo,
    stop: oneshot::Sender<()>,
    last_output: Option<String>,
}

#[derive(Debug)]
pub struct PortForwardManager {
    program: OsString,
    next_id: u64,
    sessions: BTreeMap<u64, Session>,
    events: mpsc::Sender<PortForwardEvent>,
}

impl PortForwardManager {
    #[must_use]
    pub fn new(events: mpsc::Sender<PortForwardEvent>) -> Self {
        Self::with_program("kubectl", events)
    }

    fn with_program(program: impl Into<OsString>, events: mpsc::Sender<PortForwardEvent>) -> Self {
        Self {
            program: program.into(),
            next_id: 1,
            sessions: BTreeMap::new(),
            events,
        }
    }

    /// Starts a bounded `kubectl port-forward` subprocess.
    ///
    /// # Errors
    ///
    /// Returns an error when the target is unsupported, the local binding is
    /// already owned by Kux, the session limit is reached, or `kubectl` cannot
    /// be spawned.
    pub fn start(
        &mut self,
        context: &str,
        request: PortForwardRequest,
    ) -> Result<PortForwardInfo, String> {
        if self.sessions.len() >= MAX_ACTIVE_FORWARDS {
            return Err(format!(
                "At most {MAX_ACTIVE_FORWARDS} port forwards can run at once"
            ));
        }
        if self.sessions.values().any(|session| {
            session.info.address == request.address && session.info.local_port == request.local_port
        }) {
            return Err(format!(
                "{}:{} is already used by another Kux port forward",
                request.address, request.local_port
            ));
        }

        let args = command_args(context, &request)?;
        let mut command = Command::new(&self.program);
        command
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = command
            .spawn()
            .map_err(|error| format!("start kubectl port-forward: {error}"))?;

        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        let info = PortForwardInfo {
            id,
            context: context.to_owned(),
            target: request.target,
            remote_port: request.remote_port,
            local_port: request.local_port,
            address: request.address,
            state: PortForwardState::Starting,
        };
        let (stop_tx, mut stop_rx) = oneshot::channel();
        let events = self.events.clone();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        tokio::spawn(async move {
            let stdout_task =
                stdout.map(|stdout| tokio::spawn(relay_output(stdout, id, events.clone())));
            let stderr_task =
                stderr.map(|stderr| tokio::spawn(relay_output(stderr, id, events.clone())));
            let status = tokio::select! {
                status = child.wait() => status,
                _ = &mut stop_rx => {
                    let _ = child.start_kill();
                    child.wait().await
                }
            };
            if let Some(task) = stdout_task {
                let _ = task.await;
            }
            if let Some(task) = stderr_task {
                let _ = task.await;
            }
            let success = status.is_ok_and(|status| status.success());
            let _ = events.send(PortForwardEvent::Exited { id, success }).await;
        });

        self.sessions.insert(
            id,
            Session {
                info: info.clone(),
                stop: stop_tx,
                last_output: None,
            },
        );
        Ok(info)
    }

    pub fn stop(&mut self, id: u64) -> Option<PortForwardInfo> {
        let session = self.sessions.remove(&id)?;
        let _ = session.stop.send(());
        Some(session.info)
    }

    #[must_use]
    pub fn summaries(&self, context: &str) -> Vec<PortForwardInfo> {
        self.sessions
            .values()
            .filter(|session| session.info.context == context)
            .map(|session| session.info.clone())
            .collect()
    }

    #[must_use]
    pub fn summaries_for(&self, context: &str, target: &ResourceTarget) -> Vec<PortForwardInfo> {
        self.sessions
            .values()
            .filter(|session| session.info.context == context && &session.info.target == target)
            .map(|session| session.info.clone())
            .collect()
    }

    pub fn handle_event(&mut self, event: PortForwardEvent) -> Option<String> {
        match event {
            PortForwardEvent::Output { id, line } => {
                let session = self.sessions.get_mut(&id)?;
                if line.contains("Forwarding from ") {
                    session.info.state = PortForwardState::Running;
                    Some(format!(
                        "Port forward active on {}:{}",
                        session.info.address, session.info.local_port
                    ))
                } else {
                    session.last_output = Some(line);
                    None
                }
            }
            PortForwardEvent::Exited { id, success } => {
                let session = self.sessions.remove(&id)?;
                if success {
                    Some(format!(
                        "Port forward on {}:{} stopped",
                        session.info.address, session.info.local_port
                    ))
                } else {
                    Some(session.last_output.map_or_else(
                        || "kubectl port-forward exited unexpectedly".to_owned(),
                        |line| format!("Port forward failed: {line}"),
                    ))
                }
            }
        }
    }
}

impl Drop for PortForwardManager {
    fn drop(&mut self) {
        for (_, session) in std::mem::take(&mut self.sessions) {
            let _ = session.stop.send(());
        }
    }
}

async fn relay_output(
    reader: impl AsyncRead + Unpin,
    id: u64,
    events: mpsc::Sender<PortForwardEvent>,
) {
    let mut lines = BufReader::new(reader).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if events
            .send(PortForwardEvent::Output { id, line })
            .await
            .is_err()
        {
            break;
        }
    }
}

fn command_args(context: &str, request: &PortForwardRequest) -> Result<Vec<OsString>, String> {
    let resource = match request.target.kind.known_kind() {
        Some(ResourceKind::Pods) => "pod",
        Some(ResourceKind::Services) => "service",
        _ => return Err("Port forwarding is only supported for Pods and Services".to_owned()),
    };
    let mut args = vec![OsString::from("--context"), OsString::from(context)];
    if let Some(namespace) = &request.target.namespace {
        args.extend([OsString::from("--namespace"), OsString::from(namespace)]);
    }
    args.extend([
        OsString::from("port-forward"),
        OsString::from(format!("{resource}/{}", request.target.name)),
        OsString::from(format!("{}:{}", request.local_port, request.remote_port)),
        OsString::from("--address"),
        OsString::from(&request.address),
    ]);
    Ok(args)
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::{fs, os::unix::fs::PermissionsExt, time::Duration};

    use tokio::sync::mpsc;

    use super::*;
    use crate::model::ResourceDescriptor;

    fn request(kind: ResourceKind) -> PortForwardRequest {
        PortForwardRequest {
            target: ResourceTarget {
                kind: ResourceDescriptor::from(kind),
                namespace: Some("crossplane-system".to_owned()),
                name: "provider-terraform".to_owned(),
            },
            remote_port: 8080,
            local_port: 18080,
            address: "localhost".to_owned(),
        }
    }

    #[test]
    fn builds_pod_port_forward_arguments() {
        let args = command_args("demo", &request(ResourceKind::Pods)).unwrap();
        assert_eq!(
            args,
            [
                "--context",
                "demo",
                "--namespace",
                "crossplane-system",
                "port-forward",
                "pod/provider-terraform",
                "18080:8080",
                "--address",
                "localhost",
            ]
            .map(OsString::from)
        );
    }

    #[test]
    fn builds_service_port_forward_arguments() {
        let args = command_args("demo", &request(ResourceKind::Services)).unwrap();
        assert_eq!(args[5], OsString::from("service/provider-terraform"));
    }

    #[test]
    fn rejects_non_forwardable_resources() {
        assert!(command_args("demo", &request(ResourceKind::Deployments)).is_err());
    }

    #[test]
    fn manager_starts_empty() {
        let (events, _) = mpsc::channel(4);
        let manager = PortForwardManager::new(events);
        assert!(manager.sessions.is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn manager_tracks_and_stops_a_real_child_process() {
        let fixture = std::env::temp_dir().join(format!("kux-fake-kubectl-{}", std::process::id()));
        fs::write(
            &fixture,
            "#!/bin/sh\nprintf 'Forwarding from 127.0.0.1:18080 -> 8080\\n'\nexec sleep 30\n",
        )
        .unwrap();
        let mut permissions = fs::metadata(&fixture).unwrap().permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&fixture, permissions).unwrap();

        let (event_tx, mut event_rx) = mpsc::channel(8);
        let mut manager = PortForwardManager::with_program(&fixture, event_tx);
        let target = request(ResourceKind::Pods).target;
        let info = manager
            .start("demo", request(ResourceKind::Pods))
            .expect("start fake port forward");
        let event = tokio::time::timeout(Duration::from_secs(2), event_rx.recv())
            .await
            .expect("fake forward output")
            .expect("forward event");
        manager.handle_event(event);

        assert_eq!(
            manager.summaries_for("demo", &target)[0].state,
            PortForwardState::Running
        );
        assert_eq!(manager.summaries("demo").len(), 1);
        assert!(manager.summaries("other").is_empty());
        assert_eq!(
            manager.stop(info.id).map(|forward| forward.id),
            Some(info.id)
        );
        assert!(manager.summaries_for("demo", &target).is_empty());
        fs::remove_file(fixture).unwrap();
    }
}
