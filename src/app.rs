use std::sync::mpsc;

use eframe;
use egui;

use crate::config::{self, Config};
use crate::db::{Database, MediaFile, Tag};
use crate::player::Player;
use crate::scanner::{self, ScanEvent};
use crate::thumbnail::ThumbnailWorker;
use crate::ui::gallery::{self, GalleryAction, GalleryState};
use crate::ui::sidebar::{self, SidebarAction, SidebarState};
use crate::ui::tag_editor::{self, TagEditorAction, TagEditorState};
use crate::ui::viewer::{self, ViewerAction, ViewerState};

#[derive(PartialEq)]
enum AppView {
    Gallery,
    Viewer,
}

pub struct MediaManagerApp {
    config: Config,
    db: Database,
    player: Player,
    view: AppView,

    // Data
    media_files: Vec<MediaFile>,
    tags_with_counts: Vec<(Tag, usize)>,
    all_tags: Vec<Tag>,

    // UI state
    sidebar_state: SidebarState,
    gallery_state: GalleryState,
    viewer_state: ViewerState,
    tag_editor_state: TagEditorState,

    // Background scanning
    scan_receiver: Option<mpsc::Receiver<ScanEvent>>,
    scanning: bool,

    // Background thumbnail generation (persistent worker)
    thumb_worker: ThumbnailWorker,

    // Status message
    status: String,
}

impl MediaManagerApp {
    pub fn new(_cc: &eframe::CreationContext<'_>) -> Self {
        let config = config::load_config();
        config::ensure_dirs(&config);

        let db = Database::open().expect("Failed to open database");

        // Load existing data from DB immediately (fast)
        let media_files = db.get_all_media().unwrap_or_default();
        let tags_with_counts = db.get_tag_counts().unwrap_or_default();
        let all_tags = db.get_all_tags().unwrap_or_default();

        // Start background scan
        let known_paths = db.get_all_paths().unwrap_or_default();
        let media_root = config::media_dir(&config);
        let scan_rx = scanner::scan_media_background(media_root.clone(), known_paths);

        // Start single persistent thumbnail worker and queue initial files
        let mut thumb_worker =
            ThumbnailWorker::start(media_root, config.thumbnail_width);
        for f in &media_files {
            if f.thumbnail_path.is_none() {
                thumb_worker.queue(f.clone());
            }
        }

        let columns = config.gallery_columns;

        let mut app = Self {
            config,
            db,
            player: Player::new(),
            view: AppView::Gallery,
            media_files,
            tags_with_counts,
            all_tags,
            sidebar_state: SidebarState::default(),
            gallery_state: GalleryState::new(columns),
            viewer_state: ViewerState::default(),
            tag_editor_state: TagEditorState::default(),
            scan_receiver: Some(scan_rx),
            scanning: true,
            thumb_worker,
            status: "Scanning...".to_string(),
        };
        app.update_extensions();
        app
    }

    fn start_scan(&mut self) {
        let known_paths = self.db.get_all_paths().unwrap_or_default();
        let media_root = config::media_dir(&self.config);
        let scan_rx = scanner::scan_media_background(media_root, known_paths);
        self.scan_receiver = Some(scan_rx);
        self.scanning = true;
        self.status = "Scanning...".to_string();
    }

    fn poll_scan(&mut self) {
        let rx = match &self.scan_receiver {
            Some(rx) => rx,
            None => return,
        };

        // Cap per-frame work so the scanner can't flood the main thread with
        // a huge batch of new files in a single frame.
        const MAX_EVENTS_PER_FRAME: usize = 64;

        let mut new_files_added = false;
        let mut finished = false;

        for _ in 0..MAX_EVENTS_PER_FRAME {
            match rx.try_recv() {
                Ok(ScanEvent::NewFile(file)) => {
                    self.db.upsert_media(
                        &file.relative_path,
                        &file.filename,
                        &file.media_type,
                        file.file_size,
                    ).ok();

                    if let Ok(Some(media)) = self.db.get_media_by_path(&file.relative_path) {
                        self.thumb_worker.queue(media.clone());
                        self.media_files.push(media);
                        new_files_added = true;
                    }
                }
                Ok(ScanEvent::Finished { all_paths, total, new_count }) => {
                    let removed = self.db.delete_missing(&all_paths).unwrap_or(0);
                    if removed > 0 {
                        self.refresh_media_list();
                    }
                    self.status = format!(
                        "Scan complete: {} files ({} new, {} removed)",
                        total, new_count, removed
                    );
                    self.scanning = false;
                    self.scan_receiver = None;
                    finished = true;
                    break;
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.scanning = false;
                    self.scan_receiver = None;
                    self.status = "Scan finished".to_string();
                    finished = true;
                    break;
                }
            }
        }

        if new_files_added {
            self.apply_sort();
            self.tags_with_counts = self.db.get_tag_counts().unwrap_or_default();
            self.all_tags = self.db.get_all_tags().unwrap_or_default();
        }

        // Update status while scan is in progress to show activity.
        if self.scanning && !finished {
            self.status = format!("Scanning... {} files so far", self.media_files.len());
        }
    }

