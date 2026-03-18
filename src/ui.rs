use crate::app::{App, EditOperation, InputMode, Panel, PlotFocus, KEY_TYPES, WAVE_TYPES};
use crate::data::DataType;
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    symbols,
    text::{Line, Span},
    widgets::{
        Axis, Block, Borders, Chart, Clear, Dataset, GraphType, List, ListItem, Paragraph, Wrap,
    },
    Frame,
};

const HIGHLIGHT_COLOR: Color = Color::Cyan;
const BORDER_ACTIVE: Color = Color::Cyan;
const BORDER_INACTIVE: Color = Color::DarkGray;

pub fn draw(frame: &mut Frame, app: &mut App) {
    let size = frame.area();

    // Main layout: title bar, body, status bar
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // title bar
            Constraint::Min(5),   // body
            Constraint::Length(1), // status bar
        ])
        .split(size);

    draw_title_bar(frame, app, outer[0]);
    draw_body(frame, app, outer[1]);
    draw_status_bar(frame, app, outer[2]);

    // Draw overlays
    match app.input_mode {
        InputMode::Filter => draw_filter_popup(frame, app, size),
        InputMode::Confirm => draw_confirm_popup(frame, app, size),
        InputMode::Help => draw_help_popup(frame, app, size),
        InputMode::Edit => draw_edit_popup(frame, app, size),
        InputMode::PlotLimit => draw_plot_limit_popup(frame, app, size),
        InputMode::SignalGen => draw_signal_gen_popup(frame, app, size),
        InputMode::Normal => {}
    }
}

fn draw_title_bar(frame: &mut Frame, app: &App, area: Rect) {
    let url_text = app.url_display();
    let version = format!("v{} ", env!("CARGO_PKG_VERSION"));
    let version_width = version.len() as u16;

    // Split title bar into left (title/url/help) and right (version) so they never overlap
    let h_split = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Min(0),
            Constraint::Length(version_width),
        ])
        .split(area);

    let title = Line::from(vec![
        Span::styled(" Redis TUI ", Style::default().fg(Color::White).bg(Color::Blue).add_modifier(Modifier::BOLD)),
        Span::raw(" "),
        Span::styled(url_text, Style::default().fg(Color::DarkGray)),
        Span::raw("  "),
        Span::styled("[?]Help [q]Quit", Style::default().fg(Color::DarkGray)),
    ]);
    frame.render_widget(Paragraph::new(title), h_split[0]);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(version, Style::default().fg(Color::DarkGray)))),
        h_split[1],
    );
}

fn draw_body(frame: &mut Frame, app: &mut App, area: Rect) {
    if app.plot_visible {
        // Vertical split: top row (keys + value) | bottom (full-width plot)
        let v_split = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Percentage(50),
                Constraint::Percentage(50),
            ])
            .split(area);

        // Top row: key list | value view side by side
        let h_split = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Percentage(25),
                Constraint::Percentage(75),
            ])
            .split(v_split[0]);

        draw_key_list(frame, app, h_split[0]);
        draw_value_view(frame, app, h_split[1]);

        // Bottom: full-width data plot
        draw_data_plot(frame, app, v_split[1]);
    } else {
        // No plot: full height for keys + value
        let h_split = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Percentage(25),
                Constraint::Percentage(75),
            ])
            .split(area);

        draw_key_list(frame, app, h_split[0]);
        draw_value_view(frame, app, h_split[1]);
    }
}

fn draw_key_list(frame: &mut Frame, app: &mut App, area: Rect) {
    let border_color = if app.active_panel == Panel::KeyList {
        BORDER_ACTIVE
    } else {
        BORDER_INACTIVE
    };

    let title = format!(
        " Keys ({}) [/]Filter [r]Refresh ",
        app.keys.len()
    );

    let collision_keys: std::collections::HashSet<&str> = app
        .collisions
        .iter()
        .map(|(k, _)| k.as_str())
        .collect();

    let items: Vec<ListItem> = app
        .keys
        .iter()
        .enumerate()
        .map(|(i, key)| {
            let type_badge = if i < app.key_types.len() {
                type_badge(&app.key_types[i])
            } else {
                ("???", Color::DarkGray)
            };

            let is_collision = collision_keys.contains(key.as_str());
            let plot_color = app.plot_color_for_key(key);
            let is_listening = app.listening_keys.iter().any(|k| k == key);
            let is_siggen = app.siggen_keys.iter().any(|k| k == key);
            let mut spans = vec![
                Span::styled(
                    format!("{:<6}", type_badge.0),
                    Style::default().fg(type_badge.1),
                ),
            ];
            // Indicators: P=plotting, L=listening, W=signal gen
            if let Some(color) = plot_color {
                spans.push(Span::styled("P", Style::default().fg(color)));
            } else {
                spans.push(Span::styled(" ", Style::default()));
            }
            if is_listening {
                spans.push(Span::styled("L", Style::default().fg(Color::Green)));
            } else {
                spans.push(Span::styled(" ", Style::default()));
            }
            if is_siggen {
                spans.push(Span::styled("W", Style::default().fg(Color::Red)));
            } else {
                spans.push(Span::styled(" ", Style::default()));
            }
            spans.push(Span::raw(" "));
            if is_collision {
                spans.push(Span::styled("! ", Style::default().fg(Color::Yellow)));
            }
            spans.push(Span::raw(key.as_str()));

            ListItem::new(Line::from(spans))
        })
        .collect();

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(border_color))
                .title(title),
        )
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("> ");

    frame.render_stateful_widget(list, area, &mut app.key_list_state);
}

