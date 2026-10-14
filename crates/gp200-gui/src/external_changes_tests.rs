//! Actual egui widgets and keyboard events; all device state and conflicts are synthetic.
use super::*;
use egui::accesskit::{Node, NodeId, Role};
use std::{
    collections::BTreeMap,
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, AtomicUsize, Ordering},
};

static NEXT: AtomicU64 = AtomicU64::new(0);
struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "gp200-external-ui-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
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
fn snapshot(revision: u64, gain: Option<f32>) -> Arc<PresetSnapshot> {
    let mut records = gp200_sim::captured_demo_snapshot().raw.records().to_vec();
    if let Some(gain) = gain {
        let amp = records
            .iter_mut()
            .find(|record| record.command == 0x14 && record.body[0] == 3)
            .unwrap();
        amp.body[8..12].copy_from_slice(&gain.to_le_bytes());
    }
    Arc::new(PresetSnapshot::from_raw(
        Arc::new(RawTransfer::from_records(0x12, &records).unwrap()),
        SnapshotSource::Simulator,
        Some(PresetTarget::EditBuffer),
        Revision(revision),
    ))
}
fn read_stamp(effects: &[Effect]) -> RequestStamp {
    match effects {
        [
            Effect::Device(
                DeviceCommand::RefreshTarget { stamp, .. } | DeviceCommand::ReadCurrent(stamp),
            ),
        ] => *stamp,
        other => panic!("Expected complete read: {other:?}"),
    }
}
struct Harness {
    app: StudioApp,
    context: egui::Context,
    nodes: BTreeMap<NodeId, Node>,
    submissions: Arc<AtomicUsize>,
    time: f64,
    _directory: Directory,
}
impl Harness {
    fn new(theme: ThemePreference) -> Self {
        let preferences = AppPreferences {
            theme,
            language: LanguagePreference::English,
            ..Default::default()
        };
        let mut controller = AppController::new(
            gp200_catalog::Catalog::embedded().unwrap(),
            preferences.clone(),
        );
        let effects = controller.dispatch(AppInput::Ui(UiIntent::Connect(DeviceSelection {
            input_id: "synthetic-in".into(),
            output_id: "synthetic-out".into(),
            fingerprint_hint: None,
        })));
        let [Effect::Device(DeviceCommand::Connect { intent, .. })] = &effects[..] else {
            panic!("connect");
        };
        controller.dispatch(AppInput::Device(DeviceEvent::SessionChanged {
            epoch: SessionEpoch(1),
            identity: DeviceIdentity {
                model: DeviceModel::Gp200,
                mode: BackendMode::Simulator,
                firmware: None,
                display_name: "Synthetic UI fixture".into(),
            },
            capabilities: Capabilities::simulator(),
        }));
        controller.dispatch(AppInput::Device(DeviceEvent::ConnectionChanged {
            epoch: SessionEpoch(1),
            state: ConnectionState::Ready,
        }));
        controller.dispatch(AppInput::Device(DeviceEvent::SnapshotReceived {
            stamp: RequestStamp {
                epoch: SessionEpoch(1),
                intent: *intent,
            },
            target: PresetTarget::EditBuffer,
            snapshot: snapshot(0, None),
        }));
        controller.dispatch(AppInput::Ui(UiIntent::ApplyEdit {
            document: gp200_app::device_document_id(),
            patch: EditPatch::SetMetadata(MetadataPatch {
                author: Some("Retained UI author".into()),
                ..Default::default()
            }),
        }));
        let effects =
            controller.dispatch(AppInput::Ui(UiIntent::Refresh(PresetTarget::EditBuffer)));
        controller.dispatch(AppInput::Device(DeviceEvent::SnapshotReceived {
            stamp: read_stamp(&effects),
            target: PresetTarget::EditBuffer,
            snapshot: snapshot(1, Some(73.0)),
        }));
        assert_eq!(controller.active_document().unwrap().conflicts.len(), 1);
        let directory = Directory::new();
        let submissions = Arc::new(AtomicUsize::new(0));
        let services = RuntimeServices::with_preferences_path(
            Box::new(NoDevice(submissions.clone())),
            no_wake(),
            preferences,
            directory.0.join("preferences.json"),
        );
        let context = egui::Context::default();
        theme::install_fonts(&context);
        theme::apply(&context, theme, 1.0);
        context.enable_accesskit();
        let mut harness = Self {
            app: StudioApp::with_services(controller, services),
            context,
            nodes: BTreeMap::new(),
            submissions,
            time: 1.0,
            _directory: directory,
        };
        harness.frame(vec![]);
        harness.frame(vec![]);
        harness
    }
    fn frame(&mut self, events: Vec<egui::Event>) {
        self.time += 0.1;
        let mut output = self.context.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1440.0, 1100.0),
                )),
                time: Some(self.time),
                events,
                ..Default::default()
            },
            |ui| {
                self.app.shortcuts(ui.ctx());
                self.app.render(ui);
            },
        );
        if let Some(update) = output.platform_output.accesskit_update.take() {
            self.nodes = update.nodes.into_iter().collect();
        }
        output.textures_delta.clear();
        let (inputs, selections) = self.app.services.pump();
        assert!(inputs.is_empty());
        assert!(selections.is_empty());
    }
    fn key(&mut self, key: egui::Key) {
        for pressed in [true, false] {
            self.frame(vec![egui::Event::Key {
                key,
                physical_key: Some(key),
                pressed,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            }]);
        }
    }
    fn label(&self, node: &Node) -> String {
        node.label().map(str::to_owned).unwrap_or_else(|| {
            node.labelled_by()
                .iter()
                .filter_map(|id| self.nodes.get(id))
                .filter_map(Node::value)
                .collect::<Vec<_>>()
                .join(" ")
        })
    }
    fn button(&mut self, label: &str) {
        for _ in 0..240 {
            let focused = self
                .context
                .memory(|memory| memory.focused())
                .map(|id| id.accesskit_id());
            if focused
                .and_then(|id| self.nodes.get(&id))
                .is_some_and(|node| node.role() == Role::Button && self.label(node) == label)
            {
                self.key(egui::Key::Enter);
                self.frame(vec![]);
                return;
            }
            self.key(egui::Key::Tab);
        }
        panic!("Conflict button {label} unreachable via actual keyboard navigation");
    }
    fn assert_no_submission(&self) {
        assert_eq!(self.submissions.load(Ordering::SeqCst), 0);
    }
}

