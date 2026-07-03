// Milestone B: видео через mpv render API под UI Slint.
//
// Идея: mpv рисует кадр в дефолтный фреймбуфер в колбэке BeforeRendering
// (подложка), а Slint поверх рисует свой UI с прозрачным фоном.

mod mpv;

use std::cell::Cell;
use std::rc::Rc;

use slint::{ComponentHandle, GraphicsAPI, RenderingState};

use mpv::{Mpv, MpvRenderContext};

slint::include_modules!();

/// Колбэк «у mpv готов новый кадр». Зовётся из потока mpv — просто просим
/// event loop Slint перерисоваться (там уже вызовется BeforeRendering).
unsafe extern "C" fn on_mpv_update(ctx: *mut std::ffi::c_void) {
    let weak = &*(ctx as *const slint::Weak<MainWindow>);
    let weak = weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(win) = weak.upgrade() {
            win.window().request_redraw();
        }
    });
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    // Форсируем winit + femtovg (GL) — на этом бэкенде работает наша подложка.
    if std::env::var_os("SLINT_BACKEND").is_none() {
        std::env::set_var("SLINT_BACKEND", "winit-femtovg");
    }

    let file = std::env::args().nth(1);
    if file.is_none() {
        log::warn!("Не передан путь к видео. Запуск: micromedia <файл>");
    }

    // Грузим libmpv и поднимаем плеер ДО окна.
    let mpv = match Mpv::load() {
        Ok(m) => Rc::new(m),
        Err(e) => {
            eprintln!("Ошибка mpv: {e}");
            return Err(e.into());
        }
    };

    let window = MainWindow::new()?;
    window.set_status(
        match &file {
            Some(f) => format!("mpv render API — {f}"),
            None => "mpv загружена. Передай путь к видео аргументом.".into(),
        }
        .into(),
    );

    // Пауза по кнопке из UI.
    {
        let mpv = mpv.clone();
        window.on_toggle_play(move || mpv.toggle_pause());
    }

    // Указатель на render-контекст живёт между колбэками notifier.
    let render_ctx: Rc<Cell<*mut MpvRenderContext>> = Rc::new(Cell::new(std::ptr::null_mut()));
    // Файл загружаем один раз — после создания render-контекста.
    let pending_file: Rc<Cell<Option<String>>> = Rc::new(Cell::new(file));

    // Утёкший Weak как контекст для C-колбэка mpv (живёт всю жизнь программы).
    let weak_box: *mut slint::Weak<MainWindow> = Box::into_raw(Box::new(window.as_weak()));

    let notifier_mpv = mpv.clone();
    let notifier_ctx = render_ctx.clone();
    let notifier_file = pending_file.clone();
    let weak_for_redraw = window.as_weak();

    let res = window.window().set_rendering_notifier(move |state, api| match state {
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
                        if let Some(path) = notifier_file.take() {
                            if let Err(e) = notifier_mpv.loadfile(&path) {
                                log::error!("{e}");
                            }
                        }
                        log::info!("mpv render-контекст создан");
                    }
                    Err(code) => log::error!("mpv_render_context_create -> {code}"),
                }
            }
        }
        RenderingState::BeforeRendering => {
            let ctx = notifier_ctx.get();
            if !ctx.is_null() {
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
        eprintln!(
            "Не удалось поставить rendering notifier: {e:?}. \
             Нужен GL-бэкенд (SLINT_BACKEND=winit-femtovg)."
        );
        return Err(Box::new(e));
    }

    window.run()?;
    Ok(())
}
