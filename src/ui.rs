use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Cell, Clear, Paragraph, Row, Table, Wrap},
};

use crate::{
    app::{App, ColumnHitbox, ContentKind, InputMode, ListMode, Overlay, PendingDelete, SortField},
    model::{ResourceKind, SourceState, format_age},
};

const CYAN: Color = Color::Rgb(139, 233, 253);
const PURPLE: Color = Color::Rgb(189, 147, 249);
const GREEN: Color = Color::Rgb(80, 250, 123);
const YELLOW: Color = Color::Rgb(241, 250, 140);
const RED: Color = Color::Rgb(255, 85, 85);
const ORANGE: Color = Color::Rgb(255, 184, 108);
const PINK: Color = Color::Rgb(255, 121, 198);
const MUTED: Color = Color::Rgb(98, 114, 164);
const SELECTED: Color = Color::Rgb(68, 71, 90);
const ROW_ALT: Color = Color::Rgb(44, 47, 62);
const BODY_BORDER: Color = Color::Rgb(82, 104, 158);
const HEADER_MENU_ROWS: usize = 6;
const HEADER_LANE_SPACING: u16 = 2;
const HEADER_COLUMN_SPACING: u16 = 2;
const HEADER_INFO_LABEL_WIDTH: usize = 7;
const HELP_TWO_COLUMN_MIN_WIDTH: u16 = 120;
const LOGO_ROWS: [&str; 5] = [
    " _  __ _   _ __  __",
    "| |/ /| | | |\\ \\/ /",
    "| ' / | | | | \\  / ",
    "| . \\ | |_| | /  \\ ",
    "|_|\\_\\ \\___/ /_/\\_\\",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HeaderFlow {
    Horizontal,
    Stacked,
}

#[derive(Debug)]
struct HeaderLayout {
    flow: HeaderFlow,
    height: u16,
    show_logo: bool,
    logo_width: u16,
    info_width: u16,
    info_height: u16,
    menu_width: u16,
    columns: Vec<HeaderHintColumn>,
}

pub fn draw(frame: &mut Frame<'_>, app: &mut App) {
    app.clear_chooser_hitbox();
    let area = frame.area();
    let fullscreen_content = app.overlay == Overlay::Content && app.ui.fullscreen();
    if fullscreen_content && app.input_mode == InputMode::Browse {
        draw_content(frame, app, area);
        return;
    }

    let header_layout =
        (app.ui.header() && !fullscreen_content).then(|| header_layout(app, area.width));
    let header_height = header_layout.as_ref().map_or(0, |layout| layout.height);
    let body_gap_height = u16::from(header_layout.is_some());
    let input_height = match app.input_mode {
        InputMode::Browse => 0,
        InputMode::Command | InputMode::Filter => 3,
    };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(header_height),
            Constraint::Length(input_height),
            Constraint::Length(body_gap_height),
            Constraint::Min(5),
        ])
        .split(area);

    if let Some(layout) = &header_layout {
        draw_header(frame, app, chunks[0], layout);
    }
    if app.input_mode != InputMode::Browse {
        draw_input_view(frame, app, chunks[1]);
    }
    let body_area = chunks[3];
    match app.overlay {
        Overlay::Aliases => draw_aliases(frame, app, body_area),
        Overlay::Namespaces => draw_namespaces(frame, app, body_area),
        Overlay::Contexts => draw_contexts(frame, app, body_area),
        Overlay::Help => draw_help(frame, app, body_area),
        Overlay::Content => draw_content(frame, app, body_area),
        Overlay::Details => draw_details(frame, app, body_area),
        _ => draw_resources(frame, app, body_area),
    }

    match app.overlay {
        Overlay::ConfirmDelete => draw_delete_confirmation(frame, app, area),
        Overlay::Aliases
        | Overlay::Namespaces
        | Overlay::Contexts
        | Overlay::Help
        | Overlay::Content
        | Overlay::Details
        | Overlay::None => {}
    }
}

fn header_layout(app: &App, width: u16) -> HeaderLayout {
    let natural_info_width = cluster_info_width(app);
    let logo_width = logo_width();
    let all_columns = header_hint_columns(app);
    let first_column_width = all_columns.first().map_or(0, |column| column.width);
    let flow = if natural_info_width <= width {
        HeaderFlow::Horizontal
    } else {
        HeaderFlow::Stacked
    };
    let show_logo = flow == HeaderFlow::Horizontal
        && app.ui.logo()
        && logo_width
            .saturating_add(HEADER_LANE_SPACING)
            .saturating_add(natural_info_width)
            .saturating_add(HEADER_LANE_SPACING)
            .saturating_add(first_column_width)
            <= width;
    let info_width = match flow {
        HeaderFlow::Horizontal => natural_info_width,
        HeaderFlow::Stacked => width,
    };
    let info_height = u16::try_from(cluster_info_rows(app, info_width).len()).unwrap_or(u16::MAX);
    let fixed_width = info_width.saturating_add(if show_logo {
        logo_width.saturating_add(HEADER_LANE_SPACING)
    } else {
        0
    });
    let menu_width = match flow {
        HeaderFlow::Horizontal => width
            .saturating_sub(fixed_width)
            .saturating_sub(HEADER_LANE_SPACING),
        HeaderFlow::Stacked => width,
    };
    let columns = fit_header_columns(all_columns, menu_width);
    let menu_height = if columns.is_empty() {
        0
    } else {
        u16::try_from(HEADER_MENU_ROWS).unwrap_or(u16::MAX)
    };
    let logo_height = if show_logo {
        u16::try_from(LOGO_ROWS.len()).unwrap_or(u16::MAX)
    } else {
        0
    };
    let height = match flow {
        HeaderFlow::Horizontal => info_height.max(logo_height).max(menu_height),
        HeaderFlow::Stacked => info_height.saturating_add(menu_height),
    };

    HeaderLayout {
        flow,
        height,
        show_logo,
        logo_width,
        info_width,
        info_height,
        menu_width,
        columns,
    }
}

fn draw_header(frame: &mut Frame<'_>, app: &App, area: Rect, layout: &HeaderLayout) {
    if area.is_empty() {
        return;
    }
    match layout.flow {
        HeaderFlow::Horizontal => {
            let mut x = area.x;
            if layout.show_logo {
                draw_logo(frame, Rect::new(x, area.y, layout.logo_width, area.height));
                x = x
                    .saturating_add(layout.logo_width)
                    .saturating_add(HEADER_LANE_SPACING);
            }
            draw_cluster_info(
                frame,
                app,
                Rect::new(x, area.y, layout.info_width, area.height),
            );
            if !layout.columns.is_empty() {
                x = x
                    .saturating_add(layout.info_width)
                    .saturating_add(HEADER_LANE_SPACING);
                draw_header_menu_columns(
                    frame,
                    Rect::new(x, area.y, layout.menu_width, area.height),
                    &layout.columns,
                );
            }
        }
        HeaderFlow::Stacked => {
            let info_height = layout.info_height.min(area.height);
            draw_cluster_info(
                frame,
                app,
                Rect::new(area.x, area.y, area.width, info_height),
            );
            if !layout.columns.is_empty() {
                let menu_y = area.y.saturating_add(info_height);
                let bottom = area.y.saturating_add(area.height);
                draw_header_menu_columns(
                    frame,
                    Rect::new(
                        area.x,
                        menu_y,
                        layout.menu_width,
                        bottom.saturating_sub(menu_y),
                    ),
                    &layout.columns,
                );
            }
        }
    }
}

fn logo_width() -> u16 {
    LOGO_ROWS
        .iter()
        .map(|row| Line::raw(*row).width())
        .max()
        .and_then(|width| u16::try_from(width).ok())
        .unwrap_or(0)
}

fn draw_logo(frame: &mut Frame<'_>, area: Rect) {
    let logo = LOGO_ROWS.iter().enumerate().map(|(index, row)| {
        let color = if index < 3 { CYAN } else { PURPLE };
        Line::from(Span::styled(*row, Style::default().fg(color).bold()))
    });
    frame.render_widget(Paragraph::new(logo.collect::<Vec<_>>()), area);
}

fn cluster_info_entries(app: &App) -> [(&'static str, &str); 4] {
    let context = app.active_context_info();
    [
        ("Context", &app.context),
        ("Cluster", context.cluster_display()),
        ("User", context.user_display()),
        ("Version", &app.server_version),
    ]
}

fn cluster_info_label(label: &str) -> String {
    let padding = HEADER_INFO_LABEL_WIDTH.saturating_sub(Line::raw(label).width());
    format!("{label}:{} ", " ".repeat(padding))
}

fn cluster_info_width(app: &App) -> u16 {
    cluster_info_entries(app)
        .iter()
        .map(|(label, value)| Line::raw(format!("{}{value}", cluster_info_label(label))).width())
        .max()
        .and_then(|width| u16::try_from(width).ok())
        .unwrap_or(0)
}

fn cluster_info_rows(app: &App, width: u16) -> Vec<Line<'static>> {
    cluster_info_entries(app)
        .into_iter()
        .flat_map(|(label, value)| {
            wrap_styled_segments(
                [
                    (cluster_info_label(label), Style::default().fg(MUTED)),
                    (value.to_owned(), Style::default().fg(CYAN)),
                ],
                width,
            )
        })
        .collect()
}

fn wrap_styled_segments<const N: usize>(
    segments: [(String, Style); N],
    width: u16,
) -> Vec<Line<'static>> {
    if width == 0 {
        return Vec::new();
    }
    let max_width = usize::from(width);
    let mut rows: Vec<Vec<Span<'static>>> = vec![Vec::new()];
    let mut used = 0usize;

    for (text, style) in segments {
        let mut chunk = String::new();
        for character in text.chars() {
            let character_width = Line::raw(character.to_string()).width();
            if used > 0 && used.saturating_add(character_width) > max_width {
                if !chunk.is_empty() {
                    rows.last_mut()
                        .expect("header row exists")
                        .push(Span::styled(std::mem::take(&mut chunk), style));
                }
                rows.push(Vec::new());
                used = 0;
            }
            chunk.push(character);
            used = used.saturating_add(character_width);
        }
        if !chunk.is_empty() {
            rows.last_mut()
                .expect("header row exists")
                .push(Span::styled(chunk, style));
        }
    }

    rows.into_iter().map(Line::from).collect()
}

