//! Shared desktop and Android application.

pub mod api;
pub mod campus;
pub mod cli;
pub mod crypto;
pub mod location;
pub mod platform;
pub mod route;
pub mod template;
mod textlog;
mod track;

#[cfg(feature = "gui")]
mod ui;

#[cfg(all(target_os = "android", feature = "android"))]
mod android;

#[cfg(feature = "gui")]
pub fn run_gui(options: eframe::NativeOptions) -> eframe::Result<()> {
    eframe::run_native(
        "格陵兰岛",
        options,
        Box::new(|cc| {
            #[cfg(target_os = "android")]
            android::set_context(&cc.egui_ctx);
            Ok(Box::new(ui::App::new(cc)))
        }),
    )
}
