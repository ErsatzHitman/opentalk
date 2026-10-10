//! eframe 0.36 + glow (OpenGL), with the same NativeOptions as `src/ui/app.rs::native_options` (copied).
use eframe::egui;
use renderer_probe::{page, report::Probe};

struct App {
    probe: Probe,
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        let prev = frame.info().cpu_usage.map(|s| s * 1000.0);
        if self.probe.frame(prev) {
            ui.ctx().request_repaint();
        }
        page::draw(ui);
    }
}

fn native_options() -> eframe::NativeOptions {
    let viewport = egui::ViewportBuilder::default()
        .with_title("OpenTalk probe (glow)")
        .with_app_id("OpenTalkProbe")
        .with_inner_size([1200.0, 800.0])
        .with_min_inner_size([760.0, 520.0])
        .with_decorations(false);
    let glow_options = eframe::egui_glow::GlowConfiguration {
        hardware_acceleration: eframe::egui_glow::HardwareAcceleration::Preferred,
        vsync: true,
        ..Default::default()
    };
    eframe::NativeOptions { viewport, centered: true, depth_buffer: 0, stencil_buffer: 0, multisampling: 0, glow_options, ..Default::default() }
}

fn main() -> eframe::Result {
    renderer_probe::report::mark_start();
    eframe::run_native(
        "OpenTalk probe (glow)",
        native_options(),
        Box::new(|cc| {
            page::install_fonts(&cc.egui_ctx);
            Ok(Box::new(App { probe: Probe::new("glow") }))
        }),
    )
}