fn draw_cluster_info(frame: &mut Frame<'_>, app: &App, area: Rect) {
    frame.render_widget(Paragraph::new(cluster_info_rows(app, area.width)), area);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HeaderHintKind {
    Namespace,
    Action,
}

#[derive(Debug, Clone)]
struct HeaderHint {
    key: String,
    label: String,
    kind: HeaderHintKind,
    active: bool,
}

#[derive(Debug, Clone)]
struct HeaderHintColumn {
    hints: Vec<HeaderHint>,
    key_width: usize,
    width: u16,
}

fn header_hint_columns(app: &App) -> Vec<HeaderHintColumn> {
    let namespaces = namespace_hints(app);
    let actions = action_hints(app);
    namespaces
        .chunks(HEADER_MENU_ROWS)
        .chain(actions.chunks(HEADER_MENU_ROWS))
        .map(header_hint_column)
        .collect()
}

fn header_hint_column(hints: &[HeaderHint]) -> HeaderHintColumn {
    let key_width = hints
        .iter()
        .map(|hint| hint.key.chars().count().saturating_add(2))
        .max()
        .unwrap_or(0);
    let label_width = hints
        .iter()
        .map(|hint| hint.label.chars().count())
        .max()
        .unwrap_or(0);
    let width =
        u16::try_from(key_width.saturating_add(label_width).saturating_add(1)).unwrap_or(u16::MAX);
    HeaderHintColumn {
        hints: hints.to_vec(),
        key_width,
        width,
    }
}

fn header_columns_width(columns: &[HeaderHintColumn]) -> u16 {
    let content = columns
        .iter()
        .fold(0u16, |total, column| total.saturating_add(column.width));
    let spacing_count = u16::try_from(columns.len().saturating_sub(1)).unwrap_or(u16::MAX);
    content.saturating_add(HEADER_COLUMN_SPACING.saturating_mul(spacing_count))
}

fn fit_header_columns(columns: Vec<HeaderHintColumn>, available: u16) -> Vec<HeaderHintColumn> {
    let mut visible = Vec::new();
    for column in columns {
        let spacing = if visible.is_empty() {
            0
        } else {
            HEADER_COLUMN_SPACING
        };
        let required = header_columns_width(&visible)
            .saturating_add(spacing)
            .saturating_add(column.width);
        if required > available {
            break;
        }
        visible.push(column);
    }
    visible
}

fn draw_header_menu_columns(frame: &mut Frame<'_>, area: Rect, columns: &[HeaderHintColumn]) {
    if area.is_empty() || columns.is_empty() {
        return;
    }
    let widths = columns
        .iter()
        .map(|column| Constraint::Length(column.width));
    let rows = (0..HEADER_MENU_ROWS).map(|row_index| {
        Row::new(columns.iter().map(|column| {
            column.hints.get(row_index).map_or_else(
                || Cell::from(""),
                |hint| header_hint_cell(hint, column.key_width),
            )
        }))
    });
    frame.render_widget(
        Table::new(rows, widths).column_spacing(HEADER_COLUMN_SPACING),
        area,
    );
}

fn namespace_hints(app: &App) -> Vec<HeaderHint> {
    std::iter::once(HeaderHint {
        key: "0".to_owned(),
        label: "all".to_owned(),
        kind: HeaderHintKind::Namespace,
        active: app.namespace.is_none(),
    })
    .chain(
        app.recent_namespaces
            .iter()
            .take(9)
            .enumerate()
            .map(|(index, namespace)| HeaderHint {
                key: (index + 1).to_string(),
                label: namespace.clone(),
                kind: HeaderHintKind::Namespace,
                active: app.namespace.as_deref() == Some(namespace),
            }),
    )
    .collect()
}

fn action_hints(app: &App) -> Vec<HeaderHint> {
    let mut hints = vec![action_hint(":", "Command"), action_hint("/", "Filter")];
    match app.overlay {
        Overlay::Content => {
            hints.push(action_hint("esc", "Back"));
            if matches!(
                app.content_kind,
                Some(ContentKind::Yaml | ContentKind::SecretData)
            ) {
                hints.push(action_hint("c", "Copy"));
            }
            if app.content_kind == Some(ContentKind::Yaml) {
                hints.push(action_hint(
                    "m",
                    if app.ui.managed_fields() {
                        "Managed On"
                    } else {
                        "Managed Off"
                    },
                ));
            }
            if app.can_decode_content() {
                hints.push(action_hint(
                    "x",
                    if app.is_content_decoded() {
                        "Encoded"
                    } else {
                        "Decode"
                    },
                ));
            }
            hints.push(action_hint("w", "Wrap"));
            hints.push(action_hint("f", "Fullscreen"));
            if matches!(
                app.content_kind,
                Some(ContentKind::Logs | ContentKind::PreviousLogs)
            ) {
                hints.push(action_hint("t", "Timestamps"));
            }
        }
        Overlay::Aliases => {
            hints.extend([
                action_hint("enter", "Open"),
                action_hint("j/k", "Move"),
                action_hint("esc", "Back"),
            ]);
        }
        Overlay::Namespaces => {
            hints.extend([
                action_hint("enter", "Switch"),
                action_hint("y", "YAML"),
                action_hint("j/k", "Move"),
                action_hint("esc", "Back"),
            ]);
            if !app.read_only {
                hints.push(action_hint("e", "Edit"));
            }
        }
        Overlay::Contexts => {
            hints.extend([
                action_hint("enter", "Switch"),
                action_hint("c", "Copy Name"),
                action_hint("j/k", "Move"),
                action_hint("esc", "Back"),
            ]);
            if !app.read_only {
                hints.push(action_hint("ctrl-d", "Delete"));
            }
        }
        Overlay::Details => {
            hints.extend([
                action_hint("y", "YAML"),
                action_hint("d", "Describe"),
                action_hint("esc", "Back"),
            ]);
            if !app.read_only {
                hints.push(action_hint("e", "Edit"));
            }
        }
        _ if app.is_policy_view() => {
            hints.extend([
                action_hint("c", "Copy Name"),
                action_hint("j/k", "Move"),
                action_hint("esc", "Back"),
            ]);
        }
        _ if app.is_container_view() => {
            hints.extend([
                action_hint("enter", "Logs"),
                action_hint("l", "Logs"),
                action_hint("p", "Logs Previous"),
                action_hint("c", "Copy Name"),
                action_hint("esc", "Back"),
            ]);
        }
        _ => hints.extend(resource_action_hints(app)),
    }
    hints
}

fn resource_action_hints(app: &App) -> Vec<HeaderHint> {
    let enter_label = if app.kind.is_authorization_resource() {
        "Policy".to_owned()
    } else if let Some(child) = app
        .selected_row()
        .and_then(crate::model::ResourceRow::child_resource_data)
    {
        child.kind.label().to_owned()
    } else {
        match app.kind.known_kind() {
            Some(ResourceKind::Nodes) => "Pods".to_owned(),
            Some(ResourceKind::Pods) => "Containers".to_owned(),
            _ => "Details".to_owned(),
        }
    };
    let mut hints = vec![
        action_hint("enter", &enter_label),
        action_hint("y", "YAML"),
        action_hint("d", "Describe"),
        action_hint("c", "Copy Name"),
        action_hint("n", "Copy Namespace"),
        action_hint("?", "Help"),
        action_hint("ctrl-r", "Refresh"),
    ];
    if app.kind.is_core_v1_secret() {
        hints.push(action_hint("x", "Decode"));
    }
    if app.kind.known_kind() == Some(ResourceKind::Pods) {
        hints.push(action_hint("l", "Logs"));
        hints.push(action_hint("p", "Logs Previous"));
    }
    if app.namespace.is_none() && app.kind.is_namespaced() {
        hints.push(action_hint("w", "Warp Namespace"));
    }
    if !app.read_only {
        hints.push(action_hint("e", "Edit"));
        let delete_label = if app.marks.is_empty() {
            "Delete".to_owned()
        } else {
            format!("Delete ({})", app.marks.len())
        };
        hints.push(action_hint("ctrl-d", &delete_label));
        hints.push(action_hint("ctrl-k", "Kill"));
    }
    hints
}

fn action_hint(key: &str, label: &str) -> HeaderHint {
    HeaderHint {
        key: key.to_owned(),
        label: label.to_owned(),
        kind: HeaderHintKind::Action,
        active: false,
    }
}

fn header_hint_cell(hint: &HeaderHint, key_width: usize) -> Cell<'static> {
    let key = format!("<{}>", hint.key);
    let padding = " ".repeat(key_width.saturating_sub(key.chars().count()));
    let key_color = match hint.kind {
        HeaderHintKind::Namespace => GREEN,
        HeaderHintKind::Action => PINK,
    };
    let label_style = if hint.active {
        Style::default().fg(Color::White).bg(SELECTED).bold()
    } else {
        Style::default().fg(MUTED)
    };
    Cell::from(Line::from(vec![
        Span::styled(key, Style::default().fg(key_color).bold()),
        Span::raw(padding),
        Span::styled(format!(" {}", hint.label), label_style),
    ]))
}

#[derive(Clone)]
struct ViewColumn {
    label: String,
    field: Option<SortField>,
    table_index: Option<usize>,
    table_age: bool,
    width: Constraint,
    visible_from: u16,
}

fn view_column(
    label: &'static str,
    field: SortField,
    width: Constraint,
    visible_from: u16,
) -> ViewColumn {
    ViewColumn {
        label: label.to_owned(),
        field: Some(field),
        table_index: None,
        table_age: false,
        width,
        visible_from,
    }
}

fn columns_for_view(app: &App, width: u16) -> Vec<ViewColumn> {
    let mut columns = vec![ViewColumn {
        label: String::new(),
        field: None,
        table_index: None,
        table_age: false,
        width: Constraint::Length(2),
        visible_from: 0,
    }];
    let show_namespace = app.is_resource_view()
        && app.kind.is_namespaced()
        && (app.namespace.is_none() || app.ui.wide())
        && width >= 64;
    if show_namespace {
        columns.push(view_column(
            "NAMESPACE",
            SortField::Namespace,
            Constraint::Fill(2),
            0,
        ));
    }
    let kind_columns = if app.is_policy_view() {
        policy_columns()
    } else if app.is_container_view() {
        container_columns()
    } else if app.kind.is_api_service() {
        api_service_columns()
    } else if !app.snapshot.table_columns.is_empty() {
        server_table_columns(app, show_namespace)
    } else {
        match app.kind.known_kind() {
            Some(ResourceKind::Pods) => pod_columns(),
            Some(ResourceKind::Nodes) => node_columns(),
            _ => generic_columns(app.ui.wide()),
        }
    };
    columns.extend(
        kind_columns
            .into_iter()
            .filter(|column| width >= column.visible_from),
    );
    columns
}

fn server_table_columns(app: &App, namespace_already_shown: bool) -> Vec<ViewColumn> {
    let has_age = app
        .snapshot
        .table_columns
        .iter()
        .any(|column| column.name.eq_ignore_ascii_case("age"));
    app.snapshot
        .table_columns
        .iter()
        .enumerate()
        .filter(|(_, column)| app.ui.wide() || column.priority == 0)
        .filter(|(_, column)| {
            !namespace_already_shown || !column.name.eq_ignore_ascii_case("namespace")
        })
        .map(|(index, column)| {
            let table_age = !has_age && is_created_at_column(&column.name);
            let field = match column.name.to_ascii_lowercase().as_str() {
                "name" => Some(SortField::Name),
                "status" => Some(SortField::Status),
                "ready" => Some(SortField::Ready),
                "age" => Some(SortField::Age),
                _ if table_age => Some(SortField::Age),
                _ => None,
            };
            let label = if table_age {
                "AGE".to_owned()
            } else {
                column.name.to_ascii_uppercase()
            };
            let content_width = app
                .snapshot
                .rows
                .iter()
                .take(200)
                .filter_map(|row| row.table_cells.get(index))
                .map(|cell| {
                    if table_age {
                        8
                    } else {
                        cell.text.chars().count()
                    }
                })
                .max()
                .unwrap_or(0)
                .max(label.chars().count());
            let width = if column.name.eq_ignore_ascii_case("name") {
                Constraint::Fill(5)
            } else if table_age {
                Constraint::Length(8)
            } else {
                Constraint::Length(u16::try_from(content_width.clamp(3, 40)).unwrap_or(40))
            };
            ViewColumn {
                label,
                field,
                table_index: Some(index),
                table_age,
                width,
                visible_from: 0,
            }
        })
        .collect()
}

fn is_created_at_column(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "created at" | "created-at" | "creation timestamp"
    )
}

fn api_service_columns() -> Vec<ViewColumn> {
    vec![
        view_column("NAME", SortField::Name, Constraint::Fill(5), 0),
        view_column("SERVICE", SortField::Ready, Constraint::Fill(4), 0),
        view_column("AVAILABLE", SortField::Status, Constraint::Fill(3), 0),
        view_column("AGE", SortField::Age, Constraint::Length(10), 38),
    ]
}

fn policy_columns() -> Vec<ViewColumn> {
    vec![
        view_column("NAME", SortField::Name, Constraint::Fill(4), 0),
        view_column("API-GROUP", SortField::ApiGroup, Constraint::Fill(3), 0),
        view_column("BINDING", SortField::Binding, Constraint::Fill(5), 54),
        view_column("GET", SortField::Get, Constraint::Length(7), 68),
        view_column("LIST", SortField::List, Constraint::Length(7), 76),
        view_column("WATCH", SortField::Watch, Constraint::Length(7), 84),
        view_column("CREATE", SortField::Create, Constraint::Length(8), 92),
        view_column("PATCH", SortField::Patch, Constraint::Length(7), 102),
        view_column("UPDATE", SortField::Update, Constraint::Length(8), 110),
        view_column("DELETE", SortField::Delete, Constraint::Length(8), 120),
        view_column(
            "DEL-LIST",
            SortField::DeleteCollection,
            Constraint::Length(10),
            132,
        ),
        view_column("EXTRAS", SortField::Details, Constraint::Fill(2), 152),
    ]
}

fn container_columns() -> Vec<ViewColumn> {
    vec![
        view_column("NAME", SortField::Name, Constraint::Fill(4), 0),
        view_column("READY", SortField::Ready, Constraint::Length(7), 44),
        view_column("STATUS", SortField::Status, Constraint::Length(20), 0),
        view_column("RESTARTS", SortField::Restarts, Constraint::Length(9), 64),
        view_column("IMAGE", SortField::Details, Constraint::Fill(5), 52),
        view_column("AGE", SortField::Age, Constraint::Length(8), 76),
    ]
}

fn pod_columns() -> Vec<ViewColumn> {
    vec![
        view_column("NAME", SortField::Name, Constraint::Fill(5), 0),
        view_column("READY", SortField::Ready, Constraint::Length(7), 46),
        view_column("STATUS", SortField::Status, Constraint::Length(18), 0),
        view_column("RESTARTS", SortField::Restarts, Constraint::Length(9), 68),
        view_column("CPU", SortField::Cpu, Constraint::Length(8), 84),
        view_column(
            "%CPU/R",
            SortField::CpuRequestPercent,
            Constraint::Length(8),
            100,
        ),
        view_column(
            "%CPU/L",
            SortField::CpuLimitPercent,
            Constraint::Length(8),
            116,
        ),
        view_column("MEM", SortField::Memory, Constraint::Length(10), 92),
        view_column(
            "%MEM/R",
            SortField::MemoryRequestPercent,
            Constraint::Length(8),
            132,
        ),
        view_column(
            "%MEM/L",
            SortField::MemoryLimitPercent,
            Constraint::Length(8),
            148,
        ),
        view_column("NODE", SortField::Node, Constraint::Fill(3), 76),
        view_column("AGE", SortField::Age, Constraint::Length(8), 38),
    ]
}

fn node_columns() -> Vec<ViewColumn> {
    vec![
        view_column("NAME", SortField::Name, Constraint::Fill(5), 0),
        view_column("STATUS", SortField::Status, Constraint::Length(10), 0),
        view_column("TAINTS", SortField::Taints, Constraint::Fill(3), 70),
        view_column("VERSION", SortField::Version, Constraint::Length(22), 88),
        view_column("PODS", SortField::Pods, Constraint::Length(7), 62),
        view_column("CPU", SortField::Cpu, Constraint::Length(8), 102),
        view_column(
            "CPU/A",
            SortField::CpuAllocatable,
            Constraint::Length(8),
            118,
        ),
        view_column("%CPU", SortField::CpuPercent, Constraint::Length(7), 132),
        view_column("MEM", SortField::Memory, Constraint::Length(10), 108),
        view_column(
            "MEM/A",
            SortField::MemoryAllocatable,
            Constraint::Length(10),
            146,
        ),
        view_column("%MEM", SortField::MemoryPercent, Constraint::Length(7), 160),
        view_column("AGE", SortField::Age, Constraint::Length(8), 38),
    ]
}

fn generic_columns(wide: bool) -> Vec<ViewColumn> {
    let mut columns = vec![
        view_column("NAME", SortField::Name, Constraint::Fill(5), 0),
        view_column("STATUS", SortField::Status, Constraint::Length(20), 0),
        view_column("READY", SortField::Ready, Constraint::Length(7), 46),
    ];
    if wide {
        columns.push(view_column(
            "DETAILS",
            SortField::Details,
            Constraint::Fill(2),
            110,
        ));
    }
    columns.push(view_column(
        "AGE",
        SortField::Age,
        Constraint::Length(8),
        38,
    ));
    columns
}

