use super::firmware_updater_tests::{app, render};
use eframe::egui;
#[test]
fn interrupted_history_scene_keeps_recorded_and_derived_status_separate() {
    let (mut app, context, _) = app();
    app.open_firmware_updater_preview("finished-corrupt-tail")
        .unwrap();
    let output = render(&mut app, &context, egui::vec2(640.0, 480.0), vec![]);
    assert!(!output.shapes.is_empty());
    let preview = app.firmware_ui.updater.preview.as_ref().unwrap();
    assert!(
        preview
            .rows
            .iter()
            .any(|(label, value)| label == "Recorded status" && value.contains("Host finish"))
    );
    assert!(
        preview
            .rows
            .iter()
            .any(|(label, value)| label == "Derived reopen state" && value.contains("Interrupted"))
    );
    assert!(!app.firmware_close_blocked());
}

#[test]
fn actual_attempt_dialog_rejects_changed_selection_and_a0_rechecks_original_bytes() {
    use super::firmware_updater_tests::live_fixture;
    use crate::runtime::{FilePurpose, FileSelection};
    use gp200_domain::FirmwareAttemptId;
    let (dir, mut app, _log) = live_fixture::fixture();
    // Deliberately malformed retained original journals. They are evidence to
    // inspect/export, never updater owners, authority or completion claims.
    let ledger = dir.0.join("library/firmware-attempt-ledger");
    std::fs::create_dir_all(&ledger).unwrap();
    let first = FirmwareAttemptId::parse("11111111111111111111111111111111").unwrap();
    let second = FirmwareAttemptId::parse("22222222222222222222222222222222").unwrap();
    for attempt in [&first, &second] {
        std::fs::write(
            ledger.join(format!("{}.reserved", attempt.as_str())),
            format!("gp200-firmware-local-attempt-v1:{}\n", attempt.as_str()),
        )
        .unwrap();
        std::fs::write(
            ledger.join(format!("{}.journal", attempt.as_str())),
            b"retained malformed original fixture",
        )
        .unwrap();
    }
    app.services.dialog_requests = Some(Vec::new());
    app.inspect_firmware_attempt(first.clone()).unwrap();
    live_fixture::until(&mut app, |a| a.firmware_coordinator_view().detail.is_some());
    let detail = app.firmware_coordinator_view().detail.unwrap();
    assert_eq!(detail.attempt, first);
    app.begin_firmware_attempt_export(&detail);
    let id = app.firmware_ui.updater.attempt_dialog.as_ref().unwrap().0;
    assert_eq!(
        app.services.dialog_requests.as_ref().unwrap(),
        &[FilePurpose::ExportFirmwareAttempt(id)]
    );
    let stale = dir.0.join("old-dialog.zip");
    app.receive_file_selection(FileSelection {
        purpose: FilePurpose::ExportFirmwareAttempt(id + 1),
        path: Some(stale.clone()),
    });
    assert!(!stale.exists());
    assert!(app.firmware_ui.updater.attempt_dialog.is_some());
    app.inspect_firmware_attempt(second.clone()).unwrap();
    live_fixture::until(&mut app, |a| {
        a.firmware_coordinator_view()
            .detail
            .as_ref()
            .is_some_and(|d| d.attempt == second)
    });
    let changed = dir.0.join("changed-selection.zip");
    app.receive_file_selection(FileSelection {
        purpose: FilePurpose::ExportFirmwareAttempt(id),
        path: Some(changed.clone()),
    });
    assert!(!changed.exists());
    assert!(app.firmware_ui.updater.message.contains("changed"));
    let detail = app.firmware_coordinator_view().detail.unwrap();
    app.begin_firmware_attempt_export(&detail);
    let id = app.firmware_ui.updater.attempt_dialog.as_ref().unwrap().0;
    let checked = dir.0.join("original-checked.zip");
    app.receive_file_selection(FileSelection {
        purpose: FilePurpose::ExportFirmwareAttempt(id),
        path: Some(checked.clone()),
    });
    live_fixture::until(&mut app, |a| {
        a.firmware_coordinator_view().exported.is_some()
    });
    assert!(checked.exists());
    assert_eq!(
        app.firmware_coordinator_view().exported.unwrap().attempt,
        second
    );
    app.begin_firmware_attempt_export(&detail);
    let id = app.firmware_ui.updater.attempt_dialog.as_ref().unwrap().0;
    std::fs::write(
        ledger.join(format!("{}.journal", second.as_str())),
        b"changed original bytes",
    )
    .unwrap();
    let changed_source = dir.0.join("changed-source.zip");
    app.receive_file_selection(FileSelection {
        purpose: FilePurpose::ExportFirmwareAttempt(id),
        path: Some(changed_source.clone()),
    });
    live_fixture::until(&mut app, |a| {
        a.firmware_coordinator_view()
            .diagnostics
            .iter()
            .any(|m| m.contains("changed") || m.contains("differs"))
    });
    assert!(!changed_source.exists());
    assert!(!app.firmware_coordinator_view().permits_flash);
}

