use std::{collections::VecDeque, sync::OnceLock};

use regex::Regex;

use crate::{
    config::LoggerSettings,
    source::{LogLine, LogRequest, LogSessionMeta, LogSource},
};

pub const MAX_LOG_LINES: usize = crate::config::MAX_LOG_BUFFER_LINES;
pub const MAX_LOG_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug)]
#[allow(clippy::struct_excessive_bools)] // Independent controls and stream lifecycle indicators.
pub struct LogView {
    pub request: LogRequest,
    pub auto_scroll: bool,
    pub column_lock: bool,
    pub finished: bool,
    pub refresh_pending: bool,
    pub metadata: Option<LogSessionMeta>,
    buffer_size: usize,
    lines: VecDeque<LogLine>,
    display: VecDeque<DisplayedLogLine>,
    display_bytes: usize,
    display_text_bytes: usize,
    bytes: usize,
    total_lines: u64,
    displayed_lines: u64,
}

#[derive(Debug)]
struct DisplayedLogLine {
    source: Option<LogSource>,
    text_bytes: usize,
}

impl LogView {
    #[must_use]
    pub fn new(request: LogRequest) -> Self {
        Self::with_settings(request, &LoggerSettings::default())
    }

    #[must_use]
    pub fn with_settings(request: LogRequest, settings: &LoggerSettings) -> Self {
        let settings = settings.normalized();
        Self {
            request,
            auto_scroll: !settings.disable_autoscroll,
            column_lock: settings.column_lock,
            finished: false,
            refresh_pending: true,
            metadata: None,
            buffer_size: settings.buffer,
            lines: VecDeque::new(),
            display: VecDeque::new(),
            display_bytes: 0,
            display_text_bytes: 0,
            bytes: 0,
            total_lines: 0,
            displayed_lines: 0,
        }
    }

    #[must_use]
    pub const fn buffer_size(&self) -> usize {
        self.buffer_size
    }

    pub fn append(&mut self, lines: &[LogLine]) {
        for line in lines {
            self.total_lines += 1;
            self.bytes += retained_bytes(line);
            self.lines.push_back(line.clone());
            while self.lines.len() > self.buffer_size || self.bytes > MAX_LOG_BYTES {
                let Some(line) = self.lines.pop_front() else {
                    break;
                };
                self.bytes -= retained_bytes(&line);
            }
        }
    }

    pub fn clear(&mut self) {
        self.lines.clear();
        self.bytes = 0;
        self.total_lines = 0;
        self.displayed_lines = 0;
        self.display.clear();
        self.display_bytes = 0;
        self.display_text_bytes = 0;
    }

    pub fn take_pending(&mut self) -> Vec<LogLine> {
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

    pub fn show_pending(&mut self, body: &mut String) {
        let pending = self.take_pending();
        self.append_display(body, &pending);
    }

    pub fn insert_marker(&mut self, body: &mut String, marker: String) {
        self.append_display(body, &[marker.into()]);
    }

    fn append_display(&mut self, body: &mut String, lines: &[LogLine]) {
        if self.display_text_bytes != body.len() {
            self.display = body
                .split_inclusive('\n')
                .map(|line| DisplayedLogLine {
                    source: None,
                    text_bytes: line.len(),
                })
                .collect();
            self.display_bytes = body.len();
            self.display_text_bytes = body.len();
        }
        for line in lines {
            let formatted = format_line(line, self.metadata.as_ref());
            body.push_str(&formatted);
            body.push('\n');
            let text_bytes = formatted.len() + 1;
            self.display.push_back(DisplayedLogLine {
                source: line.source.clone(),
                text_bytes,
            });
            self.display_bytes += text_bytes + source_bytes(line.source.as_ref());
            self.display_text_bytes += text_bytes;
        }
        let mut removed_text = 0;
        while self.display.len() > self.buffer_size || self.display_bytes > MAX_LOG_BYTES {
            let Some(line) = self.display.pop_front() else {
                break;
            };
            removed_text += line.text_bytes;
            self.display_bytes -= line.text_bytes + source_bytes(line.source.as_ref());
            self.display_text_bytes -= line.text_bytes;
        }
        if removed_text > 0 {
            body.drain(..removed_text);
        }
    }

    #[must_use]
    pub fn displayed_source(&self, index: usize) -> Option<&LogSource> {
        self.display
            .get(index)
            .and_then(|line| line.source.as_ref())
    }

    #[must_use]
    pub fn without_timestamp<'a>(&self, index: usize, line: &'a str) -> &'a str {
        if self.displayed_source(index).is_some() && self.metadata.is_some() {
            timestamp_parts(line).map_or(line, |(timestamp, _)| {
                line.get(timestamp.len().max(30).saturating_add(1)..)
                    .unwrap_or(line)
            })
        } else {
            without_log_timestamp(line)
        }
    }

    #[must_use]
    pub fn text(&self) -> String {
        let mut text = String::with_capacity(self.bytes);
        for line in &self.lines {
            text.push_str(&line.text);
            text.push('\n');
        }
        text
    }
}

