//! Динамическая загрузка libmpv и минимальная обёртка над mpv render API (OpenGL).
//!
//! Библиотека грузится в рантайме из `appdata/libs/` (или соседних папок) в
//! зависимости от ОС — никакой линковки libmpv на этапе сборки, чтобы бинарник
//! оставался портативным («работает с флешки»).
//!
//! Осознанно ручной FFI: готовые крейты (libmpv2 и т.п.) линкуются во время
//! сборки и требуют libmpv + заголовки, что ломает модель «грузим из appdata».

use libloading::{Library, Symbol};
use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::path::{Path, PathBuf};
use std::ptr;

// --- Непрозрачные типы mpv ---
#[repr(C)]
pub struct MpvHandle {
    _private: [u8; 0],
}
#[repr(C)]
pub struct MpvRenderContext {
    _private: [u8; 0],
}

// --- Константы render API (render.h / render_gl.h) ---
const MPV_RENDER_PARAM_INVALID: c_int = 0;
const MPV_RENDER_PARAM_API_TYPE: c_int = 1;
const MPV_RENDER_PARAM_OPENGL_INIT_PARAMS: c_int = 2;
const MPV_RENDER_PARAM_OPENGL_FBO: c_int = 3;
const MPV_RENDER_PARAM_FLIP_Y: c_int = 4;

#[repr(C)]
struct MpvRenderParam {
    type_: c_int,
    data: *mut c_void,
}

type GetProcAddress = unsafe extern "C" fn(*mut c_void, *const c_char) -> *mut c_void;

#[repr(C)]
struct MpvOpenGLInitParams {
    get_proc_address: Option<GetProcAddress>,
    get_proc_address_ctx: *mut c_void,
}

#[repr(C)]
struct MpvOpenGLFbo {
    fbo: c_int,
    w: c_int,
    h: c_int,
    internal_format: c_int,
}

// --- Сигнатуры нужных функций mpv ---
type FnCreate = unsafe extern "C" fn() -> *mut MpvHandle;
type FnInitialize = unsafe extern "C" fn(*mut MpvHandle) -> c_int;
type FnTerminateDestroy = unsafe extern "C" fn(*mut MpvHandle);
type FnSetOptionString = unsafe extern "C" fn(*mut MpvHandle, *const c_char, *const c_char) -> c_int;
type FnSetPropertyString = unsafe extern "C" fn(*mut MpvHandle, *const c_char, *const c_char) -> c_int;
type FnCommand = unsafe extern "C" fn(*mut MpvHandle, *const *const c_char) -> c_int;

type FnRenderContextCreate =
    unsafe extern "C" fn(*mut *mut MpvRenderContext, *mut MpvHandle, *mut MpvRenderParam) -> c_int;
type FnRenderContextRender = unsafe extern "C" fn(*mut MpvRenderContext, *mut MpvRenderParam) -> c_int;
type FnRenderContextSetUpdateCallback = unsafe extern "C" fn(
    *mut MpvRenderContext,
    Option<unsafe extern "C" fn(*mut c_void)>,
    *mut c_void,
);
type FnRenderContextFree = unsafe extern "C" fn(*mut MpvRenderContext);

/// Трамплин: мост между C-колбэком get_proc_address и замыканием Slint.
/// `ctx` указывает на `&dyn Fn(&CStr) -> *const c_void`, живущее на стеке
/// вызывающего кода на время `mpv_render_context_create` (mpv вызывает
/// get_proc_address только синхронно при создании контекста).
unsafe extern "C" fn get_proc_trampoline(ctx: *mut c_void, name: *const c_char) -> *mut c_void {
    let f = *(ctx as *const &dyn Fn(&CStr) -> *const c_void);
    f(CStr::from_ptr(name)) as *mut c_void
}

#[allow(dead_code)] // create/set_property_string пригодятся в следующих милстоунах
pub struct Mpv {
    // Library должна пережить все указатели на функции ниже — держим её здесь.
    _lib: Library,
    handle: *mut MpvHandle,

    create: FnCreate,
    initialize: FnInitialize,
    terminate_destroy: FnTerminateDestroy,
    set_option_string: FnSetOptionString,
    set_property_string: FnSetPropertyString,
    command: FnCommand,

    render_context_create: FnRenderContextCreate,
    render_context_render: FnRenderContextRender,
    render_context_set_update_callback: FnRenderContextSetUpdateCallback,
    render_context_free: FnRenderContextFree,
}

