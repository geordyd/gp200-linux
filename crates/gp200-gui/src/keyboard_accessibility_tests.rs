//! Real egui keyboard and AccessKit events; no OS input injection or MIDI.
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
            "gp200-keyboard-{}-{}",
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

struct Harness {
    app: StudioApp,
    context: egui::Context,
    nodes: BTreeMap<NodeId, Node>,
    submissions: Arc<AtomicUsize>,
    time: f64,
    size: egui::Vec2,
    _directory: Directory,
}
impl Harness {
    fn new(theme: ThemePreference, scale: f32) -> Self {
        let directory = Directory::new();
        let preferences = AppPreferences {
            theme,
            scale,
            ..Default::default()
        };
        let controller =
            AppController::with_demo(gp200_sim::captured_demo_snapshot(), preferences.clone())
                .unwrap();
        let submissions = Arc::new(AtomicUsize::new(0));
        let services = RuntimeServices::with_preferences_path(
            Box::new(NoDevice(submissions.clone())),
            no_wake(),
            preferences,
            directory.0.join("preferences.json"),
        );
        let context = egui::Context::default();
        theme::install_fonts(&context);
        context.enable_accesskit();
        theme::apply(&context, theme, scale);
        let mut harness = Self {
            app: StudioApp::with_services(controller, services),
            context,
            nodes: BTreeMap::new(),
            submissions,
            time: 1.0,
            size: egui::vec2(1280.0 / scale, 800.0 / scale),
            _directory: directory,
        };
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
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, self.size)),
                time: Some(self.time),
                events,
                ..Default::default()
            },
            |ui| {
                // Match the native app: global shortcuts precede real widgets.
                self.app.shortcuts(ui.ctx());
                self.app.render(ui);
            },
        );
        if let Some(update) = output.platform_output.accesskit_update.take() {
            self.nodes = update.nodes.into_iter().collect();
        }
        output.textures_delta.clear();
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
    fn focused(&self) -> Option<NodeId> {
        self.context
            .memory(|memory| memory.focused())
            .map(|id| id.accesskit_id())
    }
    fn name(&self, node: &Node) -> String {
        if let Some(label) = node.label() {
            return label.into();
        }
        node.labelled_by()
            .iter()
            .filter_map(|id| self.nodes.get(id))
            .filter_map(Node::value)
            .collect::<Vec<_>>()
            .join(" ")
    }
    fn tab_to(&mut self, predicate: impl Fn(&Self, &Node) -> bool) {
        for _ in 0..240 {
            if let Some(node) = self.focused().and_then(|id| self.nodes.get(&id))
                && predicate(self, node)
            {
                return;
            }
            self.key(egui::Key::Tab, egui::Modifiers::NONE);
        }
        let focused = self.focused().and_then(|id| self.nodes.get(&id));
        panic!("Keyboard target was unreachable; last focus: {focused:?}");
    }
    fn button(&mut self, name: &str) {
        self.tab_to(|harness, node| node.role() == Role::Button && harness.name(node) == name);
        self.key(egui::Key::Enter, egui::Modifiers::NONE);
        self.frame(vec![]);
    }
    fn assert_no_device(&self) {
        assert_eq!(self.submissions.load(Ordering::SeqCst), 0);
    }
}

fn english() -> Harness {
    Harness::new(ThemePreference::Dark, 1.0)
}
fn dirty(harness: &mut Harness) {
    harness.app.edit(EditPatch::SetMetadata(MetadataPatch {
        name: Some("Keyboard draft".into()),
        ..Default::default()
    }));
    assert!(harness.app.controller.active_document().unwrap().dirty());
}

#[test]
fn tab_and_enter_reach_every_workspace_without_mutating_a_draft_or_device() {
    let mut harness = english();
    let draft = harness
        .app
        .controller
        .active_document()
        .unwrap()
        .draft
        .clone();
    for (name, view) in [
        ("Help", ViewId::Help),
        ("Settings", ViewId::AppSettings),
        ("Diagnostics", ViewId::Diagnostics),
        ("Practice", ViewId::Practice),
        ("Performance", ViewId::Performance),
        ("Device", ViewId::GlobalSettings),
        ("Assignments", ViewId::Assignments),
        ("Assets", ViewId::Assets),
        ("Library", ViewId::Library),
        ("Editor", ViewId::Editor),
    ] {
        harness.button(name);
        assert_eq!(harness.app.controller.state().view, view);
        assert_eq!(
            harness.app.controller.active_document().unwrap().draft,
            draft
        );
    }
    harness.assert_no_device();
}