pub fn append_display_lines(body: &mut String, lines: &[String], max_lines: usize) {
    let max_lines = LoggerSettings {
        buffer: max_lines,
        ..LoggerSettings::default()
    }
    .normalized()
    .buffer;
    for line in lines {
        body.push_str(line);
        body.push('\n');
    }
    let mut retained_bytes = 0;
    for (retained_lines, line) in body.split_inclusive('\n').rev().enumerate() {
        if retained_lines == max_lines || retained_bytes + line.len() > MAX_LOG_BYTES {
            break;
        }
        retained_bytes += line.len();
    }
    if retained_bytes < body.len() {
        body.drain(..body.len() - retained_bytes);
    }
}

fn retained_bytes(line: &LogLine) -> usize {
    line.text.len() + 1 + source_bytes(line.source.as_ref())
}

fn source_bytes(source: Option<&LogSource>) -> usize {
    source.map_or(0, |source| {
        source.namespace.len() + source.pod.len() + source.container.len()
    })
}

fn format_line(line: &LogLine, metadata: Option<&LogSessionMeta>) -> String {
    let (Some(source), Some(metadata)) = (line.source.as_ref(), metadata) else {
        return line.text.clone();
    };
    let mut formatted =
        String::with_capacity(line.text.len() + source.pod.len() + source.container.len() + 32);
    let message = timestamp_parts(&line.text).map_or(line.text.as_str(), |(timestamp, message)| {
        use std::fmt::Write as _;
        let _ = write!(formatted, "{timestamp:<30} ");
        message
    });
    if metadata.multi_pods {
        formatted.push_str(&source.pod);
        formatted.push(' ');
    }
    if !source.single_container {
        formatted.push_str(&source.container);
        formatted.push(' ');
    }
    formatted.push_str(message);
    formatted
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
    timestamp_parts(line).map_or(line, |(_, message)| message)
}

fn timestamp_parts(line: &str) -> Option<(&str, &str)> {
    let (prefix, message) = line.split_once(' ')?;
    let bytes = prefix.as_bytes();
    if bytes.len() >= 20
        && bytes.get(4) == Some(&b'-')
        && bytes.get(7) == Some(&b'-')
        && bytes.get(10) == Some(&b'T')
        && bytes[..4].iter().all(u8::is_ascii_digit)
    {
        Some((prefix, message))
    } else {
        None
    }
}

/// Write a new, private screen dump; never replace a previous export.
///
/// # Errors
/// Returns an error if no state directory is configured or the dump cannot be written.
pub fn save_logs(context: &str, name: &str, body: &str) -> std::io::Result<std::path::PathBuf> {
    save_logs_in(&crate::screen_dumps::default_root()?, context, name, body)
}

