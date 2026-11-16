// MicroMedia — галерея + оверлей-просмотрщик (фото/видео) + фон (скан/воркеры).

#[allow(dead_code)] // API дозревает вместе со сканером/тегами
mod config;
mod db;
mod graphics;
mod media;
#[allow(dead_code)] // часть mpv-методов пригодится позже
mod mpv;
#[allow(dead_code)]
mod paths;
mod scanner;
mod tree;
mod videothumb;
mod viewer;
mod workers;

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::Path;
use std::rc::Rc;
use std::time::Duration;

use slint::{ComponentHandle, GraphicsAPI, Model, ModelRc, VecModel};

use mpv::Mpv;

slint::include_modules!();


/// Размер в строку для UI: пусто, если неизвестно (NULL) или не определить (-1).
fn fmt_dim(v: Option<i64>) -> slint::SharedString {
    match v {
        Some(n) if n > 0 => n.to_string().into(),
        _ => slint::SharedString::new(),
    }
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
            height: fmt_dim(f.height),
            width: fmt_dim(f.width),
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

/// Байтовый индекс начала последнего (редактируемого) токена.
fn last_token_start(text: &str) -> usize {
    if text.ends_with(char::is_whitespace) {
        text.len()
    } else {
        text.rfind(char::is_whitespace).map(|i| i + 1).unwrap_or(0)
    }
}

/// Заменяет последний токен строки фильтра на выбранный тег (+ пробел).
fn replace_last_token(text: &str, chosen: &str) -> String {
    let start = last_token_start(text);
    let neg = text[start..].starts_with('-');
    format!("{}{}{} ", &text[..start], if neg { "-" } else { "" }, chosen)
}

/// Навигация ↑/↓: подставляет тег в последний токен БЕЗ завершающего пробела,
/// сохраняя набранный префикс «-» (чтобы можно было листать дальше).
fn nav_last_token(text: &str, chosen: &str) -> String {
    let start = last_token_start(text);
    let neg = text[start..].starts_with('-');
    format!("{}{}{}", &text[..start], if neg { "-" } else { "" }, chosen)
}

/// Коммит последнего токена в фильтр: include или (exclude → с «-») + пробел.
/// None, если токен пуст (нечего добавлять).
fn commit_last_token(text: &str, exclude: bool) -> Option<String> {
    let start = last_token_start(text);
    let name = text[start..].trim_start_matches('-');
    if name.is_empty() {
        return None;
    }
    Some(format!(
        "{}{}{} ",
        &text[..start],
        if exclude { "-" } else { "" },
        name
    ))
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

/// Форматирует миллисекунды в m:ss (для полей таймкода редактора фрагментов).
fn fmt_mmss(ms: i64) -> String {
    fmt_time(ms as f64 / 1000.0)
}

/// Разбирает таймкод вида "m:ss" / "h:mm:ss" в миллисекунды. `None`, если формат некорректен.
fn parse_mmss(s: &str) -> Option<i64> {
    let parts: Vec<&str> = s.trim().split(':').collect();
    if parts.is_empty() || parts.len() > 3 {
        return None;
    }
    let mut secs: i64 = 0;
    for p in &parts {
        let v: i64 = p.trim().parse().ok()?;
        if v < 0 {
            return None;
        }
        secs = secs * 60 + v;
    }
    Some(secs * 1000)
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

    // Загрузка сохранённого состояния (сортировка, поиск тегов, громкость, GL).
    // Читаем ДО создания окна: решение о программном GL нужно принять раньше,
    // чем Slint инициализирует драйвер (Mesa читает переменные при старте GL).
    let cfg = config::Config::load(&paths.config);
    let gl_mode = graphics::setup(cfg.software_gl);

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

    window.set_sort_index(cfg.sort_index);
    window.set_filter_text(cfg.filter_text.clone().into());
    window.set_viewer_volume(cfg.volume);
    if let Some(mpv) = &mpv_opt {
        mpv.set_volume(cfg.volume as f64);
    }

    // Режим GL запоминаем в конфиге: если аппаратный оказался нерабочим, следующий
    // запуск сразу идёт программным путём, без лишнего перезапуска.
    let software_gl = Rc::new(Cell::new(gl_mode.software));

    // Сохранение состояния (вызываем при изменении сортировки/фильтра/громкости).
    let save_config: Rc<dyn Fn()> = {
        let weak = window.as_weak();
        let config_path = paths.config.clone();
        let software_gl = software_gl.clone();
        Rc::new(move || {
            if let Some(w) = weak.upgrade() {
                config::Config {
                    sort_index: w.get_sort_index(),
                    filter_text: w.get_filter_text().to_string(),
                    volume: w.get_viewer_volume(),
                    software_gl: software_gl.get(),
                }
                .save(&config_path);
            }
        })
    };

    // --- Состояние ---
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


    // --- Редактор тегов одного файла (модалка) ---
    // Отдельный от просмотрщика id, чтобы редактор не конфликтовал с открытым
    // видео/фото.
    let editor_file_id: Rc<Cell<i64>> = Rc::new(Cell::new(-1));

    let rebuild_editor_tags: Rc<dyn Fn(i64)> = {
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
                w.set_tag_editor_tags(ModelRc::new(VecModel::from(items)));
            }
        })
    };

    let open_tag_editor: Rc<dyn Fn(i64)> = {
        let weak = window.as_weak();
        let ui_db = ui_db.clone();
        let editor_file_id = editor_file_id.clone();
        let rebuild_editor_tags = rebuild_editor_tags.clone();
        Rc::new(move |id: i64| {
            if id < 0 {
                return;
            }
            editor_file_id.set(id);
            rebuild_editor_tags(id);
            if let Some(w) = weak.upgrade() {
                let title = match ui_db.file_info(id) {
                    Ok(Some((rel, _))) => format!("Теги — {}", basename(&rel)),
                    _ => "Теги".to_string(),
                };
                w.set_tag_editor_title(title.into());
                w.set_tag_editor_text("".into());
                w.set_tag_editor_suggestions(strings_model(Vec::new()));
                w.set_tag_editor_open(true);
            }
        })
    };

    rebuild_gallery();

    // Masonry-раскладка плитки: жадный алгоритм «в самую короткую колонку»
    // по реальным пропорциям файла (пока разрешение не определено — квадрат).
    window.on_compute_masonry(|container_width, gap, columns, files| {
        let columns = columns.max(1) as usize;
        let col_width = ((container_width - gap * (columns as f32 + 1.0)) / columns as f32).max(1.0);
        let mut col_heights = vec![gap; columns];
        let mut positions = Vec::with_capacity(files.row_count());
        for item in files.iter() {
            let w: f32 = item.width.parse().unwrap_or(0.0);
            let h: f32 = item.height.parse().unwrap_or(0.0);
            let aspect = if w > 0.0 && h > 0.0 { h / w } else { 1.0 };
            let tile_height = (col_width * aspect).max(1.0);
            let (col, &y) = col_heights
                .iter()
                .enumerate()
                .min_by(|a, b| a.1.partial_cmp(b.1).unwrap())
                .unwrap();
            let x = gap + col as f32 * (col_width + gap);
            positions.push(TileLayout {
                x,
                y,
                width: col_width,
                height: tile_height,
            });
            col_heights[col] = y + tile_height + gap;
        }
        let height = col_heights.iter().cloned().fold(0.0f32, f32::max);
        MasonryResult {
            positions: ModelRc::new(VecModel::from(positions)),
            height,
        }
    });

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

    // Полная переиндексация в фоне: скан → превью → хеш → длительность.
    // Один и тот же путь для старта и кнопки «Обновить». Флаг indexing не даёт
    // запустить второй проход поверх идущего.
    let start_scan: Rc<dyn Fn()> = {
        let db_path = paths.db.clone();
        let media = paths.media.clone();
        let thumbs = paths.thumbnails.clone();
        let weak = window.as_weak();
        let indexing = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        Rc::new(move || {
            use std::sync::atomic::Ordering;
            if indexing.swap(true, Ordering::SeqCst) {
                log::info!("Индексация уже идёт — повторный запуск пропущен");
                return;
            }
            let db_path = db_path.clone();
            let media = media.clone();
            let thumbs = thumbs.clone();
            let weak = weak.clone();
            let indexing = indexing.clone();
            std::thread::spawn(move || {
                // UI-обновление: дёргаем rescan-done на потоке событий.
                let on_update = move || {
                    let weak = weak.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.invoke_rescan_done();
                        }
                    });
                };
                workers::reindex(&db_path, &media, &thumbs, on_update);
                indexing.store(false, Ordering::SeqCst);
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
        let save_config = save_config.clone();
        window.on_sort_changed(move || {
            rebuild_gallery();
            save_config();
        });
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
        let save_config = save_config.clone();
        window.on_filter_edited(move |text| {
            if let Some(w) = weak.upgrade() {
                let q = last_token_query(&text);
                let sug = suggest(&q, &tag_names(), &std::collections::HashSet::new());
                w.set_filter_suggestions(strings_model(sug));
            }
            rebuild_gallery();
            save_config();
        });
    }

    {
        let weak = window.as_weak();
        let rebuild_gallery = rebuild_gallery.clone();
        let save_config = save_config.clone();
        window.on_pick_filter_suggestion(move |chosen| {
            if let Some(w) = weak.upgrade() {
                let new = replace_last_token(&w.get_filter_text(), &chosen);
                w.set_filter_text(new.into());
                w.set_filter_suggestions(strings_model(Vec::new()));
            }
            rebuild_gallery();
            save_config();
        });
    }

    // Навигация ↑/↓ по подсказкам фильтра: подставить тег в последний токен.
    // Подсказки НЕ пересчитываем — чтобы по списку можно было листать.
    {
        let weak = window.as_weak();
        let rebuild_gallery = rebuild_gallery.clone();
        let save_config = save_config.clone();
        window.on_filter_nav(move |chosen| {
            if let Some(w) = weak.upgrade() {
                let new = nav_last_token(&w.get_filter_text(), &chosen);
                w.set_filter_text(new.into());
            }
            rebuild_gallery();
            save_config();
        });
    }

    // Enter — добавить последний токен в фильтр (include);
    // Shift+Enter — как исключение (с «-»). Затем начинаем новый токен.
    {
        let weak = window.as_weak();
        let rebuild_gallery = rebuild_gallery.clone();
        let save_config = save_config.clone();
        window.on_filter_commit(move |exclude| {
            if let Some(w) = weak.upgrade() {
                if let Some(new) = commit_last_token(&w.get_filter_text(), exclude) {
                    w.set_filter_text(new.into());
                    w.set_filter_suggestions(strings_model(Vec::new()));
                }
            }
            rebuild_gallery();
            save_config();
        });
    }

    // Контекстное меню тега: изменение строки фильтра по имени тега
    // (из боковой панели и из чипов плеера — в т.ч. в отдельных окнах).
    let filter_op: Rc<dyn Fn(slint::SharedString, TokenOp)> = {
        let weak = window.as_weak();
        let rebuild_gallery = rebuild_gallery.clone();
        let save_config = save_config.clone();
        Rc::new(move |name, op| {
            if let Some(w) = weak.upgrade() {
                let new = set_filter_token(&w.get_filter_text(), name.as_str(), op);
                w.set_filter_text(new.into());
                w.set_filter_suggestions(strings_model(Vec::new()));
            }
            rebuild_gallery();
            save_config();
        })
    };
    {
        let a1 = filter_op.clone();
        window.on_filter_add_include(move |name| a1(name, TokenOp::Include));
        let a2 = filter_op.clone();
        window.on_filter_add_exclude(move |name| a2(name, TokenOp::Exclude));
        let a3 = filter_op.clone();
        window.on_filter_remove(move |name| a3(name, TokenOp::Remove));
    }

    // Контекст для плееров (встроенного и отдельных окон).
    let app = Rc::new(viewer::App {
        db: ui_db.clone(),
        media: paths.media.clone(),
        thumbs: paths.thumbnails.clone(),
        main: window.as_weak(),
        rebuild_gallery: rebuild_gallery.clone(),
        tag_names: Rc::new(tag_names.clone()),
        filter_op: filter_op.clone(),
        save_config: save_config.clone(),
        players: RefCell::new(Vec::new()),
    });


    // --- Обработчики редактора тегов (модалка) ---
    // Открытие: из плитки по file-id, из списка — по idx строки дерева.
    {
        let open_tag_editor = open_tag_editor.clone();
        window.on_open_tag_editor(move |id| open_tag_editor(id as i64));
    }
    {
        let tree_mirror = tree_mirror.clone();
        let open_tag_editor = open_tag_editor.clone();
        window.on_ctx_edit_tags(move |idx| {
            let mirror = tree_mirror.borrow();
            let Some(node) = mirror.get(idx as usize) else {
                return;
            };
            if node.is_folder {
                return;
            }
            let fid = node.file_id;
            drop(mirror);
            open_tag_editor(fid);
        });
    }
    // Автоподсказки по мере ввода (исключая уже назначенные теги файла).
    {
        let weak = window.as_weak();
        let ui_db = ui_db.clone();
        let tag_names = tag_names.clone();
        let editor_file_id = editor_file_id.clone();
        window.on_tag_editor_edited(move |text| {
            if let Some(w) = weak.upgrade() {
                let id = editor_file_id.get();
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
                w.set_tag_editor_suggestions(strings_model(sug));
            }
        });
    }
    // Назначить тег по имени (создать при необходимости).
    let editor_assign: Rc<dyn Fn(&str)> = {
        let weak = window.as_weak();
        let ui_db = ui_db.clone();
        let editor_file_id = editor_file_id.clone();
        let rebuild_editor_tags = rebuild_editor_tags.clone();
        let rebuild_gallery = rebuild_gallery.clone();
        Rc::new(move |name: &str| {
            let id = editor_file_id.get();
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
                w.set_tag_editor_text("".into());
                w.set_tag_editor_suggestions(strings_model(Vec::new()));
            }
            rebuild_editor_tags(id);
            rebuild_gallery();
        })
    };
    {
        let editor_assign = editor_assign.clone();
        window.on_tag_editor_commit(move |t| editor_assign(&t));
    }
    {
        let editor_assign = editor_assign.clone();
        window.on_tag_editor_pick(move |t| editor_assign(&t));
    }
    // Снять тег с файла (с подтверждением в UI); осиротевший тег подчищаем.
    {
        let ui_db = ui_db.clone();
        let editor_file_id = editor_file_id.clone();
        let rebuild_editor_tags = rebuild_editor_tags.clone();
        let rebuild_gallery = rebuild_gallery.clone();
        window.on_tag_editor_remove(move |tag_id| {
            let id = editor_file_id.get();
            if id < 0 {
                return;
            }
            if let Err(e) = ui_db.unassign_tag(id, tag_id as i64) {
                log::error!("unassign_tag: {e}");
            }
            let _ = ui_db.delete_tag_if_orphan(tag_id as i64);
            rebuild_editor_tags(id);
            rebuild_gallery();
        });
    }
    {
        let weak = window.as_weak();
        window.on_tag_editor_cancel(move || {
            if let Some(w) = weak.upgrade() {
                w.set_tag_editor_open(false);
            }
        });
    }

    // --- ПКМ-действия плитки по одному файлу (удаление, расположение) ---
    // Удаление переиспользует общий оверлей подтверждения (ctx_targets ниже).
    {
        let ui_db = ui_db.clone();
        let media = paths.media.clone();
        window.on_open_file_location(move |id| {
            if let Ok(Some((rel, _))) = ui_db.file_info(id as i64) {
                open_location(&media.join(&rel), false);
            }
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

    // Удаление одного файла из плитки: подтверждение через общий оверлей.
    {
        let weak = window.as_weak();
        let ctx_targets = ctx_targets.clone();
        window.on_delete_file(move |id| {
            *ctx_targets.borrow_mut() = vec![id as i64];
            if let Some(w) = weak.upgrade() {
                w.set_confirm_delete_text("Безвозвратно удалить с диска: 1 файл?".into());
                w.set_confirm_delete_open(true);
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

    // --- Встроенный просмотрщик (поверх галереи) ---
    let main_viewer_state = viewer::ViewerState::new(mpv_opt.clone(), false);
    viewer::main_viewer::wire(&window, &main_viewer_state, &app);

    // Открытие файла: фото → показать в оверлее; видео → загрузить в mpv.
    let open_file: Rc<dyn Fn(i32)> = {
        let weak = window.as_weak();
        let st = main_viewer_state.clone();
        let app = app.clone();
        Rc::new(move |id: i32| {
            if let Some(w) = weak.upgrade() {
                viewer::main_viewer::open(&w, &st, &app, id as i64, 0.0, false);
            }
        })
    };

    {
        let open_file = open_file.clone();
        window.on_open_file(move |id| open_file(id));
    }

    // «Детач»: текущий файл уезжает в новое окно плеера (со своим mpv) с той же
    // позиции; встроенный плеер закрывается и главное окно возвращается к списку.
    {
        let weak = window.as_weak();
        let st = main_viewer_state.clone();
        let app = app.clone();
        window.on_viewer_detach(move || {
            let Some(w) = weak.upgrade() else { return };
            let id = st.current.get();
            if id < 0 {
                return;
            }
            let (pos, paused) = match (&st.mpv, w.get_viewer_is_video()) {
                (Some(mpv), true) => (
                    mpv.get_double("time-pos").unwrap_or(0.0),
                    mpv.get_flag("pause").unwrap_or(false),
                ),
                _ => (0.0, false),
            };
            let volume = w.get_viewer_volume();
            w.invoke_viewer_close();
            viewer::spawn_player(&app, id, pos, paused, volume);
        });
    }

    // Закрытие главного окна завершает приложение целиком, даже если открыты
    // отдельные окна плеера (иначе цикл событий жил бы до последнего окна).
    window.window().on_close_requested(|| {
        let _ = slint::quit_event_loop();
        slint::CloseRequestResponse::HideWindow
    });

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
        let weak = window.as_weak();
        let selected = selected.clone();
        let tree_mirror = tree_mirror.clone();
        let tree_refresh_sel = tree_refresh_sel.clone();
        let sel_snapshot = sel_snapshot.clone();
        let last_click = last_click.clone();
        let thumbs = paths.thumbnails.clone();
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
            // Превью в сайдбаре: миниатюра кликнутого файла (папки не трогаем).
            let clicked_file = (!node.is_folder).then_some(node.file_id);
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

            if let Some(fid) = clicked_file {
                if let Some(w) = weak.upgrade() {
                    let p = thumbs.join(format!("{fid}.jpg"));
                    match slint::Image::load_from_path(&p) {
                        Ok(img) => {
                            w.set_preview_image(img);
                            w.set_preview_has(true);
                        }
                        Err(_) => w.set_preview_has(false),
                    }
                }
            }
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


    // --- mpv-пайплайн (render API под UI) ---
    // Ставится, если GL-контекст окажется аварийным: тогда выходим из цикла
    // событий и перезапускаемся с программным GL (см. конец main).
    let restart_software_gl = Rc::new(Cell::new(false));

    if let Some(mpv) = mpv_opt.clone() {
        let can_fall_back = gl_mode.can_fall_back();
        let restart_flag = restart_software_gl.clone();
        viewer::attach_mpv_renderer(
            &window,
            mpv,
            main_viewer_state.render_video.clone(),
            move |api| {
                let GraphicsAPI::NativeOpenGL { get_proc_address } = api else {
                    return true;
                };
                // Чем нам достался GL: на сломанном стеке Mesa контекст
                // создаётся, но рендер mpv в FBO даёт чёрный кадр — ловим
                // это по строке GL_RENDERER и уходим на программный путь.
                if let Some(info) = unsafe { graphics::probe(*get_proc_address) } {
                    log::info!("GL: {} | {} | {}", info.renderer, info.vendor, info.version);
                    if info.looks_broken() && can_fall_back {
                        log::warn!(
                            "GL: контекст выглядит нерабочим ({}) — перезапускаюсь с программным рендерингом",
                            info.renderer
                        );
                        restart_flag.set(true);
                        let _ = slint::quit_event_loop();
                        return false;
                    }
                }
                true
            },
            || {},
        );
    }

    window.run()?;

    // Аварийный GL: запоминаем выбор и стартуем заново уже программным путём.
    // Именно перезапуском, а не переключением на лету: Mesa читает
    // LIBGL_ALWAYS_SOFTWARE при инициализации драйвера, менять его в живом
    // процессе поздно.
    if restart_software_gl.get() {
        software_gl.set(true);
        save_config();
        return graphics::restart_software().map_err(Into::into);
    }

    // Финальное сохранение состояния при выходе.
    save_config();
    Ok(())
}
