//! ANSI (SGR) parsing for command output.
//!
//! The colour model is diri's — `TermColor` and `TermStyle` from
//! `crates/diri-proto/src/grid.rs` (Apache-2.0) — but the pipeline around it is
//! deliberately not.
//!
//! diri renders a terminal: a PTY feeds SwiftTerm, a headless screen keeps a
//! cell grid, and the client draws grid deltas. AutoHarness has no PTY to
//! render. Its engines speak JSON lines, and the only thing that produces
//! terminal output is a daemon-owned check command whose stdout and stderr are
//! captured as strings. So this parses those strings into styled spans rather
//! than emulating a screen — there is no cursor to move, no scroll region, and
//! no alternate buffer, because nothing here ever sends one.
//!
//! Total by construction: an unknown escape sequence is dropped rather than
//! printed, and malformed input yields plain text. Build output is untrusted
//! input like anything else a model produced.

/// A terminal colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TermColor {
    /// The pane's own foreground.
    Default,
    /// One of the 16 named colours (0-7 normal, 8-15 bright).
    Indexed(u8),
    Rgb(u8, u8, u8),
}

/// Attributes that survive into rendering. Blink and conceal are parsed and
/// discarded: neither belongs in a build log.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TermStyle {
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
}

/// A run of text sharing one appearance.
#[derive(Debug, Clone, PartialEq)]
pub struct Span {
    pub text: String,
    pub color: TermColor,
    pub style: TermStyle,
}

/// Parse one line into styled spans. Newlines are the caller's business.
pub fn parse_line(line: &str) -> Vec<Span> {
    let mut spans: Vec<Span> = Vec::new();
    let mut current = String::new();
    let mut color = TermColor::Default;
    let mut style = TermStyle::default();
    let mut chars = line.chars().peekable();

    let flush = |text: &mut String, color: TermColor, style: TermStyle, out: &mut Vec<Span>| {
        if !text.is_empty() {
            out.push(Span {
                text: std::mem::take(text),
                color,
                style,
            });
        }
    };

    while let Some(ch) = chars.next() {
        if ch != '\u{1b}' {
            current.push(ch);
            continue;
        }
        // ESC. Only CSI ... m (SGR) changes appearance; everything else is
        // consumed and dropped so it cannot leak into the output as mojibake.
        if chars.peek() != Some(&'[') {
            // Not a CSI: skip the single following byte if there is one.
            chars.next();
            continue;
        }
        chars.next(); // '['
        let mut params = String::new();
        let mut final_byte = None;
        for ch in chars.by_ref() {
            if ch.is_ascii_digit() || ch == ';' || ch == '?' {
                params.push(ch);
            } else {
                final_byte = Some(ch);
                break;
            }
        }
        if final_byte != Some('m') {
            continue; // Cursor moves, erases, modes: nothing to draw.
        }
        flush(&mut current, color, style, &mut spans);
        apply_sgr(&params, &mut color, &mut style);
    }
    flush(&mut current, color, style, &mut spans);
    spans
}

/// Apply one SGR parameter list.
fn apply_sgr(params: &str, color: &mut TermColor, style: &mut TermStyle) {
    // A bare `ESC[m` means reset, same as `ESC[0m`.
    let codes: Vec<u16> = if params.is_empty() {
        vec![0]
    } else {
        params
            .split(';')
            .map(|p| p.trim_start_matches('?').parse().unwrap_or(0))
            .collect()
    };

    let mut index = 0;
    while index < codes.len() {
        match codes[index] {
            0 => {
                *color = TermColor::Default;
                *style = TermStyle::default();
            }
            1 => style.bold = true,
            2 => style.dim = true,
            3 => style.italic = true,
            4 => style.underline = true,
            22 => {
                style.bold = false;
                style.dim = false;
            }
            23 => style.italic = false,
            24 => style.underline = false,
            30..=37 => *color = TermColor::Indexed((codes[index] - 30) as u8),
            39 => *color = TermColor::Default,
            90..=97 => *color = TermColor::Indexed((codes[index] - 90 + 8) as u8),
            // Extended colour: `38;5;n` (256) or `38;2;r;g;b` (truecolor).
            38 => match codes.get(index + 1) {
                Some(5) => {
                    if let Some(n) = codes.get(index + 2) {
                        *color = indexed_256(*n as u8);
                    }
                    index += 2;
                }
                Some(2) => {
                    if let (Some(r), Some(g), Some(b)) = (
                        codes.get(index + 2),
                        codes.get(index + 3),
                        codes.get(index + 4),
                    ) {
                        *color = TermColor::Rgb(*r as u8, *g as u8, *b as u8);
                    }
                    index += 4;
                }
                _ => {}
            },
            // Background colours are parsed so their parameters are consumed,
            // but not applied: a build log painting its own background over a
            // themed pane looks broken.
            48 => match codes.get(index + 1) {
                Some(5) => index += 2,
                Some(2) => index += 4,
                _ => {}
            },
            _ => {}
        }
        index += 1;
    }
}

