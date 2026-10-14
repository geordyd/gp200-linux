//! Multi-frame input through the production StudioApp, without OS input or MIDI.
use super::*;
use egui::accesskit::{Action, Node, NodeId, Role};
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
            "gp200-numeric-entry-{}-{}",
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

#[derive(Clone, Copy, Debug)]
enum Entry {
    Gain,
    Tempo,
    Level,
    Pan,
}
impl Entry {
    fn text(self) -> &'static str {
        match self {
            Self::Gain | Self::Level => "73",
            Self::Tempo => "150",
            Self::Pan => "-23",
        }
    }
    fn patch(self) -> EditPatch {
        match self {
            Self::Gain => unreachable!("Gain binds the actual captured model in commit"),
            Self::Tempo => EditPatch::SetMetadata(MetadataPatch {
                tempo: Some(150),
                ..Default::default()
            }),
            Self::Level => EditPatch::SetMetadata(MetadataPatch {
                level: Some(73),
                ..Default::default()
            }),
            Self::Pan => EditPatch::SetMetadata(MetadataPatch {
                native_pan: Some(PatchPan::new(-23).unwrap()),
                ..Default::default()
            }),
        }
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
    fn new(entry: Entry) -> Self {
        let directory = Directory::new();
        let preferences = AppPreferences {
            theme: ThemePreference::Dark,
            language: LanguagePreference::English,
            ..Default::default()
        };
        let controller =
            AppController::with_demo(gp200_sim::captured_demo_snapshot(), preferences.clone())
                .unwrap();
        let submissions = Arc::new(AtomicUsize::new(0));
        let services = RuntimeServices::with_preferences_path(
            Box::new(NoDevice(submissions.clone())),
            no_wake(),
            preferences.clone(),
            directory.0.join("preferences.json"),
        );
        let context = egui::Context::default();
        theme::install_fonts(&context);
        context.enable_accesskit();
        theme::apply(&context, preferences.theme, 1.0);
        let mut harness = Self {
            app: StudioApp::with_services(controller, services),
            context,
            nodes: BTreeMap::new(),
            submissions,
            time: 1.0,
            _directory: directory,
        };
        harness.app.qa_patch_inspector = !matches!(entry, Entry::Gain);
        if matches!(entry, Entry::Gain) {
            let module = harness
                .app
                .controller
                .active_document()
                .unwrap()
                .draft
                .view
                .module(ModuleId::new(3).unwrap())
                .unwrap();
            // Deliberately off the catalog's unit step. Rendering, cancelling,
            // and Undo must retain these exact imported bits.
            let patch = EditPatch::SetParameter {
                module: module.module,
                model: module.model,
                model_index: ModelIndex(0),
                parameter: ParamId::new(0).unwrap(),
                value: F32Bits(0x4214_7e6b),
            };
            harness.app.edit(patch);
        }
        harness.frame(vec![]);
        harness.frame(vec![]);
        harness
    }
    fn frame(&mut self, mut events: Vec<egui::Event>) {
        self.time += 0.25;
        let modifiers = events
            .iter()
            .find_map(|event| match event {
                egui::Event::Key { modifiers, .. } => Some(*modifiers),
                _ => None,
            })
            .unwrap_or_default();
        events.insert(0, egui::Event::ModifiersChanged(modifiers));
        let mut output = self.context.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1280.0, 800.0),
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
        // Drain actual effects into the counting disconnected DeviceService.
        // Zero submissions therefore also excludes an unprocessed queued write.
        let (inputs, selections) = self.app.services.pump();
        assert!(selections.is_empty());
        for input in inputs {
            let effects = self.app.controller.dispatch(input);
            self.app.services.enqueue(effects);
        }
    }
    fn key(&mut self, key: egui::Key, modifiers: egui::Modifiers) {
        for pressed in [true, false] {
            self.frame(vec![egui::Event::Key {
                key,
                physical_key: Some(key),
                pressed,
                repeat: false,
                modifiers,
            }]);
        }
    }
    fn name(&self, node: &Node) -> String {
        node.label().map_or_else(
            || {
                node.labelled_by()
                    .iter()
                    .filter_map(|id| self.nodes.get(id))
                    .filter_map(Node::value)
                    .collect::<Vec<_>>()
                    .join(" ")
            },
            String::from,
        )
    }
    fn focus(&mut self, entry: Entry, role: Role) {
        for _ in 0..240 {
            if let Some(node) = self
                .context
                .memory(|memory| memory.focused())
                .and_then(|id| self.nodes.get(&id.accesskit_id()))
                && node.role() == role
                && match entry {
                    Entry::Gain => self.name(node) == "Gain",
                    Entry::Tempo => {
                        node.min_numeric_value() == Some(30.0)
                            && node.max_numeric_value() == Some(300.0)
                    }
                    Entry::Level => {
                        node.min_numeric_value() == Some(0.0)
                            && node.max_numeric_value() == Some(100.0)
                    }
                    Entry::Pan => {
                        node.min_numeric_value() == Some(-100.0)
                            && node.max_numeric_value() == Some(100.0)
                    }
                }
            {
                return;
            }
            self.key(egui::Key::Tab, egui::Modifiers::NONE);
        }
        panic!("Keyboard target {entry:?} with role {role:?} was unreachable");
    }
    fn assert_no_device(&self) {
        assert_eq!(self.submissions.load(Ordering::SeqCst), 0);
    }
    fn open_simulated_buffer(&mut self) {
        let effects =
            self.app
                .controller
                .dispatch(AppInput::Ui(UiIntent::Connect(DeviceSelection {
                    input_id: "test:in".into(),
                    output_id: "test:out".into(),
                    fingerprint_hint: None,
                })));
        let Effect::Device(DeviceCommand::Connect { intent, .. }) = effects[0] else {
            panic!("Connect request did not establish a test session");
        };
        // Establish only an in-process simulated session; these handshake
        // effects are never submitted to a device or a MIDI backend.
        for event in [
            DeviceEvent::SessionChanged {
                epoch: SessionEpoch(1),
                identity: DeviceIdentity {
                    model: DeviceModel::Gp200,
                    mode: BackendMode::Simulator,
                    firmware: None,
                    display_name: "Numeric-entry test simulator".into(),
                },
                capabilities: Capabilities::simulator(),
            },
            DeviceEvent::ConnectionChanged {
                epoch: SessionEpoch(1),
                state: ConnectionState::Ready,
            },
            DeviceEvent::SnapshotReceived {
                stamp: RequestStamp {
                    epoch: SessionEpoch(1),
                    intent,
                },
                target: PresetTarget::EditBuffer,
                snapshot: gp200_sim::captured_demo_snapshot(),
            },
        ] {
            self.app.controller.dispatch(AppInput::Device(event));
        }
        assert!(
            self.app
                .controller
                .activate_document(&gp200_app::device_document_id())
        );
        assert_eq!(
            self.app.controller.active_document().unwrap().target,
            EditTarget::DeviceBuffer
        );
        self.frame(vec![]);
        self.assert_no_device();
    }
    fn type_split(&mut self, entry: Entry, before: &RecoverySession) {
        self.focus(entry, Role::SpinButton);
        assert!(self.context.text_edit_focused());
        self.key(egui::Key::A, egui::Modifiers::COMMAND);
        for character in entry.text().chars() {
            self.frame(vec![egui::Event::Text(character.to_string())]);
            assert!(
                &self.app.controller.recovery_session() == before,
                "Partial {entry:?} text must not mutate draft, exact raw bits, history, revision or A/B"
            );
            self.frame(vec![]);
            assert!(&self.app.controller.recovery_session() == before);
            self.assert_no_device();
        }
    }
}

