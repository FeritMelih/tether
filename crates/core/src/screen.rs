//! Reading the screen model: the repaint a window or viewer starts from, and the screen as
//! text or as styled runs. The repaint puts the scrollback into the viewer's own scrollback
//! before it draws the screen, so a window that attaches late can scroll back through what it
//! missed. Scrollback rows are written here from their cells, one row at a time, because
//! the model's own row formatter positions rows absolutely and a scrollback row has nowhere
//! absolute to go.

use serde_json::{json, Value};
use std::fmt::Write as _;

/// Which of the program's modes a reader cares about.
pub struct Modes {
    pub alt_screen: bool,
    pub bracketed_paste: bool,
    pub app_cursor: bool,
    pub cursor_visible: bool,
}

pub fn modes(screen: &vt100::Screen) -> Modes {
    Modes {
        alt_screen: screen.alternate_screen(),
        bracketed_paste: screen.bracketed_paste(),
        app_cursor: screen.application_cursor(),
        cursor_visible: !screen.hide_cursor(),
    }
}

/// How many rows the scrollback holds.
pub fn scrollback_len(screen: &mut vt100::Screen) -> usize {
    screen.set_scrollback(usize::MAX);
    let n = screen.scrollback();
    screen.set_scrollback(0);
    n
}

/// Calls `f` once per scrollback row, oldest first, with the screen scrolled so that the row
/// is visible at the index given. Leaves the screen scrolled back to the present.
fn each_scrollback_row(screen: &mut vt100::Screen, mut f: impl FnMut(&vt100::Screen, u16)) {
    let len = scrollback_len(screen);
    let rows = usize::from(screen.size().0.max(1));
    let mut start = 0;
    while start < len {
        screen.set_scrollback(len - start);
        let n = rows.min(len - start);
        for i in 0..n {
            f(screen, i as u16);
        }
        start += n;
    }
    screen.set_scrollback(0);
}

type Style = (vt100::Color, vt100::Color, bool, bool, bool, bool, bool);

fn style(cell: &vt100::Cell) -> Style {
    (
        cell.fgcolor(),
        cell.bgcolor(),
        cell.bold(),
        cell.dim(),
        cell.italic(),
        cell.underline(),
        cell.inverse(),
    )
}

fn sgr(s: &Style) -> String {
    let (fg, bg, bold, dim, italic, underline, inverse) = *s;
    let mut out = String::from("\x1b[0");
    if bold {
        out.push_str(";1");
    }
    if dim {
        out.push_str(";2");
    }
    if italic {
        out.push_str(";3");
    }
    if underline {
        out.push_str(";4");
    }
    if inverse {
        out.push_str(";7");
    }
    for (color, base, bright, extended) in [(fg, 30, 90, 38), (bg, 40, 100, 48)] {
        match color {
            vt100::Color::Default => {}
            vt100::Color::Idx(n) if n < 8 => {
                let _ = write!(out, ";{}", base + u16::from(n));
            }
            vt100::Color::Idx(n) if n < 16 => {
                let _ = write!(out, ";{}", bright + u16::from(n) - 8);
            }
            vt100::Color::Idx(n) => {
                let _ = write!(out, ";{extended};5;{n}");
            }
            vt100::Color::Rgb(r, g, b) => {
                let _ = write!(out, ";{extended};2;{r};{g};{b}");
            }
        }
    }
    out.push('m');
    out
}

fn blank(cell: &vt100::Cell) -> bool {
    !cell.has_contents() && cell.bgcolor() == vt100::Color::Default && !cell.inverse()
}

/// One visible row as text with its styles, trailing blanks dropped, attributes reset at the end.
fn format_row(screen: &vt100::Screen, row: u16) -> Vec<u8> {
    let cols = screen.size().1;
    let mut last = 0;
    for col in 0..cols {
        if let Some(c) = screen.cell(row, col) {
            if !blank(c) {
                last = col + 1;
            }
        }
    }
    let default: Style = (
        vt100::Color::Default,
        vt100::Color::Default,
        false,
        false,
        false,
        false,
        false,
    );
    let mut current = default;
    let mut out = String::new();
    for col in 0..last {
        let Some(cell) = screen.cell(row, col) else {
            break;
        };
        if cell.is_wide_continuation() {
            continue;
        }
        let s = style(cell);
        if s != current {
            out.push_str(&sgr(&s));
            current = s;
        }
        let text = cell.contents();
        out.push_str(if text.is_empty() { " " } else { text });
    }
    if current != default {
        out.push_str("\x1b[0m");
    }
    out.into_bytes()
}