#[test]
fn real_conflict_buttons_preserve_work_and_submit_zero_device_commands_in_both_themes() {
    for theme in [ThemePreference::Light, ThemePreference::Dark] {
        for accept in [true, false] {
            let mut harness = Harness::new(theme);
            let before = harness.app.controller.active_document().unwrap().clone();
            harness.button(if accept {
                "Accept pedal"
            } else {
                "Keep local draft"
            });
            let state = harness.app.controller.state();
            let retained = state
                .documents
                .values()
                .find(|document| matches!(document.target, EditTarget::Offline(_)))
                .expect("Whole previous document retained");
            assert_eq!(retained.draft, before.draft);
            assert_eq!(retained.undo.past, before.undo.past);
            assert!(Arc::ptr_eq(&retained.baseline, &before.baseline));
            assert_eq!(
                state.active_document,
                Some(if accept {
                    gp200_app::device_document_id()
                } else {
                    retained.id.clone()
                })
            );
            assert!(
                state.documents[&gp200_app::device_document_id()]
                    .undo
                    .past
                    .is_empty()
            );
            harness.assert_no_submission();
        }
    }
}

#[test]
fn stale_review_buttons_are_disabled_in_the_real_editor_and_send_nothing() {
    let mut harness = Harness::new(ThemePreference::Dark);
    harness.app.edit(EditPatch::SetMetadata(MetadataPatch {
        notes: Some("Typed after review".into()),
        ..Default::default()
    }));
    let before = harness.app.controller.active_document().unwrap().clone();
    harness.frame(vec![]);
    harness.frame(vec![]);
    for label in ["Accept pedal", "Keep local draft"] {
        let node = harness
            .nodes
            .values()
            .find(|node| node.role() == Role::Button && harness.label(node) == label)
            .expect("Review button rendered");
        assert!(
            node.is_disabled(),
            "Stale review must not be keyboard actionable"
        );
    }
    assert_eq!(
        harness.app.controller.active_document().unwrap().draft,
        before.draft
    );
    assert_eq!(harness.app.controller.state().documents.len(), 1);
    harness.assert_no_submission();
}

#[test]
fn pending_invalidation_schedules_a_bounded_repaint_without_a_device_submission() {
    let mut harness = Harness::new(ThemePreference::Dark);
    let raw = Arc::new(
        RawTransfer::from_records(
            0x12,
            &[RawRecord {
                command: 0x15,
                body: vec![3, 1, 0, 0],
            }],
        )
        .unwrap(),
    );
    assert!(
        harness
            .app
            .controller
            .dispatch(AppInput::Device(DeviceEvent::IncomingNotification {
                epoch: SessionEpoch(1),
                raw
            }))
            .is_empty()
    );
    assert!(harness.app.controller.external_reconciliation_pending());
    // Exercise the same hook called by native logic, on real egui frame output.
    let mut output = None;
    for time in [20.0, 20.1, 20.2] {
        output = Some(harness.context.run_ui(
            egui::RawInput {
                time: Some(time),
                ..Default::default()
            },
            |ui| {
                harness
                    .app
                    .schedule_external_reconciliation_repaint(ui.ctx())
            },
        ));
    }
    let output = output.unwrap();
    let delay = output.viewport_output[&egui::ViewportId::ROOT].repaint_delay;
    // egui subtracts its predicted frame interval from requested delays.
    assert!(delay > Duration::ZERO && delay <= Duration::from_millis(50));
    harness.assert_no_submission();
}
