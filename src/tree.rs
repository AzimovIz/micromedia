//! Построение дерева файлов из плоских `rel_path` для вида «Список».
//!
//! Папки — виртуальные узлы (в БД не хранятся, выводятся из путей). Выделение
//! папки каскадит на всех потомков; sel_state показывает полное/частичное.

use std::collections::{BTreeSet, HashMap, HashSet};

use crate::db::GalleryFile;

pub struct TreeNode {
    pub label: String,
    pub depth: i32,
    pub is_folder: bool,
    pub is_video: bool,
    pub expanded: bool,
    /// 0 — не выбрано, 1 — выбрано полностью, 2 — частично (папка).
    pub sel_state: i32,
    /// id файла (для файлов) или -1 (для папок).
    pub file_id: i64,
    /// Путь папки (для папок) или rel_path файла.
    pub path: String,
    /// Для папки — id всех файлов-потомков (для каскадного выделения).
    pub descendants: Vec<i64>,
}

#[derive(Default)]
struct Folder {
    subfolders: BTreeSet<String>,
    files: Vec<usize>, // индексы в исходном срезе files
}

/// Плоский список видимых строк дерева (с учётом раскрытых папок).
pub fn build(
    files: &[GalleryFile],
    expanded: &HashSet<String>,
    selected: &HashSet<i64>,
) -> Vec<TreeNode> {
    let mut map: HashMap<String, Folder> = HashMap::new();
    map.entry(String::new()).or_default();

    for (i, f) in files.iter().enumerate() {
        let parts: Vec<&str> = f.rel_path.split('/').collect();
        let folders = &parts[..parts.len().saturating_sub(1)];
        let mut cur = String::new();
        for comp in folders {
            let child = if cur.is_empty() {
                comp.to_string()
            } else {
                format!("{cur}/{comp}")
            };
            map.entry(cur.clone()).or_default().subfolders.insert(child.clone());
            map.entry(child.clone()).or_default();
            cur = child;
        }
        map.entry(cur).or_default().files.push(i);
    }

    let mut out = Vec::new();
    walk(&map, "", 0, files, expanded, selected, &mut out);
    out
}

/// Состояние выделения узла: 0 нет, 1 полностью, 2 частично.
pub fn sel_state(node: &TreeNode, selected: &HashSet<i64>) -> i32 {
    if node.is_folder {
        if node.descendants.is_empty() {
            return 0;
        }
        let c = node
            .descendants
            .iter()
            .filter(|id| selected.contains(id))
            .count();
        if c == 0 {
            0
        } else if c == node.descendants.len() {
            1
        } else {
            2
        }
    } else if selected.contains(&node.file_id) {
        1
    } else {
        0
    }
}

fn walk(
    map: &HashMap<String, Folder>,
    path: &str,
    depth: i32,
    files: &[GalleryFile],
    expanded: &HashSet<String>,
    selected: &HashSet<i64>,
    out: &mut Vec<TreeNode>,
) {
    let folder = match map.get(path) {
        Some(f) => f,
        None => return,
    };

    for sub in &folder.subfolders {
        let desc = descendants(map, sub, files);
        let sel = desc.iter().filter(|id| selected.contains(id)).count();
        let state = if desc.is_empty() || sel == 0 {
            0
        } else if sel == desc.len() {
            1
        } else {
            2
        };
        let exp = expanded.contains(sub);
        out.push(TreeNode {
            label: last_component(sub).to_string(),
            depth,
            is_folder: true,
            is_video: false,
            expanded: exp,
            sel_state: state,
            file_id: -1,
            path: sub.clone(),
            descendants: desc,
        });
        if exp {
            walk(map, sub, depth + 1, files, expanded, selected, out);
        }
    }

    for &fi in &folder.files {
        let f = &files[fi];
        out.push(TreeNode {
            label: f.name.clone(),
            depth,
            is_folder: false,
            is_video: f.is_video,
            expanded: false,
            sel_state: if selected.contains(&f.id) { 1 } else { 0 },
            file_id: f.id,
            path: f.rel_path.clone(),
            descendants: Vec::new(),
        });
    }
}

fn descendants(map: &HashMap<String, Folder>, path: &str, files: &[GalleryFile]) -> Vec<i64> {
    let mut out = Vec::new();
    if let Some(folder) = map.get(path) {
        for &fi in &folder.files {
            out.push(files[fi].id);
        }
        for sub in &folder.subfolders {
            out.extend(descendants(map, sub, files));
        }
    }
    out
}

fn last_component(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}
