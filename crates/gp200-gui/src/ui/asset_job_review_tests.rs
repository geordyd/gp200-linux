use super::*;
use std::{
    fs,
    sync::atomic::{AtomicU64, AtomicUsize},
    time::Duration,
};
const WAV: &[u8] =
    include_bytes!("../../../../tests/fixtures/device-assets/synthetic-ir-pcm24.wav");
static NEXT: AtomicU64 = AtomicU64::new(0);
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
struct Directory(PathBuf);
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn source() -> (Directory, PathBuf) {
    let path = std::env::temp_dir().join(format!(
        "gp200-asset-job-ui-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir(&path).unwrap();
    let file = path.join("source.wav");
    fs::write(&file, WAV).unwrap();
    (Directory(path), file)
}
fn app(ready: bool) -> (StudioApp, Arc<AtomicUsize>) {
    let mut controller = gp200_app::AppController::with_demo(
        gp200_sim::captured_demo_snapshot(),
        AppPreferences::default(),
    )
    .unwrap();
    if ready {
        let epoch = SessionEpoch(5);
        let capabilities = Capabilities::simulator();
        let identity = DeviceIdentity {
            model: DeviceModel::Gp200,
            mode: BackendMode::Simulator,
            firmware: None,
            display_name: "Synthetic review fixture".into(),
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
        let slot = AssetSlot::new(AssetBank::Ir, 19).unwrap();
        let effects = controller.dispatch(AppInput::Ui(UiIntent::RefreshAssetMetadata(slot)));
        let Effect::Device(DeviceCommand::ReadAssetMetadata { stamp, .. }) = &effects[0] else {
            panic!("metadata read")
        };
        let mut body = vec![0; 24];
        body[..2].copy_from_slice(&19_u16.to_le_bytes());
        body[8..15].copy_from_slice(b"User IR");
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
                        command: 0x1009,
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
    }
    let commands = Arc::new(AtomicUsize::new(0));
    (
        StudioApp::with_services(
            controller,
            crate::runtime::RuntimeServices::new(Box::new(NoDevice(commands.clone())), no_wake()),
        ),
        commands,
    )
}
fn finish(app: &mut StudioApp) {
    let start = Instant::now();
    while app.asset_job_ui.busy() {
        app.advance_asset_job_review();
        assert!(start.elapsed() < Duration::from_secs(5));
        std::thread::sleep(Duration::from_millis(2));
    }
}
fn render(app: &mut StudioApp, ctx: &egui::Context, events: Vec<egui::Event>) {
    let mut output = ctx.run_ui(
        egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(960.0, 640.0),
            )),
            events,
            ..Default::default()
        },
        |ui| app.asset_job_review_workspace(ui),
    );
    assert!(!output.shapes.is_empty());
    output.textures_delta.clear();
}
fn click(app: &mut StudioApp, ctx: &egui::Context, point: egui::Pos2) {
    for pressed in [true, false] {
        render(
            app,
            ctx,
            vec![
                egui::Event::PointerMoved(point),
                egui::Event::PointerButton {
                    pos: point,
                    button: egui::PointerButton::Primary,
                    pressed,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
        );
    }
}
#[test]
fn actual_inspect_and_prepare_button_clicks_seal_a_review_and_submit_zero_device_commands() {
    let (_dir, path) = source();
    let (mut app, count) = app(true);
    let original = app.controller.active_document().unwrap().draft.clone();
    app.selected_asset_kind = AssetKind::ImpulseResponse;
    app.selected_asset_slot = 19;
    app.asset_job_ui.path = path.display().to_string();
    let ctx = egui::Context::default();
    crate::theme::install_fonts(&ctx);
    for theme in [ThemePreference::Dark, ThemePreference::Light] {
        crate::theme::apply(&ctx, theme, 1.0);
        render(&mut app, &ctx, vec![]);
        render(&mut app, &ctx, vec![]);
        let point = app.asset_job_ui.inspect_rect.unwrap().center();
        click(&mut app, &ctx, point);
        finish(&mut app);
        assert!(app.asset_job_ui.local.is_some());
        render(&mut app, &ctx, vec![]);
        render(&mut app, &ctx, vec![]);
        let point = app.asset_job_ui.prepare_rect.unwrap().center();
        click(&mut app, &ctx, point);
        finish(&mut app);
        let prepared = app.asset_job_ui.prepared.as_ref().unwrap();
        assert_eq!(prepared.target().index(), 19);
        assert_eq!(
            prepared.recipe().derived_sha256(),
            JobSha256::of(include_bytes!(
                "../../../../tests/fixtures/device-assets/synthetic-ir-coefficients.bin"
            ))
        );
    }
    assert_eq!(count.load(Ordering::SeqCst), 0);
    assert!(!app.services.has_pending());
    assert!(!app.controller.device_operation_pending());
    assert_eq!(app.controller.active_document().unwrap().draft, original);
}
#[test]
fn offline_inspection_stays_useful_but_cannot_prepare_or_claim_slot_occupancy() {
    let (_dir, path) = source();
    let (mut app, count) = app(false);
    app.open_asset_job_review(AssetJobWorkspaceLaunch {
        source: path,
        bank: AssetBank::Ir,
        slot: 19,
        name: "IR".into(),
    });
    finish(&mut app);
    app.prepare_asset_job_review();
    assert!(!app.asset_job_ui.busy());
    assert!(app.asset_job_ui.prepared.is_none());
    let sealed = app.asset_job_review_report().unwrap();
    let report = &sealed["review"];
    assert_eq!(report["occupancy_known"], false);
    assert_eq!(report["full_content_backup_verified"], false);
    assert_eq!(report["permits_transfer"], false);
    assert!(report["prerequisites"].as_array().unwrap().len() >= 3);
    assert_eq!(count.load(Ordering::SeqCst), 0);
}
#[test]
fn slot_name_source_and_session_changes_retire_a_preparation_without_replay() {
    let (_dir, path) = source();
    let (mut app, count) = app(true);
    app.open_asset_job_review(AssetJobWorkspaceLaunch {
        source: path.clone(),
        bank: AssetBank::Ir,
        slot: 19,
        name: "IR".into(),
    });
    finish(&mut app);
    app.prepare_asset_job_review();
    finish(&mut app);
    assert!(app.asset_job_ui.prepared.is_some());
    app.asset_job_ui.name = "Different".into();
    app.advance_asset_job_review();
    assert!(app.asset_job_ui.local.is_none());
    assert!(app.asset_job_ui.prepared.is_none());
    app.open_asset_job_review(AssetJobWorkspaceLaunch {
        source: path,
        bank: AssetBank::Ir,
        slot: 19,
        name: "IR".into(),
    });
    finish(&mut app);
    app.prepare_asset_job_review();
    finish(&mut app);
    app.controller
        .dispatch(AppInput::Device(DeviceEvent::ConnectionChanged {
            epoch: SessionEpoch(6),
            state: ConnectionState::Disconnected,
        }));
    app.advance_asset_job_review();
    assert!(app.asset_job_ui.prepared.is_none());
    assert!(app.asset_job_ui.local.is_some());
    app.selected_asset_slot = 18;
    app.advance_asset_job_review();
    assert!(app.asset_job_ui.local.is_none());
    assert_eq!(count.load(Ordering::SeqCst), 0);
}
#[test]
fn cancelled_and_out_of_order_replies_cannot_replace_a_new_selection() {
    let (_dir, path) = source();
    let (mut app, count) = app(true);
    app.open_asset_job_review(AssetJobWorkspaceLaunch {
        source: path,
        bank: AssetBank::Ir,
        slot: 19,
        name: "IR".into(),
    });
    finish(&mut app);
    let old_local = app.asset_job_ui.local.clone().unwrap();
    let selected = app.asset_job_selection();
    let read = context(&app.controller, selected.target());
    app.asset_job_ui.pending = Some((100, Arc::new(AtomicBool::new(false)), None));
    app.asset_job_ui.accept(
        Reply {
            id: 99,
            result: Ok(Completed::Inspected(Box::new(old_local.clone()))),
        },
        &selected,
        &read,
        &app.controller.state().capabilities,
    );
    assert!(app.asset_job_ui.busy());
    app.asset_job_ui.cancel();
    app.asset_job_ui.local = None;
    app.asset_job_ui.accept(
        Reply {
            id: 100,
            result: Ok(Completed::Inspected(Box::new(old_local))),
        },
        &selected,
        &read,
        &app.controller.state().capabilities,
    );
    assert!(app.asset_job_ui.local.is_none());
    assert_eq!(count.load(Ordering::SeqCst), 0);
}
#[test]
fn exported_report_is_bound_to_bytes_and_refuses_source_or_existing_destination() {
    let (dir, path) = source();
    let (mut app, _count) = app(true);
    app.open_asset_job_review(AssetJobWorkspaceLaunch {
        source: path.clone(),
        bank: AssetBank::Ir,
        slot: 19,
        name: "IR".into(),
    });
    finish(&mut app);
    app.prepare_asset_job_review();
    finish(&mut app);
    let selection = app.asset_job_selection();
    let read = context(&app.controller, selection.target());
    let report = app
        .asset_job_ui
        .report(&read, &app.controller.state().capabilities)
        .unwrap();
    let out = dir.0.join("review.json");
    app.asset_job_ui.export_pending = Some(report.clone());
    app.asset_job_ui.export_to(out.clone());
    finish(&mut app);
    let retained: serde_json::Value = serde_json::from_slice(&fs::read(&out).unwrap()).unwrap();
    assert_eq!(retained["review"]["prepared"]["derived_bytes"], 4096);
    assert_eq!(retained["review"]["hardware_commands"], 0);
    let hash = JobSha256::of(&fs::read(&out).unwrap());
    app.asset_job_ui.export_pending = Some(report.clone());
    app.asset_job_ui.export_to(out.clone());
    finish(&mut app);
    assert_eq!(JobSha256::of(&fs::read(&out).unwrap()), hash);
    app.asset_job_ui.export_pending = Some(report);
    app.asset_job_ui.export_to(path.clone());
    finish(&mut app);
    assert_eq!(fs::read(path).unwrap(), WAV);
}
#[test]
fn new_draft_references_retire_preparation_and_are_hash_bound_in_the_report() {
    let (_dir, path) = source();
    let (mut app, count) = app(true);
    app.open_asset_job_review(AssetJobWorkspaceLaunch {
        source: path,
        bank: AssetBank::Ir,
        slot: 19,
        name: "IR".into(),
    });
    finish(&mut app);
    app.prepare_asset_job_review();
    finish(&mut app);
    let original = app.asset_job_review_report().unwrap()["report_sha256"].clone();
    let id = app.controller.active_document_id().unwrap().clone();
    let effects = app.controller.dispatch(AppInput::Ui(UiIntent::ApplyEdit {
        document: id,
        patch: EditPatch::SetModel {
            module: ModuleId::new(5).unwrap(),
            model: ModelCode(0x0a100013),
        },
    }));
    assert!(effects.is_empty());
    app.advance_asset_job_review();
    assert!(app.asset_job_ui.prepared.is_none());
    let report = app.asset_job_review_report().unwrap();
    assert_eq!(
        report["review"]["affected_presets"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_ne!(report["report_sha256"], original);
    assert_eq!(report["review"]["fresh_stored_presets_checked"], 0);
    assert_eq!(count.load(Ordering::SeqCst), 0);
}
#[test]
fn a_matched_request_with_changed_metadata_context_discards_its_valid_prepared_result() {
    let (_dir, path) = source();
    let (mut app, count) = app(true);
    app.open_asset_job_review(AssetJobWorkspaceLaunch {
        source: path,
        bank: AssetBank::Ir,
        slot: 19,
        name: "IR".into(),
    });
    finish(&mut app);
    app.prepare_asset_job_review();
    finish(&mut app);
    let old = app.asset_job_ui.prepared.take().unwrap();
    let selection = app.asset_job_selection();
    let before = context(&app.controller, selection.target());
    app.asset_job_ui.pending = Some((101, Arc::new(AtomicBool::new(false)), Some(before.digest)));
    app.controller
        .dispatch(AppInput::Device(DeviceEvent::ConnectionChanged {
            epoch: SessionEpoch(6),
            state: ConnectionState::Disconnected,
        }));
    let after = context(&app.controller, selection.target());
    app.asset_job_ui.accept(
        Reply {
            id: 101,
            result: Ok(Completed::Prepared(old)),
        },
        &selection,
        &after,
        &app.controller.state().capabilities,
    );
    assert!(app.asset_job_ui.prepared.is_none());
    assert!(app.asset_job_ui.message.contains("context changed"));
    assert_eq!(count.load(Ordering::SeqCst), 0);
}