fn draw_resources(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    let show_header = !matches!(app.snapshot.state, SourceState::Loading);
    let viewport_height = usize::from(area.height.saturating_sub(if show_header { 3 } else { 2 }));
    app.set_viewport_height(viewport_height);
    let columns = columns_for_view(app, area.width);

    let rows = app.visible_rows().map(|(position, item)| {
        let selected = position == app.selected;
        let marked = app.marks.contains(&item.key);
        let style = resource_row_style(position, selected);
        let cells = columns
            .iter()
            .map(|column| resource_cell(column, item, selected, marked, app.now_unix));
        Row::new(cells).style(style)
    });

    let widths: Vec<_> = columns.iter().map(|column| column.width).collect();
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
    let visible_count = app.filtered.len();
    let title = match &app.list_mode {
        ListMode::Resources => format!(
            " {}({})[{}]{} ",
            app.kind.plural_name(),
            app.namespace_display(),
            visible_count,
            filter_suffix
        ),
        ListMode::Containers { namespace, pod } => {
            format!(" containers({namespace}/{pod})[{visible_count}]{filter_suffix} ")
        }
        ListMode::Policy { title } => {
            format!(" {title}[{visible_count}]{filter_suffix} ")
        }
    };
    let hitbox_widths = widths.clone();
    let block = notice_block(
        Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(BODY_BORDER))
            .title(Span::styled(title, Style::default().fg(CYAN).bold()))
            .title_alignment(Alignment::Center),
        app,
    );
    let mut table = Table::new(rows, widths).column_spacing(1).block(block);
    if show_header {
        let header_cells = columns.iter().map(|column| {
            column.field.map_or_else(
                || Cell::from(column.label.clone()).style(Style::default().fg(CYAN).bold()),
                |field| column_header(app, &column.label, field),
            )
        });
        table = table.header(Row::new(header_cells).bottom_margin(1));
    }
    frame.render_widget(table, area);
    if show_header {
        update_column_hitboxes(app, area, &hitbox_widths, &columns);
    } else {
        app.set_column_hitboxes(Vec::new());
    }

    if app.filtered.is_empty() {
        let message = match &app.snapshot.state {
            SourceState::Loading => "Waiting for resources…",
            SourceState::Error(error) => error.as_ref(),
            SourceState::Ready if app.filter_error.is_some() => "Invalid regular expression",
            SourceState::Ready if app.query.is_empty() => "No resources found",
            SourceState::Ready => "No resources match the filter",
        };
        frame.render_widget(
            Paragraph::new(message)
                .alignment(Alignment::Center)
                .style(Style::default().fg(MUTED)),
            centered_rect(70, 3, area),
        );
    }
}

fn resource_cell(
    column: &ViewColumn,
    row: &crate::model::ResourceRow,
    selected: bool,
    marked: bool,
    now_unix: i64,
) -> Cell<'static> {
    let value = if let Some(index) = column.table_index {
        table_column_value(row, index, column.table_age, now_unix)
    } else if let Some(field) = column.field {
        column_value(row, field, now_unix)
    } else {
        return Cell::from(if marked { "●" } else { "" }).style(if selected || !marked {
            Style::default()
        } else {
            Style::default().fg(PINK).bold()
        });
    };

    let cell = Cell::from(value.clone());
    let Some(field) = column.field else {
        return cell;
    };
    if selected {
        cell
    } else if is_policy_verb(field) {
        cell.style(if policy_verb_allowed(row, field) {
            Style::default().fg(GREEN)
        } else {
            Style::default().fg(ORANGE)
        })
    } else {
        cell.style(resource_value_style(field, row, &value, now_unix))
    }
}

fn resource_row_style(position: usize, selected: bool) -> Style {
    if selected {
        return Style::default().fg(Color::White).bg(SELECTED).bold();
    }
    if position % 2 == 1 {
        Style::default().bg(ROW_ALT)
    } else {
        Style::default()
    }
}

fn resource_value_style(
    field: SortField,
    row: &crate::model::ResourceRow,
    value: &str,
    now_unix: i64,
) -> Style {
    match field {
        SortField::Namespace | SortField::Node => Style::default().fg(PURPLE),
        SortField::Name => Style::default().fg(CYAN),
        SortField::Status => status_style(value).bold(),
        SortField::Ready | SortField::Binding => readiness_style(value),
        SortField::Age => Style::default().fg(age_color(row.age_seconds(now_unix))),
        SortField::Version | SortField::Details => Style::default().fg(MUTED),
        _ => Style::default(),
    }
}

fn readiness_style(value: &str) -> Style {
    let normalized = value.trim().to_ascii_lowercase();
    let is_ready_fraction = normalized
        .split_once('/')
        .and_then(|(ready, total)| Some((ready.parse::<u64>().ok()?, total.parse::<u64>().ok()?)))
        .is_some_and(|(ready, total)| total > 0 && ready == total);
    let color = if normalized.is_empty() || normalized == "-" {
        MUTED
    } else if matches!(normalized.as_str(), "true" | "ready" | "running" | "active")
        || is_ready_fraction
    {
        GREEN
    } else {
        ORANGE
    };
    Style::default().fg(color)
}

const fn age_color(age_seconds: u64) -> Color {
    const HOUR: u64 = 60 * 60;
    const DAY: u64 = 24 * HOUR;
    if age_seconds < HOUR {
        GREEN
    } else if age_seconds < DAY {
        CYAN
    } else if age_seconds < 7 * DAY {
        PURPLE
    } else if age_seconds < 30 * DAY {
        YELLOW
    } else {
        MUTED
    }
}

const fn is_policy_verb(field: SortField) -> bool {
    matches!(
        field,
        SortField::Get
            | SortField::List
            | SortField::Watch
            | SortField::Create
            | SortField::Patch
            | SortField::Update
            | SortField::Delete
            | SortField::DeleteCollection
    )
}

fn update_column_hitboxes(
    app: &mut App,
    area: Rect,
    widths: &[Constraint],
    visible: &[ViewColumn],
) {
    if area.width <= 2 || area.height <= 2 {
        app.set_column_hitboxes(Vec::new());
        return;
    }
    let header_area = Rect::new(
        area.x.saturating_add(1),
        area.y.saturating_add(1),
        area.width.saturating_sub(2),
        1,
    );
    let columns = Layout::horizontal(widths.to_vec())
        .spacing(1)
        .split(header_area);
    let hitboxes = columns
        .iter()
        .zip(visible)
        .filter_map(|(column, spec)| {
            spec.field.map(|field| ColumnHitbox {
                x: column.x,
                y: column.y,
                width: column.width,
                field,
            })
        })
        .collect();
    app.set_column_hitboxes(hitboxes);
}

fn column_value(row: &crate::model::ResourceRow, field: SortField, now_unix: i64) -> String {
    let pod = row.pod_data();
    let node = row.node_data();
    match field {
        SortField::Namespace => row.namespace.to_string(),
        SortField::Name => row.name.to_string(),
        SortField::Ready | SortField::Binding => row.ready.to_string(),
        SortField::Status | SortField::ApiGroup => row.status.to_string(),
        SortField::Restarts => pod.map_or_else(
            || {
                row.container_data()
                    .map_or_else(|| "-".to_owned(), |data| data.restarts.to_string())
            },
            |data| data.restarts.to_string(),
        ),
        SortField::Cpu => format_cpu(
            pod.and_then(|data| data.cpu_millis)
                .or_else(|| node.and_then(|data| data.cpu_millis)),
        ),
        SortField::CpuRequestPercent => pod.map_or_else(
            || "-".to_owned(),
            |data| format_percent(data.cpu_millis, data.cpu_request_millis),
        ),
        SortField::CpuLimitPercent => pod.map_or_else(
            || "-".to_owned(),
            |data| format_percent(data.cpu_millis, data.cpu_limit_millis),
        ),
        SortField::Memory => format_memory(
            pod.and_then(|data| data.memory_bytes)
                .or_else(|| node.and_then(|data| data.memory_bytes)),
        ),
        SortField::MemoryRequestPercent => pod.map_or_else(
            || "-".to_owned(),
            |data| format_percent(data.memory_bytes, data.memory_request_bytes),
        ),
        SortField::MemoryLimitPercent => pod.map_or_else(
            || "-".to_owned(),
            |data| format_percent(data.memory_bytes, data.memory_limit_bytes),
        ),
        SortField::Node => pod.map_or_else(|| "-".to_owned(), |data| data.node.to_string()),
        SortField::Taints => {
            node.map_or_else(|| "-".to_owned(), |data| data.taint_count.to_string())
        }
        SortField::Version => node.map_or_else(|| "-".to_owned(), |data| data.version.to_string()),
        SortField::Pods => node.map_or_else(|| "-".to_owned(), |data| data.pods.to_string()),
        SortField::CpuAllocatable => node.map_or_else(
            || "-".to_owned(),
            |data| format_cpu(Some(data.cpu_allocatable_millis)),
        ),
        SortField::CpuPercent => node.map_or_else(
            || "-".to_owned(),
            |data| format_percent(data.cpu_millis, data.cpu_allocatable_millis),
        ),
        SortField::MemoryAllocatable => node.map_or_else(
            || "-".to_owned(),
            |data| format_memory(Some(data.memory_allocatable_bytes)),
        ),
        SortField::MemoryPercent => node.map_or_else(
            || "-".to_owned(),
            |data| format_percent(data.memory_bytes, data.memory_allocatable_bytes),
        ),
        SortField::Age => format_age(row.age_seconds(now_unix)),
        SortField::Details => row.details.to_string(),
        SortField::Get
        | SortField::List
        | SortField::Watch
        | SortField::Create
        | SortField::Patch
        | SortField::Update
        | SortField::Delete
        | SortField::DeleteCollection => {
            if policy_verb_allowed(row, field) {
                "✓".to_owned()
            } else {
                "×".to_owned()
            }
        }
    }
}

fn table_column_value(
    row: &crate::model::ResourceRow,
    index: usize,
    as_age: bool,
    now_unix: i64,
) -> String {
    if as_age {
        return format_age(row.age_seconds(now_unix));
    }
    row.table_cells
        .get(index)
        .map_or_else(String::new, |cell| cell.text.to_string())
}

fn policy_verb_allowed(row: &crate::model::ResourceRow, field: SortField) -> bool {
    let verb = match field {
        SortField::Get => "get",
        SortField::List => "list",
        SortField::Watch => "watch",
        SortField::Create => "create",
        SortField::Patch => "patch",
        SortField::Update => "update",
        SortField::Delete => "delete",
        SortField::DeleteCollection => "deletecollection",
        _ => return false,
    };
    row.policy_data().is_some_and(|data| data.allows(verb))
}

fn format_cpu(value: Option<u64>) -> String {
    value.map_or_else(|| "-".to_owned(), |millis| format!("{millis}m"))
}

fn format_memory(value: Option<u64>) -> String {
    value.map_or_else(
        || "-".to_owned(),
        |bytes| format!("{}Mi", bytes / 1_048_576),
    )
}

fn format_percent(value: Option<u64>, total: u64) -> String {
    value.filter(|_| total > 0).map_or_else(
        || "-".to_owned(),
        |value| {
            let percent = u128::from(value).saturating_mul(100) / u128::from(total);
            format!("{percent}%")
        },
    )
}

fn column_header(app: &App, label: &str, field: SortField) -> Cell<'static> {
    let selected_field = SortField::ALL[app.selected_column];
    let suffix = if app.sort_field == field {
        if app.sort_descending { " ↓" } else { " ↑" }
    } else {
        ""
    };
    let style = if selected_field == field {
        Style::default().fg(YELLOW).bold()
    } else if app.sort_field == field {
        Style::default().fg(ORANGE).bold()
    } else {
        Style::default().fg(CYAN).bold()
    };
    Cell::from(format!("{label}{suffix}")).style(style)
}

fn draw_input_view(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let (title, line) = match app.input_mode {
        InputMode::Command => (
            " Command ",
            prompt_line(
                ":",
                &app.input,
                None,
                command_completion_hint(app).as_deref(),
            ),
        ),
        InputMode::Filter => (
            " Filter ",
            prompt_line(
                "/",
                &app.input,
                app.filter_error.as_deref(),
                app.previous_filter_hint()
                    .map(|hint| format!("/{hint}"))
                    .as_deref(),
            ),
        ),
        InputMode::Browse => return,
    };
    frame.render_widget(Paragraph::new(line).block(popup_block(title)), area);
}

fn command_completion_hint(app: &App) -> Option<String> {
    let completion = app.command_suggestions().into_iter().next()?;
    completion
        .strip_prefix(&app.input)
        .filter(|suffix| !suffix.is_empty())
        .map(str::to_owned)
}

fn prompt_line(
    prefix: &str,
    input: &str,
    error: Option<&str>,
    hint: Option<&str>,
) -> Line<'static> {
    let mut spans = vec![
        Span::styled(
            format!(" {prefix}"),
            Style::default().fg(Color::Black).bg(CYAN).bold(),
        ),
        Span::styled(format!("{input}▏"), Style::default().fg(YELLOW)),
    ];
    if let Some(hint) = hint {
        spans.push(Span::styled(hint.to_owned(), Style::default().fg(MUTED)));
    }
    if let Some(error) = error {
        spans.push(Span::styled(format!("  {error}"), Style::default().fg(RED)));
    }
    Line::from(spans)
}

type HelpCategory = (&'static str, &'static [(&'static str, &'static str)]);

const HELP_CATEGORIES: &[HelpCategory] = &[
    (
        "NAVIGATION",
        &[
            ("j", "Down"),
            ("k", "Up"),
            ("h", "Left / Back"),
            ("l", "Right / Open"),
            ("g / Home", "Go to top"),
            ("Shift-G / End", "Go to bottom"),
            ("Ctrl-B / PgUp", "Page up"),
            ("Ctrl-F / PgDn", "Page down"),
            ("[", "History back"),
            ("-", "History back"),
            ("]", "History forward"),
        ],
    ),
    (
        "GENERAL",
        &[
            ("?", "Help"),
            ("Ctrl-A", "Resource aliases"),
            (":", "Command mode"),
            ("/", "Filter mode"),
            ("Esc", "Back / clear filter"),
            ("Ctrl-R", "Refresh view"),
            ("Ctrl-C", "Quit"),
            ("0…9", "Recent namespaces"),
        ],
    ),
    (
        "RESOURCES",
        &[
            ("Enter", "Open selected resource"),
            ("Space", "Mark resource"),
            ("Ctrl-Space", "Mark range"),
            ("Ctrl-\\", "Clear marks"),
            ("c / n", "Copy name / namespace"),
            ("y / d / e", "YAML / describe / edit"),
            ("l / p", "Logs / previous logs"),
        ],
    ),
    (
        "DISPLAY",
        &[
            ("Ctrl-E", "Toggle header"),
            ("Ctrl-L", "Toggle logo"),
            ("Ctrl-W", "Toggle wide columns"),
            ("Shift-← / Shift-→", "Select column"),
            ("Shift-O", "Sort selected column"),
            ("Shift-N / A", "Sort name / age"),
            ("Shift-P / S", "Sort namespace / status"),
            ("Mouse", "Click header to sort"),
        ],
    ),
    (
        "MUTATION",
        &[
            ("Ctrl-D", "Delete marked / current"),
            ("Ctrl-K", "Kill immediately"),
            ("Ctrl-Z", "Toggle faults"),
        ],
    ),
];

