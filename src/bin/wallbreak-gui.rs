use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use eframe::egui;
use wallbreak::gui::panel::Panel;

fn main() -> eframe::Result<()> {
    let close = Arc::new(AtomicBool::new(false));
    for signal in [signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM] {
        signal_hook::flag::register(signal, close.clone())
            .map_err(|error| eframe::Error::AppCreation(Box::new(error)))?;
    }
    // Hidden smoke-test hook: open the panel briefly for headless screenshots.
    let smoke = std::env::args().any(|arg| arg == "--smoke-test");
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1100.0, 750.0]),
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
                close,
            }) as Box<dyn eframe::App>)
        }),
    )
}

struct SmokePanel {
    panel: Panel,
    started: std::time::Instant,
    smoke: bool,
    close: Arc<AtomicBool>,
}

impl SmokePanel {
    fn show(&mut self, ctx: &egui::Context) {
        self.panel.show(ctx);
        // Signal handlers only set a flag. Closing the viewport runs normal
        // application destruction, including the player's graceful cleanup.
        ctx.request_repaint_after(std::time::Duration::from_millis(100));
        if self.close.load(Ordering::Relaxed)
            || (self.smoke && self.started.elapsed() > std::time::Duration::from_secs(8))
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }
}

impl eframe::App for SmokePanel {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.show(ctx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn signal_flag_and_smoke_timeout_request_normal_viewport_close() {
        for (signal, smoke, elapsed, should_close) in [
            (false, false, 9, false),
            (true, false, 0, true),
            (false, true, 0, false),
            (false, true, 9, true),
        ] {
            let mut app = SmokePanel {
                panel: Panel::new(),
                started: Instant::now() - Duration::from_secs(elapsed),
                smoke,
                close: Arc::new(AtomicBool::new(signal)),
            };
            let ctx = egui::Context::default();
            for _ in 0..5 {
                let _ = ctx.run(Default::default(), |ctx| app.show(ctx));
            }
            let output = ctx.run(Default::default(), |ctx| app.show(ctx));
            let viewport = &output.viewport_output[&egui::ViewportId::ROOT];
            assert_eq!(
                viewport.commands.contains(&egui::ViewportCommand::Close),
                should_close
            );
            if !should_close {
                assert!(viewport.repaint_delay > Duration::ZERO);
            }
            assert!(viewport.repaint_delay <= Duration::from_millis(100));
        }
    }
}
