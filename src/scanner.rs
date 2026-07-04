//! Сканер медиатеки: обходит `media/`, сверяет с БД и поддерживает индекс.
//!
//! Правило идентичности (по договорённости):
//! - файл на диске + активная запись с этим rel_path → тот же файл (обновляем);
//! - файл есть, активной записи нет → новая запись;
//! - активная запись есть, файла нет → помечаем is_deleted=1.
//!
//! rel_path всегда с прямыми слешами — чтобы индекс совпадал на Windows и Linux.

use std::collections::HashMap;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use walkdir::WalkDir;

use crate::db::{Db, NewFile};
use crate::media;

#[derive(Default, Debug)]
pub struct ScanStats {
    pub added: usize,
    pub updated: usize,
    pub deleted: usize,
    pub seen: usize,
}

/// Полный проход по `media_root` с обновлением индекса в `db`.
pub fn scan(media_root: &Path, db: &Db) -> rusqlite::Result<ScanStats> {
    // Снимок живых записей: rel_path -> (id, size, mtime).
    let mut active: HashMap<String, (i64, i64, i64)> = HashMap::new();
    for a in db.active_files()? {
        active.insert(a.rel_path, (a.id, a.size, a.mtime));
    }

    let now = unix_now();
    let mut stats = ScanStats::default();

    for entry in WalkDir::new(media_root)
        .follow_links(false)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();

        let ext = match path.extension().and_then(|e| e.to_str()) {
            Some(e) => e.to_ascii_lowercase(),
            None => continue,
        };
        let media_type = match media::classify(&ext) {
            Some(mt) => mt,
            None => continue,
        };
        let rel = match rel_path(media_root, path) {
            Some(r) => r,
            None => continue,
        };
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_string();

        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        let size = meta.len() as i64;
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);

        match active.remove(&rel) {
            Some((id, osize, omtime)) => {
                if osize != size || omtime != mtime {
                    db.touch_file(id, size, mtime)?;
                    stats.updated += 1;
                }
                stats.seen += 1;
            }
            None => {
                db.insert_file(&NewFile {
                    rel_path: &rel,
                    name: &name,
                    ext: &ext,
                    media_type,
                    size,
                    mtime,
                    added_at: now,
                })?;
                stats.added += 1;
            }
        }
    }

    // Что осталось в active — на диске не встретилось → помечаем удалёнными.
    if !active.is_empty() {
        let ids: Vec<i64> = active.values().map(|(id, _, _)| *id).collect();
        db.mark_deleted(&ids)?;
        stats.deleted = ids.len();
    }

    Ok(stats)
}

/// Путь относительно корня, компоненты через '/'.
fn rel_path(root: &Path, path: &Path) -> Option<String> {
    let rel = path.strip_prefix(root).ok()?;
    let mut s = String::new();
    for (i, comp) in rel.components().enumerate() {
        if i > 0 {
            s.push('/');
        }
        s.push_str(comp.as_os_str().to_str()?);
    }
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
