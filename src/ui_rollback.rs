use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::Style,
    text::{Line, Span},
    widgets::{Paragraph, Wrap},
};

use super::{App, MUTED, SELECTED, centered_rect, popup_block};

pub(super) fn draw(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    let Some(dialog) = app.rollback.dialog.as_ref() else {
        let popup = centered_rect(90, 7.min(area.height), area);
        super::preferences::clear_popup(frame, app, popup);
        let block = super::notice_block(popup_block(&app.theme, " Rollback ReplicaSet "), app);
        frame.render_widget(
            Paragraph::new(if app.rollback.pending.is_some() {
                "Loading selected revision and Deployment…"
            } else {
                "Esc closes this form"
            })
            .block(block)
            .wrap(Wrap { trim: false }),
            popup,
        );
        return;
    };
    let snapshot = &dialog.snapshot;
    let description = Paragraph::new(vec![
        Line::raw(format!("Context: {}", app.context)),
        Line::raw(format!(
            "Deployment: {}/{}",
            snapshot.target.namespace.as_deref().unwrap_or_default(),
            snapshot.deployment,
        )),
        Line::raw(format!(
            "Restore revision {} from ReplicaSet {}",
            snapshot.revision, snapshot.target.name,
        )),
        Line::raw(""),
        Line::raw(if snapshot.already_matches {
            "The Deployment already uses this revision's Pod template."
        } else {
            "Restore the Pod template and Deployment annotations from this revision."
        }),
        Line::raw("Preserve rollout bookkeeping annotations and the replica count."),
        Line::raw("Cancel is selected first · Tab selects Rollback · Esc cancels"),
    ])
    .wrap(Wrap { trim: false });
    let width = centered_rect(94, area.height, area).width.saturating_sub(2);
    let description_height = description.line_count(width);
    let height = u16::try_from(description_height.saturating_add(4))
        .unwrap_or(u16::MAX)
        .min(area.height);
    let popup = centered_rect(94, height, area);
    super::preferences::clear_popup(frame, app, popup);
    let block = super::notice_block(
        popup_block(&app.theme, " Rollback ReplicaSet ")
            .style(app.theme.style("dialog", Style::default())),
        app,
    );
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    let layout = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(inner);
    let scope_visible = inner.width >= 24
        && description_height <= usize::from(layout[0].height)
        && layout[1].height == 1;
    let confirm = dialog.confirm;
    if let Some(dialog) = app.rollback.dialog.as_mut() {
        dialog.scope_visible = scope_visible;
    }
    frame.render_widget(description, layout[0]);
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
            Span::styled(" [ Cancel ] ", button(!confirm)),
            Span::styled(
                if scope_visible {
                    " [ Rollback ] "
                } else {
                    " [ Resize to confirm ] "
                },
                button(confirm && scope_visible),
            ),
        ]))
        .centered(),
        layout[1],
    );
}
