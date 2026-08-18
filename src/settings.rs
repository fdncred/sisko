//! Session chrome settings (font sizes).

use std::fs;
use std::path::PathBuf;

const MIN_FONT: f32 = 10.0;
const MAX_FONT: f32 = 24.0;
const DEFAULT_REPL: f32 = 14.0;
const DEFAULT_TABLE: f32 = 12.0;

#[derive(Debug, Clone, Copy)]
pub struct UiSettings {
    pub repl_font_size: f32,
    pub table_font_size: f32,
}

impl Default for UiSettings {
    fn default() -> Self {
        Self {
            repl_font_size: DEFAULT_REPL,
            table_font_size: DEFAULT_TABLE,
        }
    }
}

impl UiSettings {
    pub fn load() -> Self {
        let path = settings_path();
        let Ok(text) = fs::read_to_string(path) else {
            return Self::default();
        };
        let mut settings = Self::default();
        for line in text.lines() {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let Ok(parsed) = value.trim().parse::<f32>() else {
                continue;
            };
            match key.trim() {
                "repl_font_size" => settings.repl_font_size = clamp_font(parsed),
                "table_font_size" => settings.table_font_size = clamp_font(parsed),
                _ => {}
            }
        }
        settings
    }

    pub fn save(&self) {
        let path = settings_path();
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let body = format!(
            "repl_font_size={}\ntable_font_size={}\n",
            self.repl_font_size, self.table_font_size
        );
        let _ = fs::write(path, body);
    }

    pub fn set_repl_font(&mut self, size: f32) {
        self.repl_font_size = clamp_font(size);
        self.save();
    }

    pub fn set_table_font(&mut self, size: f32) {
        self.table_font_size = clamp_font(size);
        self.save();
    }
}

pub fn clamp_font(size: f32) -> f32 {
    size.round().clamp(MIN_FONT, MAX_FONT)
}

pub fn settings_path() -> PathBuf {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    if cfg!(target_os = "macos") {
        home.join("Library/Application Support/sisko/settings.ini")
    } else if cfg!(windows) {
        std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .unwrap_or(home)
            .join("sisko/settings.ini")
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".config"))
            .join("sisko/settings.ini")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamps_font_range() {
        assert_eq!(clamp_font(3.0), MIN_FONT);
        assert_eq!(clamp_font(40.0), MAX_FONT);
        assert_eq!(clamp_font(13.4), 13.0);
    }
}
