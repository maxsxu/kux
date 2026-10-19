use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::Style,
    text::{Line, Span},
    widgets::{Paragraph, Wrap},
};

use super::{App, CYAN, MUTED, SELECTED, centered_rect, popup_block};

pub(super) fn draw(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    let Some(dialog) = app.sanitize_dialog.clone() else {
        return;
    };
    let scope = dialog.namespace.as_ref().map_or_else(
        || "ALL namespaces".to_owned(),
        |namespace| format!("namespace {namespace}"),
    );
    let description = vec![
        Line::raw(format!("Context: {} · {scope}", dialog.context)),
        Line::raw("Filters and marks are ignored."),
        Line::raw("Delete Pending/completed/error Pods with zero grace period:"),
        Line::raw("Completed, Pending, CrashLoopBackOff, Error, ImagePullBackOff,"),
        Line::raw("ContainerStatusUnknown, Evicted, OOMKilled."),
        Line::raw(""),
        Line::raw("Stops at the first failure."),
    ];
    let description = Paragraph::new(description).wrap(Wrap { trim: false });
    let width = centered_rect(94, area.height, area).width.saturating_sub(2);
    let description_height = description.line_count(width);
    let height = u16::try_from(description_height.saturating_add(6))
        .unwrap_or(u16::MAX)
        .max(16)
        .min(area.height);
    let popup = centered_rect(94, height, area);
    super::preferences::clear_popup(frame, app, popup);
    let block = super::notice_block(
        popup_block(&app.theme, " Sanitize Pods ")
            .style(app.theme.style("dialog", Style::default())),
        app,
    );
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    let layout = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(3),
        Constraint::Length(1),
    ])
    .split(inner);
    let scope_visible = inner.width >= 24
        && description_height <= usize::from(layout[0].height)
        && layout[1].height == 3
        && layout[2].height == 1;
    if let Some(current) = app.sanitize_dialog.as_mut() {
        current.scope_visible = scope_visible;
    }
    frame.render_widget(description, layout[0]);
    let field_style = app
        .theme
        .foreground("dialog.fieldFgColor", Style::default().fg(CYAN));
    frame.render_widget(
        Paragraph::new(vec![
            Line::raw("Type Yes Please! to confirm:"),
            Line::styled(
                format!(
                    "{}{}",
                    dialog.confirmation,
                    if dialog.focus == 0 { "▏" } else { "" }
                ),
                if dialog.focus == 0 {
                    field_style.bg(SELECTED)
                } else {
                    field_style
                },
            ),
        ])
        .wrap(Wrap { trim: false }),
        layout[1],
    );
    let button = |selected| {
        app.theme.dialog_button(
            selected,
            if selected {
                Style::default().fg(super::Color::White).bg(SELECTED).bold()
            } else {
                Style::default().fg(MUTED)
            },
        )
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(" [ Cancel ] ", button(dialog.focus == 1)),
            Span::styled(
                if scope_visible {
                    " [ OK ] "
                } else {
                    " [ Resize to confirm ] "
                },
                button(dialog.focus == 2 && scope_visible),
            ),
        ]))
        .centered(),
        layout[2],
    );
}
