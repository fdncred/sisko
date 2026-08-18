//! Map Nushell / ANSI / xterm colors onto GPUI colors.

use gpui::Hsla;
use nu_ansi_term::Color;

/// RGB triple resolved from a Nu style.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub bold: bool,
}

impl Rgb {
    pub fn new(r: u8, g: u8, b: u8) -> Self {
        Self {
            r,
            g,
            b,
            bold: false,
        }
    }

    pub fn bold(mut self) -> Self {
        self.bold = true;
        self
    }

    pub fn from_ansi_index(index: u8, bright: bool) -> Self {
        let n = if bright { index + 8 } else { index };
        let (r, g, b) = xterm256(n);
        Self {
            r,
            g,
            b,
            bold: bright,
        }
    }

    pub fn from_xterm(index: u8) -> Self {
        let (r, g, b) = xterm256(index);
        Self {
            r,
            g,
            b,
            bold: false,
        }
    }

    pub fn from_ansi(color: Color, bold: bool) -> Option<Self> {
        let (r, g, b) = match color {
            Color::Black => (80, 80, 80),
            Color::DarkGray => (154, 160, 168),
            Color::Red => (232, 88, 88),
            Color::LightRed => (255, 120, 120),
            Color::Green => (88, 196, 120),
            Color::LightGreen => (120, 220, 150),
            Color::Yellow => (230, 190, 80),
            Color::LightYellow => (245, 220, 120),
            Color::Blue => (88, 156, 232),
            Color::LightBlue => (130, 190, 255),
            Color::Purple | Color::Magenta => (188, 140, 220),
            Color::LightPurple | Color::LightMagenta => (210, 170, 240),
            Color::Cyan => (80, 200, 210),
            Color::LightCyan => (130, 230, 235),
            Color::White => (232, 234, 237),
            Color::Default => return None,
            Color::Fixed(n) => xterm256(n),
            Color::Rgb(r, g, b) => (r, g, b),
            Color::LightGray => (196, 196, 196),
        };
        Some(Self { r, g, b, bold })
    }

    pub fn from_style(style: nu_ansi_term::Style) -> Option<Self> {
        style
            .foreground
            .and_then(|c| Self::from_ansi(c, style.is_bold))
            .map(|mut rgb| {
                rgb.bold |= style.is_bold;
                rgb
            })
    }

    /// Keep the hue, but guarantee it is readable on the current chrome.
    pub fn contrast_on(self, dark_bg: bool) -> Self {
        let lum =
            0.2126 * f32::from(self.r) + 0.7152 * f32::from(self.g) + 0.0722 * f32::from(self.b);
        if dark_bg && lum < 96.0 {
            let t = (96.0 - lum) / 96.0;
            return Self {
                r: lift(self.r, 210, t),
                g: lift(self.g, 214, t),
                b: lift(self.b, 220, t),
                bold: self.bold,
            };
        }
        if !dark_bg && lum > 200.0 {
            return Self {
                r: self.r.saturating_mul(6) / 10,
                g: self.g.saturating_mul(6) / 10,
                b: self.b.saturating_mul(6) / 10,
                bold: self.bold,
            };
        }
        self
    }

    pub fn hsla(self) -> Hsla {
        gpui::rgb((u32::from(self.r) << 16) | (u32::from(self.g) << 8) | u32::from(self.b)).into()
    }
}

fn lift(channel: u8, toward: u8, t: f32) -> u8 {
    let t = t.clamp(0.35, 0.85);
    ((1.0 - t) * f32::from(channel) + t * f32::from(toward)) as u8
}

fn xterm256(n: u8) -> (u8, u8, u8) {
    const SYSTEM: [(u8, u8, u8); 16] = [
        (0, 0, 0),
        (128, 0, 0),
        (0, 128, 0),
        (128, 128, 0),
        (0, 0, 128),
        (128, 0, 128),
        (0, 128, 128),
        (192, 192, 192),
        (128, 128, 128),
        (255, 0, 0),
        (0, 255, 0),
        (255, 255, 0),
        (0, 0, 255),
        (255, 0, 255),
        (0, 255, 255),
        (255, 255, 255),
    ];
    match n {
        0..=15 => SYSTEM[n as usize],
        16..=231 => {
            let n = n - 16;
            let levels = [0_u8, 95, 135, 175, 215, 255];
            (
                levels[(n / 36) as usize],
                levels[((n / 6) % 6) as usize],
                levels[(n % 6) as usize],
            )
        }
        n => {
            let v = 8 + (n - 232) * 10;
            (v, v, v)
        }
    }
}
