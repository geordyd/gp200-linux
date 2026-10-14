use crate::{StudioApp, theme::Palette};
use eframe::egui::{self, RichText};
use gp200_catalog::{ModelDefaultPlan, ModelDefinition, SlotKind};
use gp200_domain::*;

#[cfg(all(test, gp200_private_evidence))]
mod factory_amp_picker_tests {
    use super::*;
    use gp200_app::{AppController, device_document_id};
    use std::sync::Arc;

    #[test]
    fn picker_uses_the_same_exact_candidate_feature_and_global_gate() {
        let mut app = AppController::new(
            gp200_catalog::Catalog::embedded().unwrap(),
            AppPreferences::default(),
        );
        let effects = app.dispatch(AppInput::Ui(UiIntent::Connect(DeviceSelection {
            input_id: "fixture-in".into(),
            output_id: "fixture-out".into(),
            fingerprint_hint: None,
        })));
        let Effect::Device(DeviceCommand::Connect { intent, .. }) = effects[0] else {
            panic!("{effects:?}")
        };
        app.dispatch(AppInput::Device(DeviceEvent::SessionChanged {
            epoch: SessionEpoch(1),
            identity: DeviceIdentity {
                model: DeviceModel::Gp200,
                mode: BackendMode::Hardware,
                firmware: None,
                display_name: "Synthetic picker context".into(),
            },
            capabilities: gp200_transport::verified_capabilities(),
        }));
        app.dispatch(AppInput::Device(DeviceEvent::ConnectionChanged {
            epoch: SessionEpoch(1),
            state: ConnectionState::Ready,
        }));
        let raw = RawTransfer::from_payload(0x12, include_bytes!("../../../../analysis/implementation/dependent-model-normal-v3-root/amp-auto-cab-on-v3-run01/current-before.payload.bin").to_vec()).unwrap();
        app.dispatch(AppInput::Device(DeviceEvent::SnapshotReceived {
            stamp: RequestStamp {
                epoch: SessionEpoch(1),
                intent,
            },
            target: PresetTarget::EditBuffer,
            snapshot: Arc::new(PresetSnapshot::from_raw(
                Arc::new(raw),
                SnapshotSource::Hardware {
                    epoch: SessionEpoch(1),
                },
                Some(PresetTarget::EditBuffer),
                Revision(1),
            )),
        }));
        let module = ModuleId::new(3).unwrap();
        assert!(
            blocked_reason(
                &app,
                app.active_document().unwrap(),
                module,
                ModelCode(0x07000003)
            )
            .unwrap()
            .contains("globals")
        );
        let effects = app.dispatch(AppInput::Ui(UiIntent::RefreshDeviceGlobals));
        let Effect::Device(DeviceCommand::ReadGlobals(stamp)) = effects[0] else {
            panic!("{effects:?}")
        };
        let globals = RawTransfer::from_payload(0x12, include_bytes!("../../../../analysis/implementation/dependent-model-normal-v3-root/amp-auto-cab-on-v3-run01/globals-before.payload.bin").to_vec()).unwrap();
        app.dispatch(AppInput::Device(DeviceEvent::GlobalStateReceived {
            stamp,
            state: Arc::new(
                DeviceGlobalState::from_transfer(Revision(1), SessionEpoch(1), globals, vec![])
                    .unwrap(),
            ),
        }));
        let document = &app.state().documents[&device_document_id()];
        for descriptor in FACTORY_AMP_CANDIDATES {
            assert_eq!(
                blocked_reason(&app, document, module, descriptor.model).is_none(),
                cfg!(feature = "factory-amp-trials")
            );
        }
        for excluded in [0x0700004a, 0x0700002c, 0x0700002f, 0x0700005a, 0x07100000] {
            assert!(blocked_reason(&app, document, module, ModelCode(excluded)).is_some());
        }
        assert!(blocked_reason(&app, document, module, ModelCode(0x07000001)).is_none());
    }
}