fn commit(entry: Entry, key: egui::Key) {
    let mut harness = Harness::new(entry);
    let before = harness.app.controller.recovery_session();
    let original = harness.app.controller.active_document().unwrap().clone();
    harness.type_split(entry, &before);
    harness.key(key, egui::Modifiers::NONE);
    harness.frame(vec![]);
    let after = harness.app.controller.active_document().unwrap().clone();
    let mut expected = original.draft.clone();
    let patch = if matches!(entry, Entry::Gain) {
        let module = original
            .draft
            .view
            .module(ModuleId::new(3).unwrap())
            .unwrap();
        EditPatch::SetParameter {
            module: module.module,
            model: module.model,
            model_index: ModelIndex(0),
            parameter: ParamId::new(0).unwrap(),
            value: F32Bits::from_f32(73.0),
        }
    } else {
        entry.patch()
    };
    expected.apply(&patch).unwrap();
    assert_eq!(after.draft, expected);
    assert_eq!(after.local_revision.0, original.local_revision.0 + 1);
    assert_eq!(after.undo.past.len(), original.undo.past.len() + 1);
    assert_eq!(after.undo.past.last(), Some(&original.draft));
    assert!(after.undo.future.is_empty());
    assert!(after.undo.gesture.is_none());
    assert_eq!(after.baseline, original.baseline);
    assert!(after.pending.is_empty());
    // Tab may focus the next text spinner. Escape leaves that unrelated editor
    // without committing a change, so Ctrl+Z reaches the app's preset history.
    harness.key(egui::Key::Escape, egui::Modifiers::NONE);
    harness.key(egui::Key::Z, egui::Modifiers::COMMAND);
    assert_eq!(
        harness.app.controller.active_document().unwrap().draft,
        original.draft
    );
    assert_eq!(
        harness.app.controller.active_document().unwrap().undo.past,
        original.undo.past
    );
    harness.key(
        egui::Key::Z,
        egui::Modifiers::COMMAND | egui::Modifiers::SHIFT,
    );
    assert_eq!(
        harness.app.controller.active_document().unwrap().draft,
        expected
    );
    harness.assert_no_device();
}

