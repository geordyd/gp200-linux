//! Lossless per-patch assignments. Slot positions and special quick references
//! are authoritative here; the older dense editor vectors are local metadata.
use crate::{ModelCode, ModuleId, ModuleSnapshot, ParamId, PresetSnapshot, ServiceError};
use gp200_protocol::{
    CtrlAssignmentRecord, ExpressionAssignmentRecord, ExpressionSlotId, F32Bits,
    QuickAssignmentRecord, RawRecord, RawTransfer,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexedAssignment<T> {
    pub raw_record_index: usize,
    pub record: T,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssignmentDiagnostic {
    pub raw_record_index: usize,
    pub command: u16,
    pub description: String,
}
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PatchAssignments {
    pub expressions: Vec<IndexedAssignment<ExpressionAssignmentRecord>>,
    pub ctrls: Vec<IndexedAssignment<CtrlAssignmentRecord>>,
    pub quicks: Vec<IndexedAssignment<QuickAssignmentRecord>>,
    pub diagnostics: Vec<AssignmentDiagnostic>,
    #[serde(default)]
    pub routing: Option<IndexedAssignment<PatchRouting>>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PatchRouting {
    pub send_boundary: u8,
    pub return_boundary: u8,
}

/// Explicit current-model dependency. The wire only carries module/parameter;
/// a model changed since the UI opened must invalidate the proposed reference.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParameterAssignmentTarget {
    pub module: ModuleId,
    pub expected_model: ModelCode,
    pub parameter: ParamId,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExpressionAssignmentTarget {
    Off,
    Parameter(ParameterAssignmentTarget),
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum QuickAssignmentTarget {
    Off,
    Parameter(ParameterAssignmentTarget),
    PatchVolume,
    PatchTempo,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AssignmentEdit {
    FxLoopPositions {
        send_boundary: u8,
        return_boundary: u8,
    },
    ExpressionTarget {
        slot: ExpressionSlotId,
        target: ExpressionAssignmentTarget,
    },
    ExpressionTargetAndRange {
        slot: ExpressionSlotId,
        target: ExpressionAssignmentTarget,
        minimum: F32Bits,
        maximum: F32Bits,
    },
    /// Reversed ranges are valid. Endpoints are never sorted or clamped here.
    ExpressionRange {
        slot: ExpressionSlotId,
        minimum: F32Bits,
        maximum: F32Bits,
    },
    CtrlModules {
        slot: u8,
        modules: Vec<ModuleId>,
    },
    CtrlState {
        slot: u8,
        enabled: bool,
    },
    QuickTarget {
        index: u8,
        target: QuickAssignmentTarget,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolvedAssignmentReference {
    Off,
    Parameter(ParameterAssignmentTarget),
    PatchVolume,
    PatchTempo,
    Unresolved { module_raw: u8, parameter_raw: u16 },
}

impl PatchAssignments {
    /// Compare meaningful assignment bytes. CTRL padding is volatile in repeated
    /// device reads; its original values remain present in the raw projection.
    pub fn semantically_equal(&self, other: &Self) -> bool {
        self.routing.as_ref().map(|e| &e.record) == other.routing.as_ref().map(|e| &e.record)
            && self
                .expressions
                .iter()
                .map(|e| &e.record)
                .eq(other.expressions.iter().map(|e| &e.record))
            && self
                .quicks
                .iter()
                .map(|e| &e.record)
                .eq(other.quicks.iter().map(|e| &e.record))
            && self.ctrls.len() == other.ctrls.len()
            && self.ctrls.iter().zip(&other.ctrls).all(|(a, b)| {
                a.record.slot == b.record.slot
                    && a.record.state_raw == b.record.state_raw
                    && a.record.module_mask_raw == b.record.module_mask_raw
            })
    }
    pub fn from_raw(raw: &RawTransfer) -> Self {
        let mut result = Self::default();
        let mut ids = BTreeSet::new();
        for (raw_record_index, record) in raw.records().iter().enumerate() {
            if record.command == 8 && record.body.len() == 16 {
                if ids.insert((8, 0)) {
                    result.routing = Some(IndexedAssignment {
                        raw_record_index,
                        record: PatchRouting {
                            send_boundary: record.body[2],
                            return_boundary: record.body[3],
                        },
                    });
                } else {
                    result.routing = None;
                    result.diagnostics.push(AssignmentDiagnostic {
                        raw_record_index,
                        command: 8,
                        description: "Ambiguous chain/FX routing record".into(),
                    });
                }
                continue;
            }
            let decoded = match record.command {
                0x0c => ExpressionAssignmentRecord::decode(record).map(|typed| {
                    let id = typed.slot.packed();
                    result.expressions.push(IndexedAssignment {
                        raw_record_index,
                        record: typed,
                    });
                    id
                }),
                0x0f => CtrlAssignmentRecord::decode(record).map(|typed| {
                    let id = typed.slot;
                    result.ctrls.push(IndexedAssignment {
                        raw_record_index,
                        record: typed,
                    });
                    id
                }),
                0x10 => QuickAssignmentRecord::decode(record).map(|typed| {
                    let id = typed.index;
                    result.quicks.push(IndexedAssignment {
                        raw_record_index,
                        record: typed,
                    });
                    id
                }),
                _ => continue,
            };
            let description = match decoded {
                Ok(id) if !ids.insert((record.command, id)) => Some(format!(
                    "Duplicate assignment identity {id:#04x}; editing is blocked"
                )),
                Ok(_) => None,
                Err(error) => Some(error.to_string()),
            };
            if let Some(description) = description {
                result.diagnostics.push(AssignmentDiagnostic {
                    raw_record_index,
                    command: record.command,
                    description,
                });
            }
        }
        result
    }

    pub fn apply(
        &mut self,
        modules: &[ModuleSnapshot],
        edit: &AssignmentEdit,
    ) -> Result<(), ServiceError> {
        // Validate all dependencies before changing the retained projection.
        match edit {
            AssignmentEdit::FxLoopPositions {
                send_boundary,
                return_boundary,
            } if send_boundary > return_boundary || *return_boundary > 11 => {
                return Err(ServiceError::invalid(
                    "FX Send/Return must be ordered boundaries in 0..11",
                ));
            }
            AssignmentEdit::ExpressionTarget {
                target: ExpressionAssignmentTarget::Parameter(t),
                ..
            }
            | AssignmentEdit::ExpressionTargetAndRange {
                target: ExpressionAssignmentTarget::Parameter(t),
                ..
            }
            | AssignmentEdit::QuickTarget {
                target: QuickAssignmentTarget::Parameter(t),
                ..
            } => validate_target(modules, t)?,
            AssignmentEdit::ExpressionRange {
                minimum, maximum, ..
            } if !minimum.as_f32().is_finite() || !maximum.as_f32().is_finite() => {
                return Err(ServiceError::invalid("New EXP endpoints must be finite"));
            }
            AssignmentEdit::CtrlModules { modules, .. }
                if modules.iter().collect::<BTreeSet<_>>().len() != modules.len() =>
            {
                return Err(ServiceError::invalid(
                    "CTRL module membership contains duplicates",
                ));
            }
            _ => {}
        }
        if let AssignmentEdit::ExpressionTargetAndRange {
            minimum, maximum, ..
        } = edit
            && (!minimum.as_f32().is_finite() || !maximum.as_f32().is_finite())
        {
            return Err(ServiceError::invalid("New EXP endpoints must be finite"));
        }
        if let AssignmentEdit::ExpressionTargetAndRange {
            target: ExpressionAssignmentTarget::Off,
            minimum,
            maximum,
            ..
        } = edit
            && (*minimum != F32Bits::from_f32(0.) || *maximum != F32Bits::from_f32(0.))
        {
            return Err(ServiceError::invalid(
                "Clearing EXP requires zero endpoints",
            ));
        }
        match edit {
            AssignmentEdit::FxLoopPositions {
                send_boundary,
                return_boundary,
            } => {
                let routing = self.routing.as_mut().ok_or_else(|| {
                    ServiceError::invalid("FX routing record absent or ambiguous")
                })?;
                routing.record = PatchRouting {
                    send_boundary: *send_boundary,
                    return_boundary: *return_boundary,
                };
            }
            AssignmentEdit::ExpressionTarget { slot, target }
            | AssignmentEdit::ExpressionTargetAndRange { slot, target, .. } => {
                let record = &mut unique(&mut self.expressions, |r| r.slot == *slot)?.record;
                match target {
                    ExpressionAssignmentTarget::Off => {
                        // Explicit clearing follows the captured normalized
                        // device state. Untouched OFF records stay lossless.
                        record.module_raw = 0xff;
                        record.parameter_raw = 0;
                        record.minimum = F32Bits::from_f32(0.);
                        record.maximum = F32Bits::from_f32(0.);
                    }
                    ExpressionAssignmentTarget::Parameter(t) => {
                        record.module_raw = t.module.get();
                        record.parameter_raw = u16::from(t.parameter.get());
                    }
                }
                if let AssignmentEdit::ExpressionTargetAndRange {
                    minimum, maximum, ..
                } = edit
                {
                    record.minimum = *minimum;
                    record.maximum = *maximum;
                }
            }
            AssignmentEdit::ExpressionRange {
                slot,
                minimum,
                maximum,
            } => {
                let record = &mut unique(&mut self.expressions, |r| r.slot == *slot)?.record;
                record.minimum = *minimum;
                record.maximum = *maximum;
            }
            AssignmentEdit::CtrlModules { slot, modules } => {
                let record = &mut unique(&mut self.ctrls, |r| r.slot == *slot)?.record;
                let mask = modules
                    .iter()
                    .fold(0, |mask, module| mask | (1u32 << module.get()));
                record.module_mask_raw =
                    (record.module_mask_raw & !CtrlAssignmentRecord::MODULE_MASK) | mask;
            }
            AssignmentEdit::CtrlState { slot, enabled } => {
                unique(&mut self.ctrls, |r| r.slot == *slot)?
                    .record
                    .state_raw = u8::from(*enabled);
            }
            AssignmentEdit::QuickTarget { index, target } => {
                let record = &mut unique(&mut self.quicks, |r| r.index == *index)?.record;
                let (module, parameter) = match target {
                    QuickAssignmentTarget::Off => (0xff, 0),
                    QuickAssignmentTarget::PatchVolume => (11, 0),
                    QuickAssignmentTarget::PatchTempo => (12, 0),
                    QuickAssignmentTarget::Parameter(t) => {
                        (t.module.get(), u16::from(t.parameter.get()))
                    }
                };
                record.module_raw = module;
                record.parameter_raw = parameter;
            }
        }
        Ok(())
    }

    /// Obtain the one full native record after a validated edit. Does not open
    /// a device and does not substitute a missing/duplicate assignment record.
    pub fn edited_record(
        snapshot: &PresetSnapshot,
        edit: &AssignmentEdit,
    ) -> Result<RawRecord, ServiceError> {
        let mut next = Self::from_raw(&snapshot.raw);
        next.apply(&snapshot.view.modules, edit)?;
        match edit {
            AssignmentEdit::FxLoopPositions {
                send_boundary,
                return_boundary,
            } => {
                let retained =
                    &snapshot.raw.records()[next.routing.as_ref().unwrap().raw_record_index];
                gp200_protocol::fx_loop_positions_record(retained, *send_boundary, *return_boundary)
                    .map_err(|e| ServiceError::invalid(e.to_string()))
            }
            AssignmentEdit::ExpressionTarget { slot, .. }
            | AssignmentEdit::ExpressionTargetAndRange { slot, .. }
            | AssignmentEdit::ExpressionRange { slot, .. } => {
                Ok(unique(&mut next.expressions, |r| r.slot == *slot)?
                    .record
                    .to_record())
            }
            AssignmentEdit::CtrlModules { slot, .. } | AssignmentEdit::CtrlState { slot, .. } => {
                Ok(unique(&mut next.ctrls, |r| r.slot == *slot)?
                    .record
                    .to_record())
            }
            AssignmentEdit::QuickTarget { index, .. } => {
                Ok(unique(&mut next.quicks, |r| r.index == *index)?
                    .record
                    .to_record())
            }
        }
    }

    pub fn resolve_expression(
        record: &ExpressionAssignmentRecord,
        modules: &[ModuleSnapshot],
    ) -> ResolvedAssignmentReference {
        resolve_parameter(record.module_raw, record.parameter_raw, modules)
    }
    pub fn resolve_quick(
        record: &QuickAssignmentRecord,
        modules: &[ModuleSnapshot],
    ) -> ResolvedAssignmentReference {
        match record.module_raw {
            11 => ResolvedAssignmentReference::PatchVolume,
            12 => ResolvedAssignmentReference::PatchTempo,
            _ => resolve_parameter(record.module_raw, record.parameter_raw, modules),
        }
    }

    pub(crate) fn encode_into(
        &self,
        original: &Self,
        records: &mut [RawRecord],
    ) -> Result<(), ServiceError> {
        if self.routing != original.routing {
            let (Some(new), Some(old)) = (&self.routing, &original.routing) else {
                return Err(ServiceError::invalid(
                    "Cannot add/remove retained FX routing record",
                ));
            };
            if new.raw_record_index != old.raw_record_index {
                return Err(ServiceError::invalid(
                    "Cannot move retained FX routing record",
                ));
            }
            let retained = records
                .get_mut(old.raw_record_index)
                .ok_or_else(|| ServiceError::invalid("FX routing record absent"))?;
            if retained.command != 8
                || retained.body.len() != 16
                || retained.body[2] != old.record.send_boundary
                || retained.body[3] != old.record.return_boundary
            {
                return Err(ServiceError::invalid("Retained FX routing record changed"));
            }
            *retained = gp200_protocol::fx_loop_positions_record(
                retained,
                new.record.send_boundary,
                new.record.return_boundary,
            )
            .map_err(|e| ServiceError::invalid(e.to_string()))?;
        }
        // Retain the original record positions. A dense vector must never shift
        // an OFF slot or erase an unknown/malformed TLV from the snapshot.
        encode_entries(
            &self.expressions,
            &original.expressions,
            records,
            |r| r.to_record(),
            |a, b| a.slot == b.slot,
        )?;
        // A current read may change only CTRL's native-skipped padding. Keep
        // the chosen raw before-image's bytes when encoding an edited draft;
        // never overwrite its opaque padding using readback provenance. This
        // local copy leaves the draft, history and both raw captures untouched.
        // Identity, record position and unknown mask bits are still guarded.
        let mut ctrls = self.ctrls.clone();
        for (new, old) in ctrls.iter_mut().zip(&original.ctrls) {
            new.record.retained_padding = old.record.retained_padding;
        }
        encode_entries(
            &ctrls,
            &original.ctrls,
            records,
            |r| r.to_record(),
            |a, b| {
                a.slot == b.slot
                    && a.retained_padding == b.retained_padding
                    && (a.module_mask_raw & !CtrlAssignmentRecord::MODULE_MASK)
                        == (b.module_mask_raw & !CtrlAssignmentRecord::MODULE_MASK)
            },
        )?;
        encode_entries(
            &self.quicks,
            &original.quicks,
            records,
            |r| r.to_record(),
            |a, b| a.index == b.index,
        )?;
        Ok(())
    }
}

fn unique<T>(
    entries: &mut [IndexedAssignment<T>],
    predicate: impl Fn(&T) -> bool,
) -> Result<&mut IndexedAssignment<T>, ServiceError> {
    let mut matches = entries.iter_mut().filter(|e| predicate(&e.record));
    let found = matches
        .next()
        .ok_or_else(|| ServiceError::invalid("Assignment record absent"))?;
    if matches.next().is_some() {
        return Err(ServiceError::invalid("Assignment identity is ambiguous"));
    }
    Ok(found)
}
fn validate_target(
    modules: &[ModuleSnapshot],
    target: &ParameterAssignmentTarget,
) -> Result<(), ServiceError> {
    let mut matches = modules.iter().filter(|m| m.module == target.module);
    let module = matches
        .next()
        .ok_or_else(|| ServiceError::invalid("Assignment module absent"))?;
    if matches.next().is_some()
        || module.model != target.expected_model
        || module.parameter(target.parameter).is_none()
    {
        return Err(ServiceError::invalid(
            "Assignment dependency does not match the current module/model/parameter",
        ));
    }
    Ok(())
}
fn resolve_parameter(
    module_raw: u8,
    parameter_raw: u16,
    modules: &[ModuleSnapshot],
) -> ResolvedAssignmentReference {
    if module_raw == 0xff {
        return ResolvedAssignmentReference::Off;
    }
    if let (Ok(module), Ok(parameter)) = (
        ModuleId::new(module_raw),
        u8::try_from(parameter_raw)
            .ok()
            .and_then(|id| ParamId::new(id).ok())
            .ok_or(()),
    ) {
        let mut matches = modules.iter().filter(|m| m.module == module);
        if let Some(current) = matches.next()
            && matches.next().is_none()
            && current.parameter(parameter).is_some()
        {
            return ResolvedAssignmentReference::Parameter(ParameterAssignmentTarget {
                module,
                expected_model: current.model,
                parameter,
            });
        }
    }
    ResolvedAssignmentReference::Unresolved {
        module_raw,
        parameter_raw,
    }
}
fn encode_entries<T: PartialEq>(
    next: &[IndexedAssignment<T>],
    original: &[IndexedAssignment<T>],
    records: &mut [RawRecord],
    encode: impl Fn(&T) -> RawRecord,
    identity: impl Fn(&T, &T) -> bool,
) -> Result<(), ServiceError> {
    if next.len() != original.len() {
        return Err(ServiceError::invalid(
            "Cannot add/remove retained assignment records",
        ));
    }
    for (new, old) in next.iter().zip(original) {
        if new.raw_record_index != old.raw_record_index || !identity(&new.record, &old.record) {
            return Err(ServiceError::invalid(
                "Cannot move assignment identity or alter reserved bytes",
            ));
        }
        if new.record == old.record {
            continue;
        }
        let retained = records
            .get_mut(old.raw_record_index)
            .ok_or_else(|| ServiceError::invalid("Retained assignment record absent"))?;
        if *retained != encode(&old.record) {
            return Err(ServiceError::invalid(
                "Retained assignment structure changed",
            ));
        }
        let encoded = encode(&new.record);
        // Validate identity/body shape without granting a hardware write.
        gp200_protocol::patch_assignment_write_request(
            gp200_protocol::WireContext {
                group: 0,
                selector: 0,
            },
            &encoded,
        )
        .map_err(|error| ServiceError::invalid(error.to_string()))?;
        *retained = encoded;
    }
    Ok(())
}