fn help_category_lines(categories: &[HelpCategory]) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for (category_index, (category, entries)) in categories.iter().enumerate() {
        if category_index > 0 {
            lines.push(Line::raw(""));
        }
        lines.push(Line::from(Span::styled(
            format!("  {category}"),
            Style::default().fg(CYAN).bold(),
        )));
        for (key, action) in *entries {
            lines.push(Line::from(vec![
                Span::styled(
                    format!("  {:<24}", format!("<{key}>")),
                    Style::default().fg(GREEN).bold(),
                ),
                Span::styled((*action).to_owned(), Style::default().fg(Color::White)),
            ]));
        }
    }
    lines
}

fn draw_help(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    let title = format!(" Help · {} ", app.kind.label());
    let block = notice_block(popup_block(&title).title_alignment(Alignment::Center), app);
    if area.width >= HELP_TWO_COLUMN_MIN_WIDTH {
        let inner = block.inner(area);
        let left = help_category_lines(&HELP_CATEGORIES[..2]);
        let right = help_category_lines(&HELP_CATEGORIES[2..]);
        app.set_content_scroll_extent(left.len().max(right.len()), inner.height);
        frame.render_widget(block, area);
        let columns = Layout::horizontal([
            Constraint::Percentage(50),
            Constraint::Length(2),
            Constraint::Percentage(50),
        ])
        .split(inner);
        frame.render_widget(
            Paragraph::new(left).scroll((app.content_scroll, app.content_horizontal_scroll)),
            columns[0],
        );
        frame.render_widget(
            Paragraph::new(right).scroll((app.content_scroll, app.content_horizontal_scroll)),
            columns[2],
        );
        return;
    }

    let lines = help_category_lines(HELP_CATEGORIES);
    let inner_height = block.inner(area).height;
    app.set_content_scroll_extent(lines.len(), inner_height);
    frame.render_widget(
        Paragraph::new(lines)
            .block(block)
            .scroll((app.content_scroll, app.content_horizontal_scroll)),
        area,
    );
}

fn chooser_window(visible: &[usize], selected: usize, capacity: usize) -> (usize, Vec<usize>) {
    let capacity = capacity.max(1);
    let position = visible
        .iter()
        .position(|index| *index == selected)
        .unwrap_or(0);
    let start = position
        .saturating_sub(capacity / 2)
        .min(visible.len().saturating_sub(capacity));
    (
        start,
        visible.iter().skip(start).take(capacity).copied().collect(),
    )
}

fn chooser_title(name: &str, count: usize, filter: &str, action: &str) -> String {
    let filter = if filter.is_empty() {
        String::new()
    } else {
        format!(" /{filter}")
    };
    format!(" {name}[{count}]{filter} · Enter to {action} ")
}

fn draw_empty_chooser(frame: &mut Frame<'_>, app: &App, area: Rect, title: &str, noun: &str) {
    frame.render_widget(
        Paragraph::new(format!("No {noun} match /{}", app.body_filter()))
            .alignment(Alignment::Center)
            .block(notice_block(
                popup_block(title).title_alignment(Alignment::Center),
                app,
            )),
        area,
    );
}

fn record_chooser_rows(app: &mut App, area: Rect, indices: Vec<usize>) {
    app.set_chooser_hitbox(
        area.x.saturating_add(1),
        area.y.saturating_add(2),
        area.width.saturating_sub(2),
        indices,
    );
}

fn draw_aliases(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    let visible = app.visible_resource_indices();
    let title = chooser_title("resources", visible.len(), app.body_filter(), "open");
    if visible.is_empty() {
        draw_empty_chooser(frame, app, area, &title, "resources");
        return;
    }
    let (_, displayed) = chooser_window(
        &visible,
        app.overlay_selected,
        usize::from(area.height.saturating_sub(3)),
    );
    let rows = displayed
        .iter()
        .filter_map(|index| {
            app.resource_options
                .get(*index)
                .map(|resource| (*index, resource))
        })
        .map(|(index, resource)| {
            Row::new([
                Cell::from(resource.qualified_name()),
                Cell::from(resource.command_aliases().join(" ")),
            ])
            .style(if index == app.overlay_selected {
                Style::default().fg(Color::White).bg(SELECTED).bold()
            } else {
                Style::default()
            })
        })
        .collect::<Vec<_>>();
    record_chooser_rows(app, area, displayed);
    frame.render_widget(
        Table::new(
            rows,
            [Constraint::Percentage(35), Constraint::Percentage(65)],
        )
        .header(Row::new(["RESOURCE", "ALIASES"]).style(Style::default().fg(YELLOW).bold()))
        .block(notice_block(
            popup_block(&title).title_alignment(Alignment::Center),
            app,
        )),
        area,
    );
}

fn draw_namespaces(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    let visible = app.visible_namespace_indices();
    let title = chooser_title("namespaces", visible.len(), app.body_filter(), "switch");
    if visible.is_empty() {
        draw_empty_chooser(frame, app, area, &title, "namespaces");
        return;
    }
    let (_, displayed) = chooser_window(
        &visible,
        app.overlay_selected,
        usize::from(area.height.saturating_sub(3)),
    );
    let rows = displayed
        .iter()
        .filter_map(|index| {
            app.namespace_options
                .get(*index)
                .map(|namespace| (*index, namespace))
        })
        .map(|(index, namespace)| {
            let age = namespace
                .age_seconds(app.now_unix)
                .map_or_else(|| "-".to_owned(), format_age);
            Row::new([
                Cell::from(namespace.name.clone()),
                Cell::from(namespace.status.clone()),
                Cell::from(age),
            ])
            .style(if index == app.overlay_selected {
                Style::default().fg(Color::White).bg(SELECTED).bold()
            } else {
                Style::default()
            })
        })
        .collect::<Vec<_>>();
    record_chooser_rows(app, area, displayed);
    frame.render_widget(
        Table::new(
            rows,
            [
                Constraint::Min(20),
                Constraint::Length(14),
                Constraint::Length(10),
            ],
        )
        .header(Row::new(["NAME", "STATUS", "AGE"]).style(Style::default().fg(YELLOW).bold()))
        .block(notice_block(
            popup_block(&title).title_alignment(Alignment::Center),
            app,
        )),
        area,
    );
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ContextColumn {
    Active,
    Name,
    Cluster,
    User,
    Namespace,
}

fn context_column_width(app: &App, column: ContextColumn) -> u16 {
    let header = match column {
        ContextColumn::Active => "",
        ContextColumn::Name => "NAME",
        ContextColumn::Cluster => "CLUSTER",
        ContextColumn::User => "USER",
        ContextColumn::Namespace => "NAMESPACE",
    };
    let values = app.context_options.iter().map(|context| match column {
        ContextColumn::Active => "*",
        ContextColumn::Name => context.name.as_str(),
        ContextColumn::Cluster => context.cluster_display(),
        ContextColumn::User => context.user_display(),
        ContextColumn::Namespace => context.namespace_display(),
    });
    std::iter::once(header)
        .chain(values)
        .map(|value| Line::raw(value).width())
        .max()
        .and_then(|width| u16::try_from(width).ok())
        .unwrap_or(0)
}

fn context_columns(app: &App, area_width: u16) -> Vec<(ContextColumn, u16)> {
    let available = area_width.saturating_sub(2);
    let mut columns = Vec::new();
    let mut used = 0_u16;
    for column in [
        ContextColumn::Active,
        ContextColumn::Name,
        ContextColumn::Cluster,
        ContextColumn::User,
        ContextColumn::Namespace,
    ] {
        let width = context_column_width(app, column);
        let spacing = u16::from(!columns.is_empty());
        if columns.len() < 2 || used.saturating_add(spacing).saturating_add(width) <= available {
            used = used.saturating_add(spacing).saturating_add(width);
            columns.push((column, width));
        }
    }
    columns
}

fn draw_contexts(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    let visible = app.visible_context_indices();
    let title = chooser_title("contexts", visible.len(), app.body_filter(), "switch");
    if visible.is_empty() {
        draw_empty_chooser(frame, app, area, &title, "contexts");
        return;
    }
    let (_, displayed) = chooser_window(
        &visible,
        app.overlay_selected,
        usize::from(area.height.saturating_sub(3)),
    );
    let columns = context_columns(app, area.width);
    let rows = displayed
        .iter()
        .filter_map(|index| {
            app.context_options
                .get(*index)
                .map(|context| (*index, context))
        })
        .map(|(index, context)| {
            let cells = columns
                .iter()
                .map(|(column, _)| match column {
                    ContextColumn::Active => Cell::from(if context.name == app.context {
                        "*"
                    } else {
                        " "
                    }),
                    ContextColumn::Name => Cell::from(context.name.clone()),
                    ContextColumn::Cluster => Cell::from(context.cluster_display().to_owned()),
                    ContextColumn::User => Cell::from(context.user_display().to_owned()),
                    ContextColumn::Namespace => Cell::from(context.namespace_display().to_owned()),
                })
                .collect::<Vec<_>>();
            Row::new(cells).style(if index == app.overlay_selected {
                Style::default().fg(Color::White).bg(SELECTED).bold()
            } else {
                Style::default()
            })
        })
        .collect::<Vec<_>>();
    let header = Row::new(columns.iter().map(|(column, _)| match column {
        ContextColumn::Active => "",
        ContextColumn::Name => "NAME",
        ContextColumn::Cluster => "CLUSTER",
        ContextColumn::User => "USER",
        ContextColumn::Namespace => "NAMESPACE",
    }))
    .style(Style::default().fg(YELLOW).bold());
    let constraints = columns
        .iter()
        .map(|(_, width)| Constraint::Length(*width))
        .collect::<Vec<_>>();
    record_chooser_rows(app, area, displayed);
    frame.render_widget(
        Table::new(rows, constraints)
            .column_spacing(1)
            .header(header)
            .block(notice_block(
                popup_block(&title).title_alignment(Alignment::Center),
                app,
            )),
        area,
    );
}

fn draw_details(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    let Some(row) = app.details_row.as_ref() else {
        return;
    };
    let age = format_age(row.age_seconds(app.now_unix));
    let lines = vec![
        detail_line("Kind", app.kind.label()),
        detail_line("Namespace", row.namespace.as_ref()),
        detail_line("Name", row.name.as_ref()),
        detail_line("Status", row.status.as_ref()),
        detail_line("Ready", row.ready.as_ref()),
        detail_line("Details", row.details.as_ref()),
        detail_line("Labels", row.labels.as_ref()),
        detail_line("Age", &age),
    ]
    .into_iter()
    .filter(|line| app.body_line_matches(&line.to_string()))
    .collect::<Vec<_>>();
    let filter = if app.body_filter().is_empty() {
        String::new()
    } else {
        format!(" /{}", app.body_filter())
    };
    let title = format!(" details({}/{}){filter} ", row.namespace, row.name);
    let block = notice_block(popup_block(&title).title_alignment(Alignment::Center), app);
    let inner_height = block.inner(area).height;
    app.set_content_scroll_extent(lines.len(), inner_height);
    frame.render_widget(
        Paragraph::new(lines)
            .block(block)
            .scroll((app.content_scroll, app.content_horizontal_scroll)),
        area,
    );
}

fn draw_content(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    let body = app.active_content_body();
    let filtered = if app.content_kind == Some(ContentKind::Yaml) {
        body.to_owned()
    } else {
        body.lines()
            .filter(|line| app.body_line_matches(line))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let lines = match app.content_kind {
        Some(ContentKind::Yaml | ContentKind::SecretData) => highlight_yaml(&filtered),
        Some(ContentKind::Describe) => highlight_describe(&filtered),
        _ => filtered
            .lines()
            .map(|line| Line::raw(line.to_owned()))
            .collect(),
    };
    let filter = if app.body_filter().is_empty() {
        String::new()
    } else {
        format!(" /{}", app.body_filter())
    };
    let title = format!(" {}{filter} ", app.content_title);
    let block = notice_block(popup_block(&title).title_alignment(Alignment::Center), app);
    let inner = block.inner(area);
    let wrap = app.ui.wrap();
    let content_height = paragraph_content_height(&lines, inner.width, wrap);
    app.set_content_scroll_extent(content_height, inner.height);
    let paragraph = Paragraph::new(lines)
        .block(block)
        .scroll((app.content_scroll, app.content_horizontal_scroll));
    let paragraph = if wrap {
        paragraph.wrap(Wrap { trim: false })
    } else {
        paragraph
    };
    frame.render_widget(paragraph, area);
}

fn paragraph_content_height(lines: &[Line<'_>], width: u16, wrap: bool) -> usize {
    if !wrap {
        return lines.len();
    }
    let width = usize::from(width.max(1));
    lines
        .iter()
        .map(|line| line.width().max(1).div_ceil(width))
        .sum()
}

fn highlight_yaml(body: &str) -> Vec<Line<'static>> {
    body.lines().map(highlight_yaml_line).collect()
}

fn highlight_yaml_line(line: &str) -> Line<'static> {
    let owned = line.to_owned();
    let trimmed = line.trim_start();
    if trimmed.starts_with('#') || trimmed == "---" {
        return Line::from(Span::styled(owned, Style::default().fg(MUTED).italic()));
    }
    let indentation = &line[..line.len().saturating_sub(trimmed.len())];
    let (list_marker, content) = trimmed
        .strip_prefix("- ")
        .map_or(("", trimmed), |content| ("- ", content));
    let Some(colon) = content.find(':') else {
        return Line::from(vec![
            Span::raw(indentation.to_owned()),
            Span::styled(list_marker.to_owned(), Style::default().fg(PURPLE)),
            Span::styled(content.to_owned(), yaml_scalar_style(content)),
        ]);
    };
    let key = &content[..colon];
    let value = &content[colon + 1..];
    Line::from(vec![
        Span::raw(indentation.to_owned()),
        Span::styled(list_marker.to_owned(), Style::default().fg(PURPLE)),
        Span::styled(key.to_owned(), Style::default().fg(CYAN).bold()),
        Span::styled(":", Style::default().fg(MUTED)),
        Span::styled(value.to_owned(), yaml_scalar_style(value.trim())),
    ])
}

fn yaml_scalar_style(value: &str) -> Style {
    if matches!(value, "true" | "false" | "null" | "~") {
        Style::default().fg(PURPLE)
    } else if value.parse::<f64>().is_ok() {
        Style::default().fg(ORANGE)
    } else if value.starts_with('"') || value.starts_with('\'') {
        Style::default().fg(GREEN)
    } else {
        Style::default().fg(YELLOW)
    }
}

fn highlight_describe(body: &str) -> Vec<Line<'static>> {
    body.lines().map(highlight_describe_line).collect()
}

