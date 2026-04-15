use egui;

use crate::db::Tag;

pub struct TagEditorState {
    pub is_open: bool,
    pub media_id: Option<i64>,
    pub media_tags: Vec<Tag>,
    pub new_tag_input: String,
}

impl Default for TagEditorState {
    fn default() -> Self {
        Self {
            is_open: false,
            media_id: None,
            media_tags: Vec::new(),
            new_tag_input: String::new(),
        }
    }
}

pub enum TagEditorAction {
    None,
    AddTag(i64, String),
    RemoveTag(i64, i64),
    Close,
}

pub fn show_tag_editor(
    ctx: &egui::Context,
    state: &mut TagEditorState,
    all_tags: &[Tag],
) -> TagEditorAction {
    let mut action = TagEditorAction::None;

    if !state.is_open {
        return action;
    }

    let media_id = match state.media_id {
        Some(id) => id,
        None => return TagEditorAction::Close,
    };

    egui::Window::new("Tags")
        .collapsible(false)
        .resizable(true)
        .default_width(300.0)
        .show(ctx, |ui| {
            ui.heading("File tags");
            ui.separator();

            // Current tags on this media
            ui.label("Current tags:");
            let mut tag_to_remove = None;
            for tag in &state.media_tags {
                ui.horizontal(|ui| {
                    ui.label(&tag.name);
                    if ui.small_button("x").clicked() {
                        tag_to_remove = Some(tag.id);
                    }
                });
            }
            if let Some(tag_id) = tag_to_remove {
                action = TagEditorAction::RemoveTag(media_id, tag_id);
            }

            ui.separator();

            // Add existing tag
            ui.label("Add tag:");
            let media_tag_ids: Vec<i64> = state.media_tags.iter().map(|t| t.id).collect();
            let available_tags: Vec<&Tag> = all_tags
                .iter()
                .filter(|t| !media_tag_ids.contains(&t.id))
                .collect();

            for tag in &available_tags {
                if ui.button(&tag.name).clicked() {
                    action = TagEditorAction::AddTag(media_id, tag.name.clone());
                }
            }

            ui.separator();

            // Create new tag and add
            ui.horizontal(|ui| {
                ui.text_edit_singleline(&mut state.new_tag_input);
                if ui.button("Create & add").clicked()
                    && !state.new_tag_input.trim().is_empty()
                {
                    let name = state.new_tag_input.trim().to_string();
                    state.new_tag_input.clear();
                    action = TagEditorAction::AddTag(media_id, name);
                }
            });

            ui.separator();

            if ui.button("Close").clicked() {
                action = TagEditorAction::Close;
            }
        });

    action
}
