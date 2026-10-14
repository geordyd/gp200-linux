use crate::*;
use gp200_protocol::F32Bits;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::Arc;

/// Retained signed byte; imported values outside the native slider survive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PatchPan(i8);
impl PatchPan {
    pub const fn from_raw(value: i8) -> Self {
        Self(value)
    }
    pub const fn raw(self) -> i8 {
        self.0
    }
    pub fn new(value: i16) -> Result<Self, ServiceError> {
        if !(-100..=100).contains(&value) {
            return Err(ServiceError::invalid("Patch Pan must be -100..100"));
        }
        Ok(Self(value as i8))
    }
    pub fn validate(self) -> Result<(), ServiceError> {
        Self::new(i16::from(self.0)).map(|_| ())
    }
}

/// Native numeric style is separate from a library's freeform style tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct NativeStyleId(u16);
impl NativeStyleId {
    pub const fn from_raw(value: u16) -> Self {
        Self(value)
    }
    pub const fn raw(self) -> u16 {
        self.0
    }
    pub fn new(value: u16) -> Result<Self, ServiceError> {
        if value >= 20 {
            return Err(ServiceError::invalid("Native style ID must be 0..19"));
        }
        Ok(Self(value))
    }
    pub fn validate(self) -> Result<(), ServiceError> {
        Self::new(self.0).map(|_| ())
    }
    /// Static factory names; custom names require the observed global records.
    pub fn label(self, globals: Option<&RawTransfer>) -> String {
        const FACTORY: [&str; 12] = [
            "-", "Metal", "World", "Indie", "Country", "Rock", "Funk", "Pop", "Blues", "Jazz",
            "Bass", "Acoustic",
        ];
        if let Some(name) = FACTORY.get(usize::from(self.0)) {
            return (*name).into();
        }
        if (12..20).contains(&self.0) {
            let matches: Vec<_> = globals
                .into_iter()
                .flat_map(|raw| raw.records())
                .filter(|r| {
                    r.command == 10
                        && r.body.len() == 20
                        && u16::from_le_bytes([r.body[0], r.body[1]]) == self.0
                })
                .collect();
            if matches.len() == 1 {
                let bytes = &matches[0].body[4..20];
                let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
                if let Ok(name) = std::str::from_utf8(&bytes[..end])
                    && !name.is_empty()
                {
                    return name.into();
                }
            }
            return format!("Custom style {} (name not read)", self.0 - 11);
        }
        format!("Unknown native style ID {}", self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PatchFxLoopMode(u8);
impl PatchFxLoopMode {
    pub const fn from_raw(value: u8) -> Self {
        Self(value)
    }
    pub const fn raw(self) -> u8 {
        self.0
    }
    pub const fn label(self) -> &'static str {
        match self.0 {
            0 => "Series",
            1 => "Parallel",
            _ => "Unknown mode",
        }
    }
    pub fn new(value: u8) -> Result<Self, ServiceError> {
        if value > 1 {
            return Err(ServiceError::invalid(
                "FX loop mode must be Series0 or Parallel1",
            ));
        }
        Ok(Self(value))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct MetadataPatch {
    pub name: Option<String>,
    pub author: Option<String>,
    pub style: Option<String>,
    pub notes: Option<String>,
    pub tempo: Option<u16>,
    pub level: Option<u16>,
    pub pan: Option<F32Bits>,
    pub native_pan: Option<PatchPan>,
    pub native_style: Option<NativeStyleId>,
    pub fx_send_level: Option<u16>,
    pub fx_return_level: Option<u16>,
    pub fx_loop_mode: Option<PatchFxLoopMode>,
}
impl MetadataPatch {
    pub fn validate_native(&self) -> Result<(), ServiceError> {
        if let Some(value) = self.native_pan {
            value.validate()?;
        }
        if let Some(value) = self.native_style {
            value.validate()?;
        }
        for value in [self.fx_send_level, self.fx_return_level]
            .into_iter()
            .flatten()
        {
            if value > 100 {
                return Err(ServiceError::invalid(
                    "FX loop Send/Return level must be 0..100",
                ));
            }
        }
        if let Some(value) = self.fx_loop_mode {
            PatchFxLoopMode::new(value.raw())?;
        }
        Ok(())
    }
    /// Local annotations never enter an incremental device request.
    pub fn device_fields(&self) -> Self {
        Self {
            author: None,
            style: None,
            notes: None,
            pan: None,
            ..self.clone()
        }
    }
    pub fn overlay(&mut self, newer: &Self) {
        macro_rules! merge { ($($field:ident),+) => { $(if newer.$field.is_some() { self.$field = newer.$field.clone(); })+ }; }
        merge!(
            name,
            author,
            style,
            notes,
            tempo,
            level,
            pan,
            native_pan,
            native_style,
            fx_send_level,
            fx_return_level,
            fx_loop_mode
        );
    }
    /// A narrow empirical profile, independent of the broader static codec.
    pub fn hardware_profile(&self, baseline: &MetadataPatch) -> Result<(), ServiceError> {
        self.validate_native()?;
        if self.author.is_some()
            || self.style.is_some()
            || self.notes.is_some()
            || self.pan.is_some()
        {
            return Err(ServiceError::new(
                ErrorKind::Unverified,
                "Local annotations are not device metadata edits",
            ));
        }
        if self.native_pan.is_some_and(|value| {
            ![0, 1].contains(&value.raw())
                || baseline
                    .native_pan
                    .is_none_or(|old| ![0, 1].contains(&old.raw()))
        }) {
            return Err(ServiceError::new(
                ErrorKind::Unverified,
                "Hardware Patch Pan currently supports the verified 0/1 pair",
            ));
        }
        if self.native_style.is_some_and(|value| {
            ![0, 5].contains(&value.raw())
                || baseline
                    .native_style
                    .is_none_or(|old| ![0, 5].contains(&old.raw()))
        }) {
            return Err(ServiceError::new(
                ErrorKind::Unverified,
                "Hardware native style currently supports the verified IDs0/5 pair",
            ));
        }
        if self.fx_send_level.is_some()
            || self.fx_return_level.is_some()
            || self.fx_loop_mode.is_some()
        {
            return Err(ServiceError::new(
                ErrorKind::Unverified,
                "FX loop levels/mode await a reversible hardware profile",
            ));
        }
        Ok(())
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssignmentTarget {
    pub module: ModuleId,
    pub model: ModelCode,
    pub parameter: ParamId,
    pub minimum: F32Bits,
    pub maximum: F32Bits,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExpressionPatch {
    pub controller: u8,
    pub bank: u8,
    pub targets: Vec<AssignmentTarget>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FootswitchPatch {
    pub switch: u8,
    pub tap_action: String,
    pub hold_action: String,
    pub modules: Vec<ModuleId>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuickControlPatch {
    pub index: u8,
    pub target: Option<AssignmentTarget>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum EditPatch {
    SetParameter {
        module: ModuleId,
        model: ModelCode,
        model_index: ModelIndex,
        parameter: ParamId,
        value: F32Bits,
    },
    /// The native editor writes Power first, then its bound numeric value.
    /// Initially synced snapshots cannot supply an earlier ordinary rate.
    SetSync {
        module: ModuleId,
        model: ModelCode,
        model_index: ModelIndex,
        power_parameter: ParamId,
        value_parameter: ParamId,
        enabled: bool,
        ordinary_restore: Option<F32Bits>,
    },
    SetEnabled {
        module: ModuleId,
        enabled: bool,
    },
    SetModel {
        module: ModuleId,
        model: ModelCode,
    },
    MoveModule {
        module: ModuleId,
        before: Option<ModuleId>,
    },
    SetMetadata(MetadataPatch),
    SetExpression(ExpressionPatch),
    SetFootswitch(FootswitchPatch),
    SetQuickControl(QuickControlPatch),
    SetPatchAssignment(AssignmentEdit),
}
impl EditPatch {
    pub fn operation(&self) -> OperationKind {
        match self {
            Self::SetParameter { .. } | Self::SetSync { .. } => OperationKind::SetParameter,
            Self::SetEnabled { .. } => OperationKind::SetModuleEnabled,
            Self::SetModel { .. } => OperationKind::SetModel,
            Self::MoveModule { .. } => OperationKind::SetChain,
            Self::SetMetadata(_) => OperationKind::SetMetadata,
            Self::SetExpression(_) => OperationKind::SetExpression,
            Self::SetFootswitch(_) => OperationKind::SetFootswitch,
            Self::SetQuickControl(_) => OperationKind::SetQuickControl,
            Self::SetPatchAssignment(edit) => match edit {
                AssignmentEdit::FxLoopPositions { .. } => OperationKind::SetChain,
                AssignmentEdit::ExpressionTarget { .. }
                | AssignmentEdit::ExpressionTargetAndRange { .. }
                | AssignmentEdit::ExpressionRange { .. } => OperationKind::SetExpression,
                AssignmentEdit::CtrlModules { .. } | AssignmentEdit::CtrlState { .. } => {
                    OperationKind::SetFootswitch
                }
                AssignmentEdit::QuickTarget { .. } => OperationKind::SetQuickControl,
            },
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PresetDraft {
    pub view: PresetView,
    pub metadata: MetadataPatch,
    pub expressions: Vec<ExpressionPatch>,
    pub footswitches: Vec<FootswitchPatch>,
    pub quick_controls: Vec<QuickControlPatch>,
}
impl PresetDraft {
    /// Compare the editor's retained values, independently of read provenance.
    ///
    /// Parameters (including undeclared slots and NaN payloads), raw state bytes,
    /// slot counts, unknown assignment bits and diagnostics remain exact. Only
    /// recovered-field evidence, record-position provenance and the two proven
    /// volatile CTRL padding bytes are excluded. `view` metadata/legacy vectors
    /// and metadata name/tempo/level are synchronized mirrors; their canonical
    /// values are the top-level metadata/vectors and recovered view fields.
    /// Nothing is normalized or removed from the actual draft or raw snapshot.
    pub fn semantically_equal(&self, other: &Self) -> bool {
        self.view.modules.len() == other.view.modules.len()
            && self
                .view
                .modules
                .iter()
                .zip(&other.view.modules)
                .all(|(a, b)| {
                    a.module == b.module
                        && a.state_raw == b.state_raw
                        && a.slot_count_raw == b.slot_count_raw
                        && a.model == b.model
                        && a.parameters == b.parameters
                })
            && self.view.chain.value() == other.view.chain.value()
            && self.view.name.value() == other.view.name.value()
            && self.view.tempo.value() == other.view.tempo.value()
            && self.view.level.value() == other.view.level.value()
            && self.metadata.author == other.metadata.author
            && self.metadata.style == other.metadata.style
            && self.metadata.notes == other.metadata.notes
            && self.metadata.pan == other.metadata.pan
            && self.metadata.native_pan == other.metadata.native_pan
            && self.metadata.native_style == other.metadata.native_style
            && self.metadata.fx_send_level == other.metadata.fx_send_level
            && self.metadata.fx_return_level == other.metadata.fx_return_level
            && self.metadata.fx_loop_mode == other.metadata.fx_loop_mode
            && self.expressions == other.expressions
            && self.footswitches == other.footswitches
            && self.quick_controls == other.quick_controls
            && self
                .view
                .patch_assignments
                .semantically_equal(&other.view.patch_assignments)
            && self.view.patch_assignments.diagnostics.len()
                == other.view.patch_assignments.diagnostics.len()
            && self
                .view
                .patch_assignments
                .diagnostics
                .iter()
                .zip(&other.view.patch_assignments.diagnostics)
                .all(|(a, b)| a.command == b.command && a.description == b.description)
    }

    pub fn from_snapshot(snapshot: &PresetSnapshot) -> Self {
        let mut view = snapshot.view.clone();
        // Older native-library revisions predate the assignment projection.
        // Their retained wire records remain the source for migrating it.
        if view.patch_assignments == PatchAssignments::default() {
            view.patch_assignments = PatchAssignments::from_raw(&snapshot.raw);
        }
        // Old local JSON predates the native metadata projection. Retained bytes
        // fill missing fields without changing existing annotations or raw IDs.
        let recovered = PresetSnapshot::from_raw(
            snapshot.raw.clone(),
            snapshot.source.clone(),
            snapshot.target,
            snapshot.revision,
        );
        macro_rules! migrate { ($($field:ident),+) => { $(if view.metadata.$field.is_none() { view.metadata.$field = recovered.view.metadata.$field; })+ }; }
        migrate!(
            native_pan,
            native_style,
            fx_send_level,
            fx_return_level,
            fx_loop_mode
        );
        Self {
            metadata: view.metadata.clone(),
            view,
            expressions: snapshot.view.expressions.clone(),
            footswitches: snapshot.view.footswitches.clone(),
            quick_controls: snapshot.view.quick_controls.clone(),
        }
    }
    pub fn apply(&mut self, patch: &EditPatch) -> Result<(), ServiceError> {
        match patch {
            EditPatch::SetPatchAssignment(edit) => self
                .view
                .patch_assignments
                .apply(&self.view.modules, edit)?,
            EditPatch::SetSync {
                module,
                model,
                power_parameter,
                value_parameter,
                enabled,
                ordinary_restore,
                ..
            } => {
                let value = if *enabled {
                    F32Bits::from_f32(4.0)
                } else {
                    ordinary_restore.ok_or_else(|| {
                        ServiceError::invalid("Sync Off requires an explicit ordinary value")
                    })?
                };
                let current = self
                    .view
                    .modules
                    .iter_mut()
                    .find(|m| m.module == *module)
                    .ok_or_else(|| ServiceError::invalid("Module absent from snapshot"))?;
                if current.model != *model
                    || power_parameter == value_parameter
                    || !value.as_f32().is_finite()
                    || current.parameter(*power_parameter).is_none()
                    || current.parameter(*value_parameter).is_none()
                {
                    return Err(ServiceError::invalid(
                        "Sync binding or retained value is invalid",
                    ));
                }
                current.parameters[usize::from(power_parameter.get())] =
                    F32Bits::from_f32(if *enabled { 1.0 } else { 0.0 });
                current.parameters[usize::from(value_parameter.get())] = value;
            }
            EditPatch::SetParameter {
                module,
                model,
                parameter,
                value,
                ..
            } => {
                let m = self
                    .view
                    .modules
                    .iter_mut()
                    .find(|m| m.module == *module)
                    .ok_or_else(|| ServiceError::invalid("Module absent from snapshot"))?;
                if m.model != *model {
                    return Err(ServiceError::invalid(
                        "Parameter model does not match current module",
                    ));
                }
                let slot = m
                    .parameters
                    .get_mut(parameter.get() as usize)
                    .ok_or_else(|| {
                        ServiceError::invalid("Parameter ID outside retained wire slots")
                    })?;
                *slot = *value;
            }
            EditPatch::SetEnabled { module, enabled } => {
                self.view
                    .modules
                    .iter_mut()
                    .find(|m| m.module == *module)
                    .ok_or_else(|| ServiceError::invalid("Module absent from snapshot"))?
                    .state_raw = u8::from(*enabled);
            }
            EditPatch::SetModel { module, model } => {
                self.view
                    .modules
                    .iter_mut()
                    .find(|m| m.module == *module)
                    .ok_or_else(|| ServiceError::invalid("Module absent from snapshot"))?
                    .model = *model;
            }
            EditPatch::MoveModule { module, before } => {
                let mut chain = self
                    .view
                    .chain
                    .value()
                    .cloned()
                    .unwrap_or_else(|| ModuleId::all().collect());
                if before == &Some(*module) {
                    return Ok(());
                }
                chain.retain(|m| m != module);
                let index = before
                    .and_then(|before| chain.iter().position(|m| *m == before))
                    .unwrap_or(chain.len());
                chain.insert(index, *module);
                self.view.chain = Recovered::Candidate {
                    value: chain,
                    evidence: Vec::new(),
                };
            }
            EditPatch::SetMetadata(metadata) => {
                metadata.validate_native()?;
                macro_rules! merge {
                    ($field:ident) => {
                        if metadata.$field.is_some() {
                            self.metadata.$field = metadata.$field.clone();
                        }
                    };
                }
                merge!(name);
                merge!(author);
                merge!(style);
                merge!(notes);
                merge!(tempo);
                merge!(level);
                merge!(pan);
                merge!(native_pan);
                merge!(native_style);
                merge!(fx_send_level);
                merge!(fx_return_level);
                merge!(fx_loop_mode);
                if let Some(name) = &metadata.name {
                    self.view.name = Recovered::Candidate {
                        value: name.clone(),
                        evidence: Vec::new(),
                    };
                }
                if let Some(tempo) = metadata.tempo {
                    self.view.tempo = Recovered::Candidate {
                        value: tempo,
                        evidence: Vec::new(),
                    };
                }
                if let Some(level) = metadata.level {
                    self.view.level = Recovered::Candidate {
                        value: level,
                        evidence: Vec::new(),
                    };
                }
            }
            EditPatch::SetExpression(value) => {
                self.expressions
                    .retain(|v| (v.controller, v.bank) != (value.controller, value.bank));
                self.expressions.push(value.clone());
            }
            EditPatch::SetFootswitch(value) => {
                self.footswitches.retain(|v| v.switch != value.switch);
                self.footswitches.push(value.clone());
            }
            EditPatch::SetQuickControl(value) => {
                self.quick_controls.retain(|v| v.index != value.index);
                self.quick_controls.push(value.clone());
            }
        }
        self.view.metadata = self.metadata.clone();
        self.view.expressions = self.expressions.clone();
        self.view.footswitches = self.footswitches.clone();
        self.view.quick_controls = self.quick_controls.clone();
        Ok(())
    }
    /// Encode established module, chain, tempo, level and printable ASCII name
    /// fields while retaining every other byte. Unsupported name encodings and
    /// legacy assignment/local metadata edits remain explicit local projections; native
    /// export checks that its output can represent those projections.
    pub fn snapshot(
        &self,
        baseline: &PresetSnapshot,
        source: SnapshotSource,
        revision: Revision,
    ) -> Result<PresetSnapshot, ServiceError> {
        let raw = Arc::new(crate::codec::encode_draft_fields(self, baseline)?);
        let mut snapshot = baseline.clone();
        snapshot.raw = raw;
        snapshot.view = self.view.clone();
        snapshot.source = source;
        snapshot.revision = revision;
        snapshot.asset_refs = PresetSnapshot::from_raw(
            snapshot.raw.clone(),
            snapshot.source.clone(),
            snapshot.target,
            revision,
        )
        .asset_refs;
        for reference in &mut snapshot.asset_refs {
            if let AssetRef::UserSlot { kind, slot, content } = reference
                && let Some(AssetRef::UserSlot { content: original, .. }) = baseline.asset_refs.iter().find(|reference| matches!(reference, AssetRef::UserSlot { kind: original_kind, slot: original_slot, .. } if original_kind == kind && original_slot == slot)) {
                    *content = original.clone();
                }
        }
        Ok(snapshot)
    }
}
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct GestureKey {
    pub module: ModuleId,
    pub model: ModelCode,
    pub parameter: ParamId,
}
#[derive(Debug, Clone, Default)]
pub struct UndoHistory {
    pub past: Vec<PresetDraft>,
    pub future: Vec<PresetDraft>,
    pub gesture: Option<(GestureKey, PresetDraft)>,
}
#[derive(Debug, Clone)]
pub struct PendingEdit {
    pub patch: EditPatch,
    pub stage: EditStage,
    pub revision: Revision,
}
#[derive(Debug, Clone)]
pub struct EditConflict {
    pub intent: IntentId,
    pub description: String,
    pub actual: Arc<PresetSnapshot>,
}
#[derive(Debug, Clone)]
pub struct EditorDocument {
    pub id: DocumentId,
    pub target: EditTarget,
    pub baseline: Arc<PresetSnapshot>,
    pub draft: PresetDraft,
    pub local_revision: Revision,
    pub undo: UndoHistory,
    pub pending: BTreeMap<IntentId, PendingEdit>,
    pub conflicts: Vec<EditConflict>,
}
impl EditorDocument {
    pub fn new(id: DocumentId, target: EditTarget, baseline: Arc<PresetSnapshot>) -> Self {
        Self {
            id,
            target,
            draft: PresetDraft::from_snapshot(&baseline),
            local_revision: baseline.revision,
            baseline,
            undo: UndoHistory::default(),
            pending: BTreeMap::new(),
            conflicts: Vec::new(),
        }
    }
    pub fn dirty(&self) -> bool {
        !self
            .draft
            .semantically_equal(&PresetDraft::from_snapshot(&self.baseline))
    }
    pub fn apply(&mut self, patch: &EditPatch) -> Result<(), ServiceError> {
        let before = self.draft.clone();
        self.draft.apply(patch)?;
        if before != self.draft {
            if self.undo.gesture.is_none() {
                self.undo.past.push(before);
            }
            self.undo.future.clear();
            self.local_revision.0 += 1;
        }
        Ok(())
    }
    pub fn begin_gesture(&mut self, key: GestureKey) {
        if self.undo.gesture.is_none() {
            self.undo.gesture = Some((key, self.draft.clone()));
        }
    }
    pub fn end_gesture(&mut self, key: &GestureKey) {
        if self
            .undo
            .gesture
            .as_ref()
            .is_some_and(|(active, _)| active == key)
            && let Some((_, before)) = self.undo.gesture.take()
            && before != self.draft
        {
            self.undo.past.push(before);
        }
    }
    pub fn undo(&mut self) -> bool {
        if let Some(draft) = self.undo.past.pop() {
            self.undo
                .future
                .push(std::mem::replace(&mut self.draft, draft));
            self.local_revision.0 += 1;
            true
        } else {
            false
        }
    }
    pub fn redo(&mut self) -> bool {
        if let Some(draft) = self.undo.future.pop() {
            self.undo
                .past
                .push(std::mem::replace(&mut self.draft, draft));
            self.local_revision.0 += 1;
            true
        } else {
            false
        }
    }
    pub fn discard(&mut self) {
        self.draft = PresetDraft::from_snapshot(&self.baseline);
        self.undo = UndoHistory::default();
        self.local_revision.0 += 1;
    }
}
