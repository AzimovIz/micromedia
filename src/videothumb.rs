//! Генерация миниатюры видео через mpv **software render API** — без GL и окна,
//! пригодно для фонового потока.
//!
//! Поток: отдельный headless-инстанс mpv (vo=libmpv, софт-декод) → загрузка файла
//! с seek на процент → ждём MPV_EVENT_PLAYBACK_RESTART (кадр готов) → рендерим кадр
//! в буфер в RAM (формат rgb0) сразу нужного размера → сохраняем JPEG (атомарно).

use libloading::{Library, Symbol};
use std::ffi::{c_char, c_int, c_void, CString};
use std::path::Path;
use std::time::{Duration, Instant};

use crate::mpv::{MpvHandle, MpvRenderContext};

// render.h
const API_TYPE: c_int = 1;
const SW_SIZE: c_int = 17;
const SW_FORMAT: c_int = 18;
const SW_STRIDE: c_int = 19;
const SW_POINTER: c_int = 20;
const INVALID: c_int = 0;

// client.h
const FORMAT_INT64: c_int = 4;
const FORMAT_DOUBLE: c_int = 5;
const EV_SHUTDOWN: c_int = 1;
const EV_END_FILE: c_int = 7;
const EV_FILE_LOADED: c_int = 8;
const EV_PLAYBACK_RESTART: c_int = 21;

#[repr(C)]
struct MpvEvent {
    event_id: c_int,
    error: c_int,
    reply_userdata: u64,
    data: *mut c_void,
}

#[repr(C)]
struct RenderParam {
    type_: c_int,
    data: *mut c_void,
}

type FnCreate = unsafe extern "C" fn() -> *mut MpvHandle;
type FnInit = unsafe extern "C" fn(*mut MpvHandle) -> c_int;
type FnDestroy = unsafe extern "C" fn(*mut MpvHandle);
type FnSetOpt = unsafe extern "C" fn(*mut MpvHandle, *const c_char, *const c_char) -> c_int;
type FnCommand = unsafe extern "C" fn(*mut MpvHandle, *const *const c_char) -> c_int;
type FnGetProp = unsafe extern "C" fn(*mut MpvHandle, *const c_char, c_int, *mut c_void) -> c_int;
type FnWaitEvent = unsafe extern "C" fn(*mut MpvHandle, f64) -> *mut MpvEvent;
type FnRCreate =
    unsafe extern "C" fn(*mut *mut MpvRenderContext, *mut MpvHandle, *mut RenderParam) -> c_int;
type FnRRender = unsafe extern "C" fn(*mut MpvRenderContext, *mut RenderParam) -> c_int;
type FnRFree = unsafe extern "C" fn(*mut MpvRenderContext);

unsafe fn s<T: Copy>(lib: &Library, n: &[u8]) -> Result<T, String> {
    let sym: Symbol<T> = unsafe { lib.get(n) }.map_err(|e| e.to_string())?;
    Ok(*sym)
}

