use egui;

use crate::db::{MediaFile, MediaType};
use crate::player::Player;

pub struct ViewerState {
    pub current_media: Option<MediaFile>,
    pub image_texture: Option<egui::TextureHandle>,
    pub video_texture: Option<egui::TextureHandle>,
    pub zoom: f32,
    pub fullscreen: bool,
    pub last_mouse_move: f64,
    pub last_mouse_pos: egui::Pos2,
    /// Deferred texture cleanup — drop on next frame, not during render
    drop_video_texture: bool,
}

impl Default for ViewerState {
    fn default() -> Self {
        Self {
            current_media: None,
            image_texture: None,
            video_texture: None,
            zoom: 1.0,
            fullscreen: false,
            last_mouse_move: 0.0,
            last_mouse_pos: egui::Pos2::ZERO,
            drop_video_texture: false,
        }
    }
}

impl ViewerState {
    /// Schedule texture drop for next frame (safe with wgpu)
    pub fn invalidate_video_texture(&mut self) {
        self.drop_video_texture = true;
    }

    /// Call at the start of each frame to perform deferred cleanup
    pub fn process_deferred(&mut self) {
        if self.drop_video_texture {
            self.video_texture = None;
            self.drop_video_texture = false;
        }
    }
}

const CONTROLS_HIDE_DELAY: f64 = 2.0;

pub enum ViewerAction {
    None,
    Close,
    SetAsThumbnail,
    EditTags(i64),
}

pub fn show_viewer(
    ui: &mut egui::Ui,
    ctx: &egui::Context,
    state: &mut ViewerState,
    player: &mut Player,
    media_root: &std::path::Path,
) -> ViewerAction {
    let mut action = ViewerAction::None;

    let media = match &state.current_media {
        Some(m) => m.clone(),
        None => return ViewerAction::Close,
    };

    // Apply system fullscreen
    ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(state.fullscreen));

    // Escape exits fullscreen
    if state.fullscreen && ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
        state.fullscreen = false;
    }

    if !state.fullscreen {
        // Top bar (hidden in fullscreen)
        ui.horizontal(|ui| {
            if ui.button("< Back").clicked() {
                action = ViewerAction::Close;
                player.stop();
            }

            ui.label(&media.filename);

            if ui.button("Tags").clicked() {
                action = ViewerAction::EditTags(media.id);
            }

            if media.media_type == MediaType::Video {
                if ui.button("Set as thumbnail").clicked() {
                    action = ViewerAction::SetAsThumbnail;
                }
            }
        });

        ui.separator();
    }

    match media.media_type {
        MediaType::Video => {
            show_video_player(ui, ctx, state, player, &media, media_root);
        }
        MediaType::Image => {
            show_image_viewer(ui, ctx, state, &media, media_root);
        }
    }

    action
}

fn show_video_player(
    ui: &mut egui::Ui,
    ctx: &egui::Context,
    state: &mut ViewerState,
    player: &mut Player,
    media: &MediaFile,
    media_root: &std::path::Path,
) {
    let full_path = media_root.join(&media.path);
    let full_path_str = full_path.to_string_lossy().to_string();

    // Initialize player if needed
    if player.current_file.as_deref() != Some(full_path_str.as_str()) {
        player.load(&full_path_str);
        state.invalidate_video_texture();
    }

    // Poll events and render frame
    player.poll_events();

    // Track mouse movement for auto-hide controls
    let now = ctx.input(|i| i.time);
    let mouse_pos = ctx.input(|i| i.pointer.hover_pos().unwrap_or(state.last_mouse_pos));
    if mouse_pos.distance(state.last_mouse_pos) > 2.0 {
        state.last_mouse_move = now;
        state.last_mouse_pos = mouse_pos;
    }
    let show_controls = !state.fullscreen || (now - state.last_mouse_move) < CONTROLS_HIDE_DELAY;

    // In fullscreen: video takes all space; in normal: leave room for controls
    let available = ui.available_size();
    let controls_height = if show_controls && !state.fullscreen { 80.0 } else { 0.0 };
    let video_height = (available.y - controls_height).max(200.0);
    let video_width = available.x;

    // --- Video area ---
    let video_rect;

    if player.available && player.current_file.is_some() {
        player.render_frame(video_width as u32, video_height as u32);

        // Update texture from frame buffer
        let mut frame = player.frame.lock().unwrap();
        if frame.dirty && frame.width > 0 && frame.height > 0 && !frame.pixels.is_empty() {
            let mut rgba = Vec::with_capacity(frame.pixels.len());
            for chunk in frame.pixels.chunks_exact(4) {
                rgba.push(chunk[0]);
                rgba.push(chunk[1]);
                rgba.push(chunk[2]);
                rgba.push(255);
            }
            let color_image = egui::ColorImage::from_rgba_unmultiplied(
                [frame.width as usize, frame.height as usize],
                &rgba,
            );
            match &mut state.video_texture {
                Some(tex) => tex.set(color_image, egui::TextureOptions::LINEAR),
                None => {
                    state.video_texture = Some(ctx.load_texture(
                        "video_frame", color_image, egui::TextureOptions::LINEAR,
                    ));
                }
            }
            frame.dirty = false;
        }
        drop(frame);

        let (rect, response) = ui.allocate_exact_size(
            egui::vec2(video_width, video_height),
            egui::Sense::click(),
        );
        video_rect = rect;

        if let Some(texture) = &state.video_texture {
            ui.painter().image(
                texture.id(), rect,
                egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                egui::Color32::WHITE,
            );
        } else {
            ui.painter().rect_filled(rect, 0.0, egui::Color32::BLACK);
            ui.painter().text(
                rect.center(), egui::Align2::CENTER_CENTER,
                "Loading...", egui::FontId::proportional(18.0), egui::Color32::WHITE,
            );
        }

        if response.double_clicked() {
            state.fullscreen = !state.fullscreen;
            state.last_mouse_move = now;
        } else if response.clicked() {
            player.toggle_pause();
            state.last_mouse_move = now;
        }

        ctx.request_repaint();
    } else {
        let (rect, _) = ui.allocate_exact_size(
            egui::vec2(video_width, video_height), egui::Sense::hover(),
        );
        video_rect = rect;
        ui.painter().rect_filled(rect, 0.0, egui::Color32::from_gray(30));
        let msg = match &player.error_msg {
            Some(e) => format!("libmpv not available:\n{}\n\nPlace libmpv.so or mpv-2.dll\nin appdata/libs/", e),
            None => "libmpv not loaded".to_string(),
        };
        ui.painter().text(
            rect.center(), egui::Align2::CENTER_CENTER, msg,
            egui::FontId::proportional(14.0), egui::Color32::from_gray(180),
        );
    }

    // --- Controls ---
    if state.fullscreen {
        // Overlay controls drawn on top of video
        if show_controls {
            draw_overlay_controls(ui, ctx, state, player, video_rect);
        }
    } else {
        // Normal inline controls below video
        ui.separator();
        draw_inline_controls(ui, state, player);
    }
}

