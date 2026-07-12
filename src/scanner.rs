//! Сканер медиатеки: обходит `media/`, сверяет с БД и поддерживает индекс.
//!
//! Правило идентичности:
//! - файл на диске + активная запись с этим rel_path → тот же файл (обновляем);
//! - активная запись есть, файла нет → кандидат на переезд, иначе is_deleted=1;
//! - файл есть, активной записи нет → кандидат на переезд, иначе новая запись.
//!
//! Переезд (rename/move) распознаём во второй фазе, сопоставляя «пропавшие»
//! записи с «появившимися» файлами. Запись сохраняет id, а с ним теги,
//! resume_ms, added_at и миниатюру {id}.jpg — ради этого всё и затевалось.
//!
//! rel_path всегда с прямыми слешами — чтобы индекс совпадал на Windows и Linux.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use walkdir::WalkDir;

use crate::db::{ActiveFile, Db, MediaType, NewFile, RenameFile};
use crate::media;

#[derive(Default, Debug)]
pub struct ScanStats {
    pub added: usize,
    pub updated: usize,
    pub renamed: usize,
    pub deleted: usize,
    pub seen: usize,
}

/// Файл на диске, которому не нашлось активной записи по пути.
struct DiskFile {
    rel: String,
    name: String,
    ext: String,
    media_type: MediaType,
    size: i64,
    mtime: i64,
    abs: PathBuf,
    /// xxh3 содержимого. Внешний Option — «считали ли», внутренний — «удалось ли».
    hash: Option<Option<u64>>,
}

impl DiskFile {
    /// Хэш содержимого; None — файл не прочитать. Результат кэшируется.
    fn hash(&mut self) -> Option<u64> {
        if let Some(h) = self.hash {
            return h;
        }
        let h = match crate::workers::hash_file(&self.abs) {
            Ok(h) => Some(h),
            Err(e) => {
                log::warn!("Рематч: не прочитать {}: {e}", self.rel);
                None
            }
        };
        self.hash = Some(h);
        h
    }
}

/// Запись, чей файл не встретился на диске.
struct MissingRow {
    id: i64,
    size: i64,
    mtime: i64,
    hash: Option<i64>,
    name: String,
    ext: String,
    parent: String,
}

impl MissingRow {
    fn from_active(a: ActiveFile) -> MissingRow {
        MissingRow {
            id: a.id,
            size: a.size,
            mtime: a.mtime,
            hash: a.hash,
            name: basename(&a.rel_path).to_string(),
            ext: ext_of(&a.rel_path),
            parent: parent_of(&a.rel_path).to_string(),
        }
    }
}

/// Полный проход по `media_root` с обновлением индекса в `db`.
pub fn scan(media_root: &Path, db: &Db) -> rusqlite::Result<ScanStats> {
    let mut stats = ScanStats::default();

    // --- Фаза 1: обход диска, сверка по пути. Только stat, файлы не читаем.
    let mut active: HashMap<String, ActiveFile> = HashMap::new();
    for a in db.active_files()? {
        active.insert(a.rel_path.clone(), a);
    }

    let mut appeared: Vec<DiskFile> = Vec::new();

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
            Some(a) => {
                if a.size != size || a.mtime != mtime {
                    db.touch_file(a.id, size, mtime)?;
                    stats.updated += 1;
                }
                stats.seen += 1;
            }
            None => appeared.push(DiskFile {
                rel,
                name,
                ext,
                media_type,
                size,
                mtime,
                abs: path.to_path_buf(),
                hash: None,
            }),
        }
    }

    // Что осталось в active — на диске не встретилось.
    let mut missing: Vec<MissingRow> = active.into_values().map(MissingRow::from_active).collect();
    // Детерминированный порядок разрешения, не зависящий от обхода и хэш-мапы.
    missing.sort_by_key(|m| m.id);
    appeared.sort_by(|a, b| a.rel.cmp(&b.rel));

    // --- Фаза 2: рематч. Переезд возможен, только если и пропало, и появилось.
    let renames = if missing.is_empty() || appeared.is_empty() {
        Vec::new()
    } else {
        match_renames(&missing, &mut appeared)
    };

    if !renames.is_empty() {
        let rows: Vec<RenameFile> = renames
            .iter()
            .map(|&(id, i)| {
                let f = &appeared[i];
                RenameFile {
                    id,
                    rel_path: &f.rel,
                    name: &f.name,
                    ext: &f.ext,
                    media_type: f.media_type,
                    size: f.size,
                    mtime: f.mtime,
                }
            })
            .collect();
        db.apply_renames(&rows)?;
        for &(id, i) in &renames {
            log::info!("Переезд: id={id} → {}", appeared[i].rel);
        }
        stats.renamed = renames.len();
    }

    // --- Фаза 3: остаток. Непойманные файлы → новые записи, записи → удалённые.
    let matched_files: Vec<usize> = renames.iter().map(|&(_, i)| i).collect();
    let matched_rows: Vec<i64> = renames.iter().map(|&(id, _)| id).collect();

    let now = unix_now();
    for (i, f) in appeared.iter().enumerate() {
        if matched_files.contains(&i) {
            continue;
        }
        db.insert_file(&NewFile {
            rel_path: &f.rel,
            name: &f.name,
            ext: &f.ext,
            media_type: f.media_type,
            size: f.size,
            mtime: f.mtime,
            added_at: now,
        })?;
        stats.added += 1;
    }

    let gone: Vec<i64> = missing
        .iter()
        .map(|m| m.id)
        .filter(|id| !matched_rows.contains(id))
        .collect();
    if !gone.is_empty() {
        db.mark_deleted(&gone)?;
        stats.deleted = gone.len();
    }

    Ok(stats)
}