#[test]
#[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
fn disconnected_history_review_action_is_visible_and_fits_dark_and_light_minimum_window() {
    use super::firmware_updater::FirmwareSubview;
    use gp200_domain::*;
    for theme in [ThemePreference::Dark, ThemePreference::Light] {
        let (mut app, context, count) = app();
        crate::theme::apply(&context, theme, 1.0);
        app.firmware_ui.updater.page = FirmwareSubview::History;
        assert!(app.controller.state().identity.is_none());
        for _ in 0..2 {
            render(&mut app, &context, egui::vec2(640.0, 480.0), vec![]);
        }
        let action = app
            .firmware_ui
            .updater
            .widgets
            .iter()
            .find(|(name, _, _)| name == "Review a new full-image recovery")
            .unwrap();
        assert!(
            !action.2,
            "an absent concrete owner or image must keep the action disabled"
        );
        assert!(action.1.min.x >= 0.0 && action.1.max.x <= 640.0);
        assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert!(app.review_linux_recovery().is_err());
    }
}

#[test]
#[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
fn history_live_review_descriptions_cannot_enable_query_transfer_or_manual_return() {
    use super::firmware_updater::FirmwareSubview;
    use gp200_domain::*;
    use gp200_transport::*;
    for theme in [ThemePreference::Dark, ThemePreference::Light] {
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
            crate::theme::apply(&context, theme, 1.0);
            app.firmware_ui.updater.page = FirmwareSubview::History;
            let view = LinuxCompletionView {
                phase,
                run: "descriptive-history-only".into(),
                review_digest: Some(JobSha256::of(b"history-description").hex()),
                review: Some(serde_json::json!({"purpose":"fresh-full-image-recovery-v1",
                    "historical_capture":{"records":vec![serde_json::json!({"raw":"not rendered"});561]},
                    "candidate":{"binding":{"topology":"test-description", "endpoint":"hw:999"}},
                    "history":{"selected":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                        "starts_at_section":0,"starts_at_block":0,"predecessors":[]}})),
                os_offered_bytes: 0,
                host_write_invocations: 0,
                host_accepted_blocks: 0,
                total_blocks: None,
                device_verified_bytes: 0,
                host_finished_unverified: false,
                normal_mode_verified: false,
                target_version_verified: false,
                contents_verified: false,
                quarantined: true,
                diagnostic: Some("Presentation description; no owning receiver".into()),
                retained_raw_chunks: vec![],
                allocated_raw_frontier: 0,
                retained_block_outcome: None,
                reader_retirement_diagnostics: vec![],
            };
            app.firmware_coordinator
                .test_recovery_projection(view, true);
            render(&mut app, &context, egui::vec2(640.0, 480.0), vec![]);
            app.firmware_ui.updater.completion_confirmation = true;
            app.firmware_ui.updater.completion_version_read_now = true;
            app.firmware_ui.updater.completion_display_version = "V1.8.2".into();
            render(&mut app, &context, egui::vec2(640.0, 480.0), vec![]);
            let action = app
                .firmware_ui
                .updater
                .widgets
                .iter()
                .find(|(name, _, _)| name == label)
                .unwrap_or_else(|| panic!("missing shared history control {label}"));
            assert!(
                !action.2,
                "a historical description must not supply a live approval"
            );
            assert!(action.1.min.x >= 0.0 && action.1.max.x <= 640.0);
            let stop = app
                .firmware_ui
                .updater
                .widgets
                .iter()
                .find(|(name, _, _)| name == "Stop recovery host")
                .unwrap();
            assert!(stop.1.min.x >= 0.0 && stop.1.max.x <= 640.0);
            assert!(
                app.approve_linux_completion_phase(
                    phase,
                    JobSha256::of(b"history-description").hex(),
                    Some("V1.8.2".into())
                )
                .is_err()
            );
            assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 0);
        }
    }
}

#[test]
#[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
fn retained_completed_recovery_stays_visible_after_editor_release_without_stop_action() {
    use super::firmware_updater::FirmwareSubview;
    use gp200_domain::*;
    use gp200_transport::*;
    for theme in [ThemePreference::Dark, ThemePreference::Light] {
        let (mut app, context, count) = app();
        crate::theme::apply(&context, theme, 1.0);
        app.firmware_ui.updater.page = FirmwareSubview::History;
        app.firmware_coordinator.test_recovery_projection(
            LinuxCompletionView {
                phase: LinuxCompletionPhase::ReturningEditor,
                run: "retained-result-description".into(),
                review_digest: None,
                review: None,
                os_offered_bytes: 42,
                host_write_invocations: 1,
                host_accepted_blocks: 1,
                total_blocks: Some(1),
                device_verified_bytes: 0,
                host_finished_unverified: true,
                normal_mode_verified: true,
                target_version_verified: true,
                contents_verified: false,
                quarantined: false,
                diagnostic: None,
                retained_raw_chunks: vec![],
                allocated_raw_frontier: 0,
                retained_block_outcome: None,
                reader_retirement_diagnostics: vec![],
            },
            false,
        );
        let output = render(&mut app, &context, egui::vec2(1280.0, 1600.0), vec![]);
        assert!(!output.shapes.is_empty());
        assert!(
            !app.firmware_ui
                .updater
                .widgets
                .iter()
                .any(|(name, _, _)| name == "Stop recovery host")
        );
        assert!(app.linux_completion_view().is_some());
        assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert!(!app.firmware_close_blocked());
    }
}

