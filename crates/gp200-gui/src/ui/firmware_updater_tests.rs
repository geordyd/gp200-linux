//! Actual egui updater controls. No synthetic firmware authority or device I/O.
use super::firmware_updater::*;
use crate::{FirmwareCoordinatorPhase as Phase, StudioApp, runtime::RuntimeServices};
use eframe::egui;
use gp200_domain::*;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
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
pub(super) fn app() -> (StudioApp, egui::Context, Arc<AtomicUsize>) {
    let count = Arc::new(AtomicUsize::new(0));
    let controller = gp200_app::AppController::with_demo(
        gp200_sim::captured_demo_snapshot(),
        AppPreferences::default(),
    )
    .unwrap();
    let app = StudioApp::with_services(
        controller,
        RuntimeServices::with_preferences_path(
            Box::new(NoDevice(count.clone())),
            no_wake(),
            AppPreferences::default(),
            std::env::temp_dir().join(format!(
                "gp200-fw05-ui-test-{}-{}.json",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            )),
        ),
    );
    let context = egui::Context::default();
    crate::theme::install_fonts(&context);
    context.enable_accesskit();
    (app, context, count)
}
pub(super) fn render(
    app: &mut StudioApp,
    context: &egui::Context,
    size: egui::Vec2,
    events: Vec<egui::Event>,
) -> egui::FullOutput {
    let mut output = context.run_ui(
        egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
            events,
            ..Default::default()
        },
        |ui| app.firmware_workspace(ui),
    );
    output.textures_delta.clear();
    output
}
#[test]
fn native_preview_scene_cannot_enable_capture_or_update() {
    let (mut app, context, count) = app();
    let draft = app.controller.active_document().unwrap().draft.clone();
    for scene in [
        "image-reviewed",
        "source-replaced",
        "editor-resolution",
        "backup-coverage",
        "partial-acquisition",
        "proposed-loss",
        "waiting-for-store",
        "ownership-rejected",
        "simulated-transfer",
        "host-stop",
        "owner-retiring",
        "startup-no-store",
        "restart-quarantine",
        "finished-corrupt-tail",
    ] {
        app.open_firmware_updater_preview(scene).unwrap();
        let output = render(&mut app, &context, egui::vec2(960.0, 800.0), vec![]);
        assert!(!output.shapes.is_empty());
        assert_eq!(app.firmware_coordinator_view().phase, Phase::Idle);
        assert!(
            !app.firmware_updater_preview_report().unwrap()["permits_flash"]
                .as_bool()
                .unwrap()
        );
        assert_eq!(count.load(Ordering::SeqCst), 0);
        assert_eq!(app.controller.active_document().unwrap().draft, draft);
    }
}
#[test]
fn production_no_device_preparation_and_flash_buttons_remain_disabled() {
    let (mut app, context, count) = app();
    app.firmware_ui.updater.page = FirmwareSubview::Prepare;
    for _ in 0..2 {
        render(&mut app, &context, egui::vec2(640.0, 480.0), vec![]);
    }
    for label in [
        "Begin preparation",
        "Capture read-only backup",
        "Keep local draft for read-only capture",
    ] {
        assert!(
            app.firmware_ui
                .updater
                .widgets
                .iter()
                .any(|(name, _, enabled)| name == label && !enabled)
        );
    }
    assert_eq!(count.load(Ordering::SeqCst), 0);
}

fn accessible_name(
    nodes: &std::collections::BTreeMap<egui::accesskit::NodeId, egui::accesskit::Node>,
    node: &egui::accesskit::Node,
) -> String {
    if let Some(label) = node.label() {
        return label.into();
    }
    if let Some(value) = node.value() {
        return value.into();
    }
    node.labelled_by()
        .iter()
        .filter_map(|id| nodes.get(id))
        .filter_map(egui::accesskit::Node::value)
        .collect::<Vec<_>>()
        .join(" ")
}
fn key_event(key: egui::Key, pressed: bool, modifiers: egui::Modifiers) -> egui::Event {
    egui::Event::Key {
        key,
        physical_key: Some(key),
        pressed,
        repeat: false,
        modifiers,
    }
}
#[test]
fn tab_shift_tab_enter_and_space_navigate_the_actual_updater_widgets() {
    let (mut app, context, count) = app();
    let size = egui::vec2(1280.0, 800.0);
    let mut nodes = std::collections::BTreeMap::new();
    for _ in 0..2 {
        if let Some(update) = render(&mut app, &context, size, vec![])
            .platform_output
            .accesskit_update
        {
            nodes = update.nodes.into_iter().collect();
        }
    }
    let mut reached = false;
    for _ in 0..40 {
        if let Some(id) = context.memory(|m| m.focused()).map(|id| id.accesskit_id())
            && nodes
                .get(&id)
                .is_some_and(|node| accessible_name(&nodes, node) == "Prepare update")
        {
            reached = true;
            break;
        }
        for pressed in [true, false] {
            if let Some(update) = render(
                &mut app,
                &context,
                size,
                vec![key_event(egui::Key::Tab, pressed, egui::Modifiers::NONE)],
            )
            .platform_output
            .accesskit_update
            {
                nodes = update.nodes.into_iter().collect();
            }
        }
    }
    assert!(reached, "Prepare update must be reachable by Tab");
    for pressed in [true, false] {
        render(
            &mut app,
            &context,
            size,
            vec![key_event(egui::Key::Enter, pressed, egui::Modifiers::NONE)],
        );
    }
    assert_eq!(app.firmware_ui.updater.page, FirmwareSubview::Prepare);
    for pressed in [true, false] {
        render(
            &mut app,
            &context,
            size,
            vec![key_event(egui::Key::Tab, pressed, egui::Modifiers::SHIFT)],
        );
    }
    for pressed in [true, false] {
        render(
            &mut app,
            &context,
            size,
            vec![key_event(egui::Key::Space, pressed, egui::Modifiers::NONE)],
        );
    }
    assert_eq!(app.firmware_ui.updater.page, FirmwareSubview::FileReview);
    assert_eq!(count.load(Ordering::SeqCst), 0);
}
#[test]
fn disabled_start_update_accesskit_click_cannot_request_a_device_or_owner() {
    let (mut app, context, count) = app();
    app.firmware_ui.updater.page = FirmwareSubview::Prepare;
    let size = egui::vec2(1280.0, 1600.0);
    let output = render(&mut app, &context, size, vec![]);
    let nodes: std::collections::BTreeMap<_, _> = output
        .platform_output
        .accesskit_update
        .unwrap()
        .nodes
        .into_iter()
        .collect();
    let (id, node) = nodes
        .iter()
        .find(|(_, node)| accessible_name(&nodes, node) == "Start update")
        .expect("Labelled disabled start action");
    assert!(node.is_disabled());
    render(
        &mut app,
        &context,
        size,
        vec![egui::Event::AccessKitActionRequest(
            egui::accesskit::ActionRequest {
                action: egui::accesskit::Action::Click,
                target_tree: egui::accesskit::TreeId::ROOT,
                target_node: *id,
                data: None,
            },
        )],
    );
    assert_eq!(app.firmware_coordinator_view().phase, Phase::Idle);
    assert_eq!(count.load(Ordering::SeqCst), 0);
}
#[test]
fn long_paths_hashes_and_primary_actions_fit_the_minimum_width_in_both_themes() {
    for theme in [ThemePreference::Dark, ThemePreference::Light] {
        let (mut app, context, count) = app();
        crate::theme::apply(&context, theme, 1.0);
        app.open_firmware_updater_preview("image-reviewed").unwrap();
        app.firmware_ui
            .updater
            .preview
            .as_mut()
            .unwrap()
            .rows
            .push((
                "Long source path".into(),
                format!("/{}", "long-directory-name/".repeat(30)),
            ));
        for _ in 0..2 {
            render(&mut app, &context, egui::vec2(640.0, 480.0), vec![]);
        }
        for (name, rect, _) in &app.firmware_ui.updater.widgets {
            assert!(
                rect.min.x >= 0.0 && rect.max.x <= 640.0,
                "Critical widget {name} overflowed the minimum width: {rect:?}"
            );
        }
        assert_eq!(count.load(Ordering::SeqCst), 0);
    }
}
#[test]
fn owner_byte_origins_and_coverage_limitations_reach_the_real_accessible_tree() {
    let (mut app, context, count) = app();
    app.open_firmware_updater_preview("simulated-transfer")
        .unwrap();
    let output = render(&mut app, &context, egui::vec2(1280.0, 1600.0), vec![]);
    let nodes: std::collections::BTreeMap<_, _> = output
        .platform_output
        .accesskit_update
        .unwrap()
        .nodes
        .into_iter()
        .collect();
    let text = nodes
        .values()
        .map(|node| accessible_name(&nodes, node))
        .collect::<Vec<_>>()
        .join("\n");
    for required in [
        "Locally simulated source",
        "OS-offered wire",
        "Device-verified source",
        "Simulated owner counters",
        "0 bytes",
        "Stop host observation",
    ] {
        assert!(
            text.contains(required),
            "Missing accessible origin/status: {required}"
        );
    }
    app.open_firmware_updater_preview("proposed-loss").unwrap();
    let output = render(&mut app, &context, egui::vec2(1280.0, 2400.0), vec![]);
    let nodes: std::collections::BTreeMap<_, _> = output
        .platform_output
        .accesskit_update
        .unwrap()
        .nodes
        .into_iter()
        .collect();
    let text = nodes
        .values()
        .map(|node| accessible_name(&nodes, node))
        .collect::<Vec<_>>()
        .join("\n");
    for required in [
        "User IR",
        "installed audio unavailable",
        "SnapTone",
        "Restoration and migration",
        "I have stable pedal power",
        "Start update",
    ] {
        assert!(
            text.contains(required),
            "Missing accessible coverage/loss label: {required}"
        );
    }
    assert_eq!(count.load(Ordering::SeqCst), 0);
}

