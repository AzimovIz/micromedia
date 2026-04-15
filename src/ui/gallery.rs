use std::collections::HashMap;

use egui;

use crate::config;
use crate::db::MediaFile;

pub struct GalleryState {
    pub textures: HashMap<i64, egui::TextureHandle>,
    pub columns: usize,
    pub selected_media: Option<i64>,
}

impl GalleryState {
    pub fn new(columns: usize) -> Self {
        Self {
            textures: HashMap::new(),
            columns,
            selected_media: None,
        }
    }

    pub fn load_thumbnail(&mut self, ctx: &egui::Context, media: &MediaFile) {
        if self.textures.contains_key(&media.id) {
            return;
        }

        let thumb_path = match &media.thumbnail_path {
            Some(p) => config::thumbnails_dir().join(p),
            None => return,
        };

        if !thumb_path.exists() {
            return;
        }

        if let Ok(image_data) = image::open(&thumb_path) {
            let rgba = image_data.to_rgba8();
            let size = [rgba.width() as usize, rgba.height() as usize];
            let pixels = rgba.into_raw();
            let color_image = egui::ColorImage::from_rgba_unmultiplied(size, &pixels);
            let texture = ctx.load_texture(
                format!("thumb_{}", media.id),
                color_image,
                egui::TextureOptions::LINEAR,
            );
            self.textures.insert(media.id, texture);
        }
    }
}

pub enum GalleryAction {
    None,
    OpenMedia(i64),
    EditTags(i64),
}

pub fn show_gallery(
    ui: &mut egui::Ui,
    ctx: &egui::Context,
    state: &mut GalleryState,
    media_files: &[MediaFile],
) -> GalleryAction {
    let mut action = GalleryAction::None;

    // Load thumbnails for visible items
    for file in media_files {
        state.load_thumbnail(ctx, file);
    }

    let spacing = 6.0;
    let available_width = ui.available_width();
    let columns = state.columns.max(1);
    let cell_width = ((available_width - spacing * (columns as f32 - 1.0)) / columns as f32).floor().max(80.0);
    let thumb_height = (cell_width * 0.75).floor();
    let cell_height = thumb_height + 28.0;

    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.set_min_width(available_width);

            for row_start in (0..media_files.len()).step_by(columns) {
                let row_end = (row_start + columns).min(media_files.len());
                let row_files = &media_files[row_start..row_end];

                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = spacing;

                    for file in row_files {
                        let is_selected = state.selected_media == Some(file.id);

                        let (outer_rect, response) = ui.allocate_exact_size(
                            egui::vec2(cell_width, cell_height),
                            egui::Sense::click(),
                        );

                        // Background
                        let bg = if is_selected {
                            ui.visuals().selection.bg_fill
                        } else if response.hovered() {
                            ui.visuals().widgets.hovered.bg_fill
                        } else {
                            ui.visuals().window_fill
                        };
                        ui.painter().rect_filled(outer_rect, 4.0, bg);

                        let padding = 4.0;
                        let inner_width = cell_width - padding * 2.0;
                        let inner_top = outer_rect.min.y + padding;

                        // Thumbnail area
                        let thumb_rect = egui::Rect::from_min_size(
                            egui::pos2(outer_rect.min.x + padding, inner_top),
                            egui::vec2(inner_width, thumb_height),
                        );

                        if let Some(texture) = state.textures.get(&file.id) {
                            let img_size = scale_to_fit(texture.size_vec2(), thumb_rect.size());
                            let img_pos = egui::pos2(
                                thumb_rect.center().x - img_size.x * 0.5,
                                thumb_rect.center().y - img_size.y * 0.5,
                            );
                            let img_rect = egui::Rect::from_min_size(img_pos, img_size);
                            ui.painter().image(
                                texture.id(),
                                img_rect,
                                egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                                egui::Color32::WHITE,
                            );
                        } else {
                            ui.painter().rect_filled(thumb_rect, 4.0, egui::Color32::from_gray(50));
                            ui.painter().text(
                                thumb_rect.center(),
                                egui::Align2::CENTER_CENTER,
                                match file.media_type {
                                    crate::db::MediaType::Video => "Video",
                                    crate::db::MediaType::Image => "Image",
                                },
                                egui::FontId::proportional(12.0),
                                egui::Color32::from_gray(120),
                            );
                        }

                        // Filename label (clipped to cell width)
                        let label_top = inner_top + thumb_height + 2.0;
                        let label_rect = egui::Rect::from_min_size(
                            egui::pos2(outer_rect.min.x + padding, label_top),
                            egui::vec2(inner_width, 20.0),
                        );
                        let galley = ui.painter().layout(
                            file.filename.clone(),
                            egui::FontId::proportional(11.0),
                            ui.visuals().text_color(),
                            inner_width,
                        );
                        ui.painter().with_clip_rect(label_rect).galley(
                            label_rect.min,
                            galley,
                            ui.visuals().text_color(),
                        );

                        // Handle clicks
                        if response.clicked() {
                            action = GalleryAction::OpenMedia(file.id);
                        }
                        if response.secondary_clicked() {
                            state.selected_media = Some(file.id);
                            action = GalleryAction::EditTags(file.id);
                        }
                    }
                });

                ui.add_space(spacing);
            }
        });

    action
}

fn scale_to_fit(original: egui::Vec2, max: egui::Vec2) -> egui::Vec2 {
    if original.x <= 0.0 || original.y <= 0.0 {
        return max;
    }
    let scale_x = max.x / original.x;
    let scale_y = max.y / original.y;
    let scale = scale_x.min(scale_y).min(1.0);
    original * scale
}
