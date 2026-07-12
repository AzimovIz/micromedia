//! Выбор GL-пути: аппаратный или программный (Mesa llvmpipe).
//!
//! На части систем аппаратный GL-стек нерабочий: Mesa не находит DRI-драйвер
//! («failed to get driver name for fd -1»), откатывается на zink (GL поверх
//! Vulkan), не находит и Vulkan — и отдаёт формально живой, но нерабочий
//! контекст. Окно на нём рисуется, а рендер mpv в FBO даёт чёрный кадр.
//!
//! Лечится `LIBGL_ALWAYS_SOFTWARE=1` — Mesa считает GL на CPU. Переменная
//! читается при инициализации драйвера, поэтому выставлять её надо ДО создания
//! окна; если поломка вскрылась уже после (по строке GL_RENDERER) — процесс
//! перезапускает себя.

use std::ffi::{c_char, c_uint, c_void, CStr};

const GL_VENDOR: c_uint = 0x1F00;
const GL_RENDERER: c_uint = 0x1F01;
const GL_VERSION: c_uint = 0x1F02;

/// Явный оверрайд: `1` — всегда программный GL, `0` — никогда (даже если
/// автодетект считает иначе).
const ENV_OVERRIDE: &str = "MICROMEDIA_SOFTWARE_GL";
/// Метка «этот процесс уже перезапущен» — страховка от бесконечного рестарта.
const ENV_RESTARTED: &str = "MICROMEDIA_GL_RESTARTED";
const ENV_MESA_SOFTWARE: &str = "LIBGL_ALWAYS_SOFTWARE";

/// Решение о том, как стартовать.
pub struct GlMode {
    /// Программный GL включён (нами или пользователем).
    pub software: bool,
    /// Перезапуск уже был — второй раз не пытаемся.
    pub restarted: bool,
}

impl GlMode {
    /// Можно ли ещё раз перезапуститься в программный режим.
    pub fn can_fall_back(&self) -> bool {
        !self.software && !self.restarted && !forbidden_by_user()
    }
}

/// Вызывать в начале `main()`, до создания окна.
///
/// Приоритет: явный оверрайд пользователя → выбор, сохранённый в конфиге →
/// автодетект (нет DRM-устройства — аппаратного GL не будет).
pub fn setup(config_software: bool) -> GlMode {
    let restarted = std::env::var_os(ENV_RESTARTED).is_some();

    // Пользователь сам выставил LIBGL_ALWAYS_SOFTWARE — уважаем и не трогаем.
    if std::env::var_os(ENV_MESA_SOFTWARE).is_some() {
        log::info!("GL: программный режим (задан LIBGL_ALWAYS_SOFTWARE извне)");
        return GlMode {
            software: true,
            restarted,
        };
    }

    if forbidden_by_user() {
        log::info!("GL: аппаратный режим форсирован ({ENV_OVERRIDE}=0)");
        return GlMode {
            software: false,
            restarted,
        };
    }

    let reason = if requested_by_user() {
        Some(format!("{ENV_OVERRIDE}=1"))
    } else if config_software {
        Some("выбран при прошлом запуске".to_string())
    } else if !has_drm_device() {
        Some("нет DRM-устройства (/dev/dri)".to_string())
    } else {
        None
    };

    match reason {
        Some(why) => {
            log::info!("GL: включаю программный рендеринг — {why}");
            enable_software();
            GlMode {
                software: true,
                restarted,
            }
        }
        None => GlMode {
            software: false,
            restarted,
        },
    }
}

fn requested_by_user() -> bool {
    matches!(std::env::var(ENV_OVERRIDE).as_deref(), Ok("1"))
}

fn forbidden_by_user() -> bool {
    matches!(std::env::var(ENV_OVERRIDE).as_deref(), Ok("0"))
}

fn enable_software() {
    std::env::set_var(ENV_MESA_SOFTWARE, "1");
    // Дублирующий рычаг Mesa: часть драйверов слушает именно его.
    std::env::set_var("GALLIUM_DRIVER", "llvmpipe");
}

/// Есть ли хоть одно DRM-устройство. Их отсутствие означает, что аппаратного
/// GL не будет ни при каких обстоятельствах (VM без passthrough, контейнер,
/// удалённая сессия) — тогда сразу идём программным путём.
#[cfg(target_os = "linux")]
fn has_drm_device() -> bool {
    let Ok(entries) = std::fs::read_dir("/dev/dri") else {
        return false;
    };
    entries.flatten().any(|e| {
        let name = e.file_name();
        let name = name.to_string_lossy();
        name.starts_with("renderD") || name.starts_with("card")
    })
}

#[cfg(not(target_os = "linux"))]
fn has_drm_device() -> bool {
    true // на Windows/macOS проверка бессмысленна
}

/// Запускает копию себя с программным GL и теми же аргументами. Вызывать после
/// выхода из цикла событий: старое окно должно быть закрыто, иначе пользователь
/// увидит два.
pub fn restart_software() -> std::io::Result<()> {
    let exe = std::env::current_exe()?;
    let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    log::info!("GL: перезапуск в программном режиме — {}", exe.display());
    std::process::Command::new(exe)
        .args(args)
        .env(ENV_MESA_SOFTWARE, "1")
        .env("GALLIUM_DRIVER", "llvmpipe")
        .env(ENV_RESTARTED, "1")
        .spawn()?;
    Ok(())
}

/// Читает строки GL уже созданного контекста. Вызывать только там, где контекст
/// активен (в rendering notifier Slint).
///
/// # Safety
/// Требует активного GL-контекста в текущем потоке.
pub unsafe fn probe(get_proc: &dyn Fn(&CStr) -> *const c_void) -> Option<GlInfo> {
    let p = get_proc(c"glGetString");
    if p.is_null() {
        return None;
    }
    let gl_get_string: unsafe extern "C" fn(c_uint) -> *const c_char = std::mem::transmute(p);

    let read = |name: c_uint| -> String {
        let s = gl_get_string(name);
        if s.is_null() {
            String::new()
        } else {
            CStr::from_ptr(s).to_string_lossy().into_owned()
        }
    };

    Some(GlInfo {
        vendor: read(GL_VENDOR),
        renderer: read(GL_RENDERER),
        version: read(GL_VERSION),
    })
}

pub struct GlInfo {
    pub vendor: String,
    pub renderer: String,
    pub version: String,
}

impl GlInfo {
    /// Похоже ли на аварийный контекст, на котором рендер mpv даст чёрный кадр.
    ///
    /// `zink` — признак того, что Mesa не смогла поднять родной драйвер и
    /// откатилась на GL-поверх-Vulkan; в связке со сломанным Vulkan это и есть
    /// наш случай. Пустая строка — контекст, который не отвечает на glGetString.
    /// llvmpipe/softpipe/swrast НЕ считаем поломкой: это уже софтверный рендер,
    /// он работает.
    pub fn looks_broken(&self) -> bool {
        let r = self.renderer.to_ascii_lowercase();
        r.is_empty() || r.contains("zink")
    }
}