fn highlight_describe_line(line: &str) -> Line<'static> {
    let trimmed = line.trim_start();
    let indentation = &line[..line.len().saturating_sub(trimmed.len())];
    let Some(colon) = trimmed.find(':') else {
        let style = if trimmed.starts_with("Warning") {
            Style::default().fg(RED)
        } else {
            Style::default()
        };
        return Line::from(vec![
            Span::raw(indentation.to_owned()),
            Span::styled(trimmed.to_owned(), style),
        ]);
    };
    let key = &trimmed[..colon];
    let value = &trimmed[colon + 1..];
    Line::from(vec![
        Span::raw(indentation.to_owned()),
        Span::styled(key.to_owned(), Style::default().fg(CYAN).bold()),
        Span::styled(":", Style::default().fg(MUTED)),
        Span::styled(value.to_owned(), describe_value_style(value.trim())),
    ])
}

fn describe_value_style(value: &str) -> Style {
    if value.is_empty() || value == "<none>" {
        Style::default().fg(MUTED)
    } else if matches!(value, "Running" | "Ready" | "Active") {
        Style::default().fg(GREEN)
    } else if matches!(value, "Failed" | "NotReady" | "Error") {
        Style::default().fg(RED)
    } else {
        Style::default().fg(YELLOW)
    }
}

fn draw_delete_confirmation(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let prompt = match app.pending_delete.as_ref() {
        Some(PendingDelete::Resources(targets)) if targets.len() == 1 => {
            let target = &targets[0];
            format!("Delete {} {}?", target.kind.label(), target.name)
        }
        Some(PendingDelete::Resources(targets)) => {
            let resource = targets
                .first()
                .map_or("resources", |target| target.kind.plural_name());
            format!("Delete all {} selected {resource}?", targets.len())
        }
        Some(PendingDelete::Context { name }) => format!("Delete context {name}?"),
        None => "Delete resource?".to_owned(),
    };
    let buttons = if app.confirm_armed {
        Line::from(vec![
            Span::styled(" [ Cancel ] ", Style::default().fg(MUTED)),
            Span::styled(" [ OK ] ", Style::default().fg(Color::White).bg(RED).bold()),
        ])
    } else {
        Line::from(vec![
            Span::styled(
                " [ Cancel ] ",
                Style::default().fg(Color::White).bg(SELECTED).bold(),
            ),
            Span::styled(" [ OK ] ", Style::default().fg(MUTED)),
        ])
    };
    let popup = centered_rect(60, 9, area);
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(""),
            Line::from(Span::styled(prompt, Style::default().fg(RED).bold())).centered(),
            Line::from(""),
            buttons.centered(),
            Line::from(""),
            Line::from(Span::styled(
                "Tab selects OK · Enter confirms · Esc cancels",
                Style::default().fg(MUTED),
            ))
            .centered(),
        ])
        .block(popup_block(" Confirm Delete ").border_style(Style::default().fg(RED))),
        popup,
    );
}

fn detail_line(label: &str, value: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("  {label:<12}"), Style::default().fg(MUTED)),
        Span::raw(value.to_owned()),
    ])
}

fn popup_block(title: &str) -> Block<'_> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(CYAN))
        .title(Span::styled(title, Style::default().fg(YELLOW).bold()))
}

fn notice_block<'a>(block: Block<'a>, app: &App) -> Block<'a> {
    if let Some(notice) = &app.notice {
        block.title_bottom(
            Line::from(Span::styled(
                format!(" {notice} "),
                Style::default().fg(GREEN).bold(),
            ))
            .right_aligned(),
        )
    } else {
        block
    }
}

fn status_style(status: &str) -> Style {
    let normalized = status.trim().to_ascii_lowercase();
    let color = if normalized == "terminating" {
        MUTED
    } else if normalized == "false"
        || normalized.starts_with("false ")
        || normalized.contains("failed")
        || normalized.contains("error")
        || normalized.contains("crashloop")
        || normalized.contains("notready")
    {
        RED
    } else if matches!(
        normalized.as_str(),
        "pending" | "progressing" | "unknown" | "scheduled" | "unschedulable"
    ) {
        YELLOW
    } else if matches!(
        normalized.as_str(),
        "running" | "ready" | "succeeded" | "true" | "active" | "available" | "bound"
    ) || normalized.ends_with("ready")
    {
        GREEN
    } else {
        return Style::default();
    };
    Style::default().fg(color)
}

