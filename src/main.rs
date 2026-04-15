mod app;
mod config;
mod db;
mod player;
mod scanner;
mod thumbnail;
mod ui;

fn viewport() -> egui::ViewportBuilder {
    egui::ViewportBuilder::default()
        .with_inner_size([1200.0, 800.0])
        .with_min_inner_size([800.0, 600.0])
        .with_title("MicroMedia")
}

fn try_wgpu_vulkan() -> eframe::Result<()> {
    log::info!("Trying wgpu (Vulkan/Metal/DX12)...");
    let options = eframe::NativeOptions {
        viewport: viewport(),
        renderer: eframe::Renderer::Wgpu,
        ..Default::default()
    };
    eframe::run_native(
        "MicroMedia",
        options,
        Box::new(|cc| Ok(Box::new(app::MediaManagerApp::new(cc)))),
    )
}

fn try_wgpu_gl() -> eframe::Result<()> {
    log::info!("Trying wgpu (GL backend)...");
    std::env::set_var("WGPU_BACKEND", "gl");
    let options = eframe::NativeOptions {
        viewport: viewport(),
        renderer: eframe::Renderer::Wgpu,
        ..Default::default()
    };
    eframe::run_native(
        "MicroMedia",
        options,
        Box::new(|cc| Ok(Box::new(app::MediaManagerApp::new(cc)))),
    )
}

fn try_glow() -> eframe::Result<()> {
    log::info!("Trying glow (OpenGL)...");
    let options = eframe::NativeOptions {
        viewport: viewport(),
        renderer: eframe::Renderer::Glow,
        ..Default::default()
    };
    eframe::run_native(
        "MicroMedia",
        options,
        Box::new(|cc| Ok(Box::new(app::MediaManagerApp::new(cc)))),
    )
}

fn main() -> eframe::Result<()> {
    env_logger::init();

    // Prefer X11 if user hasn't set a preference (avoids wayland issues on musl/old systems)
    if std::env::var("WINIT_UNIX_BACKEND").is_err() {
        if std::env::var("WAYLAND_DISPLAY").is_err() {
            std::env::set_var("WINIT_UNIX_BACKEND", "x11");
        }
    }

    // Try renderers in order: wgpu (Vulkan) -> wgpu (GL) -> glow (OpenGL)
    match try_wgpu_vulkan() {
        Ok(()) => Ok(()),
        Err(e) => {
            log::warn!("wgpu Vulkan failed: {e}, trying wgpu GL...");
            match try_wgpu_gl() {
                Ok(()) => Ok(()),
                Err(e) => {
                    log::warn!("wgpu GL failed: {e}, trying glow...");
                    try_glow()
                }
            }
        }
    }
}