/// Делает миниатюру `out_jpg` (вписанную в `max`×`max`) из кадра `video` на
/// позиции `seek_percent`%. Возвращает длительность видео (сек), если удалось.
pub fn generate(
    video: &Path,
    out_jpg: &Path,
    seek_percent: u32,
    max: u32,
) -> Result<Option<f64>, String> {
    let lib = crate::mpv::open_libmpv()?;

    unsafe {
        let create: FnCreate = s(&lib, b"mpv_create\0")?;
        let init: FnInit = s(&lib, b"mpv_initialize\0")?;
        let destroy: FnDestroy = s(&lib, b"mpv_terminate_destroy\0")?;
        let set_opt: FnSetOpt = s(&lib, b"mpv_set_option_string\0")?;
        let command: FnCommand = s(&lib, b"mpv_command\0")?;
        let get_prop: FnGetProp = s(&lib, b"mpv_get_property\0")?;
        let wait_event: FnWaitEvent = s(&lib, b"mpv_wait_event\0")?;
        let r_create: FnRCreate = s(&lib, b"mpv_render_context_create\0")?;
        let r_render: FnRRender = s(&lib, b"mpv_render_context_render\0")?;
        let r_free: FnRFree = s(&lib, b"mpv_render_context_free\0")?;

        let h = create();
        if h.is_null() {
            return Err("mpv_create вернул NULL".into());
        }

        let setopt = |k: &str, v: &str| {
            if let (Ok(ck), Ok(cv)) = (CString::new(k), CString::new(v)) {
                set_opt(h, ck.as_ptr(), cv.as_ptr());
            }
        };
        setopt("vo", "libmpv");
        setopt("hwdec", "no");
        setopt("audio", "no");
        setopt("terminal", "no");
        setopt("msg-level", "all=no");
        setopt("pause", "yes");
        setopt("hr-seek", "yes");
        setopt("start", &format!("{seek_percent}%"));

        if init(h) < 0 {
            destroy(h);
            return Err("mpv_initialize".into());
        }

        // SW render context.
        let mut cparams = [
            RenderParam {
                type_: API_TYPE,
                data: c"sw".as_ptr() as *mut c_void,
            },
            RenderParam {
                type_: INVALID,
                data: std::ptr::null_mut(),
            },
        ];
        let mut ctx: *mut MpvRenderContext = std::ptr::null_mut();
        if r_create(&mut ctx, h, cparams.as_mut_ptr()) < 0 {
            destroy(h);
            return Err("render_context_create(sw)".into());
        }

        // loadfile.
        let cmd = CString::new("loadfile").unwrap();
        let file =
            CString::new(video.to_string_lossy().as_ref()).map_err(|_| "путь содержит NUL")?;
        let args: [*const c_char; 3] = [cmd.as_ptr(), file.as_ptr(), std::ptr::null()];
        if command(h, args.as_ptr()) < 0 {
            r_free(ctx);
            destroy(h);
            return Err("loadfile".into());
        }

        // Ждём готовности кадра на позиции seek.
        let deadline = Instant::now() + Duration::from_secs(15);
        let mut ready = false;
        while Instant::now() < deadline {
            let ev = wait_event(h, 0.2);
            if ev.is_null() {
                continue;
            }
            match (*ev).event_id {
                EV_PLAYBACK_RESTART => {
                    ready = true;
                    break;
                }
                EV_END_FILE | EV_SHUTDOWN => break,
                _ => {}
            }
        }
        if !ready {
            r_free(ctx);
            destroy(h);
            return Err("кадр не готов (timeout/end-file)".into());
        }

        // Длительность заодно (пока mpv-хэндл жив).
        let dur = get_f64(get_prop, h, "duration");

        // Целевой размер: вписываем видео в max×max.
        let vw = get_i64(get_prop, h, "dwidth").unwrap_or(max as i64).max(1) as u32;
        let vh = get_i64(get_prop, h, "dheight").unwrap_or(max as i64).max(1) as u32;
        let (tw, th) = fit(vw, vh, max);

        // rgb0 = 4 байта/пиксель; stride кратен 64 для SIMD.
        let stride = align64(tw as usize * 4);
        let mut buf = vec![0u8; stride * th as usize];
        let mut size = [tw as c_int, th as c_int];
        let mut stride_v: usize = stride;
        let fmt = c"rgb0";
        let mut rparams = [
            RenderParam {
                type_: SW_SIZE,
                data: size.as_mut_ptr() as *mut c_void,
            },
            RenderParam {
                type_: SW_FORMAT,
                data: fmt.as_ptr() as *mut c_void,
            },
            RenderParam {
                type_: SW_STRIDE,
                data: &mut stride_v as *mut usize as *mut c_void,
            },
            RenderParam {
                type_: SW_POINTER,
                data: buf.as_mut_ptr() as *mut c_void,
            },
            RenderParam {
                type_: INVALID,
                data: std::ptr::null_mut(),
            },
        ];
        let rr = r_render(ctx, rparams.as_mut_ptr());

        r_free(ctx);
        destroy(h);

        if rr < 0 {
            return Err(format!("sw render -> {rr}"));
        }

        // rgb0 -> RgbImage (отбрасываем 4-й байт).
        let mut rgb = image::RgbImage::new(tw, th);
        for y in 0..th as usize {
            let row = &buf[y * stride..];
            for x in 0..tw as usize {
                let p = &row[x * 4..x * 4 + 3];
                rgb.put_pixel(x as u32, y as u32, image::Rgb([p[0], p[1], p[2]]));
            }
        }

        // Атомарное сохранение.
        let tmp = out_jpg.with_extension("jpg.tmp");
        {
            let mut f = std::fs::File::create(&tmp).map_err(|e| e.to_string())?;
            image::DynamicImage::ImageRgb8(rgb)
                .write_to(&mut f, image::ImageFormat::Jpeg)
                .map_err(|e| e.to_string())?;
        }
        std::fs::rename(&tmp, out_jpg).map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            e.to_string()
        })?;
        Ok(dur)
    }
}