fn draw_value_view(frame: &mut Frame, app: &App, area: Rect) {
    let border_color = if app.active_panel == Panel::ValueView {
        BORDER_ACTIVE
    } else {
        BORDER_INACTIVE
    };

    let mut lines: Vec<Line> = Vec::new();

    // Key metadata header
    if let Some(info) = &app.current_key_info {
        lines.push(Line::from(vec![
            Span::styled("Key: ", Style::default().fg(Color::Yellow)),
            Span::styled(&info.name, Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
        ]));
        lines.push(Line::from(vec![
            Span::styled("Type: ", Style::default().fg(Color::Yellow)),
            Span::styled(&info.key_type, Style::default().fg(Color::Green)),
            Span::raw("  "),
            Span::styled("TTL: ", Style::default().fg(Color::Yellow)),
            Span::styled(
                if info.ttl == -1 {
                    "none".to_string()
                } else if info.ttl == -2 {
                    "expired".to_string()
                } else {
                    format!("{}s", info.ttl)
                },
                Style::default().fg(Color::White),
            ),
            Span::raw("  "),
            Span::styled("Size: ", Style::default().fg(Color::Yellow)),
            Span::styled(format_size(info.size), Style::default().fg(Color::White)),
            Span::raw("  "),
            Span::styled("Enc: ", Style::default().fg(Color::Yellow)),
            Span::styled(&info.encoding, Style::default().fg(Color::White)),
        ]));
        lines.push(Line::from(Span::styled(
            "─".repeat(area.width.saturating_sub(2) as usize),
            Style::default().fg(Color::DarkGray),
        )));
    }

    // Value content
    let value_lines = app.format_value();
    for line in &value_lines {
        lines.push(Line::from(Span::raw(line)));
    }

    let title = format!(
        " Value [t]{} [e]{} {}",
        app.data_type,
        app.endianness,
        if app.active_panel == Panel::ValueView { "[Up/Down]Scroll" } else { "" }
    );

    let paragraph = Paragraph::new(lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(border_color))
                .title(title),
        )
        .wrap(Wrap { trim: false })
        .scroll((app.value_scroll, 0));

    frame.render_widget(paragraph, area);
}

fn draw_data_plot(frame: &mut Frame, app: &mut App, area: Rect) {
    let border_color = if app.active_panel == Panel::DataPlot {
        BORDER_ACTIVE
    } else {
        BORDER_INACTIVE
    };

    let focused_limits = match app.plot_focus {
        PlotFocus::Signal => if app.plot_auto_limits { "auto" } else { "manual" },
        PlotFocus::FFT => if app.fft_auto_limits { "auto" } else { "manual" },
    };
    let fft_label = if app.fft_enabled { "ON" } else { "OFF" };
    let log_label = if app.fft_enabled && app.fft_log_scale { " [g]log" } else if app.fft_enabled { " [g]linear" } else { "" };
    let focus_label = if app.fft_enabled {
        match app.plot_focus {
            PlotFocus::Signal => " [Signal]",
            PlotFocus::FFT => " [FFT]",
        }
    } else { "" };
    let title = format!(
        " Plot [a]{} [f]FFT:{}{}{}",
        focused_limits, fft_label, log_label, focus_label
    );

    let has_slot_data = app.plot_slots.iter().any(|s| !s.data.is_empty());
    if app.plot_data.is_empty() && !has_slot_data {
        let msg = Paragraph::new("No plottable data. Press [p] on a key to add it to the plot.")
            .style(Style::default().fg(Color::DarkGray))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(border_color))
                    .title(title),
            );
        frame.render_widget(msg, area);
        return;
    }

    if app.fft_enabled {
        // Split area: top for signal, bottom for FFT
        let plot_split = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(area);
        draw_signal_chart(frame, app, plot_split[0], &title, border_color);
        if app.fft_computing && app.fft_data.is_empty() {
            let msg = Paragraph::new("Computing FFT...")
                .style(Style::default().fg(Color::Yellow))
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(Color::DarkGray))
                        .title(" FFT "),
                );
            frame.render_widget(msg, plot_split[1]);
        } else if !app.fft_data.is_empty() {
            draw_fft_chart(frame, app, plot_split[1], border_color);
        } else {
            let msg = Paragraph::new("No FFT data")
                .style(Style::default().fg(Color::DarkGray))
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(Color::DarkGray))
                        .title(" FFT "),
                );
            frame.render_widget(msg, plot_split[1]);
        }
    } else {
        draw_signal_chart(frame, app, area, &title, border_color);
    }
}

/// Pick a chart marker that won't overflow for the given area.
/// Braille (2x4 sub-pixels) is preferred but its BrailleGrid does `width * height` as u16,
/// overflowing when the product exceeds 65535 (ratatui issue #1449).
/// Fall back to HalfBlock which uses usize internally and is safe for any size.
fn safe_marker(area: Rect) -> symbols::Marker {
    if (area.width as u32) * (area.height as u32) <= 65535 {
        symbols::Marker::Braille
    } else {
        symbols::Marker::HalfBlock
    }
}