#[test]
#[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
fn actual_private_completed_linux_catalogue_actions_fit_minimum_dark_and_light_history() {
    use super::{firmware_updater::FirmwareSubview, firmware_updater_tests::live_fixture};
    use gp200_domain::ThemePreference;
    for theme in [ThemePreference::Dark, ThemePreference::Light] {
        let (directory, mut app, log) = live_fixture::fixture();
        let registry = directory.0.join("private-descriptive-catalogue");
        let attempts = app
            .firmware_coordinator
            .test_completed_linux_history_fixture(&registry);
        app.firmware_coordinator
            .test_linux_history_registry(registry);
        app.refresh_linux_firmware_history().unwrap();
        live_fixture::until(&mut app, |a| {
            !a.firmware_coordinator.linux_history_status().0
        });
        let context = egui::Context::default();
        crate::theme::apply(&context, theme, 1.0);
        app.firmware_ui.updater.page = FirmwareSubview::History;
        let mut output = render(&mut app, &context, egui::vec2(640.0, 480.0), vec![]);
        output.textures_delta.clear();
        assert!(!output.shapes.is_empty());
        let catalogue = app.firmware_coordinator.linux_history().unwrap();
        assert_eq!(catalogue.rows.len(), 4);
        assert!(
            catalogue
                .rows
                .iter()
                .all(|row| row.resolved && !row.physical_authority)
        );
        let refresh = app
            .firmware_ui
            .updater
            .widgets
            .iter()
            .find(|(name, _, _)| name == "Refresh Linux firmware history")
            .unwrap();
        assert!(
            refresh.2
                && refresh.1.min.x >= 0.0
                && refresh.1.max.x <= 640.0
                && refresh.1.max.y <= 480.0
        );
        assert!(
            !app.firmware_ui
                .updater
                .widgets
                .iter()
                .any(
                    |(name, _, _)| name == "Select this unresolved local attempt"
                        || name == "Stop recovery host"
                )
        );
        assert!(
            app.firmware_coordinator
                .select_linux_history(Some(attempts[2].clone()))
                .is_err()
        );
        assert!(
            app.approve_linux_completion_phase(
                gp200_transport::LinuxCompletionPhase::ReviewTransfer,
                "description-only".into(),
                None
            )
            .is_err()
        );
        assert_eq!(log.lock().unwrap().offers, 0);
        assert!(!app.firmware_close_blocked());
        assert!(gp200_library::LibraryStore::open(directory.0.join("library")).is_err());
    }
}

#[test]
#[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
fn checked_zero_call_history_title_never_claims_firmware_completion_in_dark_or_light() {
    use super::{firmware_updater::FirmwareSubview, firmware_updater_tests::live_fixture};
    use gp200_domain::ThemePreference;
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
    for theme in [ThemePreference::Dark, ThemePreference::Light] {
        let (directory, mut app, log) = live_fixture::fixture();
        let registry = directory.0.join("zero-call-display-registry");
        let attempt = app
            .firmware_coordinator
            .test_zero_call_linux_history_fixture(&registry);
        app.firmware_coordinator
            .test_linux_history_registry(registry);
        app.refresh_linux_firmware_history().unwrap();
        live_fixture::until(&mut app, |a| {
            !a.firmware_coordinator.linux_history_status().0
        });
        let catalogue = app.firmware_coordinator.linux_history().unwrap();
        assert!(catalogue.diagnostics.is_empty());
        assert_eq!(catalogue.rows.len(), 1);
        let row = &catalogue.rows[0];
        assert_eq!(row.attempt, attempt);
        assert_eq!(row.local_state, 2);
        assert!(row.resolved);
        assert_eq!(
            super::firmware_history::linux_history_state_label(row),
            "Closed without firmware writes"
        );
        let context = egui::Context::default();
        crate::theme::apply(&context, theme, 1.0);
        app.firmware_ui.updater.page = FirmwareSubview::History;
        let mut output = render(&mut app, &context, egui::vec2(640.0, 480.0), vec![]);
        output.textures_delta.clear();
        let mut text = Vec::new();
        for shape in &output.shapes {
            labels(&shape.shape, &mut text);
        }
        assert!(
            text.iter()
                .any(|text| text.contains("Closed without firmware writes")),
            "{text:?}"
        );
        assert!(!text.iter().any(|text| text.contains("Completed / closed")));
        assert!(
            app.firmware_coordinator
                .select_linux_history(Some(attempt))
                .is_err()
        );
        assert!(app.linux_recovery_outcome().is_none() && app.linux_completion_view().is_none());
        assert_eq!(log.lock().unwrap().offers, 0);
    }
}
