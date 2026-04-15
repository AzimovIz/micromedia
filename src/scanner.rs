use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;
use std::sync::mpsc;
use std::thread;
use walkdir::WalkDir;

use crate::db::{detect_media_type, MediaType};

pub struct ScannedFile {
    pub relative_path: String,
    pub filename: String,
    pub media_type: MediaType,
    pub file_size: Option<i64>,
}

pub enum ScanEvent {
    /// A new file was found (not in known_paths)
    NewFile(ScannedFile),
    /// Scan finished; contains all found relative paths (for deletion of missing)
    Finished {
        all_paths: Vec<String>,
        total: usize,
        new_count: usize,
    },
}

/// Start scanning in a background thread.
/// `known_paths` — set of relative paths already in DB, used to skip existing files.
pub fn scan_media_background(
    media_dir: PathBuf,
    known_paths: HashSet<String>,
) -> mpsc::Receiver<ScanEvent> {
    let (tx, rx) = mpsc::channel();

    thread::spawn(move || {
        if !media_dir.exists() {
            fs::create_dir_all(&media_dir).ok();
        }

        let mut all_paths: Vec<String> = Vec::new();
        let mut new_count: usize = 0;

        for entry in WalkDir::new(&media_dir)
            .follow_links(true)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }

            let media_type = match detect_media_type(path) {
                Some(t) => t,
                None => continue,
            };

            let relative = match path.strip_prefix(&media_dir) {
                Ok(r) => r.to_string_lossy().replace('\\', "/"),
                Err(_) => continue,
            };

            all_paths.push(relative.clone());

            if known_paths.contains(&relative) {
                continue;
            }

            let filename = path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();

            let file_size = fs::metadata(path).ok().map(|m| m.len() as i64);

            new_count += 1;

            let _ = tx.send(ScanEvent::NewFile(ScannedFile {
                relative_path: relative,
                filename,
                media_type,
                file_size,
            }));
        }

        let total = all_paths.len();
        let _ = tx.send(ScanEvent::Finished {
            all_paths,
            total,
            new_count,
        });
    });

    rx
}
