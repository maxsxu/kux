use super::{
    App, Color, FilterMatcher, Line, Skin, Span, Style, highlight_line_matches, highlight_log_line,
    log_message_style,
};

pub(super) fn displayed_line(
    app: &App,
    index: usize,
    line: &str,
    matcher: &FilterMatcher,
    has_filter: bool,
) -> Line<'static> {
    let identity = app
        .log_view
        .as_ref()
        .and_then(|view| Some((view.displayed_source(index)?, view.metadata.as_ref()?)));
    let styled = match identity {
        Some((source, metadata)) => highlight_source_line(
            &app.theme,
            line,
            metadata.multi_pods.then_some(source.pod.as_str()),
            (!source.single_container).then_some(source.container.as_str()),
        ),
        None => highlight_log_line(&app.theme, line),
    };
    if has_filter {
        highlight_line_matches(&app.theme, styled, matcher)
    } else {
        styled
    }
}

fn highlight_source_line(
    skin: &Skin,
    line: &str,
    pod: Option<&str>,
    container: Option<&str>,
) -> Line<'static> {
    let pod = pod.filter(|name| !name.is_empty());
    let container = container.filter(|name| !name.is_empty());
    let mut offset = 0;
    let mut spans = Vec::with_capacity(6);
    if crate::log_view::without_log_timestamp(line) != line {
        let timestamp_end = line.find(' ').unwrap_or(line.len());
        let prefix_end = timestamp_end.max(30).saturating_add(1);
        let Some(timestamp) = line.get(..prefix_end) else {
            return highlight_log_line(skin, line);
        };
        if !timestamp[timestamp_end..].bytes().all(|byte| byte == b' ') {
            return highlight_log_line(skin, line);
        }
        spans.push(Span::styled(
            timestamp.to_owned(),
            skin.foreground(
                "views.logs.indicator.fgColor",
                skin.style("views.logs", Style::default().fg(Color::Gray).bold()),
            ),
        ));
        offset = prefix_end;
    }
    let color = source_color(pod.or(container).unwrap_or_default());
    for (name, bold) in [(pod, false), (container, true)] {
        let Some(name) = name else { continue };
        let Some(remaining) = line.get(offset..) else {
            return highlight_log_line(skin, line);
        };
        if !remaining.starts_with(name) || remaining.as_bytes().get(name.len()) != Some(&b' ') {
            return highlight_log_line(skin, line);
        }
        let style = skin.style("views.logs", Style::default()).fg(color);
        spans.push(Span::styled(
            format!("{name} "),
            if bold { style.bold() } else { style },
        ));
        offset += name.len() + 1;
    }
    let message = &line[offset..];
    spans.push(Span::styled(
        message.to_owned(),
        log_message_style(skin, message),
    ));
    Line::from(spans)
}

fn source_color(identity: &str) -> Color {
    const PALETTE: [Color; 8] = [
        Color::Rgb(0, 128, 128),
        Color::Rgb(0, 128, 0),
        Color::Rgb(128, 0, 128),
        Color::Rgb(0, 255, 0),
        Color::Rgb(0, 0, 255),
        Color::Rgb(255, 255, 0),
        Color::Rgb(255, 0, 255),
        Color::Rgb(0, 255, 255),
    ];
    let hash = identity.char_indices().fold(0_usize, |sum, (index, ch)| {
        sum.wrapping_add(index.wrapping_mul(ch as usize))
    });
    PALETTE[hash % PALETTE.len()]
}

#[cfg(test)]
mod tests {
    use ratatui::style::Modifier;

    use super::*;

    #[test]
    fn prefixes_keep_timestamp_first_and_use_pod_color_with_bold_container() {
        let text = format!(
            "{:<30} pod-a app ERROR cannot connect",
            "2026-10-13T06:00:00Z"
        );
        let line = highlight_source_line(&Skin::default(), &text, Some("pod-a"), Some("app"));
        assert_eq!(line.to_string(), text);
        assert_eq!(line.spans[0].style.fg, Some(Color::Gray));
        assert!(line.spans[0].style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(line.spans[1].content, "pod-a ");
        assert_eq!(line.spans[1].style.fg, Some(Color::Rgb(128, 0, 128)));
        assert_eq!(line.spans[2].content, "app ");
        assert_eq!(line.spans[2].style.fg, line.spans[1].style.fg);
        assert!(line.spans[2].style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(line.spans[3].style.fg, Some(super::super::RED));
    }

    #[test]
    fn container_and_single_container_sources_keep_plain_export_text() {
        let skin = Skin::default();
        for (text, pod, container, first) in [
            ("app INFO start", None, Some("app"), "app "),
            ("pod-a INFO start", Some("pod-a"), None, "pod-a "),
            ("INFO start", None, None, "INFO start"),
        ] {
            let line = highlight_source_line(&skin, text, pod, container);
            assert_eq!(line.to_string(), text);
            assert_eq!(line.spans[0].content, first);
        }
        assert_eq!(source_color("app"), Color::Rgb(0, 128, 128));
        assert_eq!(source_color("pod-b"), Color::Rgb(255, 0, 255));
        assert_eq!(source_color("λa"), Color::Rgb(128, 0, 128));
    }

    #[test]
    fn payload_words_cannot_impersonate_identity_or_change_message_severity() {
        let skin = Skin::default();
        let line = highlight_source_line(
            &skin,
            "error-pod app INFO healthy",
            Some("error-pod"),
            Some("app"),
        );
        assert_eq!(
            line.spans.last().unwrap().style.fg,
            Some(super::super::GREEN)
        );
        let plain = "INFO app could be text";
        assert_eq!(
            highlight_source_line(&skin, plain, Some("pod-a"), Some("app")),
            highlight_log_line(&skin, plain)
        );
        let malformed = "2026-10-13T06:00:00Z app INFO short timestamp gap";
        assert_eq!(
            highlight_source_line(&skin, malformed, None, Some("app")),
            highlight_log_line(&skin, malformed)
        );
    }
}
