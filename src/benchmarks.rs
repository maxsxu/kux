//! HTTP load generator and benchmark report storage (`:benchmarks` / `:be` and `b` on Services and Port Forwards).

use std::{
    collections::BTreeMap,
    fmt::Write as _,
    fs,
    io::{self, BufRead, BufReader, Read, Write},
    net::TcpStream,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use tokio::sync::mpsc;

use crate::config::BenchConfig;

const BENCH_TIMEOUT: Duration = Duration::from_secs(120);
const SOCKET_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BenchmarkRunRequest {
    pub cluster: Option<String>,
    pub context: String,
    pub namespace: String,
    pub target_name: String,
    pub base_url: String,
    pub config: BenchConfig,
}

#[derive(Debug, Clone)]
pub enum BenchmarkEvent {
    Completed { target: String, path: PathBuf },
    Canceled { target: String },
    Failed { target: String, error: String },
}

#[derive(Debug)]
pub struct BenchmarkManager {
    active_cancel: Option<(String, Arc<AtomicBool>)>,
    events_tx: mpsc::Sender<BenchmarkEvent>,
}

impl BenchmarkManager {
    #[must_use]
    pub fn new(events_tx: mpsc::Sender<BenchmarkEvent>) -> Self {
        Self {
            active_cancel: None,
            events_tx,
        }
    }

    #[must_use]
    pub fn is_running(&self) -> bool {
        self.active_cancel.is_some()
    }

    /// Toggle benchmark: if a benchmark is already running, cancel it and return `Ok(None)`.
    /// Otherwise start a new benchmark and return `Ok(Some(target_label))`.
    ///
    /// # Errors
    /// Returns an error if the benchmark directory cannot be resolved.
    pub fn toggle_or_start(
        &mut self,
        request: BenchmarkRunRequest,
    ) -> Result<Option<String>, String> {
        if let Some((target, cancel)) = self.active_cancel.take() {
            cancel.store(true, Ordering::SeqCst);
            let tx = self.events_tx.clone();
            tokio::spawn(async move {
                let _ = tx.send(BenchmarkEvent::Canceled { target }).await;
            });
            return Ok(None);
        }
        let dir = benchmarks_dir(request.cluster.as_deref(), &request.context)
            .map_err(|error| error.to_string())?;
        let target_label = format!("{}/{}", request.namespace, request.target_name);
        let cancel = Arc::new(AtomicBool::new(false));
        self.active_cancel = Some((target_label.clone(), Arc::clone(&cancel)));
        let tx = self.events_tx.clone();
        let target = target_label.clone();
        tokio::spawn(async move {
            let result = tokio::task::spawn_blocking(move || {
                execute_and_save_benchmark(&dir, &request, &cancel)
            })
            .await;
            match result {
                Ok(Ok(Some(path))) => {
                    let _ = tx.send(BenchmarkEvent::Completed { target, path }).await;
                }
                Ok(Ok(None)) => {}
                Ok(Err(error)) => {
                    let _ = tx.send(BenchmarkEvent::Failed { target, error }).await;
                }
                Err(error) => {
                    let _ = tx
                        .send(BenchmarkEvent::Failed {
                            target,
                            error: format!("Benchmark worker panicked: {error}"),
                        })
                        .await;
                }
            }
        });
        Ok(Some(target_label))
    }

    pub fn handle_event(&mut self, event: BenchmarkEvent) -> String {
        if let BenchmarkEvent::Completed { ref target, .. }
        | BenchmarkEvent::Canceled { ref target }
        | BenchmarkEvent::Failed { ref target, .. } = event
            && self
                .active_cancel
                .as_ref()
                .is_some_and(|(active, _)| active == target)
        {
            self.active_cancel = None;
        }
        match event {
            BenchmarkEvent::Completed { target, path } => format!(
                "Benchmark {target} completed ({})",
                path.file_name().and_then(|f| f.to_str()).unwrap_or("saved")
            ),
            BenchmarkEvent::Canceled { target } => format!("Benchmark {target} canceled"),
            BenchmarkEvent::Failed { target, error } => {
                format!("Benchmark {target} failed: {error}")
            }
        }
    }
}

/// Resolve the context-scoped benchmark directory (`<state>/kux/benchmarks/<cluster>/<context>`).
///
/// # Errors
/// Returns an error when no absolute state or home directory can be resolved.
pub fn benchmarks_dir(cluster: Option<&str>, context: &str) -> io::Result<PathBuf> {
    if let Some(override_dir) = std::env::var_os("KUX_BENCHMARKS_DIR")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
    {
        return Ok(override_dir
            .join(sanitize_bench_component(cluster.unwrap_or("default")))
            .join(sanitize_bench_component(context)));
    }
    let state = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .filter(|path| path.is_absolute())
                .map(|home| home.join(".local/state"))
        })
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "No absolute state directory configured; set XDG_STATE_HOME or HOME",
            )
        })?;
    Ok(state
        .join("kux/benchmarks")
        .join(sanitize_bench_component(cluster.unwrap_or("default")))
        .join(sanitize_bench_component(context)))
}

