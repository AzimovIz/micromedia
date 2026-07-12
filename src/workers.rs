//! Полный проход обработки медиатеки: индексация → превью → хеш → длительность.
//!
//! Одноразовая функция `reindex` запускается в фоновом потоке при старте и по
//! кнопке «Обновить». Обрабатывает ВСЕ подходящие файлы (пачками, пока не
//! кончатся), а `on_update` дёргает UI, чтобы показывать прогресс.
//!
//! Каждый шаг пишет в лог, ЧТО он собирается делать, ДО того как начнёт —
//! иначе долгий файл (или зависший mpv) выглядит снаружи как повисшая
//! программа. Уровень info; фильтр по умолчанию задан в main.rs.

use std::io::Read;
use std::path::Path;
use std::time::Instant;

use xxhash_rust::xxh3::Xxh3;

use crate::db::{Db, MediaType, Pending};

pub(crate) const THUMB_MAX: u32 = 320;
const BATCH: i64 = 16;
/// Позиция кадра для видео-превью (% длительности).
const VIDEO_THUMB_PERCENT: u32 = 20;

/// thumb_state: 0 нет, 1 готово, 2 ошибка.
const THUMB_OK: i64 = 1;
const THUMB_ERR: i64 = 2;

/// Файл, обработка которого заняла больше этого, — подозрительный: пишем warn.
const SLOW_ITEM_SECS: f32 = 10.0;

/// Полный проход: индексация media/ → превью → контент-хэш → длительность.
///
/// Превью идут раньше хэша сознательно: хэш читает каждый байт каждого файла,
/// и на видеотеке это десятки минут, в течение которых пользователь не видит
/// вообще никаких изменений. Превью же дают картинку почти сразу. Для рематча
/// переездов хэш всё равно успеет посчитаться до следующего скана.
///
/// `on_update` вызывается после скана и после каждой пачки превью, чтобы UI
/// подхватывал новые файлы и миниатюры по мере готовности.
pub fn reindex(db_path: &Path, media: &Path, thumbs: &Path, on_update: impl Fn()) {
    let started = Instant::now();
    log::info!("Индексация: старт, media={}", media.display());

    let db = match Db::open(db_path) {
        Ok(d) => d,
        Err(e) => {
            log::error!("Индексация: не удалось открыть БД: {e}");
            return;
        }
    };

    // 1. Индексация файловой системы.
    let t = Instant::now();
    log::info!("Скан: обход файловой системы…");
    match crate::scanner::scan(media, &db) {
        Ok(s) => log::info!(
            "Скан: готов за {:.1} с — добавлено {}, обновлено {}, переехало {}, удалено {} (на месте {})",
            t.elapsed().as_secs_f32(),
            s.added,
            s.updated,
            s.renamed,
            s.deleted,
            s.seen
        ),
        Err(e) => log::error!("Скан: {e}"),
    }
    on_update();

    // 2. Миниатюры — все, пачками (ошибочные помечаются и выпадают).
    let total = db.count_pending(Pending::Thumb).unwrap_or(0);
    log::info!("Превью: нужно сделать {total}");
    let t = Instant::now();
    let mut done = 0i64;
    loop {
        let batch = db.files_needing_thumb(BATCH).unwrap_or_default();
        if batch.is_empty() {
            break;
        }
        for (id, rel, mt) in batch {
            done += 1;
            let kind = match mt {
                MediaType::Image => "фото",
                MediaType::Video => "видео",
            };
            log::info!("Превью {done}/{total}: id={id} ({kind}) {rel}");
            let item = Instant::now();

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
                    report_slow("Превью", id, &rel, item);
                }
                Err(e) => {
                    log::warn!("Превью id={id} {rel}: ошибка — {e}");
                    let _ = db.set_thumb_state(id, THUMB_ERR);
                }
            }
        }
        on_update();
    }
    log::info!("Превью: готово {done} за {:.1} с", t.elapsed().as_secs_f32());

    // 3. Контент-хэш — все. При стойкой ошибке (файл пропал) пачка не даёт
    //    прогресса → выходим, чтобы не зациклиться (хэш остаётся NULL).
    let total = db.count_pending(Pending::Hash).unwrap_or(0);
    log::info!("Хэш: нужно посчитать {total} (читается всё содержимое файлов)");
    let t = Instant::now();
    let mut done = 0i64;
    loop {
        let batch = db.files_without_hash(BATCH).unwrap_or_default();
        if batch.is_empty() {
            break;
        }
        let mut progressed = false;
        for (id, rel) in batch {
            done += 1;
            log::info!("Хэш {done}/{total}: id={id} {rel}");
            let item = Instant::now();

            let src = media.join(&rel);
            match hash_file(&src) {
                Ok(h) => {
                    let _ = db.set_hash(id, h);
                    progressed = true;
                    report_slow("Хэш", id, &rel, item);
                }
                Err(e) => log::warn!("Хэш id={id} {rel}: ошибка — {e}"),
            }
        }
        if !progressed {
            log::warn!("Хэш: пачка без прогресса, шаг прерван (файлы недоступны?)");
            break;
        }
    }
    log::info!("Хэш: готово {done} за {:.1} с", t.elapsed().as_secs_f32());

    // 4. Длительность видео — все (None → -1 sentinel, чтобы не пробовать вечно).
    let total = db.count_pending(Pending::Duration).unwrap_or(0);
    log::info!("Длительность: нужно определить {total}");
    let t = Instant::now();
    let mut done = 0i64;
    loop {
        let batch = db.videos_without_duration(BATCH).unwrap_or_default();
        if batch.is_empty() {
            break;
        }
        for (id, rel) in batch {
            done += 1;
            log::info!("Длительность {done}/{total}: id={id} {rel}");
            let item = Instant::now();

            let src = media.join(&rel);
            match crate::videothumb::probe_duration(&src) {
                Some(d) => {
                    let _ = db.set_duration(id, (d * 1000.0) as i64);
                    report_slow("Длительность", id, &rel, item);
                }
                None => {
                    log::warn!("Длительность id={id} {rel}: не определить");
                    let _ = db.set_duration(id, -1);
                }
            }
        }
    }
    log::info!(
        "Длительность: готово {done} за {:.1} с",
        t.elapsed().as_secs_f32()
    );

    // 5. Разрешение (width×height): фото — из заголовка, видео — mpv-пробой.
    //    Неопределимые помечаем sentinel'ом (-1), чтобы не пробовать вечно.
    let total = db.count_pending(Pending::Resolution).unwrap_or(0);
    log::info!("Разрешение: нужно определить {total}");
    let t = Instant::now();
    let mut done = 0i64;
    loop {
        let batch = db.item_without_resolution(BATCH).unwrap_or_default();
        if batch.is_empty() {
            break;
        }
        let mut progressed = false;
        for (id, rel, is_video) in batch {
            done += 1;
            log::info!("Разрешение {done}/{total}: id={id} {rel}");
            let item = Instant::now();

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
                    log::warn!("Разрешение id={id} {rel}: не определить");
                    (-1, -1)
                }
            };
            // set_resolution(id, height, width) — порядок аргументов важен.
            if db.set_resolution(id, h, w).is_ok() {
                progressed = true;
            }
            report_slow("Разрешение", id, &rel, item);
        }
        // Защита от зацикливания, если БД не принимает записи.
        if !progressed {
            log::warn!("Разрешение: пачка без прогресса, шаг прерван (БД не пишется?)");
            break;
        }
    }
    log::info!(
        "Разрешение: готово {done} за {:.1} с",
        t.elapsed().as_secs_f32()
    );

    // Финальное обновление (длительность влияет на сортировку).
    on_update();
    log::info!(
        "Индексация: завершена за {:.1} с",
        started.elapsed().as_secs_f32()
    );
}

