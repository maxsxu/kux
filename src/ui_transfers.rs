use super::{App, CYAN, Frame, Line, MUTED, Span, Style, centered_rect, popup_block};
use ratatui::{
    layout::{Constraint, Layout, Rect},
    widgets::{Paragraph, Wrap},
};

pub(super) fn draw(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let popup = centered_rect(96, 25.min(area.height), area);
    super::preferences::clear_popup(frame, app, popup);
    let block = super::notice_block(
        popup_block(&app.theme, " Transfer ").style(app.theme.style("dialog", Style::default())),
        app,
    );
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    let Some(dialog) = &app.transfers.dialog else {
        let message = if app.transfers.pending.is_some() {
            "Loading Pod containers…"
        } else {
            "Esc closes this form"
        };
        frame.render_widget(Paragraph::new(message).centered(), inner);
        return;
    };
    let areas = Layout::vertical([Constraint::Min(1), Constraint::Length(3)]).split(inner);
    let mut lines = vec![Line::raw(if dialog.running {
        "Transferring… Esc cancels"
    } else if dialog.download {
        "Download Files · Tab moves · Enter selects · Esc cancels"
    } else {
        "Upload Files · Tab moves · Enter selects · Esc cancels"
    })];
    if dialog.containers.len() > 1 {
        lines.push(Line::raw(format!(
            "Available containers: {}",
            dialog.containers.join(", ")
        )));
    }
    lines.push(Line::raw(""));
    let label_style = app
        .theme
        .foreground("dialog.labelFgColor", Style::default().fg(CYAN));
    let mut focus_lines = Vec::new();
    for (focus, label) in [
        "Download",
        "From",
        "To",
        "No Preserve",
        "Container",
        "Retries",
    ]
    .iter()
    .enumerate()
    {
        let value = match focus {
            0 => checkbox(dialog.download),
            1 => dialog.values[0].clone(),
            2 => dialog.values[1].clone(),
            3 => checkbox(dialog.no_preserve),
            4 => dialog.values[2].clone(),
            _ => dialog.values[3].clone(),
        };
        lines.push(Line::styled(format!("{label}:"), label_style));
        let selected = focus == dialog.focus && !dialog.running;
        let mut display = value.clone();
        let editable = !matches!(focus, 0 | 3);
        if selected && editable {
            display.insert(dialog.cursor.min(display.len()), '▏');
        }
        let style = app.theme.foreground(
            "dialog.fieldFgColor",
            Style::default().fg(super::Color::White),
        );
        lines.push(Line::styled(
            display,
            if selected {
                style.bg(super::SELECTED)
            } else {
                style
            },
        ));
        if selected {
            focus_lines = lines[..lines.len() - 1].to_vec();
            focus_lines.push(Line::raw(if editable {
                format!("{}▏", &value[..dialog.cursor.min(value.len())])
            } else {
                value
            }));
        }
    }
    if focus_lines.is_empty() {
        focus_lines.clone_from(&lines);
    }
    let focused_height = Paragraph::new(focus_lines)
        .wrap(Wrap { trim: false })
        .line_count(areas[0].width);
    let scroll = focused_height.saturating_sub(usize::from(areas[0].height));
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((u16::try_from(scroll).unwrap_or(u16::MAX), 0)),
        areas[0],
    );
    draw_buttons(frame, app, areas[1], dialog.focus, dialog.running);
}

fn checkbox(checked: bool) -> String {
    if checked { "[x]" } else { "[ ]" }.to_owned()
}

fn draw_buttons(frame: &mut Frame<'_>, app: &App, area: Rect, focus: usize, running: bool) {
    let button = |selected| {
        app.theme.dialog_button(
            selected,
            if selected {
                Style::default()
                    .fg(super::Color::White)
                    .bg(super::SELECTED)
                    .bold()
            } else {
                Style::default().fg(MUTED)
            },
        )
    };
    let mut buttons = vec![Span::styled(" [ Cancel ] ", button(focus == 6 || running))];
    if !running {
        buttons.push(Span::styled(" [ OK ] ", button(focus == 7)));
    }
    frame.render_widget(
        Paragraph::new(vec![Line::raw(""), Line::from(buttons).centered()]),
        area,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        app::{Overlay, ResourceTarget, TransferDialog},
        model::ResourceKind,
    };
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::{Terminal, backend::TestBackend};

    fn app() -> App {
        let mut app = App::new(
            "demo",
            None,
            ResourceKind::Pods,
            false,
            vec!["demo".to_owned()],
        );
        app.overlay = Overlay::TransferDialog;
        app.transfers.dialog = Some(TransferDialog {
            target: ResourceTarget {
                kind: ResourceKind::Pods.into(),
                namespace: Some("team".to_owned()),
                name: "pod".to_owned(),
            },
            containers: vec!["main".to_owned(), "sidecar".to_owned()],
            values: [
                "team/pod:/tmp/from".to_owned(),
                "/tmp/to".to_owned(),
                "main".to_owned(),
                "999".to_owned(),
            ],
            download: true,
            no_preserve: false,
            focus: 0,
            cursor: 0,
            running: false,
        });
        app
    }

    fn render(app: &App, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| draw(frame, app, frame.area()))
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
    fn full_form_shows_defaults_and_available_containers() {
        let app = app();
        let text = render(&app, 100, 30);
        for label in [
            "Transfer",
            "Download Files",
            "Available containers: main, sidecar",
            "team/pod:/tmp/from",
            "No Preserve:",
            "Retries:",
            "999",
            "[ Cancel ]",
            "[ OK ]",
        ] {
            assert!(text.contains(label), "{label}");
        }
    }

    #[test]
    fn small_form_tracks_unicode_cursor_and_keeps_buttons_visible() {
        let mut app = app();
        let dialog = app.transfers.dialog.as_mut().unwrap();
        dialog.focus = 1;
        dialog.values[0] = format!("team/pod:/資料/{}last", "folder/".repeat(30));
        dialog.cursor = dialog.values[0].len();
        for (code, expected) in [(KeyCode::Home, "▏team/pod:/"), (KeyCode::End, "last▏")] {
            app.handle_key(KeyEvent::new(code, KeyModifiers::NONE));
            let text = render(&app, 60, 12);
            assert!(text.contains(expected), "{expected}: {text}");
            assert!(text.contains("[ Cancel ]"));
            assert!(text.contains("[ OK ]"));
        }
        // A resize to a terminal too small for the form must still be harmless.
        render(&app, 1, 1);
    }

    #[test]
    fn running_form_has_cancel_only_and_pod_hint_obeys_read_only() {
        let mut app = app();
        app.transfers.dialog.as_mut().unwrap().running = true;
        let text = render(&app, 100, 30);
        assert!(text.contains("Transferring… Esc cancels"));
        assert!(text.contains("[ Cancel ]"));
        assert!(!text.contains("[ OK ]"));
        app.overlay = Overlay::None;
        assert!(
            super::super::resource_action_hints(&app)
                .iter()
                .any(|hint| hint.key == "t" && hint.label == "Transfer")
        );
        app.read_only = true;
        assert!(
            !super::super::resource_action_hints(&app)
                .iter()
                .any(|hint| hint.key == "t")
        );
    }
}
