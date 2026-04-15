use std::ffi::CString;
use std::os::raw::{c_char, c_int, c_void};
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::{Arc, Mutex};

use libloading::{Library, Symbol};

use crate::config;

// --- mpv C API types ---

type MpvHandle = *mut c_void;

const MPV_FORMAT_FLAG: c_int = 3;
const MPV_FORMAT_DOUBLE: c_int = 5;

const MPV_EVENT_NONE: c_int = 0;
const MPV_EVENT_SHUTDOWN: c_int = 1;
const MPV_EVENT_END_FILE: c_int = 7;
const MPV_EVENT_PROPERTY_CHANGE: c_int = 22;

const MPV_RENDER_PARAM_API_TYPE: c_int = 1;
const MPV_RENDER_PARAM_SW_SIZE: c_int = 17;
const MPV_RENDER_PARAM_SW_FORMAT: c_int = 18;
const MPV_RENDER_PARAM_SW_STRIDE: c_int = 19;
const MPV_RENDER_PARAM_SW_POINTER: c_int = 20;

#[repr(C)]
struct MpvEvent {
    event_id: c_int,
    error: c_int,
    reply_userdata: u64,
    data: *mut c_void,
}

#[repr(C)]
struct MpvRenderParam {
    param_type: c_int,
    data: *mut c_void,
}

#[repr(C)]
struct MpvEventProperty {
    name: *const c_char,
    format: c_int,
    data: *mut c_void,
}

type MpvRenderContext = *mut c_void;

// --- Dynamic function signatures ---

type FnMpvCreate = unsafe extern "C" fn() -> MpvHandle;
type FnMpvInitialize = unsafe extern "C" fn(MpvHandle) -> c_int;
type FnMpvTerminateDestroy = unsafe extern "C" fn(MpvHandle);
type FnMpvCommand = unsafe extern "C" fn(MpvHandle, *const *const c_char) -> c_int;
type FnMpvSetOptionString = unsafe extern "C" fn(MpvHandle, *const c_char, *const c_char) -> c_int;
type FnMpvSetProperty = unsafe extern "C" fn(MpvHandle, *const c_char, c_int, *const c_void) -> c_int;
type FnMpvGetPropertyDouble = unsafe extern "C" fn(MpvHandle, *const c_char, c_int, *mut f64) -> c_int;
type FnMpvGetPropertyFlag = unsafe extern "C" fn(MpvHandle, *const c_char, c_int, *mut c_int) -> c_int;
type FnMpvObserveProperty = unsafe extern "C" fn(MpvHandle, u64, *const c_char, c_int) -> c_int;
type FnMpvWaitEvent = unsafe extern "C" fn(MpvHandle, f64) -> *const MpvEvent;
type FnMpvRenderContextCreate = unsafe extern "C" fn(*mut MpvRenderContext, MpvHandle, *const MpvRenderParam) -> c_int;
type FnMpvRenderContextRender = unsafe extern "C" fn(MpvRenderContext, *const MpvRenderParam) -> c_int;
type FnMpvRenderContextFree = unsafe extern "C" fn(MpvRenderContext);
type FnMpvRenderContextUpdate = unsafe extern "C" fn(MpvRenderContext) -> u64;

struct MpvLib {
    _lib: Library,
    create: FnMpvCreate,
    initialize: FnMpvInitialize,
    terminate_destroy: FnMpvTerminateDestroy,
    command: FnMpvCommand,
    set_option_string: FnMpvSetOptionString,
    set_property: FnMpvSetProperty,
    _get_property_double: FnMpvGetPropertyDouble,
    _get_property_flag: FnMpvGetPropertyFlag,
    observe_property: FnMpvObserveProperty,
    wait_event: FnMpvWaitEvent,
    render_context_create: FnMpvRenderContextCreate,
    render_context_render: FnMpvRenderContextRender,
    render_context_free: FnMpvRenderContextFree,
    _render_context_update: FnMpvRenderContextUpdate,
}