pub(super) fn blocked_reason(
    controller: &gp200_app::AppController,
    document: &EditorDocument,
    module: ModuleId,
    target: ModelCode,
) -> Option<String> {
    let Some(current) = document.draft.view.module(module) else {
        return Some(
            crate::localization::text("The selected module is absent from this document.").into(),
        );
    };
    if current.model == target {
        return Some(
            crate::localization::text("Selected. Existing controls stay unchanged.").into(),
        );
    }
    if document.target != EditTarget::DeviceBuffer {
        return None;
    }
    let state = controller.state();
    if state.connection != ConnectionState::Ready {
        return None; // An offline edit stays local and submits no device write.
    }
    if !document.conflicts.is_empty() || controller.device_operation_pending() {
        return Some(
            crate::localization::text(
                "Wait for the current operation or resolve the pedal conflict first.",
            )
            .into(),
        );
    }
    match controller.operation_availability(OperationKind::SetModel) {
        OperationAvailability::Blocked { reason } => return Some(reason),
        OperationAvailability::OfflineOnly => return None,
        OperationAvailability::Available => {}
    }
    if state
        .identity
        .as_ref()
        .is_some_and(|identity| identity.mode == BackendMode::Hardware)
    {
        let Some(snapshot) = &state.confirmed_buffer else {
            return Some(
                crate::localization::text("Read the pedal buffer before selecting a model.").into(),
            );
        };
        if snapshot
            .view
            .module(module)
            .is_none_or(|actual| actual.model != current.model)
        {
            return Some(
                crate::localization::text("The draft model differs from the pedal. Reconcile it before another selection.")
                    .into(),
            );
        }
        return gp200_transport::hardware_model_blocked_reason_with_globals(
            snapshot,
            state.device_globals.as_deref(),
            module,
            target,
        );
    }
    None
}

fn default_preview(model: &ModelDefinition) -> String {
    let mut values = vec![f32::NAN; 15];
    for (id, value) in model.default_values() {
        if let Some(destination) = values.get_mut(usize::from(id)) {
            *destination = value;
        }
    }
    let parts: Vec<_> = model
        .parameters_in_display_order()
        .into_iter()
        .map(|control| match control.default {
            Some(value) => format!(
                "{} {}",
                crate::localization::text(&control.name),
                crate::localization::value(&control.presentation(value, &values).text)
            ),
            None => std::format!(
                "{} default unknown",
                crate::localization::text(&control.name)
            ),
        })
        .collect();
    parts.join(" · ")
}

