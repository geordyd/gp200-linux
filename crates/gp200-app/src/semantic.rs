use gp200_domain::*;

pub(crate) fn equivalent(a: &PresetDraft, b: &PresetDraft) -> bool {
    a.semantically_equal(b)
}

pub(crate) fn same_field(a: &EditPatch, b: &EditPatch) -> bool {
    match (a, b) {
        (EditPatch::SetPatchAssignment(a), EditPatch::SetPatchAssignment(b)) => {
            assignment_key(a) == assignment_key(b)
                && std::mem::discriminant(a) == std::mem::discriminant(b)
        }
        (
            EditPatch::SetParameter {
                module: am,
                parameter: ap,
                ..
            },
            EditPatch::SetParameter {
                module: bm,
                parameter: bp,
                ..
            },
        ) => am == bm && ap == bp,
        (
            EditPatch::SetSync {
                module: am,
                power_parameter: ap,
                ..
            },
            EditPatch::SetSync {
                module: bm,
                power_parameter: bp,
                ..
            },
        ) => am == bm && ap == bp,
        (EditPatch::SetEnabled { module: a, .. }, EditPatch::SetEnabled { module: b, .. })
        | (EditPatch::SetModel { module: a, .. }, EditPatch::SetModel { module: b, .. })
        | (EditPatch::MoveModule { module: a, .. }, EditPatch::MoveModule { module: b, .. }) => {
            a == b
        }
        (EditPatch::SetMetadata(_), EditPatch::SetMetadata(_)) => true,
        (EditPatch::SetExpression(a), EditPatch::SetExpression(b)) => {
            (a.controller, a.bank) == (b.controller, b.bank)
        }
        (EditPatch::SetFootswitch(a), EditPatch::SetFootswitch(b)) => a.switch == b.switch,
        (EditPatch::SetQuickControl(a), EditPatch::SetQuickControl(b)) => a.index == b.index,
        _ => false,
    }
}

pub(crate) fn patch_matches(patch: &EditPatch, snapshot: &PresetSnapshot) -> bool {
    match patch {
        EditPatch::SetPatchAssignment(edit) => {
            let mut expected = snapshot.view.patch_assignments.clone();
            expected.apply(&snapshot.view.modules, edit).is_ok()
                && expected.semantically_equal(&snapshot.view.patch_assignments)
        }
        EditPatch::SetSync {
            module,
            model,
            power_parameter,
            value_parameter,
            enabled,
            ordinary_restore,
            ..
        } => snapshot.view.module(*module).is_some_and(|actual| {
            let value = if *enabled {
                Some(gp200_protocol::F32Bits::from_f32(4.0))
            } else {
                *ordinary_restore
            };
            actual.model == *model
                && value.is_some()
                && actual.parameter(*power_parameter)
                    == Some(gp200_protocol::F32Bits::from_f32(if *enabled {
                        1.0
                    } else {
                        0.0
                    }))
                && actual.parameter(*value_parameter) == value
        }),
        EditPatch::SetParameter {
            module,
            model,
            parameter,
            value,
            ..
        } => snapshot.view.module(*module).is_some_and(|actual| {
            actual.model == *model && actual.parameter(*parameter) == Some(*value)
        }),
        EditPatch::SetEnabled { module, enabled } => snapshot
            .view
            .module(*module)
            .is_some_and(|actual| actual.state_raw == u8::from(*enabled)),
        EditPatch::SetModel { module, model } => snapshot
            .view
            .module(*module)
            .is_some_and(|actual| actual.model == *model),
        EditPatch::SetMetadata(meta) => {
            meta.name
                .as_ref()
                .is_none_or(|value| snapshot.view.name.value() == Some(value))
                && meta
                    .tempo
                    .as_ref()
                    .is_none_or(|value| snapshot.view.tempo.value() == Some(value))
                && meta
                    .level
                    .as_ref()
                    .is_none_or(|value| snapshot.view.level.value() == Some(value))
                && meta
                    .author
                    .as_ref()
                    .is_none_or(|value| snapshot.view.metadata.author.as_ref() == Some(value))
                && meta
                    .style
                    .as_ref()
                    .is_none_or(|value| snapshot.view.metadata.style.as_ref() == Some(value))
                && meta
                    .notes
                    .as_ref()
                    .is_none_or(|value| snapshot.view.metadata.notes.as_ref() == Some(value))
                && meta
                    .pan
                    .as_ref()
                    .is_none_or(|value| snapshot.view.metadata.pan.as_ref() == Some(value))
                && meta
                    .native_pan
                    .is_none_or(|value| snapshot.view.metadata.native_pan == Some(value))
                && meta
                    .native_style
                    .is_none_or(|value| snapshot.view.metadata.native_style == Some(value))
                && meta
                    .fx_send_level
                    .is_none_or(|value| snapshot.view.metadata.fx_send_level == Some(value))
                && meta
                    .fx_return_level
                    .is_none_or(|value| snapshot.view.metadata.fx_return_level == Some(value))
                && meta
                    .fx_loop_mode
                    .is_none_or(|value| snapshot.view.metadata.fx_loop_mode == Some(value))
        }
        EditPatch::MoveModule { module, before } => {
            snapshot.view.chain.value().is_some_and(|chain| {
                let position = chain.iter().position(|candidate| candidate == module);
                match before {
                    Some(before) => position
                        .zip(chain.iter().position(|candidate| candidate == before))
                        .is_some_and(|(a, b)| a + 1 == b),
                    None => position == chain.len().checked_sub(1),
                }
            })
        }
        EditPatch::SetExpression(value) => snapshot.view.expressions.contains(value),
        EditPatch::SetFootswitch(value) => snapshot.view.footswitches.contains(value),
        EditPatch::SetQuickControl(value) => snapshot.view.quick_controls.contains(value),
    }
}

