use std::{
    collections::{BTreeMap, hash_map::RandomState},
    fmt::Write as _,
    hash::BuildHasher,
    io,
    time::Duration,
};

use futures::{AsyncRead, AsyncReadExt, StreamExt, stream::FuturesUnordered};
use k8s_openapi::{api::core::v1::Pod, jiff::Timestamp};
use kube::{Api, Client, api::LogParams};
use tokio::{sync::mpsc, task::JoinHandle};

use super::{ActionResult, LogLine, LogSessionMeta, LogSource, LogTarget, log_scope};
use crate::{config::LoggerSettings, connection::ConnectionOptions};

const BATCH_INTERVAL: Duration = Duration::from_millis(100);
const MAX_BATCH_LINES: usize = 256;
const MAX_BATCH_BYTES: usize = 256 * 1024;
const MAX_LINE_BYTES: usize = 64 * 1024;
const TRUNCATED_LINE_SUFFIX: &str = " … [line truncated at 64 KiB]";
const MAX_RESUME_BYTES: usize = 1024 * 1024;
const MAX_RESUME_LINES: usize = 5_000;

#[cfg(test)]
#[path = "log_session_tests.rs"]
mod session_tests;

#[derive(Clone, Copy)]
struct RetryPolicy {
    attempts: usize,
    initial_delay: Duration,
    max_delay: Duration,
    jitter: bool,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            // K9s v0.51.0 log retries are independent of maxConnRetry.
            attempts: 20,
            initial_delay: Duration::from_millis(500),
            max_delay: Duration::from_secs(15),
            jitter: true,
        }
    }
}