fn cancel(entry: Entry) {
    let mut harness = Harness::new(entry);
    let before = harness.app.controller.recovery_session();
    harness.type_split(entry, &before);
    harness.key(egui::Key::Escape, egui::Modifiers::NONE);
    harness.frame(vec![]);
    harness.key(egui::Key::Tab, egui::Modifiers::NONE);
    assert!(harness.app.controller.recovery_session() == before);
    harness.assert_no_device();
}

macro_rules! entry_tests {
    ($enter:ident, $blur:ident, $escape:ident, $entry:ident) => {
        #[test]
        fn $enter() {
            commit(Entry::$entry, egui::Key::Enter);
        }
        #[test]
        fn $blur() {
            commit(Entry::$entry, egui::Key::Tab);
        }
        #[test]
        fn $escape() {
            cancel(Entry::$entry);
        }
    };
}
entry_tests!(gain_enter, gain_focus_loss, gain_escape, Gain);
entry_tests!(tempo_enter, tempo_focus_loss, tempo_escape, Tempo);
entry_tests!(level_enter, level_focus_loss, level_escape, Level);
entry_tests!(pan_enter, pan_focus_loss, pan_escape, Pan);

#[test]
fn simulated_buffer_split_typing_and_escape_never_queue_device_writes() {
    for entry in [Entry::Gain, Entry::Tempo, Entry::Level, Entry::Pan] {
        let mut harness = Harness::new(entry);
        harness.open_simulated_buffer();
        let before = harness.app.controller.recovery_session();
        harness.type_split(entry, &before);
        harness.key(egui::Key::Escape, egui::Modifiers::NONE);
        harness.frame(vec![]);
        assert!(harness.app.controller.recovery_session() == before);
        harness.assert_no_device();
    }
}

#[test]
fn simulated_buffer_counter_observes_one_write_only_after_enter() {
    let mut harness = Harness::new(Entry::Gain);
    harness.open_simulated_buffer();
    let before = harness.app.controller.recovery_session();
    harness.type_split(Entry::Gain, &before);
    harness.key(egui::Key::Enter, egui::Modifiers::NONE);
    assert_eq!(harness.submissions.load(Ordering::SeqCst), 1);
    assert_eq!(
        harness
            .app
            .controller
            .active_document()
            .unwrap()
            .draft
            .view
            .module(ModuleId::new(3).unwrap())
            .unwrap()
            .parameters[0],
        F32Bits::from_f32(73.0)
    );
    // The one simulated write is rejected by the deliberately disconnected
    // counting service. This is an effect-boundary test, not a hardware ACK.
}

#[test]
fn cancelling_then_refocusing_a_numeric_entry_allows_one_fresh_commit() {
    for entry in [Entry::Gain, Entry::Tempo, Entry::Level, Entry::Pan] {
        let mut harness = Harness::new(entry);
        let before = harness.app.controller.recovery_session();
        harness.type_split(entry, &before);
        harness.key(egui::Key::Escape, egui::Modifiers::NONE);
        harness.frame(vec![]);
        assert!(harness.app.controller.recovery_session() == before);
        harness.type_split(entry, &before);
        harness.key(egui::Key::Enter, egui::Modifiers::NONE);
        assert_eq!(
            harness
                .app
                .controller
                .active_document()
                .unwrap()
                .undo
                .past
                .len(),
            before.documents[0].undo.past.len() + 1
        );
        harness.assert_no_device();
    }
}

#[test]
fn catalog_slider_keyboard_and_spinbutton_accessibility_stay_immediate() {
    let mut harness = Harness::new(Entry::Gain);
    let controls: Vec<_> = harness
        .nodes
        .values()
        .filter(|node| {
            matches!(node.role(), Role::Slider | Role::SpinButton) && harness.name(node) == "Gain"
        })
        .collect();
    assert_eq!(controls.len(), 2);
    for node in controls {
        assert_eq!(node.min_numeric_value(), Some(0.0));
        assert_eq!(node.max_numeric_value(), Some(100.0));
        for action in [
            Action::Focus,
            Action::Increment,
            Action::Decrement,
            Action::SetValue,
        ] {
            assert!(node.supports_action(action));
        }
    }
    harness.focus(Entry::Gain, Role::Slider);
    let before = harness
        .app
        .controller
        .active_document()
        .unwrap()
        .draft
        .clone();
    harness.key(egui::Key::ArrowRight, egui::Modifiers::NONE);
    assert_ne!(
        harness.app.controller.active_document().unwrap().draft,
        before
    );
    harness.key(egui::Key::Z, egui::Modifiers::COMMAND);
    assert_eq!(
        harness.app.controller.active_document().unwrap().draft,
        before
    );
    harness.focus(Entry::Gain, Role::SpinButton);
    harness.key(egui::Key::ArrowUp, egui::Modifiers::NONE);
    assert_ne!(
        harness.app.controller.active_document().unwrap().draft,
        before
    );
    harness.key(egui::Key::Escape, egui::Modifiers::NONE);
    harness.key(egui::Key::Z, egui::Modifiers::COMMAND);
    assert_eq!(
        harness.app.controller.active_document().unwrap().draft,
        before
    );
    harness.assert_no_device();
}