#[test]
fn catalog_numeric_slider_and_entry_have_the_same_accessible_label_range_and_actions() {
    let harness = english();
    let controls: Vec<_> = harness
        .nodes
        .values()
        .filter(|node| {
            matches!(node.role(), Role::Slider | Role::SpinButton) && harness.name(node) == "Gain"
        })
        .collect();
    assert_eq!(
        controls.len(),
        2,
        "Both alternatives must expose the visible Gain label"
    );
    for node in controls {
        assert_eq!(node.min_numeric_value(), Some(0.0));
        assert_eq!(node.max_numeric_value(), Some(100.0));
        assert!(node.supports_action(Action::Focus));
        assert!(node.supports_action(Action::Increment));
        assert!(node.supports_action(Action::Decrement));
        assert!(node.supports_action(Action::SetValue));
    }
    harness.assert_no_device();
}

#[test]
fn keyboard_numeric_edit_undo_and_redo_preserve_all_unselected_native_fields() {
    let mut harness = english();
    let before = harness
        .app
        .controller
        .active_document()
        .unwrap()
        .draft
        .clone();
    harness.tab_to(|harness, node| node.role() == Role::Slider && harness.name(node) == "Gain");
    harness.key(egui::Key::ArrowRight, egui::Modifiers::NONE);
    let after = harness
        .app
        .controller
        .active_document()
        .unwrap()
        .draft
        .clone();
    assert_ne!(after, before);
    let module = ModuleId::new(3).unwrap();
    let old = before.view.module(module).unwrap();
    let new = after.view.module(module).unwrap();
    assert_eq!(new.parameters[0].as_f32(), old.parameters[0].as_f32() + 1.0);
    assert_eq!(&new.parameters[1..], &old.parameters[1..]);
    assert_eq!(new.model, old.model);
    assert_eq!(new.state_raw, old.state_raw);
    for module in ModuleId::all().filter(|id| id.get() != 3) {
        assert_eq!(after.view.module(module), before.view.module(module));
    }
    assert_eq!(after.view.metadata, before.view.metadata);
    harness.key(egui::Key::Z, egui::Modifiers::COMMAND);
    assert_eq!(
        harness.app.controller.active_document().unwrap().draft,
        before
    );
    harness.key(
        egui::Key::Z,
        egui::Modifiers::COMMAND | egui::Modifiers::SHIFT,
    );
    assert_eq!(
        harness.app.controller.active_document().unwrap().draft,
        after
    );
    harness.assert_no_device();
}

#[test]
fn text_entry_undo_does_not_undo_the_preset_history() {
    let mut harness = english();
    dirty(&mut harness);
    let before = harness.app.controller.recovery_session();
    harness.tab_to(|_, node| {
        node.role() == Role::TextInput && node.placeholder() == Some("Find a preset")
    });
    harness.frame(vec![egui::Event::Text("alpha".into())]);
    harness.frame(vec![]);
    assert_eq!(harness.app.browser_search, "alpha");
    assert!(harness.context.text_edit_focused());
    harness.key(egui::Key::Z, egui::Modifiers::COMMAND);
    assert!(
        harness.app.controller.recovery_session() == before,
        "Preset draft/history changed while undoing a search field"
    );
    assert_eq!(harness.app.browser_search, "");
    harness.assert_no_device();
}

#[test]
fn keyboard_dirty_switch_cancel_preserves_exact_work_and_discard_changes_document_only_after_review()
 {
    let mut harness = english();
    dirty(&mut harness);
    let before = harness.app.controller.recovery_session();
    let id = harness.app.controller.active_document_id().unwrap().clone();
    harness.button("New from capture");
    assert!(harness.app.pending_switch.is_some());
    assert_eq!(harness.app.controller.active_document_id(), Some(&id));
    harness.key(egui::Key::Escape, egui::Modifiers::NONE);
    assert!(harness.app.pending_switch.is_none());
    assert_eq!(harness.app.controller.recovery_session(), before);
    harness.button("New from capture");
    harness.button("Discard and continue");
    assert!(harness.app.switch_wait.is_some());
    harness.app.advance_workflows();
    assert_ne!(harness.app.controller.active_document_id(), Some(&id));
    assert!(!harness.app.controller.active_document().unwrap().dirty());
    harness.assert_no_device();
}

