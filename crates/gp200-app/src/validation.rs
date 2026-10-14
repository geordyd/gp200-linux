use crate::AppController;
use gp200_domain::*;

impl AppController {
    pub(crate) fn validate_patch(
        &self,
        document: &EditorDocument,
        patch: &EditPatch,
    ) -> Result<(), ServiceError> {
        match patch {
            EditPatch::SetPatchAssignment(edit) => {
                let mut assignments = document.draft.view.patch_assignments.clone();
                assignments.apply(&document.draft.view.modules, edit)?;
                let parameter_target = match edit {
                    AssignmentEdit::ExpressionTarget {
                        target: ExpressionAssignmentTarget::Parameter(target),
                        ..
                    }
                    | AssignmentEdit::ExpressionTargetAndRange {
                        target: ExpressionAssignmentTarget::Parameter(target),
                        ..
                    }
                    | AssignmentEdit::QuickTarget {
                        target: QuickAssignmentTarget::Parameter(target),
                        ..
                    } => Some(target.clone()),
                    AssignmentEdit::ExpressionRange { slot, .. } => {
                        let record = assignments
                            .expressions
                            .iter()
                            .find(|entry| entry.record.slot == *slot)
                            .ok_or_else(|| ServiceError::invalid("EXP assignment record absent"))?;
                        match PatchAssignments::resolve_expression(
                            &record.record,
                            &document.draft.view.modules,
                        ) {
                            ResolvedAssignmentReference::Parameter(target) => Some(target),
                            ResolvedAssignmentReference::Off => {
                                return Err(ServiceError::invalid(
                                    "Select an EXP parameter before changing its endpoints",
                                ));
                            }
                            _ => {
                                return Err(ServiceError::invalid(
                                    "Unresolved EXP target cannot receive new endpoints",
                                ));
                            }
                        }
                    }
                    _ => None,
                };
                if let Some(target) = parameter_target {
                    let definition = self
                        .catalog
                        .model_by_id(target.module.get(), target.expected_model.0)
                        .and_then(|model| model.parameter(target.parameter.get()))
                        .ok_or_else(|| {
                            ServiceError::invalid(
                                "Assignment parameter ID is not declared by the selected model",
                            )
                        })?;
                    if matches!(
                        edit,
                        AssignmentEdit::ExpressionTarget { .. }
                            | AssignmentEdit::ExpressionTargetAndRange { .. }
                            | AssignmentEdit::ExpressionRange { .. }
                    ) {
                        let slot = match edit {
                            AssignmentEdit::ExpressionTarget { slot, .. }
                            | AssignmentEdit::ExpressionTargetAndRange { slot, .. }
                            | AssignmentEdit::ExpressionRange { slot, .. } => *slot,
                            _ => unreachable!(),
                        };
                        let record = &assignments
                            .expressions
                            .iter()
                            .find(|entry| entry.record.slot == slot)
                            .unwrap()
                            .record;
                        let Some((low, high, _)) = definition.numeric_bounds() else {
                            return Err(ServiceError::invalid(
                                "EXP target requires recovered numeric endpoint bounds",
                            ));
                        };
                        for value in [record.minimum.as_f32(), record.maximum.as_f32()] {
                            if !value.is_finite() || value < low || value > high {
                                return Err(ServiceError::invalid(
                                    "EXP endpoints are outside the selected parameter range; assign target and range together",
                                ));
                            }
                        }
                    }
                }
            }
            EditPatch::SetSync {
                module,
                model,
                model_index,
                power_parameter,
                value_parameter,
                enabled,
                ordinary_restore,
            } => {
                let current =
                    document.draft.view.module(*module).ok_or_else(|| {
                        ServiceError::invalid("Module is absent from this document")
                    })?;
                let definition =
                    self.catalog
                        .model_by_id(module.get(), model.0)
                        .ok_or_else(|| {
                            ServiceError::invalid("Sync model is absent from the catalog")
                        })?;
                if current.model != *model
                    || model_index.0 != u32::from(definition.xml_index)
                    || gp200_catalog::sync_value_parameter(definition, power_parameter.get())
                        .map_err(|error| ServiceError::invalid(error.to_string()))?
                        != value_parameter.get()
                {
                    return Err(ServiceError::invalid(
                        "Sync model Index or binding is invalid",
                    ));
                }
                let bits: Vec<u32> = current.parameters.iter().map(|value| value.0).collect();
                gp200_catalog::plan_control_request(
                    definition,
                    power_parameter.get(),
                    &bits,
                    gp200_catalog::ControlRequest::Sync {
                        enabled: *enabled,
                        ordinary_restore: ordinary_restore.map(|value| {
                            gp200_catalog::OrdinaryRestore::ExplicitUserValue(value.0)
                        }),
                    },
                )
                .map_err(|error| ServiceError::invalid(error.to_string()))?;
            }
            EditPatch::SetParameter {
                module,
                model,
                model_index,
                parameter,
                value,
            } => {
                let current =
                    document.draft.view.module(*module).ok_or_else(|| {
                        ServiceError::invalid("Module is absent from this document")
                    })?;
                if current.model != *model {
                    return Err(ServiceError::invalid(
                        "The parameter belongs to a different selected model",
                    ));
                }
                let definition =
                    self.catalog
                        .model_by_id(module.get(), model.0)
                        .ok_or_else(|| {
                            ServiceError::invalid(
                                "Unknown models retain their raw data and cannot be edited",
                            )
                        })?;
                if model_index.0 != u32::from(definition.xml_index) {
                    return Err(ServiceError::invalid(
                        "Parameter writes require the XML model Index, distinct from model Code",
                    ));
                }
                let control = definition.parameter(parameter.get()).ok_or_else(|| {
                    ServiceError::invalid(
                        "This XML parameter ID is not declared by the selected model",
                    )
                })?;
                let values: Vec<f32> = current
                    .parameters
                    .iter()
                    .map(|bits| bits.as_f32())
                    .collect();
                let previous = current
                    .parameter(*parameter)
                    .ok_or_else(|| ServiceError::invalid("The raw parameter slot is missing"))?;
                let presentation = control.presentation(previous.as_f32(), &values);
                if !presentation.editable {
                    return Err(ServiceError::new(
                        ErrorKind::Unverified,
                        presentation.explanation.unwrap_or_else(|| {
                            "This control's conversion remains unverified".into()
                        }),
                    ));
                }
                let bits: Vec<u32> = current.parameters.iter().map(|value| value.0).collect();
                gp200_catalog::plan_control_request(
                    definition,
                    parameter.get(),
                    &bits,
                    gp200_catalog::ControlRequest::RawBits(value.0),
                )
                .map_err(|error| ServiceError::invalid(error.to_string()))?;
            }
            EditPatch::SetEnabled { module, .. } => {
                if document.draft.view.module(*module).is_none() {
                    return Err(ServiceError::invalid("Module is absent from this document"));
                }
            }
            EditPatch::SetModel { module, model } => {
                if document.target == EditTarget::DeviceBuffer {
                    self.dependent_model_plan(*module, *model)?;
                }
                let current =
                    document.draft.view.module(*module).ok_or_else(|| {
                        ServiceError::invalid("Module is absent from this document")
                    })?;
                let definition =
                    self.catalog
                        .model_by_id(module.get(), model.0)
                        .ok_or_else(|| {
                            ServiceError::invalid(
                                "Model is not available in this module's GP-200 catalog",
                            )
                        })?;
                let mut bits: Vec<u32> = current.parameters.iter().map(|value| value.0).collect();
                gp200_catalog::ModelDefaultPlan::from_definition(definition)
                    .apply_to_bits(&mut bits)
                    .map_err(ServiceError::invalid)?;
            }
            EditPatch::MoveModule { module, before } => {
                if document.draft.view.module(*module).is_none()
                    || before.is_some_and(|id| document.draft.view.module(id).is_none())
                {
                    return Err(ServiceError::invalid(
                        "Chain targets must be present in the document",
                    ));
                }
                if document.draft.view.modules.len() != 11 {
                    return Err(ServiceError::invalid(
                        "Chain editing requires all eleven modules",
                    ));
                }
            }
            EditPatch::SetMetadata(metadata) => {
                metadata.validate_native()?;
                if metadata
                    .name
                    .as_ref()
                    .is_some_and(|name| name.chars().count() > 16 || name.contains('\0'))
                {
                    return Err(ServiceError::invalid(
                        "Preset names support at most sixteen characters and cannot contain NUL",
                    ));
                }
                if metadata
                    .pan
                    .is_some_and(|value| !value.as_f32().is_finite())
                {
                    return Err(ServiceError::invalid("Pan must be finite"));
                }
            }
            EditPatch::SetExpression(expression) => {
                if expression.controller > 1 || expression.bank > 1 {
                    return Err(ServiceError::invalid(
                        "Expression controller and bank must be 0 or 1",
                    ));
                }
                for target in &expression.targets {
                    self.validate_assignment(document, target)?;
                }
            }
            EditPatch::SetFootswitch(footswitch) => {
                if footswitch.switch >= 8 {
                    return Err(ServiceError::invalid("There are eight local CTRL slots"));
                }
                let unique: std::collections::BTreeSet<_> =
                    footswitch.modules.iter().copied().collect();
                if unique.len() != footswitch.modules.len() {
                    return Err(ServiceError::invalid(
                        "A CTRL mapping cannot repeat a module",
                    ));
                }
            }
            EditPatch::SetQuickControl(quick) => {
                if quick.index >= 3 {
                    return Err(ServiceError::invalid("There are three quick controls"));
                }
                if let Some(target) = &quick.target {
                    self.validate_assignment(document, target)?;
                }
            }
        }
        Ok(())
    }
    pub(crate) fn validate_assignment(
        &self,
        document: &EditorDocument,
        target: &AssignmentTarget,
    ) -> Result<(), ServiceError> {
        let model = document
            .draft
            .view
            .module(target.module)
            .ok_or_else(|| ServiceError::invalid("Assignment module is missing"))?;
        if model.model != target.model {
            return Err(ServiceError::invalid(
                "Assignment refers to a model that is no longer selected",
            ));
        }
        let definition = self
            .catalog
            .model_by_id(target.module.get(), target.model.0)
            .and_then(|model| model.parameter(target.parameter.get()))
            .ok_or_else(|| ServiceError::invalid("Assignment parameter is not declared"))?;
        for endpoint in [target.minimum, target.maximum] {
            if !endpoint.as_f32().is_finite() {
                return Err(ServiceError::invalid("Assignment endpoints must be finite"));
            }
            if let Some((minimum, maximum, _)) = definition.numeric_bounds()
                && (endpoint.as_f32() < minimum || endpoint.as_f32() > maximum)
            {
                return Err(ServiceError::invalid(
                    "Assignment endpoints lie outside the parameter range",
                ));
            }
        }
        // Inverted ranges are an intentional local mapping, not an error.
        Ok(())
    }
}