impl MpvLib {
    fn load(path: &Path) -> Result<Self, String> {
        unsafe {
            let lib = Library::new(path).map_err(|e| format!("Failed to load libmpv: {}", e))?;

            macro_rules! load_fn {
                ($lib:expr, $name:literal) => {{
                    let sym: Symbol<_> = $lib.get($name.as_bytes())
                        .map_err(|e| format!("Failed to load {}: {}", $name, e))?;
                    *sym
                }};
            }

            let mpv = MpvLib {
                create: load_fn!(lib, "mpv_create"),
                initialize: load_fn!(lib, "mpv_initialize"),
                terminate_destroy: load_fn!(lib, "mpv_terminate_destroy"),
                command: load_fn!(lib, "mpv_command"),
                set_option_string: load_fn!(lib, "mpv_set_option_string"),
                set_property: load_fn!(lib, "mpv_set_property"),
                _get_property_double: load_fn!(lib, "mpv_get_property"),
                _get_property_flag: load_fn!(lib, "mpv_get_property"),
                observe_property: load_fn!(lib, "mpv_observe_property"),
                wait_event: load_fn!(lib, "mpv_wait_event"),
                render_context_create: load_fn!(lib, "mpv_render_context_create"),
                render_context_render: load_fn!(lib, "mpv_render_context_render"),
                render_context_free: load_fn!(lib, "mpv_render_context_free"),
                _render_context_update: load_fn!(lib, "mpv_render_context_update"),
                _lib: lib,
            };

            Ok(mpv)
        }
    }
}

// Frame buffer shared between mpv render and egui display
pub struct FrameBuffer {
    pub pixels: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub dirty: bool,
}

pub struct Player {
    pub is_playing: bool,
    pub current_file: Option<String>,
    pub position: f64,
    pub duration: f64,
    pub volume: f64,
    pub available: bool,
    pub error_msg: Option<String>,

    mpv: Option<MpvLib>,
    handle: MpvHandle,
    render_ctx: MpvRenderContext,
    pub frame: Arc<Mutex<FrameBuffer>>,
    render_width: u32,
    render_height: u32,
}

impl Player {
    pub fn new() -> Self {
        let mut player = Player {
            is_playing: false,
            current_file: None,
            position: 0.0,
            duration: 0.0,
            volume: 100.0,
            available: false,
            error_msg: None,
            mpv: None,
            handle: ptr::null_mut(),
            render_ctx: ptr::null_mut(),
            frame: Arc::new(Mutex::new(FrameBuffer {
                pixels: Vec::new(),
                width: 0,
                height: 0,
                dirty: false,
            })),
            render_width: 854,
            render_height: 480,
        };

        match player.init_mpv() {
            Ok(()) => {
                player.available = true;
                log::info!("libmpv loaded successfully");
            }
            Err(e) => {
                player.error_msg = Some(e.clone());
                log::warn!("libmpv not available: {}", e);
            }
        }

        player
    }

    fn find_libmpv() -> Option<PathBuf> {
        // 1. Check appdata/libs/
        let libs_dir = config::appdata_dir().join("libs");
        let candidates = if cfg!(windows) {
            vec![libs_dir.join("mpv-2.dll"), libs_dir.join("libmpv-2.dll")]
        } else {
            vec![
                libs_dir.join("libmpv.so"),
                libs_dir.join("libmpv.so.2"),
                libs_dir.join("libmpv.so.1"),
            ]
        };

        for c in &candidates {
            if c.exists() {
                return Some(c.clone());
            }
        }

        // 2. Try system paths
        if cfg!(windows) {
            // Try PATH
            Some(PathBuf::from("mpv-2.dll"))
        } else {
            // Try common system locations
            let system_paths = [
                "/usr/lib/libmpv.so",
                "/usr/lib/x86_64-linux-gnu/libmpv.so",
                "/usr/lib64/libmpv.so",
                "/usr/local/lib/libmpv.so",
            ];
            for p in &system_paths {
                if Path::new(p).exists() {
                    return Some(PathBuf::from(p));
                }
            }
            // Last resort: let dlopen search
            Some(PathBuf::from("libmpv.so"))
        }
    }

