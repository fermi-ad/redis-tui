//! The help window: every panel, popup, key and click, the colours, and
//! which actions write to Redis. Opened with `?`, `F1` or a click on
//! `[?]Help` in the title bar; MANUAL.md carries the same tables.

use crate::app::App;
use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
    Frame,
};

/// Width of the key column, in cells.
pub const KEY_COL: usize = 14;

/// One help row: key (or colour sample) and meaning.
pub struct Row(pub &'static str, pub &'static str, pub Option<Color>);

pub struct Section {
    pub title: &'static str,
    pub rows: &'static [Row],
}

const fn k(key: &'static str, text: &'static str) -> Row {
    Row(key, text, None)
}

const fn c(key: &'static str, color: Color, text: &'static str) -> Row {
    Row(key, text, Some(color))
}

pub static SECTIONS: &[Section] = &[
    Section {
        title: "The screen",
        rows: &[
            k(
                "Title bar",
                "hosts; click [?]Help or [q]Quit; version at the right",
            ),
            k(
                "Key list",
                "every key: type badge, P plotted, L listening, W writing",
            ),
            k(
                "Value view",
                "the selected key's value; rate view instead with i",
            ),
            k(
                "Data plot",
                "up to 4 keys decoded as numbers; FFT below with f",
            ),
            k("Status bar", "connection, database, last action or error"),
        ],
    },
    Section {
        title: "Everywhere",
        rows: &[
            k("? / F1", "this help"),
            k("Tab", "next panel: key list, value view, data plot"),
            k("Shift-Tab", "previous panel"),
            k("0-9", "switch to Redis database 0-9"),
            k("q / Esc", "quit (Esc in a popup closes the popup)"),
            k(
                "\u{2191}\u{2193} Enter",
                "act on the panel with the cyan border",
            ),
            k(
                "letters",
                "t T e i l w p a x f g s n R z d work from any panel",
            ),
        ],
    },
    Section {
        title: "Key list",
        rows: &[
            k("\u{2191}\u{2193}", "select a key and load its value"),
            k("Enter", "load the selected key again"),
            k("/", "filter by glob, e.g. sensor:*  (Enter, Esc)"),
            k("r", "reload the key list"),
            k("click", "select a key; wheel scrolls the list"),
        ],
    },
    Section {
        title: "Value view",
        rows: &[
            k("\u{2191}\u{2193}", "scroll (wheel too)"),
            k("t / T", "data type forward / back (Int8 ... Float64)"),
            k("e", "endianness little / big"),
            k("i", "ingestion rate view (a stream you listen to)"),
        ],
    },
    Section {
        title: "Streams",
        rows: &[
            k("l", "listen live on the stream (up to 4); l again stops"),
            k("w", "signal generator on the stream; w again stops"),
        ],
    },
    Section {
        title: "Data plot",
        rows: &[
            k("p", "plot the selected key on / off (up to 4)"),
            k("\u{2191}\u{2193}", "signal / FFT plot (with FFT on)"),
            k("a", "autoscale"),
            k("x", "type the limits (X and each key's Y)"),
            k("f", "FFT on / off"),
            k("g", "FFT scale linear / log"),
            k("wheel", "zoom at the pointer; drag pans"),
            k("hover", "read the value under the pointer"),
        ],
    },
    Section {
        title: "Popups",
        rows: &[
            k("Tab Shift-Tab", "next / previous field"),
            k(
                "\u{2190}\u{2192}",
                "new key: its type; signal gen: wave, data type",
            ),
            k("Enter", "apply (list, set, stream entries stay open)"),
            k("Esc", "cancel"),
            k("Ctrl-B", "edit: binary encoding on / off"),
            k("Ctrl-T Ctrl-E", "edit, binary: data type / endianness"),
            k("y / n", "confirm / cancel a delete"),
            k("paste", "works in every text field"),
        ],
    },
    Section {
        title: "Writes to Redis",
        rows: &[
            k("s", "edit the selected key's value"),
            k("n", "new key"),
            k("R", "rename the selected key"),
            k("z", "set the TTL (-1 keeps it for ever)"),
            k("d", "delete the selected key; asks y / n first"),
            k("w", "signal generator: XADD + XTRIM until stopped"),
            k("", "s n R z open a popup; nothing is written until Enter."),
            k("", "Everything else only reads."),
        ],
    },
    Section {
        title: "Colours",
        rows: &[
            c("STR", Color::Green, "string"),
            c("LIST", Color::Blue, "list"),
            c("SET", Color::Magenta, "set"),
            c("ZSET", Color::Yellow, "sorted set"),
            c("HASH", Color::Red, "hash"),
            c("STRM", Color::Cyan, "stream"),
            c(
                "L / W",
                Color::Green,
                "listening (green) / generator writing (red)",
            ),
            c("!", Color::Yellow, "same key name on two hosts"),
            c("\u{26a0} red", Color::Red, "stream gaps or a lagging rate"),
            c("status", Color::Green, "green connected, red not connected"),
            c("cyan border", Color::Cyan, "the panel that has the keys"),
            c("grey", Color::DarkGray, "hints; no data"),
        ],
    },
    Section {
        title: "Mouse",
        rows: &[
            k("click", "a key; [?]Help or [q]Quit in the title bar"),
            k("wheel", "key list, value view: scroll; plot: zoom"),
            k("drag", "pan the plot"),
            k("Shift", "hold to select text with the terminal"),
        ],
    },
    Section {
        title: "This help",
        rows: &[
            k("\u{2191}\u{2193} PgUp PgDn", "scroll (Home End, wheel too)"),
            k("Esc ? F1 q", "close (or click anywhere)"),
        ],
    },
];