#[must_use]
pub fn sanitize_bench_component(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return "default".to_owned();
    }
    trimmed
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '.' | '_') {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

#[must_use]
pub fn sanitize_bench_target(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut last_underscore = false;
    for ch in value.chars() {
        if matches!(ch, ':' | '|' | '/') {
            if !last_underscore {
                out.push('_');
                last_underscore = true;
            }
        } else {
            out.push(ch);
            last_underscore = ch == '_';
        }
    }
    out
}

fn execute_and_save_benchmark(
    dir: &Path,
    request: &BenchmarkRunRequest,
    cancel: &AtomicBool,
) -> Result<Option<PathBuf>, String> {
    let report = run_http_benchmark(&request.base_url, &request.config, cancel)?;
    if cancel.load(Ordering::SeqCst) && report.is_empty() {
        return Ok(None);
    }
    fs::create_dir_all(dir).map_err(|error| {
        format!(
            "Unable to create benchmark directory {}: {error}",
            dir.display()
        )
    })?;
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let file_name = format!(
        "{}_{}_{}.txt",
        sanitize_bench_target(&request.namespace),
        sanitize_bench_target(&request.target_name),
        nanos
    );
    let path = dir.join(file_name);
    let mut file = fs::File::create(&path).map_err(|error| {
        format!(
            "Unable to create benchmark report {}: {error}",
            path.display()
        )
    })?;
    file.write_all(report.as_bytes()).map_err(|error| {
        format!(
            "Unable to write benchmark report {}: {error}",
            path.display()
        )
    })?;
    Ok(Some(path))
}

#[derive(Debug, Clone)]
struct ParsedTargetUrl {
    host_header: String,
    connect_addr: String,
    path_and_query: String,
}

fn parse_target_url(
    base_url: &str,
    configured_path: &str,
    configured_host: &str,
) -> Result<ParsedTargetUrl, String> {
    let rest = base_url
        .trim()
        .strip_prefix("http://")
        .or_else(|| base_url.trim().strip_prefix("https://"))
        .unwrap_or(base_url.trim());
    let (authority, base_path) = match rest.split_once('/') {
        Some((auth, p)) => (auth, format!("/{p}")),
        None => (rest, String::new()),
    };
    if authority.is_empty() {
        return Err(format!("Invalid benchmark URL: {base_url}"));
    }
    let connect_addr = if authority.contains(':') {
        authority.to_owned()
    } else {
        format!("{authority}:80")
    };
    let suffix = if configured_path.trim().is_empty() {
        "/".to_owned()
    } else if configured_path.starts_with('/') {
        configured_path.to_owned()
    } else {
        format!("/{configured_path}")
    };
    let path_and_query = if base_path.is_empty() || base_path == "/" {
        suffix
    } else if suffix == "/" {
        base_path
    } else {
        format!("{}{}", base_path.trim_end_matches('/'), suffix)
    };
    let host_header = if configured_host.trim().is_empty() {
        authority.to_owned()
    } else {
        configured_host.trim().to_owned()
    };
    Ok(ParsedTargetUrl {
        host_header,
        connect_addr,
        path_and_query,
    })
}

/// Execute a bounded HTTP benchmark against `base_url` and return a hey-compatible report.
///
/// # Errors
/// Returns an error if `base_url` cannot be parsed into a valid HTTP target.
pub fn run_http_benchmark(
    base_url: &str,
    config: &BenchConfig,
    cancel: &AtomicBool,
) -> Result<String, String> {
    let parsed = parse_target_url(base_url, &config.http.path, &config.http.host)?;
    let total_requests = config.requests.max(1);
    let concurrency = config.concurrency.max(1).min(total_requests);
    let request_bytes = Arc::new(build_http_request_bytes(&parsed, config));

    let started = Instant::now();
    let next_index = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut handles = Vec::with_capacity(concurrency);

    for _ in 0..concurrency {
        let addr = parsed.connect_addr.clone();
        let req = Arc::clone(&request_bytes);
        let counter = Arc::clone(&next_index);
        let cancel_ref = cancel.load(Ordering::SeqCst);
        if cancel_ref {
            break;
        }
        handles.push(std::thread::spawn(move || {
            let mut latencies = Vec::new();
            let mut statuses: BTreeMap<u16, usize> = BTreeMap::new();
            let mut errors: BTreeMap<String, usize> = BTreeMap::new();
            let mut total_bytes = 0_usize;
            while counter.fetch_add(1, Ordering::SeqCst) < total_requests {
                if started.elapsed() > BENCH_TIMEOUT {
                    *errors
                        .entry("benchmark timeout exceeded".to_owned())
                        .or_insert(0) += 1;
                    break;
                }
                let req_start = Instant::now();
                match send_single_http_request(&addr, &req) {
                    Ok((status, bytes)) => {
                        latencies.push(req_start.elapsed().as_secs_f64());
                        *statuses.entry(status).or_insert(0) += 1;
                        total_bytes += bytes;
                    }
                    Err(err) => {
                        latencies.push(req_start.elapsed().as_secs_f64());
                        *errors.entry(err).or_insert(0) += 1;
                    }
                }
            }
            (latencies, statuses, errors, total_bytes)
        }));
    }

    let mut all_latencies = Vec::with_capacity(total_requests);
    let mut status_counts: BTreeMap<u16, usize> = BTreeMap::new();
    let mut error_counts: BTreeMap<String, usize> = BTreeMap::new();
    let mut total_data = 0_usize;

    for handle in handles {
        if let Ok((latencies, statuses, errors, bytes)) = handle.join() {
            all_latencies.extend(latencies);
            for (code, count) in statuses {
                *status_counts.entry(code).or_insert(0) += count;
            }
            for (err, count) in errors {
                *error_counts.entry(err).or_insert(0) += count;
            }
            total_data += bytes;
        }
    }

    let elapsed = started.elapsed().as_secs_f64().max(0.000_1);
    Ok(format_hey_report(
        elapsed,
        &all_latencies,
        &status_counts,
        &error_counts,
        total_data,
    ))
}

fn build_http_request_bytes(parsed: &ParsedTargetUrl, config: &BenchConfig) -> Vec<u8> {
    let method = if config.http.method.trim().is_empty() {
        "GET"
    } else {
        config.http.method.trim()
    };
    let body_bytes = config.http.body.as_bytes();
    let mut raw = String::new();
    let _ = write!(
        raw,
        "{method} {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n",
        parsed.path_and_query, parsed.host_header
    );
    let mut has_ua = false;
    for (name, values) in &config.http.headers {
        if name.eq_ignore_ascii_case("user-agent") {
            has_ua = true;
            let joined = values.join(", ");
            let _ = write!(
                raw,
                "User-Agent: {joined} k9s/{}\r\n",
                env!("CARGO_PKG_VERSION")
            );
        } else {
            for val in values {
                let _ = write!(raw, "{name}: {val}\r\n");
            }
        }
    }
    if !has_ua {
        let _ = write!(raw, "User-Agent: k9s/{}\r\n", env!("CARGO_PKG_VERSION"));
    }
    if !config.auth.user.is_empty() || !config.auth.password.is_empty() {
        let token = BASE64.encode(format!("{}:{}", config.auth.user, config.auth.password));
        let _ = write!(raw, "Authorization: Basic {token}\r\n");
    }
    if !body_bytes.is_empty() {
        let _ = write!(raw, "Content-Length: {}\r\n", body_bytes.len());
    }
    raw.push_str("\r\n");
    let mut out = raw.into_bytes();
    out.extend_from_slice(body_bytes);
    out
}

fn send_single_http_request(addr: &str, request_bytes: &[u8]) -> Result<(u16, usize), String> {
    use std::net::ToSocketAddrs;
    let socket_addr = addr
        .to_socket_addrs()
        .map_err(|error| format!("resolve {addr}: {error}"))?
        .next()
        .ok_or_else(|| format!("no socket address resolved for {addr}"))?;
    let mut stream = TcpStream::connect_timeout(&socket_addr, SOCKET_TIMEOUT)
        .map_err(|error| format!("connect {addr}: {error}"))?;
    let _ = stream.set_read_timeout(Some(SOCKET_TIMEOUT));
    let _ = stream.set_write_timeout(Some(SOCKET_TIMEOUT));
    stream
        .write_all(request_bytes)
        .map_err(|error| format!("write request: {error}"))?;

    let mut reader = BufReader::new(stream);
    let mut status_line = String::new();
    reader
        .read_line(&mut status_line)
        .map_err(|error| format!("read response line: {error}"))?;
    let status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| format!("invalid HTTP status line: {}", status_line.trim()))?;
    let mut body = Vec::new();
    let _ = reader.read_to_end(&mut body);
    Ok((status, body.len()))
}

