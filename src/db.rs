//! Слой доступа к SQLite: схема, миграции и операции для сканера, тегов,
//! фонового хэширования и миниатюр.
//!
//! Каждый поток (UI, сканер, хэш-воркер) открывает свою `Db` (своё соединение).
//! Включён WAL, поэтому чтение UI и запись сканера идут параллельно.

use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;

/// Текущая версия схемы. Инкрементируем при добавлении миграции.
const SCHEMA_VERSION: i64 = 1;

const SCHEMA_V1: &str = r#"
CREATE TABLE files (
    id          INTEGER PRIMARY KEY,
    rel_path    TEXT    NOT NULL,          -- относительно media/
    name        TEXT    NOT NULL,
    ext         TEXT    NOT NULL,
    media_type  INTEGER NOT NULL,          -- 0 = image, 1 = video
    size        INTEGER NOT NULL,
    mtime       INTEGER NOT NULL,
    added_at    INTEGER NOT NULL,
    width       INTEGER,
    height      INTEGER,
    duration_ms INTEGER,
    resume_ms   INTEGER,
    hash        INTEGER,                   -- xxh3 контента, nullable, считается фоном
    thumb_state INTEGER NOT NULL DEFAULT 0,-- 0 нет, 1 готово, 2 ошибка
    is_deleted  INTEGER NOT NULL DEFAULT 0
);

-- Уникальность только среди живых записей: на диске по пути один файл,
-- а удалённые (is_deleted=1) записи-«призраки» могут делить путь с новыми.
CREATE UNIQUE INDEX idx_files_active_path ON files(rel_path) WHERE is_deleted = 0;
CREATE INDEX idx_files_media_type ON files(media_type);
CREATE INDEX idx_files_name       ON files(name);
CREATE INDEX idx_files_added_at   ON files(added_at);
CREATE INDEX idx_files_hash       ON files(hash);
CREATE INDEX idx_files_deleted    ON files(is_deleted);

CREATE TABLE tags (
    id         INTEGER PRIMARY KEY,
    name       TEXT NOT NULL UNIQUE COLLATE NOCASE,
    created_at INTEGER NOT NULL
);

CREATE TABLE file_tags (
    file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
    tag_id  INTEGER NOT NULL REFERENCES tags(id)  ON DELETE CASCADE,
    PRIMARY KEY (file_id, tag_id)
);

CREATE INDEX idx_file_tags_tag ON file_tags(tag_id);
"#;

/// Тип медиа. Хранится в БД целым числом.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaType {
    Image = 0,
    Video = 1,
}

impl MediaType {
    pub fn from_i64(v: i64) -> Option<MediaType> {
        match v {
            0 => Some(MediaType::Image),
            1 => Some(MediaType::Video),
            _ => None,
        }
    }
}

/// Данные для вставки нового файла при сканировании.
pub struct NewFile<'a> {
    pub rel_path: &'a str,
    pub name: &'a str,
    pub ext: &'a str,
    pub media_type: MediaType,
    pub size: i64,
    pub mtime: i64,
    pub added_at: i64,
}

/// Живая запись файла — то, что нужно сканеру для сверки с диском.
pub struct ActiveFile {
    pub id: i64,
    pub rel_path: String,
    pub size: i64,
    pub mtime: i64,
    /// NULL, если фоновый хэшер до файла ещё не дошёл.
    pub hash: Option<i64>,
}

/// Переезд существующей записи на новый путь (файл переименовали/переместили).
/// Сохраняем id — а значит теги, resume_ms, added_at и миниатюру {id}.jpg.
pub struct RenameFile<'a> {
    pub id: i64,
    pub rel_path: &'a str,
    pub name: &'a str,
    pub ext: &'a str,
    pub media_type: MediaType,
    pub size: i64,
    pub mtime: i64,
}

/// Тег.
pub struct Tag {
    pub id: i64,
    pub name: String,
}

/// Элемент галереи (для сетки и дерева).
pub struct GalleryFile {
    pub id: i64,
    pub name: String,
    pub is_video: bool,
    pub rel_path: String,
    // None, если ещё не отсканировано (NULL). Может быть -1 — «не определить».
    pub height: Option<i64>,
    pub width: Option<i64>,
}