#[test]
fn keyboard_save_preview_and_escape_do_not_submit_or_modify_the_draft() {
    let mut harness = english();
    dirty(&mut harness);
    let before = harness.app.controller.recovery_session();
    harness.button("Save to pedal…");
    assert!(harness.app.save_review);
    harness.key(egui::Key::Escape, egui::Modifiers::NONE);
    assert!(!harness.app.save_review);
    assert_eq!(harness.app.controller.recovery_session(), before);
    harness.assert_no_device();
}

#[test]
fn signal_chain_selection_has_keyboard_access_and_announces_the_active_effect() {
    let mut harness = english();
    let amp = harness
        .nodes
        .values()
        .find(|node| node.role() == Role::Button && harness.name(node).starts_with("AMP\n"))
        .unwrap();
    assert_eq!(amp.toggled(), Some(egui::accesskit::Toggled::True));
    harness.tab_to(|harness, node| {
        node.role() == Role::Button && harness.name(node).starts_with("NR\n")
    });
    harness.key(egui::Key::Enter, egui::Modifiers::NONE);
    assert_eq!(harness.app.selected_module.get(), 4);
    let amp = harness
        .nodes
        .values()
        .find(|node| node.role() == Role::Button && harness.name(node).starts_with("AMP\n"))
        .unwrap();
    let nr = harness
        .nodes
        .values()
        .find(|node| node.role() == Role::Button && harness.name(node).starts_with("NR\n"))
        .unwrap();
    assert_eq!(amp.toggled(), Some(egui::accesskit::Toggled::False));
    assert_eq!(nr.toggled(), Some(egui::accesskit::Toggled::True));
    harness.assert_no_device();
}

#[test]
fn recovery_review_keep_and_reopen_are_keyboard_actions_with_exact_offline_history() {
    let mut harness = english();
    let startup = harness.app.controller.active_document_id().unwrap().clone();
    let startup_draft = harness
        .app
        .controller
        .active_document()
        .unwrap()
        .draft
        .clone();
    let mut previous = AppController::with_demo(
        gp200_sim::captured_demo_snapshot(),
        AppPreferences::default(),
    )
    .unwrap();
    let id = previous.active_document_id().unwrap().clone();
    previous.capture_comparison(&id, ComparisonSlot::A).unwrap();
    previous.dispatch(AppInput::Ui(UiIntent::ApplyEdit {
        document: id.clone(),
        patch: EditPatch::SetMetadata(MetadataPatch {
            name: Some("Recovered keyboard work".into()),
            notes: Some("Local history retained".into()),
            ..Default::default()
        }),
    }));
    previous.capture_comparison(&id, ComparisonSlot::B).unwrap();
    let session = previous.recovery_session();
    let store =
        gp200_library::DraftRecoveryStore::open(harness._directory.0.join("recovery")).unwrap();
    let found = store.save(&session).unwrap();
    let retained_hash = found.sha256.clone();
    harness.app.recovery_ui.found = Some(found);
    harness.app.recovery_ui.replace_startup = false;
    harness.frame(vec![]);
    harness.button("Review recovery");
    assert!(harness.app.recovery_ui.review_open);
    harness.button("Keep for later");
    assert!(!harness.app.recovery_ui.review_open);
    assert_eq!(harness.app.controller.active_document_id(), Some(&startup));
    assert!(harness.app.controller.active_document().unwrap().draft == startup_draft);
    assert_eq!(store.discover().unwrap().unwrap().sha256, retained_hash);
    harness.button("Review recovery");
    harness.button("Recover all offline");
    assert!(!harness.app.recovery_ui.review_open);
    let recovered = harness.app.controller.recovery_session();
    assert_eq!(recovered.documents.len(), 2);
    let active = recovered
        .documents
        .iter()
        .find(|document| Some(&document.original_id) == recovered.active_document.as_ref())
        .unwrap();
    assert!(active.draft == session.documents[0].draft);
    assert!(active.undo == session.documents[0].undo);
    assert!(active.comparison == session.documents[0].comparison);
    assert!(matches!(active.original_target, EditTarget::Offline(_)));
    assert_eq!(store.discover().unwrap().unwrap().sha256, retained_hash);
    harness.assert_no_device();
}

