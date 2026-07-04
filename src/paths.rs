//! Расположение данных приложения относительно бинарника.
//!
//! Раскладка «работает с флешки»:
//! ```text
//! <binary>
//! media/          — все медиа; отсюда индексация, сюда импорт
//! thumbnails/     — миниатюры {file_id}.jpg
//! appdata/
//!   micromedia.db
//!   libs/         — libmpv.so / libmpv-2.dll
//!   config.toml
//! ```

use std::path::{Path, PathBuf};

pub struct Paths {
    pub base: PathBuf,
    pub media: PathBuf,
    pub thumbnails: PathBuf,
    pub appdata: PathBuf,
    pub libs: PathBuf,
    pub db: PathBuf,
    pub config: PathBuf,
}

impl Paths {
    pub fn resolve() -> Self {
        let base = base_dir();
        let appdata = base.join("appdata");
        Paths {
            media: base.join("media"),
            thumbnails: base.join("thumbnails"),
            libs: appdata.join("libs"),
            db: appdata.join("micromedia.db"),
            config: appdata.join("config.toml"),
            appdata,
            base,
        }
    }

    /// Создаёт нужные папки, если их нет.
    pub fn ensure_dirs(&self) -> std::io::Result<()> {
        for d in [&self.media, &self.thumbnails, &self.appdata, &self.libs] {
            std::fs::create_dir_all(d)?;
        }
        Ok(())
    }

    /// Путь к миниатюре файла.
    pub fn thumb_for(&self, file_id: i64) -> PathBuf {
        self.thumbnails.join(format!("{file_id}.jpg"))
    }
}

/// База данных приложения: папка бинарника, с оговорками для dev и env-оверрайда.
fn base_dir() -> PathBuf {
    if let Some(p) = std::env::var_os("MICROMEDIA_BASE") {
        return PathBuf::from(p);
    }

    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            // В dev-сборке бинарник лежит в target/debug|release — тогда базой
            // считаем корень проекта (текущую папку), а не target/…
            if is_cargo_target(dir) {
                if let Ok(cwd) = std::env::current_dir() {
                    return cwd;
                }
            }
            return dir.to_path_buf();
        }
    }

    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

fn is_cargo_target(dir: &Path) -> bool {
    // …/target/debug  или  …/target/release
    let is_profile = dir
        .file_name()
        .map(|n| n == "debug" || n == "release")
        .unwrap_or(false);
    is_profile
        && dir
            .parent()
            .and_then(|p| p.file_name())
            .map(|n| n == "target")
            .unwrap_or(false)
}
