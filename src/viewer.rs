//! Просмотрщик (фото/видео): общая логика для встроенного плеера (MainWindow)
//! и отдельных окон плеера (PlayerWindow, «детач»).
//!
//! У каждого плеера — своё состояние [`ViewerState`] и свой экземпляр mpv со
//! своим render-контекстом в GL-контексте своего окна. Поэтому несколько окон
//! играют одновременно и независимо.
//!
//! Slint генерирует для обоих окон методы с одинаковыми именами
//! (`set_viewer_*`, `on_frag_*` …), но это разные типы без общего трейта.
//! Код плеера написан один раз и разворачивается макросом `viewer_impl!` в
//! модули `main_viewer` и `player_viewer`.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

use slint::{ComponentHandle, GraphicsAPI, ModelRc, RenderingState, SharedString, VecModel};

use crate::mpv::{Mpv, MpvRenderContext};
use crate::{db, MainWindow, PlayerWindow, TokenOp};

/// Общий контекст приложения, нужный плеерам (БД, пути, галерея).
pub struct App {
    pub db: Rc<db::Db>,
    pub media: PathBuf,
    pub thumbs: PathBuf,
    pub main: slint::Weak<MainWindow>,
    pub rebuild_gallery: Rc<dyn Fn()>,
    pub tag_names: Rc<dyn Fn() -> Vec<String>>,
    /// Изменение строки фильтра галереи по имени тега (ПКМ по чипу).
    pub filter_op: Rc<dyn Fn(SharedString, TokenOp)>,
    pub save_config: Rc<dyn Fn()>,
    /// Открытые отдельные окна плеера: держим, пока окно не закрыто.
    pub players: RefCell<Vec<(PlayerWindow, Rc<ViewerState>)>>,
}

/// Состояние одного плеера.
pub struct ViewerState {
    pub mpv: Option<Rc<Mpv>>,
    /// true — отдельное окно (закрытие плеера закрывает окно).
    pub detached: bool,
    /// id открытого файла, -1 — ничего не открыто.
    pub current: Cell<i64>,
    /// Гейт: рисуем mpv-подложку только когда открыто видео.
    pub render_video: Rc<Cell<bool>>,
    /// Фрагменты открытого видео: кэш для проверки пересечений и редактора.
    fragments: RefCell<Vec<db::Fragment>>,
    /// Стартовая точка разметки (мс), пока не нажата «Конец фрагмента».
    marking_start: Cell<Option<i64>>,
    /// id редактируемого фрагмента в оверлее метаданных; -1 = создание нового.
    editing_id: Cell<i64>,
    /// id фрагмента, ожидающего подтверждения удаления.
    delete_target: Cell<i64>,
    fullscreen: Cell<bool>,
    speed_idx: Cell<usize>,
    muted: Cell<bool>,
    loop_on: Cell<bool>,
    last_activity: Cell<Instant>,
    /// Файл открыт с опцией mpv `start` — сбросить её, когда файл загрузится
    /// (иначе она применится и к следующему файлу/лупу).
    pending_start: Cell<bool>,
    /// Таймер синхронизации позиции/паузы из mpv в UI.
    timer: slint::Timer,
}

impl ViewerState {
    pub fn new(mpv: Option<Rc<Mpv>>, detached: bool) -> Rc<Self> {
        Rc::new(ViewerState {
            mpv,
            detached,
            current: Cell::new(-1),
            render_video: Rc::new(Cell::new(false)),
            fragments: RefCell::new(Vec::new()),
            marking_start: Cell::new(None),
            editing_id: Cell::new(-1),
            delete_target: Cell::new(-1),
            fullscreen: Cell::new(false),
            speed_idx: Cell::new(2), // 1.0x
            muted: Cell::new(false),
            loop_on: Cell::new(false),
            last_activity: Cell::new(Instant::now()),
            pending_start: Cell::new(false),
            timer: slint::Timer::default(),
        })
    }
}

/// Колбэк «у mpv готов новый кадр» — просим окно перерисоваться.
/// Зовётся из потока mpv; `ctx` — `Box<slint::Weak<T>>` (Weak можно слать
/// между потоками, апгрейд — уже на потоке событий).
unsafe extern "C" fn on_mpv_update<T: ComponentHandle + 'static>(ctx: *mut std::ffi::c_void) {
    let weak = (*(ctx as *const slint::Weak<T>)).clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(win) = weak.upgrade() {
            win.window().request_redraw();
        }
    });
}

