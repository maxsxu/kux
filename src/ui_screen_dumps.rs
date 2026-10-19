use super::{
    App, CYAN, Cell, Color, Frame, Line, MUTED, Rect, Row, Span, Style, Table, centered_rect,
    chooser_window, format_kubectl_age, popup_block,
};
use ratatui::{
    layout::Constraint,
    widgets::{Paragraph, TableState},
};

pub(super) fn action_hints(app: &App) -> Vec<super::HeaderHint> {
    let mut hints = vec![
        super::action_hint("enter", "Edit File"),
        super::action_hint("v", "Preview"),
        super::action_hint("c", "Copy Path"),
        super::action_hint("shift-n", "Sort Name"),
        super::action_hint("shift-a", "Sort Age"),
        super::action_hint("ctrl-r", "Refresh"),
        super::action_hint("esc", "Back"),
    ];
    if !app.read_only {
        hints.push(super::action_hint("ctrl-d", "Delete"));
    }
    hints
}

pub(super) fn draw(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    let visible = app.visible_dump_indices();
    let delete_hint = if app.read_only {
        ""
    } else {
        " · Ctrl-D delete"
    };
    let title = format!(
        " screen dumps [{}] · Enter edit · v preview{delete_hint} · Ctrl-R refresh ",
        visible.len()
    );
    let block = super::notice_block(popup_block(&app.theme, &title), app);
    let inner = block.inner(area);
    let capacity = usize::from(inner.height.saturating_sub(2)).max(1);
    app.viewport_height = capacity;
    let (start, window) = chooser_window(&visible, app.overlay_selected, capacity);
    let rows = window
        .iter()
        .map(|index| {
            let entry = &app.dumps.entries[*index];
            let age = u64::try_from(app.now_unix.saturating_sub(entry.modified_unix)).unwrap_or(0);
            Row::new([
                Cell::from(entry.name.clone()),
                Cell::from(entry.dir_display()),
                Cell::from(format!("{} B", entry.size_bytes)),
                Cell::from(format_kubectl_age(age)),
            ])
            .style(app.theme.style("views.table", Style::default()))
        })
        .collect::<Vec<_>>();
    app.set_chooser_hitbox(inner.x, inner.y.saturating_add(2), inner.width, window);
    let mut state = TableState::default();
    state.select(
        visible
            .iter()
            .position(|index| *index == app.overlay_selected)
            .map(|index| index.saturating_sub(start)),
    );
    let table = Table::new(
        rows,
        [
            Constraint::Fill(1),
            Constraint::Fill(1),
            Constraint::Length(13),
            Constraint::Length(10),
        ],
    )
    .header(
        Row::new(["NAME", "DIR", "SIZE", "AGE"])
            .style(
                app.theme
                    .style("views.table.header", Style::default().fg(CYAN).bold()),
            )
            .bottom_margin(1),
    )
    .row_highlight_style(
        app.theme
            .table_cursor(Style::default().fg(Color::White).bg(super::SELECTED).bold()),
    )
    .block(block);
    frame.render_stateful_widget(table, area, &mut state);
    if visible.is_empty() {
        frame.render_widget(
            Paragraph::new(if app.dumps.loading {
                "Loading screen dumps…"
            } else {
                "No screen dumps. Ctrl-S exports tables or content."
            })
            .style(Style::default().fg(MUTED)),
            Rect {
                y: inner.y.saturating_add(2),
                height: inner.height.saturating_sub(2),
                ..inner
            },
        );
    }
    if let Some(entry) = &app.dumps.delete_pending {
        let popup = centered_rect(80, 9, frame.area());
        super::preferences::clear_popup(frame, app, popup);
        frame.render_widget(
            Paragraph::new(vec![
                Line::raw(""),
                Line::raw(format!("Delete {}?", entry.name)),
                Line::raw(""),
                Line::from(vec![
                    Span::styled("Enter deletes", Style::default().fg(super::RED).bold()),
                    Span::raw(" · Esc cancels"),
                ]),
            ])
            .centered()
            .block(popup_block(&app.theme, " Delete screen dump ")),
            popup,
        );
    }
}

#[cfg(test)]
mod tests {
    use ratatui::{Terminal, backend::TestBackend};

    use super::*;
    use crate::{app::Overlay, model::ResourceKind};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    #[test]
    fn readonly_delete_rejection_is_visible_without_application_header() {
        let mut app = App::new(
            "demo",
            None,
            ResourceKind::Pods,
            true,
            vec!["demo".to_owned()],
        );
        app.overlay = Overlay::ScreenDumps;
        app.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL));
        let mut terminal = Terminal::new(TestBackend::new(100, 18)).unwrap();
        terminal
            .draw(|frame| {
                let area = frame.area();
                draw(frame, &mut app, area);
            })
            .unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();
        assert!(rendered.contains("Dump deletion is disabled in read-only mode"));
        assert!(!rendered.contains("Ctrl-D delete"));
        assert!(app.dumps.delete_pending.is_none());
    }
}