#[test]
fn full_app_loss_facts_fit_and_minimum_action_boundaries_stay_visible() {
    for theme in [ThemePreference::Dark, ThemePreference::Light] {
        for scene in [
            "editor-resolution",
            "backup-coverage",
            "partial-acquisition",
            "proposed-loss",
            "simulated-transfer",
            "host-stop",
            "finished-corrupt-tail",
        ] {
            let (mut app, context, count) = app();
            crate::theme::apply(&context, theme, 1.0);
            app.open_firmware_updater_preview(scene).unwrap();
            let mut nodes = std::collections::BTreeMap::new();
            let mut frame = |app: &mut StudioApp, size: egui::Vec2, events: Vec<egui::Event>| {
                let mut output = context.run_ui(
                    egui::RawInput {
                        screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
                        events,
                        ..Default::default()
                    },
                    |ui| app.render(ui),
                );
                if let Some(update) = output.platform_output.accesskit_update.take() {
                    nodes = update.nodes.into_iter().collect();
                }
                output.textures_delta.clear();
                nodes.clone()
            };
            let mut tree = std::collections::BTreeMap::new();
            for _ in 0..3 {
                tree = frame(&mut app, egui::vec2(1280.0, 800.0), vec![]);
            }
            let detail = app.firmware_ui.updater.detail_scroll.unwrap().3;
            let required: &[&str] = match app.firmware_ui.updater.page {
                FirmwareSubview::Prepare => &[
                    "User IR",
                    "installed audio unavailable",
                    "SnapTone",
                    "Conditionally not applicable",
                    "Restoration and migration",
                    "No accepted 1.8.2 restore/profile",
                    "Backup package",
                ],
                FirmwareSubview::Update => &[
                    "Locally simulated source",
                    "OS-offered wire",
                    "Device-verified source",
                    "0 bytes; physical completion remains separate",
                    "Completion",
                ],
                FirmwareSubview::History => &[
                    "Recorded status",
                    "Host finish recorded; device completion unverified",
                    "Derived reopen state",
                    "Interrupted / retained corrupt tail; device mode unknown",
                    "Accepted prefix",
                ],
                _ => unreachable!(),
            };
            for text in required {
                let node = tree
                    .values()
                    .find(|node| {
                        node.role() == egui::accesskit::Role::Label
                            && accessible_name(&tree, node).contains(text)
                            && node.bounds().is_some()
                    })
                    .unwrap_or_else(|| panic!("Missing actual rendered fact {text} in {scene}"));
                let b = node.bounds().unwrap();
                assert!(
                    b.x0 >= f64::from(detail.min.x) - 1.0
                        && b.x1 <= f64::from(detail.max.x) + 1.0
                        && b.y0 >= f64::from(detail.min.y) - 1.0
                        && b.y1 <= f64::from(detail.max.y) + 1.0,
                    "Fact {text} is outside the full app detail viewport in {theme:?}/{scene}: {b:?}, {detail:?}"
                );
            }
            for _ in 0..3 {
                tree = frame(&mut app, egui::vec2(640.0, 480.0), vec![]);
            }
            let detail = app.firmware_ui.updater.detail_scroll.unwrap().3;
            let action = match app.firmware_ui.updater.page {
                FirmwareSubview::Prepare => "Start update",
                FirmwareSubview::Update => {
                    if scene == "host-stop" {
                        "Stop requested"
                    } else {
                        "Stop host observation"
                    }
                }
                FirmwareSubview::History => "Export checked attempt…",
                _ => unreachable!(),
            };
            let node = tree
                .values()
                .find(|node| accessible_name(&tree, node) == action)
                .unwrap();
            let b = node.bounds().unwrap();
            assert!(node.is_disabled(), "Preview boundary must stay disabled");
            assert!(
                b.x0 >= 0.0 && b.x1 <= 640.0 && b.y0 >= 0.0 && b.y1 <= f64::from(detail.min.y),
                "Minimum action must be pinned visibly above the scroll region: {scene}, {b:?}, {detail:?}"
            );
            let status = &app.firmware_ui.updater.preview.as_ref().unwrap().status;
            let b = tree
                .values()
                .find(|node| accessible_name(&tree, node) == *status && node.bounds().is_some())
                .unwrap()
                .bounds()
                .unwrap();
            assert!(
                b.y0 >= 0.0 && b.y1 <= f64::from(detail.min.y),
                "Minimum status must remain visible: {scene}, {b:?}"
            );
            if app.firmware_ui.updater.page == FirmwareSubview::Prepare {
                let mut reached = std::collections::BTreeSet::new();
                for _ in 0..64 {
                    tree = frame(
                        &mut app,
                        egui::vec2(640.0, 480.0),
                        vec![
                            egui::Event::PointerMoved(detail.center()),
                            egui::Event::MouseWheel {
                                phase: egui::TouchPhase::Move,
                                unit: egui::MouseWheelUnit::Point,
                                delta: egui::vec2(0.0, -16.0),
                                modifiers: egui::Modifiers::NONE,
                            },
                        ],
                    );
                    for text in ["User IR", "SnapTone", "Restoration and migration"] {
                        if tree.values().any(|node| {
                            node.role() == egui::accesskit::Role::Label
                                && accessible_name(&tree, node) == text
                                && node.bounds().is_some_and(|b| {
                                    b.y0 >= f64::from(detail.min.y)
                                        && b.y1 <= f64::from(detail.max.y)
                                })
                        }) {
                            reached.insert(text);
                        }
                    }
                }
                assert_eq!(
                    reached.len(),
                    3,
                    "Every critical loss fact must be reachable through the actual minimum-size scroll area: {scene}, {reached:?}"
                );
            }
            assert_eq!(app.firmware_coordinator_view().phase, Phase::Idle);
            assert_eq!(count.load(Ordering::SeqCst), 0);
        }
    }
}