impl RetryPolicy {
    fn randomized_delay(self, delay: Duration, attempt: usize, random: &RandomState) -> Duration {
        if !self.jitter {
            return delay;
        }
        let per_mille = u32::try_from(random.hash_one(attempt) % 1_001).unwrap() + 500;
        delay.mul_f64(f64::from(per_mille) / 1_000.0)
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum LogRange {
    #[default]
    Tail,
    Head,
    SinceSeconds(i64),
}

impl LogRange {
    #[must_use]
    pub const fn from_since_seconds(seconds: i64) -> Self {
        if seconds > 0 {
            Self::SinceSeconds(seconds)
        } else {
            Self::Tail
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogRequest {
    pub id: u64,
    pub target: LogTarget,
    pub range: LogRange,
    pub tail_count: i64,
    pub all_containers: bool,
}

impl LogRequest {
    #[must_use]
    pub fn new(id: u64, target: LogTarget, settings: &LoggerSettings) -> Self {
        let settings = settings.normalized();
        Self {
            id,
            target,
            range: LogRange::from_since_seconds(settings.since_seconds),
            tail_count: settings.tail,
            all_containers: false,
        }
    }
}

/// Dropping the active stream also stops its HTTP request, including manager shutdown.
pub(super) struct LogTask {
    id: u64,
    task: JoinHandle<()>,
}

impl LogTask {
    pub(super) fn spawn(
        client: Client,
        connection: ConnectionOptions,
        generation: u64,
        request: LogRequest,
        results: mpsc::Sender<ActionResult>,
    ) -> Self {
        let id = request.id;
        let task = tokio::spawn(async move {
            stream_session(client, connection, generation, &request, &results).await;
        });
        Self { id, task }
    }

    pub(super) fn spawn_demo(
        generation: u64,
        request: LogRequest,
        results: mpsc::Sender<ActionResult>,
    ) -> Self {
        let id = request.id;
        let task = tokio::spawn(async move {
            stream_demo(generation, &request, &results).await;
        });
        Self { id, task }
    }

    pub(super) fn stop_if_matching(active: &mut Option<Self>, id: u64) {
        if active.as_ref().is_some_and(|task| task.id == id) {
            active.take();
        }
    }
}

impl Drop for LogTask {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// All stream futures are owned by this session. Dropping it closes every
/// request, including resolution, pending opens, retry delays and backpressure.
async fn stream_session(
    client: Client,
    connection: ConnectionOptions,
    generation: u64,
    request: &LogRequest,
    results: &mpsc::Sender<ActionResult>,
) {
    let resolved = async {
        let timeout = connection.api_call_timeout()?;
        let scope = log_scope::resolve(
            client.clone(),
            &request.target,
            request.all_containers,
            timeout,
        )
        .await?;
        Ok::<_, String>((scope, timeout))
    };
    let resolved = tokio::select! {
        result = resolved => result,
        () = results.closed() => return,
    };
    let (scope, timeout) = match resolved {
        Ok(scope) => scope,
        Err(error) => {
            let _ = send_batch(
                results,
                generation,
                request.id,
                Vec::new(),
                true,
                Some(connection.redact(&error)),
            )
            .await;
            return;
        }
    };
    if !send_resolution(results, generation, request.id, scope.metadata).await {
        return;
    }
    let count = scope.sources.len();
    let mut streams = FuturesUnordered::new();
    // A fixed concurrency cap would starve later sources behind never-ending
    // follow streams. Every resolved source gets a concurrently polled future.
    for source in &scope.sources {
        let client = client.clone();
        streams.push(async move {
            let output = LogOutput {
                results,
                generation,
                id: request.id,
                source: Some(source),
            };
            let error = stream_source(
                client,
                request,
                &output,
                RetryPolicy::default(),
                timeout,
                count,
            )
            .await;
            (source, error)
        });
    }
    let mut failures = 0_usize;
    let mut last_error = None;
    loop {
        let outcome = tokio::select! {
            result = streams.next() => result,
            () = results.closed() => return,
        };
        let Some((source, error)) = outcome else {
            break;
        };
        if let Some(error) = error {
            failures += 1;
            let error = connection.redact(&format!(
                "{}/{} [{}]: {error}",
                source.namespace, source.pod, source.container
            ));
            // Report a failed source immediately; other streams keep following.
            if !send_batch(
                results,
                generation,
                request.id,
                Vec::new(),
                false,
                Some(error.clone()),
            )
            .await
            {
                return;
            }
            last_error = Some(error);
        }
    }
    let error =
        last_error.map(|error| format!("{failures} of {count} log sources failed; {error}"));
    let _ = send_batch(results, generation, request.id, Vec::new(), true, error).await;
}

async fn send_resolution(
    results: &mpsc::Sender<ActionResult>,
    generation: u64,
    id: u64,
    metadata: LogSessionMeta,
) -> bool {
    results
        .send(ActionResult::Scoped {
            generation,
            result: Box::new(ActionResult::LogResolved { id, metadata }),
        })
        .await
        .is_ok()
}

struct LogOutput<'a> {
    results: &'a mpsc::Sender<ActionResult>,
    generation: u64,
    id: u64,
    source: Option<&'a LogSource>,
}

impl LogOutput<'_> {
    async fn send(&self, lines: Vec<String>) -> bool {
        if lines.is_empty() {
            return !self.results.is_closed();
        }
        let lines = lines
            .into_iter()
            .map(|text| LogLine {
                source: self.source.cloned(),
                text,
            })
            .collect();
        send_batch(self.results, self.generation, self.id, lines, false, None).await
    }
}

async fn api_timeout<T>(
    future: impl std::future::Future<Output = Result<T, kube::Error>>,
    timeout: Option<Duration>,
) -> Result<T, String> {
    match timeout {
        Some(timeout) => tokio::time::timeout(timeout, future)
            .await
            .map_err(|_| "Log API request timed out".to_owned())?
            .map_err(|error| error.to_string()),
        None => future.await.map_err(|error| error.to_string()),
    }
}

async fn stream_source(
    client: Client,
    request: &LogRequest,
    output: &LogOutput<'_>,
    policy: RetryPolicy,
    timeout: Option<Duration>,
    source_count: usize,
) -> Option<String> {
    let source = output.source.expect("live streams have an exact source");
    let pods = Api::<Pod>::namespaced(client, &source.namespace);
    let mut cursor = ResumeCursor::for_sources(source_count);
    let mut delay = policy.initial_delay;
    let mut nominal_delay = policy.initial_delay;
    let random = RandomState::new();
    for attempt in 1..=policy.attempts {
        let mut params = cursor.params(request);
        params.container = Some(source.container.clone());
        let opened = tokio::select! {
            result = api_timeout(pods.log_stream(&source.pod, &params), timeout) => result,
            () = output.results.closed() => return None,
        };
        let error = match opened {
            Ok(reader) => match read_stream(reader, output, &mut cursor).await {
                StreamEnd::Eof | StreamEnd::Closed => return None,
                StreamEnd::Error(error) => error.to_string(),
            },
            Err(error) => error,
        };
        let stopped = tokio::select! {
            result = stopped_pod(&pods, &source.pod, timeout) => result,
            () = output.results.closed() => return None,
        };
        if let Some(reason) = stopped {
            return Some(format!("{error}; {reason}"));
        }
        if !cursor.resumable {
            return Some(format!(
                "{error}; cannot safely resume logs: timestamp replay window unavailable; reopen logs"
            ));
        }
        if attempt == policy.attempts {
            return Some(format!(
                "log stream failed after {attempt} attempts: {error}"
            ));
        }
        tokio::select! {
            () = tokio::time::sleep(delay) => {},
            () = output.results.closed() => return None,
        }
        delay = policy.randomized_delay(nominal_delay, attempt, &random);
        nominal_delay = nominal_delay
            .saturating_add(nominal_delay / 2)
            .min(policy.max_delay);
        cursor.reconnect();
    }
    None
}

async fn stopped_pod(
    pods: &Api<Pod>,
    name: &str,
    timeout: Option<Duration>,
) -> Option<&'static str> {
    let response = match timeout {
        Some(timeout) => tokio::time::timeout(timeout, pods.get(name)).await.ok()?,
        None => pods.get(name).await,
    };
    match response {
        Ok(pod) if pod.metadata.deletion_timestamp.is_some() => Some("Pod is terminating"),
        Ok(pod)
            if matches!(
                pod.status
                    .as_ref()
                    .and_then(|status| status.phase.as_deref()),
                Some("Succeeded" | "Failed")
            ) =>
        {
            Some("Pod has completed")
        }
        Err(kube::Error::Api(error)) if error.code == 404 => Some("Pod no longer exists"),
        // A failed status check does not turn a transient cluster outage into a
        // permanent log failure. The independent retry budget still applies.
        _ => None,
    }
}

fn log_params(request: &LogRequest) -> LogParams {
    let mut params = LogParams {
        container: request.target.container.clone(),
        previous: request.target.previous,
        follow: true,
        timestamps: true,
        tail_lines: Some(
            LoggerSettings {
                tail: request.tail_count,
                ..LoggerSettings::default()
            }
            .normalized()
            .tail,
        ),
        ..LogParams::default()
    };
    match request.range {
        LogRange::Head => {
            params.follow = false;
            params.tail_lines = None;
            params.limit_bytes = Some(5_000);
        }
        LogRange::SinceSeconds(seconds) if seconds > 0 => params.since_seconds = Some(seconds),
        LogRange::Tail | LogRange::SinceSeconds(_) => {}
    }
    params
}

struct ResumeCursor {
    timestamp: Option<Timestamp>,
    lines: BTreeMap<String, usize>,
    bytes: usize,
    replay: Option<BTreeMap<String, usize>>,
    resumable: bool,
    max_bytes: usize,
    max_lines: usize,
    source_count: usize,
}

impl Default for ResumeCursor {
    fn default() -> Self {
        Self {
            timestamp: None,
            lines: BTreeMap::new(),
            bytes: 0,
            replay: None,
            resumable: true,
            max_bytes: MAX_RESUME_BYTES,
            max_lines: MAX_RESUME_LINES,
            source_count: 1,
        }
    }
}

impl ResumeCursor {
    fn for_sources(count: usize) -> Self {
        // Each source keeps its own timestamp and replay counts. Divide the
        // session budget so a large workload cannot multiply replay storage.
        // A reconnect also owns a replay copy: both maps together are at most
        // twice this budget, regardless of the number of sources.
        Self {
            max_bytes: MAX_RESUME_BYTES / count.max(1),
            max_lines: MAX_RESUME_LINES / count.max(1),
            source_count: count.max(1),
            ..Self::default()
        }
    }

    fn params(&self, request: &LogRequest) -> LogParams {
        let mut params = log_params(request);
        if request.range != LogRange::Head && self.replay.is_some() {
            params.since_time = self.timestamp;
            params.since_seconds = None;
            // A reconnect can have more new lines than the original tail limit.
            params.tail_lines = None;
        }
        params
    }

    fn reconnect(&mut self) {
        if self.timestamp.is_some() {
            self.replay = Some(self.lines.clone());
        }
    }

    fn accept(&mut self, line: &str) -> bool {
        let timestamp = line
            .split_once(' ')
            .and_then(|(timestamp, _)| timestamp.parse::<Timestamp>().ok());
        let Some(timestamp) = timestamp else {
            // timestamps=true should prevent this; never silently replay an
            // unidentifiable line after an interrupted response.
            self.resumable = false;
            return true;
        };
        if self.replay.is_some() && Some(timestamp) < self.timestamp {
            return false;
        }
        if Some(timestamp) < self.timestamp {
            // A clock step backwards makes an inclusive time cursor ambiguous.
            // Keep displaying the stream, but do not replay an unsafe interval.
            self.resumable = false;
            return true;
        }
        if Some(timestamp) != self.timestamp {
            self.timestamp = Some(timestamp);
            self.lines.clear();
            self.bytes = 0;
            self.replay = None;
            self.resumable = true;
        }
        if let Some(remaining) = self.replay.as_mut().and_then(|replay| replay.get_mut(line))
            && *remaining > 0
        {
            *remaining -= 1;
            return false;
        }
        if let Some(count) = self.lines.get_mut(line) {
            *count = count.saturating_add(1);
        } else if self.lines.len() < self.max_lines && self.bytes + line.len() <= self.max_bytes {
            self.bytes += line.len();
            self.lines.insert(line.to_owned(), 1);
        } else {
            self.resumable = false;
        }
        true
    }
}

#[derive(Default)]
struct LogBuffer {
    partial: Vec<u8>,
    truncated: bool,
    lines: Vec<String>,
    bytes: usize,
}

impl LogBuffer {
    fn push(&mut self, byte: u8, cursor: &mut ResumeCursor) {
        if byte == b'\n' {
            self.finish_line(cursor);
        } else if self.partial.len() < MAX_LINE_BYTES {
            self.partial.push(byte);
        } else {
            self.truncated = true;
        }
    }

    fn finish_line(&mut self, cursor: &mut ResumeCursor) {
        if self.partial.last() == Some(&b'\r') {
            self.partial.pop();
        }
        let mut line = String::from_utf8_lossy(&self.partial).into_owned();
        if self.truncated {
            line.push_str(TRUNCATED_LINE_SUFFIX);
        }
        if cursor.accept(&line) {
            self.bytes += line.len();
            self.lines.push(line);
        }
        self.partial.clear();
        self.truncated = false;
    }

    fn full(&self, source_count: usize) -> bool {
        // Share batching capacity across streams, while allowing one complete
        // bounded line per source. Long partial lines remain bounded per source.
        self.lines.len() >= (MAX_BATCH_LINES / source_count).max(1)
            || self.bytes >= (MAX_BATCH_BYTES / source_count).max(1)
    }

    fn take_lines(&mut self) -> Vec<String> {
        self.bytes = 0;
        std::mem::take(&mut self.lines)
    }
}

#[cfg(test)]
async fn stream_reader<R: AsyncRead + Unpin>(
    reader: R,
    generation: u64,
    id: u64,
    results: &mpsc::Sender<ActionResult>,
) {
    let outcome = read_stream(
        reader,
        &LogOutput {
            results,
            generation,
            id,
            source: None,
        },
        &mut ResumeCursor::default(),
    )
    .await;
    let error = match outcome {
        StreamEnd::Eof => None,
        StreamEnd::Error(error) => Some(error.to_string()),
        StreamEnd::Closed => return,
    };
    let _ = send_batch(results, generation, id, Vec::new(), true, error).await;
}

enum StreamEnd {
    Eof,
    Error(io::Error),
    Closed,
}

async fn read_stream<R: AsyncRead + Unpin>(
    mut reader: R,
    output: &LogOutput<'_>,
    cursor: &mut ResumeCursor,
) -> StreamEnd {
    let mut buffer = LogBuffer::default();
    let mut chunk = [0_u8; 8 * 1024];
    let mut interval = tokio::time::interval(BATCH_INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            result = reader.read(&mut chunk) => {
                match result {
                    Ok(0) => {
                        if !buffer.partial.is_empty() || buffer.truncated {
                            buffer.finish_line(cursor);
                        }
                        if !output.send(buffer.take_lines()).await {
                            return StreamEnd::Closed;
                        }
                        return StreamEnd::Eof;
                    }
                    Ok(length) => {
                        for byte in &chunk[..length] {
                            buffer.push(*byte, cursor);
                            if buffer.full(cursor.source_count)
                                && !output.send(buffer.take_lines()).await
                            {
                                return StreamEnd::Closed;
                            }
                        }
                    }
                    Err(error) => {
                        // Only complete lines have an identity safe to resume.
                        // Discard a partial interrupted line and fetch it again.
                        if !output.send(buffer.take_lines()).await {
                            return StreamEnd::Closed;
                        }
                        return StreamEnd::Error(error);
                    }
                }
            }
            _ = interval.tick(), if !buffer.lines.is_empty() => {
                if !output.send(buffer.take_lines()).await {
                    return StreamEnd::Closed;
                }
            }
            () = output.results.closed() => return StreamEnd::Closed,
        }
    }
}

async fn send_batch(
    results: &mpsc::Sender<ActionResult>,
    generation: u64,
    id: u64,
    lines: Vec<LogLine>,
    finished: bool,
    error: Option<String>,
) -> bool {
    results
        .send(ActionResult::Scoped {
            generation,
            result: Box::new(ActionResult::LogBatch {
                id,
                lines,
                finished,
                error,
            }),
        })
        .await
        .is_ok()
}

async fn stream_demo(generation: u64, request: &LogRequest, results: &mpsc::Sender<ActionResult>) {
    let explicit = request.target.container.as_ref();
    let multi_pods = request.target.resource.kind.kind.as_ref() != "Pod";
    let service = request.target.resource.kind.kind.as_ref() == "Service";
    let all = explicit.is_none() && (service || request.all_containers);
    let containers = if let Some(container) = explicit {
        vec![container.as_str()]
    } else if all {
        vec!["app", "sidecar"]
    } else {
        vec!["app"]
    };
    let pods = if multi_pods {
        vec![
            format!("{}-pod-0", request.target.name()),
            format!("{}-pod-1", request.target.name()),
        ]
    } else {
        vec![request.target.name().to_owned()]
    };
    let sources: Vec<_> = pods
        .iter()
        .flat_map(|pod| {
            containers.iter().map(|container| LogSource {
                namespace: request.target.namespace().to_owned(),
                pod: pod.clone(),
                container: (*container).to_owned(),
                single_container: explicit.is_some(),
            })
        })
        .collect();
    let metadata = LogSessionMeta {
        default_container: (!service && explicit.is_none()).then(|| "app".to_owned()),
        selected_container: (!all).then(|| containers[0].to_owned()),
        all_containers: all,
        multi_pods,
        single_container: explicit.is_some(),
        source_count: sources.len(),
    };
    if !send_resolution(results, generation, request.id, metadata).await {
        return;
    }
    let mut fixtures: Vec<_> = sources
        .iter()
        .map(|source| {
            let mut per_source = request.clone();
            per_source.target = LogTarget::pod(
                &source.namespace,
                &source.pod,
                Some(source.container.clone()),
                request.target.previous,
            );
            demo_fixture(&per_source)
                .lines()
                .map(ToOwned::to_owned)
                .collect::<Vec<_>>()
                .into_iter()
        })
        .collect();
    let mut batch = Vec::new();
    loop {
        let mut progress = false;
        for (source, fixture) in sources.iter().zip(&mut fixtures) {
            if let Some(text) = fixture.next() {
                progress = true;
                batch.push(LogLine {
                    source: Some(source.clone()),
                    text,
                });
                if batch.len() >= MAX_BATCH_LINES
                    && !send_batch(
                        results,
                        generation,
                        request.id,
                        std::mem::take(&mut batch),
                        false,
                        None,
                    )
                    .await
                {
                    return;
                }
            }
        }
        if !progress {
            break;
        }
    }
    let _ = send_batch(results, generation, request.id, batch, true, None).await;
}

#[cfg(test)]
async fn stream_logs(
    client: Client,
    generation: u64,
    request: &LogRequest,
    results: &mpsc::Sender<ActionResult>,
    policy: RetryPolicy,
) {
    let source = LogSource {
        namespace: request.target.namespace().to_owned(),
        pod: request.target.name().to_owned(),
        container: request.target.container.clone().unwrap(),
        single_container: true,
    };
    let output = LogOutput {
        results,
        generation,
        id: request.id,
        source: Some(&source),
    };
    let error = stream_source(client, request, &output, policy, None, 1).await;
    let _ = send_batch(results, generation, request.id, Vec::new(), true, error).await;
}

fn demo_fixture(request: &LogRequest) -> String {
    let params = log_params(request);
    let container = request.target.container.as_deref().unwrap_or("app");
    let count: usize = 120;
    let since_start = params.since_seconds.map_or(0, |seconds| {
        count.saturating_sub(usize::try_from(seconds.max(0) / 10).unwrap_or(count))
    });
    let tail_start = params.tail_lines.map_or(0, |tail| {
        count.saturating_sub(usize::try_from(tail).unwrap_or(0))
    });
    let mut body = (since_start.max(tail_start)..count)
        .fold(String::new(), |mut body, index| {
            let level = match index % 12 {
                4 => "WARN",
                8 => "ERROR",
                10 => "DEBUG",
                _ => "INFO",
            };
            let session = if request.target.previous { "previous" } else { "current" };
            let _ = writeln!(
                body,
                "2026-09-07T10:{:02}:{:02}Z {level} {container} {session} log {index}: handled request for {}",
                index / 6,
                (index % 6) * 10,
                request.target.name(),
            );
            body
        });
    if let Some(limit) = params.limit_bytes {
        let mut end = usize::try_from(limit).unwrap_or(0).min(body.len());
        while !body.is_char_boundary(end) {
            end -= 1;
        }
        body.truncate(end);
    }
    body
}

#[cfg(test)]
mod tests {
    use super::*;

    enum HttpReply {
        Error,
        Disconnect,
        NotFound,
        Pod(&'static str),
        Logs(String, bool),
        Hold(bool),
    }

    async fn http_fixture(replies: Vec<HttpReply>) -> (Client, JoinHandle<Vec<String>>) {
        http_fixture_with_progress(replies, None).await
    }

    async fn http_fixture_with_progress(
        replies: Vec<HttpReply>,
        progress: Option<std::sync::Arc<tokio::sync::Notify>>,
    ) -> (Client, JoinHandle<Vec<String>>) {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let config = kube::Config::new(
            format!("http://{}", listener.local_addr().unwrap())
                .parse()
                .unwrap(),
        );
        let client = Client::try_from(config).unwrap();
        let server = tokio::spawn(async move {
            let mut requests = Vec::new();
            for reply in replies {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                let mut chunk = [0; 4096];
                while !bytes.windows(4).any(|window| window == b"\r\n\r\n") {
                    let length = socket.read(&mut chunk).await.unwrap();
                    assert_ne!(length, 0);
                    bytes.extend_from_slice(&chunk[..length]);
                    assert!(bytes.len() < 65_536);
                }
                requests.push(String::from_utf8(bytes).unwrap());
                if let Some(progress) = &progress
                    && matches!(&reply, HttpReply::Hold(_))
                {
                    progress.notify_one();
                }
                let (status, body) = match reply {
                    HttpReply::Error => (
                        "500 Internal Server Error",
                        r#"{"apiVersion":"v1","kind":"Status","status":"Failure","message":"temporary logs outage","reason":"InternalError","code":500}"#.to_owned(),
                    ),
                    HttpReply::Disconnect => continue,
                    HttpReply::NotFound => (
                        "404 Not Found",
                        r#"{"apiVersion":"v1","kind":"Status","status":"Failure","message":"Pod missing","reason":"NotFound","code":404}"#.to_owned(),
                    ),
                    HttpReply::Pod(body) => ("200 OK", body.to_owned()),
                    HttpReply::Logs(body, complete) => {
                        let mut wire = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{:x}\r\n{body}\r\n",
                            body.len()
                        );
                        if complete {
                            wire.push_str("0\r\n\r\n");
                        }
                        socket.write_all(wire.as_bytes()).await.unwrap();
                        continue;
                    }
                    HttpReply::Hold(send_logs) => {
                        if send_logs {
                            let body = "2026-09-07T10:00:00Z before cancellation\n";
                            let wire = format!(
                                "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{:x}\r\n{body}\r\n",
                                body.len()
                            );
                            socket.write_all(wire.as_bytes()).await.unwrap();
                        }
                        assert_eq!(socket.read(&mut chunk).await.unwrap(), 0);
                        continue;
                    }
                };
                let wire = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket.write_all(wire.as_bytes()).await.unwrap();
            }
            requests
        });
        (client, server)
    }

    const RUNNING_POD: &str = r#"{"apiVersion":"v1","kind":"Pod","metadata":{"name":"demo","namespace":"test","uid":"original-pod"},"spec":{"containers":[{"name":"app"}]},"status":{"phase":"Running"}}"#;

    fn fast_retries(attempts: usize) -> RetryPolicy {
        RetryPolicy {
            attempts,
            initial_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(2),
            jitter: false,
        }
    }

    async fn collected_logs(
        client: Client,
        request: LogRequest,
        policy: RetryPolicy,
    ) -> (Vec<String>, Vec<String>) {
        let (sender, mut receiver) = mpsc::channel(1);
        let id = request.id;
        let task = tokio::spawn(async move {
            stream_logs(client, 42, &request, &sender, policy).await;
        });
        let mut lines = Vec::new();
        let mut errors = Vec::new();
        let mut completions = 0;
        while let Some(result) = tokio::time::timeout(Duration::from_secs(3), receiver.recv())
            .await
            .expect("log task stalled")
        {
            let ActionResult::Scoped { generation, result } = result else {
                panic!("unscoped logs");
            };
            assert_eq!(generation, 42);
            let ActionResult::LogBatch {
                id: result_id,
                lines: batch,
                finished,
                error,
            } = *result
            else {
                panic!("not a log batch");
            };
            assert_eq!(result_id, id);
            assert!(batch.len() <= MAX_BATCH_LINES);
            assert_eq!(completions, 0, "no output after completion");
            lines.extend(batch.into_iter().map(|line| line.text));
            completions += usize::from(finished);
            if let Some(error) = error {
                assert!(finished, "transient failures stay internal");
                errors.push(error);
            }
        }
        task.await.unwrap();
        assert_eq!(completions, 1);
        (lines, errors)
    }

    fn request(range: LogRange, previous: bool) -> LogRequest {
        LogRequest {
            id: 7,
            target: LogTarget::pod("test", "demo", Some("app".to_owned()), previous),
            range,
            tail_count: crate::config::DEFAULT_LOG_TAIL,
            all_containers: false,
        }
    }

    #[test]
    fn log_ranges_match_k9s_request_semantics() {
        for previous in [false, true] {
            let tail = log_params(&request(LogRange::Tail, previous));
            assert_eq!(tail.tail_lines, Some(100));
            assert!(tail.follow);
            assert_eq!(tail.previous, previous);
            assert!(tail.timestamps);
            assert_eq!(tail.container.as_deref(), Some("app"));
            assert!(tail.since_seconds.is_none());
            assert!(tail.limit_bytes.is_none());

            let head = log_params(&request(LogRange::Head, previous));
            assert!(!head.follow);
            assert_eq!(head.limit_bytes, Some(5_000));
            assert!(head.tail_lines.is_none());
            assert!(head.since_seconds.is_none());

            for seconds in [60, 300, 900, 1_800, 3_600] {
                let timed = log_params(&request(LogRange::SinceSeconds(seconds), previous));
                assert!(timed.follow);
                assert_eq!(timed.tail_lines, Some(100));
                assert_eq!(timed.since_seconds, Some(seconds));
            }
        }
    }

    #[test]
    fn configured_tail_and_since_apply_to_live_and_previous_streams() {
        let settings = LoggerSettings {
            tail: 3,
            since_seconds: 60,
            show_time: false,
            ..LoggerSettings::default()
        };
        for previous in [false, true] {
            let mut configured =
                LogRequest::new(42, request(LogRange::Tail, previous).target, &settings);
            assert_eq!(configured.id, 42);
            assert_eq!(configured.range, LogRange::SinceSeconds(60));
            assert_eq!(configured.tail_count, 3);
            let params = log_params(&configured);
            assert_eq!(params.tail_lines, Some(3));
            assert_eq!(params.since_seconds, Some(60));
            assert!(
                params.timestamps,
                "timestamps remain available for local toggling"
            );
            assert!(params.follow);
            assert_eq!(demo_fixture(&configured).lines().count(), 3);

            configured.range = LogRange::Head;
            assert!(log_params(&configured).tail_lines.is_none());
            configured.range = LogRange::Tail;
            assert_eq!(log_params(&configured).tail_lines, Some(3));
            assert!(log_params(&configured).since_seconds.is_none());
        }
        for seconds in [-100, -1, 0] {
            assert_eq!(LogRange::from_since_seconds(seconds), LogRange::Tail);
        }
    }

    #[tokio::test]
    async fn retries_open_errors_with_original_identity_and_completes_on_clean_eof() {
        for previous in [false, true] {
            let (client, server) = http_fixture(vec![
                HttpReply::Error,
                HttpReply::Pod(RUNNING_POD),
                HttpReply::Logs("2026-09-07T10:00:00Z recovered\n".into(), true),
            ])
            .await;
            let (lines, errors) = collected_logs(
                client,
                request(LogRange::SinceSeconds(60), previous),
                fast_retries(3),
            )
            .await;
            assert_eq!(lines, ["2026-09-07T10:00:00Z recovered"]);
            assert_eq!(errors, Vec::<String>::new());
            let requests = server.await.unwrap();
            assert_eq!(requests.len(), 3);
            for wire in [&requests[0], &requests[2]] {
                let path = wire.lines().next().unwrap();
                assert!(path.starts_with("GET /api/v1/namespaces/test/pods/demo/log?"));
                assert!(path.contains("container=app"));
                assert!(path.contains("timestamps=true"));
                assert!(path.contains("tailLines=100"));
                assert!(path.contains("sinceSeconds=60"));
                assert_eq!(path.contains("previous=true"), previous);
                assert!(path.contains("follow=true"));
            }
            assert!(requests[1].starts_with("GET /api/v1/namespaces/test/pods/demo "));
        }
    }

    #[tokio::test]
    async fn interrupted_chunked_stream_resumes_without_repeated_or_missing_timestamp_peers() {
        let timestamp = "2026-09-07T10:00:00.123456789Z";
        let same = format!("{timestamp} repeated");
        let new = format!("{timestamp} new message");
        let later = "2026-09-07T10:00:00.223456789Z later";
        let (client, server) = http_fixture(vec![
            HttpReply::Logs(format!("{same}\n{same}\n{timestamp} inter"), false),
            HttpReply::Pod(RUNNING_POD),
            HttpReply::Logs(
                format!("2026-09-07T09:59:59Z old\n{same}\n{same}\n{same}\n{new}\n{timestamp} interrupted\n{later}\n"),
                true,
            ),
        ])
        .await;
        let (lines, errors) = collected_logs(
            client,
            request(LogRange::SinceSeconds(60), false),
            fast_retries(3),
        )
        .await;
        assert_eq!(errors, Vec::<String>::new());
        assert_eq!(
            lines,
            [
                same.clone(),
                same.clone(),
                same,
                new,
                format!("{timestamp} interrupted"),
                later.into()
            ]
        );
        let requests = server.await.unwrap();
        let resumed = requests[2].lines().next().unwrap();
        assert!(
            // kube's query encoder uses whole seconds; replay filtering must
            // retain new nanosecond peers while suppressing older responses.
            resumed.contains("sinceTime=2026-09-07T10%3A00%3A00Z"),
            "{resumed}"
        );
        assert!(!resumed.contains("sinceSeconds="));
        assert!(!resumed.contains("tailLines="));
        assert!(resumed.contains("follow=true"));
        assert!(resumed.contains("container=app"));
    }

    #[tokio::test]
    async fn transport_failure_retries_head_without_changing_its_byte_window() {
        let (client, server) = http_fixture(vec![
            HttpReply::Disconnect,
            HttpReply::Pod(RUNNING_POD),
            HttpReply::Logs("2026-09-07T10:00:00Z start\n".into(), false),
            HttpReply::Pod(RUNNING_POD),
            HttpReply::Logs(
                "2026-09-07T10:00:00Z start\n2026-09-07T10:00:01Z end".into(),
                true,
            ),
        ])
        .await;
        let (lines, errors) =
            collected_logs(client, request(LogRange::Head, true), fast_retries(3)).await;
        assert_eq!(
            lines,
            ["2026-09-07T10:00:00Z start", "2026-09-07T10:00:01Z end"]
        );
        assert_eq!(errors, Vec::<String>::new());
        let requests = server.await.unwrap();
        assert_eq!(requests.len(), 5);
        for request in [&requests[0], &requests[2], &requests[4]] {
            let path = request.lines().next().unwrap();
            assert!(path.contains("limitBytes=5000"));
            assert!(path.contains("previous=true"));
            assert!(path.contains("container=app"));
            assert!(!path.contains("follow=true"));
            assert!(!path.contains("tailLines="));
            assert!(!path.contains("sinceTime="));
        }
    }

    #[tokio::test]
    async fn retry_exhaustion_emits_one_scoped_error_and_terminal_pods_stop_early() {
        let (client, server) = http_fixture(vec![
            HttpReply::Error,
            HttpReply::Pod(RUNNING_POD),
            HttpReply::Error,
            HttpReply::Pod(RUNNING_POD),
            HttpReply::Error,
            HttpReply::Pod(RUNNING_POD),
        ])
        .await;
        let (lines, errors) =
            collected_logs(client, request(LogRange::Tail, false), fast_retries(3)).await;
        assert_eq!(lines, Vec::<String>::new());
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("after 3 attempts"));
        assert_eq!(server.await.unwrap().len(), 6);

        for pod in [
            r#"{"apiVersion":"v1","kind":"Pod","metadata":{"name":"demo"},"status":{"phase":"Succeeded"}}"#,
            r#"{"apiVersion":"v1","kind":"Pod","metadata":{"name":"demo","deletionTimestamp":"2026-09-07T10:00:00Z"},"status":{"phase":"Running"}}"#,
        ] {
            let (client, server) = http_fixture(vec![HttpReply::Error, HttpReply::Pod(pod)]).await;
            let (_, errors) =
                collected_logs(client, request(LogRange::Tail, false), fast_retries(3)).await;
            assert_eq!(errors.len(), 1);
            assert!(errors[0].contains("Pod"));
            assert!(!errors[0].contains("after 3 attempts"));
            assert_eq!(server.await.unwrap().len(), 2);
        }
        let (client, server) = http_fixture(vec![HttpReply::Error, HttpReply::NotFound]).await;
        let (_, errors) =
            collected_logs(client, request(LogRange::Tail, false), fast_retries(3)).await;
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("Pod no longer exists"));
        assert_eq!(server.await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn dropping_log_task_closes_opening_and_reading_http_connections() {
        for send_logs in [false, true] {
            let progress = std::sync::Arc::new(tokio::sync::Notify::new());
            let (client, server) = http_fixture_with_progress(
                vec![HttpReply::Pod(RUNNING_POD), HttpReply::Hold(send_logs)],
                Some(progress.clone()),
            )
            .await;
            let (sender, mut receiver) = mpsc::channel(1);
            let task = LogTask::spawn(
                client,
                ConnectionOptions::default(),
                42,
                request(LogRange::Tail, false),
                sender,
            );
            let resolved = receiver.recv().await.unwrap();
            assert!(
                matches!(resolved, ActionResult::Scoped { result, .. } if matches!(*result, ActionResult::LogResolved { .. }))
            );
            if send_logs {
                let _ = tokio::time::timeout(Duration::from_secs(3), receiver.recv())
                    .await
                    .unwrap()
                    .unwrap();
            } else {
                tokio::time::timeout(Duration::from_secs(3), progress.notified())
                    .await
                    .unwrap();
            }
            drop(task);
            let requests = tokio::time::timeout(Duration::from_secs(3), server)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                requests.len(),
                2,
                "cancellation closes the socket without reconnecting"
            );
            assert!(receiver.recv().await.is_none());
        }
    }

    #[tokio::test]
    async fn dropping_log_task_interrupts_backoff_without_final_error() {
        let (client, server) =
            http_fixture(vec![HttpReply::Error, HttpReply::Pod(RUNNING_POD)]).await;
        let (sender, mut receiver) = mpsc::channel(1);
        let task = LogTask {
            id: 7,
            task: tokio::spawn(async move {
                stream_logs(
                    client,
                    42,
                    &request(LogRange::Tail, false),
                    &sender,
                    RetryPolicy {
                        attempts: 20,
                        initial_delay: Duration::from_secs(60),
                        max_delay: Duration::from_secs(60),
                        jitter: false,
                    },
                )
                .await;
            }),
        };
        assert_eq!(server.await.unwrap().len(), 2);
        drop(task);
        assert!(
            tokio::time::timeout(Duration::from_secs(1), receiver.recv())
                .await
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn replay_tracking_is_bounded_and_new_timestamp_restores_resumability() {
        let mut cursor = ResumeCursor::default();
        for index in 0..=MAX_RESUME_LINES {
            assert!(cursor.accept(&format!("2026-09-07T10:00:00Z message {index}")));
        }
        assert_eq!(cursor.lines.len(), MAX_RESUME_LINES);
        assert!(cursor.bytes <= MAX_RESUME_BYTES);
        assert!(!cursor.resumable);
        assert!(cursor.accept("2026-09-07T10:00:01Z fresh timestamp"));
        assert!(cursor.resumable);
        assert_eq!(cursor.lines.len(), 1);
        assert!(cursor.accept("missing timestamp"));
        assert!(!cursor.resumable);
    }

    #[test]
    fn timestamp_regression_keeps_output_but_prevents_ambiguous_replay() {
        let mut cursor = ResumeCursor::default();
        assert!(cursor.accept("2026-09-07T10:00:01Z before clock regression"));
        let high_water = cursor.timestamp;
        assert!(cursor.accept("2026-09-07T10:00:00Z clock moved backwards"));
        assert_eq!(cursor.timestamp, high_water);
        assert!(!cursor.resumable);
        assert!(cursor.accept("2026-09-07T10:00:02Z clock recovered"));
        assert!(cursor.resumable);
        assert!(cursor.timestamp > high_water);
    }

    #[test]
    fn upstream_backoff_jitter_stays_within_half_to_one_and_a_half_intervals() {
        let policy = RetryPolicy::default();
        let random = RandomState::new();
        for attempt in 0..1_000 {
            let delay = policy.randomized_delay(policy.max_delay, attempt, &random);
            assert!(
                (Duration::from_millis(7_500)..=Duration::from_millis(22_500)).contains(&delay)
            );
        }
    }

    #[tokio::test]
    async fn streams_bounded_batches_without_losing_lines_or_final_partial_line() {
        let mut input = "first\r\n".to_owned();
        input.push_str(&"long".repeat(MAX_LINE_BYTES));
        write!(input, "\n{}last", "short\n".repeat(600)).unwrap();
        let (sender, mut receiver) = mpsc::channel(1);
        let task = tokio::spawn(async move {
            stream_reader(futures::io::Cursor::new(input), 42, 7, &sender).await;
        });
        let mut lines = Vec::new();
        let mut finished_count = 0;
        while let Some(result) = receiver.recv().await {
            let ActionResult::Scoped { generation, result } = result else {
                panic!("unscoped log result");
            };
            assert_eq!(generation, 42);
            let ActionResult::LogBatch {
                id,
                lines: batch,
                finished,
                error,
            } = *result
            else {
                panic!("non-log result");
            };
            assert_eq!(id, 7);
            assert!(batch.len() <= MAX_BATCH_LINES);
            assert!(error.is_none());
            lines.extend(batch.into_iter().map(|line| line.text));
            finished_count += usize::from(finished);
        }
        task.await.unwrap();
        assert_eq!(finished_count, 1);
        assert_eq!(lines.len(), 603);
        assert_eq!(lines[0], "first");
        assert!(lines[1].ends_with(TRUNCATED_LINE_SUFFIX));
        assert!(lines[1].len() <= MAX_LINE_BYTES + TRUNCATED_LINE_SUFFIX.len());
        assert_eq!(lines.last().unwrap(), "last");
    }

    #[tokio::test]
    async fn stopping_only_matching_session_aborts_blocked_stream() {
        let (sender, receiver) = mpsc::channel(1);
        let handle = tokio::spawn(async move {
            let input = "line\n".repeat(10_000);
            stream_reader(futures::io::Cursor::new(input), 1, 7, &sender).await;
        });
        let abort = handle.abort_handle();
        let mut active = Some(LogTask {
            id: 7,
            task: handle,
        });
        tokio::task::yield_now().await;
        assert_eq!(receiver.len(), 1, "the stream is blocked by backpressure");
        LogTask::stop_if_matching(&mut active, 6);
        assert!(active.is_some());
        assert!(!abort.is_finished());
        LogTask::stop_if_matching(&mut active, 7);
        assert!(active.is_none());
        tokio::task::yield_now().await;
        assert!(abort.is_finished());
    }

    #[test]
    fn demo_supports_head_tail_and_time_windows() {
        let tail = demo_fixture(&request(LogRange::Tail, false));
        assert_eq!(tail.lines().count(), 100);
        assert!(tail.contains("current log 20"));
        let head = demo_fixture(&request(LogRange::Head, false));
        assert_eq!(head.len(), 5_000);
        assert!(head.contains("current log 0"));
        let recent = demo_fixture(&request(LogRange::SinceSeconds(60), true));
        assert_eq!(recent.lines().count(), 6);
        assert!(recent.contains("previous log 114"));
    }
}