/// Быстрое определение длительности видео (сек) без рендера кадра.
pub fn probe_duration(video: &Path) -> Option<f64> {
    let lib = crate::mpv::open_libmpv().ok()?;
    unsafe {
        let create: FnCreate = s(&lib, b"mpv_create\0").ok()?;
        let init: FnInit = s(&lib, b"mpv_initialize\0").ok()?;
        let destroy: FnDestroy = s(&lib, b"mpv_terminate_destroy\0").ok()?;
        let set_opt: FnSetOpt = s(&lib, b"mpv_set_option_string\0").ok()?;
        let command: FnCommand = s(&lib, b"mpv_command\0").ok()?;
        let get_prop: FnGetProp = s(&lib, b"mpv_get_property\0").ok()?;
        let wait_event: FnWaitEvent = s(&lib, b"mpv_wait_event\0").ok()?;

        let h = create();
        if h.is_null() {
            return None;
        }
        let setopt = |k: &str, v: &str| {
            if let (Ok(ck), Ok(cv)) = (CString::new(k), CString::new(v)) {
                set_opt(h, ck.as_ptr(), cv.as_ptr());
            }
        };
        setopt("vo", "null");
        setopt("audio", "no");
        setopt("terminal", "no");
        setopt("msg-level", "all=no");
        setopt("pause", "yes");
        if init(h) < 0 {
            destroy(h);
            return None;
        }
        let cmd = CString::new("loadfile").ok()?;
        let file = CString::new(video.to_string_lossy().as_ref()).ok()?;
        let args: [*const c_char; 3] = [cmd.as_ptr(), file.as_ptr(), std::ptr::null()];
        if command(h, args.as_ptr()) < 0 {
            destroy(h);
            return None;
        }

        let deadline = Instant::now() + Duration::from_secs(10);
        let mut dur = None;
        while Instant::now() < deadline {
            let ev = wait_event(h, 0.2);
            if ev.is_null() {
                continue;
            }
            match (*ev).event_id {
                EV_FILE_LOADED => {
                    dur = get_f64(get_prop, h, "duration");
                    break;
                }
                EV_END_FILE | EV_SHUTDOWN => break,
                _ => {}
            }
        }
        destroy(h);
        dur
    }
}

unsafe fn get_i64(f: FnGetProp, h: *mut MpvHandle, name: &str) -> Option<i64> {
    let n = CString::new(name).ok()?;
    let mut v: i64 = 0;
    if f(h, n.as_ptr(), FORMAT_INT64, &mut v as *mut i64 as *mut c_void) < 0 {
        None
    } else {
        Some(v)
    }
}

unsafe fn get_f64(f: FnGetProp, h: *mut MpvHandle, name: &str) -> Option<f64> {
    let n = CString::new(name).ok()?;
    let mut v: f64 = 0.0;
    if f(h, n.as_ptr(), FORMAT_DOUBLE, &mut v as *mut f64 as *mut c_void) < 0 || v <= 0.0 {
        None
    } else {
        Some(v)
    }
}

/// Вписывает (w,h) в квадрат max×max с сохранением пропорций.
fn fit(w: u32, h: u32, max: u32) -> (u32, u32) {
    if w >= h {
        let nh = ((h as f64 / w as f64) * max as f64).round().max(1.0) as u32;
        (max, nh)
    } else {
        let nw = ((w as f64 / h as f64) * max as f64).round().max(1.0) as u32;
        (nw, max)
    }
}

fn align64(n: usize) -> usize {
    (n + 63) & !63
}