fn draw_signal_chart(frame: &mut Frame, app: &mut App, area: Rect, title: &str, border_color: Color) {
    if area.width < 12 || area.height < 5 {
        return;
    }

    // Build data points for the primary key (backward compat)
    let primary_points: Vec<(f64, f64)> = app
        .plot_data
        .iter()
        .enumerate()
        .map(|(i, v)| (i as f64, *v))
        .collect();

    // When multiple keys: always normalize each slot to 0.0-1.0 for individual Y scales
    let normalize = app.plot_slots.len() > 1;

    struct SlotRender {
        points: Vec<(f64, f64)>,
        y_min: f64,
        y_max: f64,
    }
    let slot_renders: Vec<SlotRender> = app
        .plot_slots
        .iter()
        .map(|slot| {
            let auto_min = slot.data.iter().copied()
                .filter(|v| v.is_finite())
                .fold(f64::INFINITY, f64::min);
            let auto_max = slot.data.iter().copied()
                .filter(|v| v.is_finite())
                .fold(f64::NEG_INFINITY, f64::max);
            // Use per-slot limits if set, otherwise auto
            let y_min = slot.y_min.unwrap_or(if auto_min.is_finite() { auto_min } else { 0.0 });
            let y_max = slot.y_max.unwrap_or(if auto_max.is_finite() { auto_max } else { 1.0 });
            let points = if normalize {
                let range = y_max - y_min;
                let safe_range = if range == 0.0 { 1.0 } else { range };
                slot.data
                    .iter()
                    .enumerate()
                    .map(|(i, v)| {
                        let normalized = (v - y_min) / safe_range;
                        (i as f64, normalized)
                    })
                    .collect()
            } else {
                slot.data.iter().enumerate().map(|(i, v)| (i as f64, *v)).collect()
            };
            SlotRender { points, y_min, y_max }
        })
        .collect();

    let (x_lo, x_hi) = if !app.plot_slots.is_empty() {
        let max_len = app.plot_slots.iter()
            .map(|s| s.data.len())
            .max()
            .unwrap_or(0)
            .max(app.plot_data.len());
        if app.plot_x_max > 0.0 {
            (app.plot_x_min, app.plot_x_max)
        } else if max_len > crate::app::PLOT_WINDOW {
            ((max_len - crate::app::PLOT_WINDOW) as f64, max_len as f64)
        } else {
            (0.0, max_len as f64)
        }
    } else {
        app.signal_x_bounds()
    };

    let (y_lo, y_hi) = if app.plot_slots.len() > 1 {
        // Multi-key: always normalized 0-1, per-key Y scales rendered separately
        (-0.05, 1.05)
    } else if !app.plot_slots.is_empty() && app.plot_auto_limits {
        // Single slot, auto: use its actual bounds
        let all_data: Vec<f64> = app.plot_slots[0].data.iter().copied()
            .filter(|v| v.is_finite()).collect();
        if all_data.is_empty() {
            (0.0, 1.0)
        } else {
            let y_min = all_data.iter().copied().fold(f64::INFINITY, f64::min);
            let y_max = all_data.iter().copied().fold(f64::NEG_INFINITY, f64::max);
            let range = y_max - y_min;
            let pad = if range == 0.0 { 1.0 } else { range * 0.1 };
            (y_min - pad, y_max + pad)
        }
    } else if app.plot_auto_limits {
        app.auto_signal_bounds()
    } else {
        (app.plot_y_min, app.plot_y_max)
    };

    // Highlight border if this sub-plot is focused (when FFT is on)
    let chart_border = if app.fft_enabled && app.plot_focus == PlotFocus::Signal {
        Color::Cyan
    } else if app.fft_enabled {
        Color::DarkGray
    } else {
        border_color
    };

    // Build title with hover coords
    let hover_suffix = if !app.hover_in_fft {
        if let (Some(hx), Some(hy)) = (app.hover_data_x, app.hover_data_y) {
            format!(" x:{:.1} y:{:.2}", hx, hy)
        } else {
            String::new()
        }
    } else {
        String::new()
    };
    let full_title = format!("{}{} ", title, hover_suffix);

    let marker = safe_marker(area);

    // Build datasets: one per plot slot (normalized), or primary if no slots
    let mut datasets = Vec::new();
    if app.plot_slots.is_empty() {
        // Fallback: single dataset from primary plot_data
        datasets.push(Dataset::default()
            .name(format!("{} values", app.plot_data.len()))
            .marker(marker)
            .graph_type(GraphType::Line)
            .style(Style::default().fg(Color::Cyan))
            .data(&primary_points));
    } else {
        for (i, slot) in app.plot_slots.iter().enumerate() {
            if !slot.data.is_empty() {
                let sr = &slot_renders[i];
                // Truncate key name to keep legend compact
                let short_name = if slot.key_name.len() > 12 {
                    format!("{}...", &slot.key_name[..12])
                } else {
                    slot.key_name.clone()
                };
                let legend_name = format!("{} [{:.0}..{:.0}]", short_name, sr.y_min, sr.y_max);
                datasets.push(Dataset::default()
                    .name(legend_name)
                    .marker(marker)
                    .graph_type(GraphType::Line)
                    .style(Style::default().fg(slot.color))
                    .data(&sr.points));
            }
        }
    }

    let chart = Chart::new(datasets)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(chart_border))
                .title(full_title),
        )
        .x_axis(
            Axis::default()
                .title("Index")
                .bounds([x_lo, x_hi])
                .labels(vec![
                    Line::from(format!("{:.0}", x_lo)),
                    Line::from(format!("{:.0}", (x_lo + x_hi) / 2.0)),
                    Line::from(format!("{:.0}", x_hi)),
                ]),
        )
        .y_axis(if app.plot_slots.len() > 1 {
            // Multi-key: hide default Y-axis labels, we'll draw per-key ones
            Axis::default()
                .bounds([y_lo, y_hi])
                .labels(vec![Line::from(""), Line::from(""), Line::from("")])
        } else {
            Axis::default()
                .title("Value")
                .bounds([y_lo, y_hi])
                .labels(vec![
                    Line::from(format!("{:.2}", y_lo)),
                    Line::from(format!("{:.2}", (y_lo + y_hi) / 2.0)),
                    Line::from(format!("{:.2}", y_hi)),
                ])
        })
        .legend_position(Some(ratatui::widgets::LegendPosition::TopRight))
        .hidden_legend_constraints((Constraint::Min(0), Constraint::Min(0)));

    frame.render_widget(chart, area);

    // Store chart inner area for mouse hit testing (inside borders)
    let inner = Block::default().borders(Borders::ALL).inner(area);
    // Approximate: ratatui chart uses ~7 chars for y-axis labels, 2 for x-axis
    let y_label_width: u16 = if app.plot_slots.len() > 1 { 1 } else { 7 };
    let chart_x = inner.x + y_label_width;
    let chart_y = inner.y;
    let chart_w = inner.width.saturating_sub(y_label_width);
    let chart_h = inner.height.saturating_sub(2);
    app.signal_chart_area = Some((chart_x, chart_y, chart_w, chart_h));

    // Draw per-key Y-axis scales when multiple keys are plotted
    if app.plot_slots.len() > 1 && !slot_renders.is_empty() {
        let num_slots = slot_renders.len().min(app.plot_slots.len());
        // Each slot gets a column of Y labels on the left side
        let label_width = 9u16; // enough for "-99999.9"
        let buf_area = frame.area();

        for (i, sr) in slot_renders.iter().enumerate().take(num_slots) {
            let color = app.plot_slots[i].color;
            let col_x = inner.x + (i as u16) * label_width;
            if col_x + label_width > buf_area.width { break; }

            // Top: Y max
            if chart_y > 0 && chart_y < buf_area.height {
                frame.render_widget(
                    Paragraph::new(format!("{:.1}", sr.y_max))
                        .style(Style::default().fg(color)),
                    Rect::new(col_x, chart_y, label_width, 1),
                );
            }
            // Middle: midpoint
            let mid_y = chart_y + chart_h / 2;
            if mid_y < buf_area.height {
                let mid_val = (sr.y_min + sr.y_max) / 2.0;
                frame.render_widget(
                    Paragraph::new(format!("{:.1}", mid_val))
                        .style(Style::default().fg(color)),
                    Rect::new(col_x, mid_y, label_width, 1),
                );
            }
            // Bottom: Y min
            let bot_y = chart_y + chart_h.saturating_sub(1);
            if bot_y < buf_area.height && bot_y != mid_y {
                frame.render_widget(
                    Paragraph::new(format!("{:.1}", sr.y_min))
                        .style(Style::default().fg(color)),
                    Rect::new(col_x, bot_y, label_width, 1),
                );
            }
        }
    }

    // Draw crosshair tick marks if hovering in signal chart
    if !app.hover_in_fft {
        if let (Some(hx), Some(hy)) = (app.hover_data_x, app.hover_data_y) {
            draw_crosshair(frame, chart_x, chart_y, chart_w, chart_h, hx, hy, x_lo, x_hi, y_lo, y_hi);
        }
    }
}