fn lines() -> Vec<Line<'static>> {
    let mut out = Vec::new();
    for s in SECTIONS {
        out.push(Line::from(Span::styled(
            s.title,
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
        )));
        for Row(key, text, color) in s.rows {
            let style = Style::default()
                .fg(color.unwrap_or(Color::Green))
                .add_modifier(Modifier::BOLD);
            out.push(Line::from(vec![
                Span::styled(format!("{key:<w$}", w = KEY_COL), style),
                Span::raw(*text),
            ]));
        }
        out.push(Line::from(""));
    }
    out
}

/// Total rendered help lines.
pub fn line_count() -> u16 {
    SECTIONS.iter().map(|s| s.rows.len() as u16 + 2).sum()
}

/// Draw the help over `area`; stores the page height for paging keys.
pub fn draw(frame: &mut Frame, app: &mut App, area: Rect) {
    let w = area.width.min(84);
    let popup = Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y,
        width: w,
        height: area.height,
    };
    frame.render_widget(Clear, popup);
    let total = line_count();
    let inner_h = popup.height.saturating_sub(2);
    app.help_page = inner_h;
    let max = total.saturating_sub(inner_h);
    app.help_scroll = app.help_scroll.min(max);
    let end = (app.help_scroll + inner_h).min(total);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(" Help \u{b7} \u{2191}\u{2193} PgUp PgDn scroll \u{b7} Esc close ")
        .title_bottom(
            Line::from(format!(
                " lines {}\u{2013}{end} of {total} ",
                app.help_scroll + 1
            ))
            .right_aligned(),
        );
    frame.render_widget(
        Paragraph::new(lines())
            .scroll((app.help_scroll, 0))
            .block(block),
        popup,
    );
}

/// Scroll by `delta` lines, clamped to the content.
pub fn scroll(app: &mut App, delta: i32) {
    let max = line_count().saturating_sub(app.help_page) as i32;
    app.help_scroll = (app.help_scroll as i32 + delta).clamp(0, max.max(0)) as u16;
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{backend::TestBackend, Terminal};

    fn text() -> String {
        SECTIONS
            .iter()
            .flat_map(|s| {
                std::iter::once(s.title.to_string())
                    .chain(s.rows.iter().map(|r| format!("{} {}", r.0, r.1)))
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn every_row_fits_80_columns() {
        for s in SECTIONS {
            for Row(key, text, _) in s.rows {
                assert!(key.chars().count() < KEY_COL, "key too wide: {key}");
                assert!(KEY_COL + text.chars().count() <= 76, "too wide: {text}");
                assert!(!text.contains('\u{1b}'));
            }
        }
    }

    #[test]
    fn lists_every_key_and_the_writes() {
        let t = text();
        for key in [
            "? / F1",
            "Tab",
            "Shift-Tab",
            "0-9",
            "q / Esc",
            "Enter",
            "/",
            "r",
            "t / T",
            "e",
            "i",
            "l",
            "w",
            "p",
            "a",
            "x",
            "f",
            "g",
            "s",
            "n",
            "R",
            "z",
            "d",
            "Ctrl-B",
            "Ctrl-T",
            "Ctrl-E",
            "y / n",
            "wheel",
            "drag",
            "Shift",
            "[?]Help",
            "[q]Quit",
        ] {
            assert!(t.contains(key), "help misses {key}");
        }
        for word in [
            "Writes to Redis",
            "XADD",
            "Colours",
            "STRM",
            "Everything else only reads",
        ] {
            assert!(t.contains(word), "help misses {word}");
        }
    }

    #[test]
    fn fits_80x24_and_scrolls_to_the_end() {
        let mut app = App::new();
        let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
        let mut draw_text = |app: &mut App| {
            term.draw(|f| draw(f, app, f.area())).unwrap();
            let buf = term.backend().buffer().clone();
            (0..24)
                .map(|y| {
                    (0..80)
                        .map(|x| buf[(x, y)].symbol().to_string())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        let first = draw_text(&mut app);
        assert!(
            first.contains("The screen") && first.contains("lines 1\u{2013}22 of"),
            "{first}"
        );
        assert!(!first.contains('\u{1b}'));
        scroll(&mut app, 10_000);
        let last = draw_text(&mut app);
        assert!(last.contains("click anywhere"), "{last}");
        scroll(&mut app, -10_000);
        assert_eq!(app.help_scroll, 0);
    }
}