/// Merge fields that the user has not edited; retain competing edits explicitly.
/// Equality is bitwise for parameters and semantic for recovered metadata, so
/// opaque padding or evidence annotations cannot create a false field conflict.
#[derive(Debug, Clone)]
pub(crate) enum ConflictField {
    Model(ModuleId),
    Enabled(ModuleId),
    Parameter(ModuleId, usize),
    Name,
    Tempo,
    Level,
    NativePan,
    NativeStyle,
    FxSendLevel,
    FxReturnLevel,
    FxLoopMode,
    Chain,
    PatchAssignment(u16, u8),
}
impl ConflictField {
    pub(crate) fn description(&self) -> String {
        match self {
            Self::Model(module) => format!("{} model changed on the pedal", module.name()),
            Self::Enabled(module) => format!("{} bypass changed on the pedal", module.name()),
            Self::Parameter(module, parameter) => format!(
                "{} parameter ID {parameter} changed on the pedal",
                module.name()
            ),
            Self::Name => "Name changed on the pedal".into(),
            Self::Tempo => "Tempo changed on the pedal".into(),
            Self::Level => "Level changed on the pedal".into(),
            Self::NativePan => "Patch Pan changed on the pedal".into(),
            Self::NativeStyle => "Native style changed on the pedal".into(),
            Self::FxSendLevel => "FX Send level changed on the pedal".into(),
            Self::FxReturnLevel => "FX Return level changed on the pedal".into(),
            Self::FxLoopMode => "FX loop mode changed on the pedal".into(),
            Self::Chain => "Chain changed on the pedal".into(),
            Self::PatchAssignment(command, identity) => {
                format!("Assignment {command:#06x}/{identity:#04x} changed on the pedal")
            }
        }
    }
    fn acknowledged(&self, patch: Option<&EditPatch>) -> bool {
        match (self, patch) {
            (
                Self::PatchAssignment(command, identity),
                Some(EditPatch::SetPatchAssignment(edit)),
            ) => assignment_key(edit) == (*command, *identity),
            (
                Self::Parameter(module, parameter),
                Some(EditPatch::SetSync {
                    module: affected,
                    power_parameter,
                    value_parameter,
                    ..
                }),
            ) => {
                module == affected
                    && (*parameter == usize::from(power_parameter.get())
                        || *parameter == usize::from(value_parameter.get()))
            }
            (
                Self::Parameter(module, parameter),
                Some(EditPatch::SetParameter {
                    module: affected,
                    parameter: id,
                    ..
                }),
            ) => module == affected && *parameter == usize::from(id.get()),
            (
                Self::Model(module),
                Some(EditPatch::SetModel {
                    module: affected, ..
                }),
            ) => module == affected,
            (
                Self::Enabled(module),
                Some(EditPatch::SetEnabled {
                    module: affected, ..
                }),
            ) => module == affected,
            (Self::Name, Some(EditPatch::SetMetadata(value))) => value.name.is_some(),
            (Self::Tempo, Some(EditPatch::SetMetadata(value))) => value.tempo.is_some(),
            (Self::Level, Some(EditPatch::SetMetadata(value))) => value.level.is_some(),
            (Self::NativePan, Some(EditPatch::SetMetadata(value))) => value.native_pan.is_some(),
            (Self::NativeStyle, Some(EditPatch::SetMetadata(value))) => {
                value.native_style.is_some()
            }
            (Self::FxSendLevel, Some(EditPatch::SetMetadata(value))) => {
                value.fx_send_level.is_some()
            }
            (Self::FxReturnLevel, Some(EditPatch::SetMetadata(value))) => {
                value.fx_return_level.is_some()
            }
            (Self::FxLoopMode, Some(EditPatch::SetMetadata(value))) => value.fx_loop_mode.is_some(),
            (Self::Chain, Some(EditPatch::MoveModule { .. })) => true,
            _ => false,
        }
    }
    pub(crate) fn accept(&self, draft: &mut PresetDraft, actual: &PresetSnapshot) {
        match self {
            Self::PatchAssignment(command, identity) => {
                let destination = &mut draft.view.patch_assignments;
                let source = &actual.view.patch_assignments;
                match command {
                    8 => destination.routing.clone_from(&source.routing),
                    12 => {
                        accept_assignment(&mut destination.expressions, &source.expressions, |e| {
                            e.record.slot.packed() == *identity
                        })
                    }
                    15 => accept_assignment(&mut destination.ctrls, &source.ctrls, |e| {
                        e.record.slot == *identity
                    }),
                    16 => accept_assignment(&mut destination.quicks, &source.quicks, |e| {
                        e.record.index == *identity
                    }),
                    _ => {}
                }
            }
            Self::Model(id) | Self::Enabled(id) | Self::Parameter(id, _) => {
                let Some(module) = draft
                    .view
                    .modules
                    .iter_mut()
                    .find(|module| module.module == *id)
                else {
                    return;
                };
                let Some(other) = actual.view.module(*id) else {
                    return;
                };
                match self {
                    Self::Model(_) => {
                        module.model = other.model;
                        module.parameters.clone_from(&other.parameters);
                    }
                    Self::Enabled(_) => module.state_raw = other.state_raw,
                    Self::Parameter(_, index) => {
                        if let Some(value) = other.parameters.get(*index)
                            && let Some(destination) = module.parameters.get_mut(*index)
                        {
                            *destination = *value;
                        }
                    }
                    _ => {}
                }
            }
            Self::Name => {
                draft.view.name = actual.view.name.clone();
                draft.metadata.name = actual.view.name.value().cloned();
            }
            Self::Tempo => {
                draft.view.tempo = actual.view.tempo.clone();
                draft.metadata.tempo = actual.view.tempo.value().copied();
            }
            Self::Level => {
                draft.view.level = actual.view.level.clone();
                draft.metadata.level = actual.view.level.value().copied();
            }
            Self::Chain => draft.view.chain = actual.view.chain.clone(),
            Self::NativePan => draft.metadata.native_pan = actual.view.metadata.native_pan,
            Self::NativeStyle => draft.metadata.native_style = actual.view.metadata.native_style,
            Self::FxSendLevel => draft.metadata.fx_send_level = actual.view.metadata.fx_send_level,
            Self::FxReturnLevel => {
                draft.metadata.fx_return_level = actual.view.metadata.fx_return_level
            }
            Self::FxLoopMode => draft.metadata.fx_loop_mode = actual.view.metadata.fx_loop_mode,
        }
        draft.view.metadata = draft.metadata.clone();
    }
}