/// Controls drawn as overlay at the bottom of the video rect (fullscreen mode)
fn draw_overlay_controls(
    _ui: &mut egui::Ui,
    ctx: &egui::Context,
    state: &mut ViewerState,
    player: &mut Player,
    video_rect: egui::Rect,
) {
    let panel_height = 90.0;
    let panel_rect = egui::Rect::from_min_max(
        egui::pos2(video_rect.min.x, video_rect.max.y - panel_height),
        video_rect.max,
    );

    let overlay_layer = egui::LayerId::new(egui::Order::Foreground, egui::Id::new("video_overlay"));
    let painter = ctx.layer_painter(overlay_layer);

    // Semi-transparent background
    painter.rect_filled(panel_rect, 0.0, egui::Color32::from_black_alpha(180));

    // Build overlay UI
    let mut overlay_ui = egui::Ui::new(
        ctx.clone(),
        overlay_layer,
        egui::Id::new("overlay_controls"),
        egui::UiBuilder::new().max_rect(panel_rect),
    );

    overlay_ui.set_clip_rect(panel_rect);
    let margin = 8.0;
    overlay_ui.allocate_space(egui::vec2(0.0, margin));

    // Seek bar
    if player.duration > 0.0 {
        let bar_width = panel_rect.width() - margin * 2.0;
        overlay_ui.horizontal(|ui| {
            ui.add_space(margin);
            let (rect, response) = ui.allocate_exact_size(
                egui::vec2(bar_width, 12.0), egui::Sense::click_and_drag(),
            );
            let progress = (player.position / player.duration).clamp(0.0, 1.0) as f32;
            ui.painter().rect_filled(rect, 4.0, egui::Color32::from_gray(80));
            let filled = egui::Rect::from_min_size(rect.min, egui::vec2(rect.width() * progress, rect.height()));
            ui.painter().rect_filled(filled, 4.0, egui::Color32::from_rgb(80, 140, 220));
            if response.clicked() || response.dragged() {
                if let Some(pos) = response.interact_pointer_pos() {
                    let ratio = ((pos.x - rect.min.x) / rect.width()).clamp(0.0, 1.0);
                    player.seek((ratio as f64) * player.duration);
                }
            }
        });
    }

    overlay_ui.add_space(4.0);

    // Buttons row
    overlay_ui.horizontal(|ui| {
        ui.add_space(margin);

        let play_text = if player.is_playing { "Pause" } else { "Play" };
        if ui.button(play_text).clicked() {
            player.toggle_pause();
        }
        if ui.button("-10s").clicked() {
            player.seek((player.position - 10.0).max(0.0));
        }
        if ui.button("+10s").clicked() {
            player.seek((player.position + 10.0).min(player.duration));
        }
        if ui.button("Stop").clicked() {
            player.stop();
            state.fullscreen = false;
        }

        ui.separator();

        ui.label(format_time(player.position));
        ui.label("/");
        ui.label(format_time(player.duration));

        ui.separator();

        ui.label("Vol:");
        let mut vol = player.volume as f32;
        if ui.add(egui::Slider::new(&mut vol, 0.0..=100.0).show_value(false)).changed() {
            player.set_volume(vol as f64);
        }

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.add_space(margin);
            if ui.button("Exit fullscreen").clicked() {
                state.fullscreen = false;
            }
        });
    });
}