#[test]
fn keyboard_asset_inspection_binds_exact_bytes_and_path_edit_retires_review() {
    let mut harness = english();
    const WAV: &[u8] =
        include_bytes!("../../../tests/fixtures/device-assets/synthetic-ir-pcm24.wav");
    let source = harness._directory.0.join("source.wav");
    fs::write(&source, WAV).unwrap();
    harness.button("Assets");
    harness.button("Device review");
    harness.tab_to(|_, node| {
        node.role() == Role::TextInput && node.placeholder() == Some("WAV or CLO source path")
    });
    harness.frame(vec![egui::Event::Text(source.display().to_string())]);
    harness.key(egui::Key::Tab, egui::Modifiers::NONE);
    harness.button("Inspect exact recipe");
    let started = Instant::now();
    let report = loop {
        harness.frame(vec![]);
        if let Some(report) = harness.app.asset_job_review_report() {
            break report;
        }
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "Local inspection did not settle"
        );
        std::thread::sleep(Duration::from_millis(2));
    };
    assert_eq!(
        report["review"]["source_sha256"],
        serde_json::to_value(JobSha256::of(WAV)).unwrap()
    );
    assert_eq!(report["review"]["derived_bytes"], 4096);
    assert_eq!(report["review"]["hardware_commands"], 0);
    assert_eq!(report["review"]["permits_transfer"], false);
    assert_eq!(report["review"]["full_content_backup_verified"], false);
    let prepare = harness
        .nodes
        .values()
        .find(|node| harness.name(node) == "Prepare slot review")
        .unwrap();
    assert!(prepare.is_disabled());
    harness.tab_to(|_, node| {
        node.role() == Role::TextInput && node.placeholder() == Some("WAV or CLO source path")
    });
    harness.key(egui::Key::A, egui::Modifiers::COMMAND);
    harness.frame(vec![egui::Event::Text("changed-source.wav".into())]);
    harness.frame(vec![]);
    assert_eq!(harness.app.asset_job_ui.path, "changed-source.wav");
    assert!(harness.app.asset_job_review_report().is_none());
    assert_eq!(fs::read(&source).unwrap(), WAV);
    harness.assert_no_device();
}

#[test]
fn compact_keyboard_navigation_remains_visible_in_both_themes_and_three_scales() {
    for theme in [ThemePreference::Dark, ThemePreference::Light] {
        for scale in [1.0, 1.5, 2.0] {
            let mut harness = Harness::new(theme, scale);
            harness.size = egui::vec2(960.0 / scale, 640.0 / scale);
            harness.frame(vec![]);
            harness.frame(vec![]);
            let before = harness.app.controller.recovery_session();
            for (name, view) in [
                ("Help", ViewId::Help),
                ("Settings", ViewId::AppSettings),
                ("Editor", ViewId::Editor),
            ] {
                harness.tab_to(|harness, node| {
                    node.role() == Role::Button && harness.name(node) == name
                });
                let focused = harness.nodes.get(&harness.focused().unwrap()).unwrap();
                let bounds = focused.bounds().unwrap();
                assert!(
                    bounds.x0 >= 0.0
                        && bounds.y0 >= 0.0
                        && bounds.x1 <= f64::from(harness.size.x)
                        && bounds.y1 <= f64::from(harness.size.y),
                    "Navigation clipped at {scale}: {bounds:?}"
                );
                harness.key(egui::Key::Enter, egui::Modifiers::NONE);
                assert_eq!(harness.app.controller.state().view, view);
            }
            assert!(harness.app.controller.recovery_session() == before);
            harness.assert_no_device();
        }
    }
}