fn draw_fft_chart(frame: &mut Frame, app: &mut App, area: Rect, _border_color: Color) {
    if area.width < 12 || area.height < 5 {
        return;
    }

    let display_data = app.fft_display_data();
    let fft_points: Vec<(f64, f64)> = display_data
        .iter()
        .enumerate()
        .map(|(i, v)| (i as f64, *v))
        .collect();

    let (x_lo, x_hi) = app.fft_x_bounds();

    let (y_lo, y_hi) = if app.fft_auto_limits {
        app.auto_fft_bounds()
    } else {
        (app.fft_y_min, app.fft_y_max)
    };

    // Highlight border if this sub-plot is focused
    let chart_border = if app.plot_focus == PlotFocus::FFT {
        Color::Cyan
    } else {
        Color::DarkGray
    };

    let scale_label = if app.fft_log_scale { "log" } else { "linear" };
    let hover_suffix = if app.hover_in_fft {
        if let (Some(hx), Some(hy)) = (app.hover_data_x, app.hover_data_y) {
            format!(" bin:{:.1} mag:{:.2}", hx, hy)
        } else {
            String::new()
        }
    } else {
        String::new()
    };
    let fft_title = format!(" FFT Magnitude ({}){} ", scale_label, hover_suffix);

    let marker = safe_marker(area);
    let datasets = vec![Dataset::default()
        .name(format!("{} bins", display_data.len()))
        .marker(marker)
        .graph_type(GraphType::Line)
        .style(Style::default().fg(Color::Yellow))
        .data(&fft_points)];

    let y_title = if app.fft_log_scale { "log10(Mag)" } else { "Magnitude" };

    let chart = Chart::new(datasets)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(chart_border))
                .title(fft_title),
        )
        .x_axis(
            Axis::default()
                .title("Bin")
                .bounds([x_lo, x_hi])
                .labels(vec![
                    Line::from(format!("{:.0}", x_lo)),
                    Line::from(format!("{:.0}", (x_lo + x_hi) / 2.0)),
                    Line::from(format!("{:.0}", x_hi)),
                ]),
        )
        .y_axis(
            Axis::default()
                .title(y_title)
                .bounds([y_lo, y_hi])
                .labels(vec![
                    Line::from(format!("{:.2}", y_lo)),
                    Line::from(format!("{:.2}", (y_lo + y_hi) / 2.0)),
                    Line::from(format!("{:.2}", y_hi)),
                ]),
        );

    frame.render_widget(chart, area);

    // Store chart inner area for mouse hit testing
    let inner = Block::default().borders(Borders::ALL).inner(area);
    let chart_x = inner.x + 7;
    let chart_y = inner.y;
    let chart_w = inner.width.saturating_sub(7);
    let chart_h = inner.height.saturating_sub(2);
    app.fft_chart_area = Some((chart_x, chart_y, chart_w, chart_h));

    // Draw crosshair if hovering in FFT chart
    if app.hover_in_fft {
        if let (Some(hx), Some(hy)) = (app.hover_data_x, app.hover_data_y) {
            draw_crosshair(frame, chart_x, chart_y, chart_w, chart_h, hx, hy, x_lo, x_hi, y_lo, y_hi);
        }
    }
}

