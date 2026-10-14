use super::*;
use gp200_protocol::{F32Bits, RawRecord};
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
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

fn snapshot(level: u16, gain: u32) -> Arc<PresetSnapshot> {
    // Use the domain encoder to edit native metadata without guessing offsets.
    let baseline = gp200_sim::captured_demo_snapshot();
    let mut draft = PresetDraft::from_snapshot(&baseline);
    draft
        .apply(&EditPatch::SetMetadata(MetadataPatch {
            level: Some(level),
            ..Default::default()
        }))
        .unwrap();
    draft
        .apply(&EditPatch::SetParameter {
            module: ModuleId::new(3).unwrap(),
            model: ModelCode(0x07000015),
            model_index: ModelIndex(0),
            parameter: ParamId::new(0).unwrap(),
            value: F32Bits(gain),
        })
        .unwrap();
    let edited = draft
        .snapshot(&baseline, baseline.source.clone(), Revision(7))
        .unwrap();
    let mut records = edited.raw.records().to_vec();
    records.push(RawRecord {
        command: 0xbeef,
        body: vec![0, 255, 1, 3],
    });
    records.push(RawRecord {
        command: 0xbeef,
        body: vec![5, 6],
    });
    Arc::new(PresetSnapshot::from_raw(
        Arc::new(RawTransfer::from_records(0x12, &records).unwrap()),
        baseline.source.clone(),
        None,
        Revision(7),
    ))
}
fn version(id: &str, snapshot: Arc<PresetSnapshot>) -> LibraryVersion {
    LibraryVersion {
        id: LibraryVersionId::from(id),
        library: LibraryId::from("merge-ui-source"),
        parent: None,
        snapshot,
        metadata: LibraryMetadata {
            name: "Source voice 源音色".into(),
            ..Default::default()
        },
        created_at: "2026-10-10T12:00:00Z".into(),
    }
}
fn gain() -> OfflineMergeField {
    OfflineMergeField::Parameter {
        module: ModuleId::new(3).unwrap(),
        expected_model: ModelCode(0x07000015),
        parameter: ParamId::new(0).unwrap(),
    }
}
fn app() -> (StudioApp, Arc<AtomicUsize>, PathBuf) {
    let root = std::env::temp_dir().join(format!(
        "gp200-merge-ui-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir(&root).unwrap();
    let mut controller = gp200_app::AppController::with_demo(
        snapshot(50, 50_f32.to_bits()),
        AppPreferences::default(),
    )
    .unwrap();
    controller.dispatch(AppInput::Ui(UiIntent::NewOfflineFrom(snapshot(
        50,
        50_f32.to_bits(),
    ))));
    let mut incoming = snapshot(73, 73_f32.to_bits()).raw.records().to_vec();
    for record in incoming
        .iter_mut()
        .filter(|record| record.command == 0xbeef)
    {
        record.body = vec![9, 42, 99];
    }
    let source = version(
        "merge-ui-v1",
        Arc::new(PresetSnapshot::from_raw(
            Arc::new(RawTransfer::from_records(0x12, &incoming).unwrap()),
            SnapshotSource::Capture {
                capture_id: "different-opaque-source".into(),
            },
            None,
            Revision(7),
        )),
    );
    controller.dispatch(AppInput::Library(LibraryEvent::VersionLoaded {
        intent: IntentId(1),
        version: source.clone(),
    }));
    controller.dispatch(AppInput::Library(LibraryEvent::QueryResult {
        intent: IntentId(2),
        entries: vec![LibraryEntry {
            id: source.library.clone(),
            latest_version: source.id.clone(),
            metadata: source.metadata.clone(),
            version_count: 1,
            modified_at: source.created_at.clone(),
        }],
    }));
    let count = Arc::new(AtomicUsize::new(0));
    let services = crate::runtime::RuntimeServices::with_preferences_path(
        Box::new(CountingDisconnectedDeviceService(count.clone())),
        no_wake(),
        AppPreferences::default(),
        root.join("preferences.json"),
    );
    let mut app = StudioApp::with_services(controller, services);
    app.open_selective_merge_source(source.id);
    (app, count, root)
}
fn no_wake() -> WakeUi {
    Arc::new(|| {})
}
fn draw(app: &mut StudioApp, ctx: &egui::Context, events: Vec<egui::Event>) -> egui::FullOutput {
    let mut output = ctx.run_ui(
        egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(960., 640.),
            )),
            events,
            ..Default::default()
        },
        |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| app.selective_merge_workspace(ui));
        },
    );
    output.textures_delta.clear();
    output
}
fn rendered_labels(output: &egui::FullOutput) -> Vec<String> {
    fn collect(shape: &egui::epaint::Shape, values: &mut Vec<String>) {
        match shape {
            egui::epaint::Shape::Text(text) => values.push(text.galley.text().to_owned()),
            egui::epaint::Shape::Vec(shapes) => {
                for shape in shapes {
                    collect(shape, values);
                }
            }
            _ => {}
        }
    }
    let mut values = Vec::new();
    for shape in &output.shapes {
        collect(&shape.shape, &mut values);
    }
    values
}
fn label_point(output: &egui::FullOutput, label: &str) -> Option<egui::Pos2> {
    fn find(shape: &egui::epaint::Shape, label: &str) -> Option<egui::Pos2> {
        match shape {
            egui::epaint::Shape::Text(text) if text.galley.text() == label => {
                Some(text.pos + egui::vec2(5., 5.))
            }
            egui::epaint::Shape::Vec(shapes) => shapes.iter().find_map(|shape| find(shape, label)),
            _ => None,
        }
    }
    output
        .shapes
        .iter()
        .find_map(|shape| find(&shape.shape, label))
}
fn click(app: &mut StudioApp, ctx: &egui::Context, point: egui::Pos2) {
    for pressed in [true, false] {
        draw(
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
fn settle(app: &mut StudioApp, ctx: &egui::Context) {
    draw(app, ctx, vec![]);
    draw(app, ctx, vec![]);
}
fn cleanup(mut app: StudioApp, count: Arc<AtomicUsize>, root: PathBuf) {
    for _ in 0..100 {
        let (inputs, _) = app.services.pump();
        for input in inputs {
            app.controller.dispatch(input);
        }
        if !app.services.has_pending() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    assert_eq!(count.load(Ordering::SeqCst), 0);
    assert!(!app.controller.device_operation_pending());
    assert!(!app.services.has_pending());
    drop(app);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn actual_checkbox_review_and_apply_are_one_undo_step_without_device_submissions() {
    let (mut app, count, root) = app();
    let id = app.controller.active_document_id().unwrap().clone();
    let before = app.controller.active_document().unwrap().draft.clone();
    let source = app.selective_merge.source.as_ref().unwrap().clone();
    let ctx = egui::Context::default();
    crate::theme::install_fonts(&ctx);
    settle(&mut app, &ctx);
    let level = OfflineMergeField::Metadata(MergeMetadataField::Level);
    let point = app.selective_merge.field_rects[&level].center();
    click(&mut app, &ctx, point);
    assert!(app.selective_merge.fields.contains(&level));
    settle(&mut app, &ctx);
    let rect = app.selective_merge.review_rect.unwrap();
    click(&mut app, &ctx, rect.center());
    assert!(
        app.selective_merge.review.is_some(),
        "{}",
        app.selective_merge.message
    );
    // The existing diff reports the typed level plus its local annotation.
    assert_eq!(app.selective_merge_report().unwrap()["setting_changes"], 2);
    settle(&mut app, &ctx);
    let rect = app.selective_merge.apply_rect.unwrap();
    click(&mut app, &ctx, rect.center());
    let document = &app.controller.state().documents[&id];
    assert_eq!(document.draft.metadata.level, Some(73));
    assert_eq!(document.undo.past.len(), 1);
    assert_eq!(document.undo.past[0], before);
    assert_eq!(app.selective_merge.source.as_ref().unwrap(), &source);
    assert_eq!(document.draft.view.modules, before.view.modules);
    let exported = document
        .draft
        .snapshot(
            &document.baseline,
            document.baseline.source.clone(),
            document.local_revision,
        )
        .unwrap();
    let unknown: Vec<_> = exported
        .raw
        .records()
        .iter()
        .filter(|record| record.command == 0xbeef)
        .map(|record| record.body.clone())
        .collect();
    assert_eq!(unknown, vec![vec![0, 255, 1, 3], vec![5, 6]]);
    app.send(UiIntent::Undo(id.clone()));
    assert_eq!(app.controller.state().documents[&id].draft, before);
    app.send(UiIntent::Redo(id.clone()));
    assert_eq!(
        app.controller.state().documents[&id].draft.metadata.level,
        Some(73)
    );
    cleanup(app, count, root);
}

#[test]
fn changed_target_or_selection_retires_review_before_apply_input() {
    let (mut app, count, root) = app();
    let id = app.controller.active_document_id().unwrap().clone();
    app.review_selective_merge(vec![gain()]).unwrap();
    app.send(UiIntent::ApplyEdit {
        document: id.clone(),
        patch: EditPatch::SetMetadata(MetadataPatch {
            level: Some(61),
            ..Default::default()
        }),
    });
    let before = app.controller.state().documents[&id].draft.clone();
    app.apply_selective_merge();
    assert!(app.selective_merge.review.is_none());
    assert_eq!(app.controller.state().documents[&id].draft, before);
    app.review_selective_merge(vec![gain()]).unwrap();
    app.selective_merge
        .fields
        .insert(OfflineMergeField::Metadata(MergeMetadataField::Level));
    app.apply_selective_merge();
    assert!(app.selective_merge.review.is_none());
    assert_eq!(app.controller.state().documents[&id].draft, before);
    cleanup(app, count, root);
}

#[test]
fn back_then_changed_checkbox_requires_new_review() {
    let (mut app, count, root) = app();
    let ctx = egui::Context::default();
    app.review_selective_merge(vec![gain()]).unwrap();
    settle(&mut app, &ctx);
    let rect = app.selective_merge.back_rect.unwrap();
    click(&mut app, &ctx, rect.center());
    assert!(app.selective_merge.review.is_none());
    settle(&mut app, &ctx);
    let level = OfflineMergeField::Metadata(MergeMetadataField::Level);
    let rect = app.selective_merge.field_rects[&level];
    click(&mut app, &ctx, rect.center());
    assert_eq!(app.selective_merge.fields.len(), 2);
    assert!(app.selective_merge_report().is_none());
    cleanup(app, count, root);
}

#[test]
fn stale_source_load_is_ignored_and_matching_reply_never_opens_a_document() {
    let (mut app, count, root) = app();
    let id = app.controller.active_document_id().unwrap().clone();
    let docs = app.controller.state().documents.len();
    app.selective_merge.pending_source = Some((IntentId(90), LibraryVersionId::from("next")));
    app.selective_merge.source_version = Some(LibraryVersionId::from("next"));
    app.selective_merge.source = None;
    let stale = LibraryEvent::VersionLoaded {
        intent: IntentId(89),
        version: version("old", snapshot(40, 40_f32.to_bits())),
    };
    app.observe_selective_merge_source(&stale);
    assert!(app.selective_merge.source.is_none());
    let current = LibraryEvent::VersionLoaded {
        intent: IntentId(90),
        version: version("next", snapshot(40, 40_f32.to_bits())),
    };
    app.observe_selective_merge_source(&current);
    app.controller.dispatch(AppInput::Library(current));
    assert_eq!(
        app.selective_merge.source.as_ref().unwrap().id,
        LibraryVersionId::from("next")
    );
    assert_eq!(app.controller.active_document_id(), Some(&id));
    assert_eq!(app.controller.state().documents.len(), docs);
    cleanup(app, count, root);
}

#[test]
fn replacing_same_source_version_or_target_baseline_expires_exact_review() {
    let (mut app, count, root) = app();
    app.review_selective_merge(vec![gain()]).unwrap();
    let changed = LibraryEvent::VersionLoaded {
        intent: IntentId(99),
        version: version("merge-ui-v1", snapshot(74, 74_f32.to_bits())),
    };
    app.controller.dispatch(AppInput::Library(changed));
    app.expire_selective_merge();
    assert!(app.selective_merge.review.is_none());
    assert!(app.selective_merge.source.is_none());
    cleanup(app, count, root);
}

#[test]
fn device_document_empty_selection_and_duplicate_fields_are_rejected() {
    let (mut app, count, root) = app();
    assert!(app.review_selective_merge(vec![]).is_err());
    assert!(app.review_selective_merge(vec![gain(), gain()]).is_err());
    app.selective_merge.target = Some(DocumentId::from("device-buffer"));
    assert!(app.review_selective_merge(vec![gain()]).is_err());
    assert!(app.selective_merge.review.is_none());
    cleanup(app, count, root);
}

#[test]
fn both_themes_and_three_scales_render_selection_and_review_with_exact_hashes() {
    let (mut app, count, root) = app();
    let initial = app.controller.recovery_session();
    let mut hash = None;

    for theme in [ThemePreference::Dark, ThemePreference::Light] {
        for scale in [1., 1.5, 2.] {
            let ctx = egui::Context::default();
            crate::theme::install_fonts(&ctx);
            crate::theme::apply(&ctx, theme, scale);
            ctx.all_styles_mut(|style| style.interaction.tooltip_delay = 0.);
            app.selective_merge.review = None;
            let selection = draw(&mut app, &ctx, vec![]);
            let labels = rendered_labels(&selection);
            let style = "Native style";
            assert!(
                labels.iter().any(|label| label == style),
                "missing rendered style {style}: {labels:?}"
            );
            app.review_selective_merge(vec![
                gain(),
                OfflineMergeField::Metadata(MergeMetadataField::Level),
            ])
            .unwrap();
            let report = app.selective_merge_report().unwrap();
            let actual = report["review_sha256"].clone();
            assert_eq!(hash.get_or_insert(actual.clone()), &actual);
            let review = draw(&mut app, &ctx, vec![]);
            let labels = rendered_labels(&review);
            let gain_label = "AMP Gain: 50 → 73";
            let level_label = "Patch level: 50 → 73";
            assert!(
                labels.iter().any(|label| label == gain_label),
                "missing rendered gain {gain_label}: {labels:?}"
            );
            assert!(
                labels.iter().any(|label| label == level_label),
                "missing plain level {level_label}: {labels:?}"
            );
            assert!(
                labels
                    .iter()
                    .all(|label| !label.contains("Some(") && !label.contains("bits "))
            );
            let point = label_point(&review, gain_label).unwrap();
            draw(&mut app, &ctx, vec![egui::Event::PointerMoved(point)]);
            let tooltip = draw(&mut app, &ctx, vec![]);
            let bits = "Exact parameter bits: 42480000 → 42920000";
            assert!(
                rendered_labels(&tooltip).iter().any(|label| label == bits),
                "exact-bit hover disclosure missing {bits}"
            );
            assert_eq!(
                app.selective_merge_report().unwrap()["review_sha256"],
                actual
            );
            let bounds = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(960., 640.));
            assert!(bounds.contains_rect(app.selective_merge.apply_rect.unwrap()));
        }
    }

    assert_eq!(
        app.controller.recovery_session().documents,
        initial.documents
    );
    cleanup(app, count, root);
}

#[test]
fn actual_complete_module_and_individual_checkbox_choices_are_exclusive() {
    let (mut app, count, root) = app();
    app.selective_merge.fields.insert(gain());
    let ctx = egui::Context::default();
    let draw = |app: &mut StudioApp, events: Vec<egui::Event>| {
        let source = app
            .selective_merge
            .source
            .as_ref()
            .unwrap()
            .snapshot
            .clone();
        let target = app.controller.active_document().unwrap().clone();
        let mut output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1280., 1600.),
                )),
                events,
                ..Default::default()
            },
            |ui| app.merge_checkboxes(ui, &source, &target),
        );
        output.textures_delta.clear();
    };
    draw(&mut app, vec![]);
    draw(&mut app, vec![]);
    let full = OfflineMergeField::Module(ModuleId::new(3).unwrap());
    let point = app.selective_merge.field_rects[&full].center();
    for pressed in [true, false] {
        draw(
            &mut app,
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
    assert!(app.selective_merge.fields.contains(&full));
    assert!(!app.selective_merge.fields.contains(&gain()));
    draw(&mut app, vec![]);
    let point = app.selective_merge.field_rects[&gain()].center();
    for pressed in [true, false] {
        draw(
            &mut app,
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
    assert!(app.selective_merge.fields.contains(&gain()));
    assert!(!app.selective_merge.fields.contains(&full));
    cleanup(app, count, root);
}

#[test]
fn model_dependency_rejection_is_presented_and_coherent_exp_quick_selection_applies_exact_bits() {
    let (mut app, count, root) = app();
    let mut records = app
        .controller
        .active_document()
        .unwrap()
        .baseline
        .raw
        .records()
        .to_vec();
    for (command, slot) in [(12, 0x10), (16, 0)] {
        let record = records
            .iter_mut()
            .find(|record| record.command == command && record.body[0] == slot)
            .unwrap();
        record.body[1] = 4;
        record.body[2..4].copy_from_slice(&0_u16.to_le_bytes());
    }
    let baseline = Arc::new(PresetSnapshot::from_raw(
        Arc::new(RawTransfer::from_records(0x12, &records).unwrap()),
        SnapshotSource::Capture {
            capture_id: "assigned-native-merge".into(),
        },
        None,
        Revision(7),
    ));
    app.send(UiIntent::NewOfflineFrom(baseline));
    let id = app.controller.active_document_id().unwrap().clone();
    let original = app.controller.active_document().unwrap().draft.clone();
    let module = records
        .iter_mut()
        .find(|record| record.command == 20 && record.body[0] == 4)
        .unwrap();
    module.body[4..8].copy_from_slice(&29_u32.to_le_bytes());
    let expression = records
        .iter_mut()
        .find(|record| record.command == 12 && record.body[0] == 0x10)
        .unwrap();
    expression.body[4..8].copy_from_slice(&0x7fc0_0444_u32.to_le_bytes());
    expression.body[8..12].copy_from_slice(&0x8000_0000_u32.to_le_bytes());
    let incoming = Arc::new(PresetSnapshot::from_raw(
        Arc::new(RawTransfer::from_records(0x12, &records).unwrap()),
        SnapshotSource::Capture {
            capture_id: "assigned-native-merge-source".into(),
        },
        None,
        Revision(8),
    ));
    let source = version("assigned-model-source", incoming.clone());
    app.controller
        .dispatch(AppInput::Library(LibraryEvent::VersionLoaded {
            intent: IntentId(41),
            version: source.clone(),
        }));
    app.open_selective_merge_source(source.id);
    app.selective_merge.target = Some(id.clone());
    let field = OfflineMergeField::Module(ModuleId::new(4).unwrap());
    assert_eq!(
        app.review_selective_merge(vec![field.clone()])
            .unwrap_err()
            .kind,
        ErrorKind::Conflict
    );
    assert!(
        app.selective_merge
            .message
            .contains("unselected assignment")
    );
    assert_eq!(app.controller.state().documents[&id].draft, original);
    app.review_selective_merge(vec![
        field,
        OfflineMergeField::Expression(ExpressionSlotId::from_packed(0x10).unwrap()),
        OfflineMergeField::Quick(0),
    ])
    .unwrap();
    let expected = app
        .selective_merge
        .review
        .as_ref()
        .unwrap()
        .after()
        .raw
        .clone();
    let ctx = egui::Context::default();
    settle(&mut app, &ctx);
    let point = app.selective_merge.apply_rect.unwrap().center();
    click(&mut app, &ctx, point);
    let actual = &app.controller.state().documents[&id];
    assert_eq!(actual.undo.past.len(), 1);
    assert_eq!(
        actual
            .draft
            .view
            .patch_assignments
            .expressions
            .iter()
            .find(|item| item.record.slot.packed() == 0x10)
            .unwrap()
            .record
            .maximum,
        F32Bits(0x7fc0_0444)
    );
    assert_eq!(
        actual
            .draft
            .snapshot(
                &actual.baseline,
                actual.baseline.source.clone(),
                Revision(9)
            )
            .unwrap()
            .raw,
        expected
    );
    assert_eq!(
        app.selective_merge.source.as_ref().unwrap().snapshot,
        incoming
    );
    cleanup(app, count, root);
}

#[test]
fn apply_recovery_roundtrip_preserves_one_undo_step_and_retained_unknown_records() {
    let (mut app, count, root) = app();
    let id = app.controller.active_document_id().unwrap().clone();
    let before = app.controller.active_document().unwrap().draft.clone();
    app.review_selective_merge(vec![gain()]).unwrap();
    let expected = app
        .selective_merge
        .review
        .as_ref()
        .unwrap()
        .after()
        .raw
        .clone();
    app.apply_selective_merge();
    let session = app.controller.recovery_session();
    let encoded = serde_json::to_vec(&session).unwrap();
    let restored: RecoverySession = serde_json::from_slice(&encoded).unwrap();
    let mut reopened = gp200_app::AppController::with_demo(
        gp200_sim::captured_demo_snapshot(),
        AppPreferences::default(),
    )
    .unwrap();
    reopened.recover_offline(&restored).unwrap();
    let document = reopened
        .state()
        .documents
        .values()
        .find(|document| !document.undo.past.is_empty())
        .unwrap();
    assert_eq!(document.undo.past[0], before);
    assert_eq!(
        document
            .draft
            .snapshot(
                &document.baseline,
                document.baseline.source.clone(),
                document.local_revision
            )
            .unwrap()
            .raw,
        expected
    );
    let restored_id = document.id.clone();
    let effects = reopened.dispatch(AppInput::Ui(UiIntent::Undo(restored_id.clone())));
    assert!(
        !effects
            .iter()
            .any(|effect| matches!(effect, Effect::Device(_)))
    );
    assert_eq!(reopened.state().documents[&restored_id].draft, before);
    assert_ne!(id, restored_id);
    cleanup(app, count, root);
}

#[test]
fn applying_to_another_offline_target_makes_actual_shortcut_undo_restore_that_target() {
    let (mut app, count, root) = app();
    let target = app.controller.active_document_id().unwrap().clone();
    let before = app.controller.active_document().unwrap().draft.clone();
    app.send(UiIntent::NewOfflineFrom(snapshot(42, 42_f32.to_bits())));
    let other = app.controller.active_document_id().unwrap().clone();
    let untouched = app.controller.active_document().unwrap().draft.clone();
    assert_ne!(target, other);
    app.review_selective_merge(vec![gain()]).unwrap();
    let context = egui::Context::default();
    settle(&mut app, &context);
    let point = app.selective_merge.apply_rect.unwrap().center();
    click(&mut app, &context, point);
    assert_eq!(app.controller.active_document_id(), Some(&target));
    assert_ne!(app.controller.state().documents[&target].draft, before);
    assert_eq!(app.controller.state().documents[&other].draft, untouched);
    for pressed in [true, false] {
        let modifiers = egui::Modifiers::COMMAND;
        let mut output = context.run_ui(
            egui::RawInput {
                events: vec![
                    egui::Event::ModifiersChanged(modifiers),
                    egui::Event::Key {
                        key: egui::Key::Z,
                        physical_key: Some(egui::Key::Z),
                        pressed,
                        repeat: false,
                        modifiers,
                    },
                ],
                ..Default::default()
            },
            |ui| {
                app.shortcuts(ui.ctx());
                app.selective_merge_workspace(ui);
            },
        );
        output.textures_delta.clear();
    }
    assert_eq!(app.controller.state().documents[&target].draft, before);
    assert_eq!(app.controller.state().documents[&other].draft, untouched);
    cleanup(app, count, root);
}

#[test]
fn legacy_retained_exp_and_quick_records_are_selectable_without_rewriting_source() {
    let (mut app, count, root) = app();
    let id = app.controller.active_document_id().unwrap().clone();
    let before = app.controller.active_document().unwrap().draft.clone();
    let mut records = app
        .selective_merge
        .source
        .as_ref()
        .unwrap()
        .snapshot
        .raw
        .records()
        .to_vec();
    for (command, slot) in [(12, 0x10), (16, 0)] {
        let record = records
            .iter_mut()
            .find(|record| record.command == command && record.body[0] == slot)
            .unwrap();
        record.body[1] = 3;
        record.body[2..4].copy_from_slice(&0_u16.to_le_bytes());
    }
    let mut incoming = PresetSnapshot::from_raw(
        Arc::new(RawTransfer::from_records(0x12, &records).unwrap()),
        SnapshotSource::Capture {
            capture_id: "legacy-retained-assignments".into(),
        },
        None,
        Revision(8),
    );
    // Old saved snapshots retain the native records but predate this projection.
    incoming.view.patch_assignments = Default::default();
    let incoming = Arc::new(incoming);
    let source = version("legacy-assignment-source", incoming.clone());
    app.controller
        .dispatch(AppInput::Library(LibraryEvent::VersionLoaded {
            intent: IntentId(42),
            version: source.clone(),
        }));
    app.open_selective_merge_source(source.id);
    let source_before = app.selective_merge.source.as_ref().unwrap().clone();
    let ctx = egui::Context::default();
    ctx.all_styles_mut(|style| style.animation_time = 0.);
    let draw_checklist = |app: &mut StudioApp, events: Vec<egui::Event>| {
        let source = app
            .selective_merge
            .source
            .as_ref()
            .unwrap()
            .snapshot
            .clone();
        let target = app.controller.active_document().unwrap().clone();
        let mut output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1280., 1600.),
                )),
                events,
                ..Default::default()
            },
            |ui| app.merge_checkboxes(ui, &source, &target),
        );
        output.textures_delta.clear();
        output
    };
    draw_checklist(&mut app, vec![]);
    let output = draw_checklist(&mut app, vec![]);
    let point =
        label_point(&output, "Assignments and FX routing").expect("visible assignment header");
    for pressed in [true, false] {
        draw_checklist(
            &mut app,
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
    draw_checklist(&mut app, vec![]);
    for field in [
        OfflineMergeField::Expression(ExpressionSlotId::from_packed(0x10).unwrap()),
        OfflineMergeField::Quick(0),
    ] {
        let point = app
            .selective_merge
            .field_rects
            .get(&field)
            .expect("legacy retained assignment must have an actual selectable checkbox")
            .center();
        for pressed in [true, false] {
            draw_checklist(
                &mut app,
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
        assert!(app.selective_merge.fields.contains(&field));
    }
    assert_eq!(app.selective_merge.source.as_ref().unwrap(), &source_before);
    assert!(incoming.view.patch_assignments.expressions.is_empty());
    settle(&mut app, &ctx);
    let point = app.selective_merge.review_rect.unwrap().center();
    click(&mut app, &ctx, point);
    assert!(
        app.selective_merge.review.is_some(),
        "{}",
        app.selective_merge.message
    );
    assert_eq!(
        app.selective_merge
            .review
            .as_ref()
            .unwrap()
            .request()
            .source,
        incoming
    );
    settle(&mut app, &ctx);
    let point = app.selective_merge.apply_rect.unwrap().center();
    click(&mut app, &ctx, point);
    let actual = &app.controller.state().documents[&id];
    assert_eq!(actual.undo.past.len(), 1);
    assert_eq!(actual.undo.past[0], before);
    assert_eq!(
        actual
            .draft
            .view
            .patch_assignments
            .expressions
            .iter()
            .find(|item| item.record.slot.packed() == 0x10)
            .unwrap()
            .record
            .module_raw,
        3
    );
    assert_eq!(
        actual
            .draft
            .view
            .patch_assignments
            .quicks
            .iter()
            .find(|item| item.record.index == 0)
            .unwrap()
            .record
            .module_raw,
        3
    );
    assert_eq!(app.selective_merge.source.as_ref().unwrap(), &source_before);
    app.send(UiIntent::Undo(id.clone()));
    assert_eq!(app.controller.state().documents[&id].draft, before);
    cleanup(app, count, root);
}

#[test]
fn musician_diff_labels_use_plain_values_and_module_names() {
    use gp200_library::{KnownChange, PresetDiff};
    let amp = ModuleId::new(3).unwrap();
    let nr = ModuleId::new(4).unwrap();
    let diff = PresetDiff {
        known: vec![
            KnownChange::Name {
                before: Some("Plain voice".into()),
                after: None,
            },
            KnownChange::Tempo {
                before: None,
                after: Some(120),
            },
            KnownChange::Level {
                before: Some(50),
                after: Some(73),
            },
            KnownChange::Chain {
                before: Some(vec![amp, nr]),
                after: Some(vec![nr, amp]),
            },
        ],
        raw: vec![],
        transfer_kind_changed: false,
        asset_refs_changed: false,
    };

    let ctx = egui::Context::default();
    crate::theme::install_fonts(&ctx);
    let mut output = ctx.run_ui(egui::RawInput::default(), |ui| diff_view::render(ui, &diff));
    output.textures_delta.clear();
    let labels = rendered_labels(&output);
    let expected = [
        "Name: Plain voice → Unknown",
        "Tempo: Unknown → 120 BPM",
        "Patch level: 50 → 73",
        "Signal chain: AMP · NR → NR · AMP",
    ];
    for expected in expected {
        assert!(
            labels.iter().any(|label| label == expected),
            "missing {expected}: {labels:?}"
        );
    }
    assert!(
        labels
            .iter()
            .all(|label| !label.contains("Some(") && !label.contains("ModuleId("))
    );
}
