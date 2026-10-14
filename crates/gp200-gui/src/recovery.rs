//! Editing snapshots and review lifecycle. Disk work lives in DraftRecoveryWorker.
use crate::StudioApp;
use gp200_domain::*;
use gp200_library::{DraftRecoveryCommand, DraftRecoveryEvent, RetainedDraftRecovery};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};

pub(crate) struct RecoveryUi {
    pub found: Option<RetainedDraftRecovery>,
    pub review_open: bool,
    pub error: Option<String>,
    pub ready: bool,
    pub busy: bool,
    pub last_saved: Option<Arc<RecoverySession>>,
    pub startup_document: Option<DocumentId>,
    pub replace_startup: bool,
    pub visible_uncertain_hash: Option<String>,
    last_check: Instant,
    #[cfg(test)]
    pub recover_rect: Option<eframe::egui::Rect>,
}
impl Default for RecoveryUi {
    fn default() -> Self {
        Self {
            found: None,
            review_open: false,
            error: None,
            ready: false,
            busy: false,
            last_saved: None,
            startup_document: None,
            replace_startup: true,
            visible_uncertain_hash: None,
            last_check: Instant::now(),
            #[cfg(test)]
            recover_rect: None,
        }
    }
}
impl RecoveryUi {
    pub(crate) fn with_startup(startup_document: Option<DocumentId>) -> Self {
        Self {
            startup_document,
            ..Self::default()
        }
    }
}
impl StudioApp {
    pub(crate) fn advance_draft_recovery(&mut self) {
        let Some(worker) = &self.services.draft_recovery else {
            return;
        };
        while let Some(event) = worker.try_next_event() {
            match event {
                DraftRecoveryEvent::Discovered(found) => {
                    self.recovery_ui.found = found;
                    self.recovery_ui.ready = true;
                }
                DraftRecoveryEvent::Saved(saved) => {
                    self.recovery_ui.last_saved = Some(saved.session);
                    self.recovery_ui.busy = false;
                    self.recovery_ui.error = None;
                    self.recovery_ui.visible_uncertain_hash = None;
                }
                DraftRecoveryEvent::Discarded => {
                    self.recovery_ui.found = None;
                    self.recovery_ui.review_open = false;
                    self.recovery_ui.busy = false;
                    self.recovery_ui.last_saved = None;
                }
                DraftRecoveryEvent::Failed(error) => {
                    self.recovery_ui.busy = false;
                    self.recovery_ui.error = Some(std::format!(
                        "{}: {}. Any existing journal has been kept.",
                        if self.recovery_ui.ready {
                            "Draft recovery was not saved"
                        } else {
                            "Previous draft recovery could not be opened"
                        },
                        error.message
                    ));
                    // A discovery failure leaves ready=false: never overwrite
                    // an unsupported or damaged previous-session journal.
                }
                DraftRecoveryEvent::PublicationUncertain { visible, error } => {
                    self.recovery_ui.busy = false;
                    self.recovery_ui.visible_uncertain_hash = visible.map(|journal| journal.sha256);
                    self.recovery_ui.error = Some(std::format!(
                        "{}. Keep the app open while recovery retries, or capture important work to the library.",
                        error.message
                    ));
                    // A readable new snapshot does not confirm crash durability.
                }
                DraftRecoveryEvent::DiscardUncertain(error) => {
                    self.recovery_ui.found = None;
                    self.recovery_ui.review_open = false;
                    self.recovery_ui.busy = false;
                    self.recovery_ui.last_saved = None;
                    self.recovery_ui.error = Some(error.message);
                }
            }
        }
        if !self.recovery_ui.ready
            || self.recovery_ui.busy
            || self.recovery_ui.found.is_some()
            || self.recovery_ui.last_check.elapsed() < Duration::from_millis(500)
        {
            return;
        }
        self.recovery_ui.last_check = Instant::now();
        let session = Arc::new(self.controller.recovery_session());
        if self.recovery_ui.last_saved.as_deref() == Some(session.as_ref()) {
            return;
        }
        match worker.try_submit(DraftRecoveryCommand::Save(session)) {
            Ok(()) => self.recovery_ui.busy = true,
            Err(SubmitError::Busy(_)) => {}
            Err(SubmitError::Disconnected(_)) => {
                self.recovery_ui.ready = false;
                self.recovery_ui.error = Some(crate::localization::text("Draft recovery worker stopped. Capture important work to the library before closing.").into());
            }
        }
    }
    pub fn recover_previous_offline(&mut self) {
        let Some(found) = self.recovery_ui.found.clone() else {
            return;
        };
        let replace = self
            .recovery_ui
            .replace_startup
            .then_some(self.recovery_ui.startup_document.as_ref())
            .flatten();
        match self
            .controller
            .recover_offline_replacing_pristine(&found.session, replace)
        {
            Ok(_) => {
                self.recovery_ui.found = None;
                self.recovery_ui.review_open = false;
                self.recovery_ui.last_saved = None;
                self.notice = crate::localization::text("Previous documents recovered offline. Read the pedal manually to reconcile uncertain operations.").into();
                // Keep the old complete journal until the worker publishes the
                // new session containing the recovered documents atomically.
            }
            Err(error) => self.recovery_ui.error = Some(error.message),
        }
    }
    pub(crate) fn discard_previous_recovery(&mut self) {
        let (Some(found), Some(worker)) = (&self.recovery_ui.found, &self.services.draft_recovery)
        else {
            return;
        };
        match worker.try_submit(DraftRecoveryCommand::Discard {
            reviewed_sha256: found.sha256.clone(),
        }) {
            Ok(()) => self.recovery_ui.busy = true,
            Err(_) => self.recovery_ui.error = Some(
                crate::localization::text("Recovery worker is busy. The previous session has been kept; retry the review.")
                    .into(),
            ),
        }
    }
    /// Preview API uses a real retained journal loaded by its worker, without
    /// bypassing normal discovery, validation or recovery authority boundaries.
    pub fn open_recovery_review(&mut self) {
        self.recovery_ui.review_open = true;
    }
    pub fn recovery_review_available(&self) -> bool {
        self.recovery_ui.found.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::RuntimeServices;
    use gp200_app::AppController;
    use gp200_library::{DraftRecoveryStore, DraftRecoveryWorker};
    use std::{
        fs,
        path::PathBuf,
        sync::atomic::{AtomicUsize, Ordering},
    };
    struct CountingDevice(Arc<AtomicUsize>);
    impl DeviceService for CountingDevice {
        fn try_submit(&self, command: DeviceCommand) -> Result<(), SubmitError<DeviceCommand>> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Err(SubmitError::Disconnected(command))
        }
        fn try_next_event(&self) -> Option<DeviceEvent> {
            None
        }
        fn shutdown(&self) {}
    }
    fn app() -> (StudioApp, Arc<AtomicUsize>) {
        let count = Arc::new(AtomicUsize::new(0));
        (
            StudioApp::with_services(
                AppController::with_demo(
                    gp200_sim::captured_demo_snapshot(),
                    AppPreferences::default(),
                )
                .unwrap(),
                RuntimeServices::new(Box::new(CountingDevice(count.clone())), Arc::new(|| {})),
            ),
            count,
        )
    }
    fn temp() -> PathBuf {
        std::env::temp_dir().join(format!(
            "gp200-recovery-ui-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }
    fn wait(app: &mut StudioApp, predicate: impl Fn(&StudioApp) -> bool) {
        let start = Instant::now();
        loop {
            app.advance_draft_recovery();
            if predicate(app) {
                return;
            }
            assert!(start.elapsed() < Duration::from_secs(4));
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    fn seed(root: &PathBuf) -> RetainedDraftRecovery {
        let (mut previous, _) = app();
        let id = previous.controller.active_document_id().unwrap().clone();
        previous
            .controller
            .dispatch(AppInput::Ui(UiIntent::ApplyEdit {
                document: id.clone(),
                patch: EditPatch::SetMetadata(MetadataPatch {
                    notes: Some("Exact previous work".into()),
                    ..Default::default()
                }),
            }));
        previous
            .controller
            .capture_comparison(&id, gp200_app::ComparisonSlot::A)
            .unwrap();
        DraftRecoveryStore::open(root)
            .unwrap()
            .save(&previous.controller.recovery_session())
            .unwrap()
    }
    fn render(app: &mut StudioApp, ctx: &eframe::egui::Context, events: Vec<eframe::egui::Event>) {
        let mut out = ctx.run_ui(
            eframe::egui::RawInput {
                screen_rect: Some(eframe::egui::Rect::from_min_size(
                    eframe::egui::Pos2::ZERO,
                    eframe::egui::vec2(960.0, 640.0),
                )),
                events,
                ..Default::default()
            },
            |ui| app.render(ui),
        );
        assert!(!out.shapes.is_empty());
        out.textures_delta.clear();
    }
    #[test]
    fn startup_review_prevents_overwrite_and_real_recover_button_opens_only_offline() {
        let root = temp();
        let retained = seed(&root);
        let bytes = fs::read(root.join("session.gp200recovery")).unwrap();
        let (mut app, count) = app();
        app.services.draft_recovery =
            Some(DraftRecoveryWorker::spawn(root.clone(), Arc::new(|| {})).unwrap());
        wait(&mut app, |app| app.recovery_review_available());
        app.recovery_ui.last_check = Instant::now() - Duration::from_secs(1);
        app.advance_draft_recovery();
        assert_eq!(fs::read(root.join("session.gp200recovery")).unwrap(), bytes);
        assert!(app.recovery_ui.last_saved.is_none());
        let ctx = eframe::egui::Context::default();
        crate::theme::install_fonts(&ctx);
        app.open_recovery_review();
        render(&mut app, &ctx, vec![]);
        render(&mut app, &ctx, vec![]);
        assert_eq!(app.controller.state().documents.len(), 1);
        assert_eq!(count.load(Ordering::SeqCst), 0);
        let point = app.recovery_ui.recover_rect.unwrap().center();
        for pressed in [true, false] {
            render(
                &mut app,
                &ctx,
                vec![
                    eframe::egui::Event::PointerMoved(point),
                    eframe::egui::Event::PointerButton {
                        pos: point,
                        button: eframe::egui::PointerButton::Primary,
                        pressed,
                        modifiers: eframe::egui::Modifiers::NONE,
                    },
                ],
            );
        }
        assert!(!app.recovery_review_available());
        assert_eq!(app.controller.state().documents.len(), 1);
        assert_eq!(
            app.controller.active_document().unwrap().draft,
            retained.session.documents[0].draft
        );
        assert!(matches!(
            app.controller.active_document().unwrap().target,
            EditTarget::Offline(_)
        ));
        assert!(!app.services.has_pending());
        assert_eq!(count.load(Ordering::SeqCst), 0);
        app.recovery_ui.last_check = Instant::now() - Duration::from_secs(1);
        wait(&mut app, |app| app.recovery_ui.last_saved.is_some());
        assert_eq!(
            app.recovery_ui.last_saved.as_deref().unwrap(),
            &app.controller.recovery_session()
        );
        drop(app);
        std::thread::sleep(Duration::from_millis(20));
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn explicit_discard_acknowledges_only_journal_and_keeps_current_draft() {
        let root = temp();
        seed(&root);
        let (mut app, count) = app();
        let current = app.controller.recovery_session();
        app.services.draft_recovery =
            Some(DraftRecoveryWorker::spawn(root.clone(), Arc::new(|| {})).unwrap());
        wait(&mut app, |app| app.recovery_review_available());
        app.discard_previous_recovery();
        wait(&mut app, |app| {
            !app.recovery_review_available() && !app.recovery_ui.busy
        });
        assert_eq!(app.controller.recovery_session(), current);
        assert_eq!(count.load(Ordering::SeqCst), 0);
        drop(app);
        std::thread::sleep(Duration::from_millis(20));
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn damaged_discovery_stays_visible_and_is_not_overwritten_by_current_session() {
        let root = temp();
        seed(&root);
        let path = root.join("session.gp200recovery");
        fs::write(&path, b"partial journal").unwrap();
        let (mut app, count) = app();
        app.services.draft_recovery =
            Some(DraftRecoveryWorker::spawn(root.clone(), Arc::new(|| {})).unwrap());
        wait(&mut app, |app| app.recovery_ui.error.is_some());
        assert!(!app.recovery_ui.ready);
        app.recovery_ui.last_check = Instant::now() - Duration::from_secs(1);
        app.advance_draft_recovery();
        assert_eq!(fs::read(&path).unwrap(), b"partial journal");
        assert_eq!(count.load(Ordering::SeqCst), 0);
        let ctx = eframe::egui::Context::default();
        render(&mut app, &ctx, vec![]);
        assert!(app.recovery_ui.error.as_ref().unwrap().contains("retained"));
        drop(app);
        std::thread::sleep(Duration::from_millis(20));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn normal_close_flushes_latest_frame_without_waiting_for_autosave_timer() {
        let root = temp();
        let (mut app, count) = app();
        app.services.draft_recovery =
            Some(DraftRecoveryWorker::spawn(root.clone(), Arc::new(|| {})).unwrap());
        wait(&mut app, |app| app.recovery_ui.ready);
        let id = app.controller.active_document_id().unwrap().clone();
        app.controller.dispatch(AppInput::Ui(UiIntent::ApplyEdit {
            document: id,
            patch: EditPatch::SetMetadata(MetadataPatch {
                notes: Some("Last editing frame before close".into()),
                ..Default::default()
            }),
        }));
        let final_frame = app.controller.recovery_session();
        eframe::App::on_exit(&mut app, None);
        assert_eq!(count.load(Ordering::SeqCst), 0);
        drop(app);
        let start = Instant::now();
        let store = loop {
            match DraftRecoveryStore::open(&root) {
                Ok(store) => break store,
                Err(error)
                    if error.kind == ErrorKind::Busy
                        && start.elapsed() < Duration::from_secs(2) =>
                {
                    std::thread::sleep(Duration::from_millis(5))
                }
                Err(error) => panic!("{error}"),
            }
        };
        assert_eq!(*store.discover().unwrap().unwrap().session, final_frame);
        drop(store);
        fs::remove_dir_all(root).unwrap();
    }
}