/// Сопоставляет пропавшие записи с появившимися файлами.
/// Возвращает пары (id записи, индекс в `appeared`).
///
/// Три прохода, от надёжного к слабому: сильное правило успевает забрать
/// кандидата раньше, чем до него доберётся слабое.
fn match_renames(missing: &[MissingRow], appeared: &mut [DiskFile]) -> Vec<(i64, usize)> {
    let n_rows = missing.len();
    let mut row_taken = vec![false; n_rows];
    let mut file_taken = vec![false; appeared.len()];
    let mut out: Vec<(i64, usize)> = Vec::new();

    // Проход 1: то же имя + size + ext → переезд между папками.
    // Файл не читаем: имя и размер совпали, это перенос каталога.
    for fi in 0..appeared.len() {
        let cand: Vec<usize> = (0..n_rows)
            .filter(|&mi| {
                !row_taken[mi]
                    && missing[mi].size == appeared[fi].size
                    && missing[mi].ext == appeared[fi].ext
                    && missing[mi].name == appeared[fi].name
            })
            .collect();
        // Тай-брейк: та же родительская папка, затем тот же mtime, затем id.
        let f_parent = parent_of(&appeared[fi].rel);
        let best = cand.into_iter().min_by_key(|&mi| {
            (
                missing[mi].parent != f_parent,
                missing[mi].mtime != appeared[fi].mtime,
                missing[mi].id,
            )
        });
        if let Some(mi) = best {
            row_taken[mi] = true;
            file_taken[fi] = true;
            out.push((missing[mi].id, fi));
        }
    }

    // Проход 2: имя другое (настоящее переименование), но у записи есть хэш —
    // подтверждаем содержимым. Файл читаем только здесь и только если есть
    // хоть один кандидат того же размера с непустым хэшом. Расширение здесь
    // не требуем: хэш сильнее, а .jpeg → .jpg — обычное переименование.
    for fi in 0..appeared.len() {
        if file_taken[fi] || appeared[fi].size == 0 {
            continue;
        }
        let cand: Vec<usize> = (0..n_rows)
            .filter(|&mi| {
                !row_taken[mi]
                    && missing[mi].hash.is_some()
                    && missing[mi].size == appeared[fi].size
            })
            .collect();
        if cand.is_empty() {
            continue;
        }
        let h = match appeared[fi].hash() {
            Some(h) => h as i64,
            None => continue,
        };
        let best = cand
            .into_iter()
            .filter(|&mi| missing[mi].hash == Some(h))
            .min_by_key(|&mi| (missing[mi].mtime != appeared[fi].mtime, missing[mi].id));
        if let Some(mi) = best {
            row_taken[mi] = true;
            file_taken[fi] = true;
            out.push((missing[mi].id, fi));
        }
    }

    // Проход 3: имя другое, хэша у записи нет (фоновый воркер не дошёл) —
    // подтвердить нечем. Доверяем эвристике size+ext+mtime, но только при
    // полной однозначности: ровно одна такая запись и ровно один такой файл.
    // Пустые файлы исключены — у них совпадает всё и всегда.
    for fi in 0..appeared.len() {
        if file_taken[fi] || appeared[fi].size == 0 {
            continue;
        }
        let rows: Vec<usize> = (0..n_rows)
            .filter(|&mi| {
                !row_taken[mi]
                    && missing[mi].size == appeared[fi].size
                    && missing[mi].ext == appeared[fi].ext
                    && missing[mi].mtime == appeared[fi].mtime
            })
            .collect();
        if rows.len() != 1 || missing[rows[0]].hash.is_some() {
            continue;
        }
        // Второй такой же файл на диске → неоднозначно, не гадаем.
        let rivals = (0..appeared.len())
            .filter(|&other| {
                other != fi
                    && !file_taken[other]
                    && appeared[other].size == appeared[fi].size
                    && appeared[other].ext == appeared[fi].ext
                    && appeared[other].mtime == appeared[fi].mtime
            })
            .count();
        if rivals > 0 {
            continue;
        }
        let mi = rows[0];
        row_taken[mi] = true;
        file_taken[fi] = true;
        out.push((missing[mi].id, fi));
    }

    out
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

/// Имя файла в rel_path (после последнего '/').
fn basename(rel: &str) -> &str {
    match rel.rfind('/') {
        Some(i) => &rel[i + 1..],
        None => rel,
    }
}

/// Папка в rel_path (до последнего '/'); "" — корень медиатеки.
fn parent_of(rel: &str) -> &str {
    match rel.rfind('/') {
        Some(i) => &rel[..i],
        None => "",
    }
}

/// Расширение в нижнем регистре — так же, как его пишет фаза 1.
fn ext_of(rel: &str) -> String {
    let name = basename(rel);
    match name.rfind('.') {
        Some(i) if i + 1 < name.len() => name[i + 1..].to_ascii_lowercase(),
        _ => String::new(),
    }
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// Временная песочница: media/ + своя БД. Удаляется в Drop.
    struct Sandbox {
        root: PathBuf,
        db: Db,
    }

    impl Drop for Sandbox {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    impl Sandbox {
        fn new() -> Sandbox {
            static N: AtomicU32 = AtomicU32::new(0);
            let root = std::env::temp_dir().join(format!(
                "micromedia-scan-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::SeqCst)
            ));
            let media = root.join("media");
            std::fs::create_dir_all(&media).unwrap();
            let db = Db::open(&root.join("test.db")).unwrap();
            Sandbox { root, db }
        }

        fn media(&self) -> PathBuf {
            self.root.join("media")
        }

        /// Кладёт файл с заданным содержимым и фиксированным mtime.
        /// mtime задаём явно: он часть эвристики, и полагаться на часы нельзя.
        fn put(&self, rel: &str, content: &str, mtime: u64) {
            let p = self.media().join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, content).unwrap();
            self.set_mtime(rel, mtime);
        }

        fn set_mtime(&self, rel: &str, mtime: u64) {
            let f = std::fs::File::options()
                .write(true)
                .open(self.media().join(rel))
                .unwrap();
            f.set_modified(UNIX_EPOCH + std::time::Duration::from_secs(mtime))
                .unwrap();
        }

        /// `mv` внутри media/, сохраняющий mtime (как настоящее переименование).
        fn mv(&self, from: &str, to: &str) {
            let dst = self.media().join(to);
            std::fs::create_dir_all(dst.parent().unwrap()).unwrap();
            std::fs::rename(self.media().join(from), &dst).unwrap();
        }

        fn scan(&self) -> ScanStats {
            scan(&self.media(), &self.db).unwrap()
        }

        /// Имитирует фонового хэшера: проставляет хэши всем записям без хэша.
        fn hash_all(&self) {
            for (id, rel) in self.db.files_without_hash(1000).unwrap() {
                let h = crate::workers::hash_file(&self.media().join(&rel)).unwrap();
                self.db.set_hash(id, h).unwrap();
            }
        }

        /// id живой записи по пути.
        fn id_of(&self, rel: &str) -> Option<i64> {
            self.db
                .active_files()
                .unwrap()
                .into_iter()
                .find(|a| a.rel_path == rel)
                .map(|a| a.id)
        }
    }

    /// Переименование с посчитанным хэшом: id, теги и превью сохраняются.
    #[test]
    fn rename_with_hash_keeps_identity() {
        let s = Sandbox::new();
        s.put("a.jpg", "content-a", 1000);
        s.scan();
        s.hash_all();

        let id = s.id_of("a.jpg").unwrap();
        let tag = s.db.create_tag("отпуск", 0).unwrap();
        s.db.assign_tag(id, tag).unwrap();

        s.mv("a.jpg", "b.jpg");
        let st = s.scan();

        assert_eq!((st.renamed, st.added, st.deleted), (1, 0, 0));
        assert_eq!(s.id_of("b.jpg"), Some(id), "id должен сохраниться");
        assert_eq!(s.db.tags_for_file(id).unwrap().len(), 1, "тег на месте");
    }

    /// Перенос папки: имя то же, хэша нет — рематч без чтения файлов.
    #[test]
    fn move_to_other_folder_without_hash() {
        let s = Sandbox::new();
        s.put("a.jpg", "content-a", 1000);
        s.scan();
        let id = s.id_of("a.jpg").unwrap();

        s.mv("a.jpg", "sub/a.jpg");
        let st = s.scan();

        assert_eq!((st.renamed, st.added, st.deleted), (1, 0, 0));
        assert_eq!(s.id_of("sub/a.jpg"), Some(id));
    }

    /// Переименование без хэша и без совпадения имени: рематч по эвристике,
    /// но только пока кандидат единственный.
    #[test]
    fn rename_without_hash_unique_candidate() {
        let s = Sandbox::new();
        s.put("a.jpg", "content-a", 1000);
        s.scan();
        let id = s.id_of("a.jpg").unwrap();

        s.mv("a.jpg", "renamed.jpg");
        let st = s.scan();

        assert_eq!((st.renamed, st.added, st.deleted), (1, 0, 0));
        assert_eq!(s.id_of("renamed.jpg"), Some(id));
    }

    /// Тот же случай, но кандидатов двое (одинаковый size+ext+mtime, хэшей нет)
    /// — не гадаем, обычное поведение.
    #[test]
    fn rename_without_hash_ambiguous_is_not_matched() {
        let s = Sandbox::new();
        s.put("a.jpg", "aaaaaaaaa", 1000);
        s.put("b.jpg", "bbbbbbbbb", 1000); // тот же размер и mtime
        s.scan();

        s.mv("a.jpg", "x.jpg");
        s.mv("b.jpg", "y.jpg");
        let st = s.scan();

        assert_eq!((st.renamed, st.added, st.deleted), (0, 2, 2));
    }

    /// Тот же случай, но хэши посчитаны — неоднозначность снимается содержимым.
    #[test]
    fn rename_ambiguous_resolved_by_hash() {
        let s = Sandbox::new();
        s.put("a.jpg", "aaaaaaaaa", 1000);
        s.put("b.jpg", "bbbbbbbbb", 1000);
        s.scan();
        s.hash_all();
        let (ida, idb) = (s.id_of("a.jpg").unwrap(), s.id_of("b.jpg").unwrap());

        s.mv("a.jpg", "x.jpg");
        s.mv("b.jpg", "y.jpg");
        let st = s.scan();

        assert_eq!((st.renamed, st.added, st.deleted), (2, 0, 0));
        assert_eq!(s.id_of("x.jpg"), Some(ida));
        assert_eq!(s.id_of("y.jpg"), Some(idb));
    }

    /// Копия рядом с оригиналом — это дубликат, а не переезд: новая запись.
    #[test]
    fn copy_is_a_new_record() {
        let s = Sandbox::new();
        s.put("a.jpg", "content-a", 1000);
        s.scan();
        s.hash_all();
        let id = s.id_of("a.jpg").unwrap();

        s.put("copy.jpg", "content-a", 1000); // то же содержимое, оригинал на месте
        let st = s.scan();

        assert_eq!((st.renamed, st.added, st.deleted), (0, 1, 0));
        assert_eq!(s.id_of("a.jpg"), Some(id));
        assert!(s.id_of("copy.jpg").is_some());
    }

    /// Переименование + правка содержимого: хэш не сходится → не рематч.
    #[test]
    fn rename_with_edited_content_is_not_matched() {
        let s = Sandbox::new();
        s.put("a.jpg", "content-a", 1000);
        s.scan();
        s.hash_all();

        s.mv("a.jpg", "b.jpg");
        s.put("b.jpg", "totally-different-content", 2000);
        let st = s.scan();

        assert_eq!((st.renamed, st.added, st.deleted), (0, 1, 1));
    }

    /// После переезда миниатюра остаётся валидной: id тот же, содержимое то же,
    /// значит {id}.jpg перегенерировать не надо.
    #[test]
    fn rename_keeps_thumbnail() {
        let s = Sandbox::new();
        s.put("a.jpg", "content-a", 1000);
        s.scan();
        s.hash_all();
        let id = s.id_of("a.jpg").unwrap();
        s.db.set_thumb_state(id, 1).unwrap(); // «превью готово»

        s.mv("a.jpg", "b.jpg");
        s.scan();

        assert!(
            s.db.files_needing_thumb(10).unwrap().is_empty(),
            "переезд не должен ставить файл обратно в очередь на превью"
        );
    }

    /// Обычное удаление — по-прежнему tombstone, ничего не «переезжает».
    #[test]
    fn plain_delete_still_marks_deleted() {
        let s = Sandbox::new();
        s.put("a.jpg", "content-a", 1000);
        s.put("b.jpg", "content-bbbb", 2000);
        s.scan();
        s.hash_all();

        std::fs::remove_file(s.media().join("a.jpg")).unwrap();
        let st = s.scan();

        assert_eq!((st.renamed, st.added, st.deleted), (0, 0, 1));
        assert_eq!(s.db.file_count().unwrap(), 1);
    }

    /// Пустые файлы не склеиваем по эвристике: у всех одинаковый размер.
    /// Но переезд с сохранением имени — по-прежнему ловим.
    #[test]
    fn empty_files_are_not_guessed() {
        let s = Sandbox::new();
        s.put("a.jpg", "", 1000);
        s.scan();

        s.mv("a.jpg", "b.jpg");
        let st = s.scan();
        assert_eq!((st.renamed, st.added, st.deleted), (0, 1, 1));

        s.mv("b.jpg", "sub/b.jpg");
        let st = s.scan();
        assert_eq!((st.renamed, st.added, st.deleted), (1, 0, 0));
    }

    /// Перенос каталога целиком: все файлы переезжают, ни один не хэшируется.
    #[test]
    fn folder_move_keeps_all_ids() {
        let s = Sandbox::new();
        for i in 0..5 {
            s.put(&format!("old/{i}.jpg"), &format!("content-{i}"), 1000 + i);
        }
        s.scan();
        let ids: Vec<Option<i64>> = (0..5).map(|i| s.id_of(&format!("old/{i}.jpg"))).collect();

        std::fs::rename(s.media().join("old"), s.media().join("new")).unwrap();
        let st = s.scan();

        assert_eq!((st.renamed, st.added, st.deleted), (5, 0, 0));
        for i in 0..5 {
            assert_eq!(s.id_of(&format!("new/{i}.jpg")), ids[i as usize]);
        }
    }

    /// Смена расширения при переименовании обновляет ext и media_type.
    #[test]
    fn rename_changing_extension_updates_type() {
        let s = Sandbox::new();
        s.put("clip.mp4", "video-bytes", 1000);
        s.scan();
        s.hash_all();
        let id = s.id_of("clip.mp4").unwrap();

        s.mv("clip.mp4", "clip.mkv");
        let st = s.scan();

        assert_eq!(st.renamed, 1);
        let (rel, mt) = s.db.file_info(id).unwrap().unwrap();
        assert_eq!(rel, "clip.mkv");
        assert_eq!(mt, MediaType::Video);
    }
}