pub(crate) fn merge_readback(
    draft: &mut PresetDraft,
    old: &PresetSnapshot,
    incoming: &PresetSnapshot,
    acknowledged: Option<&EditPatch>,
    catalog: &gp200_catalog::Catalog,
    dependent: Option<&crate::BoundModelPlan>,
) -> Vec<ConflictField> {
    let mut conflicts = Vec::new();
    for module in &mut draft.view.modules {
        let (Some(previous), Some(actual)) = (
            old.view.module(module.module),
            incoming.view.module(module.module),
        ) else {
            continue;
        };
        let selected = match acknowledged {
            Some(EditPatch::SetModel {
                module: affected,
                model,
            }) if *affected == module.module => Some(*model),
            _ => dependent
                .filter(|_| module.module.get() == 5)
                .and_then(|plan| plan.paired_cabinet()),
        };
        // A later selection/Undo is a deliberate draft model change. An older
        // readback must not replace it or copy controls across two model Codes.
        let preserve_later_selection = selected.is_some() && module.model != actual.model;
        if !preserve_later_selection {
            if module.model == previous.model {
                if actual.model != previous.model && module.parameters != previous.parameters {
                    conflicts.push(ConflictField::Model(module.module));
                } else {
                    module.model = actual.model;
                }
            } else if actual.model != previous.model && actual.model != module.model {
                conflicts.push(ConflictField::Model(module.module));
            }
        }
        if module.state_raw == previous.state_raw {
            module.state_raw = actual.state_raw;
        } else if actual.state_raw != previous.state_raw && actual.state_raw != module.state_raw {
            conflicts.push(ConflictField::Enabled(module.module));
        }
        if preserve_later_selection || module.model != actual.model {
            continue;
        }
        let definition = selected
            .filter(|model| *model == actual.model)
            .and_then(|model| catalog.model_by_id(module.module.get(), model.0));
        let mut expected: Vec<u32> = previous.parameters.iter().map(|value| value.0).collect();
        if let Some(definition) = definition {
            let _ = gp200_catalog::ModelDefaultPlan::from_definition(definition)
                .apply_to_bits(&mut expected);
        }
        for (index, value) in module.parameters.iter_mut().enumerate() {
            let (Some(previous_bits), Some(actual_value)) =
                (expected.get(index), actual.parameters.get(index))
            else {
                continue;
            };
            // Native selection preserves controls that the target does not
            // declare. They cannot receive a deliberate parameter edit under
            // this model; retain its actual bits rather than replay history's
            // unrelated old-model slots.
            if definition.is_some_and(|model| model.parameter(index as u8).is_none()) {
                *value = *actual_value;
                continue;
            }
            let previous_value = F32Bits(*previous_bits);
            if *value == previous_value {
                *value = *actual_value;
            } else if *actual_value != previous_value && actual_value != value {
                conflicts.push(ConflictField::Parameter(module.module, index));
            }
        }
    }
    macro_rules! recovered {
        ($field:ident, $variant:ident) => {
            if draft.view.$field.value() == old.view.$field.value() {
                draft.view.$field = incoming.view.$field.clone();
            } else if incoming.view.$field.value() != old.view.$field.value()
                && incoming.view.$field.value() != draft.view.$field.value()
            {
                conflicts.push(ConflictField::$variant);
            }
        };
    }
    recovered!(name, Name);
    recovered!(tempo, Tempo);
    recovered!(level, Level);
    recovered!(chain, Chain);
    macro_rules! native_metadata {
        ($field:ident, $variant:ident) => {
            if draft.metadata.$field == old.view.metadata.$field {
                draft.metadata.$field = incoming.view.metadata.$field;
            } else if incoming.view.metadata.$field != old.view.metadata.$field
                && incoming.view.metadata.$field != draft.metadata.$field
            {
                conflicts.push(ConflictField::$variant);
            }
        };
    }
    native_metadata!(native_pan, NativePan);
    native_metadata!(native_style, NativeStyle);
    native_metadata!(fx_send_level, FxSendLevel);
    native_metadata!(fx_return_level, FxReturnLevel);
    native_metadata!(fx_loop_mode, FxLoopMode);
    draft.metadata.name = draft.view.name.value().cloned();
    draft.metadata.tempo = draft.view.tempo.value().copied();
    draft.metadata.level = draft.view.level.value().copied();
    draft.view.metadata = draft.metadata.clone();
    merge_assignments(
        &mut draft.view.patch_assignments,
        &old.view.patch_assignments,
        &incoming.view.patch_assignments,
        &mut conflicts,
    );
    conflicts
        .into_iter()
        .filter(|field| !field.acknowledged(acknowledged))
        .collect()
}