fn observed_session(app: &mut StudioApp, epoch_number: u64, endpoint: &str) -> DeviceJobSession {
    let epoch = SessionEpoch(epoch_number);
    let identity = DeviceIdentity {
        model: DeviceModel::Gp200,
        mode: BackendMode::Hardware,
        firmware: None,
        display_name: "Descriptive no-device regression identity".into(),
    };
    let capabilities = Capabilities::gp200_read_only();
    app.controller
        .dispatch(AppInput::Device(DeviceEvent::SessionChanged {
            epoch,
            identity: identity.clone(),
            capabilities: capabilities.clone(),
        }));
    let session = DeviceJobSession::observe_normal(
        epoch,
        DeviceSelection {
            input_id: format!("test:{endpoint}:in"),
            output_id: format!("test:{endpoint}:out"),
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
    app.controller
        .dispatch(AppInput::Device(DeviceEvent::DeviceJobSessionObserved(
            session.clone(),
        )));
    session
}
#[test]
fn manual_display_report_cannot_be_inherited_by_another_normal_session() {
    let (mut app, context, count) = app();
    let first = observed_session(&mut app, 41, "first-pedal");
    app.firmware_ui.updater.page = FirmwareSubview::Prepare;
    app.firmware_ui.updater.manual_version = "1.6.3".into();
    app.firmware_ui.updater.manual_date = "2026-10-11".into();
    let source = std::env::temp_dir().join(format!(
        "gp200-fw05-manual-report-{}-{}.bin",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(
        &source,
        include_bytes!("../../../../tests/fixtures/device-assets/synthetic-firmware.bin"),
    )
    .unwrap();
    app.inspect_firmware(source.clone());
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    while app.firmware_ui.busy() {
        app.firmware_ui.poll();
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    std::fs::remove_file(source).unwrap();
    app.associate_firmware_manual_report().unwrap();
    assert!(matches!(
        app.firmware_capture_context()
            .unwrap()
            .snaptone_applicability(),
        FirmwareAssetApplicability::ConditionalNotApplicable { .. }
    ));
    assert_eq!(
        app.firmware_ui.updater.manual_report_binding.as_ref(),
        Some(&first)
    );
    let second = observed_session(&mut app, 42, "second-pedal");
    assert!(
        app.firmware_capture_context()
            .unwrap_err()
            .message
            .contains("different or unconfirmed"),
        "A source change must reject the stale conditional report even before another UI frame"
    );
    render(&mut app, &context, egui::vec2(1280.0, 1600.0), vec![]);
    assert!(app.firmware_ui.updater.manual_report_binding.is_none());
    assert!(app.firmware_ui.updater.message.contains("session changed"));
    // Re-entry is a deliberate human association, not an automatic read.
    app.associate_firmware_manual_report().unwrap();
    assert_eq!(
        app.firmware_ui.updater.manual_report_binding.as_ref(),
        Some(&second)
    );
    assert_eq!(count.load(Ordering::SeqCst), 0);
}
#[test]
fn a_report_entered_without_hardware_session_is_not_associated_later_automatically() {
    let (mut app, context, count) = app();
    app.firmware_ui.updater.manual_version = "1.6.3".into();
    app.firmware_ui.updater.manual_date = "2026-10-11".into();
    assert!(app.associate_firmware_manual_report().is_err());
    observed_session(&mut app, 50, "later-pedal");
    render(&mut app, &context, egui::vec2(960.0, 800.0), vec![]);
    assert!(app.firmware_ui.updater.manual_report_binding.is_none());
    assert_eq!(count.load(Ordering::SeqCst), 0);
}
#[test]
fn distinct_diagnostic_panels_keep_independent_expanded_state_under_actual_clicks() {
    let context = egui::Context::default();
    crate::theme::install_fonts(&context);
    let mut headers = vec![];
    let mut frame = |events: Vec<egui::Event>| {
        headers.clear();
        let mut output = context.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(960.0, 800.0),
                )),
                events,
                ..Default::default()
            },
            |ui| {
                for (salt, message) in [
                    ("blockers", "Physical route unavailable"),
                    ("diagnostics", "An original event was retained"),
                    ("preparation", "Archive revalidation is missing"),
                ] {
                    headers.push(diagnostics(ui, salt, &[message.into()], 0).unwrap());
                }
            },
        );
        output.textures_delta.clear();
        headers.clone()
    };
    let before = frame(vec![]);
    assert_ne!(before[0].id, before[1].id);
    let point = before[0].rect.center();
    for pressed in [true, false] {
        frame(vec![
            egui::Event::PointerMoved(point),
            egui::Event::PointerButton {
                pos: point,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: egui::Modifiers::NONE,
            },
        ]);
    }
    assert!(
        !egui::collapsing_header::CollapsingState::load(&context, before[0].id)
            .unwrap()
            .is_open()
    );
    for header in &before[1..] {
        assert!(
            egui::collapsing_header::CollapsingState::load(&context, header.id)
                .unwrap()
                .is_open()
        );
    }
}
fn full_frame(app: &mut StudioApp, context: &egui::Context, events: Vec<egui::Event>) {
    let mut output = context.run_ui(
        egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(640.0, 480.0),
            )),
            events,
            ..Default::default()
        },
        |ui| app.render(ui),
    );
    output.textures_delta.clear();
}
fn click(app: &mut StudioApp, context: &egui::Context, label: &str) {
    let point = app
        .firmware_ui
        .updater
        .widgets
        .iter()
        .find(|(name, _, _)| name == label)
        .unwrap()
        .1
        .center();
    for pressed in [true, false] {
        full_frame(
            app,
            context,
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
fn updater_tabs_stay_pinned_and_each_subview_retains_its_own_real_scroll_state() {
    let (mut app, context, count) = app();
    app.open_firmware_updater_preview("proposed-loss").unwrap();
    for _ in 0..2 {
        full_frame(&mut app, &context, vec![]);
    }
    let before = app
        .firmware_ui
        .updater
        .widgets
        .iter()
        .find(|(name, _, _)| name == "Prepare update")
        .unwrap()
        .1;
    let (_, first_id, _, inner) = app.firmware_ui.updater.detail_scroll.unwrap();
    full_frame(
        &mut app,
        &context,
        vec![
            egui::Event::PointerMoved(inner.center()),
            egui::Event::MouseWheel {
                phase: egui::TouchPhase::Move,
                unit: egui::MouseWheelUnit::Point,
                delta: egui::vec2(0.0, -400.0),
                modifiers: egui::Modifiers::NONE,
            },
        ],
    );
    for _ in 0..3 {
        full_frame(&mut app, &context, vec![]);
    }
    let (_, _, first_offset, _) = app.firmware_ui.updater.detail_scroll.unwrap();
    assert!(first_offset.y > 0.0);
    let after = app
        .firmware_ui
        .updater
        .widgets
        .iter()
        .find(|(name, _, _)| name == "Prepare update")
        .unwrap()
        .1;
    assert_eq!(
        before, after,
        "Updater navigation must not move with detail scrolling"
    );
    click(&mut app, &context, "Update");
    let (page, second_id, second_offset, _) = app.firmware_ui.updater.detail_scroll.unwrap();
    assert_eq!(page, FirmwareSubview::Update);
    assert_ne!(first_id, second_id);
    assert_eq!(
        second_offset.y, 0.0,
        "A new subview must not inherit another screen's scroll offset"
    );
    click(&mut app, &context, "Prepare update");
    let (page, restored_id, restored_offset, _) = app.firmware_ui.updater.detail_scroll.unwrap();
    assert_eq!(page, FirmwareSubview::Prepare);
    assert_eq!(restored_id, first_id);
    assert!(restored_offset.y >= first_offset.y);
    assert_eq!(count.load(Ordering::SeqCst), 0);
}

// A real coordinator on the simulator service; no hardware/physical issuer.
pub(crate) mod live_fixture {
    use crate::*;
    use gp200_transport::{
        FirmwareCaptureWriteResult, FirmwareNormalCaptureObservation, MidiSession, ReceiveSink,
        TransportBackend,
    };
    use std::{
        path::PathBuf,
        sync::{
            Mutex,
            atomic::{AtomicU64, Ordering},
        },
        time::Instant,
    };
    static NEXT: AtomicU64 = AtomicU64::new(0);
    pub(crate) struct Directory(pub(crate) PathBuf);
    impl Directory {
        fn new() -> Self {
            let p = std::env::temp_dir().join(format!(
                "gp200-native-ui-coordinator-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&p).unwrap();
            Self(p)
        }
    }
    impl Drop for Directory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    #[derive(Default)]
    pub(crate) struct Log {
        pub(crate) offers: usize,
        pub(super) opens: usize,
        closes: usize,
        panic_capture_observation: bool,
        capture_observations: usize,
        fail_connect: bool,
        connect_attempts: usize,
    }
    struct ReplayBackend {
        inner: gp200_sim::SimulatorBackend,
        log: Arc<Mutex<Log>>,
    }
    impl TransportBackend for ReplayBackend {
        fn mode(&self) -> BackendMode {
            BackendMode::Simulator
        }
        fn enumerate(&mut self) -> Result<Vec<PortPairCandidate>, ServiceError> {
            self.inner.enumerate()
        }
        fn connect(
            &mut self,
            selection: &DeviceSelection,
            sink: ReceiveSink,
        ) -> Result<Box<dyn MidiSession>, ServiceError> {
            let fail = {
                let mut log = self.log.lock().unwrap();
                log.connect_attempts += 1;
                log.fail_connect
            };
            if fail {
                return Err(ServiceError::new(
                    ErrorKind::Io,
                    "Injected actual backend fresh connection failure",
                ));
            }
            let inner = self.inner.connect(selection, sink)?;
            self.log.lock().unwrap().opens += 1;
            Ok(Box::new(ReplaySession {
                inner: Some(inner),
                log: self.log.clone(),
            }))
        }
    }
    struct ReplaySession {
        inner: Option<Box<dyn MidiSession>>,
        log: Arc<Mutex<Log>>,
    }
    impl MidiSession for ReplaySession {
        fn send(&mut self, bytes: &[u8]) -> Result<(), ServiceError> {
            self.log.lock().unwrap().offers += 1;
            self.inner
                .as_mut()
                .ok_or_else(|| ServiceError::new(ErrorKind::Disconnected, "Replay closed"))?
                .send(bytes)
        }
        fn normal_capture_observation(
            &mut self,
        ) -> Result<FirmwareNormalCaptureObservation, ServiceError> {
            let panic = {
                let mut log = self.log.lock().unwrap();
                log.capture_observations += 1;
                log.panic_capture_observation
            };
            assert!(
                !panic,
                "Injected genuine MIDI session capture-observation panic"
            );
            FirmwareNormalCaptureObservation::replay(
                FirmwareUsbObservation {
                    topology: "replay only".into(),
                    vendor: 1,
                    product: 2,
                    serial: "not physical".into(),
                    interface: "simulator".into(),
                    endpoint: "explicit replay".into(),
                },
                "coordinator replay owner",
            )
        }
        fn capture_write_once(&mut self, bytes: &[u8]) -> FirmwareCaptureWriteResult {
            match self.send(bytes) {
                Ok(()) => FirmwareCaptureWriteResult::Known {
                    accepted: bytes.len(),
                    error: None,
                },
                Err(e) => FirmwareCaptureWriteResult::Unknown(e),
            }
        }
        fn close_normal_capture(&mut self) -> Result<(), ServiceError> {
            self.inner.take();
            self.log.lock().unwrap().closes += 1;
            Ok(())
        }
    }
    pub(crate) fn fixture() -> (Directory, StudioApp, Arc<Mutex<Log>>) {
        let dir = Directory::new();
        let log = Arc::new(Mutex::new(Log::default()));
        let concrete = gp200_transport::MidiDeviceService::with_backend(
            Box::new(ReplayBackend {
                inner: Default::default(),
                log: log.clone(),
            }),
            gp200_transport::TransportConfig {
                minimum_request_spacing: Duration::ZERO,
                ..Default::default()
            },
            no_wake(),
        );
        let controller = AppController::with_demo(
            gp200_sim::captured_demo_snapshot(),
            AppPreferences::default(),
        )
        .unwrap();
        let mut services =
            RuntimeServices::with_midi_preferences(concrete, no_wake(), AppPreferences::default());
        services.install_library_worker(
            gp200_library::LibraryWorker::from_store(
                gp200_library::LibraryStore::open(dir.0.join("library")).unwrap(),
                no_wake(),
            )
            .unwrap(),
        );
        services.draft_recovery = Some(
            gp200_library::DraftRecoveryWorker::spawn(dir.0.join("drafts"), no_wake()).unwrap(),
        );
        (dir, StudioApp::with_services(controller, services), log)
    }
    pub(crate) fn frame(app: &mut StudioApp) {
        let (inputs, _) = app.services.pump();
        for original in app.services.take_unoffered_controls() {
            app.controller.settle_unoffered_device_control(original);
        }
        for input in inputs {
            let effects = app.controller.dispatch(input);
            app.services.enqueue(effects);
        }
        let events = app.services.pump_workflows();
        assert!(events.is_empty());
        app.poll_firmware_preparation_export();
        app.advance_draft_recovery();
        let settled = app.firmware_producers_settled();
        app.firmware_coordinator.poll(
            &mut app.controller,
            &mut app.services,
            app.recovery_ui.last_saved.as_deref(),
            settled,
        );
    }
    pub(crate) fn until(app: &mut StudioApp, test: impl Fn(&StudioApp) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(120);
        while !test(app) {
            frame(app);
            assert!(
                Instant::now() < deadline,
                "phase {:?}, records {}, blockers {:?}, diagnostics {:?}",
                app.firmware_coordinator_view().phase,
                app.firmware_coordinator_view().records,
                app.firmware_coordinator_view().blockers,
                app.firmware_coordinator_view().diagnostics
            );
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    pub(crate) fn ready(app: &mut StudioApp) {
        app.send(UiIntent::Connect(gp200_sim::simulator_port().selection));
        until(app, |a| {
            a.controller.state().connection == ConnectionState::Ready
        });
        app.send(UiIntent::RefreshDeviceGlobals);
        until(app, |a| {
            a.controller.state().device_globals.is_some() && a.services.firmware_queues_settled()
        });
        until(app, |a| {
            a.recovery_ui.last_saved.as_deref() == Some(&a.controller.recovery_session())
                && !a.recovery_ui.busy
        });
    }
    pub(crate) fn context() -> FirmwareBackupReviewContext {
        let device =
            FirmwareDeviceAssociation::named_local_record("GP-200", "explicit simulator test only")
                .unwrap();
        FirmwareBackupReviewContext {
            manual_version: Some(
                UserReportedFirmware::new(
                    "V1.6.3",
                    device.clone(),
                    FirmwareReportDate::new(2026, 10, 11).unwrap(),
                )
                .unwrap(),
            ),
            device,
            destination: FirmwareBackupDestination {
                model: "GP-200".into(),
                version: FirmwareVersion::new(1, 8, 0).unwrap(),
                image_sha256: JobSha256::of(include_bytes!(
                    "../../../../tests/fixtures/device-assets/synthetic-firmware.bin"
                ))
                .hex(),
                restoration_profile: None,
            },
            local_material: vec![],
            preservation_uncertainty: "Installed audio unavailable".into(),
            recovery_limitations: "Replay only; contingent Windows recovery has not been exercised"
                .into(),
        }
    }
}

fn prepared_live_review() -> (
    live_fixture::Directory,
    StudioApp,
    Arc<std::sync::Mutex<live_fixture::Log>>,
) {
    let (dir, mut app, log) = live_fixture::fixture();
    live_fixture::ready(&mut app);
    app.begin_firmware_preparation().unwrap();
    app.capture_firmware_backup(live_fixture::context())
        .unwrap();
    live_fixture::until(&mut app, |a| {
        matches!(
            a.firmware_coordinator_view().phase,
            Phase::ReadOnlyReview | Phase::RecoveryRequired
        )
    });
    assert_eq!(app.firmware_coordinator_view().phase, Phase::ReadOnlyReview);
    let bytes: &[u8] =
        include_bytes!("../../../../tests/fixtures/device-assets/synthetic-firmware.bin");
    let path = dir.0.join("local-image.bin");
    std::fs::write(&path, bytes).unwrap();
    app.firmware_ui.image = Some(crate::firmware_workflow::InspectedFirmware {
        path,
        review: FirmwareReview::inspect(
            RetainedJobBlob::checked(
                AssetId("explicit simulator image".into()),
                bytes.into(),
                JobSha256::of(bytes),
            )
            .unwrap(),
            None,
            &FirmwareBackupAssessment::default(),
        )
        .unwrap(),
    });
    app.prepare_local_firmware_review().unwrap();
    live_fixture::until(&mut app, |a| {
        a.firmware_coordinator_view().phase == Phase::ReadOnlyReview
    });
    assert!(app.firmware_coordinator_view().preparation.is_some());
    app.services.dialog_requests = Some(Vec::new());
    (dir, app, log)
}

#[test]
fn actual_preparation_export_rejects_old_dialog_ids_and_changed_genuine_generation() {
    let (dir, mut app, log) = prepared_live_review();
    let view = app.firmware_coordinator_view();
    let report = view.preparation.clone().unwrap();
    let before = log.lock().unwrap().offers;
    app.begin_firmware_preparation_export();
    let id = app
        .firmware_ui
        .updater
        .preparation_dialog
        .as_ref()
        .unwrap()
        .0;
    assert_eq!(
        app.services.dialog_requests.as_ref().unwrap(),
        &[crate::runtime::FilePurpose::ExportFirmwarePreparation(id)]
    );
    let stale = dir.0.join("stale.json");
    app.receive_file_selection(crate::runtime::FileSelection {
        purpose: crate::runtime::FilePurpose::ExportFirmwarePreparation(id + 10),
        path: Some(stale.clone()),
    });
    assert!(!stale.exists());
    assert!(app.firmware_ui.updater.preparation_dialog.is_some());
    let path = dir.0.join("checked-preparation.json");
    app.receive_file_selection(crate::runtime::FileSelection {
        purpose: crate::runtime::FilePurpose::ExportFirmwarePreparation(id),
        path: Some(path.clone()),
    });
    live_fixture::until(&mut app, |a| {
        a.firmware_ui.updater.preparation_export.is_none()
    });
    let actual: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(actual, serde_json::to_value(&report).unwrap());
    assert_eq!(actual["permits_flash"], false);
    assert!(!app.firmware_coordinator_view().permits_flash);
    assert_eq!(log.lock().unwrap().offers, before);
    let original_bytes = std::fs::read(&path).unwrap();
    app.begin_firmware_preparation_export();
    let id = app
        .firmware_ui
        .updater
        .preparation_dialog
        .as_ref()
        .unwrap()
        .0;
    app.receive_file_selection(crate::runtime::FileSelection {
        purpose: crate::runtime::FilePurpose::ExportFirmwarePreparation(id),
        path: Some(path.clone()),
    });
    live_fixture::until(&mut app, |a| {
        a.firmware_ui.updater.preparation_export.is_none()
    });
    assert_eq!(std::fs::read(&path).unwrap(), original_bytes);
    let failed = app
        .firmware_ui
        .updater
        .failed_preparation_export
        .as_ref()
        .unwrap();
    assert_eq!(failed.generation, view.generation);
    assert_eq!(failed.destination, path);
    assert_eq!(serde_json::to_value(&failed.report).unwrap(), actual);
    assert!(app.firmware_ui.updater.message.contains("did not complete"));
    app.begin_firmware_preparation_export();
    let id = app
        .firmware_ui
        .updater
        .preparation_dialog
        .as_ref()
        .unwrap()
        .0;
    app.return_from_firmware_preparation().unwrap();
    live_fixture::until(&mut app, |a| {
        a.firmware_coordinator_view().phase == Phase::Idle
    });
    let changed = dir.0.join("changed.json");
    app.receive_file_selection(crate::runtime::FileSelection {
        purpose: crate::runtime::FilePurpose::ExportFirmwarePreparation(id),
        path: Some(changed.clone()),
    });
    assert!(!changed.exists());
    assert!(app.firmware_ui.updater.preparation_export.is_none());
    assert!(app.firmware_ui.updater.message.contains("changed"));
}

#[test]
fn actual_close_notice_dismissal_and_bounded_held_choices_preserve_the_preparation_guard() {
    let (_dir, mut app, log) = live_fixture::fixture();
    live_fixture::ready(&mut app);
    app.begin_firmware_preparation().unwrap();
    app.selected_asset_kind = AssetKind::Firmware;
    app.send(UiIntent::Navigate(ViewId::Assets));
    let ctx = egui::Context::default();
    crate::theme::install_fonts(&ctx);
    ctx.enable_accesskit();
    let mut output = ctx.run_ui(Default::default(), |ui| {
        app.request_firmware_preview_close(ui.ctx())
    });
    assert!(output.viewport_output.values().any(|v| {
        v.commands
            .iter()
            .any(|c| matches!(c, egui::ViewportCommand::CancelClose))
    }));
    output.textures_delta.clear();
    for _ in 0..3 {
        full_frame(&mut app, &ctx, vec![]);
    }
    assert!(app.firmware_close_request_pending());
    let before = log.lock().unwrap().offers;
    click(&mut app, &ctx, "Continue waiting");
    assert!(!app.firmware_close_request_pending());
    assert!(app.firmware_close_blocked());
    assert!(app.services.firmware_paused());
    assert_eq!(log.lock().unwrap().offers, before);
    for i in 0..34 {
        app.receive_file_selection(crate::runtime::FileSelection {
            purpose: crate::runtime::FilePurpose::ImportPresets,
            path: Some(format!("held-original-{i}.prst").into()),
        });
    }
    assert_eq!(app.pending_firmware_file_choices(), 32);
    assert_eq!(app.omitted_file_selections, 2);
    full_frame(&mut app, &ctx, vec![]);
    assert!(
        app.firmware_ui
            .updater
            .widgets
            .iter()
            .any(|(name, _, enabled)| name == "Apply held file choices" && !enabled)
    );
    assert!(app.apply_pending_firmware_file_choices().is_err());
    app.return_from_firmware_preparation().unwrap();
    assert_eq!(app.pending_firmware_file_choices(), 32);
    assert!(app.apply_pending_firmware_file_choices().is_err());
    for _ in 0..3 {
        full_frame(&mut app, &ctx, vec![]);
    }
    click(&mut app, &ctx, "Dismiss held file choices");
    assert_eq!(app.pending_firmware_file_choices(), 0);
    assert_eq!(app.omitted_file_selections, 0);
}

#[test]
fn actual_same_store_restore_retry_widget_keeps_firmware_and_normal_actions_held() {
    let (dir, mut app, log) = prepared_live_review();
    app.services
        .fail_next_library_restoration_before_worker_offer(Arc::new(
            std::sync::atomic::AtomicBool::new(true),
        ));
    app.return_from_firmware_preparation().unwrap();
    live_fixture::until(&mut app, |a| {
        a.firmware_coordinator_view().can_retry_library_restore
    });
    let epoch = app.controller.state().epoch;
    let before = log.lock().unwrap().offers;
    app.send(UiIntent::Navigate(ViewId::Assets));
    app.selected_asset_kind = AssetKind::Firmware;
    app.firmware_ui.updater.page = FirmwareSubview::Update;
    let ctx = egui::Context::default();
    crate::theme::install_fonts(&ctx);
    for _ in 0..3 {
        full_frame(&mut app, &ctx, vec![]);
    }
    click(&mut app, &ctx, "Retry local library restoration");
    assert!(!app.firmware_coordinator_view().can_retry_library_restore);
    live_fixture::until(&mut app, |a| {
        a.firmware_coordinator_view().can_return_to_editing
    });
    assert!(app.services.firmware_paused());
    assert!(app.firmware_close_blocked());
    assert!(!app.firmware_coordinator_view().permits_flash);
    assert_eq!(app.controller.state().epoch, epoch);
    assert_eq!(log.lock().unwrap().offers, before);
    assert!(gp200_library::LibraryStore::open(dir.0.join("library")).is_err());
    app.return_from_firmware_preparation().unwrap();
    live_fixture::until(&mut app, |a| {
        a.firmware_coordinator_view().phase == Phase::Idle
    });
}

#[test]
#[cfg(all(
    gp200_private_evidence,
    feature = "firmware-linux-completion",
    target_os = "linux"
))]
fn actual_genuine_shaped_completion_review_is_rendered_as_bounded_facts() {
    let bytes =
        include_bytes!("../../../gp200-transport/test-fixtures/firmware/genuine-entry-review.json");
    assert_eq!(
        JobSha256::of(bytes).hex(),
        "c44ca19fa9b18380275c5c2c7bb9408e4afb6569219cbb0684ece5b446ca9651"
    );
    let review: serde_json::Value = serde_json::from_slice(bytes).unwrap();
    let rows = completion_review_summary(&review);
    assert!(!rows.is_empty());
    assert!(rows.len() < 20);
    assert!(
        rows.iter()
            .map(|(key, value)| key.len() + value.len())
            .sum::<usize>()
            < 16384
    );
    let context = egui::Context::default();
    let mut output = context.run_ui(
        egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1000.0, 800.0),
            )),
            ..Default::default()
        },
        |ui| completion_review_details(ui, &review),
    );
    assert!(!output.shapes.is_empty());
    output.textures_delta.clear();
}

