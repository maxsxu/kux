//! Direct process execution for confirmed plugins. Every pipeline stage is
//! owned and waited for; background output is drained into bounded buffers.

use std::collections::BTreeMap;
use std::future::Future;
use std::process::{ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::plugins::PluginInvocation;

const MAX_BACKGROUND_PLUGINS: usize = 16;
const MAX_OUTPUT_BYTES: usize = 65_536;
const PIPE_DRAIN_TIMEOUT: Duration = Duration::from_millis(500);

struct Pipeline {
    children: Vec<Child>,
    stdout: Option<tokio::process::ChildStdout>,
    stderr: Vec<tokio::process::ChildStderr>,
}

fn spawn_pipeline(invocation: &PluginInvocation, capture: bool) -> Result<Pipeline, String> {
    invocation.validate()?;
    let mut pipeline = Pipeline {
        children: Vec::new(),
        stdout: None,
        stderr: Vec::new(),
    };
    let stages = std::iter::once((&invocation.program, &invocation.args))
        .chain(
            invocation
                .pipes
                .iter()
                .map(|pipe| (&pipe.program, &pipe.args)),
        )
        .collect::<Vec<_>>();
    let mut input = None;
    for (index, (program, args)) in stages.iter().enumerate() {
        let last = index + 1 == stages.len();
        let mut command = Command::new(program);
        command.args(*args).envs(&invocation.env).kill_on_drop(true);
        command.stdin(input.take().unwrap_or_else(|| {
            if capture {
                Stdio::null()
            } else {
                Stdio::inherit()
            }
        }));
        command.stdout(if last && !capture {
            Stdio::inherit()
        } else {
            Stdio::piped()
        });
        command.stderr(if capture {
            Stdio::piped()
        } else {
            Stdio::inherit()
        });
        let mut child = command
            .spawn()
            .map_err(|error| format!("Unable to start plugin stage {}: {error}", index + 1))?;
        if let Some(stderr) = child.stderr.take() {
            pipeline.stderr.push(stderr);
        }
        if last {
            pipeline.stdout = child.stdout.take();
        } else {
            input = child
                .stdout
                .take()
                .map(TryInto::try_into)
                .transpose()
                .map_err(|error| format!("Unable to connect plugin pipe: {error}"))?;
        }
        pipeline.children.push(child);
    }
    Ok(pipeline)
}

async fn wait_pipeline(children: &mut [Child]) -> Result<ExitStatus, String> {
    let mut status = None;
    let mut failed = None;
    for child in children {
        let current = child
            .wait()
            .await
            .map_err(|error| format!("Unable to wait for plugin: {error}"))?;
        if !current.success() && failed.is_none() {
            failed = Some(current);
        }
        status = Some(current);
    }
    failed
        .or(status)
        .ok_or_else(|| "Plugin pipeline has no processes".to_owned())
}

/// Run a foreground pipeline using inherited terminal I/O. The caller must
/// suspend the TUI before awaiting this function and restore it afterward.
/// Ctrl-C terminates and reaps every direct pipeline process, including stages
/// that ignore the terminal interrupt themselves.
/// Dropping the future requests termination of each direct pipeline process.
///
/// # Errors
/// Returns signal registration, spawn, pipe setup, or wait errors. Nonzero exits
/// are returned as an `ExitStatus`, including intermediate pipeline failures.
pub async fn run_foreground(invocation: &PluginInvocation) -> Result<ExitStatus, String> {
    #[cfg(unix)]
    {
        // Register before spawning: a terminal interrupt must not kill the TUI
        // during the gap between child creation and the first select poll.
        let mut interrupt =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
                .map_err(|error| format!("Unable to receive plugin interrupts: {error}"))?;
        run_foreground_until(invocation, async {
            interrupt
                .recv()
                .await
                .ok_or_else(|| "Plugin interrupt listener closed".to_owned())
        })
        .await
    }
    #[cfg(not(unix))]
    {
        run_foreground_until(invocation, async {
            tokio::signal::ctrl_c()
                .await
                .map_err(|error| format!("Unable to receive plugin interrupts: {error}"))
        })
        .await
    }
}

async fn run_foreground_until(
    invocation: &PluginInvocation,
    cancelled: impl Future<Output = Result<(), String>>,
) -> Result<ExitStatus, String> {
    let mut pipeline = spawn_pipeline(invocation, false)?;
    tokio::select! {
        result = wait_pipeline(&mut pipeline.children) => result,
        result = cancelled => {
            for child in &mut pipeline.children {
                let _ = child.start_kill();
            }
            let status = wait_pipeline(&mut pipeline.children).await;
            result?;
            status
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)]
pub struct PluginProcessResult {
    pub id: u64,
    pub name: String,
    pub description: String,
    pub success: bool,
    pub exit_code: Option<i32>,
    pub cancelled: bool,
    pub stdout: String,
    pub stderr: String,
    pub truncated: bool,
    pub overwrite_output: bool,
    pub error: Option<String>,
}

#[derive(Debug)]
struct BackgroundSession {
    stop: Option<oneshot::Sender<()>>,
    task: JoinHandle<()>,
}

/// Owns at most 16 background plugin pipelines. `poll` returns completed
/// results without blocking; dropping the manager cancels all owned tasks.
#[derive(Debug)]
pub struct PluginProcessManager {
    next_id: u64,
    sessions: BTreeMap<u64, BackgroundSession>,
    events_tx: mpsc::Sender<PluginProcessResult>,
    events_rx: mpsc::Receiver<PluginProcessResult>,
}

impl Default for PluginProcessManager {
    fn default() -> Self {
        Self::new()
    }
}

impl PluginProcessManager {
    #[must_use]
    pub fn new() -> Self {
        let (events_tx, events_rx) = mpsc::channel(MAX_BACKGROUND_PLUGINS);
        Self {
            next_id: 1,
            sessions: BTreeMap::new(),
            events_tx,
            events_rx,
        }
    }

    /// Start a background plugin with closed stdin and bounded captured output.
    ///
    /// # Errors
    /// Returns an error if the invocation is invalid, the process cannot start,
    /// or the concurrent plugin limit is reached.
    pub fn start(&mut self, invocation: PluginInvocation) -> Result<u64, String> {
        if self.sessions.len() >= MAX_BACKGROUND_PLUGINS {
            return Err(format!(
                "At most {MAX_BACKGROUND_PLUGINS} background plugins can run at once"
            ));
        }
        let pipeline = spawn_pipeline(&invocation, true)?;
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        let (stop, stopped) = oneshot::channel();
        let events = self.events_tx.clone();
        let task = tokio::spawn(async move {
            let result = run_background(id, invocation, pipeline, stopped).await;
            let _ = events.send(result).await;
        });
        self.sessions.insert(
            id,
            BackgroundSession {
                stop: Some(stop),
                task,
            },
        );
        Ok(id)
    }

    #[must_use]
    pub fn poll(&mut self) -> Vec<PluginProcessResult> {
        let mut completed = Vec::new();
        while let Ok(result) = self.events_rx.try_recv() {
            self.sessions.remove(&result.id);
            completed.push(result);
        }
        completed
    }

    /// Request stop. A cancellation result will arrive through `poll`.
    pub fn stop(&mut self, id: u64) -> bool {
        if let Some(session) = self.sessions.get_mut(&id)
            && let Some(stop) = session.stop.take()
        {
            let _ = stop.send(());
            true
        } else {
            false
        }
    }

    #[must_use]
    pub fn active_count(&self) -> usize {
        self.sessions.len()
    }
}

impl Drop for PluginProcessManager {
    fn drop(&mut self) {
        for (_, session) in std::mem::take(&mut self.sessions) {
            if let Some(stop) = session.stop {
                let _ = stop.send(());
            }
            // Aborting drops all owned Child handles (kill_on_drop enabled),
            // including if a task has not yet reached its select branch.
            session.task.abort();
        }
    }
}

#[derive(Debug, Default)]
struct OutputBuffer {
    bytes: Vec<u8>,
    truncated: bool,
}

struct OutputCapture {
    buffer: Arc<Mutex<OutputBuffer>>,
    readers: Vec<JoinHandle<()>>,
}

impl OutputCapture {
    fn new() -> Self {
        Self {
            buffer: Arc::new(Mutex::new(OutputBuffer::default())),
            readers: Vec::new(),
        }
    }

    fn read(&mut self, mut stream: impl AsyncRead + Unpin + Send + 'static) {
        let buffer = Arc::clone(&self.buffer);
        self.readers.push(tokio::spawn(async move {
            let mut chunk = [0_u8; 4096];
            while let Ok(count) = stream.read(&mut chunk).await {
                if count == 0 {
                    break;
                }
                if let Ok(mut output) = buffer.lock() {
                    let available = MAX_OUTPUT_BYTES.saturating_sub(output.bytes.len());
                    output
                        .bytes
                        .extend_from_slice(&chunk[..count.min(available)]);
                    output.truncated |= count > available;
                } else {
                    break;
                }
            }
        }));
    }

    async fn finish(mut self) -> (String, bool) {
        // A plugin may create grandchildren that retain stdout after their
        // direct parent exits. Never wait indefinitely for inherited pipe FDs.
        let drained = tokio::time::timeout(PIPE_DRAIN_TIMEOUT, async {
            for task in &mut self.readers {
                let _ = task.await;
            }
        })
        .await
        .is_ok();
        self.buffer.lock().map_or_else(
            |_| (String::new(), true),
            |buffer| {
                (
                    String::from_utf8_lossy(&buffer.bytes).into_owned(),
                    buffer.truncated || !drained,
                )
            },
        )
    }
}

impl Drop for OutputCapture {
    fn drop(&mut self) {
        for task in &self.readers {
            task.abort();
        }
    }
}

async fn run_background(
    id: u64,
    invocation: PluginInvocation,
    mut pipeline: Pipeline,
    mut stop: oneshot::Receiver<()>,
) -> PluginProcessResult {
    let mut stdout = OutputCapture::new();
    if let Some(stream) = pipeline.stdout.take() {
        stdout.read(stream);
    }
    let mut stderr = OutputCapture::new();
    for stream in pipeline.stderr.drain(..) {
        stderr.read(stream);
    }
    let (status, cancelled) = tokio::select! {
        result = wait_pipeline(&mut pipeline.children) => (result, false),
        _ = &mut stop => {
            for child in &mut pipeline.children {
                let _ = child.start_kill();
            }
            (wait_pipeline(&mut pipeline.children).await, true)
        }
    };
    let ((stdout, stdout_truncated), (stderr, stderr_truncated)) =
        tokio::join!(stdout.finish(), stderr.finish());
    let (success, exit_code, error) = match status {
        Ok(status) => (status.success() && !cancelled, status.code(), None),
        Err(error) => (false, None, Some(error)),
    };
    PluginProcessResult {
        id,
        name: invocation.name,
        description: invocation.description,
        success,
        exit_code,
        cancelled,
        stdout,
        stderr,
        truncated: stdout_truncated || stderr_truncated,
        overwrite_output: invocation.overwrite_output,
        error,
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    use crate::plugins::{PluginEnvironment, PluginStage};

    use super::*;

    static SEQUENCE: AtomicU64 = AtomicU64::new(0);

    struct FakePrograms(PathBuf);

    impl FakePrograms {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "kux-plugin-process-{}-{}",
                std::process::id(),
                SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn script(&self, name: &str, body: &str) -> String {
            let path = self.0.join(name);
            std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
            path.to_string_lossy().into_owned()
        }

        fn invocation(&self, body: &str) -> PluginInvocation {
            PluginInvocation {
                name: "test".to_owned(),
                description: "Fake test plugin".to_owned(),
                dangerous: false,
                program: self.script("plugin", body),
                args: Vec::new(),
                env: PluginEnvironment::new(),
                pipes: Vec::new(),
                background: true,
                overwrite_output: true,
            }
        }
    }

    impl Drop for FakePrograms {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    async fn completion(manager: &mut PluginProcessManager) -> PluginProcessResult {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Some(result) = manager.poll().into_iter().next() {
                    return result;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("fake plugin finished")
    }

    #[tokio::test]
    async fn direct_argv_and_environment_are_delivered_without_shell_evaluation() {
        let programs = FakePrograms::new();
        let mut invocation =
            programs.invocation("printf '<%s>\\n' \"$1\" \"$NAME\"; printf 'diagnostic' >&2");
        invocation.args.push("one two; $(never-execute)".to_owned());
        invocation
            .env
            .insert("NAME".to_owned(), "resource; $(also-never)".to_owned());
        let mut manager = PluginProcessManager::new();
        let id = manager.start(invocation).unwrap();
        let result = completion(&mut manager).await;
        assert_eq!(result.id, id);
        assert!(result.success);
        assert_eq!(
            result.stdout,
            "<one two; $(never-execute)>\n<resource; $(also-never)>\n"
        );
        assert_eq!(result.stderr, "diagnostic");
        assert!(result.overwrite_output);
        assert_eq!(manager.active_count(), 0);
    }

    #[tokio::test]
    async fn background_pipelines_drain_bounded_output_and_capture_all_stage_errors() {
        let programs = FakePrograms::new();
        let mut invocation = programs.invocation("i=0; while [ \"$i\" -lt 3000 ]; do printf 'abcdefghijklmnopqrstuvwxyz\\n'; i=$((i+1)); done; printf 'producer-error' >&2");
        invocation.pipes.push(PluginStage { program: programs.script("consumer", "while IFS= read -r line; do printf '%s\\n' \"$line\"; done; printf 'consumer-error' >&2"), args: Vec::new() });
        let mut manager = PluginProcessManager::new();
        manager.start(invocation).unwrap();
        let result = completion(&mut manager).await;
        assert!(result.success);
        assert_eq!(result.stdout.len(), MAX_OUTPUT_BYTES);
        assert!(result.truncated);
        assert!(result.stderr.contains("producer-error"));
        assert!(result.stderr.contains("consumer-error"));
    }

    #[tokio::test]
    async fn foreground_reports_intermediate_failure_even_if_consumer_succeeds() {
        let programs = FakePrograms::new();
        let mut invocation = programs.invocation("exit 7");
        invocation.background = false;
        invocation.pipes.push(PluginStage {
            program: programs.script("consumer", "while IFS= read -r line; do :; done; exit 0"),
            args: Vec::new(),
        });
        assert_eq!(run_foreground(&invocation).await.unwrap().code(), Some(7));
    }

    #[tokio::test]
    async fn foreground_inherits_io_and_uses_explicit_env_and_args() {
        let programs = FakePrograms::new();
        let mut invocation =
            programs.invocation("printf '%s|%s' \"$NAME\" \"$1\" > \"$OUTPUT_FILE\"");
        invocation.background = false;
        let output = programs.0.join("received");
        invocation.env.insert(
            "OUTPUT_FILE".to_owned(),
            output.to_string_lossy().into_owned(),
        );
        invocation.env.insert("NAME".to_owned(), "a b".to_owned());
        invocation.args.push("x;y".to_owned());
        assert!(run_foreground(&invocation).await.unwrap().success());
        assert_eq!(std::fs::read_to_string(output).unwrap(), "a b|x;y");
    }

    #[tokio::test]
    async fn foreground_cancellation_kills_and_reaps_every_direct_pipeline_stage() {
        let programs = FakePrograms::new();
        let mut invocation =
            programs.invocation("trap '' INT; printf '%s' \"$$\" > \"$FIRST_PID\"; exec sleep 30");
        invocation.background = false;
        invocation.pipes.push(PluginStage {
            program: programs.script(
                "consumer",
                "trap '' INT; printf '%s' \"$$\" > \"$SECOND_PID\"; exec sleep 30",
            ),
            args: Vec::new(),
        });
        let pid_files = [programs.0.join("first-pid"), programs.0.join("second-pid")];
        for (name, path) in ["FIRST_PID", "SECOND_PID"].iter().zip(&pid_files) {
            invocation
                .env
                .insert((*name).to_owned(), path.to_string_lossy().into_owned());
        }
        let cancelled = async {
            loop {
                if pid_files.iter().all(|path| {
                    std::fs::read_to_string(path).is_ok_and(|pid| pid.parse::<u32>().is_ok())
                }) {
                    return Ok(());
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        };
        let status = tokio::time::timeout(
            Duration::from_secs(5),
            run_foreground_until(&invocation, cancelled),
        )
        .await
        .expect("foreground cancellation completes")
        .unwrap();
        assert!(!status.success());
        for path in &pid_files {
            let pid = std::fs::read_to_string(path).unwrap();
            assert!(
                !Command::new("/bin/kill")
                    .args(["-0", &pid])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
                    .await
                    .unwrap()
                    .success(),
                "cancelled child {pid} must be reaped before return"
            );
        }
    }

    #[tokio::test]
    async fn stop_cancels_process_and_reaps_before_freeing_capacity() {
        let programs = FakePrograms::new();
        let invocation = programs.invocation("exec sleep 30");
        let mut manager = PluginProcessManager::new();
        let id = manager.start(invocation).unwrap();
        assert!(manager.stop(id));
        assert!(!manager.stop(id));
        assert_eq!(manager.active_count(), 1);
        let result = completion(&mut manager).await;
        assert!(result.cancelled);
        assert!(!result.success);
        assert_eq!(manager.active_count(), 0);
    }

    #[tokio::test]
    async fn spawn_failure_and_invalid_invocation_do_not_allocate_session() {
        let programs = FakePrograms::new();
        let mut invocation = programs.invocation("exit 0");
        invocation.program = programs
            .0
            .join("missing-program")
            .to_string_lossy()
            .into_owned();
        let mut manager = PluginProcessManager::new();
        assert!(manager.start(invocation.clone()).is_err());
        invocation.program = "bad\0program".to_owned();
        assert!(manager.start(invocation).is_err());
        assert_eq!(manager.active_count(), 0);
    }

    #[tokio::test]
    async fn dropping_manager_terminates_owned_background_child() {
        let programs = FakePrograms::new();
        let pid_file = programs.0.join("pid");
        let mut invocation =
            programs.invocation("printf '%s' \"$$\" > \"$PID_FILE\"; exec sleep 30");
        invocation.env.insert(
            "PID_FILE".to_owned(),
            pid_file.to_string_lossy().into_owned(),
        );
        let mut manager = PluginProcessManager::new();
        manager.start(invocation).unwrap();
        let pid = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Ok(pid) = std::fs::read_to_string(&pid_file)
                    && pid.parse::<u32>().is_ok()
                {
                    break pid;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("fake child announced its pid");
        drop(manager);
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let alive = Command::new("/bin/kill")
                    .args(["-0", &pid])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
                    .await
                    .unwrap()
                    .success();
                if !alive {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("manager drop kills and reaps its owned child");
    }
}