/// Normal inline controls below video (non-fullscreen mode)
fn draw_inline_controls(ui: &mut egui::Ui, state: &mut ViewerState, player: &mut Player) {
    // Seek bar
    if player.duration > 0.0 {
        let bar_width = ui.available_width();
        let (rect, response) = ui.allocate_exact_size(
            egui::vec2(bar_width, 16.0), egui::Sense::click_and_drag(),
        );
        let progress = (player.position / player.duration).clamp(0.0, 1.0) as f32;
        ui.painter().rect_filled(rect, 4.0, egui::Color32::from_gray(50));
        let filled = egui::Rect::from_min_size(rect.min, egui::vec2(rect.width() * progress, rect.height()));
        ui.painter().rect_filled(filled, 4.0, egui::Color32::from_rgb(80, 140, 220));
        if response.hovered() {
            ui.painter().rect_stroke(rect, 4.0, egui::Stroke::new(1.0, egui::Color32::from_gray(150)));
        }
        if response.clicked() || response.dragged() {
            if let Some(pos) = response.interact_pointer_pos() {
                let ratio = ((pos.x - rect.min.x) / rect.width()).clamp(0.0, 1.0);
                player.seek((ratio as f64) * player.duration);
            }
        }
        ui.horizontal(|ui| {
            ui.label(format_time(player.position));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(format_time(player.duration));
            });
        });
    }

    ui.horizontal(|ui| {
        let play_text = if player.is_playing { "Pause" } else { "Play" };
        if ui.button(play_text).clicked() {
            player.toggle_pause();
        }
        if ui.button("-10s").clicked() {
            player.seek((player.position - 10.0).max(0.0));
        }
        if ui.button("+10s").clicked() {
            player.seek((player.position + 10.0).min(player.duration));
        }
        if ui.button("Stop").clicked() {
            player.stop();
            state.invalidate_video_texture();
            if state.fullscreen {
                state.fullscreen = false;
            }
        }

        ui.separator();

        ui.label("Volume:");
        let mut vol = player.volume as f32;
        if ui.add(egui::Slider::new(&mut vol, 0.0..=100.0).suffix("%")).changed() {
            player.set_volume(vol as f64);
        }

        ui.separator();

        let fs_text = if state.fullscreen { "Exit fullscreen" } else { "Fullscreen" };
        if ui.button(fs_text).clicked() {
            state.fullscreen = !state.fullscreen;
        }
    });
}

fn show_image_viewer(
    ui: &mut egui::Ui,
    ctx: &egui::Context,
    state: &mut ViewerState,
    media: &MediaFile,
    media_root: &std::path::Path,
) {
    // Load image texture if not loaded
    if state.image_texture.is_none() {
        let full_path = media_root.join(&media.path);
        if let Ok(img) = image::open(&full_path) {
            let rgba = img.to_rgba8();
            let size = [rgba.width() as usize, rgba.height() as usize];
            let pixels = rgba.into_raw();
            let color_image = egui::ColorImage::from_rgba_unmultiplied(size, &pixels);
            state.image_texture = Some(ctx.load_texture(
                "viewer_image",
                color_image,
                egui::TextureOptions::LINEAR,
            ));
        }
    }

    // Zoom controls
    ui.horizontal(|ui| {
        if ui.button("-").clicked() {
            state.zoom = (state.zoom - 0.1).max(0.1);
        }
        ui.label(format!("{:.0}%", state.zoom * 100.0));
        if ui.button("+").clicked() {
            state.zoom = (state.zoom + 0.1).min(5.0);
        }
        if ui.button("100%").clicked() {
            state.zoom = 1.0;
        }
        if ui.button("Fit").clicked() {
            state.zoom = 0.0; // special value: fit
        }
    });

    ui.separator();

    // Display image
    if let Some(texture) = &state.image_texture {
        let available = ui.available_size();
        let img_size = texture.size_vec2();

        let display_size = if state.zoom == 0.0 {
            // Fit to available width
            let scale = (available.x / img_size.x).min(1.0);
            img_size * scale
        } else {
            img_size * state.zoom
        };

        egui::ScrollArea::both().show(ui, |ui| {
            ui.image(egui::load::SizedTexture::new(texture.id(), display_size));
        });
    }
}

fn format_time(seconds: f64) -> String {
    let total = seconds as u64;
    let h = total / 3600;
    let m = (total % 3600) / 60;
    let s = total % 60;
    if h > 0 {
        format!("{}:{:02}:{:02}", h, m, s)
    } else {
        format!("{}:{:02}", m, s)
    }
}
