//! Native descriptive updater preview. It never constructs a MIDI backend.
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
    app: StudioApp,
    submissions: Arc<AtomicUsize>,
    screenshot: PathBuf,
    started: Instant,
    frames: usize,
    screenshot_requested: bool,
    saved: Option<mpsc::Receiver<Result<(), String>>>,
}
impl eframe::App for Preview {
    fn logic(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        self.app.logic(ctx, frame);
        self.frames += 1;
        if self.frames == 8 {
            println!(
                "{}",
                self.app
                    .firmware_updater_preview_report()
                    .expect("descriptive preview")
            );
        }
        if self.frames >= 12 && !self.screenshot_requested {
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(Default::default()));
            self.screenshot_requested = true;
        }
        if let Some(pixels) = ctx.input(|input| {
            input.events.iter().find_map(|event| match event {
                egui::Event::Screenshot { image, .. } => Some(image.clone()),
                _ => None,
            })
        }) && self.saved.is_none()
        {
            println!(
                "{}",
                serde_json::json!({"logical_points":[ctx.content_rect().width(), ctx.content_rect().height()],"pixels_per_point":ctx.pixels_per_point(),"screenshot_pixels":[pixels.width(),pixels.height()]})
            );
            let (send, receive) = mpsc::channel();
            let path = self.screenshot.clone();
            self.saved = Some(receive);
            std::thread::spawn(move || {
                let bytes: Vec<u8> = pixels
                    .pixels
                    .iter()
                    .flat_map(|pixel| pixel.to_array())
                    .collect();
                let result = image::save_buffer_with_format(
                    path,
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
            assert_eq!(self.submissions.load(Ordering::SeqCst), 0);
            self.app.request_firmware_preview_close(ctx);
        }
        assert!(
            self.started.elapsed() < Duration::from_secs(30),
            "Native screenshot did not settle"
        );
        ctx.request_repaint_after(Duration::from_millis(16));
    }
    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        self.app.ui(ui, frame);
    }
    fn on_exit(&mut self, gl: Option<&eframe::glow::Context>) {
        self.app.on_exit(gl);
    }
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.len() != 6 {
        return Err("Usage: firmware_updater_ui_preview SCENE dark|light WIDTHxHEIGHT SCALE normal|compact SCREENSHOT".into());
    }
    // The caller must provide private per-run XDG roots before process startup.
    // The harness does not mutate process environment or use the ordinary store.
    for name in ["XDG_DATA_HOME", "XDG_CONFIG_HOME", "XDG_RUNTIME_DIR"] {
        let path = std::env::var_os(name)
            .map(PathBuf::from)
            .ok_or("Private XDG roots are required for native preview")?;
        if !path.is_absolute()
            || !path.is_dir()
            || !path.to_string_lossy().contains("fw05-native-updater-ui")
        {
            return Err(format!(
                "{name} must be an existing private fw05-native-updater-ui directory"
            )
            .into());
        }
    }
    let theme = match args[1].as_str() {
        "dark" => ThemePreference::Dark,
        "light" => ThemePreference::Light,
        _ => return Err("Unknown theme".into()),
    };
    let (width, height) = args[2].split_once('x').ok_or("Invalid size")?;
    let width: f32 = width.parse()?;
    let height: f32 = height.parse()?;
    let scale: f32 = args[3].parse()?;
    if ![1.0, 1.25, 2.0].contains(&scale) {
        return Err("Scale must be 1, 1.25 or 2".into());
    }
    let compact = match args[4].as_str() {
        "normal" => false,
        "compact" => true,
        _ => return Err("Unknown density".into()),
    };
    let scene = args[0].clone();
    let screenshot = PathBuf::from(&args[5]);
    if !screenshot.is_absolute() || screenshot.exists() {
        return Err("Screenshot must be a new absolute output path".into());
    }
    let submissions = Arc::new(AtomicUsize::new(0));
    eframe::run_native(
        "GP200 Studio • native updater preview",
        eframe::NativeOptions {
            persist_window: false,
            glow_options: eframe::egui_glow::GlowConfiguration {
                vsync: false,
                ..Default::default()
            },
            viewport: egui::ViewportBuilder::default()
                .with_inner_size([width * scale, height * scale])
                .with_min_inner_size([640.0 * scale, 480.0 * scale]),
            ..Default::default()
        },
        Box::new(move |cc| {
            gp200_gui::theme::install_fonts(&cc.egui_ctx);
            let preferences = AppPreferences {
                theme,
                scale,
                compact,
                ..Default::default()
            };
            let controller = gp200_app::AppController::with_demo(
                gp200_sim::captured_demo_snapshot(),
                preferences.clone(),
            )
            .map_err(|e| e.to_string())?;
            let services = RuntimeServices::with_preferences(
                Box::new(NoDevice(submissions.clone())),
                no_wake(),
                preferences,
            );
            let mut app = StudioApp::with_services(controller, services);
            app.open_firmware_updater_preview(&scene)?;
            Ok(Box::new(Preview {
                app,
                submissions,
                screenshot,
                started: Instant::now(),
                frames: 0,
                screenshot_requested: false,
                saved: None,
            }))
        }),
    )?;
    println!(
        "{}",
        serde_json::json!({"native_preview_finished":true,"hardware_backend_constructed":false,"physical_update_requests":0})
    );
    Ok(())
}
