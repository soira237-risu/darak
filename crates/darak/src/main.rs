#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod delete;
mod doc;
mod fonts;
mod search_panel;
mod treemap;

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("Darak")
            .with_inner_size([1280.0, 820.0])
            .with_min_inner_size([640.0, 400.0])
            .with_icon(
                eframe::icon_data::from_png_bytes(include_bytes!("../assets/icon-256.png"))
                    .expect("창 아이콘 PNG를 읽지 못함"),
            ),
        ..Default::default()
    };
    eframe::run_native("darak", options, Box::new(|cc| Ok(Box::new(app::App::new(cc)))))
}
