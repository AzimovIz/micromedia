//! Классификация файлов по расширению.
//!
//! Видео отдаём mpv (он играет практически всё) — список расширений просто
//! отсекает не-медиа при сканировании. Картинки декодируем крейтом `image`.

use crate::db::MediaType;

const IMAGE_EXTS: &[&str] = &["jpg", "jpeg", "png", "webp", "gif", "bmp"];

const VIDEO_EXTS: &[&str] = &[
    "mp4", "mkv", "avi", "webm", "mov", "m4v", "wmv", "flv", "mpg", "mpeg", "ts", "m2ts", "3gp",
    "ogv",
];

/// Тип медиа по расширению (уже в нижнем регистре, без точки). `None` — не медиа.
pub fn classify(ext: &str) -> Option<MediaType> {
    if IMAGE_EXTS.contains(&ext) {
        Some(MediaType::Image)
    } else if VIDEO_EXTS.contains(&ext) {
        Some(MediaType::Video)
    } else {
        None
    }
}