impl Mpv {
    /// Находит и загружает libmpv, создаёт и инициализирует mpv-хэндл.
    pub fn load() -> Result<Self, String> {
        let path = find_libmpv().ok_or_else(|| {
            "libmpv не найдена. Положи libmpv.so (Linux) / libmpv-2.dll (Windows) \
             в appdata/libs/ рядом с бинарником или в ./libs/."
                .to_string()
        })?;

        log::info!("Загружаю libmpv: {}", path.display());
        let lib = unsafe { Library::new(&path) }
            .map_err(|e| format!("не удалось загрузить {}: {e}", path.display()))?;

        unsafe {
            // Достаём все символы; значения fn-указателей копируем из Symbol
            // (fn-указатели Copy), а сама Library хранится в структуре и держит
            // их валидными.
            fn sym<T: Copy>(lib: &Library, name: &[u8]) -> Result<T, String> {
                let s: Symbol<T> =
                    unsafe { lib.get(name) }.map_err(|e| {
                        format!("нет символа {}: {e}", String::from_utf8_lossy(name))
                    })?;
                Ok(*s)
            }

            let create: FnCreate = sym(&lib, b"mpv_create\0")?;
            let initialize: FnInitialize = sym(&lib, b"mpv_initialize\0")?;
            let terminate_destroy: FnTerminateDestroy = sym(&lib, b"mpv_terminate_destroy\0")?;
            let set_option_string: FnSetOptionString = sym(&lib, b"mpv_set_option_string\0")?;
            let set_property_string: FnSetPropertyString =
                sym(&lib, b"mpv_set_property_string\0")?;
            let command: FnCommand = sym(&lib, b"mpv_command\0")?;
            let render_context_create: FnRenderContextCreate =
                sym(&lib, b"mpv_render_context_create\0")?;
            let render_context_render: FnRenderContextRender =
                sym(&lib, b"mpv_render_context_render\0")?;
            let render_context_set_update_callback: FnRenderContextSetUpdateCallback =
                sym(&lib, b"mpv_render_context_set_update_callback\0")?;
            let render_context_free: FnRenderContextFree =
                sym(&lib, b"mpv_render_context_free\0")?;

            let handle = create();
            if handle.is_null() {
                return Err("mpv_create вернул NULL".into());
            }

            // Опции до инициализации. vo НЕ трогаем — render-контекст (создаётся
            // позже, в RenderingSetup) сам включит вывод через libmpv.
            let mpv = Mpv {
                _lib: lib,
                handle,
                create,
                initialize,
                terminate_destroy,
                set_option_string,
                set_property_string,
                command,
                render_context_create,
                render_context_render,
                render_context_set_update_callback,
                render_context_free,
            };

            // ОБЯЗАТЕЛЬНО: вывод через render API, иначе mpv откроет своё окно.
            mpv.set_option("vo", "libmpv");
            // Разумные дефолты для встроенного плеера.
            mpv.set_option("terminal", "no");
            mpv.set_option("msg-level", "all=warn");
            mpv.set_option("keep-open", "yes");
            // hwdec: на Windows с нативным WGL прямой интероп D3D11<->GL даёт
            // зелёные артефакты. Безопасный дефолт — без hwdec (софт-декод);
            // можно переопределить через env (например MICROMEDIA_HWDEC=auto-copy,
            // чтобы вернуть аппаратный декод с копированием кадра в RAM).
            let hwdec = std::env::var("MICROMEDIA_HWDEC").unwrap_or_else(|_| "no".to_string());
            mpv.set_option("hwdec", &hwdec);
            // Качественный скейлинг (особенно даунскейл, когда видео больше окна).
            mpv.set_option("scale", "spline36");
            mpv.set_option("cscale", "spline36");
            mpv.set_option("dscale", "mitchell");
            mpv.set_option("correct-downscaling", "yes");
            mpv.set_option("sigmoid-upscaling", "yes");

            let ret = (mpv.initialize)(mpv.handle);
            if ret < 0 {
                return Err(format!("mpv_initialize завершился с кодом {ret}"));
            }

            Ok(mpv)
        }
    }

    fn set_option(&self, name: &str, value: &str) {
        let (n, v) = (cstr(name), cstr(value));
        let ret = unsafe { (self.set_option_string)(self.handle, n.as_ptr(), v.as_ptr()) };
        if ret < 0 {
            log::warn!("mpv set_option {name}={value} -> {ret}");
        }
    }

    /// Создаёт OpenGL render-контекст. Должно вызываться, когда GL-контекст
    /// Slint текущий (внутри rendering notifier, состояние RenderingSetup).
    ///
    /// # Safety
    /// Требует активного OpenGL-контекста в текущем потоке.
    pub unsafe fn create_render_context(
        &self,
        get_proc: &dyn Fn(&CStr) -> *const c_void,
    ) -> Result<*mut MpvRenderContext, i32> {
        let get_proc_ptr =
            &get_proc as *const &dyn Fn(&CStr) -> *const c_void as *mut c_void;

        let mut init = MpvOpenGLInitParams {
            get_proc_address: Some(get_proc_trampoline),
            get_proc_address_ctx: get_proc_ptr,
        };

        let mut params = [
            MpvRenderParam {
                type_: MPV_RENDER_PARAM_API_TYPE,
                data: c"opengl".as_ptr() as *mut c_void,
            },
            MpvRenderParam {
                type_: MPV_RENDER_PARAM_OPENGL_INIT_PARAMS,
                data: &mut init as *mut _ as *mut c_void,
            },
            MpvRenderParam {
                type_: MPV_RENDER_PARAM_INVALID,
                data: ptr::null_mut(),
            },
        ];

        let mut ctx: *mut MpvRenderContext = ptr::null_mut();
        let ret = (self.render_context_create)(&mut ctx, self.handle, params.as_mut_ptr());
        if ret < 0 {
            return Err(ret);
        }
        Ok(ctx)
    }

