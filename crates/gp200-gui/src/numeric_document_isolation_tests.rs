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
            "gp200-numeric-isolation-{}-{}",
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
            AppController::with_demo(named_snapshot("Isolation A"), preferences.clone()).unwrap();
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
    fn install_simulated_buffer(&mut self) {
        let active = self.app.controller.active_document_id().unwrap().clone();
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
                snapshot: named_snapshot("Isolation B"),
            },
        ] {
            self.app.controller.dispatch(AppInput::Device(event));
        }
        assert!(self.app.controller.activate_document(&active));
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

fn named_snapshot(name: &str) -> Arc<PresetSnapshot> {
    let baseline = gp200_sim::captured_demo_snapshot();
    let mut draft = PresetDraft::from_snapshot(&baseline);
    draft
        .apply(&EditPatch::SetMetadata(MetadataPatch {
            name: Some(name.into()),
            ..Default::default()
        }))
        .unwrap();
    Arc::new(
        draft
            .snapshot(&baseline, baseline.source.clone(), baseline.revision)
            .unwrap(),
    )
}

impl Harness {
    fn click_document(&mut self, name: &str) {
        let bounds = self
            .nodes
            .values()
            .find(|node| node.supports_action(Action::Click) && self.name(node) == name)
            .unwrap_or_else(|| panic!("Sidebar document {name:?} was unavailable"))
            .bounds()
            .unwrap();
        let position = egui::pos2(
            ((bounds.x0 + bounds.x1) * 0.5) as f32,
            ((bounds.y0 + bounds.y1) * 0.5) as f32,
        );
        // A full click is valid egui input in one frame. The sidebar handles
        // this click before the editor renders and observes lost focus.
        self.frame(vec![
            egui::Event::PointerMoved(position),
            egui::Event::PointerButton {
                pos: position,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: egui::Modifiers::NONE,
            },
            egui::Event::PointerButton {
                pos: position,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::NONE,
            },
        ]);
    }
    fn assert_exact(&self, before: &RecoverySession, active: &DocumentId) {
        let mut expected = before.clone();
        expected.active_document = Some(active.clone());
        assert!(
            self.app.controller.recovery_session() == expected,
            "Document switching must preserve exact draft fields, baselines, history, revisions and A/B"
        );
        self.assert_no_device();
    }
}

fn switch_offline(entry: Entry) {
    let mut harness = Harness::new(entry);
    let a = harness.app.controller.active_document_id().unwrap().clone();
    harness
        .app
        .controller
        .dispatch(AppInput::Ui(UiIntent::NewOfflineFrom(named_snapshot(
            "Isolation B",
        ))));
    let b = harness.app.controller.active_document_id().unwrap().clone();
    harness.app.controller.activate_document(&a);
    harness.frame(vec![]);
    let before = harness.app.controller.recovery_session();
    harness.type_split(entry, &before);
    harness.click_document("Isolation B");
    assert_eq!(harness.app.controller.active_document_id(), Some(&b));
    harness.assert_exact(&before, &b);
    for _ in 0..3 {
        harness.frame(vec![]);
        harness.assert_exact(&before, &b);
    }
    harness.click_document("Isolation A");
    assert_eq!(harness.app.controller.active_document_id(), Some(&a));
    harness.assert_exact(&before, &a);
    harness.frame(vec![]);
    harness.assert_exact(&before, &a);
    // A fresh entry after the round trip must still commit once.
    harness.type_split(entry, &before);
    harness.key(egui::Key::Enter, egui::Modifiers::NONE);
    let after = harness.app.controller.active_document().unwrap();
    assert_eq!(after.undo.past.len(), 1);
    assert_eq!(
        after.local_revision.0,
        before.documents[0].local_revision.0 + 1
    );
    harness.assert_no_device();
}

