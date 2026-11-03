//! Native download manager: the window version of `kit list`, `plan`, `install` and `status`.

use eframe::egui;
use emergency_kit::ui::{app::App, backend::Config};

fn main() -> eframe::Result {
    let cfg = match Config::discover() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: {e:#}");
            std::process::exit(1);
        }
    };
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Blackout Kit")
            .with_app_id("blackout-kit")
            .with_inner_size([1240.0, 780.0])
            .with_min_inner_size([900.0, 560.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Blackout Kit",
        options,
        Box::new(|cc| Ok(Box::new(App::new(cc, cfg)))),
    )
}