#[test]
fn catalog_numeric_drag_still_groups_multiple_frames_into_one_undo_step() {
    let mut harness = Harness::new(Entry::Gain);
    let before = harness.app.controller.active_document().unwrap().clone();
    let bounds = harness
        .nodes
        .values()
        .find(|node| node.role() == Role::SpinButton && harness.name(node) == "Gain")
        .unwrap()
        .bounds()
        .unwrap();
    let origin = egui::pos2(
        ((bounds.x0 + bounds.x1) * 0.5) as f32,
        ((bounds.y0 + bounds.y1) * 0.5) as f32,
    );
    harness.frame(vec![
        egui::Event::PointerMoved(origin),
        egui::Event::PointerButton {
            pos: origin,
            button: egui::PointerButton::Primary,
            pressed: true,
            modifiers: egui::Modifiers::NONE,
        },
    ]);
    for distance in [8.0, 16.0, 24.0] {
        harness.frame(vec![egui::Event::PointerMoved(
            origin + egui::vec2(distance, 0.0),
        )]);
    }
    let end = origin + egui::vec2(24.0, 0.0);
    harness.frame(vec![egui::Event::PointerButton {
        pos: end,
        button: egui::PointerButton::Primary,
        pressed: false,
        modifiers: egui::Modifiers::NONE,
    }]);
    let after = harness.app.controller.active_document().unwrap();
    assert_ne!(after.draft, before.draft);
    assert_eq!(after.undo.past.len(), before.undo.past.len() + 1);
    assert_eq!(after.undo.past.last(), Some(&before.draft));
    assert!(after.undo.gesture.is_none());
    harness.key(egui::Key::Z, egui::Modifiers::COMMAND);
    assert_eq!(
        harness.app.controller.active_document().unwrap().draft,
        before.draft
    );
    harness.assert_no_device();
}

#[test]
fn catalog_numeric_accesskit_setvalue_increment_and_decrement_remain_immediate() {
    let mut harness = Harness::new(Entry::Gain);
    for (action, data) in [
        (
            Action::SetValue,
            Some(egui::accesskit::ActionData::NumericValue(73.0)),
        ),
        (Action::Increment, None),
        (Action::Decrement, None),
    ] {
        let before = harness.app.controller.active_document().unwrap().clone();
        let target = *harness
            .nodes
            .iter()
            .find(|(_, node)| node.role() == Role::SpinButton && harness.name(node) == "Gain")
            .unwrap()
            .0;
        harness.frame(vec![egui::Event::AccessKitActionRequest(
            egui::accesskit::ActionRequest {
                action,
                target_tree: egui::accesskit::TreeId::ROOT,
                target_node: target,
                data,
            },
        )]);
        let after = harness.app.controller.active_document().unwrap();
        assert_ne!(after.draft, before.draft);
        assert_eq!(after.undo.past.len(), before.undo.past.len() + 1);
        harness.key(egui::Key::Z, egui::Modifiers::COMMAND);
        assert_eq!(
            harness.app.controller.active_document().unwrap().draft,
            before.draft
        );
        harness.assert_no_device();
    }
}

#[test]
fn invalid_numeric_text_does_not_commit_on_enter_or_focus_loss() {
    for entry in [Entry::Gain, Entry::Tempo, Entry::Level, Entry::Pan] {
        for key in [egui::Key::Enter, egui::Key::Tab] {
            let mut harness = Harness::new(entry);
            let before = harness.app.controller.recovery_session();
            harness.focus(entry, Role::SpinButton);
            harness.key(egui::Key::A, egui::Modifiers::COMMAND);
            harness.frame(vec![egui::Event::Text("invalid".into())]);
            assert!(harness.app.controller.recovery_session() == before);
            harness.key(key, egui::Modifiers::NONE);
            harness.frame(vec![]);
            assert!(harness.app.controller.recovery_session() == before);
            harness.assert_no_device();
        }
    }
}
