use super::{
    App, ViewColumn, column_value, columns_for_view_options, computed_column_value,
    format_kubectl_age, table_column_value,
};
use crate::{log_view::plain_log_line, model::ResourceRow};
use std::{
    io::{self, Write},
    sync::Arc,
};

/// Immutable, inexpensive capture of the current filtered/sorted table. Formatting
/// happens on the file worker, so exporting a large table does not freeze rendering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableExportSnapshot {
    rows: Arc<Vec<Arc<ResourceRow>>>,
    indices: Vec<usize>,
    columns: Vec<ViewColumn>,
    now_unix: i64,
}

impl TableExportSnapshot {
    #[must_use]
    pub fn capture(app: &App) -> Self {
        Self {
            rows: Arc::clone(&app.snapshot.rows),
            indices: app.filtered.clone(),
            columns: columns_for_view_options(app, u16::MAX, false)
                .into_iter()
                .filter(|column| !column.label.is_empty())
                .collect(),
            now_unix: app.now_unix,
        }
    }

    /// Write the full filtered table as CSV, including columns outside the viewport.
    ///
    /// # Errors
    /// Propagates writer errors. No resource values are fetched during export.
    pub fn write_csv(&self, output: &mut (impl Write + ?Sized)) -> io::Result<()> {
        write_record(
            output,
            self.columns.iter().map(|column| column.label.as_str()),
        )?;
        for index in &self.indices {
            let Some(row) = self.rows.get(*index) else {
                continue;
            };
            let values = self
                .columns
                .iter()
                .map(|column| {
                    if let Some(path) = &column.json_path {
                        row.json_path_value(path)
                    } else if column.kubectl_age {
                        format_kubectl_age(row.age_seconds(self.now_unix))
                    } else if let Some(index) = column.table_index {
                        table_column_value(row, index, column.table_age, self.now_unix)
                    } else if let Some(field) = column.field {
                        column_value(row, field, self.now_unix)
                    } else {
                        computed_column_value(row, &column.label)
                    }
                })
                .collect::<Vec<_>>();
            write_record(output, values.iter().map(String::as_str))?;
        }
        Ok(())
    }
}

pub(crate) fn write_record<'a>(
    output: &mut (impl Write + ?Sized),
    values: impl IntoIterator<Item = &'a str>,
) -> io::Result<()> {
    for (index, value) in values.into_iter().enumerate() {
        if index > 0 {
            output.write_all(b",")?;
        }
        let value = value
            .lines()
            .map(plain_log_line)
            .collect::<Vec<_>>()
            .join("\n");
        output.write_all(b"\"")?;
        output.write_all(value.replace('"', "\"\"").as_bytes())?;
        output.write_all(b"\"")?;
    }
    output.write_all(b"\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        app::AppCommand,
        model::{ResourceKind, ResourceSnapshot, SourceState},
    };
    use std::time::Instant;

    #[test]
    fn export_captures_all_filtered_sorted_rows_independent_of_viewport_and_later_updates() {
        let mut app = App::new(
            "demo",
            None,
            ResourceKind::Pods,
            false,
            vec!["demo".to_owned()],
        );
        let rows = ["z-api", "b-api", "a-api", "worker"]
            .into_iter()
            .map(|name| Arc::new(ResourceRow::new("team,a", name, "Running", "1/1", "0", 0)))
            .collect();
        app.apply_snapshot(ResourceSnapshot {
            generation: 1,
            kind: ResourceKind::Pods.into(),
            rows: Arc::new(rows),
            table_columns: Arc::from([]),
            state: SourceState::Ready,
            revision: 1,
            emitted_at: Instant::now(),
        });
        assert!(matches!(
            app.run_command("pods /api"),
            AppCommand::Request(_)
        ));
        // request_view awaits a new generation; capture its resulting filtered snapshot.
        app.apply_snapshot(ResourceSnapshot {
            generation: app.generation,
            kind: ResourceKind::Pods.into(),
            rows: Arc::new(
                ["z-api", "b-api", "a-api", "worker"]
                    .into_iter()
                    .map(|name| {
                        Arc::new(ResourceRow::new("team,a", name, "Running", "1/1", "0", 0))
                    })
                    .collect(),
            ),
            table_columns: Arc::from([]),
            state: SourceState::Ready,
            revision: 2,
            emitted_at: Instant::now(),
        });
        app.viewport_height = 1;
        let snapshot = TableExportSnapshot::capture(&app);
        app.filtered.clear();
        let mut csv = Vec::new();
        snapshot.write_csv(&mut csv).unwrap();
        let text = String::from_utf8(csv).unwrap();
        assert_eq!(text.lines().count(), 4);
        assert!(text.contains("\"team,a\""));
        assert!(text.find("a-api").unwrap() < text.find("b-api").unwrap());
        assert!(text.find("b-api").unwrap() < text.find("z-api").unwrap());
        assert!(!text.contains("worker"));
    }

    #[test]
    fn csv_escapes_quotes_newlines_and_strips_terminal_controls() {
        let mut csv = Vec::new();
        write_record(&mut csv, ["a\"b", "line1\nline2", "\x1b[31mred\x1b[0m\x07"]).unwrap();
        assert_eq!(
            String::from_utf8(csv).unwrap(),
            "\"a\"\"b\",\"line1\nline2\",\"red\"\n"
        );
    }
}
