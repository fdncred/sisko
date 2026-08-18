//! Parse ANSI/SGR sequences from Nushell help and other printed text.

use crate::color::Rgb;

/// One run of styled text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnsiSpan {
    pub text: String,
    pub fg: Option<Rgb>,
    pub bg: Option<Rgb>,
    pub bold: bool,
}

/// One visual line of styled spans.
pub type AnsiLine = Vec<AnsiSpan>;

#[derive(Clone, Copy, Default)]
struct Paint {
    fg: Option<Rgb>,
    bg: Option<Rgb>,
    bold: bool,
}

/// True when `text` has real or escaped CSI/SGR sequences.
pub fn looks_like_ansi(text: &str) -> bool {
    text.contains('\u{1b}')
        || text.contains("\\e[")
        || text.contains("\\E[")
        || text.contains("\\x1b")
        || text.contains("\\x1B")
        || text.contains("\\u{1b}")
        || text.contains("\\u{1B}")
        || text.contains("\\033[")
}

/// Turn common escaped ESC forms into a real ESC so SGR parsing can run.
pub fn unescape_ansi(input: &str) -> String {
    let chars: Vec<char> = input.chars().collect();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '\\' {
            if let Some((next, ch)) = decode_escape(&chars, i) {
                out.push(ch);
                i = next;
                continue;
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

fn decode_escape(chars: &[char], i: usize) -> Option<(usize, char)> {
    let next = *chars.get(i + 1)?;
    match next {
        'e' | 'E' => Some((i + 2, '\u{1b}')),
        'x' | 'X' => {
            let h1 = chars.get(i + 2)?.to_ascii_lowercase();
            let h2 = chars.get(i + 3)?.to_ascii_lowercase();
            if h1 == '1' && h2 == 'b' {
                return Some((i + 4, '\u{1b}'));
            }
            None
        }
        'u' | 'U' => decode_unicode_escape(chars, i),
        '0' => {
            if chars.get(i + 2) == Some(&'3') && chars.get(i + 3) == Some(&'3') {
                return Some((i + 4, '\u{1b}'));
            }
            None
        }
        _ => None,
    }
}

fn decode_unicode_escape(chars: &[char], i: usize) -> Option<(usize, char)> {
    if chars.get(i + 2) == Some(&'{') {
        let mut j = i + 3;
        let mut hex = String::new();
        while j < chars.len() && chars[j] != '}' {
            if !chars[j].is_ascii_hexdigit() || hex.len() > 6 {
                return None;
            }
            hex.push(chars[j]);
            j += 1;
        }
        if chars.get(j) != Some(&'}') {
            return None;
        }
        let value = u32::from_str_radix(&hex, 16).ok()?;
        if value == 0x1b {
            return Some((j + 1, '\u{1b}'));
        }
        return None;
    }
    None
}

/// Text with CSI sequences removed, after unescaping literal forms.
pub fn visible_text(input: &str) -> String {
    parse_ansi_lines(input)
        .into_iter()
        .map(|line| line.into_iter().map(|span| span.text).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Split `input` into display lines, applying CSI SGR color/bold sequences.
pub fn parse_ansi_lines(input: &str) -> Vec<AnsiLine> {
    parse_ansi_lines_raw(&unescape_ansi(input))
}

fn parse_ansi_lines_raw(input: &str) -> Vec<AnsiLine> {
    let mut lines: Vec<AnsiLine> = vec![Vec::new()];
    let mut paint = Paint::default();
    let mut buf = String::new();
    let chars: Vec<char> = input.chars().collect();
    let mut i = 0;

    let flush = |buf: &mut String, paint: Paint, line: &mut AnsiLine| {
        if buf.is_empty() {
            return;
        }
        line.push(AnsiSpan {
            text: std::mem::take(buf),
            fg: paint.fg,
            bg: paint.bg,
            bold: paint.bold,
        });
    };

    while i < chars.len() {
        if chars[i] == '\u{1b}' && i + 1 < chars.len() && chars[i + 1] == '[' {
            if let Some((next, params)) = read_csi_sgr(&chars, i) {
                flush(&mut buf, paint, lines.last_mut().expect("line"));
                apply_sgr(&mut paint, &params);
                i = next;
                continue;
            }
            if let Some(next) = skip_escape(&chars, i) {
                i = next;
                continue;
            }
        }
        if chars[i] == '\n' {
            flush(&mut buf, paint, lines.last_mut().expect("line"));
            lines.push(Vec::new());
            i += 1;
            continue;
        }
        if chars[i] == '\r' {
            i += 1;
            continue;
        }
        buf.push(chars[i]);
        i += 1;
    }
    flush(&mut buf, paint, lines.last_mut().expect("line"));
    if lines.len() > 1 && lines.last().is_some_and(|line| line.is_empty()) {
        lines.pop();
    }
    lines
}

fn read_csi_sgr(chars: &[char], start: usize) -> Option<(usize, Vec<u16>)> {
    let mut i = start + 2;
    let mut raw = String::new();
    while i < chars.len() {
        let c = chars[i];
        if c == 'm' {
            let params = parse_params(&raw);
            return Some((i + 1, params));
        }
        if c.is_ascii_digit() || c == ';' || c == ':' {
            raw.push(c);
            i += 1;
            continue;
        }
        return None;
    }
    None
}

fn skip_escape(chars: &[char], start: usize) -> Option<usize> {
    if start + 1 >= chars.len() {
        return Some(start + 1);
    }
    match chars[start + 1] {
        '[' => {
            let mut i = start + 2;
            while i < chars.len() {
                let c = chars[i];
                i += 1;
                if ('@'..='~').contains(&c) {
                    return Some(i);
                }
            }
            Some(chars.len())
        }
        ']' => {
            let mut i = start + 2;
            while i < chars.len() {
                if chars[i] == '\u{7}' {
                    return Some(i + 1);
                }
                if chars[i] == '\u{1b}' && i + 1 < chars.len() && chars[i + 1] == '\\' {
                    return Some(i + 2);
                }
                i += 1;
            }
            Some(chars.len())
        }
        _ => Some(start + 2),
    }
}

fn parse_params(raw: &str) -> Vec<u16> {
    if raw.is_empty() {
        return vec![0];
    }
    raw.split([';', ':'])
        .map(|part| part.parse().unwrap_or(0))
        .collect()
}

fn apply_sgr(paint: &mut Paint, params: &[u16]) {
    let mut i = 0;
    while i < params.len() {
        match params[i] {
            0 => *paint = Paint::default(),
            1 => paint.bold = true,
            22 => paint.bold = false,
            30..=37 => paint.fg = Some(Rgb::from_ansi_index((params[i] - 30) as u8, false)),
            90..=97 => paint.fg = Some(Rgb::from_ansi_index((params[i] - 90) as u8, true)),
            40..=47 => paint.bg = Some(Rgb::from_ansi_index((params[i] - 40) as u8, false)),
            100..=107 => paint.bg = Some(Rgb::from_ansi_index((params[i] - 100) as u8, true)),
            39 => paint.fg = None,
            49 => paint.bg = None,
            38 | 48 => {
                let is_fg = params[i] == 38;
                if let Some((used, color)) = read_extended(&params[i + 1..]) {
                    if is_fg {
                        paint.fg = Some(color);
                    } else {
                        paint.bg = Some(color);
                    }
                    i += used;
                }
            }
            _ => {}
        }
        i += 1;
    }
}

fn read_extended(params: &[u16]) -> Option<(usize, Rgb)> {
    match params.first().copied()? {
        5 => {
            let n = *params.get(1)? as u8;
            Some((2, Rgb::from_xterm(n)))
        }
        2 => {
            let r = *params.get(1)? as u8;
            let g = *params.get(2)? as u8;
            let b = *params.get(3)? as u8;
            Some((4, Rgb::new(r, g, b)))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_is_one_span() {
        let lines = parse_ansi_lines("hello");
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0][0].text, "hello");
        assert!(lines[0][0].fg.is_none());
    }

    #[test]
    fn strips_sgr_and_keeps_color() {
        let lines = parse_ansi_lines("\u{1b}[32mgreen\u{1b}[0m plain");
        assert_eq!(lines[0].len(), 2);
        assert_eq!(lines[0][0].text, "green");
        assert!(lines[0][0].fg.is_some());
        assert_eq!(lines[0][1].text, " plain");
        assert!(lines[0][1].fg.is_none());
    }

    #[test]
    fn splits_lines_and_preserves_style() {
        let lines = parse_ansi_lines("\u{1b}[1;34mA\nB\u{1b}[0m");
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0][0].text, "A");
        assert!(lines[0][0].bold);
        assert_eq!(lines[1][0].text, "B");
        assert!(lines[1][0].bold);
    }

    #[test]
    fn xterm_256_foreground() {
        let lines = parse_ansi_lines("\u{1b}[38;5;196mred\u{1b}[m");
        assert_eq!(lines[0][0].text, "red");
        assert_eq!(lines[0][0].fg.map(|c| (c.r, c.g, c.b)), Some((255, 0, 0)));
    }

    #[test]
    fn unescapes_common_literal_forms() {
        for input in [
            "\\e[32mgreen\\e[0m",
            "\\u{1b}[32mgreen\\u{1b}[0m",
            "\\x1b[32mgreen\\x1b[0m",
            "\\033[32mgreen\\033[0m",
        ] {
            let lines = parse_ansi_lines(input);
            assert_eq!(lines[0][0].text, "green", "input={input}");
            assert!(lines[0][0].fg.is_some(), "input={input}");
        }
    }
}
