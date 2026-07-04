//! Фоновые воркеры: генерация миниатюр и контент-хэш (xxh3).
//!
//! Отдельный поток со своим соединением к БД. Обрабатывает пачками, при простое
//! засыпает. Живёт до закрытия приложения (демон-поток).

use std::io::Read;
use std::path::Path;
use std::time::Duration;

use xxhash_rust::xxh3::Xxh3;

use crate::db::{Db, MediaType};
use crate::paths::Paths;

pub(crate) const THUMB_MAX: u32 = 320;
const BATCH: i64 = 16;
/// Позиция кадра для видео-превью (% длительности).
const VIDEO_THUMB_PERCENT: u32 = 20;

/// thumb_state: 0 нет, 1 готово, 2 ошибка.
const THUMB_OK: i64 = 1;
const THUMB_ERR: i64 = 2;

pub fn spawn(paths: &Paths) {
    let db_path = paths.db.clone();
    let media = paths.media.clone();
    let thumbs = paths.thumbnails.clone();

    std::thread::spawn(move || {
        let db = match Db::open(&db_path) {
            Ok(d) => d,
            Err(e) => {
                log::error!("Воркеры: не удалось открыть БД: {e}");
                return;
            }
        };

        loop {
            let mut progressed = false;

            // --- Миниатюры ---
            if let Ok(batch) = db.files_needing_thumb(BATCH) {
                for (id, rel, mt) in batch {
                    let src = media.join(&rel);
                    let dst = thumbs.join(format!("{id}.jpg"));
                    let res: Result<Option<f64>, String> = match mt {
                        MediaType::Image => make_image_thumb(&src, &dst).map(|_| None),
                        MediaType::Video => {
                            crate::videothumb::generate(&src, &dst, VIDEO_THUMB_PERCENT, THUMB_MAX)
                        }
                    };
                    match res {
                        Ok(dur) => {
                            let _ = db.set_thumb_state(id, THUMB_OK);
                            if let Some(d) = dur {
                                let _ = db.set_duration(id, (d * 1000.0) as i64);
                            }
                        }
                        Err(e) => {
                            log::warn!("Миниатюра {rel}: {e}");
                            let _ = db.set_thumb_state(id, THUMB_ERR);
                        }
                    }
                    progressed = true;
                }
            }

            // --- Контент-хэш ---
            if let Ok(batch) = db.files_without_hash(BATCH) {
                for (id, rel) in batch {
                    let src = media.join(&rel);
                    match hash_file(&src) {
                        Ok(h) => {
                            let _ = db.set_hash(id, h);
                            progressed = true;
                        }
                        // При ошибке hash остаётся NULL; файл, скорее всего, пропал
                        // и следующий скан пометит is_deleted=1 — тогда он выпадет.
                        Err(e) => log::warn!("Хэш {rel}: {e}"),
                    }
                }
            }

            // --- Длительность видео (по одному, пока не кончатся) ---
            if let Ok(batch) = db.videos_without_duration(BATCH) {
                for (id, rel) in batch {
                    let src = media.join(&rel);
                    match crate::videothumb::probe_duration(&src) {
                        // -1 = «неизвестно» (не пробуем повторно вечно)
                        Some(d) => {
                            let _ = db.set_duration(id, (d * 1000.0) as i64);
                        }
                        None => {
                            log::warn!("Длительность {rel}: не определить");
                            let _ = db.set_duration(id, -1);
                        }
                    }
                    progressed = true;
                }
            }

            if !progressed {
                std::thread::sleep(Duration::from_secs(2));
            }
        }
    });
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
fn hash_file(path: &Path) -> std::io::Result<u64> {
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
