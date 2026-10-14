//! Synthetic immutable preparation QA. No hardware backend is constructed.
use eframe::egui;
use gp200_domain::*;
use gp200_gui::{AssetJobWorkspaceLaunch, StudioApp, runtime::RuntimeServices};
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};
struct NoDevice(Arc<AtomicUsize>);
impl DeviceService for NoDevice {
    fn try_submit(&self, command: DeviceCommand) -> Result<(), SubmitError<DeviceCommand>> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Err(SubmitError::Disconnected(command))
    }
    fn try_next_event(&self) -> Option<DeviceEvent> {
        None
    }
    fn shutdown(&self) {}
}
struct Preview {
    studio: StudioApp,
    count: Arc<AtomicUsize>,
    prepare: bool,
    prepared_requested: bool,
    reported: bool,
    frames: u32,
    screenshot_requested: bool,
    path: PathBuf,
    saved: Option<mpsc::Receiver<Result<(), String>>>,
    started: Instant,
}
impl eframe::App for Preview {
    fn logic(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        self.studio.logic(ctx, frame);
        if let Some(report) = self.studio.asset_job_review_report() {
            let prepared = !report["review"]["prepared"].is_null();
            if self.prepare
                && !self.prepared_requested
                && report["review"]["prerequisites"]
                    .as_array()
                    .unwrap()
                    .is_empty()
            {
                self.studio.prepare_asset_job_review();
                self.prepared_requested = true;
            }
            if (!self.prepare || prepared) && !self.reported {
                println!(
                    "{}",
                    serde_json::json!({"native_worker_review":report,"hardware_backend_constructed":false,"device_service_submissions":self.count.load(Ordering::SeqCst)})
                );
                self.reported = true;
            }
        }
        if self.reported {
            self.frames += 1;
        }
        if self.frames >= 8 && !self.screenshot_requested {
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(Default::default()));
            self.screenshot_requested = true;
        }
        let pixels = ctx.input(|input| {
            input.events.iter().find_map(|event| {
                if let egui::Event::Screenshot { image, .. } = event {
                    Some(image.clone())
                } else {
                    None
                }
            })
        });
        if let Some(pixels) = pixels
            && self.saved.is_none()
        {
            let (send, receive) = mpsc::channel();
            let path = self.path.clone();
            self.saved = Some(receive);
            std::thread::spawn(move || {
                let bytes: Vec<u8> = pixels
                    .pixels
                    .iter()
                    .flat_map(|pixel| pixel.to_array())
                    .collect();
                let result = image::save_buffer_with_format(
                    &path,
                    &bytes,
                    pixels.width() as u32,
                    pixels.height() as u32,
                    image::ColorType::Rgba8,
                    image::ImageFormat::Png,
                )
                .map_err(|e| e.to_string());
                let _ = send.send(result);
            });
        }
        if let Some(saved) = &self.saved
            && let Ok(result) = saved.try_recv()
        {
            result.expect("Native screenshot save failed");
            assert_eq!(self.count.load(Ordering::SeqCst), 0);
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        assert!(
            self.started.elapsed() < Duration::from_secs(30),
            "Native asset review did not settle"
        );
        ctx.request_repaint_after(Duration::from_millis(16));
    }
    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        self.studio.ui(ui, frame);
    }
    fn on_exit(&mut self, gl: Option<&eframe::glow::Context>) {
        self.studio.on_exit(gl);
    }
}
fn fixture_controller(
    theme: ThemePreference,
    bank: AssetBank,
    ready: bool,
) -> gp200_app::AppController {
    let mut controller = gp200_app::AppController::with_demo(
        gp200_sim::captured_demo_snapshot(),
        AppPreferences {
            theme,
            ..Default::default()
        },
    )
    .unwrap();
    if !ready {
        return controller;
    }
    let epoch = SessionEpoch(5);
    let capabilities = Capabilities::simulator();
    let identity = DeviceIdentity {
        model: DeviceModel::Gp200,
        mode: BackendMode::Simulator,
        firmware: None,
        display_name: "Synthetic asset review fixture".into(),
    };
    controller.dispatch(AppInput::Device(DeviceEvent::SessionChanged {
        epoch,
        identity: identity.clone(),
        capabilities: capabilities.clone(),
    }));
    let session = DeviceJobSession::observe_normal(
        epoch,
        DeviceSelection {
            input_id: "fixture-input".into(),
            output_id: "fixture-output".into(),
            fingerprint_hint: None,
        },
        Revision(1),
        &RawTransfer::from_records(
            gp200_protocol::WRITE_KIND,
            &[gp200_protocol::RawRecord {
                command: 0x2001,
                body: vec![1, 0, 2, 0],
            }],
        )
        .unwrap(),
        &identity,
        &capabilities,
    )
    .unwrap();
    controller.dispatch(AppInput::Device(DeviceEvent::DeviceJobSessionObserved(
        session,
    )));
    controller.dispatch(AppInput::Device(DeviceEvent::ConnectionChanged {
        epoch,
        state: ConnectionState::Ready,
    }));
    let slot = AssetSlot::new(bank, bank.slots() - 1).unwrap();
    let effects = controller.dispatch(AppInput::Ui(UiIntent::RefreshAssetMetadata(slot)));
    let Effect::Device(DeviceCommand::ReadAssetMetadata { stamp, .. }) = &effects[0] else {
        panic!("metadata read")
    };
    let mut body = vec![0; 24];
    body[..2].copy_from_slice(&slot.index().to_le_bytes());
    body[8..15].copy_from_slice(b"Unknown");
    let metadata = Arc::new(
        DeviceAssetMetadata::from_transfer(
            slot,
            *stamp,
            Revision(3),
            SnapshotSource::Simulator,
            10,
            RawTransfer::from_records(
                gp200_protocol::WRITE_KIND,
                &[gp200_protocol::RawRecord {
                    command: bank.metadata_command(),
                    body,
                }],
            )
            .unwrap(),
        )
        .unwrap(),
    );
    controller.dispatch(AppInput::Device(DeviceEvent::AssetMetadataReceived {
        stamp: *stamp,
        metadata,
    }));
    controller
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.len() != 5 {
        return Err(
            "Usage: asset_job_ui_preview ir|clo|offline SOURCE dark|light WIDTHxHEIGHT SCREENSHOT"
                .into(),
        );
    }
    let bank = if args[0] == "clo" {
        AssetBank::SnapTone
    } else {
        AssetBank::Ir
    };
    let ready = match args[0].as_str() {
        "ir" | "clo" => true,
        "offline" => false,
        _ => return Err("Unknown preview case".into()),
    };
    let theme = match args[2].as_str() {
        "dark" => ThemePreference::Dark,
        "light" => ThemePreference::Light,
        _ => return Err("Unknown theme".into()),
    };
    let size: Vec<f32> = args[3]
        .split('x')
        .map(str::parse)
        .collect::<Result<_, _>>()?;
    if size.len() != 2 {
        return Err("Invalid size".into());
    }
    let controller = fixture_controller(theme, bank, ready);
    let count = Arc::new(AtomicUsize::new(0));
    let check = count.clone();
    let source = PathBuf::from(&args[1]);
    let screenshot = PathBuf::from(&args[4]);
    eframe::run_native(
        "GP200 Studio · synthetic asset review",
        eframe::NativeOptions {
            persist_window: false,
            glow_options: eframe::egui_glow::GlowConfiguration {
                vsync: false,
                ..Default::default()
            },
            viewport: egui::ViewportBuilder::default()
                .with_inner_size([size[0], size[1]])
                .with_min_inner_size([640.0, 480.0]),
            ..Default::default()
        },
        Box::new(move |cc| {
            gp200_gui::theme::install_fonts(&cc.egui_ctx);
            let ctx = cc.egui_ctx.clone();
            let services = RuntimeServices::new(
                Box::new(NoDevice(count.clone())),
                Arc::new(move || ctx.request_repaint()),
            );
            let mut studio = StudioApp::with_services(controller, services);
            studio.open_asset_job_review(AssetJobWorkspaceLaunch {
                source,
                bank,
                slot: bank.slots() - 1,
                name: "Reviewed asset".into(),
            });
            Ok(Box::new(Preview {
                studio,
                count,
                prepare: ready,
                prepared_requested: false,
                reported: false,
                frames: 0,
                screenshot_requested: false,
                path: screenshot,
                saved: None,
                started: Instant::now(),
            }))
        }),
    )?;
    assert_eq!(check.load(Ordering::SeqCst), 0);
    Ok(())
}