/// Ключ сортировки галереи. Индексы совпадают с порядком меню в UI.
#[derive(Clone, Copy)]
pub enum SortKey {
    NameAsc,
    NameDesc,
    AddedAsc,
    AddedDesc,
    MtimeAsc,
    MtimeDesc,
    SizeAsc,
    SizeDesc,
    DurationAsc,
    DurationDesc,
    TypeAsc,
    TypeDesc,
}

impl SortKey {
    pub fn from_index(i: i32) -> SortKey {
        use SortKey::*;
        match i {
            1 => NameDesc,
            2 => AddedAsc,
            3 => AddedDesc,
            4 => MtimeAsc,
            5 => MtimeDesc,
            6 => SizeAsc,
            7 => SizeDesc,
            8 => DurationAsc,
            9 => DurationDesc,
            10 => TypeAsc,
            11 => TypeDesc,
            _ => NameAsc,
        }
    }

    /// Тело выражения ORDER BY (NULL-длительности всегда в конце).
    fn order_clause(&self) -> &'static str {
        use SortKey::*;
        match self {
            NameAsc => "f.name COLLATE NOCASE ASC",
            NameDesc => "f.name COLLATE NOCASE DESC",
            AddedAsc => "f.added_at ASC, f.name COLLATE NOCASE",
            AddedDesc => "f.added_at DESC, f.name COLLATE NOCASE",
            MtimeAsc => "f.mtime ASC, f.name COLLATE NOCASE",
            MtimeDesc => "f.mtime DESC, f.name COLLATE NOCASE",
            SizeAsc => "f.size ASC, f.name COLLATE NOCASE",
            SizeDesc => "f.size DESC, f.name COLLATE NOCASE",
            // NULL и неизвестные (<0) — всегда в конце, для обоих направлений.
            DurationAsc => {
                "(f.duration_ms IS NULL OR f.duration_ms < 0), f.duration_ms ASC, f.name COLLATE NOCASE"
            }
            DurationDesc => {
                "(f.duration_ms IS NULL OR f.duration_ms < 0), f.duration_ms DESC, f.name COLLATE NOCASE"
            }
            TypeAsc => "f.media_type ASC, f.name COLLATE NOCASE",
            TypeDesc => "f.media_type DESC, f.name COLLATE NOCASE",
        }
    }
}

pub struct Db {
    conn: Connection,
}

impl Db {
    /// Открывает (создаёт при необходимости) БД, включает прагмы и мигрирует схему.
    pub fn open(path: &Path) -> rusqlite::Result<Db> {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        let db = Db { conn };
        db.migrate()?;
        Ok(db)
    }

