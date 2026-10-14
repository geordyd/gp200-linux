//! Native recovery preview using a real private journal and simulator only.
use eframe::egui;
use gp200_app::{AppController, ComparisonSlot};
use gp200_domain::*;
use gp200_gui::{LaunchOptions, StudioApp};
use gp200_library::DraftRecoveryStore;

struct Preview {
    app: StudioApp,
    recover: bool,
    recovered: bool,
}
impl eframe::App for Preview {
    fn logic(&mut self, context: &egui::Context, frame: &mut eframe::Frame) {
        self.app.logic(context, frame);
        if self.recover
            && !self.recovered
            && self.app.recovery_review_available()
            && self.app.controller.state().connection == ConnectionState::Ready
            && self.app.controller.state().confirmed_buffer.is_some()
            && !self.app.controller.device_operation_pending()
        {
            self.app.recover_previous_offline();
            self.recovered = true;
        }
    }
    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        self.app.ui(ui, frame);
    }
    fn on_exit(&mut self, gl: Option<&eframe::glow::Context>) {
        self.app.on_exit(gl);
    }
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() != 4 || !matches!(args[0].as_str(), "review" | "recovered") {
        return Err(
            "Usage: recovery_ui_preview review|recovered dark|light WIDTHxHEIGHT SCREENSHOT".into(),
        );
    }
    if std::env::var_os("XDG_DATA_HOME").is_none() || std::env::var_os("XDG_CONFIG_HOME").is_none()
    {
        return Err("Preview requires isolated XDG_DATA_HOME and XDG_CONFIG_HOME".into());
    }
    let options = LaunchOptions::parse([
        "--demo".into(),
        "--view".into(),
        "editor".into(),
        "--theme".into(),
        args[1].clone(),
        "--size".into(),
        args[2].clone(),
        "--screenshot".into(),
        args[3].clone(),
    ])?;
    let status = options.qa_result.clone();
    let mut previous = AppController::with_demo(
        gp200_sim::captured_demo_snapshot(),
        AppPreferences::default(),
    )?;
    let first = previous.active_document_id().unwrap().clone();
    previous.capture_comparison(&first, ComparisonSlot::A)?;
    previous.dispatch(AppInput::Ui(UiIntent::ApplyEdit {
        document: first.clone(),
        patch: EditPatch::SetMetadata(MetadataPatch {
            name: Some("Night drive lead".into()),
            notes: Some(
                "Original off-grid wah Position retained; revised local stage notes.".into(),
            ),
            native_pan: Some(PatchPan::new(-14)?),
            ..Default::default()
        }),
    }));
    previous.capture_comparison(&first, ComparisonSlot::B)?;
    previous.dispatch(AppInput::Ui(UiIntent::NewOfflineFrom(
        gp200_sim::captured_demo_snapshot(),
    )));
    let second = previous.active_document_id().unwrap().clone();
    previous.dispatch(AppInput::Ui(UiIntent::ApplyEdit {
        document: second,
        patch: EditPatch::SetMetadata(MetadataPatch {
            name: Some("Ambient break".into()),
            notes: Some("Retained second document with an independent edit history.".into()),
            ..Default::default()
        }),
    }));
    previous.activate_document(&first);
    let mut session = previous.recovery_session();
    // Descriptive uncertainty is a synthetic render fixture. It never carries
    // a device operation or validated stamp and is labelled as such in QA.
    session.documents[0].original_target = EditTarget::DeviceBuffer;
    session.documents[0].uncertainty.push(RecoveryUncertainty {
        original_intent: 17,
        revision: Revision(1),
        description:
            "Preview fixture: readback was pending at the last snapshot; pedal result unknown."
                .into(),
        patch: None,
        actual: None,
    });
    session.session_uncertainty.push("Preview fixture: a Save result was not confirmed. Read the pedal manually before making another change.".into());
    {
        let store = DraftRecoveryStore::open(gp200_library::data_dir()?.join("draft-recovery"))?;
        store.save(&session)?;
    }
    let recover = args[0] == "recovered";
    let size = [options.width.unwrap(), options.height.unwrap()];
    eframe::run_native(
        "GP200 Studio · offline recovery preview",
        eframe::NativeOptions {
            persist_window: false,
            glow_options: eframe::egui_glow::GlowConfiguration {
                vsync: false,
                ..Default::default()
            },
            viewport: egui::ViewportBuilder::default().with_inner_size(size),
            ..Default::default()
        },
        Box::new(move |cc| {
            StudioApp::new(cc, options)
                .map(|mut app| {
                    if !recover {
                        app.open_recovery_review();
                    }
                    Box::new(Preview {
                        app,
                        recover,
                        recovered: false,
                    }) as Box<dyn eframe::App>
                })
                .map_err(Into::into)
        }),
    )?;
    if status.load(std::sync::atomic::Ordering::Relaxed) != 0 {
        return Err("Recovery native preview failed".into());
    }
    Ok(())
}
