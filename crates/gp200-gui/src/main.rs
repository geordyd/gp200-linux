use eframe::egui;
use gp200_gui::{LaunchOptions, StudioApp};
fn main() -> eframe::Result {
    if std::env::args().any(|argument| argument == "--calibration-helper-info") {
        println!(
            "{}",
            serde_json::json!({
                "available": gp200_assets::calibration_helper_available(),
                "sha256": gp200_assets::calibration_helper_sha256(),
                "mode": "offline independent calibration; no MIDI or network",
            })
        );
        return Ok(());
    }
    if std::env::args().any(|argument| argument == "--version") {
        println!("GP200 Linux {} (Linux native)", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if std::env::args().any(|argument| argument == "--help") {
        println!(
            "GP200 Linux\n  --demo\n  --connect\n  --scan-bank\n  --capture-device-backup\n  --read-globals\n  --read-ir-metadata\n  --open PRESET_OR_BUNDLE\n  --theme dark|light|system\n  --scale 1..2\n  --view editor|library|assets|assignments|device|performance|practice|diagnostics|settings|help\n  --qa-model-picker\n  --qa-selected-module NAME_OR_ID\n  --qa-assignment-tab expression|ctrl|quick|routing\n  --qa-global-tab audio|midi|external|footswitch|eq|display\n  --qa-library-tab presets|backups|compare\n  --qa-performance-arrange\n  --size WIDTHxHEIGHT\n  --screenshot /absolute/path.png\n\nDefault mode opens an offline capture. --connect selects one unique GP-200, identifies it, and reads its buffer; it writes no presets. With --screenshot it waits for Ready (10 second limit). --scan-bank then reads 256 stored presets sequentially, with a 45 second scan limit. A failed capture exits with code 3.\n--qa-model-picker, --qa-selected-module, --qa-assignment-tab and --qa-global-tab only select visible panels; they send no edit. --read-globals requires --demo or --connect, issues one settings read after Ready, and waits up to 10 seconds before a screenshot.\n--read-ir-metadata requires --demo or --connect and explicitly reads all20 IR metadata slots sequentially after other requested reads, with a 15 second limit. Metadata does not establish occupancy or a complete asset backup.\n--capture-device-backup requires --demo or --connect, reads current buffer,256 stored slots,globals and verified asset metadata, and saves each item before the next read. It waits for a terminal capture (90 second limit); missing asset data/unsupported metadata remain explicitly incomplete. It sends no edits.\n--screenshot writes a native renderer screenshot on a worker and exits."
        );
        println!(
            "--qa-ir-history list|ATTEMPT_ID opens retained local IR observations; it sends no device command."
        );
        println!(
            "--calibration-helper-info reports the embedded offline helper availability and SHA-256 as JSON without opening the GUI or MIDI."
        );
        return Ok(());
    }
    let options = match LaunchOptions::parse(std::env::args().skip(1)) {
        Ok(options) => options,
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(2);
        }
    };
    let qa_result = options.qa_result.clone();
    eframe::run_native(
        "GP200 Linux",
        eframe::NativeOptions {
            persist_window: options.width.is_none() && options.screenshot.is_none(),
            // Unattended QA must not wait for a compositor presentation callback
            // when a window is obscured. Normal interactive rendering keeps vsync.
            glow_options: eframe::egui_glow::GlowConfiguration {
                vsync: options.screenshot.is_none(),
                ..Default::default()
            },
            viewport: egui::ViewportBuilder::default()
                .with_app_id("gp200-studio")
                .with_inner_size([
                    options.width.unwrap_or(1280.0),
                    options.height.unwrap_or(800.0),
                ])
                .with_min_inner_size([640.0, 480.0]),
            ..Default::default()
        },
        Box::new(move |cc| {
            StudioApp::new(cc, options)
                .map(|app| Box::new(app) as Box<dyn eframe::App>)
                .map_err(Into::into)
        }),
    )?;
    let code = qa_result.load(std::sync::atomic::Ordering::Relaxed);
    if code != 0 {
        std::process::exit(code);
    }
    Ok(())
}
