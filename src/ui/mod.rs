//! The launcher window.

mod app;

use anyhow::{Context, Result};

pub fn run() -> Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([560.0, 620.0])
            .with_min_inner_size([460.0, 420.0])
            .with_title("rsclient"),
        ..Default::default()
    };

    eframe::run_native(
        "rsclient",
        options,
        Box::new(|cc| Ok(Box::new(app::App::new(cc)) as Box<dyn eframe::App>)),
    )
    .map_err(|e| anyhow::anyhow!("{e}"))
    .context("could not open the launcher window")
}