#[test]
fn equal_gain_offline_documents_retire_unfinished_text() {
    switch_offline(Entry::Gain);
}
#[test]
fn equal_tempo_offline_documents_retire_unfinished_text() {
    switch_offline(Entry::Tempo);
}
#[test]
fn equal_level_offline_documents_retire_unfinished_text() {
    switch_offline(Entry::Level);
}
#[test]
fn equal_pan_offline_documents_retire_unfinished_text() {
    switch_offline(Entry::Pan);
}

#[test]
fn offline_to_equal_simulated_buffer_never_submits_unfinished_text() {
    for entry in [Entry::Gain, Entry::Tempo, Entry::Level, Entry::Pan] {
        let mut harness = Harness::new(entry);
        harness.install_simulated_buffer();
        harness.frame(vec![]);
        let before = harness.app.controller.recovery_session();
        harness.type_split(entry, &before);
        harness.click_document("Isolation B");
        let buffer = gp200_app::device_document_id();
        assert_eq!(harness.app.controller.active_document_id(), Some(&buffer));
        harness.assert_exact(&before, &buffer);
        for _ in 0..3 {
            harness.frame(vec![]);
            harness.assert_exact(&before, &buffer);
        }
    }
}

#[test]
fn session_epoch_change_retires_text_with_the_same_offline_document_and_baseline() {
    for entry in [Entry::Gain, Entry::Tempo, Entry::Level, Entry::Pan] {
        let mut harness = Harness::new(entry);
        let original = harness.app.controller.active_document().unwrap().clone();
        let before = harness.app.controller.recovery_session();
        harness.type_split(entry, &before);
        assert_eq!(harness.app.controller.state().epoch, SessionEpoch(0));
        harness.install_simulated_buffer();
        assert_eq!(harness.app.controller.state().epoch, SessionEpoch(1));
        let current = harness.app.controller.active_document().unwrap();
        assert_eq!(current.id, original.id);
        assert!(Arc::ptr_eq(&current.baseline, &original.baseline));
        let expected = harness.app.controller.recovery_session();
        for _ in 0..3 {
            harness.frame(vec![]);
            harness.assert_exact(&expected, &original.id);
        }
        harness.key(egui::Key::Tab, egui::Modifiers::NONE);
        harness.assert_exact(&expected, &original.id);
    }
}

#[test]
fn hidden_editor_round_trip_does_not_revive_unfinished_numeric_text() {
    for entry in [Entry::Gain, Entry::Tempo, Entry::Level, Entry::Pan] {
        let mut harness = Harness::new(entry);
        let active = harness.app.controller.active_document_id().unwrap().clone();
        let before = harness.app.controller.recovery_session();
        harness.type_split(entry, &before);
        harness.click_document("Help");
        assert_eq!(harness.app.controller.state().view, ViewId::Help);
        for _ in 0..2 {
            harness.frame(vec![]);
        }
        harness.click_document("Editor");
        assert_eq!(harness.app.controller.state().view, ViewId::Editor);
        harness.assert_exact(&before, &active);
        // Refocusing without replacing the text must expose the baseline,
        // rather than accepting an entry abandoned on a different page.
        harness.focus(entry, Role::SpinButton);
        harness.key(egui::Key::Enter, egui::Modifiers::NONE);
        harness.assert_exact(&before, &active);
    }
}

#[test]
fn hidden_module_round_trip_does_not_revive_unfinished_gain_text() {
    let mut harness = Harness::new(Entry::Gain);
    let active = harness.app.controller.active_document_id().unwrap().clone();
    let before = harness.app.controller.recovery_session();
    harness.type_split(Entry::Gain, &before);
    for name in ["WAH", "AMP"] {
        let label = harness
            .nodes
            .values()
            .filter(|node| node.supports_action(Action::Click))
            .map(|node| harness.name(node))
            .find(|label| label.starts_with(&format!("{name}\n")))
            .unwrap();
        harness.click_document(&label);
        assert_eq!(harness.app.selected_module.name(), name);
        for _ in 0..2 {
            harness.frame(vec![]);
        }
        harness.assert_exact(&before, &active);
    }
    harness.focus(Entry::Gain, Role::SpinButton);
    harness.key(egui::Key::Enter, egui::Modifiers::NONE);
    harness.assert_exact(&before, &active);
}

