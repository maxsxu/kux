//! Pulses dashboard renderer (`:pulses` / `:pu` / `:hz`).

use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Paragraph},
};

use crate::{
    app::{App, InputMode},
    model::{PulseData, SourceState},
};

use super::{
    BODY_BORDER, CYAN, GREEN, MUTED, ORANGE, RED, SELECTED, YELLOW, draw_empty_resources,
    notice_block, resource_list_scope,
};

const SPARKS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

pub(super) fn draw(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    app.set_column_hitboxes(Vec::new());
    app.set_resource_rows_hitbox(0, 0, 0, 0, 0);
    app.set_list_scroll_extent(0, area.width);
    app.set_viewport_height(app.filtered.len().max(1));

    let filter = if app.input_mode == InputMode::Filter && !app.input.is_empty() {
        &app.input
    } else {
        &app.query
    };
    let filter_suffix = if filter.is_empty() {
        String::new()
    } else {
        format!(" /{filter}")
    };
    let title = format!(" Pulses({}){filter_suffix} ", resource_list_scope(app));
    let block = notice_block(
        Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(app.theme.color("frame.border.fgColor", BODY_BORDER)))
            .title(Span::styled(
                title,
                app.theme
                    .style("frame.title", Style::default().fg(CYAN).bold()),
            ))
            .title_alignment(Alignment::Center),
        app,
    );
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if app.filtered.is_empty() || matches!(app.snapshot.state, SourceState::Loading { loaded: 0 }) {
        draw_empty_resources(frame, app, area);
        return;
    }
    draw_pulse_grid(frame, app, inner);
}

fn draw_pulse_grid(frame: &mut Frame<'_>, app: &App, inner: Rect) {
    let items: Vec<(usize, &PulseData)> = app
        .filtered
        .iter()
        .enumerate()
        .filter_map(|(pos, &idx)| {
            app.snapshot
                .rows
                .get(idx)
                .and_then(|row| row.pulse_data().map(|p| (pos, p)))
        })
        .collect();

    let gauges: Vec<(usize, &PulseData)> = items
        .iter()
        .copied()
        .filter(|(_, p)| !p.is_sparkline)
        .collect();
    let sparklines: Vec<(usize, &PulseData)> = items
        .iter()
        .copied()
        .filter(|(_, p)| p.is_sparkline)
        .collect();

    let gauge_rows = gauges.len().div_ceil(4);
    let total_rows = gauge_rows + usize::from(!sparklines.is_empty());
    if total_rows == 0 || inner.height == 0 || inner.width == 0 {
        return;
    }

    let mut row_constraints = Vec::with_capacity(total_rows);
    for _ in 0..gauge_rows {
        row_constraints.push(Constraint::Fill(1));
    }
    if !sparklines.is_empty() {
        row_constraints.push(Constraint::Fill(1));
    }
    let v_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(row_constraints)
        .split(inner);

    for (row_idx, chunk) in gauges.chunks(4).enumerate() {
        let Some(&row_area) = v_chunks.get(row_idx) else {
            break;
        };
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Ratio(1, 4),
                Constraint::Ratio(1, 4),
                Constraint::Ratio(1, 4),
                Constraint::Ratio(1, 4),
            ])
            .split(row_area);
        for (col_idx, &(pos, pulse)) in chunk.iter().enumerate() {
            if let Some(&cell_area) = cols.get(col_idx) {
                draw_gauge_tile(frame, app, cell_area, pulse, pos == app.selected);
            }
        }
    }

    if !sparklines.is_empty()
        && let Some(&spark_row_area) = v_chunks.last()
    {
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Ratio(1, 2), Constraint::Ratio(1, 2)])
            .split(spark_row_area);
        for (idx, &(pos, pulse)) in sparklines.iter().take(2).enumerate() {
            if let Some(&cell_area) = cols.get(idx) {
                draw_sparkline_tile(frame, app, cell_area, pulse, pos == app.selected);
            }
        }
    }
}