/// Bytes that, written to a fresh terminal of the screen's size, reproduce it: its scrollback
/// when asked for, the screen, the cursor, the input modes and the title.
pub fn repaint(screen: &mut vt100::Screen, scrollback: bool, title: Option<&str>) -> Vec<u8> {
    let rows = screen.size().0;
    let mut out = Vec::new();
    // Out of any alternate screen, attributes and scroll region reset, everything cleared.
    out.extend(b"\x1b[?1049l\x1b[0m\x1b[r\x1b[H\x1b[2J\x1b[3J");
    let alt = screen.alternate_screen();
    if scrollback && !alt {
        let mut lines: Vec<(Vec<u8>, bool)> = Vec::new();
        each_scrollback_row(screen, |s, row| {
            lines.push((format_row(s, row), s.row_wrapped(row)))
        });
        let n = lines.len();
        for (i, (bytes, wrapped)) in lines.into_iter().enumerate() {
            out.extend(bytes);
            // A row the program wrapped runs on into the next, as it did when it was drawn.
            if !wrapped || i + 1 == n {
                out.extend(b"\r\n");
            }
        }
        if n > 0 {
            // Every row but the one the cursor is on holds scrollback; scroll them up into
            // the viewer's own scrollback, so the screen below starts blank.
            let on_screen = n.min(usize::from(rows.saturating_sub(1)));
            out.extend(format!("\x1b[{rows};1H").as_bytes());
            out.extend(std::iter::repeat_n(b'\n', on_screen));
        }
    }
    if alt {
        out.extend(b"\x1b[?1049h");
    }
    out.extend(screen.state_formatted());
    if let Some(t) = title {
        out.extend(format!("\x1b]0;{}\x07", t.replace(['\x07', '\x1b'], "")).as_bytes());
    }
    out
}

/// The screen as text, one string per row with trailing blanks trimmed; the scrollback first
/// when asked for.
pub fn text(screen: &mut vt100::Screen, scrollback: bool) -> Vec<String> {
    let cols = screen.size().1;
    let mut lines = Vec::new();
    if scrollback {
        each_scrollback_row(screen, |s, row| {
            lines.push(
                s.rows(0, cols)
                    .nth(usize::from(row))
                    .unwrap_or_default()
                    .trim_end()
                    .to_string(),
            );
        });
    }
    lines.extend(screen.rows(0, cols).map(|r| r.trim_end().to_string()));
    lines
}

fn color(c: vt100::Color) -> Option<Value> {
    match c {
        vt100::Color::Default => None,
        vt100::Color::Idx(n) => Some(json!(n)),
        vt100::Color::Rgb(r, g, b) => Some(json!(format!("#{r:02x}{g:02x}{b:02x}"))),
    }
}

fn runs(screen: &vt100::Screen, row: u16) -> Value {
    let cols = screen.size().1;
    let mut runs: Vec<(Style, String)> = Vec::new();
    for col in 0..cols {
        let Some(cell) = screen.cell(row, col) else {
            break;
        };
        if cell.is_wide_continuation() {
            continue;
        }
        let s = style(cell);
        let t = if cell.has_contents() {
            cell.contents()
        } else {
            " "
        };
        match runs.last_mut() {
            Some((last, text)) if *last == s => text.push_str(t),
            _ => runs.push((s, t.to_string())),
        }
    }
    // Trailing plain blanks carry nothing.
    if let Some((s, text)) = runs.last_mut() {
        if s.0 == vt100::Color::Default && s.1 == vt100::Color::Default && !s.6 {
            let trimmed = text.trim_end().len();
            text.truncate(trimmed);
        }
    }
    runs.retain(|(_, t)| !t.is_empty());
    Value::Array(
        runs.into_iter()
            .map(|((fg, bg, bold, dim, italic, underline, inverse), t)| {
                let mut run = json!({ "t": t });
                if let Some(c) = color(fg) {
                    run["fg"] = c;
                }
                if let Some(c) = color(bg) {
                    run["bg"] = c;
                }
                for (name, on) in [
                    ("bold", bold),
                    ("dim", dim),
                    ("italic", italic),
                    ("underline", underline),
                    ("inverse", inverse),
                ] {
                    if on {
                        run[name] = json!(true);
                    }
                }
                run
            })
            .collect(),
    )
}