    fn refresh_media_list(&mut self) {
        let mut files = if self.sidebar_state.included_tags.is_empty()
            && self.sidebar_state.excluded_tags.is_empty()
        {
            self.db.get_all_media().unwrap_or_default()
        } else {
            self.db
                .get_media_by_tag_filter(
                    &self.sidebar_state.included_tags,
                    &self.sidebar_state.excluded_tags,
                    self.sidebar_state.match_all,
                )
                .unwrap_or_default()
        };

        // Search filter
        let query = self.sidebar_state.search_query.to_lowercase();
        if !query.is_empty() {
            files.retain(|f| f.filename.to_lowercase().contains(&query));
        }

        // Extension filter
        let ext = &self.sidebar_state.extension_filter;
        if !ext.is_empty() {
            files.retain(|f| {
                f.filename
                    .rsplit('.')
                    .next()
                    .map(|e| e.eq_ignore_ascii_case(ext))
                    .unwrap_or(false)
            });
        }

        self.media_files = files;
        self.apply_sort();
        self.tags_with_counts = self.db.get_tag_counts().unwrap_or_default();
        self.all_tags = self.db.get_all_tags().unwrap_or_default();

        // Update available extensions list
        self.update_extensions();
    }

    fn update_extensions(&mut self) {
        let all_files = self.db.get_all_media().unwrap_or_default();
        let mut exts: Vec<String> = all_files
            .iter()
            .filter_map(|f| {
                f.filename.rsplit('.').next().map(|e| e.to_lowercase())
            })
            .collect();
        exts.sort();
        exts.dedup();
        self.sidebar_state.available_extensions = exts;
    }

    fn apply_sort(&mut self) {
        use crate::ui::sidebar::SortMode;
        match self.sidebar_state.sort_mode {
            SortMode::Name => self.media_files.sort_by(|a, b| a.filename.to_lowercase().cmp(&b.filename.to_lowercase())),
            SortMode::NameDesc => self.media_files.sort_by(|a, b| b.filename.to_lowercase().cmp(&a.filename.to_lowercase())),
            SortMode::Size => self.media_files.sort_by_key(|f| f.file_size.unwrap_or(0)),
            SortMode::SizeDesc => self.media_files.sort_by_key(|f| std::cmp::Reverse(f.file_size.unwrap_or(0))),
            SortMode::Type => self.media_files.sort_by(|a, b| a.media_type.as_str().cmp(b.media_type.as_str())),
            SortMode::DateAdded => self.media_files.sort_by(|a, b| a.created_at.cmp(&b.created_at)),
            SortMode::DateAddedDesc => self.media_files.sort_by(|a, b| b.created_at.cmp(&a.created_at)),
        }
    }

    fn poll_thumbnails(&mut self) {
        while let Some(result) = self.thumb_worker.try_recv() {
            if let Some(thumb_filename) = result.thumb_filename {
                self.db
                    .set_thumbnail(result.media_id, &thumb_filename, false)
                    .ok();
                if let Some(file) = self.media_files.iter_mut().find(|f| f.id == result.media_id) {
                    file.thumbnail_path = Some(thumb_filename);
                }
            }
        }
    }

    fn open_media(&mut self, media_id: i64) {
        if let Some(media) = self.media_files.iter().find(|f| f.id == media_id).cloned() {
            self.viewer_state = ViewerState::default();
            self.viewer_state.current_media = Some(media);
            self.view = AppView::Viewer;
        }
    }

    fn open_tag_editor(&mut self, media_id: i64) {
        let media_tags = self.db.get_tags_for_media(media_id).unwrap_or_default();
        self.tag_editor_state = TagEditorState {
            is_open: true,
            media_id: Some(media_id),
            media_tags,
            new_tag_input: String::new(),
        };
    }
}

