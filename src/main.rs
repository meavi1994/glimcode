mod app;
mod chrome;
mod diff;
mod document;
mod editor_analysis;
mod editor_structure;
mod explorer;
mod file_find;
mod folding;
mod git;
mod git_view;
mod highlighting;
mod ignore_rules;
mod markdown;
mod markdown_images;
mod navigation;
mod preferences;
mod preview;
mod ranking;
mod search_highlight;
mod search_view;
mod session;
mod sheet;
mod terminal;
mod test_files;
mod worktrees;

fn main() -> eframe::Result {
    if let Some(status) = terminal::run_share_client() {
        std::process::exit(status);
    }
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