/// Map a 256-colour index onto something renderable.
fn indexed_256(n: u8) -> TermColor {
    match n {
        0..=15 => TermColor::Indexed(n),
        // 6x6x6 cube.
        16..=231 => {
            let n = n - 16;
            let level = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
            TermColor::Rgb(level(n / 36), level((n / 6) % 6), level(n % 6))
        }
        // Greyscale ramp.
        _ => {
            let v = 8 + (n - 232) * 10;
            TermColor::Rgb(v, v, v)
        }
    }
}

/// Strip every escape sequence, for places that want plain text.
pub fn strip(text: &str) -> String {
    parse_line(text).into_iter().map(|s| s.text).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(line: &str) -> Vec<String> {
        parse_line(line).into_iter().map(|s| s.text).collect()
    }

    #[test]
    fn plain_text_is_one_span() {
        let spans = parse_line("hello world");
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].text, "hello world");
        assert_eq!(spans[0].color, TermColor::Default);
        assert_eq!(spans[0].style, TermStyle::default());
    }

    /// The case that matters: a failing test line from a real build.
    #[test]
    fn colour_splits_the_line_and_survives_a_reset() {
        let spans = parse_line("test \u{1b}[31mFAILED\u{1b}[0m after 3s");
        assert_eq!(
            texts("test \u{1b}[31mFAILED\u{1b}[0m after 3s"),
            ["test ", "FAILED", " after 3s"]
        );
        assert_eq!(spans[0].color, TermColor::Default);
        assert_eq!(spans[1].color, TermColor::Indexed(1));
        assert_eq!(spans[2].color, TermColor::Default);
    }

    #[test]
    fn styles_accumulate_and_clear_individually() {
        let spans = parse_line("\u{1b}[1m\u{1b}[4mboth\u{1b}[24monly bold");
        assert!(spans[0].style.bold && spans[0].style.underline);
        assert!(spans[1].style.bold && !spans[1].style.underline);
    }

    #[test]
    fn bright_and_extended_colours_parse() {
        assert_eq!(parse_line("\u{1b}[92mx")[0].color, TermColor::Indexed(10));
        assert_eq!(
            parse_line("\u{1b}[38;2;12;34;56mx")[0].color,
            TermColor::Rgb(12, 34, 56)
        );
        // 256-colour cube and greyscale both resolve to something drawable.
        assert!(matches!(
            parse_line("\u{1b}[38;5;196mx")[0].color,
            TermColor::Rgb(..)
        ));
        assert!(matches!(
            parse_line("\u{1b}[38;5;240mx")[0].color,
            TermColor::Rgb(..)
        ));
    }

    /// A background colour must not paint over the pane's own theme.
    #[test]
    fn background_colours_are_consumed_but_not_applied() {
        let spans = parse_line("\u{1b}[41mred bg\u{1b}[0m");
        assert_eq!(spans[0].text, "red bg");
        assert_eq!(spans[0].color, TermColor::Default);
        // Its parameters must still be eaten, not printed.
        assert_eq!(strip("\u{1b}[48;2;1;2;3mx"), "x");
        assert_eq!(strip("\u{1b}[48;5;9mx"), "x");
    }

    /// Non-SGR sequences are dropped, never rendered as mojibake.
    #[test]
    fn cursor_and_erase_sequences_leave_no_residue() {
        for line in [
            "\u{1b}[2Kcleared",
            "\u{1b}[1;1Hhome",
            "\u{1b}[?25lhidden",
            "\u{1b}[Amoved",
        ] {
            let text = strip(line);
            assert!(!text.contains('\u{1b}'), "{line:?} left an escape");
            assert!(
                !text.contains('['),
                "{line:?} leaked its parameters: {text}"
            );
        }
    }

    /// Build output is untrusted input; malformed escapes must not panic or
    /// swallow the whole line.
    #[test]
    fn malformed_input_degrades_to_text() {
        assert_eq!(strip(""), "");
        assert_eq!(strip("\u{1b}"), "");
        assert_eq!(strip("\u{1b}["), "");
        assert_eq!(strip("\u{1b}[999999999m x"), " x");
        assert_eq!(strip("\u{1b}[;;;m x"), " x");
        // A bare ESC[m is a reset.
        let spans = parse_line("\u{1b}[1mbold\u{1b}[m plain");
        assert!(spans[0].style.bold);
        assert!(!spans[1].style.bold);
    }

    #[test]
    fn multibyte_text_is_not_split() {
        assert_eq!(strip("\u{1b}[32m✓ passé — done\u{1b}[0m"), "✓ passé — done");
    }
}