/// The screen as rows of styled runs; the scrollback first when asked for.
pub fn cells(screen: &mut vt100::Screen, scrollback: bool) -> Vec<Value> {
    let mut rows = Vec::new();
    if scrollback {
        each_scrollback_row(screen, |s, row| rows.push(runs(s, row)));
    }
    for row in 0..screen.size().0 {
        rows.push(runs(screen, row));
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fill(p: &mut vt100::Parser, n: usize) {
        for i in 0..n {
            p.process(format!("line \x1b[31m{i}\x1b[m\r\n").as_bytes());
        }
    }

    /// A viewer given the repaint ends up where the model is: the same screen, the same
    /// cursor, and the scrollback in its own scrollback.
    #[test]
    fn a_repaint_reproduces_the_screen_and_the_scrollback() {
        let mut a = vt100::Parser::new(5, 20, 100);
        fill(&mut a, 12);
        a.process(b"\x1b[1mbold\x1b[m tail\x1b[2;3H");
        let bytes = repaint(a.screen_mut(), true, Some("t"));

        let mut b = vt100::Parser::new(5, 20, 100);
        b.process(&bytes);
        assert_eq!(b.screen().contents(), a.screen().contents());
        assert_eq!(b.screen().cursor_position(), a.screen().cursor_position());
        assert_eq!(text(b.screen_mut(), true), text(a.screen_mut(), true));
        assert_eq!(
            scrollback_len(b.screen_mut()),
            scrollback_len(a.screen_mut())
        );
        let lines = text(b.screen_mut(), true);
        assert_eq!(lines[0], "line 0");
        assert_eq!(
            b.screen().cell(4, 0).unwrap().bold(),
            a.screen().cell(4, 0).unwrap().bold()
        );
    }

    #[test]
    fn a_short_scrollback_leaves_no_blank_lines() {
        let mut a = vt100::Parser::new(5, 20, 100);
        fill(&mut a, 6);
        let mut b = vt100::Parser::new(5, 20, 100);
        b.process(&repaint(a.screen_mut(), true, None));
        assert_eq!(text(b.screen_mut(), true), text(a.screen_mut(), true));
    }

    #[test]
    fn a_wrapped_row_runs_on() {
        let mut a = vt100::Parser::new(3, 10, 100);
        a.process(b"0123456789abcdefghij\r\n1\r\n2\r\n3\r\n");
        let mut b = vt100::Parser::new(3, 10, 100);
        b.process(&repaint(a.screen_mut(), true, None));
        assert_eq!(text(b.screen_mut(), true), text(a.screen_mut(), true));
    }

    #[test]
    fn an_alternate_screen_is_repainted_there() {
        let mut a = vt100::Parser::new(4, 10, 10);
        a.process(b"main\r\n\x1b[?1049hfull\x1b[?2004h");
        let mut b = vt100::Parser::new(4, 10, 10);
        b.process(&repaint(a.screen_mut(), true, None));
        assert!(b.screen().alternate_screen());
        assert!(b.screen().bracketed_paste());
        assert_eq!(b.screen().contents(), "full");
    }

    #[test]
    fn cells_carry_runs() {
        let mut a = vt100::Parser::new(2, 10, 0);
        a.process(b"a\x1b[1;38;5;196mbc\x1b[m d");
        let rows = cells(a.screen_mut(), false);
        assert_eq!(
            rows[0],
            json!([{ "t": "a" }, { "t": "bc", "fg": 196, "bold": true }, { "t": " d" }])
        );
        assert_eq!(rows[1], json!([]));
    }
}