    fn migrate(&self) -> rusqlite::Result<()> {
        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT);",
        )?;

        let current: i64 = self
            .conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'schema_version'",
                [],
                |r| r.get::<_, String>(0),
            )
            .optional()?
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);

        if current < 1 {
            self.conn.execute_batch(SCHEMA_V1)?;
        }
        // будущие миграции: if current < 2 { ... }

        if current != SCHEMA_VERSION {
            self.conn.execute(
                "INSERT INTO meta(key, value) VALUES('schema_version', ?1)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![SCHEMA_VERSION.to_string()],
            )?;
            log::info!("Схема БД: v{current} -> v{SCHEMA_VERSION}");
        }
        Ok(())
    }

    // --- Сканер ---

    /// Все живые записи (для сверки с содержимым диска).
    pub fn active_files(&self) -> rusqlite::Result<Vec<ActiveFile>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, rel_path, size, mtime, hash FROM files WHERE is_deleted = 0",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(ActiveFile {
                id: r.get(0)?,
                rel_path: r.get(1)?,
                size: r.get(2)?,
                mtime: r.get(3)?,
                hash: r.get(4)?,
            })
        })?;
        rows.collect()
    }

    /// Переносит записи на новые пути одной транзакцией.
    ///
    /// Конфликт с `idx_files_active_path` здесь невозможен: цель переезда —
    /// путь файла, который есть на диске, а такой путь не может принадлежать
    /// другой живой записи (её бы нашли по пути ещё в первой фазе скана).
    ///
    /// hash/thumb_state/added_at/resume_ms и теги не трогаем: содержимое то же.
    pub fn apply_renames(&self, renames: &[RenameFile]) -> rusqlite::Result<()> {
        if renames.is_empty() {
            return Ok(());
        }
        let tx = self.conn.unchecked_transaction()?;
        {
            let mut stmt = tx.prepare(
                "UPDATE files
                    SET rel_path = ?2, name = ?3, ext = ?4, media_type = ?5,
                        size = ?6, mtime = ?7
                  WHERE id = ?1",
            )?;
            for r in renames {
                stmt.execute(params![
                    r.id,
                    r.rel_path,
                    r.name,
                    r.ext,
                    r.media_type as i64,
                    r.size,
                    r.mtime,
                ])?;
            }
        }
        tx.commit()
    }

    /// Вставляет новый файл, возвращает id.
    pub fn insert_file(&self, f: &NewFile) -> rusqlite::Result<i64> {
        self.conn.execute(
            "INSERT INTO files (rel_path, name, ext, media_type, size, mtime, added_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                f.rel_path,
                f.name,
                f.ext,
                f.media_type as i64,
                f.size,
                f.mtime,
                f.added_at,
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Обновляет размер/mtime существующей записи (файл на месте, но изменился).
    /// Если содержимое поменялось — сбрасывает хэш и миниатюру на перегенерацию.
    pub fn touch_file(&self, id: i64, size: i64, mtime: i64) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE files
                SET size = ?2, mtime = ?3, hash = NULL, thumb_state = 0
              WHERE id = ?1 AND (size <> ?2 OR mtime <> ?3)",
            params![id, size, mtime],
        )?;
        Ok(())
    }

    /// Помечает записи как удалённые (файлов нет на диске).
    pub fn mark_deleted(&self, ids: &[i64]) -> rusqlite::Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        {
            let mut stmt = tx.prepare("UPDATE files SET is_deleted = 1 WHERE id = ?1")?;
            for id in ids {
                stmt.execute([id])?;
            }
        }
        tx.commit()
    }

    /// Живые файлы для галереи с фильтром по тегам.
    /// `include`/`exclude` — id тегов; `match_all` = true → файл должен иметь ВСЕ
    /// include-теги (AND), false → хотя бы один (OR). Пустой include = без фильтра.
    /// `name_query` — подстрока имени (пусто = без фильтра).
    /// `type_filter` — 0 все, 1 только изображения, 2 только видео.
    pub fn query_files(
        &self,
        include: &[i64],
        exclude: &[i64],
        match_all: bool,
        name_query: &str,
        type_filter: i32,
        sort: SortKey,
    ) -> rusqlite::Result<Vec<GalleryFile>> {
        use rusqlite::types::Value;
        let mut sql = String::from(
            "SELECT f.id, f.name, f.media_type, f.rel_path, f.width, f.height FROM files f WHERE f.is_deleted = 0",
        );
        let mut params: Vec<Value> = Vec::new();

        if !include.is_empty() {
            let ph = placeholders(include.len());
            if match_all {
                sql.push_str(&format!(
                    " AND (SELECT COUNT(DISTINCT ft.tag_id) FROM file_tags ft
                           WHERE ft.file_id = f.id AND ft.tag_id IN ({ph})) = {}",
                    include.len()
                ));
            } else {
                sql.push_str(&format!(
                    " AND EXISTS (SELECT 1 FROM file_tags ft
                                  WHERE ft.file_id = f.id AND ft.tag_id IN ({ph}))"
                ));
            }
            params.extend(include.iter().map(|&v| Value::Integer(v)));
        }
        if !exclude.is_empty() {
            let ph = placeholders(exclude.len());
            sql.push_str(&format!(
                " AND NOT EXISTS (SELECT 1 FROM file_tags ft
                                  WHERE ft.file_id = f.id AND ft.tag_id IN ({ph}))"
            ));
            params.extend(exclude.iter().map(|&v| Value::Integer(v)));
        }
        let nq = name_query.trim();
        if !nq.is_empty() {
            // Экранируем спецсимволы LIKE (%, _, \) через ESCAPE '\'.
            let esc: String = nq
                .chars()
                .flat_map(|c| match c {
                    '%' | '_' | '\\' => vec!['\\', c],
                    _ => vec![c],
                })
                .collect();
            sql.push_str(" AND f.name LIKE ? ESCAPE '\\'");
            params.push(Value::Text(format!("%{esc}%")));
        }
        match type_filter {
            1 => sql.push_str(" AND f.media_type = 0"),
            2 => sql.push_str(" AND f.media_type = 1"),
            _ => {}
        }
        sql.push_str(" ORDER BY ");
        sql.push_str(sort.order_clause());

        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(params.iter()), |r| {
            let mt: i64 = r.get(2)?;
            Ok(GalleryFile {
                id: r.get(0)?,
                name: r.get(1)?,
                is_video: mt == 1,
                rel_path: r.get(3)?,
                width: r.get(4)?,
                height: r.get(5)?
            })
        })?;
        rows.collect()
    }

    pub fn file_count(&self) -> rusqlite::Result<i64> {
        self.conn
            .query_row("SELECT COUNT(*) FROM files WHERE is_deleted = 0", [], |r| {
                r.get(0)
            })
    }

    /// Путь (относительно media/) и тип файла по id — для просмотрщика.
    pub fn file_info(&self, id: i64) -> rusqlite::Result<Option<(String, MediaType)>> {
        self.conn
            .query_row(
                "SELECT rel_path, media_type FROM files WHERE id = ?1",
                [id],
                |r| {
                    let rel: String = r.get(0)?;
                    let mt = MediaType::from_i64(r.get(1)?).unwrap_or(MediaType::Image);
                    Ok((rel, mt))
                },
            )
            .optional()
    }

    // --- Фоновое хэширование ---

    /// Пачка живых файлов без хэша (для фонового воркера).
    pub fn files_without_hash(&self, limit: i64) -> rusqlite::Result<Vec<(i64, String)>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, rel_path FROM files
              WHERE hash IS NULL AND is_deleted = 0
              LIMIT ?1",
        )?;
        let rows = stmt.query_map([limit], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.collect()
    }

    pub fn set_hash(&self, id: i64, hash: u64) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE files SET hash = ?2 WHERE id = ?1",
            params![id, hash as i64],
        )?;
        Ok(())
    }

    // --- Длительность видео (фоновый проход) ---

    /// Видео без заполненной длительности (duration_ms IS NULL).
    pub fn videos_without_duration(&self, limit: i64) -> rusqlite::Result<Vec<(i64, String)>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, rel_path FROM files
              WHERE media_type = 1 AND duration_ms IS NULL AND is_deleted = 0
              LIMIT ?1",
        )?;
        let rows = stmt.query_map([limit], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.collect()
    }

    pub fn set_duration(&self, id: i64, ms: i64) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE files SET duration_ms = ?2 WHERE id = ?1",
            params![id, ms],
        )?;
        Ok(())
    }

    // --- Разрешение медиа (фоновый проход) ---

    /// Файлы без заполненного разрешения (height IS NULL).
    pub fn item_without_resolution(&self, limit: i64) -> rusqlite::Result<Vec<(i64, String, bool)>> {
        // media_type: 0=image, 1=video → читаем как bool (is_video).
        let mut stmt = self.conn.prepare(
            "SELECT id, rel_path, media_type FROM files
              WHERE height IS NULL AND is_deleted = 0
              LIMIT ?1",
        )?;
        let rows = stmt.query_map([limit], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        rows.collect()
    }

    pub fn set_resolution(&self, id: i64, height: i64, width: i64) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE files SET height = ?2, width = ?3 WHERE id = ?1",
            params![id, height, width],
        )?;
        Ok(())
    }

    // --- Миниатюры ---

    /// Живые файлы, которым нужна миниатюра (state 0, либо 3 — ранее отложенные видео).
    pub fn files_needing_thumb(
        &self,
        limit: i64,
    ) -> rusqlite::Result<Vec<(i64, String, MediaType)>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, rel_path, media_type FROM files
              WHERE thumb_state IN (0, 3) AND is_deleted = 0
              LIMIT ?1",
        )?;
        let rows = stmt.query_map([limit], |r| {
            let mt = MediaType::from_i64(r.get(2)?).unwrap_or(MediaType::Image);
            Ok((r.get(0)?, r.get(1)?, mt))
        })?;
        rows.collect()
    }

    pub fn set_thumb_state(&self, id: i64, state: i64) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE files SET thumb_state = ?2 WHERE id = ?1",
            params![id, state],
        )?;
        Ok(())
    }

    // --- Теги ---

    pub fn list_tags(&self) -> rusqlite::Result<Vec<Tag>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, name FROM tags ORDER BY name COLLATE NOCASE")?;
        let rows = stmt.query_map([], |r| {
            Ok(Tag {
                id: r.get(0)?,
                name: r.get(1)?,
            })
        })?;
        rows.collect()
    }

    /// Создаёт тег (или возвращает id существующего с тем же именем).
    pub fn create_tag(&self, name: &str, now: i64) -> rusqlite::Result<i64> {
        self.conn.execute(
            "INSERT INTO tags(name, created_at) VALUES(?1, ?2)
             ON CONFLICT(name) DO NOTHING",
            params![name, now],
        )?;
        self.conn
            .query_row("SELECT id FROM tags WHERE name = ?1", [name], |r| r.get(0))
    }

    // Заготовки под будущий экран «управление тегами» (переименование/ручное
    // удаление). Авто-очистка осиротевших тегов идёт через delete_tag_if_orphan.
    #[allow(dead_code)]
    pub fn rename_tag(&self, id: i64, new_name: &str) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE tags SET name = ?2 WHERE id = ?1",
            params![id, new_name],
        )?;
        Ok(())
    }

    #[allow(dead_code)]
    pub fn delete_tag(&self, id: i64) -> rusqlite::Result<()> {
        self.conn.execute("DELETE FROM tags WHERE id = ?1", [id])?;
        Ok(())
    }

    pub fn assign_tag(&self, file_id: i64, tag_id: i64) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT INTO file_tags(file_id, tag_id) VALUES(?1, ?2)
             ON CONFLICT DO NOTHING",
            params![file_id, tag_id],
        )?;
        Ok(())
    }

    pub fn unassign_tag(&self, file_id: i64, tag_id: i64) -> rusqlite::Result<()> {
        self.conn.execute(
            "DELETE FROM file_tags WHERE file_id = ?1 AND tag_id = ?2",
            params![file_id, tag_id],
        )?;
        Ok(())
    }

    /// Удаляет тег, если на него не осталось ни одной привязки в file_tags.
    /// Файлы с is_deleted=1 считаются «живыми» (их строки file_tags остаются),
    /// поэтому тег удаляется только когда файлов с ним нет совсем.
    /// Возвращает true, если тег был удалён.
    pub fn delete_tag_if_orphan(&self, tag_id: i64) -> rusqlite::Result<bool> {
        let n = self.conn.execute(
            "DELETE FROM tags WHERE id = ?1
               AND NOT EXISTS (SELECT 1 FROM file_tags WHERE tag_id = ?1)",
            [tag_id],
        )?;
        Ok(n > 0)
    }

    pub fn tags_for_file(&self, file_id: i64) -> rusqlite::Result<Vec<Tag>> {
        let mut stmt = self.conn.prepare(
            "SELECT t.id, t.name FROM tags t
               JOIN file_tags ft ON ft.tag_id = t.id
              WHERE ft.file_id = ?1
              ORDER BY t.name COLLATE NOCASE",
        )?;
        let rows = stmt.query_map([file_id], |r| {
            Ok(Tag {
                id: r.get(0)?,
                name: r.get(1)?,
            })
        })?;
        rows.collect()
    }
}

/// "?,?,?" для IN-списка из n элементов.
fn placeholders(n: usize) -> String {
    let mut s = String::with_capacity(n * 2);
    for i in 0..n {
        if i > 0 {
            s.push(',');
        }
        s.push('?');
    }
    s
}