fn save_logs_in(
    base: &std::path::Path,
    context: &str,
    name: &str,
    body: &str,
) -> std::io::Result<std::path::PathBuf> {
    crate::screen_dumps::ScreenDumpStore::new(base.to_owned(), None, context)?.save(
        name,
        body,
        crate::screen_dumps::DumpFormat::Logs,
    )
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
            tail_count: crate::config::DEFAULT_LOG_TAIL,
            target: LogTarget::pod("ns", "pod", None, false),
            all_containers: false,
        });
        view.append(&["first".into()]);
        assert_eq!(view.take_pending(), [LogLine::from("first")]);
        let lines: Vec<_> = (0..MAX_LOG_LINES + 10)
            .map(|index| format!("line-{index}").into())
            .collect();
        view.append(&lines);
        let pending = view.take_pending();
        assert_eq!(pending.len(), MAX_LOG_LINES);
        assert_eq!(pending[0].text, "line-10");
        assert_eq!(view.take_pending().len(), 0);
        let mut display = "marker\n".to_owned();
        view.append_display(&mut display, &pending);
        assert_eq!(display.lines().count(), MAX_LOG_LINES);
        let large: LogLine = "🙂".repeat(64 * 1024).into();
        for _ in 0..20 {
            view.append(std::slice::from_ref(&large));
        }
        assert!(view.text().len() <= MAX_LOG_BYTES);
        view.show_pending(&mut display);
        assert!(display.len() <= MAX_LOG_BYTES);
    }

    #[test]
    fn configured_buffer_bounds_backlog_display_and_resume_with_same_capacity() {
        let settings = LoggerSettings {
            buffer: 3,
            disable_autoscroll: true,
            column_lock: true,
            ..LoggerSettings::default()
        };
        let request = LogRequest::new(7, LogTarget::pod("ns", "pod", None, false), &settings);
        let mut view = LogView::with_settings(request, &settings);
        assert!(!view.auto_scroll);
        assert!(view.column_lock);
        assert_eq!(view.buffer_size(), 3);
        view.append(&["one".into(), "two".into()]);
        let mut display = String::new();
        view.show_pending(&mut display);
        view.append(&["three".into(), "four".into(), "five".into(), "six".into()]);
        assert_eq!(view.text(), "four\nfive\nsix\n");
        view.show_pending(&mut display);
        assert_eq!(display, "four\nfive\nsix\n");
        assert_eq!(view.take_pending().len(), 0);
        view.insert_marker(&mut display, "mark".to_owned());
        assert_eq!(display, "five\nsix\nmark\n");
        view.clear();
        assert_eq!(view.buffer_size(), 3);
        assert!(!view.auto_scroll);
        assert!(view.column_lock);
        assert_eq!(view.text().len(), 0);
        view.append(&["new".into()]);
        assert_eq!(view.take_pending(), [LogLine::from("new")]);
    }

    #[test]
    fn displayed_identity_and_text_share_one_byte_budget_across_batches() {
        let mut view = LogView::new(LogRequest::new(
            9,
            LogTarget::pod("ns", "pod", None, false),
            &LoggerSettings::default(),
        ));
        view.metadata = Some(LogSessionMeta {
            default_container: None,
            selected_container: Some("app".to_owned()),
            all_containers: true,
            multi_pods: false,
            single_container: true,
            source_count: 1,
        });
        let mut body = String::new();
        for index in 0..12 {
            view.append(&[LogLine {
                source: Some(LogSource {
                    namespace: "n".repeat(MAX_LOG_BYTES / 4),
                    pod: "pod".to_owned(),
                    container: "app".to_owned(),
                    single_container: true,
                }),
                text: format!("line-{index}"),
            }]);
            view.show_pending(&mut body);
            assert!(view.bytes <= MAX_LOG_BYTES);
            assert!(view.display_bytes <= MAX_LOG_BYTES);
            assert_eq!(view.display.len(), body.lines().count());
            assert_eq!(view.display_text_bytes, body.len());
        }
        assert_eq!(body, "line-9\nline-10\nline-11\n");
        assert_eq!(view.displayed_source(0).unwrap().pod, "pod");
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