    /// Устанавливает колбэк «есть новый кадр». Колбэк зовётся из потока mpv.
    ///
    /// # Safety
    /// `cb_ctx` должен пережить render-контекст.
    pub unsafe fn set_update_callback(
        &self,
        ctx: *mut MpvRenderContext,
        cb: unsafe extern "C" fn(*mut c_void),
        cb_ctx: *mut c_void,
    ) {
        (self.render_context_set_update_callback)(ctx, Some(cb), cb_ctx);
    }

    /// Рисует текущий кадр mpv в дефолтный фреймбуфер (fbo 0) заданного размера.
    ///
    /// # Safety
    /// Требует активного OpenGL-контекста Slint (вызывать в BeforeRendering).
    pub unsafe fn render(&self, ctx: *mut MpvRenderContext, width: i32, height: i32) {
        let mut fbo = MpvOpenGLFbo {
            fbo: 0,
            w: width,
            h: height,
            internal_format: 0,
        };
        // Дефолтный фреймбуфер имеет начало координат снизу — переворачиваем,
        // чтобы кадр не был вверх ногами.
        let mut flip: c_int = 1;

        let mut params = [
            MpvRenderParam {
                type_: MPV_RENDER_PARAM_OPENGL_FBO,
                data: &mut fbo as *mut _ as *mut c_void,
            },
            MpvRenderParam {
                type_: MPV_RENDER_PARAM_FLIP_Y,
                data: &mut flip as *mut _ as *mut c_void,
            },
            MpvRenderParam {
                type_: MPV_RENDER_PARAM_INVALID,
                data: ptr::null_mut(),
            },
        ];

        (self.render_context_render)(ctx, params.as_mut_ptr());
    }

    /// # Safety — контекст больше не используется после освобождения.
    pub unsafe fn free_render_context(&self, ctx: *mut MpvRenderContext) {
        (self.render_context_free)(ctx);
    }

    /// Загружает и запускает воспроизведение файла.
    pub fn loadfile(&self, path: &str) -> Result<(), String> {
        let cmd = cstr("loadfile");
        let file = CString::new(path).map_err(|_| "путь содержит NUL".to_string())?;
        let args: [*const c_char; 3] = [cmd.as_ptr(), file.as_ptr(), ptr::null()];
        let ret = unsafe { (self.command)(self.handle, args.as_ptr()) };
        if ret < 0 {
            return Err(format!("loadfile('{path}') -> {ret}"));
        }
        Ok(())
    }

    /// Переключение паузы (для проверки командного канала).
    pub fn toggle_pause(&self) {
        // читаем текущее значение? проще — командой cycle
        let cmd = cstr("cycle");
        let prop = cstr("pause");
        let args: [*const c_char; 3] = [cmd.as_ptr(), prop.as_ptr(), ptr::null()];
        unsafe {
            (self.command)(self.handle, args.as_ptr());
        }
    }
}

impl Drop for Mpv {
    fn drop(&mut self) {
        unsafe {
            (self.terminate_destroy)(self.handle);
        }
    }
}

fn cstr(s: &str) -> CString {
    CString::new(s).expect("строка без NUL")
}

#[cfg(target_os = "windows")]
const LIB_NAMES: &[&str] = &["libmpv-2.dll", "mpv-2.dll", "libmpv.dll", "mpv-1.dll"];
#[cfg(not(target_os = "windows"))]
const LIB_NAMES: &[&str] = &["libmpv.so", "libmpv.so.2", "libmpv.so.1"];

/// Ищет libmpv в приоритетном порядке каталогов рядом с бинарником и в CWD.
fn find_libmpv() -> Option<PathBuf> {
    // Явный оверрайд для отладки.
    if let Ok(p) = std::env::var("MICROMEDIA_MPV") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Some(p);
        }
    }

    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            dirs.push(dir.join("appdata").join("libs"));
            dirs.push(dir.join("libs"));
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        dirs.push(cwd.join("appdata").join("libs"));
        dirs.push(cwd.join("libs"));
    }

    for dir in &dirs {
        if let Some(hit) = first_existing(dir) {
            return Some(hit);
        }
    }
    None
}

fn first_existing(dir: &Path) -> Option<PathBuf> {
    for name in LIB_NAMES {
        let p = dir.join(name);
        if p.is_file() {
            return Some(p);
        }
    }
    None
}