fn draw_status_bar(frame: &mut Frame, app: &App, area: Rect) {
    let status_color = if app.connected {
        Color::Green
    } else {
        Color::Red
    };

    let mut spans = vec![Span::raw(" ")];

    if app.host_count > 1 {
        let conn_text = format!("{}/{} hosts", app.hosts_connected, app.host_count);
        spans.push(Span::styled(conn_text, Style::default().fg(status_color)));
    } else {
        let status_text = if app.connected { "Connected" } else { "Disconnected" };
        spans.push(Span::styled(status_text, Style::default().fg(status_color)));
    }

    spans.push(Span::raw(" | "));
    spans.push(Span::raw(format!("DB: {} ", app.db)));
    spans.push(Span::raw("| "));
    spans.push(Span::raw(format!("Keys: {} ", app.db_size)));

    if !app.collisions.is_empty() {
        spans.push(Span::raw("| "));
        spans.push(Span::styled(
            format!("{} collisions ", app.collisions.len()),
            Style::default().fg(Color::Yellow),
        ));
    }

    if let Some(ref host) = app.current_key_host {
        spans.push(Span::raw("| "));
        spans.push(Span::styled(
            format!("@{} ", host),
            Style::default().fg(Color::Cyan),
        ));
    }

    spans.push(Span::raw("| "));
    spans.push(Span::styled(&app.status_message, Style::default().fg(Color::DarkGray)));

    let line = Line::from(spans);
    let bar = Paragraph::new(line).style(Style::default().bg(Color::Black));
    frame.render_widget(bar, area);
}

fn draw_filter_popup(frame: &mut Frame, app: &App, area: Rect) {
    let popup_area = centered_rect(50, 3, area);
    frame.render_widget(Clear, popup_area);

    let text = format!("Filter: {}_", app.filter_text);
    let popup = Paragraph::new(text).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(HIGHLIGHT_COLOR))
            .title(" Filter Keys (Enter to apply, Esc to cancel) "),
    );
    frame.render_widget(popup, popup_area);
}

fn draw_confirm_popup(frame: &mut Frame, app: &App, area: Rect) {
    let popup_area = centered_rect(50, 5, area);
    frame.render_widget(Clear, popup_area);

    let msg = if let Some(action) = &app.confirm_action {
        format!("{}?\n\n[y] Yes  [n/Esc] No", action)
    } else {
        "Confirm? [y/n]".to_string()
    };

    let popup = Paragraph::new(msg).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Red))
            .title(" Confirm "),
    );
    frame.render_widget(popup, popup_area);
}

fn draw_help_popup(frame: &mut Frame, app: &App, area: Rect) {
    let max_height = area.height.saturating_sub(2).min(50) as u16;
    let popup_area = centered_rect(72, max_height, area);
    frame.render_widget(Clear, popup_area);

    let dim = Style::default().fg(Color::DarkGray);
    let key_style = Style::default().fg(Color::Green).add_modifier(Modifier::BOLD);

    let help_text = vec![
        Line::from(Span::styled("Redis TUI — Keyboard Reference", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD))),
        Line::from(""),
        // --- Navigation ---
        Line::from(vec![Span::styled("Navigation", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD))]),
        Line::from(vec![
            Span::styled("  Up/Down  ", key_style),
            Span::raw("Navigate the key list, scroll value view, or"),
        ]),
        Line::from(Span::styled("            switch between Signal/FFT plots", dim)),
        Line::from(vec![
            Span::styled("  Enter    ", key_style),
            Span::raw("Load the selected key's value and plot its data"),
        ]),
        Line::from(vec![
            Span::styled("  Tab      ", key_style),
            Span::raw("Cycle focus: Key List → Value View → Data Plot"),
        ]),
        Line::from(vec![
            Span::styled("  Shift+Tab", key_style),
            Span::raw("  Cycle focus in reverse"),
        ]),
        Line::from(""),
        // --- Key Operations ---
        Line::from(vec![Span::styled("Key Operations", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD))]),
        Line::from(vec![
            Span::styled("  /        ", key_style),
            Span::raw("Filter keys by glob pattern (e.g. sensor:*)"),
        ]),
        Line::from(vec![
            Span::styled("  r        ", key_style),
            Span::raw("Refresh the key list from Redis"),
        ]),
        Line::from(vec![
            Span::styled("  s        ", key_style),
            Span::raw("Edit the selected key's value"),
        ]),
        Line::from(Span::styled("            Ctrl+B toggles binary encoding mode", dim)),
        Line::from(vec![
            Span::styled("  n        ", key_style),
            Span::raw("Create a new key (string, list, hash, set, stream)"),
        ]),
        Line::from(vec![
            Span::styled("  d        ", key_style),
            Span::raw("Delete the selected key (with confirmation)"),
        ]),
        Line::from(vec![
            Span::styled("  R        ", key_style),
            Span::raw("Rename the selected key"),
        ]),
        Line::from(vec![
            Span::styled("  z        ", key_style),
            Span::raw("Set TTL (expiry) on the selected key in seconds"),
        ]),
        Line::from(vec![
            Span::styled("  0-9      ", key_style),
            Span::raw("Switch to Redis database 0-9"),
        ]),
        Line::from(""),
        // --- Streams ---
        Line::from(vec![Span::styled("Streams", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD))]),
        Line::from(vec![
            Span::styled("  l        ", key_style),
            Span::raw("Start/stop live stream listener (XREAD)"),
        ]),
        Line::from(Span::styled("            Blocks on the selected stream key for new entries", dim)),
        Line::from(vec![
            Span::styled("  w        ", key_style),
            Span::raw("Open signal generator config (for stream keys)"),
        ]),
        Line::from(Span::styled("            Generates sine/square/saw waves into the stream", dim)),
        Line::from(""),
        // --- Data Plot ---
        Line::from(vec![Span::styled("Data Plot", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD))]),
        Line::from(vec![
            Span::styled("  p        ", key_style),
            Span::raw("Show/hide the plot panel (hidden by default)"),
        ]),
        Line::from(vec![
            Span::styled("  t / T    ", key_style),
            Span::raw("Cycle data type: Int8..Float64, String, Blob"),
        ]),
        Line::from(vec![
            Span::styled("  e        ", key_style),
            Span::raw("Toggle byte order: Little-Endian ↔ Big-Endian"),
        ]),
        Line::from(vec![
            Span::styled("  a        ", key_style),
            Span::raw("Auto-fit axis limits to data range"),
        ]),
        Line::from(vec![
            Span::styled("  x        ", key_style),
            Span::raw("Set manual X-axis limits on the focused plot"),
        ]),
        Line::from(vec![
            Span::styled("  y        ", key_style),
            Span::raw("Set manual Y-axis limits on the focused plot"),
        ]),
        Line::from(vec![
            Span::styled("  f        ", key_style),
            Span::raw("Toggle FFT frequency analysis (split view)"),
        ]),
        Line::from(vec![
            Span::styled("  g        ", key_style),
            Span::raw("Toggle FFT Y-axis: linear ↔ log₁₀ scale"),
        ]),
        Line::from(Span::styled("            Use Up/Down to switch focus between Signal and FFT", dim)),
        Line::from(""),
        // --- Mouse ---
        Line::from(vec![Span::styled("Mouse (Plot)", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD))]),
        Line::from(vec![
            Span::styled("  Scroll   ", key_style),
            Span::raw("Zoom in/out on the plot under the cursor"),
        ]),
        Line::from(vec![
            Span::styled("  Drag     ", key_style),
            Span::raw("Pan the plot view (X and Y axes)"),
        ]),
        Line::from(""),
        // --- General ---
        Line::from(vec![Span::styled("General", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD))]),
        Line::from(vec![
            Span::styled("  ?        ", key_style),
            Span::raw("Toggle this help screen"),
        ]),
        Line::from(vec![
            Span::styled("  q / Esc  ", key_style),
            Span::raw("Quit the application (or close a popup)"),
        ]),
    ];

    let total_lines = help_text.len() as u16;
    let inner_height = popup_area.height.saturating_sub(2); // border top + bottom
    let max_scroll = total_lines.saturating_sub(inner_height);
    let scroll = app.help_scroll.min(max_scroll);

    let popup = Paragraph::new(help_text)
        .scroll((scroll, 0))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(HIGHLIGHT_COLOR))
                .title(" Help — Up/Down to scroll, ? or Esc to close "),
        );
    frame.render_widget(popup, popup_area);
}

