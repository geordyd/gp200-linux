//! Native offline UI QA through the normal StudioApp renderer and workers.
//! Uses only an offline capture or the simulator; rejects physical connection.
use gp200_domain::{AssetBank, DeviceModel};
use gp200_gui::{
    AssetJobWorkspaceLaunch, FirmwareSimulation, FirmwareWorkspaceLaunch, HelpTopic, LaunchOptions,
    StudioApp,
};
use std::path::PathBuf;
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let mut forwarded = vec![];
    let mut topic = None;
    let mut firmware = None;
    let mut asset = None;
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--help-topic" => {
                topic = Some(
                    HelpTopic::parse(&args.next().ok_or("Missing help topic")?)
                        .ok_or("Unknown help topic")?,
                )
            }
            "--firmware-review" => {
                firmware = Some(PathBuf::from(args.next().ok_or("Missing firmware path")?))
            }
            "--asset-review" => {
                asset = Some(PathBuf::from(args.next().ok_or("Missing asset path")?))
            }
            "--connect" => return Err("Offline UI preview never opens hardware".into()),
            _ => forwarded.push(argument),
        }
    }
    let mut options = LaunchOptions::parse(forwarded)?;
    if let Some(source) = firmware {
        options.firmware_workspace = Some(FirmwareWorkspaceLaunch {
            source,
            identity_model: DeviceModel::Gp200,
            chunk_bytes: 128,
            simulation: Some(FirmwareSimulation::Preview),
        });
    }
    let status = options.qa_result.clone();
    eframe::run_native(
        "GP200 Studio",
        eframe::NativeOptions {
            persist_window: false,
            glow_options: eframe::egui_glow::GlowConfiguration {
                vsync: false,
                ..Default::default()
            },
            viewport: eframe::egui::ViewportBuilder::default().with_inner_size([
                options.width.unwrap_or(1280.0),
                options.height.unwrap_or(800.0),
            ]),
            ..Default::default()
        },
        Box::new(move |cc| {
            StudioApp::new(cc, options)
                .map(|mut app| {
                    if let Some(topic) = topic {
                        app.open_help_topic(topic);
                    }
                    if let Some(source) = asset {
                        app.open_asset_job_review(AssetJobWorkspaceLaunch {
                            source,
                            bank: AssetBank::Ir,
                            slot: 19,
                            name: "Reviewed asset".into(),
                        });
                    }
                    Box::new(app) as Box<dyn eframe::App>
                })
                .map_err(Into::into)
        }),
    )?;
    if status.load(std::sync::atomic::Ordering::Relaxed) != 0 {
        return Err("Native offline UI preview failed".into());
    }
    Ok(())
}