/// Подключает рендер mpv подложкой под UI окна (render API в fbo 0).
///
/// `on_setup` вызывается в `RenderingSetup` до создания render-контекста;
/// вернёт `false` — контекст не создаётся (аварийный выход на программный GL).
/// `on_ready` — render-контекст создан: только теперь можно грузить файл.
/// Если загрузить его раньше, `vo=libmpv` не находит контекст, и mpv
/// отключает видеодорожку (чёрное окно, звук при этом идёт).
pub fn attach_mpv_renderer<T: ComponentHandle + 'static>(
    win: &T,
    mpv: Rc<Mpv>,
    gate: Rc<Cell<bool>>,
    on_setup: impl Fn(&GraphicsAPI) -> bool + 'static,
    on_ready: impl Fn() + 'static,
) {
    let ctx: Cell<*mut MpvRenderContext> = Cell::new(std::ptr::null_mut());
    // Контекст для колбэка обновления: живёт ровно столько же, сколько render-контекст.
    let cb_box: Cell<*mut slint::Weak<T>> = Cell::new(std::ptr::null_mut());
    let weak = win.as_weak();

    let res = win.window().set_rendering_notifier(move |state, api| match state {
        RenderingState::RenderingSetup => {
            if !on_setup(api) {
                return;
            }
            if let GraphicsAPI::NativeOpenGL { get_proc_address } = api {
                match unsafe { mpv.create_render_context(*get_proc_address) } {
                    Ok(c) => {
                        let b = Box::into_raw(Box::new(weak.clone()));
                        unsafe {
                            mpv.set_update_callback(
                                c,
                                on_mpv_update::<T>,
                                b as *mut std::ffi::c_void,
                            );
                        }
                        ctx.set(c);
                        cb_box.set(b);
                        log::info!("mpv render-контекст создан");
                        on_ready();
                    }
                    Err(code) => log::error!("mpv_render_context_create -> {code}"),
                }
            }
        }
        RenderingState::BeforeRendering => {
            let c = ctx.get();
            if gate.get() && !c.is_null() {
                if let Some(w) = weak.upgrade() {
                    let size = w.window().size();
                    unsafe { mpv.render(c, size.width as i32, size.height as i32) };
                }
            }
        }
        RenderingState::RenderingTeardown => {
            let c = ctx.replace(std::ptr::null_mut());
            if !c.is_null() {
                // После free mpv больше не зовёт колбэк — можно освободить его контекст.
                unsafe { mpv.free_render_context(c) };
            }
            let b = cb_box.replace(std::ptr::null_mut());
            if !b.is_null() {
                drop(unsafe { Box::from_raw(b) });
            }
        }
        _ => {}
    });
    if let Err(e) = res {
        log::warn!("Rendering notifier недоступен: {e:?}");
    }
}

/// Диапазон [start_ms,end_ms) не должен пересекать/включать ни один
/// существующий фрагмент. `skip_id` — исключить сам редактируемый
/// фрагмент (-1 при создании нового).
fn frag_overlaps(fragments: &[db::Fragment], start_ms: i64, end_ms: i64, skip_id: i64) -> bool {
    fragments
        .iter()
        .any(|f| f.id != skip_id && f.start_ms < end_ms && start_ms < f.end_ms)
}