fn centered_rect(percent_x: u16, height: u16, area: Rect) -> Rect {
    let vertical = Layout::vertical([
        Constraint::Fill(1),
        Constraint::Length(height.min(area.height)),
        Constraint::Fill(1),
    ])
    .split(area);
    Layout::horizontal([
        Constraint::Percentage((100 - percent_x) / 2),
        Constraint::Percentage(percent_x),
        Constraint::Percentage((100 - percent_x) / 2),
    ])
    .split(vertical[1])[1]
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Instant};

    use crossterm::event::{
        KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };
    use ratatui::{Terminal, backend::TestBackend};

    use super::*;
    use crate::{
        kubeconfig::ContextInfo,
        model::{
            NamespaceInfo, NodeData, PodData, PolicyData, ResourceDescriptor, ResourceOperations,
            ResourceRow, ResourceSnapshot, TableCell, TableColumn,
        },
    };

    fn rendered_lines(terminal: &Terminal<TestBackend>, width: u16) -> Vec<String> {
        terminal
            .backend()
            .buffer()
            .content()
            .chunks(usize::from(width))
            .map(|row| row.iter().map(ratatui::buffer::Cell::symbol).collect())
            .collect()
    }

    fn type_command(app: &mut App, command: &str) {
        app.handle_key(KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE));
        for character in command.chars() {
            app.handle_key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE));
        }
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    }

    fn app_with_rendered_pod() -> App {
        let mut app = App::new(
            "demo",
            Some("default".to_owned()),
            ResourceKind::Pods,
            false,
            vec!["demo".to_owned()],
        );
        app.apply_snapshot(ResourceSnapshot {
            generation: app.generation,
            kind: ResourceKind::Pods.into(),
            rows: Arc::new(vec![Arc::new(ResourceRow::new(
                "default",
                "api-0",
                "Running",
                "1/1",
                "0",
                app.now_unix - 60,
            ))]),
            table_columns: Arc::from([]),
            state: SourceState::Ready,
            revision: 1,
            emitted_at: Instant::now(),
        });
        app
    }

    fn header_test_app() -> App {
        let context = "gke_sncloud-test_us-west1-long-context";
        let mut app = App::new(
            context,
            Some("sn-system".to_owned()),
            ResourceKind::Pods,
            false,
            vec![ContextInfo {
                name: context.to_owned(),
                cluster: Some("gke_sncloud-test-us-west1".to_owned()),
                user: Some("max@example.com".to_owned()),
                namespace: Some("sn-system".to_owned()),
                kubeconfig_backed: true,
            }],
        );
        app.recent_namespaces = vec![
            "sn-system".to_owned(),
            "namespace-two".to_owned(),
            "namespace-three".to_owned(),
            "namespace-four".to_owned(),
            "namespace-five".to_owned(),
            "namespace-six".to_owned(),
            "namespace-seven".to_owned(),
            "namespace-eight".to_owned(),
            "namespace-nine".to_owned(),
        ];
        app.set_server_version("v1.34.8-gke.1278000");
        app
    }

    #[test]
    fn header_layout_reserves_full_info_and_culls_whole_hotkey_columns() {
        let app = header_test_app();
        let all_columns = header_hint_columns(&app);
        let layout = header_layout(&app, 140);

        assert_eq!(layout.flow, HeaderFlow::Horizontal);
        assert!(layout.show_logo);
        assert_eq!(layout.info_width, cluster_info_width(&app));
        assert!(layout.columns.len() < all_columns.len());
        assert!(layout.columns.len() >= 2);
        assert!(layout.columns[..2].iter().all(|column| {
            column
                .hints
                .first()
                .is_some_and(|hint| hint.kind == HeaderHintKind::Namespace)
        }));
        assert!(header_columns_width(&layout.columns) <= layout.menu_width);
    }

    #[test]
    fn logo_rows_are_classic_ascii_and_cell_aligned() {
        assert_eq!(
            LOGO_ROWS,
            [
                " _  __ _   _ __  __",
                "| |/ /| | | |\\ \\/ /",
                "| ' / | | | | \\  / ",
                "| . \\ | |_| | /  \\ ",
                "|_|\\_\\ \\___/ /_/\\_\\",
            ]
        );
        let widths: Vec<_> = LOGO_ROWS
            .iter()
            .map(|row| Line::raw(*row).width())
            .collect();

        assert!(
            widths
                .iter()
                .all(|width| *width == usize::from(logo_width()))
        );
        assert!(LOGO_ROWS.iter().all(|row| row.is_ascii()));
    }

    #[test]
    fn rendered_header_keeps_full_context_and_complete_hotkeys() {
        let mut app = header_test_app();
        let backend = TestBackend::new(140, 36);
        let mut terminal = Terminal::new(backend).expect("header terminal");

        terminal
            .draw(|frame| draw(frame, &mut app))
            .expect("draw measured header");
        let lines = rendered_lines(&terminal, 140);
        let rendered = lines.join("\n");

        assert!(rendered.contains("Context: gke_sncloud-test_us-west1-long-context"));
        assert!(rendered.contains("Cluster: gke_sncloud-test-us-west1"));
        assert!(rendered.contains("User:    max@example.com"));
        assert!(!rendered.contains("Namespace:"));
        assert!(rendered.contains("<0> all"));
        assert!(!rendered.contains("Kill"));
        for (index, row) in LOGO_ROWS.iter().enumerate() {
            assert_eq!(&lines[index][..usize::from(logo_width())], *row);
        }
    }

    #[test]
    fn narrow_header_wraps_info_without_overlapping_the_body() {
        let mut app = header_test_app();
        let width = 32;
        let layout = header_layout(&app, width);
        assert_eq!(layout.flow, HeaderFlow::Stacked);
        assert!(!layout.show_logo);
        assert!(layout.info_height > 4);

        let backend = TestBackend::new(width, 64);
        let mut terminal = Terminal::new(backend).expect("narrow terminal");
        terminal
            .draw(|frame| draw(frame, &mut app))
            .expect("draw wrapped header");
        let lines = rendered_lines(&terminal, width);
        let header_without_spaces: String = lines[..usize::from(layout.info_height)]
            .join("")
            .chars()
            .filter(|character| !character.is_whitespace())
            .collect();
        let body_row = lines
            .iter()
            .position(|line| line.contains("pods(sn-system)[0]"))
            .expect("resource title");

        assert!(header_without_spaces.contains(app.context.as_str()));
        assert!(body_row >= usize::from(layout.height));
    }

    #[test]
    fn header_and_logo_toggles_still_control_rendering() {
        let mut app = header_test_app();
        app.set_logo_visible(false);
        let backend = TestBackend::new(180, 30);
        let mut terminal = Terminal::new(backend).expect("toggle terminal");

        terminal
            .draw(|frame| draw(frame, &mut app))
            .expect("draw without logo");
        let lines = rendered_lines(&terminal, 180);
        let rendered = lines.join("\n");
        assert!(!rendered.contains(LOGO_ROWS[0]));
        assert!(rendered.contains(app.context.as_str()));
        assert!(lines[0].starts_with("Context:"));
        let layout = header_layout(&app, 180);
        let body_row = lines
            .iter()
            .position(|line| line.contains("pods(sn-system)[0]"))
            .expect("resource title");
        assert_eq!(body_row, usize::from(layout.height) + 1);

        app.handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL));
        terminal
            .draw(|frame| draw(frame, &mut app))
            .expect("draw without header");
        let lines = rendered_lines(&terminal, 180);
        let rendered = lines.join("\n");
        assert!(!rendered.contains("Context:"));
        assert!(!rendered.contains("Command"));
        assert!(lines[0].contains("pods(sn-system)[0]"));
    }

    #[test]
    fn generic_resource_status_and_ready_columns_use_bounded_widths() {
        let kind = ResourceDescriptor::new(
            "config.streamnative.io",
            "v1alpha1",
            "config.streamnative.io/v1alpha1",
            "CloudEnvironment",
            "cloudenvironments",
            true,
            ResourceOperations::all(),
        );
        let mut app = App::new(
            "demo",
            None,
            ResourceKind::Pods,
            false,
            vec!["demo".to_owned()],
        );
        app.kind = kind.clone();
        app.apply_snapshot(ResourceSnapshot {
            generation: app.generation,
            kind,
            rows: Arc::new(vec![
                Arc::new(ResourceRow::new(
                    "o-for6u",
                    "aws-usw1-dev-obc99",
                    "KubernetesReady",
                    "-",
                    "-",
                    app.now_unix - 3_600,
                )),
                Arc::new(ResourceRow::new(
                    "o-for6u",
                    "aws-usw1-production-m2v3f",
                    "ConnectivityReady",
                    "-",
                    "-",
                    app.now_unix - 3_600,
                )),
            ]),
            table_columns: Arc::from([]),
            state: SourceState::Ready,
            revision: 1,
            emitted_at: Instant::now(),
        });

        let columns = columns_for_view(&app, 200);
        let widths = columns
            .iter()
            .map(|column| column.width)
            .collect::<Vec<_>>();
        let resolved = Layout::horizontal(widths)
            .spacing(1)
            .split(Rect::new(0, 0, 198, 1));
        let width_for = |field| {
            columns
                .iter()
                .position(|column| column.field == Some(field))
                .map(|index| resolved[index].width)
                .expect("column exists")
        };

        assert_eq!(width_for(SortField::Status), 20);
        assert_eq!(width_for(SortField::Ready), 7);
    }

    #[test]
    fn renders_k9s_style_main_view_and_overlays() {
        let mut app = App::new(
            "demo",
            None,
            ResourceKind::Pods,
            false,
            vec!["demo".to_owned()],
        );
        app.apply_snapshot(ResourceSnapshot {
            generation: 1,
            kind: ResourceKind::Pods.into(),
            rows: Arc::new(vec![Arc::new(
                ResourceRow::new("default", "api-0", "Running", "1/1", "0", app.now_unix - 60)
                    .with_labels("app=api"),
            )]),
            table_columns: Arc::from([]),
            state: SourceState::Ready,
            revision: 1,
            emitted_at: Instant::now(),
        });
        app.set_server_version("v1.32.1");
        let backend = TestBackend::new(140, 40);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| draw(frame, &mut app))
            .expect("draw main view");
        let rendered: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect();
        assert!(rendered.contains("pods(all)[1]"));
        assert!(rendered.contains("v1.32.1"));
        assert!(!rendered.contains("Recent namespaces"));
        assert!(!rendered.contains("watching"));
        assert!(rendered.contains("<0> all"));
        app.overlay = Overlay::Help;
        terminal
            .draw(|frame| draw(frame, &mut app))
            .expect("draw help");
        app.overlay = Overlay::Aliases;
        terminal
            .draw(|frame| draw(frame, &mut app))
            .expect("draw aliases");
        app.show_content(
            "YAML(default/api-0)",
            "kind: Pod\nmetadata:\n  name: api-0",
            None,
            None,
            None,
            ContentKind::Yaml,
        );
        terminal
            .draw(|frame| draw(frame, &mut app))
            .expect("draw content");
        let lines = rendered_lines(&terminal, 140);
        let title = lines
            .iter()
            .find(|line| line.contains("YAML(default/api-0)"))
            .expect("in-place content title");
        assert!(title.starts_with('╭'));
        assert!(!title.contains("c copy"));
        assert!(lines.iter().any(|line| line.contains("Copy")));
        let metadata_column = lines
            .iter()
            .find_map(|line| line.find("metadata:"))
            .expect("metadata line");
        let name_column = lines
            .iter()
            .find_map(|line| line.find("name: api-0"))
            .expect("nested name line");
        assert_eq!(name_column, metadata_column + 2);
        assert!(!lines.iter().any(|line| line.contains("NAME       STATUS")));
    }

    #[test]
    fn responsive_layout_renders_at_narrow_and_wide_sizes() {
        for (width, height) in [(50, 18), (80, 24), (200, 45)] {
            let mut app = App::new(
                "demo",
                Some("default".to_owned()),
                ResourceKind::Pods,
                false,
                vec!["demo".to_owned()],
            );
            let backend = TestBackend::new(width, height);
            let mut terminal = Terminal::new(backend).expect("test terminal");
            terminal
                .draw(|frame| draw(frame, &mut app))
                .expect("draw responsive layout");
            let rendered = rendered_lines(&terminal, width).join("\n");
            assert!(rendered.contains("<0> all"));
            assert!(rendered.contains("<1> default"));
            app.handle_key(crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Char(':'),
                crossterm::event::KeyModifiers::NONE,
            ));
            terminal
                .draw(|frame| draw(frame, &mut app))
                .expect("draw command completion palette");
        }
    }

    #[test]
    fn context_chooser_replaces_the_body_instead_of_using_a_popup() {
        let mut app = App::new(
            "demo",
            Some("default".to_owned()),
            ResourceKind::Pods,
            false,
            vec![
                ContextInfo::synthetic("demo"),
                ContextInfo {
                    name: "production".to_owned(),
                    cluster: Some("production-cluster".to_owned()),
                    user: Some("release-bot".to_owned()),
                    namespace: None,
                    kubeconfig_backed: true,
                },
            ],
        );
        app.handle_key(KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE));
        for character in "ctx".chars() {
            app.handle_key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE));
        }
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.overlay, Overlay::Contexts);

        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).expect("context terminal");
        terminal
            .draw(|frame| draw(frame, &mut app))
            .expect("draw contexts in body");
        let lines = rendered_lines(&terminal, 100);
        let title = lines
            .iter()
            .find(|line| line.contains("contexts[2]"))
            .expect("context body title");
        assert!(title.starts_with('╭'));
        assert!(lines.iter().any(|line| {
            line.contains("NAME")
                && line.contains("CLUSTER")
                && line.contains("USER")
                && line.contains("NAMESPACE")
        }));
        assert!(lines.iter().any(|line| line.contains("production")));
        assert!(lines.iter().any(|line| line.contains("production-cluster")));
        assert!(lines.iter().any(|line| line.contains("release-bot")));
        assert!(lines.iter().any(|line| line.contains("default")));
        assert!(!lines.iter().any(|line| line.contains("pods(default)")));
    }

    #[test]
    fn narrow_context_table_omits_whole_trailing_columns() {
        let mut app = App::new(
            "prod",
            None,
            ResourceKind::Pods,
            false,
            vec![ContextInfo {
                name: "production".to_owned(),
                cluster: Some("production-cluster".to_owned()),
                user: Some("release-bot".to_owned()),
                namespace: None,
                kubeconfig_backed: true,
            }],
        );
        type_command(&mut app, "ctx");
        let backend = TestBackend::new(32, 24);
        let mut terminal = Terminal::new(backend).expect("narrow context terminal");

        terminal
            .draw(|frame| draw(frame, &mut app))
            .expect("draw narrow contexts");
        let rendered = rendered_lines(&terminal, 32).join("\n");

        assert!(rendered.contains("NAME"));
        assert!(rendered.contains("production"));
        assert!(!rendered.contains("CLUST"));
        assert!(!rendered.contains("production-"));
    }

    #[test]
    fn namespace_and_resource_choosers_replace_the_body() {
        let mut app = App::new(
            "demo",
            Some("default".to_owned()),
            ResourceKind::Pods,
            false,
            vec!["demo".to_owned()],
        );
        let now = app.now_unix;
        app.set_namespace_options(
            "demo",
            vec![
                NamespaceInfo::discovered("default", "Active", Some(now - 3_600)),
                NamespaceInfo::discovered("production", "Terminating", Some(now - 86_400)),
            ],
        );
        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).expect("chooser terminal");

        type_command(&mut app, "ns");
        terminal
            .draw(|frame| draw(frame, &mut app))
            .expect("draw namespaces");
        let lines = rendered_lines(&terminal, 100);
        let title = lines
            .iter()
            .find(|line| line.contains("namespaces[3]"))
            .expect("namespace body title");
        assert!(title.starts_with('╭'));
        assert!(lines.iter().any(|line| {
            line.contains("NAME") && line.contains("STATUS") && line.contains("AGE")
        }));
        assert!(lines.iter().any(|line| line.contains("production")));
        assert!(
            lines
                .iter()
                .any(|line| line.contains("Active") && line.contains("1h"))
        );
        assert!(
            lines
                .iter()
                .any(|line| line.contains("Terminating") && line.contains("1d"))
        );
        let all_row = lines
            .iter()
            .find(|line| line.contains("<all>"))
            .expect("all namespaces row");
        assert!(all_row.matches('-').count() >= 2);
        assert!(!lines.iter().any(|line| line.contains("pods(default)")));
        let production_row = u16::try_from(
            lines
                .iter()
                .position(|line| line.contains("production"))
                .expect("production row"),
        )
        .expect("row index fits u16");
        assert!(app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 2,
            row: production_row,
            modifiers: KeyModifiers::NONE,
        }));
        assert_eq!(
            app.namespace_options[app.overlay_selected].name,
            "production"
        );

        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL));
        terminal
            .draw(|frame| draw(frame, &mut app))
            .expect("draw resources");
        let lines = rendered_lines(&terminal, 100);
        let title = lines
            .iter()
            .find(|line| line.contains("resources[6]"))
            .expect("resource chooser body title");
        assert!(title.starts_with('╭'));
        assert!(lines.iter().any(|line| line.contains("RESOURCE")));
        assert!(!lines.iter().any(|line| line.contains("pods(default)")));
    }

    #[test]
    fn chooser_no_match_is_visible_in_the_body() {
        let mut app = App::new(
            "demo",
            None,
            ResourceKind::Pods,
            false,
            vec!["demo".to_owned()],
        );
        app.overlay = Overlay::Aliases;
        app.body_query = "never-matches".to_owned();
        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).expect("empty chooser terminal");

        terminal
            .draw(|frame| draw(frame, &mut app))
            .expect("draw empty chooser");
        let rendered = rendered_lines(&terminal, 100).join("\n");
        assert!(rendered.contains("resources[0] /never-matches"));
        assert!(rendered.contains("No resources match /never-matches"));
    }

    #[test]
    fn help_renders_categorized_in_the_body_while_delete_remains_a_popup() {
        let mut app = app_with_rendered_pod();
        let backend = TestBackend::new(160, 40);
        let mut terminal = Terminal::new(backend).expect("modal terminal");

        app.overlay = Overlay::Help;
        terminal
            .draw(|frame| draw(frame, &mut app))
            .expect("draw help body");
        let help_lines = rendered_lines(&terminal, 160);
        let help = help_lines
            .into_iter()
            .find(|line| line.contains("Help · Pods"))
            .expect("help title");
        assert!(help.starts_with('╭'));
        let help_lines = rendered_lines(&terminal, 160);
        let rendered = help_lines.join("\n");
        assert!(rendered.contains("NAVIGATION"));
        assert!(rendered.contains("GENERAL"));
        assert!(rendered.contains("RESOURCES"));
        assert!(rendered.contains("<j>"));
        assert!(rendered.contains("Down"));
        let first_category_row = help_lines
            .iter()
            .find(|line| line.contains("NAVIGATION"))
            .expect("first help category row");
        assert!(first_category_row.contains("RESOURCES"));
        assert!(
            first_category_row
                .find("RESOURCES")
                .is_some_and(|column| column > 80)
        );

        app.overlay = Overlay::None;
        app.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL));
        terminal
            .draw(|frame| draw(frame, &mut app))
            .expect("draw delete popup");
        let confirm = rendered_lines(&terminal, 160)
            .into_iter()
            .find(|line| line.contains("Confirm Delete"))
            .expect("confirmation title");
        assert!(!confirm.starts_with('╭'));
    }

    #[test]
    fn delete_confirmation_identifies_the_full_marked_batch() {
        let mut app = app_with_rendered_pod();
        let target = crate::app::ResourceTarget {
            kind: ResourceKind::Pods.into(),
            namespace: Some("default".to_owned()),
            name: "api-0".to_owned(),
        };
        app.pending_delete = Some(PendingDelete::Resources(vec![
            target.clone(),
            crate::app::ResourceTarget {
                name: "api-1".to_owned(),
                ..target
            },
        ]));
        app.overlay = Overlay::ConfirmDelete;
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).expect("batch delete terminal");

        terminal
            .draw(|frame| draw(frame, &mut app))
            .expect("draw batch delete confirmation");
        let rendered = rendered_lines(&terminal, 120).join("\n");
        assert!(rendered.contains("Delete all 2 selected pods?"));
    }

    #[test]
    fn namespace_body_advertises_yaml_for_selected_resources() {
        let mut app = app_with_rendered_pod();
        app.overlay = Overlay::Namespaces;

        assert!(
            action_hints(&app)
                .iter()
                .any(|hint| hint.key == "y" && hint.label == "YAML")
        );
    }

    #[test]
    fn resource_header_reports_the_marked_delete_count() {
        let mut app = app_with_rendered_pod();
        app.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE));

        assert!(
            action_hints(&app)
                .iter()
                .any(|hint| hint.key == "ctrl-d" && hint.label == "Delete (1)")
        );
    }

    #[test]
    fn context_body_advertises_copy_for_the_selected_context() {
        let mut app = app_with_rendered_pod();
        app.overlay = Overlay::Contexts;

        assert!(
            action_hints(&app)
                .iter()
                .any(|hint| hint.key == "c" && hint.label == "Copy Name")
        );
    }

    #[test]
    fn resource_edit_hint_is_available_only_when_mutations_are_enabled() {
        let mut app = app_with_rendered_pod();
        for overlay in [Overlay::None, Overlay::Namespaces, Overlay::Details] {
            app.overlay = overlay;
            assert!(
                action_hints(&app)
                    .iter()
                    .any(|hint| hint.key == "e" && hint.label == "Edit"),
                "{overlay:?}"
            );
        }

        app.read_only = true;
        for overlay in [Overlay::None, Overlay::Namespaces, Overlay::Details] {
            app.overlay = overlay;
            assert!(
                action_hints(&app).iter().all(|hint| hint.key != "e"),
                "{overlay:?}"
            );
        }
    }

    #[test]
    fn context_delete_hint_and_confirmation_name_the_target() {
        let mut app = App::new(
            "prod",
            None,
            ResourceKind::Pods,
            false,
            vec![
                ContextInfo {
                    name: "dev".to_owned(),
                    cluster: Some("kind-dev".to_owned()),
                    user: Some("developer".to_owned()),
                    namespace: None,
                    kubeconfig_backed: true,
                },
                ContextInfo {
                    name: "prod".to_owned(),
                    cluster: Some("gke-prod".to_owned()),
                    user: Some("alice".to_owned()),
                    namespace: None,
                    kubeconfig_backed: true,
                },
            ],
        );
        type_command(&mut app, "ctx");
        app.overlay_selected = app
            .context_options
            .iter()
            .position(|item| item.name == "dev")
            .expect("dev context");
        assert!(
            action_hints(&app)
                .iter()
                .any(|hint| hint.key == "ctrl-d" && hint.label == "Delete")
        );

        app.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL));
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).expect("context delete terminal");
        terminal
            .draw(|frame| draw(frame, &mut app))
            .expect("draw context delete confirmation");
        let rendered = rendered_lines(&terminal, 100).join("\n");
        assert!(rendered.contains("Delete context dev?"));
    }

    #[test]
    fn secret_decode_hotkey_and_copy_notice_stay_in_place() {
        let mut app = App::new(
            "demo",
            Some("default".to_owned()),
            ResourceKind::Pods,
            false,
            vec!["demo".to_owned()],
        );
        app.show_content(
            "YAML(default/api-token)",
            "data:\n  token: c2VjcmV0",
            None,
            Some("data:\n  token: secret".to_owned()),
            None,
            ContentKind::Yaml,
        );
        app.show_transient_notice("YAML copied to clipboard");
        let backend = TestBackend::new(120, 28);
        let mut terminal = Terminal::new(backend).expect("secret terminal");
        terminal
            .draw(|frame| draw(frame, &mut app))
            .expect("draw encoded secret");
        let rendered = rendered_lines(&terminal, 120).join("\n");
        assert!(rendered.contains("Decode"));
        assert!(rendered.contains("YAML copied to clipboard"));

        app.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        terminal
            .draw(|frame| draw(frame, &mut app))
            .expect("draw decoded secret with header");
        let rendered = rendered_lines(&terminal, 120).join("\n");
        assert!(rendered.contains("Encoded"));
        app.handle_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE));
        terminal
            .draw(|frame| draw(frame, &mut app))
            .expect("draw decoded fullscreen secret");
        let rendered = rendered_lines(&terminal, 120).join("\n");
        assert!(rendered.contains("token: secret"));
        assert!(rendered.contains("Secret values decoded"));
    }

    #[test]
    fn command_and_filter_input_render_above_the_resource_title() {
        let mut app = App::new(
            "demo",
            Some("default".to_owned()),
            ResourceKind::Pods,
            false,
            vec!["demo".to_owned()],
        );
        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).expect("test terminal");

        app.handle_key(KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE));
        terminal
            .draw(|frame| draw(frame, &mut app))
            .expect("draw command input");
        let lines = rendered_lines(&terminal, 100);
        let command_border = lines
            .iter()
            .find(|line| line.contains("╭ Command"))
            .expect("command border");
        assert!(command_border.starts_with('╭'));
        assert!(!lines.iter().any(|line| line.contains("Completions")));
        assert_eq!(command_completion_hint(&app), Some("context".to_owned()));
        let prompt_row = lines
            .iter()
            .position(|line| line.contains(":▏"))
            .expect("command prompt");
        let title_row = lines
            .iter()
            .position(|line| line.contains("pods(default)[0]"))
            .expect("resource title");
        assert!(prompt_row < title_row);

        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE));
        for character in "api".chars() {
            app.handle_key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE));
        }
        terminal
            .draw(|frame| draw(frame, &mut app))
            .expect("draw filter input");
        let lines = rendered_lines(&terminal, 100);
        let prompt_row = lines
            .iter()
            .position(|line| line.contains("/api▏"))
            .expect("filter prompt");
        let title_row = lines
            .iter()
            .position(|line| line.contains("pods(default)[0] /api"))
            .expect("resource title with filter");
        assert!(prompt_row < title_row);
    }

    #[test]
    fn resource_title_count_tracks_live_and_accepted_filter_results() {
        let mut app = App::new(
            "demo",
            None,
            ResourceKind::Pods,
            false,
            vec!["demo".to_owned()],
        );
        app.apply_snapshot(ResourceSnapshot {
            generation: app.generation,
            kind: ResourceKind::Pods.into(),
            rows: Arc::new(vec![
                Arc::new(ResourceRow::new(
                    "default",
                    "api-0",
                    "Running",
                    "1/1",
                    "0",
                    app.now_unix - 60,
                )),
                Arc::new(ResourceRow::new(
                    "default",
                    "worker-0",
                    "Running",
                    "1/1",
                    "0",
                    app.now_unix - 60,
                )),
                Arc::new(ResourceRow::new(
                    "default",
                    "database-0",
                    "Running",
                    "1/1",
                    "0",
                    app.now_unix - 60,
                )),
            ]),
            table_columns: Arc::from([]),
            state: SourceState::Ready,
            revision: 1,
            emitted_at: Instant::now(),
        });
        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).expect("filter count terminal");

        app.handle_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE));
        for character in "api".chars() {
            app.handle_key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE));
        }
        terminal
            .draw(|frame| draw(frame, &mut app))
            .expect("draw live filter count");
        let rendered = rendered_lines(&terminal, 100).join("\n");
        assert!(rendered.contains("pods(all)[1] /api"));
        assert!(!rendered.contains("pods(all)[3] /api"));

        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        terminal
            .draw(|frame| draw(frame, &mut app))
            .expect("draw accepted filter count");
        let rendered = rendered_lines(&terminal, 100).join("\n");
        assert!(rendered.contains("pods(all)[1] /api"));
    }

    #[test]
    fn filter_hint_is_muted_and_command_completion_is_inline() {
        let command = prompt_line(":", "po", None, Some("ds"));
        assert_eq!(
            command.spans.last().and_then(|span| span.style.fg),
            Some(MUTED)
        );
        assert_eq!(
            command.spans.last().map(|span| span.content.as_ref()),
            Some("ds")
        );

        let filter = prompt_line("/", "", None, Some("/previous"));
        assert_eq!(
            filter.spans.last().and_then(|span| span.style.fg),
            Some(MUTED)
        );
        assert_eq!(
            filter.spans.last().map(|span| span.content.as_ref()),
            Some("/previous")
        );
    }

    #[test]
    fn pod_and_node_views_use_k9s_columns() {
        let mut pods = App::new(
            "demo",
            Some("default".to_owned()),
            ResourceKind::Pods,
            false,
            vec!["demo".to_owned()],
        );
        let pod =
            ResourceRow::new("default", "api-0", "Running", "1/1", "3", 0).with_pod_data(PodData {
                restarts: 3,
                node: Arc::from("worker-0"),
                cpu_millis: Some(25),
                cpu_request_millis: 100,
                cpu_limit_millis: 200,
                memory_bytes: Some(64 * 1_048_576),
                memory_request_bytes: 128 * 1_048_576,
                memory_limit_bytes: 256 * 1_048_576,
            });
        pods.apply_snapshot(ResourceSnapshot {
            generation: 1,
            kind: ResourceKind::Pods.into(),
            rows: Arc::new(vec![Arc::new(pod)]),
            table_columns: Arc::from([]),
            state: SourceState::Ready,
            revision: 1,
            emitted_at: Instant::now(),
        });
        let backend = TestBackend::new(200, 30);
        let mut terminal = Terminal::new(backend).expect("pod terminal");
        terminal
            .draw(|frame| draw(frame, &mut pods))
            .expect("draw pod columns");
        let rendered = rendered_lines(&terminal, 200).join("\n");
        for header in [
            "NAME", "READY", "STATUS", "RESTARTS", "CPU", "%CPU/R", "%CPU/L", "MEM", "%MEM/R",
            "%MEM/L", "NODE", "AGE",
        ] {
            assert!(rendered.contains(header), "missing pod column {header}");
        }

        let mut nodes = App::new(
            "demo",
            None,
            ResourceKind::Nodes,
            false,
            vec!["demo".to_owned()],
        );
        let node = ResourceRow::new("-", "worker-0", "Ready", "worker", "v1.32.0", 0)
            .with_node_data(NodeData {
                taints: Arc::from("dedicated=system:NoSchedule,spot=true:NoExecute"),
                taint_count: 2,
                version: Arc::from("v1.32.0"),
                pods: 42,
                cpu_millis: Some(500),
                cpu_allocatable_millis: 4_000,
                memory_bytes: Some(4_096 * 1_048_576),
                memory_allocatable_bytes: 16_384 * 1_048_576,
            });
        assert_eq!(column_value(&node, SortField::Taints, 0), "2");
        nodes.apply_snapshot(ResourceSnapshot {
            generation: 1,
            kind: ResourceKind::Nodes.into(),
            rows: Arc::new(vec![Arc::new(node)]),
            table_columns: Arc::from([]),
            state: SourceState::Ready,
            revision: 1,
            emitted_at: Instant::now(),
        });
        terminal
            .draw(|frame| draw(frame, &mut nodes))
            .expect("draw node columns");
        let rendered = rendered_lines(&terminal, 200).join("\n");
        for header in [
            "NAME", "STATUS", "TAINTS", "VERSION", "PODS", "CPU", "CPU/A", "%CPU", "MEM", "MEM/A",
            "%MEM", "AGE",
        ] {
            assert!(rendered.contains(header), "missing node column {header}");
        }
        assert!(!rendered.contains("NAMESPACE"));
    }

    #[test]
    fn discovered_resource_scope_controls_namespace_column() {
        let mut app = App::new(
            "demo",
            None,
            ResourceKind::Pods,
            false,
            vec!["demo".to_owned()],
        );
        app.kind = crate::model::ResourceDescriptor::new(
            "apiextensions.k8s.io",
            "v1",
            "apiextensions.k8s.io/v1",
            "CustomResourceDefinition",
            "customresourcedefinitions",
            false,
            crate::model::ResourceOperations::all(),
        );
        assert!(
            columns_for_view(&app, 160)
                .iter()
                .all(|column| column.label != "NAMESPACE")
        );

        app.kind = crate::model::ResourceDescriptor::new(
            "example.io",
            "v1",
            "example.io/v1",
            "Widget",
            "widgets",
            true,
            crate::model::ResourceOperations::all(),
        );
        assert!(
            columns_for_view(&app, 160)
                .iter()
                .any(|column| column.label == "NAMESPACE")
        );
    }

    #[test]
    fn server_table_columns_follow_kubectl_normal_and_wide_priorities() {
        let mut app = App::new(
            "demo",
            Some("default".to_owned()),
            ResourceKind::Pods,
            false,
            vec!["demo".to_owned()],
        );
        let kind = ResourceDescriptor::new(
            "example.io",
            "v1",
            "example.io/v1",
            "Widget",
            "widgets",
            true,
            ResourceOperations::all(),
        );
        app.kind = kind.clone();
        let row =
            ResourceRow::new("default", "example", "-", "-", "-", 3_600).with_table_cells(vec![
                TableCell {
                    text: Arc::from("example"),
                    timestamp_unix: None,
                },
                TableCell {
                    text: Arc::from("Ready"),
                    timestamp_unix: None,
                },
                TableCell {
                    text: Arc::from("10.0.0.1"),
                    timestamp_unix: None,
                },
                TableCell {
                    text: Arc::from("1970-01-01T01:00:00Z"),
                    timestamp_unix: Some(3_600),
                },
            ]);
        app.apply_snapshot(ResourceSnapshot {
            generation: app.generation,
            kind,
            rows: Arc::new(vec![Arc::new(row)]),
            table_columns: vec![
                TableColumn {
                    name: Arc::from("Name"),
                    format: Arc::from("name"),
                    priority: 0,
                },
                TableColumn {
                    name: Arc::from("Phase"),
                    format: Arc::from(""),
                    priority: 0,
                },
                TableColumn {
                    name: Arc::from("Endpoint"),
                    format: Arc::from(""),
                    priority: 1,
                },
                TableColumn {
                    name: Arc::from("Age"),
                    format: Arc::from("date"),
                    priority: 0,
                },
            ]
            .into(),
            state: SourceState::Ready,
            revision: 1,
            emitted_at: Instant::now(),
        });

        let labels = |app: &App| {
            columns_for_view(app, 160)
                .into_iter()
                .filter(|column| !column.label.is_empty())
                .map(|column| column.label)
                .collect::<Vec<_>>()
        };
        assert_eq!(labels(&app), ["NAME", "PHASE", "AGE"]);
        app.handle_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL));
        assert_eq!(
            labels(&app),
            ["NAMESPACE", "NAME", "PHASE", "ENDPOINT", "AGE"]
        );
        assert_eq!(
            table_column_value(app.snapshot.rows[0].as_ref(), 3, false, app.now_unix),
            "1970-01-01T01:00:00Z"
        );
    }

    #[test]
    fn created_at_becomes_a_live_sortable_age_when_age_is_absent() {
        let mut app = App::new(
            "demo",
            Some("default".to_owned()),
            ResourceKind::Pods,
            false,
            vec!["demo".to_owned()],
        );
        let kind = ResourceDescriptor::new(
            "cloud.example.io",
            "v1",
            "cloud.example.io/v1",
            "Environment",
            "environments",
            true,
            ResourceOperations::all(),
        );
        app.kind = kind.clone();
        app.now_unix = 10_800;
        let row =
            ResourceRow::new("default", "example", "-", "-", "-", 3_600).with_table_cells(vec![
                TableCell {
                    text: Arc::from("example"),
                    timestamp_unix: None,
                },
                TableCell {
                    text: Arc::from("1970-01-01T01:00:00Z"),
                    timestamp_unix: Some(3_600),
                },
            ]);
        app.apply_snapshot(ResourceSnapshot {
            generation: app.generation,
            kind,
            rows: Arc::new(vec![Arc::new(row)]),
            table_columns: vec![
                TableColumn {
                    name: Arc::from("Name"),
                    format: Arc::from("name"),
                    priority: 0,
                },
                TableColumn {
                    name: Arc::from("Created At"),
                    format: Arc::from(""),
                    priority: 0,
                },
            ]
            .into(),
            state: SourceState::Ready,
            revision: 1,
            emitted_at: Instant::now(),
        });

        let columns = columns_for_view(&app, 120);
        let age = columns
            .iter()
            .find(|column| column.table_index == Some(1))
            .expect("age column");
        assert_eq!(age.label, "AGE");
        assert_eq!(age.field, Some(SortField::Age));
        assert!(age.table_age);
        assert_eq!(
            table_column_value(app.snapshot.rows[0].as_ref(), 1, true, app.now_unix),
            "2h"
        );
    }

    #[test]
    fn loading_a_new_resource_hides_temporary_column_headers() {
        let mut app = App::new(
            "demo",
            None,
            ResourceKind::Pods,
            false,
            vec!["demo".to_owned()],
        );
        let kind = ResourceDescriptor::new(
            "cloud.example.io",
            "v1",
            "cloud.example.io/v1",
            "Environment",
            "environments",
            true,
            ResourceOperations::all(),
        );
        app.kind = kind.clone();
        app.apply_snapshot(ResourceSnapshot::loading(app.generation, kind));

        let backend = TestBackend::new(120, 24);
        let mut terminal = Terminal::new(backend).expect("loading terminal");
        terminal
            .draw(|frame| draw_resources(frame, &mut app, frame.area()))
            .expect("draw loading resource");
        let rendered = rendered_lines(&terminal, 120).join("\n");
        assert!(rendered.contains("Waiting for resources"));
        assert!(!rendered.contains("NAMESPACE"));
        assert!(!rendered.contains("NAME"));
        assert!(!rendered.contains("STATUS"));
        assert!(!rendered.contains("READY"));
        assert!(!rendered.contains("AGE"));
    }

    #[test]
    fn header_menu_is_a_six_row_column_first_k9s_grid() {
        let mut app = App::new(
            "demo",
            Some("team-1".to_owned()),
            ResourceKind::Pods,
            false,
            vec!["demo".to_owned()],
        );
        app.recent_namespaces = (1..=9).map(|index| format!("team-{index}")).collect();

        let width = 220;
        let layout = header_layout(&app, width);
        let backend = TestBackend::new(width, layout.height);
        let mut terminal = Terminal::new(backend).expect("header terminal");
        terminal
            .draw(|frame| draw_header(frame, &app, frame.area(), &layout))
            .expect("draw header grid");
        let lines = rendered_lines(&terminal, width);
        assert_eq!(lines.len(), HEADER_MENU_ROWS);
        let column =
            |line: &str, needle: &str| line.find(needle).map(|byte| line[..byte].chars().count());

        let first_row = &lines[0];
        let namespace_zero = column(first_row, "<0> all").expect("all namespace");
        let namespace_six = column(first_row, "<6> team-6").expect("second namespace column");
        let command = column(first_row, "<:>")
            .unwrap_or_else(|| panic!("first action column in {first_row:?}"));
        let copy_namespace = column(first_row, "<n>").expect("second action column");
        assert!(namespace_zero < namespace_six);
        assert!(namespace_six < command);
        assert!(command < copy_namespace);

        for (row, key) in ["0", "1", "2", "3", "4", "5"].into_iter().enumerate() {
            assert_eq!(
                column(&lines[row], &format!("<{key}>")).expect("namespace key"),
                namespace_zero,
                "namespace key {key} on row {row} in {:?}",
                lines[row]
            );
        }
        for (row, key) in [":", "/", "enter", "y", "d", "c"].into_iter().enumerate() {
            let token = format!("<{key}>");
            assert_eq!(
                column(&lines[row], &token).expect("action key"),
                command,
                "action keys align vertically"
            );
        }

        let cells = terminal.backend().buffer().content();
        assert_eq!(cells[namespace_zero].fg, GREEN);
        assert_eq!(cells[command].fg, PINK);
    }

    #[test]
    fn api_service_view_uses_service_availability_projection() {
        let api_service = ResourceDescriptor::new(
            "apiregistration.k8s.io",
            "v1",
            "apiregistration.k8s.io/v1",
            "APIService",
            "apiservices",
            false,
            ResourceOperations::all(),
        );
        let mut app = App::new(
            "demo",
            None,
            ResourceKind::Pods,
            false,
            vec!["demo".to_owned()],
        );
        app.kind = api_service.clone();
        app.apply_snapshot(ResourceSnapshot {
            generation: app.generation,
            kind: api_service,
            rows: Arc::new(vec![Arc::new(ResourceRow::new(
                "-", "v1.", "True", "Local", "", 0,
            ))]),
            table_columns: Arc::from([]),
            state: SourceState::Ready,
            revision: 1,
            emitted_at: Instant::now(),
        });

        let labels: Vec<_> = columns_for_view(&app, 120)
            .into_iter()
            .filter(|column| column.field.is_some())
            .map(|column| column.label)
            .collect();
        assert_eq!(labels, ["NAME", "SERVICE", "AVAILABLE", "AGE"]);

        let backend = TestBackend::new(120, 24);
        let mut terminal = Terminal::new(backend).expect("APIService terminal");
        terminal
            .draw(|frame| draw(frame, &mut app))
            .expect("draw APIService view");
        let rendered = rendered_lines(&terminal, 120).join("\n");
        assert!(rendered.contains("apiservices(all)[1]"));
        assert!(rendered.contains("v1."));
        assert!(rendered.contains("Local"));
        assert!(rendered.contains("True"));
        assert!(!rendered.contains("NAMESPACE"));
    }

    #[test]
    fn policy_view_renders_k9s_authorization_matrix() {
        let service_account = ResourceDescriptor::new(
            "",
            "v1",
            "v1",
            "ServiceAccount",
            "serviceaccounts",
            true,
            ResourceOperations::all(),
        );
        let mut app = App::new(
            "demo",
            Some("argo".to_owned()),
            ResourceKind::Pods,
            false,
            vec!["demo".to_owned()],
        );
        app.kind = service_account.clone();
        app.apply_snapshot(ResourceSnapshot {
            generation: app.generation,
            kind: service_account,
            rows: Arc::new(vec![Arc::new(
                ResourceRow::new(
                    "-",
                    "secrets",
                    "core",
                    "RO:reader",
                    "resourceNames=token",
                    0,
                )
                .with_policy_data(PolicyData {
                    verbs: vec![Arc::from("get"), Arc::from("list"), Arc::from("watch")],
                }),
            )]),
            table_columns: Arc::from([]),
            state: SourceState::Ready,
            revision: 1,
            emitted_at: Instant::now(),
        });
        app.list_mode = ListMode::Policy {
            title: "policy(ServiceAccount:argo/apiserver-admin)".to_owned(),
        };

        let labels: Vec<_> = columns_for_view(&app, 180)
            .into_iter()
            .filter(|column| column.field.is_some())
            .map(|column| column.label)
            .collect();
        assert_eq!(
            labels,
            [
                "NAME",
                "API-GROUP",
                "BINDING",
                "GET",
                "LIST",
                "WATCH",
                "CREATE",
                "PATCH",
                "UPDATE",
                "DELETE",
                "DEL-LIST",
                "EXTRAS",
            ]
        );

        let backend = TestBackend::new(180, 24);
        let mut terminal = Terminal::new(backend).expect("policy terminal");
        terminal
            .draw(|frame| draw(frame, &mut app))
            .expect("draw policy matrix");
        let rendered = rendered_lines(&terminal, 180).join("\n");
        assert!(rendered.contains("policy(ServiceAccount:argo/apiserver-admin)[1]"));
        assert!(rendered.contains("secrets"));
        assert!(rendered.contains("RO:reader"));
        assert!(rendered.contains('✓'));
        assert!(rendered.contains('×'));
        assert!(rendered.contains("resourceNames="));
    }

    #[test]
    fn fullscreen_content_keeps_command_and_filter_prompts_visible() {
        let mut app = App::new(
            "demo",
            Some("default".to_owned()),
            ResourceKind::Pods,
            false,
            vec!["demo".to_owned()],
        );
        app.show_content(
            "YAML(default/api-0)",
            "kind: Pod\nmetadata:\n  name: api-0",
            None,
            None,
            None,
            ContentKind::Yaml,
        );
        app.handle_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE));

        let backend = TestBackend::new(100, 20);
        let mut terminal = Terminal::new(backend).expect("fullscreen terminal");
        terminal
            .draw(|frame| draw(frame, &mut app))
            .expect("draw fullscreen command");
        let rendered = rendered_lines(&terminal, 100).join("\n");
        assert!(rendered.contains("╭ Command"));
        assert!(rendered.contains(":▏"));
        assert!(rendered.contains("YAML(default/api-0)"));

        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE));
        terminal
            .draw(|frame| draw(frame, &mut app))
            .expect("draw fullscreen filter");
        let rendered = rendered_lines(&terminal, 100).join("\n");
        assert!(rendered.contains("╭ Filter"));
        assert!(rendered.contains("/▏"));
        assert!(rendered.contains("YAML(default/api-0)"));
    }

    #[test]
    fn shift_g_reaches_the_last_line_in_yaml_view() {
        let mut app = App::new(
            "demo",
            Some("default".to_owned()),
            ResourceKind::Pods,
            false,
            vec!["demo".to_owned()],
        );
        let yaml = (0..100)
            .map(|index| format!("key{index:03}: value"))
            .collect::<Vec<_>>()
            .join("\n");
        app.show_content(
            "YAML(default/api-0)",
            yaml,
            None,
            None,
            None,
            ContentKind::Yaml,
        );
        app.handle_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE));
        let backend = TestBackend::new(80, 14);
        let mut terminal = Terminal::new(backend).expect("YAML terminal");
        terminal
            .draw(|frame| draw(frame, &mut app))
            .expect("draw YAML top");

        app.handle_key(KeyEvent::new(KeyCode::Char('G'), KeyModifiers::SHIFT));
        terminal
            .draw(|frame| draw(frame, &mut app))
            .expect("draw YAML bottom");
        let rendered = rendered_lines(&terminal, 80).join("\n");
        assert!(app.content_scroll > 0);
        assert!(rendered.contains("key099: value"));
    }

    #[test]
    fn yaml_filter_scrolls_to_a_match_without_hiding_neighboring_lines() {
        let mut app = App::new(
            "demo",
            Some("default".to_owned()),
            ResourceKind::Pods,
            false,
            vec!["demo".to_owned()],
        );
        let yaml = (0..60)
            .map(|index| format!("key{index:03}: value"))
            .collect::<Vec<_>>()
            .join("\n");
        app.show_content(
            "YAML(default/api-0)",
            yaml,
            None,
            None,
            None,
            ContentKind::Yaml,
        );
        app.handle_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE));
        for character in "key040".chars() {
            app.handle_key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE));
        }

        let backend = TestBackend::new(80, 14);
        let mut terminal = Terminal::new(backend).expect("YAML search terminal");
        terminal
            .draw(|frame| draw(frame, &mut app))
            .expect("draw searched YAML");
        let rendered = rendered_lines(&terminal, 80).join("\n");
        assert!(app.content_scroll > 0);
        assert!(rendered.contains("key040: value"));
        assert!(rendered.contains("key041: value"));
    }

    #[test]
    fn container_list_reuses_body_layout_and_parent_identity() {
        let mut app = App::new(
            "demo",
            Some("default".to_owned()),
            ResourceKind::Pods,
            false,
            vec!["demo".to_owned()],
        );
        app.apply_snapshot(ResourceSnapshot {
            generation: 1,
            kind: ResourceKind::Pods.into(),
            rows: Arc::new(vec![Arc::new(ResourceRow::new(
                "default", "api-0", "Running", "1/1", "0", 0,
            ))]),
            table_columns: Arc::from([]),
            state: SourceState::Ready,
            revision: 1,
            emitted_at: Instant::now(),
        });
        let crate::app::AppCommand::Action(crate::app::ActionIntent::Containers { target }) =
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        else {
            panic!("container request");
        };
        app.show_containers(
            target,
            vec![Arc::new(
                ResourceRow::new("default", "api", "Running", "true", "example/api:v1", 0)
                    .with_container_data(crate::model::ContainerData {
                        image: Arc::from("example/api:v1"),
                        kind: Arc::from("container"),
                        restarts: 0,
                    }),
            )],
        );
        let backend = TestBackend::new(120, 24);
        let mut terminal = Terminal::new(backend).expect("container terminal");
        terminal
            .draw(|frame| draw(frame, &mut app))
            .expect("draw containers");
        let rendered = rendered_lines(&terminal, 120).join("\n");
        assert!(rendered.contains("containers(default/api-0)[1]"));
        assert!(rendered.contains("IMAGE"));
        assert!(!rendered.contains("NAMESPACE"));
    }

    #[test]
    fn selected_column_uses_highlight_without_underlining() {
        let mut app = App::new(
            "demo",
            None,
            ResourceKind::Pods,
            false,
            vec!["demo".to_owned()],
        );
        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| draw(frame, &mut app))
            .expect("draw selected column");
        assert!(
            terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .all(|cell| { !cell.modifier.contains(ratatui::style::Modifier::UNDERLINED) })
        );
    }

    #[test]
    fn resource_rows_use_subtle_banding_without_overriding_selection() {
        assert_eq!(resource_row_style(0, false).bg, None);
        assert_eq!(resource_row_style(1, false).bg, Some(ROW_ALT));
        assert_eq!(
            resource_row_style(1, true).bg,
            Some(SELECTED),
            "selection remains the strongest row treatment"
        );
    }

    #[test]
    fn body_colors_express_resource_semantics() {
        assert_eq!(readiness_style("3/3").fg, Some(GREEN));
        assert_eq!(readiness_style("2/3").fg, Some(ORANGE));
        assert_eq!(readiness_style("-").fg, Some(MUTED));
        assert_eq!(status_style("KubernetesReady").fg, Some(GREEN));
        assert_eq!(status_style("NotReady").fg, Some(RED));
        assert_eq!(status_style("Scheduled").fg, Some(YELLOW));
        assert_eq!(age_color(30 * 60), GREEN);
        assert_eq!(age_color(12 * 60 * 60), CYAN);
        assert_eq!(age_color(2 * 24 * 60 * 60), PURPLE);
        assert_eq!(age_color(14 * 24 * 60 * 60), YELLOW);
        assert_eq!(age_color(60 * 24 * 60 * 60), MUTED);
    }

    #[test]
    fn yaml_highlighter_styles_keys_scalars_and_comments() {
        let lines = highlight_yaml("# object\nkind: Pod\nenabled: true\nreplicas: 3");
        assert_eq!(lines[0].spans[0].style.fg, Some(MUTED));
        assert_eq!(lines[1].spans[2].style.fg, Some(CYAN));
        assert_eq!(lines[2].spans[4].style.fg, Some(PURPLE));
        assert_eq!(lines[3].spans[4].style.fg, Some(ORANGE));
    }

    #[test]
    fn describe_highlighter_preserves_nested_indentation() {
        let lines = highlight_describe("Name: api-0\nContainers:\n  api:\n    Ready: true");
        assert_eq!(lines[0].spans[1].style.fg, Some(CYAN));
        assert_eq!(lines[2].spans[0].content.as_ref(), "  ");
        assert_eq!(lines[3].spans[0].content.as_ref(), "    ");
    }
}