#[test]
fn keyboard_firmware_review_and_local_packet_preview_keep_hardware_unavailable() {
    let mut harness = english();
    const IMAGE: &[u8] =
        include_bytes!("../../../tests/fixtures/device-assets/synthetic-firmware.bin");
    let source = harness._directory.0.join("synthetic-firmware.bin");
    fs::write(&source, IMAGE).unwrap();
    let before = harness.app.controller.recovery_session();
    harness.button("Assets");
    harness.button("Firmware");
    harness.tab_to(|_, node| {
        node.role() == Role::TextInput && node.placeholder() == Some("Firmware .bin path")
    });
    harness.frame(vec![egui::Event::Text(source.display().to_string())]);
    harness.key(egui::Key::Tab, egui::Modifiers::NONE);
    harness.button("Inspect image");
    let started = Instant::now();
    while harness.app.firmware_review_report().is_none() {
        harness.app.firmware_ui.poll();
        harness.frame(vec![]);
        assert!(started.elapsed() < Duration::from_secs(3));
        std::thread::sleep(Duration::from_millis(2));
    }
    let image = harness.app.firmware_ui.image.clone().unwrap();
    let input_hash = image.review.summary().source_sha256;
    assert_eq!(input_hash, JobSha256::of(IMAGE));
    harness.button("Run offline rehearsal");
    let started = Instant::now();
    while harness.app.firmware_ui.busy() {
        harness.app.firmware_ui.poll();
        harness.frame(vec![]);
        assert!(started.elapsed() < Duration::from_secs(3));
        std::thread::sleep(Duration::from_millis(2));
    }
    let rehearsal = harness.app.firmware_ui.rehearsal.as_ref().unwrap();
    assert_eq!(
        rehearsal.phase,
        FirmwareRehearsalPhase::AwaitingBlockAcknowledgement
    );
    assert_eq!(rehearsal.simulated_block_acknowledgements, 0);
    assert_eq!(rehearsal.simulated_section_acknowledgements, 0);
    assert!(harness.app.controller.recovery_session() == before);
    assert_eq!(fs::read(&source).unwrap(), IMAGE);
    assert!(harness.nodes.values().all(|node| {
        node.role() != Role::Button
            || !matches!(
                harness.name(node).as_str(),
                "Flash" | "Update firmware" | "Transfer firmware"
            )
    }));
    harness.assert_no_device();
}

#[test]
fn keyboard_numeric_entry_accepts_exact_value_then_undoes_once() {
    let mut harness = english();
    let before = harness
        .app
        .controller
        .active_document()
        .unwrap()
        .draft
        .clone();
    // DragValue retains SpinButton semantics while its focused implementation
    // is TextEdit, keeping screen-reader increment actions during text entry.
    harness.tab_to(|harness, node| node.role() == Role::SpinButton && harness.name(node) == "Gain");
    assert!(harness.context.text_edit_focused());
    harness.key(egui::Key::A, egui::Modifiers::COMMAND);
    harness.frame(vec![egui::Event::Text("73".into())]);
    harness.key(egui::Key::Enter, egui::Modifiers::NONE);
    let after = harness
        .app
        .controller
        .active_document()
        .unwrap()
        .draft
        .clone();
    assert_eq!(
        after
            .view
            .module(ModuleId::new(3).unwrap())
            .unwrap()
            .parameters[0],
        F32Bits::from_f32(73.0)
    );
    harness.key(egui::Key::Z, egui::Modifiers::COMMAND);
    assert!(harness.app.controller.active_document().unwrap().draft == before);
    harness.assert_no_device();
}

#[test]
fn firmware_barrier_actual_keyboard_preserves_document_undo_and_blocks_normal_submissions() {
    let mut harness = Harness::new(ThemePreference::Dark, 1.0);
    let document = harness.app.controller.active_document_id().unwrap().clone();
    let before = harness.app.controller.recovery_session();
    let editor = harness
        .app
        .controller
        .begin_firmware_editor_preparation()
        .unwrap();
    let runtime = harness.app.services.freeze_firmware().unwrap();
    harness.key(egui::Key::Z, egui::Modifiers::COMMAND);
    harness.key(
        egui::Key::Z,
        egui::Modifiers::COMMAND | egui::Modifiers::SHIFT,
    );
    harness.key(egui::Key::S, egui::Modifiers::COMMAND);
    harness
        .app
        .send(UiIntent::Refresh(PresetTarget::EditBuffer));
    let (inputs, _) = harness.app.services.pump();
    assert!(inputs.is_empty());
    assert_eq!(harness.submissions.load(Ordering::SeqCst), 0);
    assert_eq!(harness.app.controller.recovery_session(), before);
    assert_eq!(harness.app.controller.active_document_id(), Some(&document));
    harness
        .app
        .services
        .release_firmware_with(&runtime, || {
            harness
                .app
                .controller
                .cancel_firmware_editor_preparation(&editor)
        })
        .unwrap();
}