#[must_use]
pub fn format_hey_report(
    total_secs: f64,
    latencies: &[f64],
    status_counts: &BTreeMap<u16, usize>,
    error_counts: &BTreeMap<String, usize>,
    total_data: usize,
) -> String {
    let mut sorted = latencies.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let slowest = sorted.last().copied().unwrap_or(0.0);
    let fastest = sorted.first().copied().unwrap_or(0.0);
    let count_f64 = f64::from(u32::try_from(sorted.len()).unwrap_or(u32::MAX));
    let average = if sorted.is_empty() {
        0.0
    } else {
        sorted.iter().sum::<f64>() / count_f64
    };
    let rps = if total_secs > 0.0 {
        count_f64 / total_secs
    } else {
        0.0
    };
    let size_per_req = if sorted.is_empty() {
        0
    } else {
        total_data / sorted.len()
    };

    let mut out = String::new();
    let _ = writeln!(out, "\nSummary:");
    let _ = writeln!(out, "  Total:\t{total_secs:.4} secs");
    let _ = writeln!(out, "  Slowest:\t{slowest:.4} secs");
    let _ = writeln!(out, "  Fastest:\t{fastest:.4} secs");
    let _ = writeln!(out, "  Average:\t{average:.4} secs");
    let _ = writeln!(out, "  Requests/sec:\t{rps:.4}");
    let _ = writeln!(out, "\n  Total data:\t{total_data} bytes");
    let _ = writeln!(out, "  Size/request:\t{size_per_req} bytes");
    let _ = writeln!(out, "\nStatus code distribution:");
    for (code, count) in status_counts {
        let _ = writeln!(out, "  [{code}]\t{count} responses");
    }
    if !error_counts.is_empty() {
        let _ = writeln!(out, "\nError distribution:");
        for (err, count) in error_counts {
            let _ = writeln!(out, "  [{count}]\t{err}");
        }
    }
    out
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedBenchmarkMetrics {
    pub status: String,
    pub total_time: String,
    pub req_per_sec: String,
    pub ok_2xx: String,
    pub err_4xx_5xx: String,
}

#[must_use]
pub fn parse_benchmark_report(data: &str) -> ParsedBenchmarkMetrics {
    let mut total_time = "n/a".to_owned();
    let mut req_per_sec = "0".to_owned();
    let mut ok_count = 0_u64;
    let mut err_count = 0_u64;
    let mut has_errors = data.contains("Error distribution");

    for line in data.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("Total:") {
            if let Some(secs) = rest.split_whitespace().next() {
                secs.clone_into(&mut total_time);
            }
        } else if let Some(rest) = trimmed.strip_prefix("Requests/sec:") {
            if let Some(rps) = rest.split_whitespace().next() {
                req_per_sec = format_rps_integer(rps);
            }
        } else if trimmed.starts_with('[')
            && let Some((bracket, after)) = trimmed[1..].split_once(']')
            && bracket.len() == 3
            && bracket.chars().all(|c| c.is_ascii_digit())
            && let Some(count_str) = after.split_whitespace().next()
            && after.contains("responses")
            && let Ok(count) = count_str.parse::<u64>()
        {
            if bracket.starts_with('2') {
                ok_count += count;
            } else if bracket.starts_with('4') || bracket.starts_with('5') {
                err_count += count;
            }
        }
    }
    if total_time == "n/a" && ok_count == 0 && err_count == 0 {
        has_errors = true;
    }
    ParsedBenchmarkMetrics {
        status: if has_errors {
            "fail".to_owned()
        } else {
            "pass".to_owned()
        },
        total_time,
        req_per_sec,
        ok_2xx: ok_count.to_string(),
        err_4xx_5xx: err_count.to_string(),
    }
}