    fn init_mpv(&mut self) -> Result<(), String> {
        let lib_path = Self::find_libmpv()
            .ok_or_else(|| "libmpv not found. Place libmpv.so (Linux) or mpv-2.dll (Windows) in appdata/libs/".to_string())?;

        let mpv = MpvLib::load(&lib_path)?;

        unsafe {
            let handle = (mpv.create)();
            if handle.is_null() {
                return Err("mpv_create failed".to_string());
            }

            // Configure mpv for software video output
            let opt = |k: &str, v: &str| {
                let key = CString::new(k).unwrap();
                let val = CString::new(v).unwrap();
                (mpv.set_option_string)(handle, key.as_ptr(), val.as_ptr());
            };

            opt("vo", "libmpv");
            opt("hwdec", "no");
            opt("video-timing-offset", "0");
            opt("idle", "yes");
            opt("input-default-bindings", "no");
            opt("input-vo-keyboard", "no");
            opt("osc", "no");
            opt("osd-level", "0");

            // Read-ahead buffer for slow storage
            opt("cache", "yes");
            opt("demuxer-max-bytes", "100MiB");
            opt("demuxer-readahead-secs", "10");
            opt("cache-secs", "30");

            let ret = (mpv.initialize)(handle);
            if ret < 0 {
                (mpv.terminate_destroy)(handle);
                return Err(format!("mpv_initialize failed: {}", ret));
            }

            // Create software render context
            let api_type = CString::new("sw").unwrap();

            let params = [
                MpvRenderParam {
                    param_type: MPV_RENDER_PARAM_API_TYPE,
                    data: api_type.as_ptr() as *mut c_void,
                },
                MpvRenderParam {
                    param_type: 0,
                    data: ptr::null_mut(),
                },
            ];

            let mut render_ctx: MpvRenderContext = ptr::null_mut();
            let ret = (mpv.render_context_create)(
                &mut render_ctx,
                handle,
                params.as_ptr(),
            );
            if ret < 0 {
                (mpv.terminate_destroy)(handle);
                return Err(format!("mpv_render_context_create failed: {}", ret));
            }

            // Observe properties
            let prop = |name: &str, format: c_int, ud: u64| {
                let n = CString::new(name).unwrap();
                (mpv.observe_property)(handle, ud, n.as_ptr(), format);
            };
            prop("time-pos", MPV_FORMAT_DOUBLE, 1);
            prop("duration", MPV_FORMAT_DOUBLE, 2);
            prop("pause", MPV_FORMAT_FLAG, 3);

            self.handle = handle;
            self.render_ctx = render_ctx;
            self.mpv = Some(mpv);
        }

        Ok(())
    }

    pub fn load(&mut self, path: &str) {
        let mpv = match &self.mpv {
            Some(m) => m,
            None => return,
        };

        let cmd_loadfile = CString::new("loadfile").unwrap();
        let cmd_path = CString::new(path).unwrap();
        let args: [*const c_char; 3] = [
            cmd_loadfile.as_ptr(),
            cmd_path.as_ptr(),
            ptr::null(),
        ];

        unsafe {
            (mpv.command)(self.handle, args.as_ptr());
        }

        self.current_file = Some(path.to_string());
        self.is_playing = true;
        self.position = 0.0;
        log::info!("Player: loading {}", path);
    }

    pub fn toggle_pause(&mut self) {
        let mpv = match &self.mpv {
            Some(m) => m,
            None => {
                self.is_playing = !self.is_playing;
                return;
            }
        };

        let cmd = CString::new("cycle").unwrap();
        let prop = CString::new("pause").unwrap();
        let args: [*const c_char; 3] = [cmd.as_ptr(), prop.as_ptr(), ptr::null()];

        unsafe {
            (mpv.command)(self.handle, args.as_ptr());
        }
    }

    pub fn stop(&mut self) {
        if let Some(mpv) = &self.mpv {
            let cmd = CString::new("stop").unwrap();
            let args: [*const c_char; 2] = [cmd.as_ptr(), ptr::null()];
            unsafe {
                (mpv.command)(self.handle, args.as_ptr());
            }
        }
        self.is_playing = false;
        self.current_file = None;
        self.position = 0.0;
        self.duration = 0.0;
    }

    pub fn seek(&mut self, position: f64) {
        let mpv = match &self.mpv {
            Some(m) => m,
            None => return,
        };

        let cmd = CString::new("seek").unwrap();
        let pos_str = CString::new(format!("{}", position)).unwrap();
        let mode = CString::new("absolute").unwrap();
        let args: [*const c_char; 4] = [cmd.as_ptr(), pos_str.as_ptr(), mode.as_ptr(), ptr::null()];

        unsafe {
            (mpv.command)(self.handle, args.as_ptr());
        }
        self.position = position;
    }

    pub fn set_volume(&mut self, volume: f64) {
        self.volume = volume.clamp(0.0, 100.0);

        if let Some(mpv) = &self.mpv {
            let prop = CString::new("volume").unwrap();
            let val = self.volume;
            unsafe {
                (mpv.set_property)(
                    self.handle,
                    prop.as_ptr(),
                    MPV_FORMAT_DOUBLE,
                    &val as *const f64 as *const c_void,
                );
            }
        }
    }

