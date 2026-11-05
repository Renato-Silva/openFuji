//! openFuji desktop app: remote shooting with live view and picture import.

mod app;
mod worker;

fn main() -> eframe::Result {
    env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("warn,openfuji=info"),
    )
    .init();
    // `openfuji [camera-ip] [--gallery]`: with an IP, connect right away.
    let args: Vec<String> = std::env::args().skip(1).collect();
    let gallery = args.iter().any(|a| a == "--gallery");
    let ip = args.into_iter().find(|a| !a.starts_with("--"));
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("openFuji")
            .with_app_id("openfuji")
            .with_inner_size([1180.0, 760.0])
            .with_min_inner_size([760.0, 480.0]),
        ..Default::default()
    };
    eframe::run_native(
        "openFuji",
        options,
        Box::new(move |cc| Ok(Box::new(app::App::new(cc, ip, gallery)))),
    )
}
