//! Полный проход обработки медиатеки: индексация → хеш → превью → длительность.
//!
//! Одноразовая функция `reindex` запускается в фоновом потоке при старте и по
//! кнопке «Обновить». Обрабатывает ВСЕ подходящие файлы (пачками, пока не
//! кончатся), а `on_update` дёргает UI, чтобы показывать прогресс.

use std::io::Read;
use std::path::Path;

use xxhash_rust::xxh3::Xxh3;

use crate::db::{Db, MediaType};

pub(crate) const THUMB_MAX: u32 = 320;
const BATCH: i64 = 16;
/// Позиция кадра для видео-превью (% длительности).
const VIDEO_THUMB_PERCENT: u32 = 20;

/// thumb_state: 0 нет, 1 готово, 2 ошибка.
const THUMB_OK: i64 = 1;
const THUMB_ERR: i64 = 2;

/// Полный проход: индексация media/ → контент-хэш → превью → длительность.
/// `on_update` вызывается после скана и после каждой пачки превью, чтобы UI
/// подхватывал новые файлы и миниатюры по мере готовности.
pub fn reindex(db_path: &Path, media: &Path, thumbs: &Path, on_update: impl Fn()) {
    let db = match Db::open(db_path) {
        Ok(d) => d,
        Err(e) => {
            log::error!("Индексация: не удалось открыть БД: {e}");
            return;
        }
    };

    // 1. Индексация файловой системы.
    match crate::scanner::scan(media, &db) {
        Ok(s) => log::info!(
            "Скан: добавлено {}, обновлено {}, переехало {}, удалено {} (на месте {})",
            s.added,
            s.updated,
            s.renamed,
            s.deleted,
            s.seen
        ),
        Err(e) => log::error!("Скан: {e}"),
    }
    on_update();

    // 2. Контент-хэш — все. Идёт ПЕРЕД превью: хэш — единственное, чем скан
    //    может подтвердить переезд файла (сам файл к тому моменту уже пропал),
    //    поэтому чем раньше библиотека прохэширована, тем строже рематч.
    //    При стойкой ошибке (файл пропал) пачка не даёт прогресса → выходим,
    //    чтобы не зациклиться (хэш остаётся NULL).
    loop {
        let batch = db.files_without_hash(BATCH).unwrap_or_default();
        if batch.is_empty() {
            break;
        }
        let mut progressed = false;
        for (id, rel) in batch {
            let src = media.join(&rel);
            match hash_file(&src) {
                Ok(h) => {
                    let _ = db.set_hash(id, h);
                    progressed = true;
                }
                Err(e) => log::warn!("Хэш {rel}: {e}"),
            }
        }
        if !progressed {
            break;
        }
    }

    // 3. Миниатюры — все, пачками (ошибочные помечаются и выпадают).
    loop {
        let batch = db.files_needing_thumb(BATCH).unwrap_or_default();
        if batch.is_empty() {
            break;
        }
        for (id, rel, mt) in batch {
            let src = media.join(&rel);
            let dst = thumbs.join(format!("{id}.jpg"));
            let res: Result<(Option<f64>, Option<(i64, i64)>), String> = match mt {
                MediaType::Image => make_image_thumb(&src, &dst).map(|_| (None, None)),
                MediaType::Video => {
                    crate::videothumb::generate(&src, &dst, VIDEO_THUMB_PERCENT, THUMB_MAX)
                }
            };
            match res {
                Ok((dur, dims)) => {
                    let _ = db.set_thumb_state(id, THUMB_OK);
                    if let Some(d) = dur {
                        let _ = db.set_duration(id, (d * 1000.0) as i64);
                    }
                    // Разрешение видео получаем даром вместе с превью; фото
                    // заполняются отдельным проходом (шаг 5) из заголовка.
                    if let Some((w, h)) = dims {
                        let _ = db.set_resolution(id, h, w);
                    }
                }
                Err(e) => {
                    log::warn!("Миниатюра {rel}: {e}");
                    let _ = db.set_thumb_state(id, THUMB_ERR);
                }
            }
        }
        on_update();
    }

    // 4. Длительность видео — все (None → -1 sentinel, чтобы не пробовать вечно).
    loop {
        let batch = db.videos_without_duration(BATCH).unwrap_or_default();
        if batch.is_empty() {
            break;
        }
        for (id, rel) in batch {
            let src = media.join(&rel);
            match crate::videothumb::probe_duration(&src) {
                Some(d) => {
                    let _ = db.set_duration(id, (d * 1000.0) as i64);
                }
                None => {
                    log::warn!("Длительность {rel}: не определить");
                    let _ = db.set_duration(id, -1);
                }
            }
        }
    }

    // 5. Разрешение (width×height): фото — из заголовка, видео — mpv-пробой.
    //    Неопределимые помечаем sentinel'ом (-1), чтобы не пробовать вечно.
    loop {
        let batch = db.item_without_resolution(BATCH).unwrap_or_default();
        if batch.is_empty() {
            break;
        }
        let mut progressed = false;
        for (id, rel, is_video) in batch {
            let src = media.join(&rel);
            let dims = if is_video {
                crate::videothumb::probe_resolution(&src)
            } else {
                // image_dimensions читает только заголовок (без полного декода).
                image::image_dimensions(&src)
                    .ok()
                    .map(|(w, h)| (w as i64, h as i64))
            };
            let (w, h) = match dims {
                Some(wh) => wh,
                None => {
                    log::warn!("Разрешение {rel}: не определить");
                    (-1, -1)
                }
            };
            // set_resolution(id, height, width) — порядок аргументов важен.
            if db.set_resolution(id, h, w).is_ok() {
                progressed = true;
            }
        }
        // Защита от зацикливания, если БД не принимает записи.
        if !progressed {
            break;
        }
    }

    // Финальное обновление (длительность влияет на сортировку).
    on_update();
}

/// Делает JPEG-миниатюру, вписанную в THUMB_MAX×THUMB_MAX с сохранением пропорций.
///
/// Пишем во временный файл и атомарно переименовываем — иначе UI может прочитать
/// наполовину записанный JPEG (нижняя часть превью «серая»).
pub(crate) fn make_image_thumb(src: &Path, dst: &Path) -> Result<(), String> {
    let img = image::open(src).map_err(|e| e.to_string())?;
    // .thumbnail — быстрый ресайз с сохранением пропорций.
    let thumb = img.thumbnail(THUMB_MAX, THUMB_MAX).to_rgb8();

    let tmp = dst.with_extension("jpg.tmp");
    {
        let mut file = std::fs::File::create(&tmp).map_err(|e| e.to_string())?;
        image::DynamicImage::ImageRgb8(thumb)
            .write_to(&mut file, image::ImageFormat::Jpeg)
            .map_err(|e| e.to_string())?;
        // file закрывается в конце блока -> данные на диске до rename.
    }
    std::fs::rename(&tmp, dst).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        e.to_string()
    })
}

/// Потоковый xxh3 содержимого файла.
pub(crate) fn hash_file(path: &Path) -> std::io::Result<u64> {
    let mut f = std::fs::File::open(path)?;
    let mut hasher = Xxh3::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.digest())
}