    /// Poll mpv events and update state. Call this each frame.
    pub fn poll_events(&mut self) {
        let mpv = match &self.mpv {
            Some(m) => m,
            None => return,
        };

        unsafe {
            loop {
                let event = (mpv.wait_event)(self.handle, 0.0);
                if event.is_null() {
                    break;
                }
                let ev = &*event;
                match ev.event_id {
                    MPV_EVENT_NONE => break,
                    MPV_EVENT_PROPERTY_CHANGE => {
                        if !ev.data.is_null() {
                            let prop = &*(ev.data as *const MpvEventProperty);
                            match ev.reply_userdata {
                                1 => {
                                    // time-pos
                                    if prop.format == MPV_FORMAT_DOUBLE && !prop.data.is_null() {
                                        self.position = *(prop.data as *const f64);
                                    }
                                }
                                2 => {
                                    // duration
                                    if prop.format == MPV_FORMAT_DOUBLE && !prop.data.is_null() {
                                        self.duration = *(prop.data as *const f64);
                                    }
                                }
                                3 => {
                                    // pause
                                    if prop.format == MPV_FORMAT_FLAG && !prop.data.is_null() {
                                        let paused = *(prop.data as *const c_int) != 0;
                                        self.is_playing = !paused;
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                    MPV_EVENT_END_FILE => {
                        self.is_playing = false;
                    }
                    MPV_EVENT_SHUTDOWN => {
                        self.is_playing = false;
                        self.current_file = None;
                    }
                    _ => {}
                }
            }
        }
    }

    /// Render the current frame to the internal pixel buffer.
    /// Call this each frame when video is loaded.
    pub fn render_frame(&mut self, width: u32, height: u32) {
        let mpv = match &self.mpv {
            Some(m) => m,
            None => return,
        };

        if self.render_ctx.is_null() || self.current_file.is_none() {
            return;
        }

        // Check if mpv has a new frame ready; skip render if not
        let update_flags = unsafe { (mpv._render_context_update)(self.render_ctx) };
        if update_flags == 0 {
            return;
        }

        self.render_width = width.max(2);
        self.render_height = height.max(2);

        let stride = (self.render_width * 4) as i32;
        let buf_size = (stride as u32 * self.render_height) as usize;

        let mut frame = self.frame.lock().unwrap();
        if frame.pixels.len() != buf_size {
            frame.pixels.resize(buf_size, 0);
        }
        frame.width = self.render_width;
        frame.height = self.render_height;

        let mut size: [c_int; 2] = [self.render_width as c_int, self.render_height as c_int];
        let format = CString::new("rgb0").unwrap();

        let params = [
            MpvRenderParam {
                param_type: MPV_RENDER_PARAM_SW_SIZE,
                data: size.as_mut_ptr() as *mut c_void,
            },
            MpvRenderParam {
                param_type: MPV_RENDER_PARAM_SW_FORMAT,
                data: format.as_ptr() as *mut c_void,
            },
            MpvRenderParam {
                param_type: MPV_RENDER_PARAM_SW_STRIDE,
                data: &stride as *const i32 as *mut c_void,
            },
            MpvRenderParam {
                param_type: MPV_RENDER_PARAM_SW_POINTER,
                data: frame.pixels.as_mut_ptr() as *mut c_void,
            },
            MpvRenderParam {
                param_type: 0,
                data: ptr::null_mut(),
            },
        ];

        let ret = unsafe {
            (mpv.render_context_render)(self.render_ctx, params.as_ptr())
        };
        if ret >= 0 {
            frame.dirty = true;
        } else {
            log::warn!("mpv_render_context_render failed: {}", ret);
        }
    }

    /// Capture current frame as a JPEG thumbnail using mpv's screenshot command.
    pub fn screenshot_to_file(&self, output_path: &std::path::Path) -> bool {
        let mpv = match &self.mpv {
            Some(m) => m,
            None => return false,
        };

        let cmd = CString::new("screenshot-to-file").unwrap();
        let path = CString::new(output_path.to_string_lossy().as_ref()).unwrap();
        let flags = CString::new("video").unwrap();
        let args: [*const c_char; 4] = [
            cmd.as_ptr(),
            path.as_ptr(),
            flags.as_ptr(),
            ptr::null(),
        ];

        let ret = unsafe { (mpv.command)(self.handle, args.as_ptr()) };
        if ret < 0 {
            log::warn!("screenshot-to-file failed: {}", ret);
            return false;
        }

        // Give mpv a moment to write the file
        std::thread::sleep(std::time::Duration::from_millis(100));
        output_path.exists() && std::fs::metadata(output_path).map(|m| m.len() > 0).unwrap_or(false)
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        if let Some(mpv) = &self.mpv {
            if !self.render_ctx.is_null() {
                unsafe {
                    (mpv.render_context_free)(self.render_ctx);
                }
            }
            if !self.handle.is_null() {
                unsafe {
                    (mpv.terminate_destroy)(self.handle);
                }
            }
        }
    }
}