impl StudioApp {
    pub(super) fn model_dialog(&mut self, ctx: &egui::Context) {
        let palette = Palette::for_context(ctx);
        let Some(document) = self.controller.active_document().cloned() else {
            self.model_picker = false;
            return;
        };
        let mut open = true;
        egui::Window::new(std::format!("Choose {} model", self.selected_module.name()))
            .open(&mut open).default_width(660.0).default_height(520.0)
            .collapsible(false).show(ctx, |ui| {
                ui.add(egui::TextEdit::singleline(&mut self.catalog_search)
                    .hint_text(crate::localization::text("Search models and descriptions")).desired_width(f32::INFINITY));
                let models: Vec<_> = self.controller.catalog()
                    .search_models(Some(self.selected_module.get()), &self.catalog_search)
                    .into_iter().cloned().collect();
                ui.horizontal_wrapped(|ui| {
                    ui.small(std::format!("{} models", models.len()));
                    ui.small(if document.target == EditTarget::DeviceBuffer {
                        crate::localization::text("Pedal selections use the connected validation profile")
                    } else { crate::localization::text("Offline draft — all catalog models available") });
                });
                if document.target == EditTarget::DeviceBuffer
                    && self.controller.state().identity.as_ref().is_some_and(|identity| identity.mode == BackendMode::Hardware)
                    && self.controller.state().confirmed_buffer.as_ref().is_some_and(|snapshot| {
                        models.iter().any(|model| snapshot.view.module(self.selected_module).is_some_and(|current|
                            dependent_model_pair(self.selected_module, current.model, ModelCode(model.code)) || factory_amp_pair(self.selected_module, current.model, ModelCode(model.code))))
                    })
                    && self.controller.state().device_globals.as_ref().is_none_or(|globals|
                        globals.epoch != self.controller.state().epoch || dependent_globals_shape(globals.records().raw()).is_err())
                {
                    ui.small("Read pedal settings to check Auto CAB before changing models.");
                    if ui.add_enabled(!self.controller.device_operation_pending()
                        && matches!(self.controller.operation_availability(OperationKind::ReadGlobals), OperationAvailability::Available),
                        egui::Button::new(crate::localization::text("Read global settings"))).clicked()
                    {
                        self.send(UiIntent::RefreshDeviceGlobals);
                    }
                }
                if cfg!(feature = "factory-amp-trials") && self.selected_module.get() == 3 && document.target == EditTarget::DeviceBuffer {
                    ui.colored_label(palette.warning, "Factory amp trial build: each Match CL pair requires separate pedal validation.");
                }
                ui.add_space(6.0);
                egui::ScrollArea::vertical().id_salt("model-picker-scroll")
                    .max_height((ctx.content_rect().height() - 230.0).clamp(160.0, 460.0))
                    .show(ui, |ui| {
                        for model in models {
                            let reason = blocked_reason(&self.controller, &document, self.selected_module, ModelCode(model.code));
                            let selected = document.draft.view.module(self.selected_module)
                                .is_some_and(|current| current.model.0 == model.code);
                            ui.horizontal_wrapped(|ui| {
                                let button = ui.add_enabled(reason.is_none(), egui::Button::new(&model.name));
                                if button.clicked() {
                                    self.edit(EditPatch::SetModel { module: self.selected_module, model: ModelCode(model.code) });
                                    self.model_picker = false;
                                }
                                if selected { ui.colored_label(palette.accent, crate::localization::text("Selected")); }
                                ui.small(match model.slot_kind() {
                                    SlotKind::Factory => std::format!("{} controls", model.parameters.len()),
                                    SlotKind::UserIr { slot } => std::format!("User IR slot {}", slot + 1),
                                    SlotKind::SnapTone { slot } => std::format!("SnapTone slot {}", slot + 1),
                                });
                            });
                            if let Some(description) = model.description() { ui.small(crate::localization::description(&model.module, model.code, description)); }
                            ui.label(RichText::new(std::format!("Defaults: {}", default_preview(&model)))
                                .small().color(palette.muted));
                            if self.selected_module.get() == 3 && let Some(candidate) = factory_amp_descriptor(ModelCode(model.code)) && !candidate.independently_blocked_controls.is_empty() {
                                let names = candidate.independently_blocked_controls.iter().filter_map(|id| model.parameter(*id)).map(|control| control.name.as_str()).collect::<Vec<_>>().join(", ");
                                ui.small(format!("Separate control validation required: {names}."));
                            }
                            let plan = ModelDefaultPlan::from_definition(&model);
                            if !plan.unknown_defaults.is_empty() {
                                ui.colored_label(palette.warning, crate::localization::text("Unknown defaults keep their current raw values."));
                            }
                            if let Some(reason) = reason { ui.small(crate::localization::message(&reason)); }
                            ui.add_space(4.0);
                            ui.separator();
                        }
                    });
                ui.add_space(6.0);
                ui.small(crate::localization::text("Selecting a different model applies its declared defaults in one Undo step. Hidden raw controls stay intact."));
            });
        if !open {
            self.model_picker = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offline_picker_accepts_factory_and_user_slots_but_same_code_is_inert() {
        let mut controller = gp200_app::AppController::new(
            gp200_catalog::Catalog::embedded().unwrap(),
            AppPreferences::default(),
        );
        controller.dispatch(AppInput::Ui(UiIntent::NewOfflineFrom(
            gp200_sim::captured_demo_snapshot(),
        )));
        let document = controller.active_document().unwrap();
        let module = ModuleId::new(5).unwrap();
        assert!(blocked_reason(&controller, document, module, ModelCode(0x0a000000)).is_none());
        assert!(blocked_reason(&controller, document, module, ModelCode(0x0a100000)).is_none());
        assert!(
            blocked_reason(
                &controller,
                document,
                module,
                document.draft.view.module(module).unwrap().model
            )
            .is_some()
        );
    }

    #[test]
    fn preview_marks_unrecovered_defaults_and_uses_sparse_control_ids() {
        let catalog = gp200_catalog::Catalog::embedded().unwrap();
        let cab = catalog.model_by_id(5, 0x0a000000).unwrap();
        let preview = default_preview(cab);
        assert!(preview.contains("Volume 50"));
        assert!(preview.contains("Low Cut OFF"));
        assert_eq!(
            cab.parameters
                .iter()
                .map(|p| p.parameter_id)
                .collect::<Vec<_>>(),
            [1, 5, 6]
        );
        let mut model = cab.clone();
        model.parameters[0].default = None;
        assert!(default_preview(&model).contains("default unknown"));
    }

    #[test]
    fn picker_qa_flag_is_passive_and_every_module_renders_without_an_edit() {
        let options = crate::LaunchOptions::parse(["--qa-model-picker".to_owned()]).unwrap();
        assert!(options.qa_model_picker);
        assert!(!options.demo && !options.connect && !options.scan_bank && !options.read_globals);
        for preference in [ThemePreference::Dark, ThemePreference::Light] {
            for size in [egui::vec2(1280.0, 800.0), egui::vec2(960.0, 640.0)] {
                let controller = gp200_app::AppController::with_demo(
                    gp200_sim::captured_demo_snapshot(),
                    AppPreferences::default(),
                )
                .unwrap();
                let services = crate::runtime::RuntimeServices::new(
                    Box::new(gp200_sim::simulator_service(std::sync::Arc::new(|| {}))),
                    std::sync::Arc::new(|| {}),
                );
                let mut app = StudioApp::with_services(controller, services);
                let original = app.controller.active_document().unwrap().draft.clone();
                let context = egui::Context::default();
                crate::theme::install_fonts(&context);
                crate::theme::apply(&context, preference, 1.0);
                for module in ModuleId::all() {
                    app.selected_module = module;
                    app.model_picker = true;
                    let mut output = context.run_ui(
                        egui::RawInput {
                            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
                            ..Default::default()
                        },
                        |ui| app.model_dialog(ui.ctx()),
                    );
                    assert!(!output.shapes.is_empty());
                    output.textures_delta.clear();
                    assert_eq!(app.controller.active_document().unwrap().draft, original);
                    assert!(
                        app.controller
                            .active_document()
                            .unwrap()
                            .undo
                            .past
                            .is_empty()
                    );
                    assert!(!app.controller.device_operation_pending());
                }
            }
        }
    }

    #[test]
    fn connected_picker_shares_actor_transition_gates_and_blocks_pending_edits() {
        let mut controller = gp200_app::AppController::new(
            gp200_catalog::Catalog::embedded().unwrap(),
            AppPreferences::default(),
        );
        let effects = controller.dispatch(AppInput::Ui(UiIntent::Connect(DeviceSelection {
            input_id: "fixture-in".into(),
            output_id: "fixture-out".into(),
            fingerprint_hint: None,
        })));
        let Effect::Device(DeviceCommand::Connect { intent, .. }) = effects[0] else {
            panic!()
        };
        controller.dispatch(AppInput::Device(DeviceEvent::SessionChanged {
            epoch: SessionEpoch(1),
            identity: DeviceIdentity {
                model: DeviceModel::Gp200,
                mode: BackendMode::Hardware,
                firmware: None,
                display_name: "Fixture GP-200".into(),
            },
            capabilities: gp200_transport::verified_capabilities(),
        }));
        controller.dispatch(AppInput::Device(DeviceEvent::ConnectionChanged {
            epoch: SessionEpoch(1),
            state: ConnectionState::Ready,
        }));
        controller.dispatch(AppInput::Device(DeviceEvent::SnapshotReceived {
            stamp: RequestStamp {
                epoch: SessionEpoch(1),
                intent,
            },
            target: PresetTarget::EditBuffer,
            snapshot: gp200_sim::captured_demo_snapshot(),
        }));
        let document = controller.active_document().unwrap().clone();
        let dly = ModuleId::new(8).unwrap();
        assert!(blocked_reason(&controller, &document, dly, ModelCode(0x0b00001d)).is_none());
        assert!(blocked_reason(&controller, &document, dly, ModelCode(0x0b000021)).is_some());
        assert!(
            blocked_reason(
                &controller,
                &document,
                ModuleId::new(3).unwrap(),
                ModelCode(0x07000001)
            )
            .is_some()
        );
        let mut offline = document.clone();
        offline.target = EditTarget::Offline(DocumentId("local".into()));
        assert!(blocked_reason(&controller, &offline, dly, ModelCode(0x0b000021)).is_none());
        controller.dispatch(AppInput::Ui(UiIntent::ApplyEdit {
            document: document.id.clone(),
            patch: EditPatch::SetParameter {
                module: ModuleId::new(3).unwrap(),
                model: ModelCode(0x07000015),
                model_index: ModelIndex(13),
                parameter: ParamId::new(0).unwrap(),
                value: F32Bits::from_f32(51.0),
            },
        }));
        assert!(blocked_reason(&controller, &document, dly, ModelCode(0x0b00001d)).is_some());
    }
}
