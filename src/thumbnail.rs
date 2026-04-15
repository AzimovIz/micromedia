use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;
use std::thread;

use crate::config;
use crate::db::{MediaFile, MediaType};

/// Generates a thumbnail for a video file using ffmpeg.
/// Returns the relative path (from appdata/thumbnails/) on success.
pub fn generate_thumbnail(media_path: &Path, media_id: i64, thumbnail_width: u32) -> Option<String> {
    let thumbs_dir = config::thumbnails_dir();
    let thumb_filename = format!("{}.jpg", media_id);
    let thumb_path = thumbs_dir.join(&thumb_filename);

    let ffmpeg = find_ffmpeg();

    let status = Command::new(&ffmpeg)
        .args([
            "-y",
            "-ss", "5",
            "-i",
        ])
        .arg(media_path)
        .args([
            "-frames:v", "1",
            "-vf", &format!("scale={}:-1", thumbnail_width),
        ])
        .arg(&thumb_path)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();

    match status {
        Ok(s) if s.success() && thumb_path.exists() => Some(thumb_filename),
        _ => {
            // Try at 0 seconds for very short videos
            let status = Command::new(&ffmpeg)
                .args(["-y", "-ss", "0", "-i"])
                .arg(media_path)
                .args(["-frames:v", "1", "-vf", &format!("scale={}:-1", thumbnail_width)])
                .arg(&thumb_path)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
            match status {
                Ok(s) if s.success() && thumb_path.exists() => Some(thumb_filename),
                _ => None,
            }
        }
    }
}

/// Generates a thumbnail from image file by resizing it.
pub fn generate_image_thumbnail(media_path: &Path, media_id: i64, thumbnail_width: u32) -> Option<String> {
    let img = image::open(media_path).ok()?;
    let thumb = img.thumbnail(thumbnail_width, thumbnail_width);
    let thumbs_dir = config::thumbnails_dir();
    let thumb_filename = format!("{}.jpg", media_id);
    let thumb_path = thumbs_dir.join(&thumb_filename);
    thumb.save(&thumb_path).ok()?;
    Some(thumb_filename)
}

pub struct ThumbnailResult {
    pub media_id: i64,
    pub thumb_filename: Option<String>,
}

/// Spawns a background thread to generate thumbnails for all media without one.
/// Returns a receiver to poll for results.
pub fn generate_thumbnails_background(
    media_files: Vec<MediaFile>,
    media_root: PathBuf,
    thumbnail_width: u32,
) -> mpsc::Receiver<ThumbnailResult> {
    let (tx, rx) = mpsc::channel();

    thread::spawn(move || {
        for file in media_files {
            if file.thumbnail_path.is_some() {
                continue;
            }

            let full_path = media_root.join(&file.path);
            let thumb = match file.media_type {
                MediaType::Video => generate_thumbnail(&full_path, file.id, thumbnail_width),
                MediaType::Image => generate_image_thumbnail(&full_path, file.id, thumbnail_width),
            };

            let _ = tx.send(ThumbnailResult {
                media_id: file.id,
                thumb_filename: thumb,
            });
        }
    });

    rx
}

fn find_ffmpeg() -> PathBuf {
    // Check appdata/libs/ first (portable)
    let portable = config::appdata_dir().join("libs").join(ffmpeg_binary_name());
    if portable.exists() {
        return portable;
    }
    // Fall back to system PATH
    PathBuf::from(ffmpeg_binary_name())
}

fn ffmpeg_binary_name() -> &'static str {
    if cfg!(windows) {
        "ffmpeg.exe"
    } else {
        "ffmpeg"
    }
}
