//! `XRay` dependency tree renderer (`:xray <resource> [namespace]`).

use ratatui::{
    Frame,
    layout::{Alignment, Rect},
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Paragraph},
};

use crate::{
    app::{App, FilterMatcher, InputMode},
    model::{ResourceRow, SourceState},
    theme::Skin,
};

use super::{
    BODY_BORDER, CYAN, GREEN, MUTED, ORANGE, PINK, SELECTED, YELLOW, draw_empty_resources,
    highlight_line_matches, notice_block, resource_list_scope,
};

pub(super) fn draw(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    let viewport_height = usize::from(area.height.saturating_sub(2)).max(1);
    app.set_viewport_height(viewport_height);
    app.set_list_scroll_extent(0, area.width.saturating_sub(2));
    app.set_column_hitboxes(Vec::new());

    let filter = if app.input_mode == InputMode::Filter && !app.input.is_empty() {
        &app.input
    } else {
        &app.query
    };
    let filter_matcher = FilterMatcher::new(filter);
    let filter_suffix = if filter.is_empty() {
        String::new()
    } else {
        format!(" /{filter}")
    };
    let target_label = app.field_selector.as_deref().unwrap_or("v1/pods");
    let title = format!(
        " XRay · {target_label}({})[{}]{filter_suffix} ",
        resource_list_scope(app),
        app.filtered.len()
    );
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
        app.set_resource_rows_hitbox(0, 0, 0, 0, 0);
        draw_empty_resources(frame, app, area);
        return;
    }

    let visible_rows: Vec<(usize, &ResourceRow)> = app.visible_rows().collect();
    let count = visible_rows.len();
    let offset = app.offset;

    let all_filtered: Vec<&ResourceRow> = app
        .filtered
        .iter()
        .filter_map(|idx| app.snapshot.rows.get(*idx).map(AsRef::as_ref))
        .collect();

    let mut lines = Vec::with_capacity(count);
    for (pos, row) in visible_rows {
        let selected = pos == app.selected;
        lines.push(render_tree_line(
            &app.theme,
            &all_filtered,
            pos,
            row,
            selected,
            app.xray_collapsed.contains(&row.key),
            &filter_matcher,
        ));
    }

    frame.render_widget(Paragraph::new(lines), inner);
    app.set_resource_rows_hitbox(inner.x, inner.y, inner.width, offset, count);
}

fn render_tree_line(
    skin: &Skin,
    all_filtered: &[&ResourceRow],
    pos: usize,
    row: &ResourceRow,
    selected: bool,
    collapsed: bool,
    matcher: &FilterMatcher,
) -> Line<'static> {
    let Some(xray) = row.xray_data() else {
        return Line::raw(row.name.to_string());
    };
    let mut prefix = String::new();
    if xray.depth > 0 {
        for d in 1..xray.depth {
            let ancestor_has_more = ancestor_at_depth_has_next_sibling(all_filtered, pos, d);
            if ancestor_has_more {
                prefix.push_str("│  ");
            } else {
                prefix.push_str("   ");
            }
        }
        let has_next =
            node_has_next_sibling(all_filtered, pos, xray.depth, xray.parent_path.as_deref());
        if has_next {
            prefix.push_str("├─ ");
        } else {
            prefix.push_str("└─ ");
        }
    } else {
        prefix.push(' ');
    }

    let fold_glyph = if collapsed {
        "▸ "
    } else if xray.children_count > 0 {
        "▾ "
    } else {
        ""
    };
    let count_badge = if xray.children_count > 0 {
        format!("[{}]", xray.children_count)
    } else {
        String::new()
    };
    let info_badge = if xray.info.is_empty() {
        String::new()
    } else {
        format!(" ({})", xray.info)
    };
    let status_badge = match xray.status.as_ref() {
        "toast" => " [FAULT]",
        "noref" => " [MISSING]",
        "completed" => " [COMPLETED]",
        _ => "",
    };

    if selected {
        let cursor_style = skin.table_cursor(
            skin.style("views.xray", Style::default())
                .fg(Color::Black)
                .bg(skin.color("views.xray.cursorColor", SELECTED))
                .bold(),
        );
        let text = format!(
            "{prefix}{fold_glyph}{} {}{count_badge}{info_badge}{status_badge}",
            xray.emoji, row.name
        );
        return Line::from(Span::styled(text, cursor_style));
    }

    let tree_color = skin.color("views.xray.graphicColor", BODY_BORDER);
    let node_color = match xray.status.as_ref() {
        "toast" => skin.color("views.xray.killColor", PINK),
        "noref" => skin.color("frame.status.pendingColor", ORANGE),
        "completed" => skin.color("views.xray.completedColor", MUTED),
        _ if xray.is_namespace => skin.color("frame.title.fgColor", CYAN),
        _ if xray.is_container => skin.color("views.xray.fgColor", GREEN),
        _ => skin.color("views.xray.fgColor", Color::White),
    };
    let count_color = skin.color("frame.title.counterColor", YELLOW);
    let info_color = skin.color("info.fgColor", MUTED);

    let line = Line::from(vec![
        Span::styled(prefix, Style::default().fg(tree_color)),
        Span::styled(fold_glyph.to_owned(), Style::default().fg(count_color)),
        Span::raw(format!("{} ", xray.emoji)),
        Span::styled(row.name.to_string(), Style::default().fg(node_color).bold()),
        Span::styled(count_badge, Style::default().fg(count_color)),
        Span::styled(info_badge, Style::default().fg(info_color)),
        Span::styled(
            status_badge.to_owned(),
            Style::default().fg(node_color).bold(),
        ),
    ]);
    highlight_line_matches(skin, line, matcher)
}

