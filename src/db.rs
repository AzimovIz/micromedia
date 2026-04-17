use rusqlite::{Connection, Result, params};
use std::path::Path;

use crate::config;

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct MediaFile {
    pub id: i64,
    pub path: String,
    pub filename: String,
    pub media_type: MediaType,
    pub file_size: Option<i64>,
    pub duration_secs: Option<f64>,
    pub thumbnail_path: Option<String>,
    pub custom_thumbnail: bool,
    pub created_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MediaType {
    Video,
    Image,
}

impl MediaType {
    pub fn as_str(&self) -> &str {
        match self {
            MediaType::Video => "video",
            MediaType::Image => "image",
        }
    }

    pub fn from_str(s: &str) -> Self {
        match s {
            "video" => MediaType::Video,
            _ => MediaType::Image,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Tag {
    pub id: i64,
    pub name: String,
}

pub struct Database {
    conn: Connection,
}

impl Database {
    pub fn open() -> Result<Self> {
        let path = config::db_path();
        let conn = Connection::open(&path)?;
        let db = Database { conn };
        db.migrate()?;
        Ok(db)
    }

    fn migrate(&self) -> Result<()> {
        self.conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS media_files (
                id INTEGER PRIMARY KEY,
                path TEXT NOT NULL UNIQUE,
                filename TEXT NOT NULL,
                media_type TEXT NOT NULL,
                file_size INTEGER,
                duration_secs REAL,
                thumbnail_path TEXT,
                custom_thumbnail INTEGER DEFAULT 0,
                created_at TEXT DEFAULT (datetime('now')),
                scanned_at TEXT DEFAULT (datetime('now'))
            );

            CREATE TABLE IF NOT EXISTS tags (
                id INTEGER PRIMARY KEY,
                name TEXT NOT NULL UNIQUE
            );

            CREATE TABLE IF NOT EXISTS media_tags (
                media_id INTEGER REFERENCES media_files(id) ON DELETE CASCADE,
                tag_id INTEGER REFERENCES tags(id) ON DELETE CASCADE,
                PRIMARY KEY (media_id, tag_id)
            );
            ",
        )?;
        Ok(())
    }

    // --- Media files ---

    pub fn upsert_media(&self, path: &str, filename: &str, media_type: &MediaType, file_size: Option<i64>) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO media_files (path, filename, media_type, file_size, scanned_at)
             VALUES (?1, ?2, ?3, ?4, datetime('now'))
             ON CONFLICT(path) DO UPDATE SET
                file_size = ?4,
                scanned_at = datetime('now')",
            params![path, filename, media_type.as_str(), file_size],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn get_media_by_path(&self, path: &str) -> Result<Option<MediaFile>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, path, filename, media_type, file_size, duration_secs, thumbnail_path, custom_thumbnail, created_at
             FROM media_files WHERE path = ?1"
        )?;
        let mut rows = stmt.query_map(params![path], |row| {
            Ok(MediaFile {
                id: row.get(0)?,
                path: row.get(1)?,
                filename: row.get(2)?,
                media_type: MediaType::from_str(&row.get::<_, String>(3)?),
                file_size: row.get(4)?,
                duration_secs: row.get(5)?,
                thumbnail_path: row.get(6)?,
                custom_thumbnail: row.get::<_, i32>(7)? != 0,
                created_at: row.get(8)?,
            })
        })?;
        match rows.next() {
            Some(Ok(f)) => Ok(Some(f)),
            _ => Ok(None),
        }
    }

    pub fn get_all_media(&self) -> Result<Vec<MediaFile>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, path, filename, media_type, file_size, duration_secs, thumbnail_path, custom_thumbnail, created_at
             FROM media_files ORDER BY filename"
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(MediaFile {
                id: row.get(0)?,
                path: row.get(1)?,
                filename: row.get(2)?,
                media_type: MediaType::from_str(&row.get::<_, String>(3)?),
                file_size: row.get(4)?,
                duration_secs: row.get(5)?,
                thumbnail_path: row.get(6)?,
                custom_thumbnail: row.get::<_, i32>(7)? != 0,
                created_at: row.get(8)?,
            })
        })?;
        rows.collect()
    }

    pub fn get_all_paths(&self) -> Result<std::collections::HashSet<String>> {
        let mut stmt = self.conn.prepare("SELECT path FROM media_files")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        let mut set = std::collections::HashSet::new();
        for r in rows {
            if let Ok(p) = r {
                set.insert(p);
            }
        }
        Ok(set)
    }

    pub fn get_media_by_tag_filter(
        &self,
        included: &[i64],
        excluded: &[i64],
        match_all: bool,
    ) -> Result<Vec<MediaFile>> {
        if included.is_empty() && excluded.is_empty() {
            return self.get_all_media();
        }

        let mut sql = String::from(
            "SELECT m.id, m.path, m.filename, m.media_type, m.file_size, m.duration_secs, m.thumbnail_path, m.custom_thumbnail, m.created_at
             FROM media_files m
             WHERE 1=1"
        );
        let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();

        if !included.is_empty() {
            let ph = included.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            if match_all {
                sql.push_str(&format!(
                    " AND (SELECT COUNT(DISTINCT mt.tag_id) FROM media_tags mt WHERE mt.media_id = m.id AND mt.tag_id IN ({})) = {}",
                    ph, included.len()
                ));
            } else {
                sql.push_str(&format!(
                    " AND EXISTS (SELECT 1 FROM media_tags mt WHERE mt.media_id = m.id AND mt.tag_id IN ({}))",
                    ph
                ));
            }
            for id in included {
                params.push(Box::new(*id));
            }
        }

        if !excluded.is_empty() {
            let ph = excluded.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            sql.push_str(&format!(
                " AND NOT EXISTS (SELECT 1 FROM media_tags mt WHERE mt.media_id = m.id AND mt.tag_id IN ({}))",
                ph
            ));
            for id in excluded {
                params.push(Box::new(*id));
            }
        }

        sql.push_str(" ORDER BY m.filename");

        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(params.iter()), |row| {
            Ok(MediaFile {
                id: row.get(0)?,
                path: row.get(1)?,
                filename: row.get(2)?,
                media_type: MediaType::from_str(&row.get::<_, String>(3)?),
                file_size: row.get(4)?,
                duration_secs: row.get(5)?,
                thumbnail_path: row.get(6)?,
                custom_thumbnail: row.get::<_, i32>(7)? != 0,
                created_at: row.get(8)?,
            })
        })?;
        rows.collect()
    }

    pub fn set_thumbnail(&self, media_id: i64, thumb_path: &str, custom: bool) -> Result<()> {
        self.conn.execute(
            "UPDATE media_files SET thumbnail_path = ?1, custom_thumbnail = ?2 WHERE id = ?3",
            params![thumb_path, custom as i32, media_id],
        )?;
        Ok(())
    }

    pub fn delete_missing(&self, existing_paths: &[String]) -> Result<usize> {
        if existing_paths.is_empty() {
            let count = self.conn.execute("DELETE FROM media_files", [])?;
            return Ok(count);
        }
        let placeholders: Vec<String> = existing_paths.iter().map(|_| "?".to_string()).collect();
        let ph = placeholders.join(",");
        let query = format!("DELETE FROM media_files WHERE path NOT IN ({})", ph);
        let mut stmt = self.conn.prepare(&query)?;
        let params: Vec<Box<dyn rusqlite::types::ToSql>> = existing_paths.iter().map(|p| Box::new(p.clone()) as Box<dyn rusqlite::types::ToSql>).collect();
        let count = stmt.execute(rusqlite::params_from_iter(params.iter()))?;
        Ok(count)
    }

    // --- Tags ---

    pub fn get_all_tags(&self) -> Result<Vec<Tag>> {
        let mut stmt = self.conn.prepare("SELECT id, name FROM tags ORDER BY name")?;
        let rows = stmt.query_map([], |row| {
            Ok(Tag {
                id: row.get(0)?,
                name: row.get(1)?,
            })
        })?;
        rows.collect()
    }

    pub fn create_tag(&self, name: &str) -> Result<i64> {
        self.conn.execute(
            "INSERT OR IGNORE INTO tags (name) VALUES (?1)",
            params![name],
        )?;
        let mut stmt = self.conn.prepare("SELECT id FROM tags WHERE name = ?1")?;
        stmt.query_row(params![name], |row| row.get(0))
    }

    pub fn delete_tag(&self, tag_id: i64) -> Result<()> {
        self.conn.execute("DELETE FROM media_tags WHERE tag_id = ?1", params![tag_id])?;
        self.conn.execute("DELETE FROM tags WHERE id = ?1", params![tag_id])?;
        Ok(())
    }

    pub fn get_tags_for_media(&self, media_id: i64) -> Result<Vec<Tag>> {
        let mut stmt = self.conn.prepare(
            "SELECT t.id, t.name FROM tags t
             JOIN media_tags mt ON t.id = mt.tag_id
             WHERE mt.media_id = ?1
             ORDER BY t.name"
        )?;
        let rows = stmt.query_map(params![media_id], |row| {
            Ok(Tag {
                id: row.get(0)?,
                name: row.get(1)?,
            })
        })?;
        rows.collect()
    }

    pub fn add_tag_to_media(&self, media_id: i64, tag_id: i64) -> Result<()> {
        self.conn.execute(
            "INSERT OR IGNORE INTO media_tags (media_id, tag_id) VALUES (?1, ?2)",
            params![media_id, tag_id],
        )?;
        Ok(())
    }

    pub fn remove_tag_from_media(&self, media_id: i64, tag_id: i64) -> Result<()> {
        self.conn.execute(
            "DELETE FROM media_tags WHERE media_id = ?1 AND tag_id = ?2",
            params![media_id, tag_id],
        )?;
        Ok(())
    }

    pub fn get_tag_counts(&self) -> Result<Vec<(Tag, usize)>> {
        let mut stmt = self.conn.prepare(
            "SELECT t.id, t.name, COUNT(mt.media_id) as cnt
             FROM tags t
             LEFT JOIN media_tags mt ON t.id = mt.tag_id
             GROUP BY t.id
             ORDER BY t.name"
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                Tag {
                    id: row.get(0)?,
                    name: row.get(1)?,
                },
                row.get::<_, usize>(2)?,
            ))
        })?;
        rows.collect()
    }
}

pub fn detect_media_type(path: &Path) -> Option<MediaType> {
    let ext = path.extension()?.to_str()?.to_lowercase();
    match ext.as_str() {
        "mp4" | "mkv" | "avi" | "webm" | "mov" | "wmv" | "flv" | "m4v" => Some(MediaType::Video),
        "jpg" | "jpeg" | "png" | "gif" | "webp" | "bmp" | "tiff" => Some(MediaType::Image),
        _ => None,
    }
}
