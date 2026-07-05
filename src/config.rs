//! Сохранение небольшого состояния программы в `appdata/config.toml`.
//!
//! Пока храним только: сортировку, строку поиска по тегам, громкость видео.
//! Формат — простой `key = value` (валидный TOML для этих скаляров), парсер
//! ручной, чтобы не тянуть serde/toml (сборка офлайн, «работает с флешки»).

use std::path::Path;

pub struct Config {
    pub sort_index: i32,
    pub filter_text: String,
    pub volume: f32,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            sort_index: 0,
            filter_text: String::new(),
            volume: 100.0,
        }
    }
}

impl Config {
    /// Читает конфиг; при отсутствии/ошибке возвращает значения по умолчанию.
    pub fn load(path: &Path) -> Self {
        let mut cfg = Config::default();
        let Ok(text) = std::fs::read_to_string(path) else {
            return cfg;
        };
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((key, val)) = line.split_once('=') else {
                continue;
            };
            let (key, val) = (key.trim(), val.trim());
            match key {
                "sort_index" => {
                    if let Ok(v) = val.parse::<i32>() {
                        cfg.sort_index = v;
                    }
                }
                "volume" => {
                    if let Ok(v) = val.parse::<f32>() {
                        cfg.volume = v.clamp(0.0, 100.0);
                    }
                }
                "filter_text" => cfg.filter_text = unquote(val),
                _ => {}
            }
        }
        cfg
    }

    /// Атомарно пишет конфиг (tmp + rename).
    pub fn save(&self, path: &Path) {
        let body = format!(
            "sort_index = {}\nvolume = {}\nfilter_text = \"{}\"\n",
            self.sort_index,
            self.volume,
            escape(&self.filter_text),
        );
        let tmp = path.with_extension("toml.tmp");
        if std::fs::write(&tmp, body).is_ok() {
            let _ = std::fs::rename(&tmp, path);
        }
    }
}

fn escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Снимает кавычки и разэкранирует `\"` / `\\`.
fn unquote(s: &str) -> String {
    let inner = s
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .unwrap_or(s);
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('"') => out.push('"'),
                Some('\\') => out.push('\\'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}
