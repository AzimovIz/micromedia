use egui;

use crate::db::Tag;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SortMode {
    Name,
    NameDesc,
    Size,
    SizeDesc,
    Type,
    DateAdded,
    DateAddedDesc,
}

impl SortMode {
    pub fn label(&self) -> &str {
        match self {
            SortMode::Name => "Name A-Z",
            SortMode::NameDesc => "Name Z-A",
            SortMode::Size => "Size (small)",
            SortMode::SizeDesc => "Size (large)",
            SortMode::Type => "Type",
            SortMode::DateAdded => "Date added (old)",
            SortMode::DateAddedDesc => "Date added (new)",
        }
    }
}

pub struct SidebarState {
    pub included_tags: Vec<i64>,
    pub excluded_tags: Vec<i64>,
    pub match_all: bool,
    pub new_tag_name: String,
    pub show_create_tag: bool,
    pub sort_mode: SortMode,
    pub search_query: String,
    pub extension_filter: String,
    pub available_extensions: Vec<String>,
}

impl Default for SidebarState {
    fn default() -> Self {
        Self {
            included_tags: Vec::new(),
            excluded_tags: Vec::new(),
            match_all: true,
            new_tag_name: String::new(),
            show_create_tag: false,
            sort_mode: SortMode::Name,
            search_query: String::new(),
            extension_filter: String::new(),
            available_extensions: Vec::new(),
        }
    }
}

pub enum SidebarAction {
    None,
    CreateTag(String),
    DeleteTag(i64),
    FilterChanged,
    SortChanged,
    Rescan,
}

pub fn show_sidebar(
    ui: &mut egui::Ui,
    state: &mut SidebarState,
    tags: &[(Tag, usize)],
) -> SidebarAction {
    let mut action = SidebarAction::None;

    ui.heading("MicroMedia");
    ui.separator();

    if ui.button("Scan").clicked() {
        action = SidebarAction::Rescan;
    }

    ui.separator();

    // Search by name
    ui.label("Search:");
    let search_response = ui.text_edit_singleline(&mut state.search_query);
    if search_response.changed() {
        action = SidebarAction::FilterChanged;
    }

    ui.separator();

    // Extension filter
    ui.label("Extension:");
    let ext_label = if state.extension_filter.is_empty() {
        "All".to_string()
    } else {
        state.extension_filter.clone()
    };
    egui::ComboBox::from_id_salt("ext_filter")
        .selected_text(&ext_label)
        .show_ui(ui, |ui| {
            if ui.selectable_label(state.extension_filter.is_empty(), "All").clicked() {
                state.extension_filter.clear();
                action = SidebarAction::FilterChanged;
            }
            for ext in &state.available_extensions {
                if ui.selectable_label(state.extension_filter == *ext, ext).clicked() {
                    state.extension_filter = ext.clone();
                    action = SidebarAction::FilterChanged;
                }
            }
        });

    ui.separator();

    // Sort mode
    ui.label("Sort by:");
    let current_label = state.sort_mode.label().to_string();
    egui::ComboBox::from_id_salt("sort_mode")
        .selected_text(&current_label)
        .show_ui(ui, |ui| {
            for mode in [SortMode::Name, SortMode::NameDesc, SortMode::Size, SortMode::SizeDesc, SortMode::Type, SortMode::DateAdded, SortMode::DateAddedDesc] {
                if ui.selectable_value(&mut state.sort_mode, mode, mode.label()).changed() {
                    action = SidebarAction::SortChanged;
                }
            }
        });

    ui.separator();

    let filter_active = !state.included_tags.is_empty() || !state.excluded_tags.is_empty();
    if filter_active {
        if ui.button("Clear filter").clicked() {
            state.included_tags.clear();
            state.excluded_tags.clear();
            action = SidebarAction::FilterChanged;
        }
        ui.separator();
    }

    let include_color = egui::Color32::from_rgb(120, 200, 120);
    let exclude_color = egui::Color32::from_rgb(220, 120, 120);
    let mut pending_delete: Option<i64> = None;

    egui::ScrollArea::vertical()
        .max_height(ui.available_height() - 60.0)
        .show(ui, |ui| {
            for (tag, count) in tags {
                let included = state.included_tags.contains(&tag.id);
                let excluded = state.excluded_tags.contains(&tag.id);

                ui.horizontal(|ui| {
                    if ui
                        .selectable_label(included, " + ")
                        .on_hover_text("Include (show only items with this tag)")
                        .clicked()
                    {
                        state.excluded_tags.retain(|&id| id != tag.id);
                        if included {
                            state.included_tags.retain(|&id| id != tag.id);
                        } else {
                            state.included_tags.push(tag.id);
                        }
                        action = SidebarAction::FilterChanged;
                    }

                    if ui
                        .selectable_label(excluded, " − ")
                        .on_hover_text("Exclude (hide items with this tag)")
                        .clicked()
                    {
                        state.included_tags.retain(|&id| id != tag.id);
                        if excluded {
                            state.excluded_tags.retain(|&id| id != tag.id);
                        } else {
                            state.excluded_tags.push(tag.id);
                        }
                        action = SidebarAction::FilterChanged;
                    }

                    let text = format!("{} ({})", tag.name, count);
                    let rich = if included {
                        egui::RichText::new(text).color(include_color).strong()
                    } else if excluded {
                        egui::RichText::new(text).color(exclude_color).strikethrough()
                    } else {
                        egui::RichText::new(text)
                    };
                    let name_resp = ui.label(rich);
                    name_resp.context_menu(|ui| {
                        if ui.button("Delete tag").clicked() {
                            pending_delete = Some(tag.id);
                            ui.close_menu();
                        }
                    });

                    if included || excluded {
                        if ui
                            .small_button("×")
                            .on_hover_text("Remove from filter")
                            .clicked()
                        {
                            state.included_tags.retain(|&id| id != tag.id);
                            state.excluded_tags.retain(|&id| id != tag.id);
                            action = SidebarAction::FilterChanged;
                        }
                    }
                });
            }
        });

    if let Some(id) = pending_delete {
        action = SidebarAction::DeleteTag(id);
    }

    ui.separator();

    // Create tag
    if state.show_create_tag {
        ui.horizontal(|ui| {
            ui.text_edit_singleline(&mut state.new_tag_name);
            if ui.button("+").clicked() && !state.new_tag_name.trim().is_empty() {
                let name = state.new_tag_name.trim().to_string();
                state.new_tag_name.clear();
                state.show_create_tag = false;
                action = SidebarAction::CreateTag(name);
            }
            if ui.button("Cancel").clicked() {
                state.new_tag_name.clear();
                state.show_create_tag = false;
            }
        });
    } else if ui.button("Create tag").clicked() {
        state.show_create_tag = true;
    }

    action
}
