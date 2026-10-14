//! Native renderer QA with immutable synthetic library revisions and a counting
//! disconnected DeviceService. No MIDI backend is constructed or opened.
use eframe::egui;
use gp200_domain::*;
use gp200_gui::{StudioApp, runtime::RuntimeServices};
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

struct CountingDisconnectedDeviceService(Arc<AtomicUsize>);
impl DeviceService for CountingDisconnectedDeviceService {
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
    report: serde_json::Value,
    frames: u32,
    path: PathBuf,
    requested: bool,
    saved: Option<mpsc::Receiver<Result<(), String>>>,
    started: Instant,
}
impl eframe::App for Preview {
    fn logic(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        self.studio.logic(ctx, frame);
        self.frames += 1;
        if self.frames >= 10 && !self.requested {
            println!(
                "{}",
                serde_json::json!({
                    "native_merge_review": self.report,
                    "hardware_backend_constructed": false,
                    "device_service_submissions": self.count.load(Ordering::SeqCst),
                })
            );
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(Default::default()));
            self.requested = true;
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
                .map_err(|error| error.to_string());
                let _ = send.send(result);
            });
        }
        if let Some(saved) = &self.saved
            && let Ok(result) = saved.try_recv()
        {
            result.expect("Native merge screenshot save failed");
            assert_eq!(self.count.load(Ordering::SeqCst), 0);
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        assert!(
            self.started.elapsed() < Duration::from_secs(30),
            "Native merge preview did not settle"
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

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.len() != 5 {
        return Err("Expected selection|review dark|light scale WIDTHxHEIGHT PNG".into());
    }
    let review = match args[0].as_str() {
        "selection" => false,
        "review" => true,
        _ => return Err("Unknown stage".into()),
    };
    let theme = match args[1].as_str() {
        "dark" => ThemePreference::Dark,
        "light" => ThemePreference::Light,
        _ => return Err("Unknown theme".into()),
    };
    let scale: f32 = args[2].parse()?;
    if !scale.is_finite() || !(0.5..=3.).contains(&scale) {
        return Err("Scale out of bounds".into());
    }
    let (width, height) = args[3].split_once('x').ok_or("Invalid viewport")?;
    let size = [width.parse::<f32>()?, height.parse::<f32>()?];
    let path = PathBuf::from(&args[4]);
    let count = Arc::new(AtomicUsize::new(0));
    let final_count = count.clone();
    eframe::run_native(
        "GP200 Studio",
        eframe::NativeOptions {
            persist_window: false,
            viewport: egui::ViewportBuilder::default().with_inner_size(size),
            glow_options: eframe::egui_glow::GlowConfiguration {
                vsync: false,
                ..Default::default()
            },
            ..Default::default()
        },
        Box::new(move |cc| {
            gp200_gui::theme::install_fonts(&cc.egui_ctx);
            let preferences = AppPreferences {
                theme,
                scale,
                ..Default::default()
            };
            let target = gp200_sim::captured_demo_snapshot();
            let mut draft = PresetDraft::from_snapshot(&target);
            draft.apply(&EditPatch::SetMetadata(MetadataPatch {
                name: Some("Source voice".into()),
                level: Some(73),
                ..Default::default()
            }))?;
            draft.apply(&EditPatch::SetParameter {
                module: ModuleId::new(3).unwrap(),
                model: ModelCode(0x07000015),
                model_index: ModelIndex(0),
                parameter: ParamId::new(0).unwrap(),
                value: gp200_protocol::F32Bits::from_f32(73.),
            })?;
            let source = Arc::new(draft.snapshot(
                &target,
                SnapshotSource::Capture {
                    capture_id: "synthetic-native-merge-source".into(),
                },
                Revision(7),
            )?);
            let version = LibraryVersion {
                id: LibraryVersionId::from("native-merge-source-v1"),
                library: LibraryId::from("native-merge-source"),
                parent: None,
                snapshot: source,
                metadata: LibraryMetadata {
                    name: "Source voice".into(),
                    notes: "Synthetic immutable revision for native QA".into(),
                    ..Default::default()
                },
                created_at: "2026-10-10T12:00:00Z".into(),
            };
            let mut controller =
                gp200_app::AppController::with_demo(target.clone(), preferences.clone())?;
            controller.dispatch(AppInput::Ui(UiIntent::NewOfflineFrom(target)));
            controller.dispatch(AppInput::Library(LibraryEvent::VersionLoaded {
                intent: IntentId(1),
                version: version.clone(),
            }));
            controller.dispatch(AppInput::Library(LibraryEvent::QueryResult {
                intent: IntentId(2),
                entries: vec![LibraryEntry {
                    id: version.library.clone(),
                    latest_version: version.id.clone(),
                    metadata: version.metadata.clone(),
                    version_count: 1,
                    modified_at: version.created_at.clone(),
                }],
            }));
            let context = cc.egui_ctx.clone();
            let wake: WakeUi = Arc::new(move || context.request_repaint());
            let services = RuntimeServices::with_preferences(
                Box::new(CountingDisconnectedDeviceService(count.clone())),
                wake,
                preferences,
            );
            let mut studio = StudioApp::with_services(controller, services);
            studio.open_selective_merge_source(version.id);
            studio.review_selective_merge(vec![
                OfflineMergeField::Metadata(MergeMetadataField::Level),
                OfflineMergeField::Parameter {
                    module: ModuleId::new(3).unwrap(),
                    expected_model: ModelCode(0x07000015),
                    parameter: ParamId::new(0).unwrap(),
                },
            ])?;
            let report = studio
                .selective_merge_report()
                .expect("Exact synthetic review");
            if !review {
                studio.show_selective_merge_selection();
            }
            Ok(Box::new(Preview {
                studio,
                count,
                report,
                frames: 0,
                path,
                requested: false,
                saved: None,
                started: Instant::now(),
            }))
        }),
    )?;
    assert_eq!(final_count.load(Ordering::SeqCst), 0);
    Ok(())
}
