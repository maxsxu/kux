use std::{
    ffi::OsString,
    path::Path,
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use k8s_openapi::api::core::v1::Pod;
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Command,
    sync::{mpsc, watch},
    task::JoinHandle,
};

use crate::{
    app::{App, ResourceTarget},
    connection::ConnectionOptions,
};

const OUTPUT_LIMIT: usize = 16 * 1024;

pub(crate) async fn load_pod(
    client: kube::Client,
    target: &ResourceTarget,
) -> Result<TransferPod, String> {
    if target.kind.known_kind() != Some(crate::model::ResourceKind::Pods) {
        return Err("File transfer requires a Pod".to_owned());
    }
    let namespace = target
        .namespace
        .as_deref()
        .filter(|namespace| !namespace.is_empty())
        .ok_or_else(|| "Pod namespace is required".to_owned())?;
    let pod = tokio::time::timeout(
        Duration::from_secs(10),
        kube::Api::<Pod>::namespaced(client, namespace).get(&target.name),
    )
    .await
    .map_err(|_| "Loading transfer containers timed out".to_owned())?
    .map_err(|error| error.to_string())?;
    Ok(TransferPod::from_pod(&pod))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferPod {
    pub containers: Vec<String>,
}

impl TransferPod {
    #[must_use]
    pub fn from_pod(pod: &Pod) -> Self {
        let Some(spec) = &pod.spec else {
            return Self {
                containers: Vec::new(),
            };
        };
        let mut containers: Vec<_> = spec
            .containers
            .iter()
            .map(|container| container.name.clone())
            .collect();
        if let Some(default) = pod
            .metadata
            .annotations
            .as_ref()
            .and_then(|annotations| annotations.get("kubectl.kubernetes.io/default-container"))
            && let Some(index) = containers.iter().position(|name| name == default)
        {
            let default = containers.remove(index);
            containers.insert(0, default);
        }
        containers.extend(
            spec.init_containers
                .iter()
                .flatten()
                .filter(|container| container.restart_policy.as_deref() == Some("Always"))
                .map(|container| container.name.clone()),
        );
        containers.extend(
            spec.ephemeral_containers
                .iter()
                .flatten()
                .map(|container| container.name.clone()),
        );
        Self { containers }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferRequest {
    pub target: ResourceTarget,
    pub download: bool,
    pub from: String,
    pub to: String,
    pub no_preserve: bool,
    pub container: String,
    pub retries: i32,
}

impl TransferRequest {
    /// Validate and trim form values before handing them to kubectl.
    ///
    /// # Errors
    /// Returns a diagnostic when either path cannot identify a copy endpoint.
    pub fn normalized(mut self) -> Result<Self, String> {
        self.from = self.from.trim().to_owned();
        self.to = self.to.trim().to_owned();
        self.container = self.container.trim().to_owned();
        for (label, value) in [("From", &self.from), ("To", &self.to)] {
            if value.is_empty() || value.starts_with('-') || value.chars().any(char::is_control) {
                return Err(format!(
                    "{label} requires a usable path; prefix local names beginning with '-' with './'"
                ));
            }
        }
        let (remote, local) = if self.download {
            (&self.from, &self.to)
        } else {
            (&self.to, &self.from)
        };
        let Some((pod, path)) = remote.split_once(':') else {
            return Err("Remote path must use [namespace/]pod:path".to_owned());
        };
        if pod.is_empty()
            || path.is_empty()
            || pod.split('/').any(str::is_empty)
            || pod.matches('/').count() > 1
        {
            return Err(
                "Remote path must include a Pod and a nonempty file or directory path".to_owned(),
            );
        }
        if local.contains(':') {
            return Err(
                "Local path cannot contain ':' because kubectl interprets it as a remote path"
                    .to_owned(),
            );
        }
        if self.container.chars().any(char::is_control) {
            return Err("Container cannot contain control characters".to_owned());
        }
        Ok(self)
    }

    #[must_use]
    pub fn kubectl_args(&self, connection: &ConnectionOptions, context: &str) -> Vec<OsString> {
        let mut args = connection.kubectl_args(Some(context));
        args.extend([
            "cp".into(),
            self.from.clone().into(),
            self.to.clone().into(),
            format!("--no-preserve={}", self.no_preserve).into(),
            format!("--retries={}", self.retries).into(),
        ]);
        if !self.container.is_empty() {
            args.push(format!("-c={}", self.container).into());
        }
        args
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransferIntent {
    Start {
        id: u64,
        request: Box<TransferRequest>,
    },
    Cancel {
        id: u64,
    },
}

#[derive(Debug)]
pub struct TransferCompletion {
    pub id: u64,
    pub context: String,
    pub generation: u64,
    pub result: Result<String, String>,
}

struct RunningTransfer {
    id: u64,
    context: String,
    generation: u64,
    cancel: watch::Sender<bool>,
    task: JoinHandle<()>,
}

pub struct TransferRuntime {
    demo: bool,
    program: OsString,
    write_enabled: Arc<AtomicBool>,
    running: Option<RunningTransfer>,
    sender: mpsc::Sender<TransferCompletion>,
    pub results: mpsc::Receiver<TransferCompletion>,
}

impl TransferRuntime {
    #[must_use]
    pub fn new(demo: bool) -> Self {
        let (sender, results) = mpsc::channel(8);
        Self {
            demo,
            program: "kubectl".into(),
            write_enabled: Arc::new(AtomicBool::new(false)),
            running: None,
            sender,
            results,
        }
    }

    pub fn sync(&mut self, app: &App) {
        self.write_enabled.store(!app.read_only, Ordering::Release);
        if let Some(running) = &self.running
            && (app.read_only
                || running.context != app.context
                || running.generation != app.generation)
        {
            let _ = running.cancel.send(true);
        }
    }

    pub fn submit(&mut self, app: &mut App, intent: TransferIntent) {
        self.sync(app);
        match intent {
            TransferIntent::Cancel { id } => {
                if let Some(running) = &self.running
                    && running.id == id
                {
                    let _ = running.cancel.send(true);
                }
            }
            TransferIntent::Start { id, request } => {
                if app.read_only {
                    app.transfer_failed(
                        id,
                        "File transfers are disabled in read-only mode".to_owned(),
                    );
                    return;
                }
                if self
                    .running
                    .as_ref()
                    .is_some_and(|running| !running.task.is_finished())
                {
                    app.transfer_failed(
                        id,
                        "A file transfer is still stopping; try again shortly".to_owned(),
                    );
                    return;
                }
                let (cancel, receiver) = watch::channel(false);
                let context = app.context.clone();
                let generation = app.generation;
                let connection = app.connection.clone();
                let program = self.program.clone();
                let demo = self.demo;
                let permitted = self.write_enabled.clone();
                let sender = self.sender.clone();
                let captured_context = context.clone();
                let task = tokio::spawn(async move {
                    let result = execute_transfer(
                        &program,
                        *request,
                        &connection,
                        &captured_context,
                        demo,
                        &permitted,
                        receiver,
                    )
                    .await;
                    // Completion delivery must not prevent shutdown if the UI
                    // has stopped draining results.
                    let _ = sender.try_send(TransferCompletion {
                        id,
                        context: captured_context,
                        generation,
                        result,
                    });
                });
                self.running = Some(RunningTransfer {
                    id,
                    context,
                    generation,
                    cancel,
                    task,
                });
            }
        }
    }

    pub async fn shutdown(&mut self) {
        if let Some(running) = self.running.take() {
            let _ = running.cancel.send(true);
            let _ = running.task.await;
        }
    }
}

impl Drop for TransferRuntime {
    fn drop(&mut self) {
        if let Some(running) = &self.running {
            let _ = running.cancel.send(true);
        }
    }
}

async fn validate_local(request: &TransferRequest) -> Result<(), String> {
    let local = if request.download {
        &request.to
    } else {
        &request.from
    };
    let local = local.clone();
    let download = request.download;
    tokio::task::spawn_blocking(move || {
        let path = Path::new(&local);
        match std::fs::metadata(path) {
            Ok(metadata) if metadata.is_file() || metadata.is_dir() => Ok(()),
            Ok(_) => Err("Local path must be a regular file or directory".to_owned()),
            Err(error) if download && error.kind() == std::io::ErrorKind::NotFound => {
                let parent = path
                    .parent()
                    .filter(|parent| !parent.as_os_str().is_empty())
                    .unwrap_or_else(|| Path::new("."));
                match std::fs::metadata(parent) {
                    Ok(metadata) if metadata.is_dir() => Ok(()),
                    _ => Err("Download destination parent directory does not exist".to_owned()),
                }
            }
            Err(error) => Err(format!("Cannot access local path: {error}")),
        }
    })
    .await
    .map_err(|error| format!("Path validation failed: {error}"))?
}

async fn canceled(receiver: &mut watch::Receiver<bool>) {
    if !*receiver.borrow() {
        let _ = receiver.changed().await;
    }
}

async fn bounded_output(mut stream: impl AsyncRead + Unpin) -> String {
    let mut bytes = Vec::new();
    let mut chunk = [0; 4096];
    loop {
        match stream.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(length) => {
                bytes.extend_from_slice(&chunk[..length]);
                if bytes.len() > OUTPUT_LIMIT {
                    bytes.drain(..bytes.len() - OUTPUT_LIMIT);
                }
            }
        }
    }
    String::from_utf8_lossy(&bytes).trim().to_owned()
}

async fn execute_transfer(
    program: &std::ffi::OsStr,
    request: TransferRequest,
    connection: &ConnectionOptions,
    context: &str,
    demo: bool,
    permitted: &AtomicBool,
    mut cancel: watch::Receiver<bool>,
) -> Result<String, String> {
    let request = request.normalized()?;
    tokio::select! {
        result = validate_local(&request) => result?,
        () = canceled(&mut cancel) => return Err("File transfer canceled".to_owned()),
    }
    if !permitted.load(Ordering::Acquire) {
        return Err("File transfers are disabled in read-only mode".to_owned());
    }
    if *cancel.borrow() {
        return Err("File transfer canceled".to_owned());
    }
    let direction = if request.download {
        "Download"
    } else {
        "Upload"
    };
    if demo {
        return Ok(format!(
            "Demo: would {} {} to {}",
            direction.to_lowercase(),
            request.from,
            request.to
        ));
    }
    let mut child = Command::new(program)
        .args(request.kubectl_args(connection, context))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| format!("Unable to run kubectl cp: {error}"))?;
    let mut stdout = tokio::spawn(bounded_output(child.stdout.take().expect("piped stdout")));
    let mut stderr = tokio::spawn(bounded_output(child.stderr.take().expect("piped stderr")));
    let status = tokio::select! {
        result = child.wait() => result.map_err(|error| format!("Unable to wait for kubectl cp: {error}")),
        () = canceled(&mut cancel) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            Err("File transfer canceled".to_owned())
        }
    };
    let diagnostics = async {
        let (stdout, stderr) = tokio::join!(&mut stdout, &mut stderr);
        format!(
            "{}\n{}",
            stderr.unwrap_or_default(),
            stdout.unwrap_or_default()
        )
        .trim()
        .to_owned()
    };
    let diagnostics = tokio::time::timeout(Duration::from_secs(1), diagnostics)
        .await
        .unwrap_or_default();
    // An exec credential helper may retain an inherited pipe after kubectl
    // exits. Stop both readers even when the bounded drain times out.
    if !stdout.is_finished() {
        stdout.abort();
        let _ = stdout.await;
    }
    if !stderr.is_finished() {
        stderr.abort();
        let _ = stderr.await;
    }
    let status = status?;
    if status.success() {
        Ok(format!(
            "{direction} completed: {} → {}",
            request.from, request.to
        ))
    } else {
        Err(connection.redact(&format!("kubectl cp failed ({status}): {diagnostics}")))
    }
}

#[cfg(test)]
#[path = "transfer_tests.rs"]
mod tests;
