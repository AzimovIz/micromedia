// MicroMedia — галерея + оверлей-просмотрщик (фото/видео) + фон (скан/воркеры).

#[allow(dead_code)] // API дозревает вместе со сканером/тегами
mod db;
mod media;
#[allow(dead_code)] // часть mpv-методов пригодится позже
mod mpv;
#[allow(dead_code)]
mod paths;
mod scanner;
mod tree;
mod videothumb;
mod workers;

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::Path;
use std::rc::Rc;
use std::time::Duration;

use slint::{ComponentHandle, GraphicsAPI, Model, ModelRc, RenderingState, VecModel};

use mpv::{Mpv, MpvRenderContext};

slint::include_modules!();

/// Колбэк «у mpv готов новый кадр» — просим Slint перерисоваться.
unsafe extern "C" fn on_mpv_update(ctx: *mut std::ffi::c_void) {
    let weak = &*(ctx as *const slint::Weak<MainWindow>);
    let weak = weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(win) = weak.upgrade() {
            win.window().request_redraw();
        }
    });
}

/// Преобразует список файлов из БД в модель Slint.
fn items_from(list: Vec<db::GalleryFile>, thumbs: &Path) -> Vec<FileItem> {
    list.into_iter()
        .map(|f| FileItem {
            id: f.id as i32,
            name: f.name.into(),
            thumb_path: thumbs
                .join(format!("{}.jpg", f.id))
                .to_string_lossy()
                .into_owned()
                .into(),
            is_video: f.is_video,
        })
        .collect()
}

fn strings_model(v: Vec<String>) -> ModelRc<slint::SharedString> {
    let items: Vec<slint::SharedString> = v.into_iter().map(|s| s.into()).collect();
    ModelRc::new(VecModel::from(items))
}

/// Разбирает строку фильтра "tag1 -tag2" в id include/exclude.
/// impossible=true — если include-токен не соответствует тегу (→ пустой результат).
fn parse_filter(
    text: &str,
    name_to_id: &HashMap<String, i64>,
) -> (Vec<i64>, Vec<i64>, bool) {
    let mut inc = Vec::new();
    let mut exc = Vec::new();
    let mut impossible = false;
    for tok in text.split_whitespace() {
        let (neg, name) = match tok.strip_prefix('-') {
            Some(rest) => (true, rest),
            None => (false, tok),
        };
        if name.is_empty() {
            continue;
        }
        match name_to_id.get(&name.to_lowercase()) {
            Some(&id) => {
                if neg {
                    exc.push(id)
                } else {
                    inc.push(id)
                }
            }
            None if !neg => impossible = true,
            None => {}
        }
    }
    (inc, exc, impossible)
}

/// Текст последнего (редактируемого) токена без ведущего '-'.
fn last_token_query(text: &str) -> String {
    if text.ends_with(char::is_whitespace) {
        return String::new();
    }
    let start = text.rfind(char::is_whitespace).map(|i| i + 1).unwrap_or(0);
    text[start..].trim_start_matches('-').to_string()
}

/// Действие над строкой фильтра для конкретного тега.
enum TokenOp {
    Include,
    Exclude,
    Remove,
}

/// Выставляет тег `name` в строке фильтра в нужное состояние: сначала убирает
/// любое его вхождение (`name`/`-name`), затем добавляет нужное (или ничего).
fn set_filter_token(text: &str, name: &str, op: TokenOp) -> String {
    let name_lc = name.to_lowercase();
    let mut tokens: Vec<String> = text
        .split_whitespace()
        .filter(|tok| {
            let bare = tok.strip_prefix('-').unwrap_or(tok);
            bare.to_lowercase() != name_lc
        })
        .map(|s| s.to_string())
        .collect();
    match op {
        TokenOp::Include => tokens.push(name.to_string()),
        TokenOp::Exclude => tokens.push(format!("-{name}")),
        TokenOp::Remove => {}
    }
    tokens.join(" ")
}

/// Заменяет последний токен строки фильтра на выбранный тег (+ пробел).
fn replace_last_token(text: &str, chosen: &str) -> String {
    let start = if text.ends_with(char::is_whitespace) {
        text.len()
    } else {
        text.rfind(char::is_whitespace).map(|i| i + 1).unwrap_or(0)
    };
    let neg = text[start..].starts_with('-');
    format!("{}{}{} ", &text[..start], if neg { "-" } else { "" }, chosen)
}

/// Существующие теги, чьё имя содержит query (исключая уже назначенные).
fn suggest(
    query: &str,
    names: &[String],
    exclude: &std::collections::HashSet<String>,
) -> Vec<String> {
    if query.is_empty() {
        return Vec::new();
    }
    let q = query.to_lowercase();
    names
        .iter()
        .filter(|n| n.to_lowercase().contains(&q) && !exclude.contains(*n))
        .take(8)
        .cloned()
        .collect()
}

fn basename(rel: &str) -> String {
    rel.rsplit('/').next().unwrap_or(rel).to_string()
}

/// Открывает расположение объекта в системном файловом менеджере.
fn open_location(abs: &Path, is_folder: bool) {
    #[cfg(target_os = "windows")]
    {
        if is_folder {
            let _ = std::process::Command::new("explorer").arg(abs).spawn();
        } else {
            let _ = std::process::Command::new("explorer")
                .arg(format!("/select,{}", abs.display()))
                .spawn();
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        let dir = if is_folder {
            abs.to_path_buf()
        } else {
            abs.parent()
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|| abs.to_path_buf())
        };
        let _ = std::process::Command::new("xdg-open").arg(dir).spawn();
    }
}

/// Пресеты скорости воспроизведения (циклический перебор).
const SPEEDS: [f64; 6] = [0.5, 0.75, 1.0, 1.25, 1.5, 2.0];

