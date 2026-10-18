mod app;
mod chrome;
mod diff;
mod document;
mod editor_structure;
mod explorer;
mod file_find;
mod folding;
mod git;
mod git_view;
mod highlighting;
mod markdown;
mod markdown_images;
mod navigation;
mod preferences;
mod preview;
mod search_view;
mod sheet;
mod terminal;

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([1400.0, 900.0])
            .with_min_inner_size([850.0, 550.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Glimcode",
        options,
        Box::new(|cc| Ok(Box::new(app::Coder::new(cc)))),
    )
}
