//! Native file review/rehearsal QA. The only device backend is the simulator.
use gp200_domain::DeviceModel;
use gp200_gui::{FirmwareSimulation, FirmwareWorkspaceLaunch, LaunchOptions, StudioApp};
use std::path::PathBuf;
struct Preview {
    studio: StudioApp,
    reported: bool,
}
impl eframe::App for Preview {
    fn logic(&mut self, ctx: &eframe::egui::Context, frame: &mut eframe::Frame) {
        self.studio.logic(ctx, frame);
        if !self.reported
            && let Some((report, message)) = self.studio.firmware_review_report()
        {
            println!(
                "{}",
                serde_json::json!({"native_worker_review":report, "native_worker_message":message})
            );
            self.reported = true;
        }
    }
    fn ui(&mut self, ui: &mut eframe::egui::Ui, frame: &mut eframe::Frame) {
        self.studio.ui(ui, frame);
    }
    fn on_exit(&mut self, gl: Option<&eframe::glow::Context>) {
        self.studio.on_exit(gl);
    }
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.len() != 5 {
        return Err("Usage: firmware_ui_preview inspect|preview|all|missing-block|missing-section|mismatch IMAGE dark|light WIDTHxHEIGHT SCREENSHOT".into());
    }
    let simulation = match args[0].as_str() {
        "inspect" => None,
        "preview" | "mismatch" => Some(FirmwareSimulation::Preview),
        "all" => Some(FirmwareSimulation::AllAcknowledged),
        "missing-block" => Some(FirmwareSimulation::MissingBlockAcknowledgement),
        "missing-section" => Some(FirmwareSimulation::MissingSectionAcknowledgement),
        _ => return Err("Unknown preview mode".into()),
    };
    let mut options = LaunchOptions::parse([
        "--demo".into(),
        "--view".into(),
        "assets".into(),
        "--theme".into(),
        args[2].clone(),
        "--size".into(),
        args[3].clone(),
        "--screenshot".into(),
        args[4].clone(),
    ])?;
    options.firmware_workspace = Some(FirmwareWorkspaceLaunch {
        source: PathBuf::from(&args[1]),
        identity_model: if args[0] == "mismatch" {
            DeviceModel::Gp200Lt
        } else {
            DeviceModel::Gp200
        },
        chunk_bytes: 128,
        simulation,
    });
    let status = options.qa_result.clone();
    eframe::run_native(
        "GP200 Studio · offline firmware review",
        eframe::NativeOptions {
            persist_window: false,
            glow_options: eframe::egui_glow::GlowConfiguration {
                vsync: false,
                ..Default::default()
            },
            viewport: eframe::egui::ViewportBuilder::default()
                .with_inner_size([options.width.unwrap(), options.height.unwrap()])
                .with_min_inner_size([640.0, 480.0]),
            ..Default::default()
        },
        Box::new(move |cc| {
            StudioApp::new(cc, options)
                .map(|app| {
                    Box::new(Preview {
                        studio: app,
                        reported: false,
                    }) as Box<dyn eframe::App>
                })
                .map_err(Into::into)
        }),
    )?;
    if status.load(std::sync::atomic::Ordering::Relaxed) != 0 {
        return Err("Native preview failed".into());
    }
    println!(
        "{}",
        serde_json::json!({"operation":"offline_firmware_ui_preview", "mode":args[0], "hardware_commands":0, "hardware_backend_constructed":false})
    );
    Ok(())
}