fn assignment_key(edit: &AssignmentEdit) -> (u16, u8) {
    match edit {
        AssignmentEdit::FxLoopPositions { .. } => (8, 0),
        AssignmentEdit::ExpressionTarget { slot, .. }
        | AssignmentEdit::ExpressionTargetAndRange { slot, .. }
        | AssignmentEdit::ExpressionRange { slot, .. } => (12, slot.packed()),
        AssignmentEdit::CtrlModules { slot, .. } | AssignmentEdit::CtrlState { slot, .. } => {
            (15, *slot)
        }
        AssignmentEdit::QuickTarget { index, .. } => (16, *index),
    }
}
fn accept_assignment<T: Clone>(
    destination: &mut [IndexedAssignment<T>],
    source: &[IndexedAssignment<T>],
    predicate: impl Fn(&IndexedAssignment<T>) -> bool,
) {
    if let Some(actual) = source.iter().find(|entry| predicate(entry))
        && let Some(ours) = destination.iter_mut().find(|entry| predicate(entry))
    {
        *ours = actual.clone();
    }
}
// The three projections and the per-record identity/equality/retention policy
// are separate inputs; keeping them explicit prevents volatile CTRL padding
// from accidentally entering the semantic comparison.
#[allow(clippy::too_many_arguments)]
fn merge_assignment_entries<T: Clone>(
    destination: &mut Vec<IndexedAssignment<T>>,
    previous: &[IndexedAssignment<T>],
    actual: &[IndexedAssignment<T>],
    command: u16,
    identity: impl Fn(&T) -> u8,
    equal: impl Fn(&T, &T) -> bool,
    retain: impl Fn(&mut T, &T),
    conflicts: &mut Vec<ConflictField>,
) {
    for ours in destination.iter_mut() {
        let id = identity(&ours.record);
        let (Some(old), Some(new)) = (
            previous.iter().find(|entry| identity(&entry.record) == id),
            actual.iter().find(|entry| identity(&entry.record) == id),
        ) else {
            conflicts.push(ConflictField::PatchAssignment(command, id));
            continue;
        };
        if equal(&ours.record, &old.record) {
            *ours = new.clone();
        } else {
            if !equal(&new.record, &old.record) && !equal(&new.record, &ours.record) {
                conflicts.push(ConflictField::PatchAssignment(command, id));
            }
            ours.raw_record_index = new.raw_record_index;
            retain(&mut ours.record, &new.record);
        }
    }
    for new in actual {
        if !destination
            .iter()
            .any(|entry| identity(&entry.record) == identity(&new.record))
        {
            destination.push(new.clone());
        }
    }
}
fn merge_assignments(
    destination: &mut PatchAssignments,
    previous: &PatchAssignments,
    actual: &PatchAssignments,
    conflicts: &mut Vec<ConflictField>,
) {
    if destination.routing.as_ref().map(|e| &e.record)
        == previous.routing.as_ref().map(|e| &e.record)
    {
        destination.routing.clone_from(&actual.routing);
    } else {
        if actual.routing.as_ref().map(|e| &e.record)
            != previous.routing.as_ref().map(|e| &e.record)
            && actual.routing.as_ref().map(|e| &e.record)
                != destination.routing.as_ref().map(|e| &e.record)
        {
            conflicts.push(ConflictField::PatchAssignment(8, 0));
        }
        if let (Some(ours), Some(new)) = (&mut destination.routing, &actual.routing) {
            ours.raw_record_index = new.raw_record_index;
        }
    }
    merge_assignment_entries(
        &mut destination.expressions,
        &previous.expressions,
        &actual.expressions,
        12,
        |r| r.slot.packed(),
        |a, b| a == b,
        |_, _| {},
        conflicts,
    );
    merge_assignment_entries(
        &mut destination.ctrls,
        &previous.ctrls,
        &actual.ctrls,
        15,
        |r| r.slot,
        |a, b| {
            a.slot == b.slot && a.state_raw == b.state_raw && a.module_mask_raw == b.module_mask_raw
        },
        |ours, new| ours.retained_padding = new.retained_padding,
        conflicts,
    );
    merge_assignment_entries(
        &mut destination.quicks,
        &previous.quicks,
        &actual.quicks,
        16,
        |r| r.index,
        |a, b| a == b,
        |_, _| {},
        conflicts,
    );
    destination.diagnostics.clone_from(&actual.diagnostics);
}