#[test]
#[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
fn transfer_review_renders_its_finite_policy_without_estimated_hardware_duration() {
    fn labels(shape: &egui::epaint::Shape, values: &mut Vec<String>) {
        match shape {
            egui::epaint::Shape::Text(text) => values.push(text.galley.text().to_owned()),
            egui::epaint::Shape::Vec(shapes) => {
                for shape in shapes {
                    labels(shape, values);
                }
            }
            _ => {}
        }
    }
    for (review, expected_transfer, expected_approval) in [
        (
            serde_json::json!({"transfer_seconds":1800,"approval_wait_seconds":300}),
            "30 minutes (1800 seconds) after approval",
            "5 minutes (300 seconds) from review publication",
        ),
        (
            serde_json::json!({"transfer_seconds":900,"approval_wait_seconds":300}),
            "15 minutes (900 seconds) after approval",
            "5 minutes (300 seconds) from review publication",
        ),
        (
            serde_json::json!({"transfer_seconds":61,"approval_wait_seconds":31}),
            "61 seconds after approval",
            "31 seconds from review publication",
        ),
        (
            serde_json::json!({}),
            "Unavailable in this review",
            "Unavailable in this review",
        ),
        (
            serde_json::json!({"transfer_seconds":"1800","approval_wait_seconds":-1}),
            "Unavailable in this review",
            "Unavailable in this review",
        ),
        (
            serde_json::json!({"transfer_seconds":3601,"approval_wait_seconds":901}),
            "Unavailable in this review",
            "Unavailable in this review",
        ),
    ] {
        let (mut app, context, count) = app();
        // This descriptive render fixture cannot supply an actual phase owner.
        let mut view = query_presentation_view();
        view.phase = gp200_transport::LinuxCompletionPhase::ReviewTransfer;
        view.review = Some(review);
        app.firmware_ui.updater.completion_confirmation = true;
        let coordinator = app.firmware_coordinator_view();
        let mut output = context.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1000.0, 1800.0),
                )),
                ..Default::default()
            },
            |ui| {
                app.firmware_linux_live_progress(ui, view.clone(), &coordinator, false, true);
            },
        );
        let mut text = Vec::new();
        for shape in &output.shapes {
            labels(&shape.shape, &mut text);
        }
        assert!(text.iter().any(|value| value == "Transfer timeout"));
        assert!(text.iter().any(|value| value == expected_transfer));
        assert!(text.iter().any(|value| value == "Approval window"));
        assert!(text.iter().any(|value| value == expected_approval));
        assert!(
            text.iter()
                .any(|value| value.contains("Complete hardware duration is unmeasured."))
        );
        if expected_transfer == "Unavailable in this review" {
            assert_eq!(
                text.iter()
                    .filter(|value| *value == "Unavailable in this review")
                    .count(),
                2
            );
            assert!(!text.iter().any(|value| value.contains("after approval")
                || value.contains("from review publication")));
        }
        assert!(
            app.firmware_ui
                .updater
                .widgets
                .iter()
                .any(|(name, _, enabled)| name == "Transfer reviewed firmware" && !enabled)
        );
        assert_eq!(count.load(Ordering::SeqCst), 0);
        output.textures_delta.clear();
    }
}

