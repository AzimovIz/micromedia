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
    pub selected_tags: Vec<i64>,
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
            selected_tags: Vec::new(),
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
    ui.heading("Tags");

    // Match mode toggle
    ui.horizontal(|ui| {
        ui.label("Filter:");
        if ui.selectable_label(state.match_all, "AND").clicked() {
            state.match_all = true;
            action = SidebarAction::FilterChanged;
        }
        if ui.selectable_label(!state.match_all, "OR").clicked() {
            state.match_all = false;
            action = SidebarAction::FilterChanged;
        }
    });

    ui.separator();

    // Tag list
    if !state.selected_tags.is_empty() {
        if ui.button("Clear filter").clicked() {
            state.selected_tags.clear();
            action = SidebarAction::FilterChanged;
        }
        ui.separator();
    }

    egui::ScrollArea::vertical()
        .max_height(ui.available_height() - 60.0)
        .show(ui, |ui| {
            for (tag, count) in tags {
                let selected = state.selected_tags.contains(&tag.id);
                let label = format!("{} ({})", tag.name, count);

                let response = ui.horizontal(|ui| {
                    let toggled = ui.selectable_label(selected, &label).clicked();

                    let delete_clicked = ui
                        .small_button("x")
                        .on_hover_text("Delete tag")
                        .clicked();

                    (toggled, delete_clicked)
                });

                let (toggled, delete_clicked) = response.inner;

                if toggled {
                    if selected {
                        state.selected_tags.retain(|&id| id != tag.id);
                    } else {
                        state.selected_tags.push(tag.id);
                    }
                    action = SidebarAction::FilterChanged;
                }

                if delete_clicked {
                    action = SidebarAction::DeleteTag(tag.id);
                }
            }
        });

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
