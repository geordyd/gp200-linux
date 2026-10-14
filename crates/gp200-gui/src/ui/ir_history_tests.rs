//! IR history real-widget/worker regression tests. No native input injection or hardware.
use super::*;
#[path = "../../../gp200-library/tests/common/ir_history_fixture.rs"]
mod fixture;
use egui::accesskit::{Node, NodeId};
use fixture::*;
use std::{
    collections::BTreeMap,
    sync::atomic::{AtomicUsize, Ordering},
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
fn app(root: &TempRoot) -> (StudioApp, Arc<AtomicUsize>) {
    let count = Arc::new(AtomicUsize::new(0));
    let preferences = AppPreferences::default();
    let controller = gp200_app::AppController::with_demo(
        gp200_sim::captured_demo_snapshot(),
        preferences.clone(),
    )
    .unwrap();
    let wake: WakeUi = Arc::new(|| {});
    let mut services = crate::runtime::RuntimeServices::with_preferences_path(
        Box::new(NoDevice(count.clone())),
        wake.clone(),
        preferences,
        root.0.join("preferences.json"),
    );
    let worker = LibraryWorker::open(&root.0, wake).unwrap();
    services.workflows = Some(worker.workflow_client());
    services.library = Some(Box::new(worker));
    (StudioApp::with_services(controller, services), count)
}
fn settle(app: &mut StudioApp) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while app.ir_history_ui.local_read_pending() || app.ir_history_ui.export_pending.is_some() {
        for event in app.services.pump_workflows() {
            if !app.observe_ir_history_event(&event) {
                app.observe_workflow_event(event);
            }
        }
        assert!(Instant::now() < deadline, "Local IR UI did not settle");
        std::thread::sleep(Duration::from_millis(2));
    }
}
fn frame(
    app: &mut StudioApp,
    ctx: &egui::Context,
    events: Vec<egui::Event>,
) -> BTreeMap<NodeId, Node> {
    workspace_frame(app, ctx, events, false)
}
fn workspace_frame(
    app: &mut StudioApp,
    ctx: &egui::Context,
    events: Vec<egui::Event>,
    assets: bool,
) -> BTreeMap<NodeId, Node> {
    let mut output = ctx.run_ui(
        egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(960.0, 640.0),
            )),
            events,
            ..Default::default()
        },
        |ui| {
            if assets {
                app.assets(ui);
            } else {
                app.ir_history_workspace(ui);
            }
        },
    );
    assert!(!output.shapes.is_empty());
    output.textures_delta.clear();
    output
        .platform_output
        .accesskit_update
        .take()
        .map(|update| update.nodes.into_iter().collect())
        .unwrap_or_default()
}
fn activate_label(app: &mut StudioApp, ctx: &egui::Context, label: &str, assets: bool) {
    let mut nodes = workspace_frame(app, ctx, vec![], assets);
    for _ in 0..100 {
        if let Some(focus) = ctx.memory(|memory| memory.focused())
            && nodes.get(&focus.accesskit_id()).and_then(Node::label) == Some(label)
        {
            for pressed in [true, false] {
                workspace_frame(
                    app,
                    ctx,
                    vec![egui::Event::Key {
                        key: egui::Key::Enter,
                        physical_key: Some(egui::Key::Enter),
                        pressed,
                        repeat: false,
                        modifiers: egui::Modifiers::NONE,
                    }],
                    assets,
                );
            }
            return;
        }
        for pressed in [true, false] {
            let next = workspace_frame(
                app,
                ctx,
                vec![egui::Event::Key {
                    key: egui::Key::Tab,
                    physical_key: Some(egui::Key::Tab),
                    pressed,
                    repeat: false,
                    modifiers: egui::Modifiers::NONE,
                }],
                assets,
            );
            if !next.is_empty() {
                nodes.extend(next);
            }
        }
    }
    panic!("Widget {label} was unreachable");
}
#[test]
fn assets_history_tab_keeps_old_tabs_and_snaptone_separate_using_actual_keyboard_widgets() {
    let root = TempRoot::new("gui-history-tabs");
    let (mut app, count) = app(&root);
    app.selected_asset_kind = AssetKind::ImpulseResponse;
    let ctx = egui::Context::default();
    ctx.enable_accesskit();
    crate::theme::install_fonts(&ctx);
    crate::theme::apply(&ctx, ThemePreference::Dark, 1.0);
    activate_label(&mut app, &ctx, "IR history", true);
    settle(&mut app);
    assert!(app.ir_history_ui.open);
    assert!(!app.asset_job_ui.open);
    activate_label(&mut app, &ctx, "Device review", true);
    assert!(!app.ir_history_ui.open);
    assert!(app.asset_job_ui.open);
    activate_label(&mut app, &ctx, "Slots and sources", true);
    assert!(!app.ir_history_ui.open);
    assert!(!app.asset_job_ui.open);
    activate_label(&mut app, &ctx, "SnapTone", true);
    let nodes = workspace_frame(&mut app, &ctx, vec![], true);
    assert!(
        !nodes
            .values()
            .any(|node| node.label() == Some("IR history"))
    );
    assert_eq!(count.load(Ordering::SeqCst), 0);
}
#[test]
fn cancelling_the_history_dialog_clears_only_its_request_and_keeps_the_asset_export_folder() {
    let root = TempRoot::new("gui-history-cancel");
    let output = TempRoot::new("gui-history-remembered-output");
    let mut store = LibraryStore::open(&root.0).unwrap();
    let id = populate(&mut store, true);
    drop(store);
    let (mut app, count) = app(&root);
    app.open_ir_history_attempt(id.clone());
    settle(&mut app);
    let detail = app.ir_history_ui.detail.as_ref().unwrap().clone();
    app.ir_history_ui.export_dialog = Some(IrHistoryExportRequest {
        attempt: id,
        observed_snapshot_sha256: detail.snapshot_sha256.clone(),
        kind: detail.export_kind,
        destination: PathBuf::new(),
    });
    let purpose = FilePurpose::ExportIrHistory.directory_purpose();
    assert_eq!(purpose, DialogDirectoryPurpose::AssetExport);
    let mut directories = DialogDirectories::new();
    let selected = output.0.join("earlier-export.zip");
    assert!(crate::preferences::remember_selection(
        &mut directories,
        purpose,
        Some(&selected)
    ));
    let before = directories.clone();
    assert!(!crate::preferences::remember_selection(
        &mut directories,
        purpose,
        None
    ));
    assert_eq!(directories, before);
    assert_eq!(
        crate::preferences::initial_directory(&mut directories, purpose),
        Some(output.0.clone())
    );
    app.handle_file_selection(crate::runtime::FileSelection {
        purpose: FilePurpose::ExportIrHistory,
        path: None,
    });
    assert!(app.ir_history_ui.export_dialog.is_none());
    assert!(app.ir_history_ui.export_pending.is_none());
    assert!(Arc::ptr_eq(
        &detail,
        app.ir_history_ui.detail.as_ref().unwrap()
    ));
    assert_eq!(std::fs::read_dir(&output.0).unwrap().count(), 0);
    assert_eq!(count.load(Ordering::SeqCst), 0);
}
#[test]
fn mismatched_attempt_reply_and_old_roster_reply_cannot_replace_the_selected_detail() {
    let root = TempRoot::new("gui-history-mismatched-reply");
    let mut store = LibraryStore::open(&root.0).unwrap();
    let first = populate(&mut store, true);
    let second = populate(&mut store, true);
    let wrong = ir_history_detail(&mut store, &second, &|| false).unwrap();
    let page = ir_history_page(&mut store, IrHistoryPageRequest::default(), &|| false).unwrap();
    drop(store);
    let (mut app, count) = app(&root);
    app.refresh_ir_history();
    let list_intent = app.ir_history_ui.list_pending.unwrap();
    app.open_ir_history_attempt(first);
    let detail_intent = app.ir_history_ui.detail_pending.as_ref().unwrap().0;
    assert!(!app.observe_ir_history_event(&WorkflowEvent {
        intent: list_intent,
        result: Ok(WorkflowResult::IrHistoryPage(page))
    }));
    assert!(app.observe_ir_history_event(&WorkflowEvent {
        intent: detail_intent,
        result: Ok(WorkflowResult::IrHistoryDetail(wrong))
    }));
    assert!(app.ir_history_ui.detail.is_none());
    assert!(app.ir_history_ui.message.contains("differs"));
    assert_eq!(count.load(Ordering::SeqCst), 0);
}
#[test]
fn export_reply_requires_the_exact_pending_attempt_mode_path_snapshot_and_intent() {
    let root = TempRoot::new("gui-history-export-correlation");
    let output = TempRoot::new("gui-history-export-correlation-output");
    let mut store = LibraryStore::open(&root.0).unwrap();
    let first = populate(&mut store, true);
    let second = populate(&mut store, true);
    let detail = ir_history_detail(&mut store, &first, &|| false).unwrap();
    drop(store);
    let (mut app, count) = app(&root);
    let request = IrHistoryExportRequest {
        attempt: first,
        kind: detail.export_kind,
        observed_snapshot_sha256: detail.snapshot_sha256.clone(),
        destination: output.0.join("requested.zip"),
    };
    for mismatch in ["attempt", "mode", "path", "snapshot", "intent"] {
        let intent = IntentId(999);
        app.ir_history_ui.export_pending = Some((intent, request.clone()));
        app.ir_history_ui.message.clear();
        let mut receipt = IrHistoryExportReceipt {
            destination: request.destination.clone(),
            attempt: request.attempt.clone(),
            kind: request.kind,
            archive_sha256: ContentHash::of(b"archive"),
            archive_bytes: 7,
            source_snapshot_sha256: request.observed_snapshot_sha256.clone(),
            file_count: 1,
        };
        let mut reply_intent = intent;
        match mismatch {
            "attempt" => receipt.attempt = second.clone(),
            "mode" => receipt.kind = IrHistoryExportKind::RawDiagnostics,
            "path" => receipt.destination = output.0.join("unrelated.zip"),
            "snapshot" => receipt.source_snapshot_sha256 = ContentHash::of(b"other snapshot"),
            "intent" => reply_intent = IntentId(998),
            _ => unreachable!(),
        }
        let handled = app.observe_ir_history_event(&WorkflowEvent {
            intent: reply_intent,
            result: Ok(WorkflowResult::IrHistoryExport(receipt)),
        });
        if mismatch == "intent" {
            assert!(!handled);
            assert!(app.ir_history_ui.export_pending.is_some());
            assert!(app.ir_history_ui.message.is_empty());
        } else {
            assert!(handled);
            assert!(app.ir_history_ui.export_pending.is_none());
            assert!(app.ir_history_ui.message.contains("differs from"));
            assert!(!app.ir_history_ui.message.starts_with("Saved local"));
        }
    }
    assert_eq!(count.load(Ordering::SeqCst), 0);
}
#[test]
fn expanded_raw_statuses_preserve_pre_send_stale_mismatch_duplicate_and_payload_labels_with_wrapping()
 {
    let root = TempRoot::new("gui-history-raw-labels");
    let mut store = LibraryStore::open(&root.0).unwrap();
    let f = fixture();
    let mut ledger = IrAttemptLedger::open(&mut store).unwrap();
    let id = ledger.reserve_id().unwrap();
    let binding = IrAttemptBinding::from_prepared(id.clone(), &f.job).unwrap();
    let mut cursor = ledger
        .begin_reserved(&id, binding.clone(), &material(&f), 100)
        .unwrap();
    let mut context = binding.context().to_dto();
    context.session.epoch = SessionEpoch(6);
    cursor = ledger
        .append_observation(&cursor, status(context.clone(), 99, 20, 0x9001, 0xfedcba98))
        .unwrap();
    cursor = ledger
        .append_observation(&cursor, status(context, 101, 20, 0x9001, 0xfedcba98))
        .unwrap();
    ledger
        .close(&cursor, IrAttemptFinishReason::ObservationComplete)
        .unwrap();
    drop(ledger);
    let detail = ir_history_detail(&mut store, &id, &|| false).unwrap();
    let retained = detail.checked.as_ref().unwrap();
    for (theme, scale) in [(ThemePreference::Dark, 1.0), (ThemePreference::Light, 1.25)] {
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        crate::theme::install_fonts(&ctx);
        crate::theme::apply(&ctx, theme, scale);
        for theme in [egui::Theme::Dark, egui::Theme::Light] {
            ctx.style_mut_of(theme, |style| style.animation_time = 0.0);
        }
        let mut labels = Vec::new();
        let mut nodes = BTreeMap::new();
        let mut opened = std::collections::BTreeSet::new();
        let mut events = vec![];
        let mut release = None;
        for _ in 0..64 {
            let mut out = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(400.0, 1600.0),
                    )),
                    events: std::mem::take(&mut events),
                    ..Default::default()
                },
                |ui| {
                    render_observations(ui, retained);
                    let before = ui.available_rect_before_wrap().right();
                    ui.label(hex(&vec![0xab; 4096]));
                    assert!(
                        ui.min_rect().right() <= before + 1.0,
                        "Raw payload escaped the available width"
                    );
                },
            );
            out.textures_delta.clear();
            if let Some(update) = out.platform_output.accesskit_update {
                for (id, node) in update.nodes {
                    if let Some(label) = node.label() {
                        labels.push(label.to_owned());
                    }
                    if let Some(value) = node.value() {
                        labels.push(value.to_owned());
                    }
                    nodes.insert(id, node);
                }
            }
            let focused_label = ctx
                .memory(|memory| memory.focused())
                .and_then(|focus| nodes.get(&focus.accesskit_id()))
                .and_then(Node::label)
                .map(str::to_owned);
            if let Some(key) = release.take() {
                events = vec![egui::Event::Key {
                    key,
                    physical_key: Some(key),
                    pressed: false,
                    repeat: false,
                    modifiers: egui::Modifiers::NONE,
                }];
                continue;
            }
            let key = if focused_label
                .as_ref()
                .is_some_and(|label| label.starts_with("Observation ") && !opened.contains(label))
            {
                opened.insert(focused_label.unwrap());
                egui::Key::Enter
            } else {
                egui::Key::Tab
            };
            events = vec![egui::Event::Key {
                key,
                physical_key: Some(key),
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            }];
            release = Some(key);
        }
        assert_eq!(
            opened.len(),
            2,
            "Both raw observations must be opened using actual keyboard widgets"
        );
        assert!(
            labels
                .iter()
                .any(|label| label.contains("Raw slot: 20; flags: 36865; value: 4275878552")),
            "Expanded labels: {labels:?}"
        );
        assert!(
            labels
                .iter()
                .any(|label| label.contains("session matches: false; slot matches: false"))
        );
        assert!(
            labels
                .iter()
                .any(|label| label == "Received before any matching accounted send observation")
        );
        assert!(
            labels
                .iter()
                .any(|label| label.starts_with("Duplicate of earlier status"))
        );
        assert!(
            labels
                .iter()
                .any(|label| label.starts_with("Raw transfer kind:"))
        );
    }
}
#[test]
fn history_lists_and_opens_actual_local_evidence_without_any_device_command_or_draft_change() {
    let root = TempRoot::new("gui-history");
    let mut store = LibraryStore::open(&root.0).unwrap();
    let id = populate(&mut store, true);
    drop(store);
    let (mut app, count) = app(&root);
    let draft = app.controller.active_document().unwrap().draft.clone();
    app.refresh_ir_history();
    settle(&mut app);
    assert_eq!(app.ir_history_ui.page.as_ref().unwrap().rows[0].attempt, id);
    app.open_ir_history_attempt(id.clone());
    settle(&mut app);
    let detail = app.ir_history_ui.detail.as_ref().unwrap();
    assert_eq!(detail.attempt, id);
    assert_eq!(
        detail.checked.as_ref().unwrap().local_state,
        IrLocalAttemptState::LocalUnverified
    );
    assert_eq!(app.controller.active_document().unwrap().draft, draft);
    assert!(app.controller.state().prepared_jobs.is_empty());
    assert_eq!(count.load(Ordering::SeqCst), 0);
}
#[test]
fn out_of_order_local_detail_reply_cannot_replace_a_new_selection() {
    let root = TempRoot::new("gui-history-order");
    let mut store = LibraryStore::open(&root.0).unwrap();
    let first = populate(&mut store, true);
    let second = populate(&mut store, false);
    let old = ir_history_detail(&mut store, &first, &|| false).unwrap();
    drop(store);
    let (mut app, count) = app(&root);
    app.open_ir_history_attempt(first);
    let old_intent = app.ir_history_ui.detail_pending.as_ref().unwrap().0;
    app.open_ir_history_attempt(second.clone());
    assert!(!app.observe_ir_history_event(&WorkflowEvent {
        intent: old_intent,
        result: Ok(WorkflowResult::IrHistoryDetail(old))
    }));
    assert!(app.ir_history_ui.detail.is_none());
    settle(&mut app);
    assert_eq!(app.ir_history_ui.detail.as_ref().unwrap().attempt, second);
    assert_eq!(count.load(Ordering::SeqCst), 0);
}
#[test]
fn unreadable_attempt_offers_only_diagnostic_export_and_preserves_the_exact_journal() {
    let root = TempRoot::new("gui-history-bad");
    let output = TempRoot::new("gui-history-export-output");
    let mut store = LibraryStore::open(&root.0).unwrap();
    let id = populate(&mut store, true);
    drop(store);
    let raw = b"raw unreadable history";
    std::fs::write(root.journal(&id), raw).unwrap();
    let (mut app, count) = app(&root);
    app.open_ir_history_attempt(id.clone());
    settle(&mut app);
    let detail = app.ir_history_ui.detail.as_ref().unwrap().clone();
    assert_eq!(detail.export_kind, IrHistoryExportKind::RawDiagnostics);
    let ctx = egui::Context::default();
    ctx.enable_accesskit();
    crate::theme::install_fonts(&ctx);
    crate::theme::apply(&ctx, ThemePreference::Dark, 1.0);
    let nodes = frame(&mut app, &ctx, vec![]);
    let labels: Vec<_> = nodes.values().filter_map(Node::label).collect();
    assert!(labels.contains(&"Export raw diagnostics…"));
    assert!(!labels.contains(&"Export evidence…"));
    app.ir_history_ui.export_dialog = Some(IrHistoryExportRequest {
        attempt: id.clone(),
        observed_snapshot_sha256: detail.snapshot_sha256.clone(),
        kind: detail.export_kind,
        destination: PathBuf::new(),
    });
    let path = output.0.join("diagnostics.zip");
    app.export_ir_history_to(path.clone());
    settle(&mut app);
    assert!(path.exists());
    assert_eq!(std::fs::read(root.journal(&id)).unwrap(), raw);
    assert!(app.ir_history_ui.message.contains("raw diagnostics"));
    assert_eq!(count.load(Ordering::SeqCst), 0);
}
#[test]
fn details_are_english_and_keyboard_reachable_in_both_themes_and_at_scaled_sizes() {
    for (theme, scale) in [(ThemePreference::Dark, 1.0), (ThemePreference::Light, 1.25)] {
        let root = TempRoot::new("gui-history-keyboard");
        let mut store = LibraryStore::open(&root.0).unwrap();
        let id = populate(&mut store, true);
        drop(store);
        let (mut app, count) = app(&root);
        app.open_ir_history_attempt(id);
        settle(&mut app);
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        crate::theme::install_fonts(&ctx);
        crate::theme::apply(&ctx, theme, scale);
        let mut nodes = frame(&mut app, &ctx, vec![]);
        let mut reached = false;
        for _ in 0..80 {
            if let Some(focus) = ctx.memory(|memory| memory.focused())
                && nodes.get(&focus.accesskit_id()).and_then(Node::label) == Some("Back to history")
            {
                reached = true;
                break;
            }
            let next_nodes = frame(
                &mut app,
                &ctx,
                vec![egui::Event::Key {
                    key: egui::Key::Tab,
                    physical_key: Some(egui::Key::Tab),
                    pressed: true,
                    repeat: false,
                    modifiers: egui::Modifiers::NONE,
                }],
            );
            if !next_nodes.is_empty() {
                nodes = next_nodes;
            }
            frame(
                &mut app,
                &ctx,
                vec![egui::Event::Key {
                    key: egui::Key::Tab,
                    physical_key: Some(egui::Key::Tab),
                    pressed: false,
                    repeat: false,
                    modifiers: egui::Modifiers::NONE,
                }],
            );
        }
        assert!(reached, "Back to history was unreachable from the keyboard");
        let labels: Vec<_> = nodes.values().filter_map(Node::label).collect();
        assert!(!labels.iter().any(|label| matches!(
            *label,
            "Retry" | "Resume" | "Send" | "Restore original" | "Delete" | "Rename"
        )));
        frame(
            &mut app,
            &ctx,
            vec![egui::Event::Key {
                key: egui::Key::Enter,
                physical_key: Some(egui::Key::Enter),
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            }],
        );
        frame(
            &mut app,
            &ctx,
            vec![egui::Event::Key {
                key: egui::Key::Enter,
                physical_key: Some(egui::Key::Enter),
                pressed: false,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            }],
        );
        assert!(app.ir_history_ui.detail.is_none());
        assert_eq!(count.load(Ordering::SeqCst), 0);
    }
}
#[test]
fn raw_start_values_are_preserved_without_a_calendar_or_clock_origin_inference() {
    for ms in [0, 100, 951_782_400_000, 4_107_542_400_000, u64::MAX] {
        let label = start_label(ms);
        assert_eq!(
            label,
            format!("Recorded start: {ms} ms; calendar date unavailable")
        );
        assert!(!label.contains("UTC"));
        assert!(!label.contains("1970-01-01"));
        assert!(!label.contains("monotonic"));
        assert!(!label.contains("Unix"));
    }
}