#[test]
#[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
fn completion_render_projection_cannot_enable_an_actual_phase_action_in_dark_or_light_mode() {
    use gp200_transport::{LinuxCompletionOutcome, LinuxCompletionPhase, LinuxCompletionView};
    for dark in [false, true] {
        for (phase, label) in [
            (
                LinuxCompletionPhase::ReviewBootQuery,
                "Query update receiver",
            ),
            (
                LinuxCompletionPhase::ReviewTransfer,
                "Transfer reviewed firmware",
            ),
            (
                LinuxCompletionPhase::ConfirmApplicationVersion,
                "Confirm displayed version",
            ),
        ] {
            let (mut app, context, count) = app();
            context.set_visuals(if dark {
                egui::Visuals::dark()
            } else {
                egui::Visuals::light()
            });
            app.firmware_ui.updater.page = FirmwareSubview::Update;
            let view = LinuxCompletionView {
                phase,
                run: "descriptive renderer fixture only".into(),
                review_digest: Some(JobSha256::of(b"descriptive review").hex()),
                review: Some(
                    serde_json::json!({"actual_chunk":255,"image_sha256":"descriptive image only"}),
                ),
                os_offered_bytes: 0,
                host_write_invocations: 0,
                host_accepted_blocks: 0,
                total_blocks: Some(25307),
                device_verified_bytes: 0,
                host_finished_unverified: false,
                normal_mode_verified: false,
                target_version_verified: false,
                contents_verified: false,
                quarantined: true,
                diagnostic: None,
                retained_raw_chunks: vec![],
                allocated_raw_frontier: 0,
                retained_block_outcome: None,
                reader_retirement_diagnostics: vec![],
            };
            app.firmware_coordinator
                .test_completion_projection(LinuxCompletionOutcome {
                    view,
                    selection: None,
                    original_normal_epoch: SessionEpoch(0),
                    verified_normal_epoch: None,
                    application_version: None,
                    journal_path: String::new(),
                    editor_return_allowed: false,
                });
            render(&mut app, &context, egui::vec2(1000.0, 1800.0), vec![]);
            app.firmware_ui.updater.completion_confirmation = true;
            app.firmware_ui.updater.completion_display_version = "V1.8.2".into();
            app.firmware_ui.updater.completion_version_read_now = true;
            render(&mut app, &context, egui::vec2(1000.0, 1800.0), vec![]);
            let widget = app
                .firmware_ui
                .updater
                .widgets
                .iter()
                .find(|(name, _, _)| name == label)
                .unwrap_or_else(|| panic!("missing actual widget {label}"));
            assert!(
                !widget.2,
                "a descriptive view cannot stand in for the live owner phase"
            );
            assert_eq!(count.load(Ordering::SeqCst), 0);
            assert!(
                app.approve_linux_completion_phase(
                    phase,
                    JobSha256::of(b"descriptive review").hex(),
                    Some("V1.8.2".into())
                )
                .is_err()
            );
            assert_eq!(count.load(Ordering::SeqCst), 0);
        }
    }
}

