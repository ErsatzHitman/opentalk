//! eframe 0.36 + wgpu restricted to the DX12 backend. Window options match the glow probe.
//! `PROBE_POWER=low` selects the low-power (integrated) adapter instead of the high-performance one.
use eframe::egui;
use eframe::egui_wgpu::{WgpuConfiguration, WgpuSetup, WgpuSetupCreateNew};
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
        .with_title("OpenTalk probe (wgpu dx12)")
        .with_app_id("OpenTalkProbe")
        .with_inner_size([1200.0, 800.0])
        .with_min_inner_size([760.0, 520.0])
        .with_decorations(false);
    let mut setup = WgpuSetupCreateNew::without_display_handle();
    setup.instance_descriptor.backends = eframe::wgpu::Backends::DX12;
    if std::env::var("PROBE_POWER").as_deref() == Ok("low") {
        setup.power_preference = eframe::wgpu::PowerPreference::LowPower;
    }
    let wgpu_options = WgpuConfiguration { wgpu_setup: WgpuSetup::CreateNew(setup), ..Default::default() };
    eframe::NativeOptions { viewport, centered: true, depth_buffer: 0, stencil_buffer: 0, multisampling: 0, renderer: eframe::Renderer::Wgpu, wgpu_options, ..Default::default() }
}

fn main() -> eframe::Result {
    renderer_probe::report::mark_start();
    eframe::run_native(
        "OpenTalk probe (wgpu dx12)",
        native_options(),
        Box::new(|cc| {
            page::install_fonts(&cc.egui_ctx);
            Ok(Box::new(App { probe: Probe::new("wgpu-dx12") }))
        }),
    )
}