fn draw_plot_limit_popup(frame: &mut Frame, app: &App, area: Rect) {
    let popup_height = (app.edit_fields.len() as u16) + 5; // fields + title + blank + buttons + borders
    let popup_area = centered_rect(50, popup_height, area);
    frame.render_widget(Clear, popup_area);

    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(Span::styled(
        "Plot Settings",
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from(""));

    for (i, (label, value)) in app.edit_fields.iter().enumerate() {
        let is_focused = i == app.edit_focus;
        let cursor = if is_focused { "_" } else { "" };
        let indicator = if is_focused { "> " } else { "  " };
        let label_style = if is_focused {
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Yellow)
        };
        let input_style = if is_focused {
            Style::default().fg(Color::White).bg(Color::DarkGray)
        } else {
            Style::default().fg(Color::White)
        };
        lines.push(Line::from(vec![
            Span::styled(indicator, Style::default().fg(Color::Cyan)),
            Span::styled(format!("{}: ", label), label_style),
            Span::styled(format!("{}{}", value, cursor), input_style),
        ]));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("[Enter]", Style::default().fg(Color::Green)),
        Span::raw(" Apply  "),
        Span::styled("[Esc]", Style::default().fg(Color::Red)),
        Span::raw(" Cancel  "),
        Span::styled("[Tab]", Style::default().fg(Color::Yellow)),
        Span::raw(" Next line"),
    ]));

    let popup = Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(HIGHLIGHT_COLOR))
            .title(" Plot Limits "),
    );
    frame.render_widget(popup, popup_area);
}

fn draw_signal_gen_popup(frame: &mut Frame, app: &App, area: Rect) {
    let popup_area = centered_rect(60, 18, area);
    frame.render_widget(Clear, popup_area);

    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(Span::styled(
        "Signal Generator",
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from(""));

    // Row 0: Wave Type selector
    let wave_focused = app.signal_gen_focus == 0;
    let wave_indicator = if wave_focused { "> " } else { "  " };
    let wave_label_style = if wave_focused {
        Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::Yellow)
    };
    lines.push(Line::from(vec![
        Span::styled(wave_indicator, Style::default().fg(Color::Cyan)),
        Span::styled("Wave: ", wave_label_style),
        Span::styled("< ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            WAVE_TYPES[app.signal_gen_wave_idx],
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ),
        Span::styled(" >", Style::default().fg(Color::DarkGray)),
        if wave_focused {
            Span::raw("  (Left/Right)")
        } else {
            Span::raw("")
        },
    ]));

    // Row 1: Data Type selector
    let dtype_focused = app.signal_gen_focus == 1;
    let dtype_indicator = if dtype_focused { "> " } else { "  " };
    let dtype_label_style = if dtype_focused {
        Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::Yellow)
    };
    let dtype_name = format!("{}", DataType::all()[app.signal_gen_dtype_idx]);
    lines.push(Line::from(vec![
        Span::styled(dtype_indicator, Style::default().fg(Color::Cyan)),
        Span::styled("Type: ", dtype_label_style),
        Span::styled("< ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            dtype_name,
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ),
        Span::styled(" >", Style::default().fg(Color::DarkGray)),
        if dtype_focused {
            Span::raw("  (Left/Right)")
        } else {
            Span::raw("")
        },
    ]));

    lines.push(Line::from(""));

    // Rows 2-5: Text input fields
    for (i, (label, value)) in app.signal_gen_fields.iter().enumerate() {
        let focus_idx = i + 2; // offset by 2 selector rows
        let is_focused = app.signal_gen_focus == focus_idx;
        let indicator = if is_focused { "> " } else { "  " };
        let label_style = if is_focused {
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Yellow)
        };
        let input_style = if is_focused {
            Style::default().fg(Color::White).bg(Color::DarkGray)
        } else {
            Style::default().fg(Color::White)
        };
        let cursor = if is_focused { "_" } else { "" };
        lines.push(Line::from(vec![
            Span::styled(indicator, Style::default().fg(Color::Cyan)),
            Span::styled(format!("{}: ", label), label_style),
            Span::styled(format!("{}{}", value, cursor), input_style),
        ]));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("[Enter]", Style::default().fg(Color::Green)),
        Span::raw(" Start  "),
        Span::styled("[Esc]", Style::default().fg(Color::Red)),
        Span::raw(" Cancel  "),
        Span::styled("[Tab]", Style::default().fg(Color::Yellow)),
        Span::raw(" Next"),
    ]));

    let popup = Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(HIGHLIGHT_COLOR))
            .title(" Signal Generator "),
    );
    frame.render_widget(popup, popup_area);
}