/// Отдельно подсвечивает файлы, на которых шаг заметно буксует, — по ним и
/// надо искать причину «зависания».
fn report_slow(stage: &str, id: i64, rel: &str, since: Instant) {
    let secs = since.elapsed().as_secs_f32();
    if secs >= SLOW_ITEM_SECS {
        log::warn!("{stage} id={id} {rel}: долго — {secs:.1} с");
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Дымовой прогон всего конвейера на картинке: доходит до конца, делает
    /// превью, проставляет хэш и разрешение. Логи смотреть с --nocapture.
    #[test]
    fn reindex_processes_an_image_end_to_end() {
        let _ = env_logger::builder()
            .filter_level(log::LevelFilter::Info)
            .is_test(true)
            .try_init();
        let root = std::env::temp_dir().join(format!("micromedia-reindex-{}", std::process::id()));
        let (media, thumbs) = (root.join("media"), root.join("thumbs"));
        std::fs::create_dir_all(&media).unwrap();
        std::fs::create_dir_all(&thumbs).unwrap();
        image::RgbImage::new(64, 48)
            .save(media.join("pic.png"))
            .unwrap();

        let db_path = root.join("test.db");
        reindex(&db_path, &media, &thumbs, || {});

        let db = Db::open(&db_path).unwrap();
        assert_eq!(db.file_count().unwrap(), 1);
        assert_eq!(db.count_pending(Pending::Hash).unwrap(), 0, "хэш посчитан");
        assert_eq!(db.count_pending(Pending::Thumb).unwrap(), 0, "превью есть");
        assert_eq!(db.count_pending(Pending::Resolution).unwrap(), 0);
        let id = db.active_files().unwrap()[0].id;
        assert!(thumbs.join(format!("{id}.jpg")).exists());

        let _ = std::fs::remove_dir_all(&root);
    }
}