/// Открывает файл в новом отдельном окне плеера с позиции `start` (сек).
pub fn spawn_player(app: &Rc<App>, id: i64, start: f64, paused: bool, volume: f32) {
    let w = match PlayerWindow::new() {
        Ok(w) => w,
        Err(e) => {
            log::error!("Окно плеера: {e}");
            return;
        }
    };
    // Свой экземпляр mpv на окно — так окна играют независимо.
    let mpv = match Mpv::load() {
        Ok(m) => Some(Rc::new(m)),
        Err(e) => {
            log::warn!("mpv для окна плеера недоступен: {e}");
            None
        }
    };
    if let Some(m) = &mpv {
        m.set_volume(volume as f64);
    }
    w.set_viewer_volume(volume);

    let st = ViewerState::new(mpv.clone(), true);
    player_viewer::wire(&w, &st, app);

    // ПКМ по чипу тега меняет фильтр галереи в главном окне.
    {
        let f = app.filter_op.clone();
        w.on_filter_add_include(move |n| f(n, TokenOp::Include));
        let f = app.filter_op.clone();
        w.on_filter_add_exclude(move |n| f(n, TokenOp::Exclude));
        let f = app.filter_op.clone();
        w.on_filter_remove(move |n| f(n, TokenOp::Remove));
    }

    // Крестик окна ОС — как кнопка ✕ плеера.
    {
        let app = Rc::downgrade(app);
        let st = Rc::downgrade(&st);
        w.window().on_close_requested(move || {
            if let (Some(app), Some(st)) = (app.upgrade(), st.upgrade()) {
                release_player(&app, &st);
            }
            slint::CloseRequestResponse::HideWindow
        });
    }

    match mpv {
        // Файл грузим, только когда у окна появится render-контекст mpv (он
        // создаётся на первой отрисовке, после show). Открываем из таймера,
        // а не прямо из rendering notifier: менять свойства UI посреди
        // отрисовки нельзя. `opened` — на случай повторного RenderingSetup.
        Some(m) => {
            let weak = w.as_weak();
            let st_weak = Rc::downgrade(&st);
            let app_weak = Rc::downgrade(app);
            let opened = Cell::new(false);
            attach_mpv_renderer(&w, m, st.render_video.clone(), |_| true, move || {
                if opened.replace(true) {
                    return;
                }
                let (weak, st_weak, app_weak) = (weak.clone(), st_weak.clone(), app_weak.clone());
                slint::Timer::single_shot(Duration::ZERO, move || {
                    if let (Some(w), Some(st), Some(app)) =
                        (weak.upgrade(), st_weak.upgrade(), app_weak.upgrade())
                    {
                        player_viewer::open(&w, &st, &app, id, start, paused);
                    }
                });
            });
        }
        None => player_viewer::open(&w, &st, app, id, start, paused),
    }

    if let Err(e) = w.show() {
        log::error!("Окно плеера: {e}");
        return;
    }
    app.players.borrow_mut().push((w, st));
}

/// Останавливает плеер отдельного окна и отпускает окно. Само окно (и mpv)
/// освобождаем отложенно — нельзя ронять компонент изнутри его же колбэка.
fn release_player(app: &Rc<App>, st: &Rc<ViewerState>) {
    st.timer.stop();
    st.render_video.set(false);
    st.current.set(-1);
    if let Some(mpv) = &st.mpv {
        mpv.stop();
    }
    let app = Rc::downgrade(app);
    let st = Rc::downgrade(st);
    slint::Timer::single_shot(Duration::ZERO, move || {
        let (Some(app), Some(st)) = (app.upgrade(), st.upgrade()) else { return };
        // Выносим из списка до drop, чтобы не держать borrow во время
        // освобождения окна (Teardown рендера, остановка mpv).
        let removed: Vec<_> = {
            let mut v = app.players.borrow_mut();
            let (gone, keep): (Vec<_>, Vec<_>) =
                v.drain(..).partition(|(_, s)| Rc::ptr_eq(s, &st));
            *v = keep;
            gone
        };
        drop(st);
        drop(removed);
    });
}

