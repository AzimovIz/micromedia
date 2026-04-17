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

/// Persistent background worker that processes thumbnail requests one-by-one.
/// A single thread owns the ffmpeg invocations; the main thread queues files
/// via `queue()` and drains completed results via `try_recv()`.
pub struct ThumbnailWorker {
    input: mpsc::Sender<MediaFile>,
    output: mpsc::Receiver<ThumbnailResult>,
    pending: usize,
}

impl ThumbnailWorker {
    pub fn start(media_root: PathBuf, thumbnail_width: u32) -> Self {
        let (in_tx, in_rx) = mpsc::channel::<MediaFile>();
        let (out_tx, out_rx) = mpsc::channel::<ThumbnailResult>();

        thread::spawn(move || {
            while let Ok(file) = in_rx.recv() {
                let full_path = media_root.join(&file.path);
                let thumb = match file.media_type {
                    MediaType::Video => generate_thumbnail(&full_path, file.id, thumbnail_width),
                    MediaType::Image => generate_image_thumbnail(&full_path, file.id, thumbnail_width),
                };
                let _ = out_tx.send(ThumbnailResult {
                    media_id: file.id,
                    thumb_filename: thumb,
                });
            }
        });

        ThumbnailWorker {
            input: in_tx,
            output: out_rx,
            pending: 0,
        }
    }

    pub fn queue(&mut self, file: MediaFile) {
        if file.thumbnail_path.is_some() {
            return;
        }
        if self.input.send(file).is_ok() {
            self.pending += 1;
        }
    }

    pub fn try_recv(&mut self) -> Option<ThumbnailResult> {
        match self.output.try_recv() {
            Ok(r) => {
                self.pending = self.pending.saturating_sub(1);
                Some(r)
            }
            _ => None,
        }
    }

    pub fn busy(&self) -> bool {
        self.pending > 0
    }
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
