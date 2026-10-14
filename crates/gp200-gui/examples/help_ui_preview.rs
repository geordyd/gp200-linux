//! Native, offline help/preferences preview. No physical backend or browser launch.
use gp200_domain::DialogDirectoryPurpose;
use gp200_gui::{HelpTopic, LaunchOptions, StudioApp};
use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() != 4 {
        return Err("Usage: help_ui_preview start|editing|backups|assets|troubleshooting|about|settings dark|light WIDTHxHEIGHT SCREENSHOT".into());
    }
    let settings = args[0] == "settings";
    let topic = if settings {
        HelpTopic::Start
    } else {
        HelpTopic::parse(&args[0]).ok_or("Unknown help topic")?
    };
    let options = LaunchOptions::parse([
        "--demo".into(),
        "--view".into(),
        if settings {
            "settings".into()
        } else {
            "help".into()
        },
        "--theme".into(),
        args[1].clone(),
        "--size".into(),
        args[2].clone(),
        "--screenshot".into(),
        args[3].clone(),
    ])?;
    let status = options.qa_result.clone();
    // Real existing folders in the isolated preview preferences. No dialog is opened.
    let mut preferences = gp200_gui::runtime::load_preferences();
    let folder = PathBuf::from(
        std::env::var_os("XDG_DATA_HOME").ok_or("Preview needs isolated XDG_DATA_HOME")?,
    );
    std::fs::create_dir_all(&folder)?;
    for (purpose, name) in [
        (DialogDirectoryPurpose::PresetImport, "Imported presets"),
        (DialogDirectoryPurpose::PresetExport, "Portable exports"),
        (DialogDirectoryPurpose::IrImport, "Impulse responses"),
        (DialogDirectoryPurpose::NamImport, "NAM models"),
        (
            DialogDirectoryPurpose::CalibrationStimulus,
            "Calibration stimulus",
        ),
    ] {
        let path = folder.join(name);
        std::fs::create_dir_all(&path)?;
        preferences.dialog_directories.insert(purpose, path);
    }
    let config = gp200_gui::runtime::config_path();
    std::fs::create_dir_all(config.parent().ok_or("Config has no parent")?)?;
    std::fs::write(config, serde_json::to_vec(&preferences)?)?;
    eframe::run_native(
        "GP200 Studio · offline help preview",
        eframe::NativeOptions {
            persist_window: false,
            glow_options: eframe::egui_glow::GlowConfiguration {
                vsync: false,
                ..Default::default()
            },
            viewport: eframe::egui::ViewportBuilder::default()
                .with_inner_size([options.width.unwrap(), options.height.unwrap()]),
            ..Default::default()
        },
        Box::new(move |cc| {
            StudioApp::new(cc, options)
                .map(|mut app| {
                    if !settings {
                        app.open_help_topic(topic);
                    }
                    Box::new(app) as Box<dyn eframe::App>
                })
                .map_err(Into::into)
        }),
    )?;
    if status.load(std::sync::atomic::Ordering::Relaxed) != 0 {
        return Err("Preview QA failed".into());
    }
    Ok(())
}