fn draw_gauge_tile(
    frame: &mut Frame<'_>,
    app: &App,
    area: Rect,
    pulse: &PulseData,
    selected: bool,
) {
    let border_color = if selected {
        CYAN
    } else if pulse.faults > 0 {
        RED
    } else {
        GREEN
    };
    let title_style = if selected {
        app.theme
            .table_cursor(Style::default().fg(Color::White).bg(SELECTED).bold())
    } else {
        Style::default().fg(Color::White).bold()
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border_color))
        .title(Span::styled(format!(" {} ", pulse.title), title_style))
        .title_alignment(Alignment::Center);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.height == 0 || inner.width == 0 {
        return;
    }

    let ok_str = format!("{:02}", pulse.total);
    let fault_str = format!("{:02}", pulse.faults);
    let ok_style = if pulse.total == 0 {
        Style::default().fg(MUTED)
    } else {
        Style::default().fg(GREEN).bold()
    };
    let fault_style = if pulse.faults == 0 {
        Style::default().fg(MUTED)
    } else {
        Style::default().fg(ORANGE).bold()
    };

    if inner.height >= 3 && inner.width >= 16 {
        let ok_matrix = render_dot_number(&ok_str);
        let fault_matrix = render_dot_number(&fault_str);
        let mut lines = Vec::new();
        let top_pad = usize::from(inner.height.saturating_sub(3) / 2);
        for _ in 0..top_pad {
            lines.push(Line::from(""));
        }
        for r in 0..3 {
            let sep = if r == 1 { " ⠔ " } else { "   " };
            let delta_l = if r == 0 {
                delta_glyph(pulse.delta_total)
            } else {
                " "
            };
            let delta_r = if r == 0 {
                delta_glyph(pulse.delta_faults)
            } else {
                " "
            };
            lines.push(
                Line::from(vec![
                    Span::styled(delta_l, ok_style),
                    Span::styled(ok_matrix[r].clone(), ok_style),
                    Span::styled(sep, Style::default().fg(MUTED)),
                    Span::styled(fault_matrix[r].clone(), fault_style),
                    Span::styled(delta_r, fault_style),
                ])
                .alignment(Alignment::Center),
            );
        }
        frame.render_widget(Paragraph::new(lines), inner);
    } else {
        let line = Line::from(vec![
            Span::styled(ok_str, ok_style),
            Span::styled(" / ", Style::default().fg(MUTED)),
            Span::styled(fault_str, fault_style),
        ])
        .alignment(Alignment::Center);
        frame.render_widget(Paragraph::new(vec![line]), inner);
    }
}

fn draw_sparkline_tile(
    frame: &mut Frame<'_>,
    app: &App,
    area: Rect,
    pulse: &PulseData,
    selected: bool,
) {
    let pct = pulse
        .current
        .saturating_mul(100)
        .checked_div(pulse.capacity)
        .unwrap_or(0);
    let unit = if pulse.gvr.as_ref() == "cpu" {
        "m"
    } else {
        "Mi"
    };
    let legend = if pulse.capacity > 0 {
        format!(
            " {} {}%({}{unit}/{}{unit}) ",
            pulse.title, pct, pulse.current, pulse.capacity
        )
    } else {
        format!(" {} n/a ", pulse.title)
    };
    let border_color = if selected {
        CYAN
    } else if pct >= 90 {
        RED
    } else if pct >= 70 {
        ORANGE
    } else {
        GREEN
    };
    let title_style = if selected {
        app.theme
            .table_cursor(Style::default().fg(Color::White).bg(SELECTED).bold())
    } else {
        Style::default().fg(YELLOW).bold()
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border_color))
        .title(Span::styled(legend, title_style))
        .title_alignment(Alignment::Center);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.height == 0 || inner.width == 0 {
        return;
    }

    let max_val = pulse
        .capacity
        .max(pulse.series.iter().copied().max().unwrap_or(1).max(1));
    let max_cols = usize::from(inner.width.saturating_sub(2)).max(1);
    let tail: &[u64] = if pulse.series.len() > max_cols {
        &pulse.series[pulse.series.len() - max_cols..]
    } else {
        &pulse.series
    };
    let mut spark_str = String::with_capacity(tail.len());
    for &val in tail {
        let idx = usize::try_from((val.saturating_mul(7)) / max_val)
            .unwrap_or(7)
            .min(7);
        spark_str.push(SPARKS[idx]);
    }
    let bar_color = if pct >= 90 {
        RED
    } else if pct >= 70 {
        ORANGE
    } else {
        GREEN
    };
    let mut lines = Vec::new();
    let pad = usize::from(inner.height.saturating_sub(2));
    for _ in 0..pad {
        lines.push(Line::from(Span::styled(
            "│",
            Style::default().fg(BODY_BORDER),
        )));
    }
    lines.push(Line::from(vec![
        Span::styled("│", Style::default().fg(BODY_BORDER)),
        Span::styled(spark_str, Style::default().fg(bar_color).bold()),
    ]));
    if inner.height >= 2 {
        let axis = format!(
            "└{}",
            "─".repeat(usize::from(inner.width.saturating_sub(1)))
        );
        lines.push(Line::from(Span::styled(
            axis,
            Style::default().fg(BODY_BORDER),
        )));
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

const fn delta_glyph(delta: i8) -> &'static str {
    if delta > 0 {
        "↑"
    } else if delta < 0 {
        "↓"
    } else {
        " "
    }
}