/// Форматирует секунды в m:ss.
fn fmt_time(secs: f64) -> String {
    let s = secs.max(0.0) as i64;
    format!("{}:{:02}", s / 60, s % 60)
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("info,zbus=warn,tracing=warn"),
    )
    .init();

    if std::env::var_os("SLINT_BACKEND").is_none() {
        std::env::set_var("SLINT_BACKEND", "winit-femtovg");
    }

    // Фундамент данных.
    let paths = paths::Paths::resolve();
    paths.ensure_dirs()?;
    let ui_db = Rc::new(db::Db::open(&paths.db)?);
    log::info!(
        "Данные: media={}, БД={} ({} файлов в индексе)",
        paths.media.display(),
        paths.db.display(),
        ui_db.file_count()?
    );

    workers::spawn(&paths);

    // Загрузка mpv (для просмотрщика видео).
    let mpv_opt: Option<Rc<Mpv>> = match Mpv::load() {
        Ok(m) => Some(Rc::new(m)),
        Err(e) => {
            log::warn!("mpv недоступен: {e}");
            None
        }
    };

    let window = MainWindow::new()?;

    // Пункты сортировки (порядок = SortKey::from_index).
    window.set_sort_options(strings_model(
        [
            "Имя ↑",
            "Имя ↓",
            "Добавлено ↑",
            "Добавлено ↓",
            "Дата файла ↑",
            "Дата файла ↓",
            "Размер ↑",
            "Размер ↓",
            "Длительность ↑",
            "Длительность ↓",
            "Тип ↑",
            "Тип ↓",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect(),
    ));

    // --- Состояние ---
    let current_open_id: Rc<Cell<i64>> = Rc::new(Cell::new(-1));
    // Мультивыбор (вид «Список»): выбранные id, раскрытые папки, зеркало дерева.
    let selected: Rc<RefCell<std::collections::HashSet<i64>>> =
        Rc::new(RefCell::new(std::collections::HashSet::new()));
    let expanded: Rc<RefCell<std::collections::HashSet<String>>> =
        Rc::new(RefCell::new(std::collections::HashSet::new()));
    let tree_mirror: Rc<RefCell<Vec<tree::TreeNode>>> = Rc::new(RefCell::new(Vec::new()));
    // Постоянная модель дерева: обновляем содержимое на месте, чтобы делегаты
    // (и детект двойного клика) не пересоздавались при изменении выделения.
    let tree_model: Rc<VecModel<TreeRow>> = Rc::new(VecModel::default());
    window.set_tree_rows(tree_model.clone().into());

    // Имена всех тегов + карта имя(lowercase)->id.
    let tag_names = {
        let ui_db = ui_db.clone();
        move || -> Vec<String> {
            ui_db
                .list_tags()
                .unwrap_or_default()
                .into_iter()
                .map(|t| t.name)
                .collect()
        }
    };
    let name_to_id = {
        let ui_db = ui_db.clone();
        move || -> HashMap<String, i64> {
            ui_db
                .list_tags()
                .unwrap_or_default()
                .into_iter()
                .map(|t| (t.name.to_lowercase(), t.id))
                .collect()
        }
    };

    // Пересборка галереи (плитка + дерево) по строке фильтра и сортировке.
    let rebuild_gallery: Rc<dyn Fn()> = {
        let weak = window.as_weak();
        let ui_db = ui_db.clone();
        let thumbs = paths.thumbnails.clone();
        let name_to_id = name_to_id.clone();
        let selected = selected.clone();
        let expanded = expanded.clone();
        let tree_mirror = tree_mirror.clone();
        let tree_model = tree_model.clone();
        Rc::new(move || {
            if let Some(w) = weak.upgrade() {
                let (inc, exc, impossible) = parse_filter(&w.get_filter_text(), &name_to_id());
                let sort = db::SortKey::from_index(w.get_sort_index());
                let name_q = w.get_search_text();
                let type_filter = w.get_type_filter();
                let list = if impossible {
                    Vec::new()
                } else {
                    ui_db
                        .query_files(&inc, &exc, true, &name_q, type_filter, sort)
                        .unwrap_or_else(|e| {
                            log::error!("Галерея: {e}");
                            Vec::new()
                        })
                };
                w.set_file_count(list.len() as i32);

                // Список всех тегов (по алфавиту) для боковой панели.
                let tag_list: Vec<String> = ui_db
                    .list_tags()
                    .unwrap_or_default()
                    .into_iter()
                    .map(|t| t.name)
                    .collect();
                w.set_all_tags(strings_model(tag_list));

                // Дерево (вид «Список»).
                let nodes = tree::build(&list, &expanded.borrow(), &selected.borrow());
                let rows: Vec<TreeRow> = nodes
                    .iter()
                    .map(|n| TreeRow {
                        label: n.label.clone().into(),
                        depth: n.depth,
                        is_folder: n.is_folder,
                        is_video: n.is_video,
                        expanded: n.expanded,
                        sel_state: n.sel_state,
                    })
                    .collect();
                tree_model.set_vec(rows);
                *tree_mirror.borrow_mut() = nodes;
                w.set_selected_count(selected.borrow().len() as i32);

                // Плитка.
                let items = items_from(list, &thumbs);
                w.set_files(ModelRc::new(VecModel::from(items)));
            }
        })
    };

    // Теги открытого файла (назначенные) для чипов просмотрщика.
    let rebuild_viewer_tags: Rc<dyn Fn(i64)> = {
        let weak = window.as_weak();
        let ui_db = ui_db.clone();
        Rc::new(move |id: i64| {
            if let Some(w) = weak.upgrade() {
                let items: Vec<TagItem> = ui_db
                    .tags_for_file(id)
                    .unwrap_or_default()
                    .into_iter()
                    .map(|t| TagItem {
                        id: t.id as i32,
                        name: t.name.into(),
                    })
                    .collect();
                w.set_viewer_tags(ModelRc::new(VecModel::from(items)));
            }
        })
    };

    rebuild_gallery();

    // Ленивая подгрузка миниатюр с кэшем (промахи не кэшируем).
    let thumb_cache: Rc<RefCell<HashMap<String, slint::Image>>> =
        Rc::new(RefCell::new(HashMap::new()));
    {
        let cache = thumb_cache.clone();
        window.on_load_thumb(move |path| {
            let key = path.as_str();
            if key.is_empty() {
                return slint::Image::default();
            }
            if let Some(img) = cache.borrow().get(key) {
                return img.clone();
            }
            match slint::Image::load_from_path(Path::new(key)) {
                Ok(img) => {
                    cache.borrow_mut().insert(key.to_string(), img.clone());
                    img
                }
                Err(_) => slint::Image::default(),
            }
        });
    }

    // Скан media/ в фоне; по завершении дёргаем rescan-done → пересборка UI.
    let start_scan: Rc<dyn Fn()> = {
        let db_path = paths.db.clone();
        let media = paths.media.clone();
        let weak = window.as_weak();
        Rc::new(move || {
            let db_path = db_path.clone();
            let media = media.clone();
            let weak = weak.clone();
            std::thread::spawn(move || {
                match db::Db::open(&db_path) {
                    Ok(sdb) => match scanner::scan(&media, &sdb) {
                        Ok(s) => log::info!(
                            "Скан: добавлено {}, обновлено {}, удалено {} (на месте {})",
                            s.added,
                            s.updated,
                            s.deleted,
                            s.seen
                        ),
                        Err(e) => log::error!("Скан: ошибка БД: {e}"),
                    },
                    Err(e) => log::error!("Скан: не удалось открыть БД: {e}"),
                }
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(w) = weak.upgrade() {
                        w.invoke_rescan_done();
                    }
                });
            });
        })
    };

    {
        let rebuild_gallery = rebuild_gallery.clone();
        window.on_rescan_done(move || rebuild_gallery());
    }

    // Кнопка «Обновить» запускает скан (по завершении обновит сетку и список).
    {
        let start_scan = start_scan.clone();
        window.on_refresh(move || start_scan());
    }

    {
        let rebuild_gallery = rebuild_gallery.clone();
        window.on_sort_changed(move || rebuild_gallery());
    }

    // Поиск по имени + фильтр по типу — просто пересобираем галерею.
    {
        let rebuild_gallery = rebuild_gallery.clone();
        window.on_search_changed(move || rebuild_gallery());
    }

    // Старт: первичный скан.
    start_scan();

    // --- Фильтр: ввод + автоподсказки ---
    {
        let weak = window.as_weak();
        let tag_names = tag_names.clone();
        let rebuild_gallery = rebuild_gallery.clone();
        window.on_filter_edited(move |text| {
            if let Some(w) = weak.upgrade() {
                let q = last_token_query(&text);
                let sug = suggest(&q, &tag_names(), &std::collections::HashSet::new());
                w.set_filter_suggestions(strings_model(sug));
            }
            rebuild_gallery();
        });
    }

    {
        let weak = window.as_weak();
        let rebuild_gallery = rebuild_gallery.clone();
        window.on_pick_filter_suggestion(move |chosen| {
            if let Some(w) = weak.upgrade() {
                let new = replace_last_token(&w.get_filter_text(), &chosen);
                w.set_filter_text(new.into());
                w.set_filter_suggestions(strings_model(Vec::new()));
            }
            rebuild_gallery();
        });
    }

    // Контекстное меню тега: изменение строки фильтра по имени тега.
    {
        let apply: Rc<dyn Fn(slint::SharedString, TokenOp)> = {
            let weak = window.as_weak();
            let rebuild_gallery = rebuild_gallery.clone();
            Rc::new(move |name, op| {
                if let Some(w) = weak.upgrade() {
                    let new = set_filter_token(&w.get_filter_text(), name.as_str(), op);
                    w.set_filter_text(new.into());
                    w.set_filter_suggestions(strings_model(Vec::new()));
                }
                rebuild_gallery();
            })
        };
        let a1 = apply.clone();
        window.on_filter_add_include(move |name| a1(name, TokenOp::Include));
        let a2 = apply.clone();
        window.on_filter_add_exclude(move |name| a2(name, TokenOp::Exclude));
        let a3 = apply.clone();
        window.on_filter_remove(move |name| a3(name, TokenOp::Remove));
    }

    // --- Назначение тегов файлу (чипы + добавление с автодополнением) ---
    // Общая логика «назначить тег по имени (создать при необходимости)».
    let assign_by_name: Rc<dyn Fn(&str)> = {
        let weak = window.as_weak();
        let ui_db = ui_db.clone();
        let current = current_open_id.clone();
        let rebuild_viewer_tags = rebuild_viewer_tags.clone();
        let rebuild_gallery = rebuild_gallery.clone();
        Rc::new(move |name: &str| {
            let id = current.get();
            let name = name.trim();
            if id < 0 || name.is_empty() {
                return;
            }
            match ui_db.create_tag(name, now_unix()) {
                Ok(tag_id) => {
                    if let Err(e) = ui_db.assign_tag(id, tag_id) {
                        log::error!("assign_tag: {e}");
                    }
                }
                Err(e) => log::error!("create_tag: {e}"),
            }
            if let Some(w) = weak.upgrade() {
                w.set_add_tag_text("".into());
                w.set_add_tag_suggestions(strings_model(Vec::new()));
            }
            rebuild_viewer_tags(id);
            rebuild_gallery();
        })
    };

    {
        let weak = window.as_weak();
        let ui_db = ui_db.clone();
        let tag_names = tag_names.clone();
        let current = current_open_id.clone();
        window.on_add_tag_edited(move |text| {
            if let Some(w) = weak.upgrade() {
                let id = current.get();
                let assigned: std::collections::HashSet<String> = if id >= 0 {
                    ui_db
                        .tags_for_file(id)
                        .unwrap_or_default()
                        .into_iter()
                        .map(|t| t.name)
                        .collect()
                } else {
                    std::collections::HashSet::new()
                };
                let sug = suggest(text.trim(), &tag_names(), &assigned);
                w.set_add_tag_suggestions(strings_model(sug));
            }
        });
    }

    {
        let assign_by_name = assign_by_name.clone();
        window.on_add_tag_commit(move |text| assign_by_name(&text));
    }

    {
        let assign_by_name = assign_by_name.clone();
        window.on_pick_add_suggestion(move |chosen| assign_by_name(&chosen));
    }

    // Клик по назначенному чипу — снять тег.
    {
        let ui_db = ui_db.clone();
        let rebuild_viewer_tags = rebuild_viewer_tags.clone();
        let rebuild_gallery = rebuild_gallery.clone();
        let current = current_open_id.clone();
        window.on_toggle_file_tag(move |tag_id| {
            let id = current.get();
            if id < 0 {
                return;
            }
            if let Err(e) = ui_db.unassign_tag(id, tag_id as i64) {
                log::error!("unassign_tag: {e}");
            }
            // Если у тега не осталось ни одного файла — удаляем сам тег.
            let _ = ui_db.delete_tag_if_orphan(tag_id as i64);
            rebuild_viewer_tags(id);
            rebuild_gallery();
        });
    }

    // --- Шаг C: ПКМ-меню списка (массовые операции над выделением) ---
    let ctx_targets: Rc<RefCell<Vec<i64>>> = Rc::new(RefCell::new(Vec::new()));
    // Режим панели тегов: 0 — добавить, 1 — убрать.
    let bulk_mode: Rc<Cell<i32>> = Rc::new(Cell::new(0));

    // Набор целей: если строка входит в выделение целиком — берём всё выделение;
    // иначе операция касается только этой строки (файл или все файлы папки).
    let compute_targets: Rc<dyn Fn(i32) -> Vec<i64>> = {
        let tree_mirror = tree_mirror.clone();
        let selected = selected.clone();
        Rc::new(move |idx: i32| {
            let mirror = tree_mirror.borrow();
            let Some(node) = mirror.get(idx as usize) else {
                return Vec::new();
            };
            let sel = selected.borrow();
            if node.is_folder {
                let full = !node.descendants.is_empty()
                    && node.descendants.iter().all(|id| sel.contains(id));
                if full {
                    sel.iter().copied().collect()
                } else {
                    node.descendants.clone()
                }
            } else if sel.contains(&node.file_id) {
                sel.iter().copied().collect()
            } else {
                vec![node.file_id]
            }
        })
    };

    // Применение тега ко всем целям (add/remove в зависимости от bulk_mode).
    let bulk_apply: Rc<dyn Fn(&str)> = {
        let weak = window.as_weak();
        let ui_db = ui_db.clone();
        let ctx_targets = ctx_targets.clone();
        let bulk_mode = bulk_mode.clone();
        let rebuild_gallery = rebuild_gallery.clone();
        Rc::new(move |name: &str| {
            let name = name.trim();
            let ids = ctx_targets.borrow().clone();
            if !name.is_empty() && !ids.is_empty() {
                if bulk_mode.get() == 0 {
                    match ui_db.create_tag(name, now_unix()) {
                        Ok(tag_id) => {
                            for id in &ids {
                                let _ = ui_db.assign_tag(*id, tag_id);
                            }
                        }
                        Err(e) => log::error!("create_tag: {e}"),
                    }
                } else if let Some(tag_id) = ui_db.list_tags().ok().and_then(|v| {
                    v.into_iter()
                        .find(|t| t.name.eq_ignore_ascii_case(name))
                        .map(|t| t.id)
                }) {
                    for id in &ids {
                        let _ = ui_db.unassign_tag(*id, tag_id);
                    }
                    // Тег мог осиротеть после массового снятия — подчищаем.
                    let _ = ui_db.delete_tag_if_orphan(tag_id);
                }
            }
            if let Some(w) = weak.upgrade() {
                w.set_bulk_tag_open(false);
                w.set_bulk_tag_text("".into());
            }
            rebuild_gallery();
        })
    };

    // Открыть панель тегов (общий код для add/remove).
    let open_bulk_panel: Rc<dyn Fn(i32, i32)> = {
        let weak = window.as_weak();
        let compute_targets = compute_targets.clone();
        let ctx_targets = ctx_targets.clone();
        let bulk_mode = bulk_mode.clone();
        Rc::new(move |idx: i32, mode: i32| {
            let t = compute_targets(idx);
            let n = t.len();
            if n == 0 {
                return;
            }
            *ctx_targets.borrow_mut() = t;
            bulk_mode.set(mode);
            if let Some(w) = weak.upgrade() {
                let title = if mode == 0 {
                    format!("Добавить теги — {n} файл(ов)")
                } else {
                    format!("Удалить теги — {n} файл(ов)")
                };
                w.set_bulk_tag_title(title.into());
                w.set_bulk_tag_text("".into());
                w.set_bulk_tag_suggestions(strings_model(Vec::new()));
                w.set_bulk_tag_open(true);
            }
        })
    };

    {
        let open_bulk_panel = open_bulk_panel.clone();
        window.on_ctx_add_tags(move |idx| open_bulk_panel(idx, 0));
    }
    {
        let open_bulk_panel = open_bulk_panel.clone();
        window.on_ctx_remove_tags(move |idx| open_bulk_panel(idx, 1));
    }
    {
        let bulk_apply = bulk_apply.clone();
        window.on_bulk_tag_commit(move |text| bulk_apply(&text));
    }
    {
        let bulk_apply = bulk_apply.clone();
        window.on_bulk_tag_pick(move |chosen| bulk_apply(&chosen));
    }
    {
        let weak = window.as_weak();
        window.on_bulk_tag_cancel(move || {
            if let Some(w) = weak.upgrade() {
                w.set_bulk_tag_open(false);
            }
        });
    }
    {
        let weak = window.as_weak();
        let tag_names = tag_names.clone();
        window.on_bulk_tag_edited(move |text| {
            if let Some(w) = weak.upgrade() {
                let sug = suggest(text.trim(), &tag_names(), &std::collections::HashSet::new());
                w.set_bulk_tag_suggestions(strings_model(sug));
            }
        });
    }

    // Удаление с диска: показать подтверждение.
    {
        let weak = window.as_weak();
        let compute_targets = compute_targets.clone();
        let ctx_targets = ctx_targets.clone();
        window.on_ctx_delete(move |idx| {
            let t = compute_targets(idx);
            let n = t.len();
            if n == 0 {
                return;
            }
            *ctx_targets.borrow_mut() = t;
            if let Some(w) = weak.upgrade() {
                w.set_confirm_delete_text(
                    format!("Безвозвратно удалить с диска: {n} файл(ов)?").into(),
                );
                w.set_confirm_delete_open(true);
            }
        });
    }
    {
        let weak = window.as_weak();
        let ui_db = ui_db.clone();
        let ctx_targets = ctx_targets.clone();
        let selected = selected.clone();
        let rebuild_gallery = rebuild_gallery.clone();
        let media = paths.media.clone();
        let thumbs = paths.thumbnails.clone();
        window.on_confirm_delete_ok(move || {
            let ids = ctx_targets.borrow().clone();
            for id in &ids {
                if let Ok(Some((rel, _mt))) = ui_db.file_info(*id) {
                    let abs = media.join(&rel);
                    if let Err(e) = std::fs::remove_file(&abs) {
                        log::warn!("Удаление {rel}: {e}");
                    }
                    let _ = std::fs::remove_file(thumbs.join(format!("{id}.jpg")));
                }
            }
            let _ = ui_db.mark_deleted(&ids);
            {
                let mut sel = selected.borrow_mut();
                for id in &ids {
                    sel.remove(id);
                }
            }
            if let Some(w) = weak.upgrade() {
                w.set_confirm_delete_open(false);
            }
            rebuild_gallery();
        });
    }
    {
        let weak = window.as_weak();
        window.on_confirm_delete_cancel(move || {
            if let Some(w) = weak.upgrade() {
                w.set_confirm_delete_open(false);
            }
        });
    }

    // Открыть расположение объекта под курсором.
    {
        let tree_mirror = tree_mirror.clone();
        let media = paths.media.clone();
        window.on_ctx_open_location(move |idx| {
            let mirror = tree_mirror.borrow();
            let Some(node) = mirror.get(idx as usize) else {
                return;
            };
            let abs = media.join(&node.path);
            let is_folder = node.is_folder;
            drop(mirror);
            open_location(&abs, is_folder);
        });
    }

    // Гейт: рисуем mpv-подложку только когда открыт видео-просмотр.
    let render_video: Rc<Cell<bool>> = Rc::new(Cell::new(false));

    // Открытие файла: фото → показать в оверлее; видео → загрузить в mpv.
    // --- Состояние расширенного плеера ---
    let fullscreen: Rc<Cell<bool>> = Rc::new(Cell::new(false));
    let speed_idx: Rc<Cell<usize>> = Rc::new(Cell::new(2)); // 1.0x
    let muted: Rc<Cell<bool>> = Rc::new(Cell::new(false));
    let loop_on: Rc<Cell<bool>> = Rc::new(Cell::new(false));
    let last_activity: Rc<Cell<std::time::Instant>> =
        Rc::new(Cell::new(std::time::Instant::now()));

    let open_file: Rc<dyn Fn(i32)> = {
        let weak = window.as_weak();
        let ui_db = ui_db.clone();
        let media = paths.media.clone();
        let mpv = mpv_opt.clone();
        let render_video = render_video.clone();
        let current = current_open_id.clone();
        let rebuild_viewer_tags = rebuild_viewer_tags.clone();
        let loop_on = loop_on.clone();
        Rc::new(move |id: i32| {
            let Some(w) = weak.upgrade() else { return };
            match ui_db.file_info(id as i64) {
                Ok(Some((rel, mt))) => {
                    let full = media.join(&rel);
                    current.set(id as i64);
                    rebuild_viewer_tags(id as i64);
                    w.set_add_tag_text("".into());
                    w.set_add_tag_suggestions(strings_model(Vec::new()));
                    w.set_viewer_name(basename(&rel).into());
                    // Сброс зума/панорамы при открытии нового файла.
                    w.set_viewer_zoom(1.0);
                    w.set_viewer_pan_x(0.0);
                    w.set_viewer_pan_y(0.0);
                    w.set_viewer_controls_visible(true);

                    // Анимированные GIF проигрываем через mpv (анимация + луп),
                    // получая заодно зум/пан/фуллскрин. mpv должен быть доступен.
                    let is_gif = rel.to_lowercase().ends_with(".gif");
                    let via_mpv = matches!(mt, db::MediaType::Video)
                        || (is_gif && mpv.is_some());

                    if via_mpv {
                        if let Some(mpv) = &mpv {
                            w.set_viewer_is_video(true);
                            w.set_viewer_paused(false);
                            w.set_viewer_pos(0.0);
                            w.set_viewer_duration(0.0);
                            w.set_viewer_open(true);
                            render_video.set(true);
                            // Сброс трансформации видео в mpv.
                            mpv.set_video_zoom(1.0);
                            mpv.set_video_pan(0.0, 0.0);
                            // GIF по умолчанию зациклен; обычное видео — нет.
                            loop_on.set(is_gif);
                            mpv.set_loop(is_gif);
                            w.set_viewer_loop(is_gif);
                            if let Err(e) = mpv.loadfile(&full.to_string_lossy()) {
                                log::error!("{e}");
                            }
                            w.window().request_redraw();
                        }
                    } else {
                        render_video.set(false);
                        match slint::Image::load_from_path(&full) {
                            Ok(img) => w.set_viewer_image(img),
                            Err(e) => log::warn!("Фото {rel}: {e}"),
                        }
                        w.set_viewer_is_video(false);
                        w.set_viewer_open(true);
                    }
                }
                Ok(None) => log::warn!("Файл id={id} не найден в БД"),
                Err(e) => log::error!("file_info: {e}"),
            }
        })
    };

    {
        let open_file = open_file.clone();
        window.on_open_file(move |id| open_file(id));
    }

    // Вид «Список»: одиночный клик — выделение (мгновенно), двойной —
    // открыть/раскрыть. Выделение обновляем НА МЕСТЕ (без пересборки модели),
    // иначе делегаты пересоздаются и детект двойного клика ломается.
    //
    // Оптимистичная схема с откатом: Slint при двойном клике шлёт `clicked`
    // ДВАЖДЫ, а затем `double-clicked`. Каждый одиночный клик сразу применяет
    // выделение (мгновенный отклик). Перед первым кликом пары запоминаем снимок
    // выделения; при `double-clicked` откатываем к нему и выполняем действие.

    // Пересчёт состояний строк дерева из текущего `selected` (без структурных
    // изменений модели) + обновление счётчика.
    let tree_refresh_sel: Rc<dyn Fn()> = {
        let weak = window.as_weak();
        let selected = selected.clone();
        let tree_mirror = tree_mirror.clone();
        let tree_model = tree_model.clone();
        Rc::new(move || {
            let mirror = tree_mirror.borrow();
            let sel = selected.borrow();
            for (i, n) in mirror.iter().enumerate() {
                let state = tree::sel_state(n, &sel);
                if let Some(mut row) = tree_model.row_data(i) {
                    if row.sel_state != state {
                        row.sel_state = state;
                        tree_model.set_row_data(i, row);
                    }
                }
            }
            if let Some(w) = weak.upgrade() {
                w.set_selected_count(sel.len() as i32);
            }
        })
    };

    // Снимок выделения до первого клика пары и метка времени последнего клика
    // (для распознавания «второй клик пары» — тогда снимок НЕ обновляем).
    let sel_snapshot: Rc<RefCell<std::collections::HashSet<i64>>> =
        Rc::new(RefCell::new(std::collections::HashSet::new()));
    let last_click: Rc<RefCell<Option<(i32, std::time::Instant)>>> = Rc::new(RefCell::new(None));

    {
        let selected = selected.clone();
        let tree_mirror = tree_mirror.clone();
        let tree_refresh_sel = tree_refresh_sel.clone();
        let sel_snapshot = sel_snapshot.clone();
        let last_click = last_click.clone();
        window.on_tree_clicked(move |idx| {
            // Второй клик пары по той же строке в пределах окна двойного клика?
            let now = std::time::Instant::now();
            let is_pair_second = last_click
                .borrow()
                .map(|(i, t)| i == idx && now.duration_since(t) < Duration::from_millis(500))
                .unwrap_or(false);
            *last_click.borrow_mut() = Some((idx, now));
            // Снимок берём только перед ПЕРВЫМ кликом пары.
            if !is_pair_second {
                *sel_snapshot.borrow_mut() = selected.borrow().clone();
            }

            let mirror = tree_mirror.borrow();
            let Some(node) = mirror.get(idx as usize) else { return };
            {
                let mut sel = selected.borrow_mut();
                if node.is_folder {
                    let all = !node.descendants.is_empty()
                        && node.descendants.iter().all(|id| sel.contains(id));
                    if all {
                        for id in &node.descendants {
                            sel.remove(id);
                        }
                    } else {
                        for id in &node.descendants {
                            sel.insert(*id);
                        }
                    }
                } else if !sel.remove(&node.file_id) {
                    sel.insert(node.file_id);
                }
            }
            drop(mirror);
            tree_refresh_sel();
        });
    }

    // Клик по треугольнику раскрытия у папки — только раскрыть/свернуть.
    {
        let expanded = expanded.clone();
        let tree_mirror = tree_mirror.clone();
        let rebuild_gallery = rebuild_gallery.clone();
        window.on_tree_expand(move |idx| {
            let mirror = tree_mirror.borrow();
            let Some(node) = mirror.get(idx as usize) else { return };
            if !node.is_folder {
                return;
            }
            let path = node.path.clone();
            drop(mirror);
            let mut exp = expanded.borrow_mut();
            if !exp.remove(&path) {
                exp.insert(path);
            }
            drop(exp);
            rebuild_gallery();
        });
    }

    {
        let expanded = expanded.clone();
        let tree_mirror = tree_mirror.clone();
        let rebuild_gallery = rebuild_gallery.clone();
        let open_file = open_file.clone();
        let selected = selected.clone();
        let tree_refresh_sel = tree_refresh_sel.clone();
        let sel_snapshot = sel_snapshot.clone();
        let last_click = last_click.clone();
        window.on_tree_double(move |idx| {
            // Откатываем выделение к снимку до первого клика пары.
            *selected.borrow_mut() = sel_snapshot.borrow().clone();
            *last_click.borrow_mut() = None;
            tree_refresh_sel();

            let mirror = tree_mirror.borrow();
            let Some(node) = mirror.get(idx as usize) else { return };
            if node.is_folder {
                let path = node.path.clone();
                drop(mirror);
                let mut exp = expanded.borrow_mut();
                if !exp.remove(&path) {
                    exp.insert(path);
                }
                drop(exp);
                rebuild_gallery();
            } else {
                let id = node.file_id as i32;
                drop(mirror);
                open_file(id);
            }
        });
    }

    {
        let weak = window.as_weak();
        let mpv = mpv_opt.clone();
        let render_video = render_video.clone();
        let current = current_open_id.clone();
        let fullscreen = fullscreen.clone();
        window.on_viewer_close(move || {
            if let Some(w) = weak.upgrade() {
                if fullscreen.get() {
                    w.window().set_fullscreen(false);
                    fullscreen.set(false);
                    w.set_viewer_fullscreen(false);
                }
                w.set_viewer_open(false);
                w.set_viewer_is_video(false);
                w.set_viewer_controls_visible(true);
            }
            current.set(-1);
            render_video.set(false);
            if let Some(mpv) = &mpv {
                mpv.stop();
            }
        });
    }

    {
        let weak = window.as_weak();
        let mpv = mpv_opt.clone();
        window.on_viewer_toggle_play(move || {
            if let Some(mpv) = &mpv {
                mpv.toggle_pause();
                if let Some(w) = weak.upgrade() {
                    w.set_viewer_paused(mpv.get_flag("pause").unwrap_or(false));
                    w.window().request_redraw();
                }
            }
        });
    }

    {
        let mpv = mpv_opt.clone();
        window.on_viewer_seek(move |secs| {
            if let Some(mpv) = &mpv {
                // Игнорируем «эхо» от таймера позиции: сеем только реальные скачки.
                let cur = mpv.get_double("time-pos").unwrap_or(-1e9);
                if (secs as f64 - cur).abs() > 0.75 {
                    mpv.set_double("time-pos", secs as f64);
                }
            }
        });
    }

    // Относительная перемотка (скипы ±5/±10).
    {
        let weak = window.as_weak();
        let mpv = mpv_opt.clone();
        window.on_viewer_skip(move |secs| {
            if let Some(mpv) = &mpv {
                mpv.seek_relative(secs as f64);
                if let Some(w) = weak.upgrade() {
                    w.window().request_redraw();
                }
            }
        });
    }

    // Покадровый шаг (ставит на паузу).
    {
        let weak = window.as_weak();
        let mpv = mpv_opt.clone();
        window.on_viewer_frame_step(move |dir| {
            if let Some(mpv) = &mpv {
                mpv.frame_step(dir >= 0);
                if let Some(w) = weak.upgrade() {
                    w.set_viewer_paused(mpv.get_flag("pause").unwrap_or(true));
                    w.window().request_redraw();
                }
            }
        });
    }

    // Громкость.
    {
        let mpv = mpv_opt.clone();
        window.on_viewer_set_volume(move |v| {
            if let Some(mpv) = &mpv {
                mpv.set_volume(v as f64);
            }
        });
    }

    // Mute.
    {
        let weak = window.as_weak();
        let mpv = mpv_opt.clone();
        let muted = muted.clone();
        window.on_viewer_toggle_mute(move || {
            let m = !muted.get();
            muted.set(m);
            if let Some(mpv) = &mpv {
                mpv.set_flag("mute", m);
            }
            if let Some(w) = weak.upgrade() {
                w.set_viewer_muted(m);
            }
        });
    }

    // Скорость (циклический перебор пресетов).
    {
        let weak = window.as_weak();
        let mpv = mpv_opt.clone();
        let speed_idx = speed_idx.clone();
        window.on_viewer_cycle_speed(move || {
            let i = (speed_idx.get() + 1) % SPEEDS.len();
            speed_idx.set(i);
            let s = SPEEDS[i];
            if let Some(mpv) = &mpv {
                mpv.set_speed(s);
            }
            if let Some(w) = weak.upgrade() {
                w.set_viewer_speed(s as f32);
            }
        });
    }

    // Луп.
    {
        let weak = window.as_weak();
        let mpv = mpv_opt.clone();
        let loop_on = loop_on.clone();
        window.on_viewer_toggle_loop(move || {
            let l = !loop_on.get();
            loop_on.set(l);
            if let Some(mpv) = &mpv {
                mpv.set_loop(l);
            }
            if let Some(w) = weak.upgrade() {
                w.set_viewer_loop(l);
            }
        });
    }

    // Полноэкранный режим.
    {
        let weak = window.as_weak();
        let fullscreen = fullscreen.clone();
        let last_activity = last_activity.clone();
        window.on_viewer_toggle_fullscreen(move || {
            let fs = !fullscreen.get();
            fullscreen.set(fs);
            if let Some(w) = weak.upgrade() {
                w.window().set_fullscreen(fs);
                w.set_viewer_fullscreen(fs);
                w.set_viewer_controls_visible(true);
                last_activity.set(std::time::Instant::now());
            }
        });
    }

    // Активность мыши — сброс авто-скрытия контролов.
    {
        let weak = window.as_weak();
        let last_activity = last_activity.clone();
        window.on_viewer_activity(move || {
            last_activity.set(std::time::Instant::now());
            if let Some(w) = weak.upgrade() {
                if !w.get_viewer_controls_visible() {
                    w.set_viewer_controls_visible(true);
                }
            }
        });
    }

    // Применение зума/панорамы к видео (для фото — чистый Slint).
    {
        let weak = window.as_weak();
        let mpv = mpv_opt.clone();
        window.on_viewer_transform_changed(move || {
            let Some(w) = weak.upgrade() else { return };
            if !w.get_viewer_is_video() {
                return;
            }
            if let Some(mpv) = &mpv {
                let z = w.get_viewer_zoom() as f64;
                mpv.set_video_zoom(z);
                // mpv `video-pan-*` — доля МАСШТАБИРОВАННОГО видео: при зуме та же
                // доля даёт больший пиксельный сдвиг. Делим на зум, чтобы движение
                // за курсором было 1:1 в пикселях окна (панораму копим как долю окна).
                let inv = if z > 0.0 { 1.0 / z } else { 1.0 };
                mpv.set_video_pan(
                    w.get_viewer_pan_x() as f64 * inv,
                    w.get_viewer_pan_y() as f64 * inv,
                );
                w.window().request_redraw();
            }
        });
    }

    // «Set as thumbnail»: текущий кадр видео → миниатюра файла.
    // Снимок делаем на UI-потоке (mpv-хэндл не Send), но тяжёлый ресайз/энкод
    // уводим в фоновый поток, чтобы не фризить интерфейс.
    {
        let weak = window.as_weak();
        let mpv = mpv_opt.clone();
        let current = current_open_id.clone();
        let thumbs = paths.thumbnails.clone();
        window.on_viewer_set_thumbnail(move || {
            let id = current.get();
            let Some(mpv) = &mpv else { return };
            if id < 0 {
                return;
            }
            let dst = thumbs.join(format!("{id}.jpg"));
            let tmp = thumbs.join(format!("{id}.cap.jpg"));
            // Быстрый снимок текущего кадра во временный JPEG (энкод одного кадра).
            if let Err(e) = mpv.screenshot_to_file(&tmp.to_string_lossy()) {
                log::error!("{e}");
                return;
            }
            let weak = weak.clone();
            std::thread::spawn(move || {
                if let Err(e) = crate::workers::make_image_thumb(&tmp, &dst) {
                    log::error!("Миниатюра из кадра: {e}");
                }
                let _ = std::fs::remove_file(&tmp);
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(w) = weak.upgrade() {
                        w.invoke_thumb_updated(id as i32);
                    }
                });
            });
        });
    }

    // Фоновая генерация миниатюры завершена — сбрасываем кэш и обновляем плитку.
    {
        let thumbs = paths.thumbnails.clone();
        let thumb_cache = thumb_cache.clone();
        let rebuild_gallery = rebuild_gallery.clone();
        window.on_thumb_updated(move |id| {
            let dst = thumbs.join(format!("{id}.jpg"));
            thumb_cache
                .borrow_mut()
                .remove(&dst.to_string_lossy().into_owned());
            rebuild_gallery();
        });
    }

    // Таймер плеера: тянем позицию/длительность/паузу из mpv в UI.
    let player_timer = slint::Timer::default();
    {
        let weak = window.as_weak();
        let mpv = mpv_opt.clone();
        let fullscreen = fullscreen.clone();
        let last_activity = last_activity.clone();
        player_timer.start(
            slint::TimerMode::Repeated,
            Duration::from_millis(250),
            move || {
                let Some(w) = weak.upgrade() else { return };
                if !w.get_viewer_open() || !w.get_viewer_is_video() {
                    return;
                }
                if let Some(mpv) = &mpv {
                    let pos = mpv.get_double("time-pos").unwrap_or(0.0);
                    let dur = mpv.get_double("duration").unwrap_or(0.0);
                    w.set_viewer_pos(pos as f32);
                    w.set_viewer_duration(dur as f32);
                    w.set_viewer_time_text(
                        format!("{} / {}", fmt_time(pos), fmt_time(dur)).into(),
                    );
                    if let Some(paused) = mpv.get_flag("pause") {
                        w.set_viewer_paused(paused);
                    }
                }
                // Авто-скрытие контролов в полноэкранном режиме.
                if fullscreen.get() {
                    let idle = last_activity.get().elapsed() > Duration::from_millis(2500);
                    if idle && w.get_viewer_controls_visible() {
                        w.set_viewer_controls_visible(false);
                    }
                } else if !w.get_viewer_controls_visible() {
                    w.set_viewer_controls_visible(true);
                }
            },
        );
    }

    // --- mpv-пайплайн (render API под UI) ---
    if let Some(mpv) = mpv_opt.clone() {
        let render_ctx: Rc<Cell<*mut MpvRenderContext>> = Rc::new(Cell::new(std::ptr::null_mut()));
        let weak_box: *mut slint::Weak<MainWindow> = Box::into_raw(Box::new(window.as_weak()));

        let notifier_mpv = mpv.clone();
        let notifier_ctx = render_ctx.clone();
        let weak_for_redraw = window.as_weak();
        let gate = render_video.clone();

        let res = window
            .window()
            .set_rendering_notifier(move |state, api| match state {
                RenderingState::RenderingSetup => {
                    if let GraphicsAPI::NativeOpenGL { get_proc_address } = api {
                        match unsafe { notifier_mpv.create_render_context(*get_proc_address) } {
                            Ok(ctx) => {
                                notifier_ctx.set(ctx);
                                unsafe {
                                    notifier_mpv.set_update_callback(
                                        ctx,
                                        on_mpv_update,
                                        weak_box as *mut std::ffi::c_void,
                                    );
                                }
                                log::info!("mpv render-контекст создан");
                            }
                            Err(code) => log::error!("mpv_render_context_create -> {code}"),
                        }
                    }
                }
                RenderingState::BeforeRendering => {
                    let ctx = notifier_ctx.get();
                    if gate.get() && !ctx.is_null() {
                        if let Some(win) = weak_for_redraw.upgrade() {
                            let size = win.window().size();
                            unsafe {
                                notifier_mpv.render(ctx, size.width as i32, size.height as i32);
                            }
                        }
                    }
                }
                RenderingState::RenderingTeardown => {
                    let ctx = notifier_ctx.replace(std::ptr::null_mut());
                    if !ctx.is_null() {
                        unsafe { notifier_mpv.free_render_context(ctx) };
                    }
                }
                _ => {}
            });

        if let Err(e) = res {
            log::warn!("Rendering notifier недоступен: {e:?}");
        }
    }

    window.run()?;
    Ok(())
}