fn node_has_next_sibling(
    rows: &[&ResourceRow],
    pos: usize,
    depth: usize,
    parent_path: Option<&str>,
) -> bool {
    for next in rows.iter().skip(pos + 1) {
        let Some(xr) = next.xray_data() else {
            continue;
        };
        if xr.depth < depth {
            return false;
        }
        if xr.depth == depth {
            return xr.parent_path.as_deref() == parent_path;
        }
    }
    false
}

fn ancestor_at_depth_has_next_sibling(
    rows: &[&ResourceRow],
    pos: usize,
    target_depth: usize,
) -> bool {
    for prev_idx in (0..pos).rev() {
        let Some(xr) = rows[prev_idx].xray_data() else {
            continue;
        };
        if xr.depth == target_depth {
            return node_has_next_sibling(rows, prev_idx, target_depth, xr.parent_path.as_deref());
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Instant};

    use ratatui::{Terminal, backend::TestBackend};

    use crate::{
        app::App,
        model::{ResourceDescriptor, ResourceKind, ResourceSnapshot, SourceState},
        ui::{draw, tests::rendered_lines},
    };

    #[test]
    fn xray_tree_renders_hierarchy_glyphs_and_collapse_state() {
        let mut app = App::new(
            "demo",
            Some("prod".to_owned()),
            ResourceKind::Pods,
            false,
            vec!["demo".to_owned()],
        );
        app.kind = ResourceDescriptor::xray();
        app.field_selector = Some("apps/v1/deployments".to_owned());
        let rows =
            crate::source::kubernetes::xray::build_demo_xray_rows(&crate::source::WatchRequest {
                instance: None,
                generation: app.generation,
                context: "demo".to_owned(),
                kind: ResourceDescriptor::xray(),
                namespace: Some("prod".to_owned()),
                field_selector: Some("apps/v1/deployments".to_owned()),
                label_selector: None,
            });
        app.apply_snapshot(ResourceSnapshot {
            generation: app.generation,
            kind: ResourceDescriptor::xray(),
            rows: Arc::new(rows),
            table_columns: crate::source::kubernetes::xray::xray_columns(),
            state: SourceState::Ready,
            revision: 1,
            emitted_at: Instant::now(),
        });
        let backend = TestBackend::new(100, 20);
        let mut terminal = Terminal::new(backend).expect("xray terminal");
        terminal
            .draw(|frame| draw(frame, &mut app))
            .expect("draw xray");
        let rendered = rendered_lines(&terminal, 100).join("\n");
        assert!(rendered.contains("XRay · apps/v1/deployments(prod)[7]"));
        assert!(rendered.contains("deploy-0000000"));
        assert!(rendered.contains("pod-0000000"));
        assert!(rendered.contains("app-config"));
        assert!(rendered.contains("├─") || rendered.contains("└─"));
    }
}
