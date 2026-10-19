#[cfg(test)]
use super::Span;
use super::{App, CYAN, Line, Style};
#[cfg(test)]
use crate::app::{ListMode, Overlay};
use ratatui::{
    Frame,
    layout::Rect,
    style::Color,
    widgets::{Block, Clear, Paragraph},
};

pub(super) fn body_style(app: &App) -> Style {
    // Give the built-in inverted palette a light canvas while preserving an
    // explicitly transparent skin's terminal-default background.
    let fallback = if app.ui.invert() {
        Style::default().fg(Color::White).bg(Color::Black)
    } else {
        Style::default()
    };
    app.theme.style("body", fallback)
}

pub(super) fn invert_frame(frame: &mut Frame<'_>) {
    let mut colors = std::collections::HashMap::new();
    for cell in &mut frame.buffer_mut().content {
        cell.fg = *colors
            .entry(cell.fg)
            .or_insert_with(|| crate::theme::invert_color(cell.fg));
        cell.bg = *colors
            .entry(cell.bg)
            .or_insert_with(|| crate::theme::invert_color(cell.bg));
    }
}

pub(super) fn clear_popup(frame: &mut Frame<'_>, app: &App, area: Rect) {
    frame.render_widget(Clear, area);
    frame.render_widget(Block::default().style(body_style(app)), area);
}

#[cfg(test)]
pub(super) fn crumbs(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let current = app.history_cursor.min(app.history.len().saturating_sub(1));
    let mut labels = app
        .history
        .iter()
        .take(current + 1)
        .rev()
        .take(4)
        .map(|view| view.kind.plural.to_string())
        .collect::<Vec<_>>();
    labels.reverse();
    match &app.list_mode {
        ListMode::Containers { pod, .. } => {
            labels.push(pod.clone());
            labels.push("containers".to_owned());
        }
        ListMode::Policy { title } | ListMode::References { title } => {
            labels.push(title.clone());
        }
        ListMode::Resources => {}
    }
    let overlay = match app.overlay {
        Overlay::Contexts => Some("contexts"),
        Overlay::Namespaces => Some("namespaces"),
        Overlay::Aliases => Some("aliases"),
        Overlay::Help => Some("help"),
        Overlay::ScreenDumps => Some("screendumps"),
        Overlay::PortForwards => Some("portforwards"),
        Overlay::Content => Some(app.content_title.as_str()),
        _ => None,
    };
    if let Some(label) = overlay {
        labels.push(label.to_owned());
    }
    let active = labels.len().saturating_sub(1);
    let mut spans = Vec::new();
    for (index, label) in labels.into_iter().enumerate() {
        let label = label.to_lowercase().replace(' ', "");
        let style = app.theme.style(
            "frame.crumbs",
            Style::default().fg(Color::Black).bg(CYAN).bold(),
        );
        let style = if index == active {
            style.bg(app
                .theme
                .color("frame.crumbs.activeColor", Color::Rgb(255, 165, 0)))
        } else {
            style
        };
        spans.push(Span::styled(format!(" <{label}> "), style));
        spans.push(Span::raw(" "));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

pub(super) fn splash(frame: &mut Frame<'_>, app: &App) {
    let area = frame.area();
    frame.render_widget(Block::default().style(body_style(app)), area);
    let text = vec![
        Line::raw(""),
        Line::styled(
            "KUX",
            app.theme
                .style("frame.title", Style::default().fg(CYAN).bold()),
        ),
        Line::raw(""),
        Line::raw("Kubernetes terminal"),
        Line::raw(format!("Context: {}", app.context)),
    ];
    let height = 7_u16.min(area.height);
    let bounds = Rect::new(
        area.x,
        area.y + area.height.saturating_sub(height) / 2,
        area.width,
        height,
    );
    frame.render_widget(Paragraph::new(text).centered(), bounds);
    if app.ui.invert() {
        invert_frame(frame);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::UserConfig, model::ResourceKind, theme::Skin};
    use ratatui::{Terminal, backend::TestBackend};

    fn app() -> App {
        App::new(
            "demo",
            None,
            ResourceKind::Pods,
            false,
            vec!["demo".to_owned()],
        )
    }

    #[test]
    fn inversion_sets_default_canvas_but_preserves_explicit_transparency() {
        let mut app = app();
        let mut config = UserConfig::default();
        config.settings.ui.invert = true;
        app.apply_user_config(config.clone());
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal
            .draw(|frame| super::super::draw(frame, &mut app))
            .unwrap();
        let blank = &terminal.backend().buffer()[(99, 29)];
        assert_eq!(blank.bg, crate::theme::invert_color(Color::Black));
        app.overlay = Overlay::ConfirmDelete;
        terminal
            .draw(|frame| super::super::draw(frame, &mut app))
            .unwrap();
        let popup = super::super::centered_rect(60, 9, Rect::new(0, 0, 100, 30));
        assert_eq!(
            terminal.backend().buffer()[(popup.x + 1, popup.y + 1)].bg,
            crate::theme::invert_color(Color::Black)
        );
        config.theme =
            Skin::from_yaml("k9s:\n  body:\n    fgColor: default\n    bgColor: default\n").unwrap();
        app.apply_user_config(config);
        terminal
            .draw(|frame| super::super::draw(frame, &mut app))
            .unwrap();
        assert_eq!(terminal.backend().buffer()[(99, 29)].bg, Color::Reset);
        terminal.draw(|frame| splash(frame, &app)).unwrap();
        assert_eq!(terminal.backend().buffer()[(0, 0)].fg, Color::Reset);
        assert_eq!(terminal.backend().buffer()[(0, 0)].bg, Color::Reset);
    }

    #[test]
    fn crumbs_use_component_names_and_active_background_independently_of_header() {
        let mut app = app();
        let mut config = UserConfig::default();
        config.settings.ui.headless = true;
        config.theme = Skin::from_yaml("k9s:\n  frame:\n    crumbs:\n      fgColor: '#112233'\n      bgColor: '#334455'\n      activeColor: '#556677'\n").unwrap();
        app.apply_user_config(config.clone());
        app.overlay = Overlay::ScreenDumps;
        let mut terminal = Terminal::new(TestBackend::new(100, 1)).unwrap();
        terminal
            .draw(|frame| crumbs(frame, &app, frame.area()))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let first: String = buffer.content[..100]
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect();
        assert!(first.starts_with(" <pods>   <screendumps>"));
        assert_eq!(buffer[(2, 0)].fg, Color::Rgb(0x11, 0x22, 0x33));
        assert_eq!(buffer[(2, 0)].bg, Color::Rgb(0x33, 0x44, 0x55));
        assert_eq!(buffer[(12, 0)].bg, Color::Rgb(0x55, 0x66, 0x77));

        let mut main_terminal = Terminal::new(TestBackend::new(100, 25)).unwrap();
        main_terminal
            .draw(|frame| super::super::draw(frame, &mut app))
            .unwrap();
        let first: String = main_terminal.backend().buffer().content[..100]
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect();
        assert!(first.starts_with('╭'));
        assert!(first.contains("screen dumps"));
    }
    #[test]
    fn reference_crumbs_name_the_source_and_active_reference_view() {
        let mut app = app();
        app.list_mode = ListMode::References {
            title: "UsedBy(configmaps/default/settings)".to_owned(),
        };
        let mut terminal = Terminal::new(TestBackend::new(100, 1)).unwrap();
        terminal
            .draw(|frame| crumbs(frame, &app, frame.area()))
            .unwrap();
        let first: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect();
        assert!(first.starts_with(" <pods>   <usedby(configmaps/default/settings)>"));
    }
}