#[test]
fn model_change_and_undo_do_not_revive_unfinished_gain_text() {
    let mut harness = Harness::new(Entry::Gain);
    let original = harness.app.controller.active_document().unwrap().clone();
    let before = harness.app.controller.recovery_session();
    harness.type_split(Entry::Gain, &before);
    let module = ModuleId::new(3).unwrap();
    let old_model = original.draft.view.module(module).unwrap().model;
    let alternate = if old_model == ModelCode(0x0700_0001) {
        ModelCode(0x0700_0015)
    } else {
        ModelCode(0x0700_0001)
    };
    assert!(
        harness
            .app
            .controller
            .catalog()
            .model_by_id(3, alternate.0)
            .is_some()
    );
    // These are the same typed edit/Undo operations issued by the model picker
    // and history toolbar. Render the intervening model through the real egui
    // editor, then return to the exact original numeric values via Undo.
    harness.app.edit(EditPatch::SetModel {
        module,
        model: alternate,
    });
    for _ in 0..2 {
        harness.frame(vec![]);
    }
    assert_eq!(
        harness
            .app
            .controller
            .active_document()
            .unwrap()
            .draft
            .view
            .module(module)
            .unwrap()
            .model,
        alternate
    );
    harness.app.send(UiIntent::Undo(original.id.clone()));
    assert_eq!(
        harness.app.controller.active_document().unwrap().draft,
        original.draft
    );
    let expected = harness.app.controller.recovery_session();
    for _ in 0..2 {
        harness.frame(vec![]);
        harness.assert_exact(&expected, &original.id);
    }
    harness.focus(Entry::Gain, Role::SpinButton);
    harness.key(egui::Key::Enter, egui::Modifiers::NONE);
    harness.assert_exact(&expected, &original.id);
}

#[test]
fn equal_baseline_same_buffer_capture_replacement_retires_unfinished_text() {
    for entry in [Entry::Gain, Entry::Tempo, Entry::Level, Entry::Pan] {
        let mut harness = Harness::new(entry);
        harness.install_simulated_buffer();
        let buffer = gp200_app::device_document_id();
        harness.app.controller.activate_document(&buffer);
        harness.frame(vec![]);
        let before = harness.app.controller.recovery_session();
        harness.type_split(entry, &before);
        let original = harness
            .app
            .controller
            .active_document()
            .unwrap()
            .baseline
            .clone();
        let slot = SlotId::new(original.context.unwrap().selector).unwrap();
        let effects = harness
            .app
            .controller
            .dispatch(AppInput::Ui(UiIntent::AuditionStored(slot)));
        let [
            Effect::Device(DeviceCommand::SelectStored {
                stamp,
                slot: actual,
            }),
        ] = effects.as_slice()
        else {
            panic!("Expected one explicit synthetic selection request");
        };
        assert_eq!(*actual, slot);
        let replacement = Arc::new((*original).clone());
        assert!(!Arc::ptr_eq(&original, &replacement));
        // Inject a fresh immutable capture through the real controller. The
        // synthetic selection request is not submitted to the device service.
        harness
            .app
            .controller
            .dispatch(AppInput::Device(DeviceEvent::SnapshotReceived {
                stamp: *stamp,
                target: PresetTarget::EditBuffer,
                snapshot: replacement,
            }));
        let expected = harness.app.controller.recovery_session();
        assert_eq!(harness.app.controller.active_document_id(), Some(&buffer));
        for _ in 0..3 {
            harness.frame(vec![]);
            harness.assert_exact(&expected, &buffer);
        }
        harness.key(egui::Key::Tab, egui::Modifiers::NONE);
        harness.assert_exact(&expected, &buffer);
    }
}