#[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
fn query_presentation_view() -> gp200_transport::LinuxCompletionView {
    gp200_transport::LinuxCompletionView {
        phase: gp200_transport::LinuxCompletionPhase::ReviewBootQuery,
        run: "descriptive prior observation for the unavailable-read regression".into(),
        review_digest: Some(JobSha256::of(b"one presentation review").hex()),
        review: Some(
            serde_json::json!({"candidate":{"binding":{"topology":"/fixture/usb/1-2","endpoint":"fixture ALSA endpoint"}}}),
        ),
        os_offered_bytes: 0,
        host_write_invocations: 0,
        host_accepted_blocks: 0,
        total_blocks: Some(25307),
        device_verified_bytes: 0,
        host_finished_unverified: false,
        normal_mode_verified: false,
        target_version_verified: false,
        contents_verified: false,
        quarantined: true,
        diagnostic: None,
        retained_raw_chunks: vec![],
        allocated_raw_frontier: 0,
        retained_block_outcome: None,
        reader_retirement_diagnostics: vec![],
    }
}

/// Seeds a descriptive prior UI observation for the identity of a real default
/// unreadable monitor. It does not attach a transport owner or simulate a
/// captured mutex-contention event. Actual frames use monitor.view() == None.
#[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
fn seed_unavailable_completion_presentation(
    app: &mut StudioApp,
    directory: std::path::PathBuf,
) -> crate::firmware_update_coordinator::CompletionPresentationOwner {
    app.firmware_coordinator
        .test_unreadable_completion_monitor(directory);
    let (owner, observed) = app.firmware_coordinator.linux_completion_presentation();
    assert!(observed.is_none());
    let owner = owner.unwrap();
    let now = std::time::Instant::now();
    app.firmware_ui.updater.completion_presentation_now = Some(now);
    app.firmware_ui.updater.project_completion(
        Some(owner.clone()),
        Some(query_presentation_view()),
        None,
        now,
    );
    owner
}

