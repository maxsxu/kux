use super::{App, CYAN, Frame, Line, MUTED, Span, Style, centered_rect, popup_block};
use ratatui::{
    layout::{Constraint, Layout, Rect},
    widgets::{Paragraph, Wrap},
};

pub(super) fn draw(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let count = app
        .image_editor
        .dialog
        .as_ref()
        .map_or(0, |dialog| dialog.values.len());
    let height = u16::try_from(count.saturating_mul(3).saturating_add(8))
        .unwrap_or(u16::MAX)
        .min(area.height);
    let popup = centered_rect(96, height, area);
    super::preferences::clear_popup(frame, app, popup);
    let block = super::notice_block(
        popup_block(&app.theme, " Set image ").style(app.theme.style("dialog", Style::default())),
        app,
    );
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    let Some(dialog) = &app.image_editor.dialog else {
        let message = if app.image_editor.pending.is_some() {
            "Loading container images…"
        } else {
            "Esc closes this form"
        };
        frame.render_widget(Paragraph::new(message).centered(), inner);
        return;
    };
    let areas = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(1),
        Constraint::Length(3),
    ])
    .split(inner);
    frame.render_widget(
        Paragraph::new(format!(
            "{} {} · Tab moves · Enter selects · Esc cancels",
            dialog.snapshot.target.kind.kind, dialog.snapshot.target.name
        ))
        .wrap(Wrap { trim: false }),
        areas[0],
    );
    let mut lines = Vec::new();
    for (index, image) in dialog.snapshot.images.iter().enumerate() {
        let kind = if image.init { "init" } else { "container" };
        lines.push(Line::styled(
            format!("{} ({kind})", image.name),
            app.theme
                .foreground("dialog.labelFgColor", Style::default().fg(CYAN)),
        ));
        let mut value = dialog.values[index].clone();
        if dialog.focus == index {
            value.insert(dialog.cursor.min(value.len()), '▏');
        }
        let style = app.theme.foreground(
            "dialog.fieldFgColor",
            Style::default().fg(super::Color::White),
        );
        let style = if dialog.focus == index {
            style.bg(super::SELECTED)
        } else {
            style
        };
        lines.push(Line::styled(value, style));
        lines.push(Line::raw(""));
    }
    let focus_lines = if dialog.focus < count {
        let mut prefix = lines[..=(dialog.focus * 3)].to_vec();
        let value = &dialog.values[dialog.focus];
        prefix.push(Line::raw(format!(
            "{}▏",
            &value[..dialog.cursor.min(value.len())]
        )));
        prefix
    } else {
        lines.clone()
    };
    let focused_height = Paragraph::new(focus_lines)
        .wrap(Wrap { trim: false })
        .line_count(areas[1].width);
    let scroll = focused_height.saturating_sub(usize::from(areas[1].height));
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((u16::try_from(scroll).unwrap_or(u16::MAX), 0)),
        areas[1],
    );
    draw_buttons(frame, app, areas[2], dialog.focus, count);
}

fn draw_buttons(frame: &mut Frame<'_>, app: &App, area: Rect, focus: usize, count: usize) {
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
    frame.render_widget(
        Paragraph::new(vec![
            Line::raw(""),
            Line::from(vec![
                Span::styled(" [ Cancel ] ", button(focus == count)),
                Span::styled(" [ Save ] ", button(focus == count + 1)),
            ])
            .centered(),
        ]),
        area,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        app::{ImageDialog, Overlay, ResourceTarget},
        model::ResourceKind,
        source::{ContainerImage, ImageSnapshot},
    };
    use ratatui::{Terminal, backend::TestBackend};

    #[test]
    fn narrow_form_scrolls_to_focused_container_and_keeps_save_cancel_visible() {
        let mut app = App::new(
            "demo",
            None,
            ResourceKind::Pods,
            false,
            vec!["demo".to_owned()],
        );
        let snapshot = ImageSnapshot {
            target: ResourceTarget {
                kind: ResourceKind::Pods.into(),
                namespace: Some("team".to_owned()),
                name: "selected-pod".to_owned(),
            },
            uid: "uid".to_owned(),
            resource_version: "8".to_owned(),
            images: (0..8)
                .map(|index| ContainerImage {
                    name: format!("worker-{index}"),
                    image: format!("registry.example.test/path/worker:{index}"),
                    init: index == 0,
                })
                .collect(),
        };
        let values = snapshot
            .images
            .iter()
            .map(|image| image.image.clone())
            .collect();
        app.image_editor.dialog = Some(ImageDialog {
            snapshot,
            values,
            focus: 7,
            cursor: 0,
        });
        app.overlay = Overlay::ImageDialog;
        let mut terminal = Terminal::new(TestBackend::new(60, 12)).unwrap();
        terminal
            .draw(|frame| {
                let area = frame.area();
                draw(frame, &app, area);
            })
            .unwrap();
        let rendered: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect();
        assert!(rendered.contains("worker-7 (container)"));
        assert!(rendered.contains("[ Cancel ]"));
        assert!(rendered.contains("[ Save ]"));
        assert!(rendered.contains("▏registry.example.test/path/worker:7"));

        let dialog = app.image_editor.dialog.as_mut().unwrap();
        dialog.values[7] = format!("prefix-{}:latest", "repository/".repeat(30));
        dialog.cursor = dialog.values[7].len();
        for (key, expected) in [
            (crossterm::event::KeyCode::Home, "▏prefix-"),
            (crossterm::event::KeyCode::End, ":latest▏"),
        ] {
            app.handle_key(crossterm::event::KeyEvent::new(
                key,
                crossterm::event::KeyModifiers::NONE,
            ));
            terminal
                .draw(|frame| {
                    let area = frame.area();
                    draw(frame, &app, area);
                })
                .unwrap();
            let rendered: String = terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(ratatui::buffer::Cell::symbol)
                .collect();
            assert!(
                rendered.contains(expected),
                "editing cursor remains visible: {expected}"
            );
        }
    }
}
