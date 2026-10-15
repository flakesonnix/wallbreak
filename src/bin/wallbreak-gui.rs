use eframe::egui;
use wallbreak::gui::panel::Panel;

fn main() -> eframe::Result<()> {
    // Hidden smoke-test hook: open the panel briefly for headless screenshots.
    let smoke = std::env::args().any(|arg| arg == "--smoke-test");
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([640.0, 560.0]),
        ..Default::default()
    };
    eframe::run_native(
        "wallbreak",
        options,
        Box::new(move |_cc| {
            let mut panel = Panel::new();
            panel.refresh();
            Ok(Box::new(SmokePanel {
                panel,
                started: std::time::Instant::now(),
                smoke,
            }) as Box<dyn eframe::App>)
        }),
    )
}

struct SmokePanel {
    panel: Panel,
    started: std::time::Instant,
    smoke: bool,
}

impl eframe::App for SmokePanel {
    fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        self.panel.update(ctx, frame);
        if self.smoke {
            ctx.request_repaint();
            if self.started.elapsed() > std::time::Duration::from_secs(8) {
                std::process::exit(0);
            }
        }
    }
}
