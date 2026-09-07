use std::{fmt::Write as _, io, time::Duration};

use futures::{AsyncRead, AsyncReadExt};
use k8s_openapi::api::core::v1::Pod;
use kube::{Api, Client, api::LogParams};
use tokio::{sync::mpsc, task::JoinHandle};

use super::ActionResult;

const BATCH_INTERVAL: Duration = Duration::from_millis(100);
const MAX_BATCH_LINES: usize = 256;
const MAX_BATCH_BYTES: usize = 256 * 1024;
const MAX_LINE_BYTES: usize = 64 * 1024;
const TRUNCATED_LINE_SUFFIX: &str = " … [line truncated at 64 KiB]";

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum LogRange {
    #[default]
    Tail,
    Head,
    SinceSeconds(i64),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogTarget {
    pub namespace: String,
    pub pod: String,
    pub container: Option<String>,
    pub previous: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogRequest {
    pub id: u64,
    pub target: LogTarget,
    pub range: LogRange,
}

/// Dropping the active stream also stops its HTTP request, including manager shutdown.
pub(super) struct LogTask {
    id: u64,
    task: JoinHandle<()>,
}

impl LogTask {
    pub(super) fn spawn(
        client: Client,
        generation: u64,
        request: LogRequest,
        results: mpsc::Sender<ActionResult>,
    ) -> Self {
        let id = request.id;
        let task = tokio::spawn(async move {
            let params = log_params(&request);
            let pods = Api::<Pod>::namespaced(client, &request.target.namespace);
            match pods.log_stream(&request.target.pod, &params).await {
                Ok(reader) => stream_reader(reader, generation, id, &results).await,
                Err(error) => {
                    let _ = send_batch(
                        &results,
                        generation,
                        id,
                        Vec::new(),
                        true,
                        Some(error.to_string()),
                    )
                    .await;
                }
            }
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
            let fixture = demo_fixture(&request);
            stream_reader(
                futures::io::Cursor::new(fixture.into_bytes()),
                generation,
                id,
                &results,
            )
            .await;
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

fn log_params(request: &LogRequest) -> LogParams {
    let mut params = LogParams {
        container: request.target.container.clone(),
        previous: request.target.previous,
        follow: !request.target.previous,
        timestamps: true,
        tail_lines: Some(100),
        ..LogParams::default()
    };
    match request.range {
        LogRange::Tail => {}
        LogRange::Head => {
            params.follow = false;
            params.tail_lines = None;
            params.limit_bytes = Some(5_000);
        }
        LogRange::SinceSeconds(seconds) => params.since_seconds = Some(seconds),
    }
    params
}

#[derive(Default)]
struct LogBuffer {
    partial: Vec<u8>,
    truncated: bool,
    lines: Vec<String>,
    bytes: usize,
}

impl LogBuffer {
    fn push(&mut self, byte: u8) {
        if byte == b'\n' {
            self.finish_line();
        } else if self.partial.len() < MAX_LINE_BYTES {
            self.partial.push(byte);
        } else {
            self.truncated = true;
        }
    }

    fn finish_line(&mut self) {
        if self.partial.last() == Some(&b'\r') {
            self.partial.pop();
        }
        let mut line = String::from_utf8_lossy(&self.partial).into_owned();
        if self.truncated {
            line.push_str(TRUNCATED_LINE_SUFFIX);
        }
        self.bytes += line.len();
        self.lines.push(line);
        self.partial.clear();
        self.truncated = false;
    }

    fn full(&self) -> bool {
        self.lines.len() >= MAX_BATCH_LINES || self.bytes >= MAX_BATCH_BYTES
    }

    fn take_lines(&mut self) -> Vec<String> {
        self.bytes = 0;
        std::mem::take(&mut self.lines)
    }
}

async fn stream_reader<R: AsyncRead + Unpin>(
    mut reader: R,
    generation: u64,
    id: u64,
    results: &mpsc::Sender<ActionResult>,
) {
    let mut buffer = LogBuffer::default();
    let mut chunk = [0_u8; 8 * 1024];
    let mut interval = tokio::time::interval(BATCH_INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            result = reader.read(&mut chunk) => {
                match result {
                    Ok(0) => {
                        finish_stream(&mut buffer, generation, id, results, None).await;
                        return;
                    }
                    Ok(length) => {
                        for byte in &chunk[..length] {
                            buffer.push(*byte);
                            if buffer.full()
                                && !send_batch(results, generation, id, buffer.take_lines(), false, None).await
                            {
                                return;
                            }
                        }
                    }
                    Err(error) => {
                        finish_stream(&mut buffer, generation, id, results, Some(error)).await;
                        return;
                    }
                }
            }
            _ = interval.tick(), if !buffer.lines.is_empty() => {
                if !send_batch(results, generation, id, buffer.take_lines(), false, None).await {
                    return;
                }
            }
            () = results.closed() => return,
        }
    }
}

async fn finish_stream(
    buffer: &mut LogBuffer,
    generation: u64,
    id: u64,
    results: &mpsc::Sender<ActionResult>,
    error: Option<io::Error>,
) {
    if !buffer.partial.is_empty() || buffer.truncated {
        buffer.finish_line();
    }
    let _ = send_batch(
        results,
        generation,
        id,
        buffer.take_lines(),
        true,
        error.map(|error| error.to_string()),
    )
    .await;
}

async fn send_batch(
    results: &mpsc::Sender<ActionResult>,
    generation: u64,
    id: u64,
    lines: Vec<String>,
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
                request.target.pod,
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

    fn request(range: LogRange, previous: bool) -> LogRequest {
        LogRequest {
            id: 7,
            target: LogTarget {
                namespace: "test".to_owned(),
                pod: "demo".to_owned(),
                container: Some("app".to_owned()),
                previous,
            },
            range,
        }
    }

    #[test]
    fn log_ranges_match_k9s_request_semantics() {
        for previous in [false, true] {
            let tail = log_params(&request(LogRange::Tail, previous));
            assert_eq!(tail.tail_lines, Some(100));
            assert_eq!(tail.follow, !previous);
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
                assert_eq!(timed.follow, !previous);
                assert_eq!(timed.tail_lines, Some(100));
                assert_eq!(timed.since_seconds, Some(seconds));
            }
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
            lines.extend(batch);
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
