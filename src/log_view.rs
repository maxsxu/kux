use std::{collections::VecDeque, sync::OnceLock};

use regex::Regex;

use crate::source::LogRequest;

pub const MAX_LOG_LINES: usize = 5_000;
pub const MAX_LOG_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug)]
#[allow(clippy::struct_excessive_bools)] // Independent controls and stream lifecycle indicators.
pub struct LogView {
    pub request: LogRequest,
    pub auto_scroll: bool,
    pub column_lock: bool,
    pub finished: bool,
    pub refresh_pending: bool,
    lines: VecDeque<String>,
    bytes: usize,
    total_lines: u64,
    displayed_lines: u64,
}

impl LogView {
    #[must_use]
    pub fn new(request: LogRequest) -> Self {
        Self {
            request,
            auto_scroll: true,
            column_lock: false,
            finished: false,
            refresh_pending: true,
            lines: VecDeque::new(),
            bytes: 0,
            total_lines: 0,
            displayed_lines: 0,
        }
    }

    pub fn append(&mut self, lines: &[String]) {
        for line in lines {
            self.total_lines += 1;
            self.bytes += line.len() + 1;
            self.lines.push_back(line.clone());
        }
        while self.lines.len() > MAX_LOG_LINES || self.bytes > MAX_LOG_BYTES {
            let Some(line) = self.lines.pop_front() else {
                break;
            };
            self.bytes -= line.len() + 1;
        }
    }

    pub fn clear(&mut self) {
        self.lines.clear();
        self.bytes = 0;
        self.total_lines = 0;
        self.displayed_lines = 0;
    }

    pub fn take_pending(&mut self) -> Vec<String> {
        let first_retained = self.total_lines.saturating_sub(self.lines.len() as u64);
        let skip = self.displayed_lines.saturating_sub(first_retained);
        let pending = self
            .lines
            .iter()
            .skip(usize::try_from(skip).unwrap_or(usize::MAX))
            .cloned()
            .collect();
        self.displayed_lines = self.total_lines;
        pending
    }

    #[must_use]
    pub fn text(&self) -> String {
        let mut text = String::with_capacity(self.bytes);
        for line in &self.lines {
            text.push_str(line);
            text.push('\n');
        }
        text
    }
}

pub fn append_display_lines(body: &mut String, lines: &[String]) {
    for line in lines {
        body.push_str(line);
        body.push('\n');
    }
    let mut retained_bytes = 0;
    for (retained_lines, line) in body.split_inclusive('\n').rev().enumerate() {
        if retained_lines == MAX_LOG_LINES || retained_bytes + line.len() > MAX_LOG_BYTES {
            break;
        }
        retained_bytes += line.len();
    }
    if retained_bytes < body.len() {
        body.drain(..body.len() - retained_bytes);
    }
}

/// Terminal styling/control sequences are not log text and must not reach exports.
#[must_use]
pub(crate) fn plain_log_line(line: &str) -> String {
    static ESCAPES: OnceLock<Regex> = OnceLock::new();
    let escapes = ESCAPES.get_or_init(|| {
        Regex::new(r"\x1b(?:\[[0-?]*[ -/]*[@-~]|\][^\x07\x1b]*(?:\x07|\x1b\\)|[@-_])")
            .expect("terminal escape pattern")
    });
    escapes
        .replace_all(line, "")
        .chars()
        .filter(|ch| !ch.is_control() || *ch == '\t')
        .collect()
}

#[must_use]
pub fn without_log_timestamp(line: &str) -> &str {
    let Some((prefix, message)) = line.split_once(' ') else {
        return line;
    };
    let bytes = prefix.as_bytes();
    if bytes.len() >= 20
        && bytes.get(4) == Some(&b'-')
        && bytes.get(7) == Some(&b'-')
        && bytes.get(10) == Some(&b'T')
        && bytes[..4].iter().all(u8::is_ascii_digit)
    {
        message
    } else {
        line
    }
}

/// Write a new, private screen dump; never replace a previous export.
///
/// # Errors
/// Returns an error if no state directory is configured or the dump cannot be written.
pub fn save_logs(context: &str, name: &str, body: &str) -> std::io::Result<std::path::PathBuf> {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(std::path::PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| {
            std::env::var_os("HOME").map(|home| std::path::PathBuf::from(home).join(".local/state"))
        })
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "No state directory configured",
            )
        })?;
    save_logs_in(&base.join("kux/screen-dumps"), context, name, body)
}

fn save_logs_in(
    base: &std::path::Path,
    context: &str,
    name: &str,
    body: &str,
) -> std::io::Result<std::path::PathBuf> {
    use std::io::Write;
    let sanitize = |value: &str| {
        let value: String = value
            .chars()
            .take(120)
            .map(|ch| {
                if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') {
                    ch
                } else {
                    '_'
                }
            })
            .collect();
        if value.is_empty() {
            "logs".to_owned()
        } else {
            value
        }
    };
    let directory = base.join(sanitize(context));
    std::fs::create_dir_all(&directory)?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(std::io::Error::other)?
        .as_nanos();
    let path = directory.join(format!("{}-{now}.log", sanitize(name)));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&path)?;
    file.write_all(body.as_bytes())?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::{LogRange, LogTarget};

    #[test]
    fn bounded_backlog_resumes_without_duplicates_or_old_lines() {
        let mut view = LogView::new(LogRequest {
            id: 1,
            range: LogRange::Tail,
            target: LogTarget {
                namespace: "ns".into(),
                pod: "pod".into(),
                container: None,
                previous: false,
            },
        });
        view.append(&["first".to_owned()]);
        assert_eq!(view.take_pending(), ["first"]);
        let lines: Vec<_> = (0..MAX_LOG_LINES + 10)
            .map(|index| format!("line-{index}"))
            .collect();
        view.append(&lines);
        let pending = view.take_pending();
        assert_eq!(pending.len(), MAX_LOG_LINES);
        assert_eq!(pending[0], "line-10");
        assert!(view.take_pending().is_empty());
        let mut display = "marker\n".to_owned();
        append_display_lines(&mut display, &pending);
        assert_eq!(display.lines().count(), MAX_LOG_LINES);
        let large = "🙂".repeat(64 * 1024);
        for _ in 0..20 {
            view.append(std::slice::from_ref(&large));
        }
        assert!(view.text().len() <= MAX_LOG_BYTES);
        append_display_lines(&mut display, &view.take_pending());
        assert!(display.len() <= MAX_LOG_BYTES);
    }

    #[test]
    fn log_export_creates_unique_private_files_with_sanitized_paths() {
        let directory = std::env::temp_dir().join(format!(
            "kux-log-export-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let first = save_logs_in(&directory, "../../context", "ns/pod", "example\n").unwrap();
        let second = save_logs_in(&directory, "../../context", "ns/pod", "second").unwrap();
        assert_ne!(first, second);
        assert!(first.starts_with(&directory));
        assert_eq!(std::fs::read_to_string(&first).unwrap(), "example\n");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&first).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        std::fs::remove_dir_all(directory).unwrap();
    }
}
