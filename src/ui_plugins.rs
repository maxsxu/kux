use super::{
    App, CYAN, Color, Frame, Line, MUTED, RED, Rect, SELECTED, Span, Style, centered_rect,
    drain_button_style, popup_block,
};
use crate::{app::PluginDialog, plugins::InputType};
use ratatui::{
    layout::{Constraint, Layout},
    widgets::{Paragraph, Wrap},
};

pub(super) fn draw_plugin_dialog(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let Some(dialog) = &app.plugin_dialog else {
        return;
    };
    let confirm = dialog.confirmation.is_some();
    let title = format!(
        " {} {} ",
        if confirm { "Confirm" } else { "Plugin" },
        dialog.plugin.description
    );
    let height = if confirm {
        16
    } else {
        12 + u16::try_from(dialog.plugin.inputs.len()).unwrap_or(5) * 2
    };
    let popup = centered_rect(90, height, area);
    super::preferences::clear_popup(frame, app, popup);
    let block = popup_block(&app.theme, &title)
        .style(app.theme.style("dialog", Style::default()))
        .border_style(
            app.theme
                .foreground("frame.border.focusColor", Style::default().fg(CYAN)),
        );
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    let parts = Layout::vertical([Constraint::Min(1), Constraint::Length(3)]).split(inner);
    let (lines, focused_line) = body_lines(app, dialog);
    let width = parts[0].width;
    let height = usize::from(parts[0].height);
    let focus_height = Paragraph::new(
        lines
            .iter()
            .take(focused_line + 1)
            .cloned()
            .collect::<Vec<_>>(),
    )
    .wrap(Wrap { trim: false })
    .line_count(width);
    let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
    let max_scroll = paragraph.line_count(width).saturating_sub(height);
    let scroll = if confirm {
        usize::from(dialog.preview_scroll).min(max_scroll)
    } else {
        focus_height.saturating_sub(height).min(max_scroll)
    };
    frame.render_widget(
        paragraph.scroll((u16::try_from(scroll).unwrap_or(u16::MAX), 0)),
        parts[0],
    );
    frame.render_widget(Paragraph::new(button_lines(app, dialog)), parts[1]);
}

fn body_lines(app: &App, dialog: &PluginDialog) -> (Vec<Line<'static>>, usize) {
    let mut lines = vec![Line::raw("")];
    if let Some(invocation) = &dialog.confirmation {
        lines.push(Line::raw("Run this command? (PgUp / PgDn scroll)"));
        lines.push(Line::raw(""));
        lines.extend(
            invocation
                .preview()
                .lines()
                .map(|line| Line::raw(line.to_owned())),
        );
    } else {
        lines.push(Line::raw(
            "Tab changes field · Space / arrows select options · Esc cancels",
        ));
        lines.push(Line::raw(""));
        for (index, input) in dialog.plugin.inputs.iter().enumerate() {
            let value = dialog.values.get(&input.name).map_or("", String::as_str);
            let marker = if input.required { "*" } else { " " };
            let suffix = match input.kind {
                InputType::Bool => " (Space toggles)",
                InputType::Dropdown => " (← / → select)",
                _ => "",
            };
            let label = app
                .theme
                .foreground("dialog.labelFgColor", Style::default().fg(CYAN));
            let mut value_style = app
                .theme
                .foreground("dialog.fieldFgColor", Style::default().fg(Color::White));
            if dialog.focus == index {
                value_style = value_style.bg(SELECTED).bold();
            }
            lines.push(Line::from(vec![
                Span::styled(format!("{marker} {}: ", input.label), label),
                Span::styled(
                    format!("{value}{}", if dialog.focus == index { "▏" } else { "" }),
                    value_style,
                ),
                Span::styled(suffix, Style::default().fg(MUTED)),
            ]));
            lines.push(Line::raw(""));
        }
    }
    if let Some(notice) = &app.notice {
        lines.push(Line::styled(
            notice.clone(),
            app.theme
                .foreground("frame.status.errorColor", Style::default().fg(RED)),
        ));
    }
    let focused_line = if dialog.focus >= dialog.plugin.inputs.len() {
        lines.len().saturating_sub(1)
    } else {
        3 + dialog.focus * 2
    };
    (lines, focused_line)
}

fn button_lines(app: &App, dialog: &PluginDialog) -> Vec<Line<'static>> {
    let run_label = if dialog.confirmation.is_some() || !dialog.plugin.should_confirm() {
        " [ Run ] "
    } else {
        " [ Continue ] "
    };
    vec![
        Line::raw(""),
        Line::from(vec![
            Span::styled(
                " [ Cancel ] ",
                drain_button_style(&app.theme, dialog.focus == dialog.cancel_index()),
            ),
            Span::styled(
                run_label,
                drain_button_style(&app.theme, dialog.focus == dialog.cancel_index() + 1),
            ),
        ])
        .centered(),
        Line::raw("Tab selects · Enter activates · Esc cancels").centered(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{app::Overlay, model::ResourceKind, plugins::parse_plugin_file};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::{Terminal, backend::TestBackend};
    use std::path::Path;

    fn dialog(extra: &str) -> App {
        let mut app = App::new(
            "demo",
            None,
            ResourceKind::Pods,
            false,
            vec!["demo".to_owned()],
        );
        app.overlay = Overlay::Contexts;
        let definition = format!(
            "plugins:\n  sample:\n    shortCut: v\n    description: Sample plugin\n    scopes: [contexts]\n    command: echo\n    args: ['$NAME']\n{extra}"
        );
        let plugin = parse_plugin_file(Path::new("plugins.yaml"), &definition)
            .unwrap()
            .remove("sample")
            .unwrap();
        app.activate_plugin(plugin);
        app
    }

    fn text(app: &mut App, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| draw_plugin_dialog(frame, app, frame.area()))
            .unwrap();
        terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect()
    }

    #[test]
    fn typed_form_and_confirmation_are_rendered_with_visible_actions() {
        let mut app = dialog(
            "    inputs:\n      - name: count\n        label: Replicas\n        type: number\n        required: true\n",
        );
        let form = text(&mut app, 100, 25);
        assert!(form.contains("Sample plugin"));
        assert!(form.contains("* Replicas:"));
        assert!(form.contains("[ Continue ]"));
        app.handle_key(KeyEvent::new(KeyCode::Char('3'), KeyModifiers::NONE));
        app.plugin_dialog.as_mut().unwrap().focus = 2;
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        let confirm = text(&mut app, 100, 25);
        assert!(confirm.contains("Confirm Sample plugin"));
        assert!(confirm.contains("\"echo\" \"demo\""));
        assert!(confirm.contains("[ Run ]"));
    }

    #[test]
    fn long_preview_scrolls_without_hiding_confirmation_actions() {
        let mut app = dialog("    confirm: true\n");
        app.plugin_dialog
            .as_mut()
            .unwrap()
            .confirmation
            .as_mut()
            .unwrap()
            .args = vec![
            "long command argument ".repeat(100),
            "END_MARKER".to_owned(),
        ];
        let first = text(&mut app, 45, 12);
        assert!(first.contains("[ Cancel ]"));
        assert!(first.contains("[ Run ]"));
        app.plugin_dialog.as_mut().unwrap().preview_scroll = u16::MAX;
        let last = text(&mut app, 45, 12);
        assert!(last.contains("END_MARKER"));
        assert!(last.contains("[ Cancel ]"));
        assert!(last.contains("[ Run ]"));
        for (width, height) in [(1, 1), (15, 5), (30, 8)] {
            let _ = text(&mut app, width, height);
        }
    }
}