macro_rules! viewer_impl {
    ($W:ident, $m:ident) => {
        pub mod $m {
            use super::*;
            use crate::{
                basename, fmt_mmss, fmt_time, now_unix, parse_mmss, strings_model, suggest,
                FragmentItem, TagItem, SPEEDS,
            };

            type W = crate::$W;

            /// Теги открытого файла (назначенные) для чипов просмотрщика.
            fn rebuild_tags(w: &W, app: &App, id: i64) {
                let items: Vec<TagItem> = app
                    .db
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

            /// Фрагменты открытого видео: обновляет кэш + модель для полоски.
            fn rebuild_fragments(w: &W, st: &ViewerState, app: &App, id: i64) {
                let list = app.db.fragments_for_file(id).unwrap_or_default();
                let items: Vec<FragmentItem> = list
                    .iter()
                    .map(|f| FragmentItem {
                        id: f.id as i32,
                        start: f.start_ms as f32 / 1000.0,
                        end: f.end_ms as f32 / 1000.0,
                        title: f.title.clone().into(),
                    })
                    .collect();
                w.set_viewer_fragments(ModelRc::new(VecModel::from(items)));
                *st.fragments.borrow_mut() = list;
            }

            /// Открытие файла: фото → показать; видео/GIF → загрузить в mpv
            /// (с позиции `start` сек, на паузе или нет).
            pub fn open(w: &W, st: &ViewerState, app: &App, id: i64, start: f64, paused: bool) {
                let (rel, mt) = match app.db.file_info(id) {
                    Ok(Some(x)) => x,
                    Ok(None) => {
                        log::warn!("Файл id={id} не найден в БД");
                        return;
                    }
                    Err(e) => {
                        log::error!("file_info: {e}");
                        return;
                    }
                };
                let full = app.media.join(&rel);
                st.current.set(id);
                rebuild_tags(w, app, id);
                rebuild_fragments(w, st, app, id);
                // Смена файла отменяет незавершённую разметку фрагмента.
                st.marking_start.set(None);
                w.set_frag_marking(false);
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
                let via_mpv =
                    matches!(mt, db::MediaType::Video) || (is_gif && st.mpv.is_some());

                if via_mpv {
                    let Some(mpv) = &st.mpv else { return };
                    w.set_viewer_is_video(true);
                    w.set_viewer_paused(paused);
                    w.set_viewer_pos(start as f32);
                    w.set_viewer_duration(0.0);
                    w.set_viewer_open(true);
                    st.render_video.set(true);
                    // Сброс трансформации видео в mpv.
                    mpv.set_video_zoom(1.0);
                    mpv.set_video_pan(0.0, 0.0);
                    // GIF по умолчанию зациклен; обычное видео — нет.
                    st.loop_on.set(is_gif);
                    mpv.set_loop(is_gif);
                    w.set_viewer_loop(is_gif);
                    mpv.set_pause(paused);
                    // Стартовая позиция — опцией `start` (без видимого скачка с 0).
                    if start > 0.0 {
                        mpv.set_property_str("start", &format!("{start:.3}"));
                        st.pending_start.set(true);
                    } else {
                        mpv.set_property_str("start", "none");
                        st.pending_start.set(false);
                    }
                    if let Err(e) = mpv.loadfile(&full.to_string_lossy()) {
                        log::error!("{e}");
                    }
                    w.window().request_redraw();
                } else {
                    st.render_video.set(false);
                    match slint::Image::load_from_path(&full) {
                        Ok(img) => w.set_viewer_image(img),
                        Err(e) => log::warn!("Фото {rel}: {e}"),
                    }
                    w.set_viewer_is_video(false);
                    w.set_viewer_open(true);
                }
            }

            /// Подключает все колбэки плеера и таймер синхронизации.
            pub fn wire(w: &W, st: &Rc<ViewerState>, app: &Rc<App>) {
                // --- Закрытие ---
                {
                    let weak = w.as_weak();
                    let st = Rc::downgrade(st);
                    let app = Rc::downgrade(app);
                    w.on_viewer_close(move || {
                        let (Some(st), Some(app)) = (st.upgrade(), app.upgrade()) else {
                            return;
                        };
                        if let Some(w) = weak.upgrade() {
                            if st.fullscreen.get() {
                                w.window().set_fullscreen(false);
                                st.fullscreen.set(false);
                                w.set_viewer_fullscreen(false);
                            }
                            if st.detached {
                                let _ = w.hide();
                                super::release_player(&app, &st);
                                return;
                            }
                            w.set_viewer_open(false);
                            w.set_viewer_is_video(false);
                            w.set_viewer_controls_visible(true);
                        }
                        st.current.set(-1);
                        st.render_video.set(false);
                        if let Some(mpv) = &st.mpv {
                            mpv.stop();
                        }
                    });
                }

                // --- Воспроизведение ---
                {
                    let weak = w.as_weak();
                    let st = st.clone();
                    w.on_viewer_toggle_play(move || {
                        if let Some(mpv) = &st.mpv {
                            mpv.toggle_pause();
                            if let Some(w) = weak.upgrade() {
                                w.set_viewer_paused(mpv.get_flag("pause").unwrap_or(false));
                                w.window().request_redraw();
                            }
                        }
                    });
                }
                {
                    let st = st.clone();
                    w.on_viewer_seek(move |secs| {
                        if let Some(mpv) = &st.mpv {
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
                    let weak = w.as_weak();
                    let st = st.clone();
                    w.on_viewer_skip(move |secs| {
                        if let Some(mpv) = &st.mpv {
                            mpv.seek_relative(secs as f64);
                            if let Some(w) = weak.upgrade() {
                                w.window().request_redraw();
                            }
                        }
                    });
                }
                // Покадровый шаг (ставит на паузу).
                {
                    let weak = w.as_weak();
                    let st = st.clone();
                    w.on_viewer_frame_step(move |dir| {
                        if let Some(mpv) = &st.mpv {
                            mpv.frame_step(dir >= 0);
                            if let Some(w) = weak.upgrade() {
                                w.set_viewer_paused(mpv.get_flag("pause").unwrap_or(true));
                                w.window().request_redraw();
                            }
                        }
                    });
                }
                // Громкость. В конфиг сохраняется только громкость главного окна.
                {
                    let st = st.clone();
                    let app = Rc::downgrade(app);
                    w.on_viewer_set_volume(move |v| {
                        if let Some(mpv) = &st.mpv {
                            mpv.set_volume(v as f64);
                        }
                        if !st.detached {
                            if let Some(app) = app.upgrade() {
                                (app.save_config)();
                            }
                        }
                    });
                }
                // Mute.
                {
                    let weak = w.as_weak();
                    let st = st.clone();
                    w.on_viewer_toggle_mute(move || {
                        let m = !st.muted.get();
                        st.muted.set(m);
                        if let Some(mpv) = &st.mpv {
                            mpv.set_flag("mute", m);
                        }
                        if let Some(w) = weak.upgrade() {
                            w.set_viewer_muted(m);
                        }
                    });
                }
                // Скорость (циклический перебор пресетов).
                {
                    let weak = w.as_weak();
                    let st = st.clone();
                    w.on_viewer_cycle_speed(move || {
                        let i = (st.speed_idx.get() + 1) % SPEEDS.len();
                        st.speed_idx.set(i);
                        let s = SPEEDS[i];
                        if let Some(mpv) = &st.mpv {
                            mpv.set_speed(s);
                        }
                        if let Some(w) = weak.upgrade() {
                            w.set_viewer_speed(s as f32);
                        }
                    });
                }
                // Луп.
                {
                    let weak = w.as_weak();
                    let st = st.clone();
                    w.on_viewer_toggle_loop(move || {
                        let l = !st.loop_on.get();
                        st.loop_on.set(l);
                        if let Some(mpv) = &st.mpv {
                            mpv.set_loop(l);
                        }
                        if let Some(w) = weak.upgrade() {
                            w.set_viewer_loop(l);
                        }
                    });
                }
                // Полноэкранный режим (своего окна).
                {
                    let weak = w.as_weak();
                    let st = st.clone();
                    w.on_viewer_toggle_fullscreen(move || {
                        let fs = !st.fullscreen.get();
                        st.fullscreen.set(fs);
                        if let Some(w) = weak.upgrade() {
                            w.window().set_fullscreen(fs);
                            w.set_viewer_fullscreen(fs);
                            w.set_viewer_controls_visible(true);
                            st.last_activity.set(Instant::now());
                        }
                    });
                }
                // Активность мыши — сброс авто-скрытия контролов.
                {
                    let weak = w.as_weak();
                    let st = st.clone();
                    w.on_viewer_activity(move || {
                        st.last_activity.set(Instant::now());
                        if let Some(w) = weak.upgrade() {
                            if !w.get_viewer_controls_visible() {
                                w.set_viewer_controls_visible(true);
                            }
                        }
                    });
                }
                // Применение зума/панорамы к видео (для фото — чистый Slint).
                {
                    let weak = w.as_weak();
                    let st = st.clone();
                    w.on_viewer_transform_changed(move || {
                        let Some(w) = weak.upgrade() else { return };
                        if !w.get_viewer_is_video() {
                            return;
                        }
                        if let Some(mpv) = &st.mpv {
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
                    let st = st.clone();
                    let app = Rc::downgrade(app);
                    w.on_viewer_set_thumbnail(move || {
                        let Some(app) = app.upgrade() else { return };
                        let id = st.current.get();
                        let Some(mpv) = &st.mpv else { return };
                        if id < 0 {
                            return;
                        }
                        let dst = app.thumbs.join(format!("{id}.jpg"));
                        // PNG, а не JPEG: сохранение в JPEG требует, чтобы libmpv была собрана
                        // с libjpeg, а портативная сборка идёт без него (docs/build-libmpv.md).
                        // PNG mpv умеет через zlib, который есть в любой сборке. В JPEG-миниатюру
                        // кадр всё равно перекодирует make_image_thumb.
                        let tmp = app.thumbs.join(format!("{id}.cap.png"));
                        if let Err(e) = mpv.screenshot_to_file(&tmp.to_string_lossy()) {
                            log::error!("{e}");
                            return;
                        }
                        // Плитка галереи — в главном окне.
                        let main = app.main.clone();
                        std::thread::spawn(move || {
                            if let Err(e) = crate::workers::make_image_thumb(&tmp, &dst) {
                                log::error!("Миниатюра из кадра: {e}");
                            }
                            let _ = std::fs::remove_file(&tmp);
                            let _ = slint::invoke_from_event_loop(move || {
                                if let Some(m) = main.upgrade() {
                                    m.invoke_thumb_updated(id as i32);
                                }
                            });
                        });
                    });
                }

                // --- Теги открытого файла (чипы + добавление с автодополнением) ---
                // Назначить тег по имени (создать при необходимости).
                let assign_by_name: Rc<dyn Fn(&str)> = {
                    let weak = w.as_weak();
                    let st = st.clone();
                    let app = Rc::downgrade(app);
                    Rc::new(move |name: &str| {
                        let Some(app) = app.upgrade() else { return };
                        let id = st.current.get();
                        let name = name.trim();
                        if id < 0 || name.is_empty() {
                            return;
                        }
                        match app.db.create_tag(name, now_unix()) {
                            Ok(tag_id) => {
                                if let Err(e) = app.db.assign_tag(id, tag_id) {
                                    log::error!("assign_tag: {e}");
                                }
                            }
                            Err(e) => log::error!("create_tag: {e}"),
                        }
                        if let Some(w) = weak.upgrade() {
                            w.set_add_tag_text("".into());
                            w.set_add_tag_suggestions(strings_model(Vec::new()));
                            rebuild_tags(&w, &app, id);
                        }
                        (app.rebuild_gallery)();
                    })
                };
                {
                    let weak = w.as_weak();
                    let st = st.clone();
                    let app = Rc::downgrade(app);
                    w.on_add_tag_edited(move |text| {
                        let (Some(w), Some(app)) = (weak.upgrade(), app.upgrade()) else {
                            return;
                        };
                        let id = st.current.get();
                        let assigned: std::collections::HashSet<String> = if id >= 0 {
                            app.db
                                .tags_for_file(id)
                                .unwrap_or_default()
                                .into_iter()
                                .map(|t| t.name)
                                .collect()
                        } else {
                            std::collections::HashSet::new()
                        };
                        let sug = suggest(text.trim(), &(app.tag_names)(), &assigned);
                        w.set_add_tag_suggestions(strings_model(sug));
                    });
                }
                {
                    let assign_by_name = assign_by_name.clone();
                    w.on_add_tag_commit(move |text| assign_by_name(&text));
                }
                {
                    let assign_by_name = assign_by_name.clone();
                    w.on_pick_add_suggestion(move |chosen| assign_by_name(&chosen));
                }
                // Крестик на чипе (после подтверждения) — снять тег.
                {
                    let weak = w.as_weak();
                    let st = st.clone();
                    let app = Rc::downgrade(app);
                    w.on_toggle_file_tag(move |tag_id| {
                        let Some(app) = app.upgrade() else { return };
                        let id = st.current.get();
                        if id < 0 {
                            return;
                        }
                        if let Err(e) = app.db.unassign_tag(id, tag_id as i64) {
                            log::error!("unassign_tag: {e}");
                        }
                        // Если у тега не осталось ни одного файла — удаляем сам тег.
                        let _ = app.db.delete_tag_if_orphan(tag_id as i64);
                        if let Some(w) = weak.upgrade() {
                            rebuild_tags(&w, &app, id);
                        }
                        (app.rebuild_gallery)();
                    });
                }

                // --- Фрагменты видео ---
                // Клик по кнопке «Начало фрагмента» / «Конец фрагмента».
                {
                    let weak = w.as_weak();
                    let st = st.clone();
                    w.on_frag_mark_clicked(move || {
                        let Some(w) = weak.upgrade() else { return };
                        let Some(mpv) = &st.mpv else { return };
                        let pos_ms = (mpv.get_double("time-pos").unwrap_or(0.0) * 1000.0) as i64;

                        match st.marking_start.get() {
                            None => {
                                st.marking_start.set(Some(pos_ms));
                                w.set_frag_marking(true);
                            }
                            Some(start_ms) => {
                                st.marking_start.set(None);
                                w.set_frag_marking(false);
                                let end_ms = pos_ms;
                                let invalid = end_ms <= start_ms
                                    || frag_overlaps(&st.fragments.borrow(), start_ms, end_ms, -1);
                                if invalid {
                                    w.set_frag_error_text("Invalid range: end is before start, or it overlaps another fragment.".into());
                                    w.set_frag_error_open(true);
                                    return;
                                }
                                st.editing_id.set(-1);
                                w.set_frag_editor_heading("New fragment".into());
                                w.set_frag_editor_title_text("".into());
                                w.set_frag_editor_start_text(fmt_mmss(start_ms).into());
                                w.set_frag_editor_end_text(fmt_mmss(end_ms).into());
                                w.set_frag_editor_open(true);
                            }
                        }
                    });
                }
                // Сохранение оверлея метаданных: и для создания, и для редактирования
                // (поля start/end тоже редактируемые — валидация здесь же, повторно).
                {
                    let weak = w.as_weak();
                    let st = st.clone();
                    let app = Rc::downgrade(app);
                    w.on_frag_editor_save(move || {
                        let (Some(w), Some(app)) = (weak.upgrade(), app.upgrade()) else {
                            return;
                        };
                        let editing_id = st.editing_id.get();
                        w.set_frag_editor_open(false);

                        let start_ms = parse_mmss(&w.get_frag_editor_start_text());
                        let end_ms = parse_mmss(&w.get_frag_editor_end_text());
                        let (start_ms, end_ms) = match (start_ms, end_ms) {
                            (Some(s), Some(e)) => (s, e),
                            _ => {
                                w.set_frag_error_text("Invalid time format. Use m:ss.".into());
                                w.set_frag_error_open(true);
                                return;
                            }
                        };
                        let invalid = end_ms <= start_ms
                            || frag_overlaps(&st.fragments.borrow(), start_ms, end_ms, editing_id);
                        if invalid {
                            w.set_frag_error_text("Invalid range: end is before start, or it overlaps another fragment.".into());
                            w.set_frag_error_open(true);
                            return;
                        }

                        let id = st.current.get();
                        let title = w.get_frag_editor_title_text().to_string();
                        let result = if editing_id < 0 {
                            app.db
                                .create_fragment(id, start_ms, end_ms, &title, now_unix())
                                .map(|_| ())
                        } else {
                            app.db.update_fragment(editing_id, start_ms, end_ms, &title)
                        };
                        if let Err(e) = result {
                            log::error!("Сохранение фрагмента: {e}");
                        }
                        rebuild_fragments(&w, &st, &app, id);
                    });
                }
                {
                    let weak = w.as_weak();
                    w.on_frag_editor_cancel(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_frag_editor_open(false);
                        }
                    });
                }
                {
                    let weak = w.as_weak();
                    w.on_frag_error_ok(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_frag_error_open(false);
                        }
                    });
                }
                // ПКМ-меню сегмента: «Редактировать» — открыть оверлей с текущими значениями.
                {
                    let weak = w.as_weak();
                    let st = st.clone();
                    w.on_frag_edit(move |id| {
                        let Some(w) = weak.upgrade() else { return };
                        let id = id as i64;
                        let Some(f) = st
                            .fragments
                            .borrow()
                            .iter()
                            .find(|f| f.id == id)
                            .map(|f| (f.start_ms, f.end_ms, f.title.clone()))
                        else {
                            return;
                        };
                        // Открытие редактора отменяет незавершённую разметку кнопкой.
                        st.marking_start.set(None);
                        w.set_frag_marking(false);

                        st.editing_id.set(id);
                        w.set_frag_editor_heading("Edit fragment".into());
                        w.set_frag_editor_title_text(f.2.into());
                        w.set_frag_editor_start_text(fmt_mmss(f.0).into());
                        w.set_frag_editor_end_text(fmt_mmss(f.1).into());
                        w.set_frag_editor_open(true);
                    });
                }
                // ПКМ-меню сегмента: «Удалить» — подтверждение, затем удаление.
                {
                    let weak = w.as_weak();
                    let st = st.clone();
                    w.on_frag_delete_request(move |id| {
                        st.delete_target.set(id as i64);
                        if let Some(w) = weak.upgrade() {
                            w.set_frag_delete_confirm_open(true);
                        }
                    });
                }
                {
                    let weak = w.as_weak();
                    let st = st.clone();
                    let app = Rc::downgrade(app);
                    w.on_frag_delete_confirm_ok(move || {
                        let Some(app) = app.upgrade() else { return };
                        if let Err(e) = app.db.delete_fragment(st.delete_target.get()) {
                            log::error!("Удаление фрагмента: {e}");
                        }
                        if let Some(w) = weak.upgrade() {
                            w.set_frag_delete_confirm_open(false);
                            rebuild_fragments(&w, &st, &app, st.current.get());
                        }
                    });
                }
                {
                    let weak = w.as_weak();
                    w.on_frag_delete_confirm_cancel(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_frag_delete_confirm_open(false);
                        }
                    });
                }

                // Таймер плеера: тянем позицию/длительность/паузу из mpv в UI.
                // Состояние держим слабо: таймер живёт внутри самого состояния.
                {
                    let weak = w.as_weak();
                    let st_weak = Rc::downgrade(st);
                    st.timer.start(
                        slint::TimerMode::Repeated,
                        Duration::from_millis(250),
                        move || {
                            let (Some(w), Some(st)) = (weak.upgrade(), st_weak.upgrade()) else {
                                return;
                            };
                            if st.current.get() < 0 || !w.get_viewer_is_video() {
                                return;
                            }
                            if let Some(mpv) = &st.mpv {
                                let pos = mpv.get_double("time-pos").unwrap_or(0.0);
                                let dur = mpv.get_double("duration").unwrap_or(0.0);
                                // Файл загружен — стартовая позиция больше не нужна.
                                if st.pending_start.get() && dur > 0.0 {
                                    mpv.set_property_str("start", "none");
                                    st.pending_start.set(false);
                                }
                                w.set_viewer_pos(pos as f32);
                                w.set_viewer_duration(dur as f32);
                                w.set_viewer_time_text(
                                    format!("{} / {}", fmt_time(pos), fmt_time(dur)).into(),
                                );
                                if let Some(paused) = mpv.get_flag("pause") {
                                    w.set_viewer_paused(paused);
                                }
                                // Кнопки разметки фрагмента недоступны, пока позиция плеера
                                // внутри уже существующего фрагмента (нельзя начинать/
                                // заканчивать новый прямо посреди чужого диапазона).
                                let pos_ms = (pos * 1000.0) as i64;
                                let inside = st
                                    .fragments
                                    .borrow()
                                    .iter()
                                    .any(|f| f.start_ms <= pos_ms && pos_ms < f.end_ms);
                                w.set_frag_buttons_disabled(inside);
                            }
                            // Авто-скрытие контролов в полноэкранном режиме.
                            if st.fullscreen.get() {
                                let idle =
                                    st.last_activity.get().elapsed() > Duration::from_millis(2500);
                                if idle && w.get_viewer_controls_visible() {
                                    w.set_viewer_controls_visible(false);
                                }
                            } else if !w.get_viewer_controls_visible() {
                                w.set_viewer_controls_visible(true);
                            }
                        },
                    );
                }
            }
        }
    };
}

viewer_impl!(MainWindow, main_viewer);
viewer_impl!(PlayerWindow, player_viewer);