fn render_dot_number(digits: &str) -> [String; 3] {
    let mut rows = [String::new(), String::new(), String::new()];
    for ch in digits.chars() {
        let d = ch.to_digit(10).unwrap_or(0);
        let glyph = dot_glyph(d);
        for r in 0..3 {
            rows[r].push_str(glyph[r]);
        }
    }
    rows
}

const fn dot_glyph(digit: u32) -> [&'static str; 3] {
    match digit {
        1 => [" ╻ ", " ┃ ", " ╹ "],
        2 => ["╺━┓", "┏━┛", "┗━╸"],
        3 => ["━━┓", "╺━┫", "━━┛"],
        4 => ["╻ ╻", "┗━┫", "  ╹"],
        5 => ["┏━╸", "┗━┓", "╺━┛"],
        6 => ["┏━╸", "┣━┓", "┗━┛"],
        7 => ["━━┓", "  ┃", "  ╹"],
        8 => ["┏━┓", "┣━┫", "┗━┛"],
        9 => ["┏━┓", "┗━┫", "╺━┛"],
        _ => ["┏━┓", "┃ ┃", "┗━┛"],
    }
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Instant};

    use ratatui::{Terminal, backend::TestBackend};

    use crate::{
        app::App,
        model::{ResourceDescriptor, ResourceKind, ResourceSnapshot, SourceState},
        ui::draw,
    };

    #[test]
    fn renders_pulses_gauges_and_sparklines() {
        let mut app = App::new(
            "demo",
            None,
            ResourceKind::Pods,
            false,
            vec!["demo".to_owned()],
        );
        app.kind = ResourceDescriptor::pulses();
        let specs = crate::source::kubernetes::resource_views::pulse_gauge_specs(true);
        assert_eq!(specs.len(), 16);
        let mut rows: Vec<Arc<_>> = specs
            .into_iter()
            .enumerate()
            .map(|(idx, (kind, title))| {
                let gvr = kind.gvr();
                Arc::new(
                    crate::source::kubernetes::resource_views::project_pulse_row(
                        "-",
                        &gvr,
                        title,
                        kind,
                        false,
                        12,
                        u64::from(idx == 4),
                        1,
                        0,
                        0,
                        0,
                        Arc::from([]),
                    ),
                )
            })
            .collect();
        rows.push(Arc::new(
            crate::source::kubernetes::resource_views::project_pulse_row(
                "-",
                "cpu",
                "Cpu",
                ResourceDescriptor::from(ResourceKind::Pods),
                true,
                0,
                0,
                0,
                0,
                1_200,
                4_000,
                Arc::from([800, 1_000, 1_200]),
            ),
        ));
        rows.push(Arc::new(
            crate::source::kubernetes::resource_views::project_pulse_row(
                "-",
                "memory",
                "Memory",
                ResourceDescriptor::from(ResourceKind::Pods),
                true,
                0,
                0,
                0,
                0,
                2_048,
                8_192,
                Arc::from([1_024, 1_536, 2_048]),
            ),
        ));
        app.apply_snapshot(ResourceSnapshot {
            generation: app.generation,
            kind: ResourceDescriptor::pulses(),
            rows: Arc::new(rows),
            table_columns: crate::source::kubernetes::resource_views::pulse_columns(),
            state: SourceState::Ready,
            revision: 1,
            emitted_at: Instant::now(),
        });
        let mut terminal = Terminal::new(TestBackend::new(140, 38)).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let rendered = crate::ui::tests::rendered_lines(&terminal, 140).join("\n");
        assert!(rendered.contains("Pulses(all)"));
        assert!(rendered.contains("Nodes"));
        assert!(rendered.contains("Pods"));
        assert!(rendered.contains("Deployments"));
        assert!(rendered.contains("Cpu 30%(1200m/4000m)"));
        assert!(rendered.contains("Memory 25%(2048Mi/8192Mi)"));
    }
}