fn format_rps_integer(rps: &str) -> String {
    rps.parse::<f64>()
        .map_or_else(|_| "0".to_owned(), |v| format!("{v:.0}"))
}

#[cfg(test)]
mod tests {
    use std::net::TcpListener;

    use super::*;

    #[test]
    fn parses_hey_report_metrics_and_runs_local_http_benchmark() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            for index in 0..4 {
                if let Ok((mut stream, _)) = listener.accept() {
                    let mut buf = [0_u8; 512];
                    let _ = stream.read(&mut buf);
                    let req_text = String::from_utf8_lossy(&buf);
                    assert!(req_text.contains("Authorization: Basic dXNlcjpzZWNyZXQ="));
                    let code = if index == 3 {
                        "500 Internal Server Error"
                    } else {
                        "200 OK"
                    };
                    let resp = format!(
                        "HTTP/1.1 {code}\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok"
                    );
                    let _ = stream.write_all(resp.as_bytes());
                }
            }
        });

        let mut cfg = BenchConfig {
            concurrency: 2,
            requests: 4,
            ..BenchConfig::default()
        };
        cfg.auth.user = "user".to_owned();
        cfg.auth.password = "secret".to_owned();
        cfg.http.path = "/healthz".to_owned();

        let cancel = AtomicBool::new(false);
        let report = run_http_benchmark(&format!("http://{addr}"), &cfg, &cancel).unwrap();
        server.join().unwrap();

        assert!(!report.contains("secret"));
        let metrics = parse_benchmark_report(&report);
        assert_eq!(metrics.status, "pass");
        assert_eq!(metrics.ok_2xx, "3");
        assert_eq!(metrics.err_4xx_5xx, "1");
        assert_ne!(metrics.total_time, "n/a");
    }
}