fn draw_edit_popup(frame: &mut Frame, app: &App, area: Rect) {
    let field_count = app.edit_fields.len();
    let is_new_key = app.edit_operation == Some(EditOperation::NewKey);
    let is_multi = app.is_multi_entry_edit();
    let extra_type = if is_new_key { 2 } else { 0 };
    let extra_count = if is_multi && app.edit_multi_count > 0 { 1 } else { 0 };
    let extra_binary = if app.edit_binary_mode { 2 } else { 1 }; // binary mode row + type/endian row
    let height = (5 + field_count * 2 + extra_type + extra_count + extra_binary).min(24) as u16;
    let popup_area = centered_rect(60, height, area);
    frame.render_widget(Clear, popup_area);

    let title = if is_new_key {
        " New Key ".to_string()
    } else {
        format!(" Edit: {} ", app.edit_key)
    };

    let mut lines: Vec<Line> = Vec::new();

    // Operation label + multi-entry count
    let mut op_line = vec![
        Span::styled("Operation: ", Style::default().fg(Color::Yellow)),
        Span::styled(
            app.edit_op_label(),
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ),
    ];
    if is_multi && app.edit_multi_count > 0 {
        op_line.push(Span::styled(
            format!("  ({} added)", app.edit_multi_count),
            Style::default().fg(Color::Cyan),
        ));
    }
    lines.push(Line::from(op_line));

    // Type selector for new key
    if is_new_key {
        let type_name = KEY_TYPES[app.new_key_type_idx];
        lines.push(Line::from(vec![
            Span::styled("Type: ", Style::default().fg(Color::Yellow)),
            Span::styled("< ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                type_name,
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" >", Style::default().fg(Color::DarkGray)),
            Span::raw("  (Left/Right to change)"),
        ]));
    }

    // Binary mode toggle
    let bin_label = if app.edit_binary_mode { "ON" } else { "OFF" };
    let bin_color = if app.edit_binary_mode { Color::Green } else { Color::DarkGray };
    lines.push(Line::from(vec![
        Span::styled("Binary: ", Style::default().fg(Color::Yellow)),
        Span::styled(bin_label, Style::default().fg(bin_color).add_modifier(Modifier::BOLD)),
        Span::styled("  [Ctrl+B]toggle", Style::default().fg(Color::DarkGray)),
    ]));

    // Data type & endianness selectors (only when binary mode is on)
    let dtype_name = format!("{}", DataType::all()[app.edit_binary_dtype_idx]);
    let endian_name = format!("{}", app.endianness);
    if app.edit_binary_mode {
        lines.push(Line::from(vec![
            Span::styled("  Encode: ", Style::default().fg(Color::Yellow)),
            Span::styled(&dtype_name, Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
            Span::styled(" / ", Style::default().fg(Color::DarkGray)),
            Span::styled(&endian_name, Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
            Span::styled("  [Ctrl+T]type [Ctrl+E]endian", Style::default().fg(Color::DarkGray)),
        ]));
    }

    lines.push(Line::from(""));

    // Fields
    for (i, (label, value)) in app.edit_fields.iter().enumerate() {
        let is_focused = i == app.edit_focus;
        let label_style = if is_focused {
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Yellow)
        };

        let cursor = if is_focused { "_" } else { "" };
        let input_style = if is_focused {
            Style::default().fg(Color::White).bg(Color::DarkGray)
        } else {
            Style::default().fg(Color::White)
        };
        let indicator = if is_focused { "> " } else { "  " };
        let display_val = format!("{}{}", value, cursor);
        lines.push(Line::from(vec![
            Span::styled(indicator, Style::default().fg(Color::Cyan)),
            Span::styled(format!("{}: ", label), label_style),
            Span::styled(display_val, input_style),
        ]));
    }

    // Show hint about value format when binary mode is on
    if app.edit_binary_mode {
        lines.push(Line::from(Span::styled(
            "  (enter comma/space-separated numbers)",
            Style::default().fg(Color::DarkGray),
        )));
    }

    lines.push(Line::from(""));

    // Footer with context-aware instructions
    if is_multi {
        lines.push(Line::from(vec![
            Span::styled("[Enter]", Style::default().fg(Color::Green)),
            Span::raw(" Add entry  "),
            Span::styled("[Esc]", Style::default().fg(Color::Red)),
            Span::raw(" Done  "),
            Span::styled("[Tab]", Style::default().fg(Color::Yellow)),
            Span::raw(" Next line"),
        ]));
    } else {
        lines.push(Line::from(vec![
            Span::styled("[Enter]", Style::default().fg(Color::Green)),
            Span::raw(" Submit  "),
            Span::styled("[Esc]", Style::default().fg(Color::Red)),
            Span::raw(" Cancel  "),
            Span::styled("[Tab]", Style::default().fg(Color::Yellow)),
            Span::raw(" Next line"),
        ]));
    }

    let popup = Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(HIGHLIGHT_COLOR))
            .title(title),
    );
    frame.render_widget(popup, popup_area);
}