#[test]
#[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
fn unavailable_live_observation_preserves_clicked_checkbox_scroll_and_button_geometry() {
    let (mut app, context, count) = app();
    let size = egui::vec2(640.0, 480.0);
    // Warm actual font/layout resources before the one-second observation TTL.
    render(&mut app, &context, size, vec![]);
    app.firmware_ui.updater.page = FirmwareSubview::Update;
    let owner = seed_unavailable_completion_presentation(&mut app, "presentation-only-run".into());
    for _ in 0..2 {
        render(&mut app, &context, size, vec![]);
    }
    let inner = app.firmware_ui.updater.detail_scroll.unwrap().3;
    let confirmation_rect = app
        .firmware_ui
        .updater
        .widgets
        .iter()
        .find(|(name, _, _)| name == "Approve update-receiver identity query")
        .unwrap()
        .1;
    let scroll = (confirmation_rect.center().y - inner.center().y).max(80.0);
    render(
        &mut app,
        &context,
        size,
        vec![
            egui::Event::PointerMoved(inner.center()),
            egui::Event::MouseWheel {
                phase: egui::TouchPhase::Start,
                unit: egui::MouseWheelUnit::Point,
                delta: egui::Vec2::ZERO,
                modifiers: egui::Modifiers::NONE,
            },
            egui::Event::MouseWheel {
                phase: egui::TouchPhase::Move,
                unit: egui::MouseWheelUnit::Point,
                delta: egui::vec2(0.0, -scroll),
                modifiers: egui::Modifiers::NONE,
            },
        ],
    );
    render(
        &mut app,
        &context,
        size,
        vec![egui::Event::MouseWheel {
            phase: egui::TouchPhase::End,
            unit: egui::MouseWheelUnit::Point,
            delta: egui::Vec2::ZERO,
            modifiers: egui::Modifiers::NONE,
        }],
    );
    let (_, _, offset, inner) = app.firmware_ui.updater.detail_scroll.unwrap();
    assert!(offset.y > 0.0, "the real scroll area must have moved");
    let point = app
        .firmware_ui
        .updater
        .widgets
        .iter()
        .find(|(name, _, _)| name == "Approve update-receiver identity query")
        .unwrap()
        .1
        .center();
    assert!(
        inner.contains(point),
        "checkbox must be visible before clicking it"
    );
    for pressed in [true, false] {
        render(
            &mut app,
            &context,
            size,
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
    assert!(
        app.firmware_ui.updater.completion_confirmation,
        "real checkbox input must be retained"
    );
    let before = app.firmware_ui.updater.detail_scroll.unwrap().2;
    let button = app
        .firmware_ui
        .updater
        .widgets
        .iter()
        .find(|(name, _, _)| name == "Query update receiver")
        .unwrap()
        .1;
    render(&mut app, &context, size, vec![]);
    assert!(app.firmware_ui.updater.completion_confirmation);
    assert_eq!(app.firmware_ui.updater.detail_scroll.unwrap().2, before);
    let after = app
        .firmware_ui
        .updater
        .widgets
        .iter()
        .find(|(name, _, _)| name == "Query update receiver")
        .unwrap();
    assert_eq!(after.1, button);
    assert!(!after.2, "cached presentation cannot enable a query");
    // An unchanged subsequent observation preserves the local statement too.
    app.firmware_ui.updater.project_completion(
        Some(owner),
        Some(query_presentation_view()),
        None,
        std::time::Instant::now(),
    );
    render(&mut app, &context, size, vec![]);
    assert!(app.firmware_ui.updater.completion_confirmation);
    assert_eq!(app.firmware_ui.updater.detail_scroll.unwrap().2, before);
    assert_eq!(
        app.firmware_ui
            .updater
            .widgets
            .iter()
            .find(|(name, _, _)| name == "Query update receiver")
            .unwrap()
            .1,
        button
    );
    assert_eq!(count.load(Ordering::SeqCst), 0);
}

#[test]
#[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
fn completion_statements_clear_for_each_owner_review_and_age_boundary() {
    let (mut app, _, count) = app();
    let owner = seed_unavailable_completion_presentation(&mut app, "first-presentation-run".into());
    let now = std::time::Instant::now();
    for boundary in [
        "monitor",
        "generation",
        "directory",
        "run",
        "phase",
        "digest",
        "outcome",
        "absent",
        "expired",
    ] {
        let ui = &mut app.firmware_ui.updater;
        ui.project_completion(
            Some(owner.clone()),
            Some(query_presentation_view()),
            None,
            now,
        );
        ui.completion_confirmation = true;
        ui.completion_display_version = "V1.8.2".into();
        ui.completion_version_read_now = true;
        let mut changed_owner = owner.clone();
        let mut changed_view = query_presentation_view();
        match boundary {
            "monitor" => {
                app.firmware_coordinator
                    .test_unreadable_completion_monitor("first-presentation-run".into());
                changed_owner = app
                    .firmware_coordinator
                    .linux_completion_presentation()
                    .0
                    .unwrap();
            }
            "generation" => changed_owner.generation += 1,
            "directory" => changed_owner.run_directory = "second-presentation-run".into(),
            "run" => changed_view.run = "another attempt with the same digest".into(),
            "phase" => changed_view.phase = gp200_transport::LinuxCompletionPhase::ReviewTransfer,
            "digest" => changed_view.review_digest = Some(JobSha256::of(b"another review").hex()),
            _ => {}
        }
        let ui = &mut app.firmware_ui.updater;
        match boundary {
            "outcome" => {
                changed_view.phase = gp200_transport::LinuxCompletionPhase::RecoveryRequired;
                ui.project_completion(None, Some(changed_view), None, now);
            }
            "absent" => ui.project_completion(None, None, None, now),
            "expired" => ui.project_completion(
                Some(changed_owner),
                None,
                None,
                now + COMPLETION_PRESENTATION_TTL,
            ),
            _ => ui.project_completion(Some(changed_owner), Some(changed_view), None, now),
        }
        assert!(
            !ui.completion_confirmation,
            "query/transfer statement survived {boundary}"
        );
        assert!(
            ui.completion_display_version.is_empty(),
            "version text survived {boundary}"
        );
        assert!(
            !ui.completion_version_read_now,
            "fresh-display statement survived {boundary}"
        );
    }
    assert_eq!(count.load(Ordering::SeqCst), 0);
}

#[test]
#[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
fn cached_phase_buttons_and_real_approval_reject_an_unreadable_monitor_without_signal_files() {
    use gp200_transport::LinuxCompletionPhase;
    let (mut app, context, count) = app();
    let directory = std::env::temp_dir().join(format!(
        "gp200-unavailable-presentation-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir(&directory).unwrap();
    render(&mut app, &context, egui::vec2(1000.0, 1800.0), vec![]);
    let owner = seed_unavailable_completion_presentation(&mut app, directory.clone());
    app.firmware_ui.updater.page = FirmwareSubview::Update;
    for (phase, label) in [
        (
            LinuxCompletionPhase::ReviewBootQuery,
            "Query update receiver",
        ),
        (
            LinuxCompletionPhase::ReviewTransfer,
            "Transfer reviewed firmware",
        ),
        (
            LinuxCompletionPhase::ConfirmApplicationVersion,
            "Confirm displayed version",
        ),
    ] {
        let mut view = query_presentation_view();
        view.phase = phase;
        let digest = view.review_digest.clone().unwrap();
        app.firmware_ui.updater.project_completion(
            Some(owner.clone()),
            Some(view),
            None,
            std::time::Instant::now(),
        );
        app.firmware_ui.updater.completion_confirmation = true;
        app.firmware_ui.updater.completion_display_version = "V1.8.2".into();
        app.firmware_ui.updater.completion_version_read_now = true;
        render(&mut app, &context, egui::vec2(1000.0, 1800.0), vec![]);
        assert!(
            !app.firmware_ui
                .updater
                .widgets
                .iter()
                .find(|(name, _, _)| name == label)
                .unwrap()
                .2
        );
        let error = app
            .approve_linux_completion_phase(phase, digest, Some("V1.8.2".into()))
            .unwrap_err();
        assert_eq!(error.message, "Actual completion owner absent");
        assert!(!app.firmware_coordinator.linux_completion_helper_pending());
        assert_eq!(
            std::fs::read_dir(&directory).unwrap().count(),
            0,
            "no exact-phase signal may be created from a cached projection"
        );
        assert_eq!(count.load(Ordering::SeqCst), 0);
    }
    std::fs::remove_dir(directory).unwrap();
}

#[test]
#[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
fn update_history_navigation_keeps_the_same_live_presentation_confirmation() {
    let (mut app, context, count) = app();
    let size = egui::vec2(1000.0, 1800.0);
    render(&mut app, &context, size, vec![]);
    seed_unavailable_completion_presentation(&mut app, "navigation-presentation-run".into());
    app.firmware_ui.updater.page = FirmwareSubview::Update;
    app.firmware_ui.updater.completion_confirmation = true;
    for label in ["History & recovery", "Update"] {
        render(&mut app, &context, size, vec![]);
        let point = app
            .firmware_ui
            .updater
            .widgets
            .iter()
            .find(|(name, _, _)| name == label)
            .unwrap()
            .1
            .center();
        for pressed in [true, false] {
            render(
                &mut app,
                &context,
                size,
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
        assert_eq!(
            app.firmware_ui.updater.page,
            if label == "Update" {
                FirmwareSubview::Update
            } else {
                FirmwareSubview::History
            }
        );
        assert!(app.firmware_ui.updater.completion_confirmation);
        assert!(app.firmware_ui.updater.completion_presentation.is_some());
        assert!(
            !app.firmware_ui
                .updater
                .widgets
                .iter()
                .find(|(name, _, _)| name == "Query update receiver")
                .unwrap()
                .2
        );
    }
    assert_eq!(count.load(Ordering::SeqCst), 0);
}

#[test]
#[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
fn completion_cache_expires_off_page_and_a_returned_outcome_projection_clears_statements() {
    use gp200_transport::{LinuxCompletionOutcome, LinuxCompletionPhase};
    let (mut app, context, count) = app();
    let size = egui::vec2(1000.0, 1800.0);
    render(&mut app, &context, size, vec![]);
    let owner =
        seed_unavailable_completion_presentation(&mut app, "expiry-presentation-run".into());
    let now = std::time::Instant::now();
    app.firmware_ui.updater.completion_presentation_now = Some(now);
    let old = now - COMPLETION_PRESENTATION_TTL;
    app.firmware_ui.updater.project_completion(
        Some(owner),
        Some(query_presentation_view()),
        None,
        old,
    );
    app.firmware_ui.updater.completion_confirmation = true;
    app.firmware_ui.updater.completion_display_version = "V1.8.2".into();
    app.firmware_ui.updater.completion_version_read_now = true;
    app.firmware_ui.updater.page = FirmwareSubview::FileReview;
    render(&mut app, &context, size, vec![]);
    assert!(app.firmware_ui.updater.completion_presentation.is_none());
    assert!(!app.firmware_ui.updater.completion_confirmation);
    assert!(
        app.firmware_ui
            .updater
            .completion_display_version
            .is_empty()
    );
    assert!(!app.firmware_ui.updater.completion_version_read_now);
    seed_unavailable_completion_presentation(&mut app, "returned-presentation-run".into());
    app.firmware_ui.updater.completion_confirmation = true;
    app.firmware_ui.updater.completion_display_version = "V1.8.2".into();
    app.firmware_ui.updater.completion_version_read_now = true;
    let mut returned = query_presentation_view();
    returned.phase = LinuxCompletionPhase::RecoveryRequired;
    app.firmware_coordinator
        .test_completion_projection(LinuxCompletionOutcome {
            view: returned,
            selection: None,
            original_normal_epoch: SessionEpoch(0),
            verified_normal_epoch: None,
            application_version: None,
            journal_path: "descriptive returned outcome".into(),
            editor_return_allowed: false,
        });
    app.firmware_ui.updater.page = FirmwareSubview::Update;
    render(&mut app, &context, size, vec![]);
    assert!(app.firmware_ui.updater.completion_cache.is_none());
    assert_eq!(
        app.firmware_ui
            .updater
            .completion_presentation
            .as_ref()
            .unwrap()
            .view
            .phase,
        LinuxCompletionPhase::RecoveryRequired
    );
    assert!(!app.firmware_ui.updater.completion_confirmation);
    assert!(
        app.firmware_ui
            .updater
            .completion_display_version
            .is_empty()
    );
    assert!(!app.firmware_ui.updater.completion_version_read_now);
    assert_eq!(count.load(Ordering::SeqCst), 0);
}

#[test]
fn firmware_prepare_can_read_missing_globals_before_any_editor_freeze() {
    let (_dir, mut app, log) = live_fixture::fixture();
    app.send(UiIntent::Connect(gp200_sim::simulator_port().selection));
    live_fixture::until(&mut app, |app| {
        app.controller.state().connection == ConnectionState::Ready
            && app.services.firmware_queues_settled()
    });
    assert!(app.controller.state().device_globals.is_none());
    app.firmware_ui.updater.page = FirmwareSubview::Prepare;
    let context = egui::Context::default();
    crate::theme::install_fonts(&context);
    let size = egui::vec2(1280.0, 1600.0);
    for _ in 0..2 {
        render(&mut app, &context, size, vec![]);
    }
    let (_, rect, enabled) = app
        .firmware_ui
        .updater
        .widgets
        .iter()
        .find(|(name, _, _)| name == "Read global settings")
        .expect("Prepare exposes the normal read action");
    assert!(*enabled);
    let point = rect.center();
    let offers = log.lock().unwrap().offers;
    assert!(
        app.firmware_ui
            .updater
            .widgets
            .iter()
            .any(|(name, _, enabled)| name == "Begin preparation" && !enabled)
    );
    for pressed in [true, false] {
        render(
            &mut app,
            &context,
            size,
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
    assert!(app.controller.device_operation_pending());
    render(&mut app, &context, size, vec![]);
    assert!(
        app.firmware_ui
            .updater
            .widgets
            .iter()
            .any(|(name, _, enabled)| name == "Read global settings" && !enabled)
    );
    live_fixture::until(&mut app, |app| {
        app.controller
            .state()
            .device_globals
            .as_ref()
            .is_some_and(|globals| globals.epoch == app.controller.state().epoch)
            && app.services.firmware_queues_settled()
    });
    assert_eq!(app.firmware_coordinator_view().phase, Phase::Idle);
    assert!(!app.services.firmware_paused());
    assert!(!app.controller.firmware_editor_blocks_normal_actions());
    assert!(app.services.library.is_some());
    assert!(log.lock().unwrap().offers > offers);
}

#[test]
#[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
fn recovery_history_button_blocks_previous_documents_and_exposes_the_actual_offline_review() {
    let (directory, mut app, log) = live_fixture::fixture();
    let previous_root = directory.0.join("previous-drafts");
    let mut previous = app.controller.recovery_session();
    let mut second = previous.documents[0].clone();
    second.original_id = DocumentId("second-ui-previous-document".into());
    previous.documents.push(second);
    let retained = gp200_library::DraftRecoveryStore::open(&previous_root)
        .unwrap()
        .save(&previous)
        .unwrap();
    let previous_bytes = std::fs::read(previous_root.join("session.gp200recovery")).unwrap();
    app.services.draft_recovery =
        Some(gp200_library::DraftRecoveryWorker::spawn(previous_root.clone(), no_wake()).unwrap());
    app.recovery_ui = Default::default();
    live_fixture::until(&mut app, |app| app.recovery_ui.found.is_some());
    assert_eq!(
        app.recovery_ui.found.as_ref().unwrap().sha256,
        retained.sha256
    );
    // An inspected synthetic image enables all the ordinary UI image checks.
    // It confers no physical authority and is rejected if actually submitted.
    let bytes: &[u8] =
        include_bytes!("../../../../tests/fixtures/device-assets/synthetic-firmware.bin");
    app.firmware_ui.image = Some(crate::firmware_workflow::InspectedFirmware {
        path: directory.0.join("inert-synthetic-image.bin"),
        review: FirmwareReview::inspect(
            RetainedJobBlob::checked(
                AssetId("inert recovery UI fixture".into()),
                bytes.into(),
                JobSha256::of(bytes),
            )
            .unwrap(),
            None,
            &FirmwareBackupAssessment::default(),
        )
        .unwrap(),
    });
    app.firmware_coordinator
        .test_linux_history_registry(directory.0.join("isolated-empty-registry"));
    app.firmware_ui.updater.page = FirmwareSubview::History;
    let context = egui::Context::default();
    crate::theme::install_fonts(&context);
    context.enable_accesskit();
    let size = egui::vec2(1280.0, 2000.0);
    render(&mut app, &context, size, vec![]);
    live_fixture::until(&mut app, |app| {
        !app.firmware_coordinator.linux_history_status().0
    });
    let mut nodes = std::collections::BTreeMap::new();
    for _ in 0..2 {
        if let Some(update) = render(&mut app, &context, size, vec![])
            .platform_output
            .accesskit_update
        {
            nodes.extend(update.nodes);
        }
    }
    assert!(
        nodes
            .values()
            .any(|node| accessible_name(&nodes, node).contains("Recover all offline"))
    );
    let (_, rect, enabled) = app
        .firmware_ui
        .updater
        .widgets
        .iter()
        .find(|(name, _, _)| name == "Review a new full-image recovery")
        .unwrap();
    assert!(!enabled);
    let point = rect.center();
    for pressed in [true, false] {
        render(
            &mut app,
            &context,
            size,
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
    assert!(!app.services.firmware_paused());
    assert!(!app.firmware_close_blocked());
    assert_eq!(
        std::fs::read(previous_root.join("session.gp200recovery")).unwrap(),
        previous_bytes
    );
    let (_, rect, enabled) = app
        .firmware_ui
        .updater
        .widgets
        .iter()
        .find(|(name, _, _)| name == "Review previous offline documents")
        .unwrap();
    assert!(*enabled);
    let point = rect.center();
    for pressed in [true, false] {
        render(
            &mut app,
            &context,
            size,
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
    assert!(app.recovery_ui.review_open);
    assert_eq!(app.controller.state().documents.len(), 1);
    app.recover_previous_offline();
    render(&mut app, &context, size, vec![]);
    assert!(
        app.firmware_ui
            .updater
            .widgets
            .iter()
            .any(|(name, _, enabled)| name == "Review a new full-image recovery" && !enabled)
    );
    live_fixture::until(&mut app, |app| {
        !app.recovery_ui.busy
            && app.recovery_ui.last_saved.as_deref() == Some(&app.controller.recovery_session())
    });
    render(&mut app, &context, size, vec![]);
    assert!(
        app.firmware_ui
            .updater
            .widgets
            .iter()
            .any(|(name, _, enabled)| name == "Review a new full-image recovery" && *enabled)
    );
    assert_eq!(app.controller.state().documents.len(), 3);
    assert!(app.controller.state().identity.is_none());
    assert!(app.services.library.is_some());
    assert!(!app.services.firmware_paused());
    assert_eq!(log.lock().unwrap().offers, 0);
    assert_eq!(log.lock().unwrap().opens, 0);
}

#[test]
#[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
fn actual_egui_settled_recovery_close_button_exits_only_the_host_and_keeps_normal_admission_held() {
    let (directory, mut app, log) = live_fixture::fixture();
    // Actual simulator ticket/original-store return and restoration; only the
    // returned LinuxRecoveryOutcome DTO is substituted by the cfg(test) seam.
    crate::firmware_update_coordinator::test_prepare_settled_recovery_host_close(
        &mut app,
        &directory.0,
    );
    app.selected_asset_kind = AssetKind::Firmware;
    app.send(UiIntent::Navigate(ViewId::Assets));
    let ctx = egui::Context::default();
    crate::theme::install_fonts(&ctx);
    ctx.enable_accesskit();
    let before = serde_json::to_vec(&app.linux_recovery_outcome()).unwrap();
    let mut ordinary = ctx.run_ui(Default::default(), |ui| {
        app.request_firmware_preview_close(ui.ctx())
    });
    ordinary.textures_delta.clear();
    assert!(ordinary.viewport_output.values().any(|viewport| {
        viewport
            .commands
            .iter()
            .any(|command| matches!(command, egui::ViewportCommand::CancelClose))
    }));
    assert!(app.firmware_close_request_pending());
    for _ in 0..3 {
        full_frame(&mut app, &ctx, vec![]);
    }
    let label = "Close app and keep recovery required";
    let (_, rect, enabled) = app
        .firmware_ui
        .updater
        .widgets
        .iter()
        .find(|(name, _, _)| name == label)
        .unwrap();
    assert!(*enabled);
    let point = rect.center();
    let mut close_emitted = false;
    for pressed in [true, false] {
        let mut output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(640.0, 480.0),
                )),
                events: vec![
                    egui::Event::PointerMoved(point),
                    egui::Event::PointerButton {
                        pos: point,
                        button: egui::PointerButton::Primary,
                        pressed,
                        modifiers: egui::Modifiers::NONE,
                    },
                ],
                ..Default::default()
            },
            |ui| app.render(ui),
        );
        output.textures_delta.clear();
        close_emitted |= output.viewport_output.values().any(|viewport| {
            viewport
                .commands
                .iter()
                .any(|command| matches!(command, egui::ViewportCommand::Close))
        });
    }
    assert!(close_emitted);
    assert!(!app.firmware_close_request_pending());
    assert!(app.firmware_close_blocked());
    assert!(
        app.services.firmware_paused() && app.controller.firmware_editor_blocks_normal_actions()
    );
    assert_eq!(
        app.firmware_coordinator_view().phase,
        Phase::RecoveryRequired
    );
    assert_eq!(
        serde_json::to_vec(&app.linux_recovery_outcome()).unwrap(),
        before
    );
    assert!(app.return_from_firmware_preparation().is_err());
    assert!(gp200_library::LibraryStore::open(directory.0.join("library")).is_err());
    assert_eq!(log.lock().unwrap().opens, 0);
    assert_eq!(log.lock().unwrap().offers, 0);
}