impl eframe::App for MediaManagerApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Deferred texture cleanup (must happen before rendering)
        self.viewer_state.process_deferred();

        // Poll background tasks
        self.poll_scan();
        self.poll_thumbnails();

        // Tag editor window (floating)
        let tag_action = tag_editor::show_tag_editor(
            ctx,
            &mut self.tag_editor_state,
            &self.all_tags,
        );
        match tag_action {
            TagEditorAction::AddTag(media_id, tag_name) => {
                let tag_id = self.db.create_tag(&tag_name).unwrap_or(0);
                if tag_id > 0 {
                    self.db.add_tag_to_media(media_id, tag_id).ok();
                    // Refresh tag editor state
                    self.tag_editor_state.media_tags =
                        self.db.get_tags_for_media(media_id).unwrap_or_default();
                    self.refresh_media_list();
                }
            }
            TagEditorAction::RemoveTag(media_id, tag_id) => {
                self.db.remove_tag_from_media(media_id, tag_id).ok();
                self.tag_editor_state.media_tags =
                    self.db.get_tags_for_media(media_id).unwrap_or_default();
                self.refresh_media_list();
            }
            TagEditorAction::Close => {
                self.tag_editor_state.is_open = false;
            }
            TagEditorAction::None => {}
        }

        let video_fullscreen = self.view == AppView::Viewer && self.viewer_state.fullscreen;

        // Left sidebar (hidden in video fullscreen)
        if !video_fullscreen {
            egui::SidePanel::left("sidebar")
                .default_width(200.0)
                .show(ctx, |ui| {
                    let sidebar_action =
                        sidebar::show_sidebar(ui, &mut self.sidebar_state, &self.tags_with_counts);

                    match sidebar_action {
                        SidebarAction::Rescan => {
                            if !self.scanning {
                                self.start_scan();
                            }
                        }
                        SidebarAction::CreateTag(name) => {
                            self.db.create_tag(&name).ok();
                            self.refresh_media_list();
                        }
                        SidebarAction::DeleteTag(tag_id) => {
                            self.db.delete_tag(tag_id).ok();
                            self.sidebar_state.included_tags.retain(|&id| id != tag_id);
                            self.sidebar_state.excluded_tags.retain(|&id| id != tag_id);
                            self.refresh_media_list();
                        }
                        SidebarAction::FilterChanged => {
                            self.refresh_media_list();
                            // Clear gallery textures to avoid stale ones
                            self.gallery_state.textures.clear();
                        }
                        SidebarAction::SortChanged => {
                            self.apply_sort();
                        }
                        SidebarAction::None => {}
                    }
                });
        }

        // Bottom status bar (hidden in video fullscreen)
        if !video_fullscreen {
            egui::TopBottomPanel::bottom("status_bar").show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(&self.status);
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(format!("{} files", self.media_files.len()));
                        ui.separator();
                        let mut cols = self.gallery_state.columns;
                        let slider = ui.add_sized(
                            [140.0, 18.0],
                            egui::Slider::new(&mut cols, 3..=10).show_value(false),
                        );
                        ui.label(format!("Columns: {}", cols));
                        if slider.changed() {
                            self.gallery_state.columns = cols;
                            if self.config.gallery_columns != cols {
                                self.config.gallery_columns = cols;
                                config::save_config(&self.config);
                            }
                        }
                    });
                });
            });
        }

        // Central panel
        egui::CentralPanel::default().show(ctx, |ui| match &self.view {
            AppView::Gallery => {
                let gallery_action = gallery::show_gallery(
                    ui,
                    ctx,
                    &mut self.gallery_state,
                    &self.media_files,
                );

                match gallery_action {
                    GalleryAction::OpenMedia(id) => {
                        self.open_media(id);
                    }
                    GalleryAction::EditTags(id) => {
                        self.open_tag_editor(id);
                    }
                    GalleryAction::None => {}
                }
            }
            AppView::Viewer => {
                let media_root = config::media_dir(&self.config);
                let viewer_action = viewer::show_viewer(
                    ui,
                    ctx,
                    &mut self.viewer_state,
                    &mut self.player,
                    &media_root,
                );

                match viewer_action {
                    ViewerAction::Close => {
                        self.player.stop();
                        self.viewer_state.invalidate_video_texture();
                        self.viewer_state.fullscreen = false;
                        self.view = AppView::Gallery;
                    }
                    ViewerAction::SetAsThumbnail => {
                        if let Some(media) = &self.viewer_state.current_media {
                            let media_id = media.id;
                            let thumbs_dir = config::thumbnails_dir();
                            let thumb_filename = format!("{}.png", media_id);
                            let thumb_path = thumbs_dir.join(&thumb_filename);

                            if self.player.screenshot_to_file(&thumb_path) {
                                self.db.set_thumbnail(media_id, &thumb_filename, true).ok();
                                if let Some(f) = self.media_files.iter_mut().find(|f| f.id == media_id) {
                                    f.thumbnail_path = Some(thumb_filename);
                                    f.custom_thumbnail = true;
                                }
                                self.gallery_state.textures.remove(&media_id);
                                self.status = "Thumbnail set from current frame".to_string();
                            } else {
                                self.status = "Failed to capture screenshot".to_string();
                            }
                        }
                    }
                    ViewerAction::EditTags(id) => {
                        self.open_tag_editor(id);
                    }
                    ViewerAction::None => {}
                }
            }
        });

        // Request repaint while background work is in progress
        if self.scanning || self.thumb_worker.busy() {
            ctx.request_repaint();
        }
    }
}