/// Draw crosshair tick marks at the hover position on both axes
fn draw_crosshair(
    frame: &mut Frame,
    cx: u16, cy: u16, cw: u16, ch: u16,
    data_x: f64, data_y: f64,
    x_lo: f64, x_hi: f64, y_lo: f64, y_hi: f64,
) {
    if cw == 0 || ch == 0 || x_hi <= x_lo || y_hi <= y_lo {
        return;
    }
    let frac_x = (data_x - x_lo) / (x_hi - x_lo);
    let frac_y = (data_y - y_lo) / (y_hi - y_lo);

    if frac_x < 0.0 || frac_x > 1.0 || frac_y < 0.0 || frac_y > 1.0 {
        return;
    }

    let px = cx + (frac_x * cw as f64) as u16;
    let py = cy + ((1.0 - frac_y) * ch as f64) as u16;

    let crosshair_style = Style::default().fg(Color::White);

    // Vertical line (sparse dashes)
    for y in cy..cy + ch {
        if y != py && y % 2 == 0 {
            if px < cx + cw {
                frame.render_widget(
                    Paragraph::new("│").style(Style::default().fg(Color::DarkGray)),
                    Rect::new(px, y, 1, 1),
                );
            }
        }
    }

    // Horizontal line (sparse dashes)
    for x in cx..cx + cw {
        if x != px && x % 3 == 0 {
            if py < cy + ch {
                frame.render_widget(
                    Paragraph::new("─").style(Style::default().fg(Color::DarkGray)),
                    Rect::new(x, py, 1, 1),
                );
            }
        }
    }

    // Crosshair center
    if px < cx + cw && py < cy + ch {
        frame.render_widget(
            Paragraph::new("┼").style(crosshair_style),
            Rect::new(px, py, 1, 1),
        );
    }

    // X-axis tick mark (at bottom of chart area)
    let tick_y = cy + ch;
    if px < cx + cw && tick_y < cy + ch + 2 {
        let label = format!("{:.1}", data_x);
        let label_len = label.len() as u16;
        let label_x = px.saturating_sub(label_len / 2);
        if label_x + label_len <= cx + cw + 4 {
            frame.render_widget(
                Paragraph::new(label).style(Style::default().fg(Color::Yellow)),
                Rect::new(label_x, tick_y, label_len + 1, 1),
            );
        }
    }

    // Y-axis tick mark (at left edge of chart area)
    if py < cy + ch {
        let label = format!("{:.2}", data_y);
        let label_len = label.len() as u16;
        let label_x = cx.saturating_sub(label_len + 1);
        frame.render_widget(
            Paragraph::new(label).style(Style::default().fg(Color::Yellow)),
            Rect::new(label_x, py, label_len + 1, 1),
        );
    }
}

fn centered_rect(percent_x: u16, height: u16, area: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length((area.height.saturating_sub(height)) / 2),
            Constraint::Length(height),
            Constraint::Min(0),
        ])
        .split(area);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}

fn type_badge(key_type: &str) -> (&str, Color) {
    match key_type {
        "string" => ("STR", Color::Green),
        "list" => ("LIST", Color::Blue),
        "set" => ("SET", Color::Magenta),
        "zset" => ("ZSET", Color::Yellow),
        "hash" => ("HASH", Color::Red),
        "stream" => ("STRM", Color::Cyan),
        _ => ("???", Color::DarkGray),
    }
}

fn format_size(bytes: i64) -> String {
    if bytes < 0 {
        return "?".to_string();
    }
    if bytes < 1024 {
        format!("{} B", bytes)
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

// App helper method for URL display
impl App {
    pub fn url_display(&self) -> String {
        format!("db:{}", self.db)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{backend::TestBackend, Terminal};

    #[test]
    fn version_matches_cargo_toml() {
        let version = env!("CARGO_PKG_VERSION");
        assert_eq!(version, "1.1.0");
    }

    #[test]
    fn title_bar_contains_version() {
        let backend = TestBackend::new(80, 3);
        let mut terminal = Terminal::new(backend).unwrap();
        let app = App::new();

        terminal
            .draw(|frame| {
                let area = Rect::new(0, 0, 80, 1);
                draw_title_bar(frame, &app, area);
            })
            .unwrap();

        let buffer = terminal.backend().buffer().clone();
        // Collect the first row into a string
        let row: String = (0..80).map(|x| buffer.cell((x, 0)).unwrap().symbol().chars().next().unwrap_or(' ')).collect();

        assert!(row.contains("Redis TUI"), "title bar should contain 'Redis TUI', got: {}", row);
        assert!(row.contains("v1.1.0"), "title bar should contain 'v1.1.0' in top-right, got: {}", row);
        // Version should be right-aligned (near end of row)
        let version_pos = row.find("v1.1.0").unwrap();
        assert!(version_pos > 60, "version should be right-aligned, found at position {}", version_pos);
    }

    #[test]
    fn title_bar_contains_help_and_quit() {
        let backend = TestBackend::new(80, 3);
        let mut terminal = Terminal::new(backend).unwrap();
        let app = App::new();

        terminal
            .draw(|frame| {
                let area = Rect::new(0, 0, 80, 1);
                draw_title_bar(frame, &app, area);
            })
            .unwrap();

        let buffer = terminal.backend().buffer().clone();
        let row: String = (0..80).map(|x| buffer.cell((x, 0)).unwrap().symbol().chars().next().unwrap_or(' ')).collect();

        assert!(row.contains("[?]Help"), "title bar should contain '[?]Help', got: {}", row);
        assert!(row.contains("[q]Quit"), "title bar should contain '[q]Quit', got: {}", row);
    }
}
