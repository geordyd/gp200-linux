//! Durable protected movement checkpoints. This module never opens a transport.
//! Saved journals are descriptive; reopening pauses every unfinished movement.
use crate::{
    BackupAcquisition, BackupOrigin, BundleLimits, ContentHash, DeviceBackup, LibraryStore,
    PresetInsertionBoundary, PresetMoveAction, PresetMovementObserved, PresetMovementOperation,
    PresetMovementPlan, PresetMovementReview, RestoreDispatch, RestoreEntryState, RestoreJournal,
    RestorePlan, RestoreReview, RestoreRunState, RestoreSource,
    device_backup::{check_snapshot, parse_raw, raw_bytes},
    store::{Manifest, decode_snapshot, invalid, io, read_bounded, reject_symlink},
    workflows::{decode_workflows, new_id, valid_workflow_id},
};
use gp200_domain::*;
use gp200_protocol::{GlobalRecords, RawTransfer, full_stored_write_request};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    sync::Arc,
};

pub const MOVEMENT_POLICY: &str = "gp200-inactive-movement-preservation-existing-2026-10-11-v1";
pub(crate) const MOVEMENT_EXTENSION: &str = "gp200-studio.preset-movement.v1";
const MAX_EXTENSION: usize = 8 * 1024 * 1024;
const MAX_ROUNDS: usize = 1024;
const MAX_FAILURES: usize = 128;

pub fn movement_policy_hash() -> ContentHash {
    ContentHash::of(include_bytes!(
        "fixtures/movement-preservation-policy-v1.json"
    ))
}
fn conflict(message: impl Into<String>) -> ServiceError {
    ServiceError::new(ErrorKind::Conflict, message)
}
fn same_device(a: &BackupOrigin, b: &BackupOrigin) -> bool {
    a.device_key == b.device_key
        && a.identity.model == b.identity.model
        && a.identity.mode == b.identity.mode
        && a.identity.firmware == b.identity.firmware
}
fn execution_hash(
    prepared: &ContentHash,
    plan: &ContentHash,
    archive: &ContentHash,
    descriptor: &ContentHash,
    profile: &ValidationProfileId,
) -> Result<ContentHash, ServiceError> {
    Ok(ContentHash::of(
        &serde_json::to_vec(&json!({
            "schema": "gp200-studio.movement-execution-review", "version": 1,
            "prepared_review_hash": prepared, "policy_id": MOVEMENT_POLICY,
            "policy_hash": movement_policy_hash(), "restore_plan_hash": plan,
            "before_archive_hash": archive, "before_descriptor_hash": descriptor,
            "operation_profile": profile,
        }))
        .map_err(invalid)?,
    ))
}
/// Checked against the unchanged prepared review and durable originals.
/// No deserializer or saved-journal conversion can create this review.
#[derive(Debug, Clone)]
pub struct MovementExecutionReview {
    prepared: PresetMovementReview,
    prepared_bytes: Vec<u8>,
    prepared_hash: ContentHash,
    execution_hash: ContentHash,
}
impl MovementExecutionReview {
    pub fn prepared(&self) -> &PresetMovementReview {
        &self.prepared
    }
    pub fn prepared_hash(&self) -> &ContentHash {
        &self.prepared_hash
    }
    pub fn execution_hash(&self) -> &ContentHash {
        &self.execution_hash
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MovementRunState {
    ReadyForFreshRound,
    InFlight { target: SlotId },
    PausedReconcileRequired { reason: String },
    PausedReconciled { report_hash: ContentHash },
    Completed { final_round_hash: ContentHash },
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MovementObject {
    Current,
    Globals,
    Stored(SlotId),
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MovementRawObservation {
    pub revision: Revision,
    pub raw_hash: ContentHash,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MovementRoundReceipt {
    pub schema: u32,
    pub execution_hash: ContentHash,
    pub previous: Option<ContentHash>,
    pub origin: BackupOrigin,
    pub policy_hash: ContentHash,
    pub expected_slots: BTreeSet<SlotId>,
    pub current: MovementRawObservation,
    pub globals: MovementRawObservation,
    pub stored: BTreeMap<SlotId, MovementRawObservation>,
    /// Passing means preserved under the pinned policy, not raw byte equality.
    pub preserved_under_policy: bool,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MovementReconciledSlot {
    Expected,
    Original,
    NeedsRetryReview,
    Conflict,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MovementReconciliation {
    pub journal: String,
    pub sequence: u64,
    pub execution_hash: ContentHash,
    pub origin: BackupOrigin,
    pub round_hash: ContentHash,
    pub rows: BTreeMap<SlotId, MovementReconciledSlot>,
    pub preservation_passed: bool,
}
impl MovementReconciliation {
    pub fn review_hash(&self) -> Result<ContentHash, ServiceError> {
        Ok(ContentHash::of(&serde_json::to_vec(self).map_err(invalid)?))
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MovementJournal {
    pub schema: u32,
    pub id: String,
    pub sequence: u64,
    pub original_origin: BackupOrigin,
    pub live_origin: BackupOrigin,
    pub profile: ValidationProfileId,
    pub execution_hash: ContentHash,
    pub prepared_hash: ContentHash,
    pub restore_plan_hash: ContentHash,
    pub policy_hash: ContentHash,
    pub original_backup: String,
    pub archive_basename: String,
    pub archive_hash: ContentHash,
    pub descriptor_hash: ContentHash,
    pub original_current: ContentHash,
    pub original_globals: ContentHash,
    pub original_slots: BTreeMap<SlotId, ContentHash>,
    pub sentinels: BTreeSet<SlotId>,
    pub rounds: Vec<ContentHash>,
    pub failed_rounds: Vec<ContentHash>,
    pub failed_actor_readbacks: Vec<ContentHash>,
    pub last_round: Option<ContentHash>,
    pub reconciliation: Option<ContentHash>,
    pub state: MovementRunState,
    restore: RestoreJournal,
}
impl MovementJournal {
    pub fn restore(&self) -> &RestoreJournal {
        &self.restore
    }
    pub fn is_completed(&self) -> bool {
        matches!(self.state, MovementRunState::Completed { .. })
    }
}
/// A durable library acknowledgement, not a device command or actor capability.
/// The guarded actor/runtime adapter is a separately implemented feature.
#[derive(Debug, Clone)]
pub struct MovementDispatch {
    journal: String,
    sequence: u64,
    execution_hash: ContentHash,
    preflight: ContentHash,
    observed: PresetMovementObserved,
    restore: RestoreDispatch,
}
impl MovementDispatch {
    pub fn journal(&self) -> &str {
        &self.journal
    }
    pub fn sequence(&self) -> u64 {
        self.sequence
    }
    pub fn execution_hash(&self) -> &ContentHash {
        &self.execution_hash
    }
    pub fn preflight_hash(&self) -> &ContentHash {
        &self.preflight
    }
    pub fn preflight(&self) -> &PresetMovementObserved {
        &self.observed
    }
    pub fn target(&self) -> SlotId {
        self.restore.target
    }
    pub fn epoch(&self) -> SessionEpoch {
        self.restore.epoch
    }
    pub fn expected_target_revision(&self) -> Revision {
        self.restore.expected_target_revision
    }
    pub fn snapshot(&self) -> &Arc<PresetSnapshot> {
        &self.restore.snapshot
    }
    pub fn expected_raw_hash(&self) -> &ContentHash {
        &self.restore.expected_raw_hash
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MovementByteDifference {
    /// Offset in the kind-prefixed complete raw frame.
    pub offset: usize,
    pub reference: Option<u8>,
    pub actual: Option<u8>,
    pub excluded_by_policy: bool,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MovementDifferenceReport {
    pub object: MovementObject,
    pub reference_raw_hash: ContentHash,
    pub actual_raw_hash: ContentHash,
    pub preserved_under_policy: bool,
    pub differences: Vec<MovementByteDifference>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MovementExtension {
    schema_version: u32,
    records: BTreeMap<String, MovementJournal>,
}
impl Default for MovementExtension {
    fn default() -> Self {
        Self {
            schema_version: 1,
            records: BTreeMap::new(),
        }
    }
}
pub(crate) fn decode_movement_extension(
    manifest: &Manifest,
) -> Result<MovementExtension, ServiceError> {
    let Some(value) = manifest.extensions.get(MOVEMENT_EXTENSION) else {
        return Ok(MovementExtension::default());
    };
    if serde_json::to_vec(value).map_err(invalid)?.len() > MAX_EXTENSION {
        return Err(invalid("Movement extension exceeds 8 MiB"));
    }
    let result: MovementExtension = serde_json::from_value(value.clone()).map_err(invalid)?;
    if result.schema_version != 1 {
        return Err(ServiceError::unsupported(
            "Unsupported movement extension schema; no rollback",
        ));
    }
    if result.records.len() > 128 {
        return Err(invalid("Too many movement journals"));
    }
    let ordinary = decode_workflows(manifest)?;
    let mut ids: BTreeSet<_> = result.records.keys().cloned().collect();
    for (id, j) in &result.records {
        j.original_origin.validate()?;
        j.live_origin.validate()?;
        j.restore.validate()?;
        if let RestoreRunState::Paused { reason } = &j.restore.state
            && (reason.is_empty() || reason.len() > 4096)
        {
            return Err(invalid("Invalid private movement pause reason"));
        }
        if id != &j.id
            || !valid_workflow_id(id)
            || !valid_workflow_id(&j.restore.id)
            || !ids.insert(j.restore.id.clone())
            || ordinary.restores.contains_key(&j.restore.id)
            || j.schema != 1
            || j.sequence == 0
            || j.profile.0.is_empty()
            || j.profile.0.len() > 128
            || !same_device(&j.original_origin, &j.live_origin)
            || j.restore.target != j.live_origin
            || j.original_slots.is_empty()
            || j.original_slots.len() > 256
            || j.sentinels.len() > 2
            || j.rounds.is_empty()
            || j.rounds.len() > MAX_ROUNDS
            || j.failed_rounds.len() > MAX_FAILURES
            || j.failed_actor_readbacks.len() > MAX_FAILURES
            || j.last_round.as_ref() != j.rounds.last()
            || j.policy_hash != movement_policy_hash()
            || j.restore.plan_hash != j.restore_plan_hash
            || !j.restore.excluded.is_empty()
            || j.restore.current_buffer_raw_hash != j.original_current
            || j.execution_hash
                != execution_hash(
                    &j.prepared_hash,
                    &j.restore_plan_hash,
                    &j.archive_hash,
                    &j.descriptor_hash,
                    &j.profile,
                )?
        {
            return Err(invalid("Invalid embedded movement journal binding"));
        }
        let in_flight: Vec<_> = j
            .restore
            .entries
            .iter()
            .filter(|e| e.state == RestoreEntryState::InFlight)
            .collect();
        let coherent = match &j.state {
            MovementRunState::ReadyForFreshRound => {
                j.restore.state == RestoreRunState::Ready
                    && j.restore.entries.iter().all(|e| {
                        matches!(
                            e.state,
                            RestoreEntryState::Planned | RestoreEntryState::Verified { .. }
                        )
                    })
            }
            MovementRunState::InFlight { target } => {
                j.restore.state == RestoreRunState::Ready
                    && in_flight.len() == 1
                    && in_flight[0].plan.selection.target == *target
            }
            MovementRunState::PausedReconcileRequired { reason } => {
                !reason.is_empty()
                    && reason.len() <= 4096
                    && matches!(j.restore.state, RestoreRunState::Paused { .. })
                    && in_flight.is_empty()
            }
            MovementRunState::PausedReconciled { report_hash } => {
                j.reconciliation.as_ref() == Some(report_hash)
                    && matches!(j.restore.state, RestoreRunState::Paused { .. })
                    && in_flight.is_empty()
            }
            MovementRunState::Completed { final_round_hash } => {
                j.last_round.as_ref() == Some(final_round_hash)
                    && j.restore.state == RestoreRunState::Completed
            }
        };
        if !coherent {
            return Err(invalid("Movement and private restore states disagree"));
        }
    }
    Ok(result)
}
struct Protection {
    original: BackupOrigin,
    profile: ValidationProfileId,
    floor: MovementRoundReceipt,
    current: Arc<PresetSnapshot>,
    globals: GlobalRecords,
    originals: BTreeMap<SlotId, Arc<PresetSnapshot>>,
    expected: BTreeMap<SlotId, Arc<PresetSnapshot>>,
    sentinels: BTreeSet<SlotId>,
}
fn prepared_value(review: &PresetMovementReview) -> Value {
    let m = review.movement();
    let b = review.before();
    let operation = match m.operation() {
        PresetMovementOperation::Insertion => "insertion".into(),
        PresetMovementOperation::Selection(action) => format!("selection-{action:?}"),
    };
    let steps: Vec<_> = m
        .insertion_steps()
        .iter()
        .map(|s| json!({"source":s.source,"boundary":s.boundary.get(),"destination":s.destination}))
        .collect();
    let mappings: Vec<_> = m
        .affected_mappings()
        .map(|m| json!({"source":m.source,"destination":m.destination}))
        .collect();
    let slots: Vec<_> = b.slots().iter().map(|s| json!({"role":format!("{:?}",s.role()),"capture":s.capture(),"observed_revision":s.observed_revision()})).collect();
    json!({"format":"gp200-studio.preset-movement-review","schema":1,"policy":"preserve-inactive-current-and-globals",
        "operation":operation,"steps":steps,"mappings":mappings,"original_selection":m.original_selection(),
        "final_selection":m.final_selection(),"current_before":m.current_before(),
        "original_reselection_destination_descriptive_only":m.current_after_original_reselection(),
        "backup":b.backup(),"origin":b.origin(),"operation_profile":b.profile(),"archive_hash":b.archive_hash(),
        "descriptor_hash":b.descriptor_hash(),"current":b.current(),"current_observed_revision":b.current_observed_revision(),
        "globals":b.globals(),"globals_observed_revision":b.globals_observed_revision(),"slots":slots,"restore":review.restore_plan()})
}
fn field<T: DeserializeOwned>(value: &Value, name: &str) -> Result<T, ServiceError> {
    serde_json::from_value(
        value
            .get(name)
            .ok_or_else(|| invalid(format!("Missing movement descriptor {name}")))?
            .clone(),
    )
    .map_err(invalid)
}
fn movement_from_value(v: &Value, model: DeviceModel) -> Result<PresetMovementPlan, ServiceError> {
    let selection: Vec<SlotId> = field(v, "original_selection")?;
    let current: Option<SlotId> = field(v, "current_before")?;
    let operation: String = field(v, "operation")?;
    let m = if operation == "insertion" {
        let steps: Vec<Value> = field(v, "steps")?;
        if selection.len() != 1 || steps.len() != 1 {
            return Err(invalid("Invalid insertion descriptor"));
        }
        let source: SlotId = field(&steps[0], "source")?;
        if selection[0] != source {
            return Err(invalid("Insertion selection disagrees"));
        }
        PresetMovementPlan::insert(
            model,
            source,
            PresetInsertionBoundary::new(model, field(&steps[0], "boundary")?).map_err(invalid)?,
            current,
        )
    } else {
        let action = match operation.as_str() {
            "selection-Top" => PresetMoveAction::Top,
            "selection-Bottom" => PresetMoveAction::Bottom,
            "selection-Previous" => PresetMoveAction::Previous,
            "selection-Next" => PresetMoveAction::Next,
            _ => return Err(invalid("Unknown movement operation")),
        };
        PresetMovementPlan::selected(model, &selection, action, current)
    }
    .map_err(invalid)?;
    let steps: Vec<_> = m
        .insertion_steps()
        .iter()
        .map(|s| json!({"source":s.source,"boundary":s.boundary.get(),"destination":s.destination}))
        .collect();
    let mappings: Vec<_> = m
        .affected_mappings()
        .map(|s| json!({"source":s.source,"destination":s.destination}))
        .collect();
    if m.is_noop()
        || v["steps"] != json!(steps)
        || v["mappings"] != json!(mappings)
        || v["original_selection"] != json!(m.original_selection())
        || v["final_selection"] != json!(m.final_selection())
        || v["original_reselection_destination_descriptive_only"]
            != json!(m.current_after_original_reselection())
    {
        return Err(invalid(
            "Movement descriptor is not the checked complete permutation",
        ));
    }
    Ok(m)
}
impl LibraryStore {
    fn movement_extension(&self) -> Result<MovementExtension, ServiceError> {
        decode_movement_extension(&self.manifest)
    }
    fn require_movement_writer(&self) -> Result<(), ServiceError> {
        if self.movement_publication_uncertain {
            return Err(ServiceError::new(
                ErrorKind::Indeterminate,
                "Library publication was uncertain; reopen before any new movement or manifest write",
            ));
        }
        Ok(())
    }
    fn commit_movement_extension(
        &mut self,
        extension: MovementExtension,
    ) -> Result<(), ServiceError> {
        self.require_movement_writer()?;
        let mut candidate = self.manifest.clone();
        candidate.extensions.insert(
            MOVEMENT_EXTENSION.into(),
            serde_json::to_value(extension).map_err(invalid)?,
        );
        self.validate_movement_manifest(&candidate)?;
        self.commit(candidate)
    }
    fn protection(
        &self,
        manifest: &Manifest,
        j: &MovementJournal,
    ) -> Result<Protection, ServiceError> {
        let workflows = decode_workflows(manifest)?;
        let backup = workflows
            .backups
            .get(&j.original_backup)
            .ok_or_else(|| invalid("Original movement backup missing from candidate manifest"))?;
        if backup.origin != j.original_origin
            || backup.acquisition != BackupAcquisition::Finished
            || !backup.read_errors.is_empty()
            || ContentHash::of(&serde_json::to_vec(backup).map_err(invalid)?) != j.descriptor_hash
        {
            return Err(invalid("Original movement backup descriptor changed"));
        }
        self.validate_backup_blobs(backup)?;
        self.validate_movement_archive(j, backup)?;
        let plan: RestorePlan =
            serde_json::from_slice(&self.read_blob(j.restore_plan_hash.as_str())?)
                .map_err(invalid)?;
        plan.validate()?;
        if plan.review_hash()? != j.restore_plan_hash
            || !plan.excluded.is_empty()
            || plan.target != j.original_origin
            || plan.current_buffer_raw_hash != j.original_current
            || plan.entries.len() != j.restore.entries.len()
            || plan
                .entries
                .iter()
                .zip(&j.restore.entries)
                .any(|(a, b)| a != &b.plan)
        {
            return Err(invalid("Complete immutable movement RestorePlan disagrees"));
        }
        let preimage = self.read_blob(j.prepared_hash.as_str())?;
        if preimage.len() > 2 * 1024 * 1024 {
            return Err(invalid("Movement review preimage exceeds bounds"));
        }
        let v: Value = serde_json::from_slice(&preimage).map_err(invalid)?;
        let allowed: BTreeSet<_> = [
            "format",
            "schema",
            "policy",
            "operation",
            "steps",
            "mappings",
            "original_selection",
            "final_selection",
            "current_before",
            "original_reselection_destination_descriptive_only",
            "backup",
            "origin",
            "operation_profile",
            "archive_hash",
            "descriptor_hash",
            "current",
            "current_observed_revision",
            "globals",
            "globals_observed_revision",
            "slots",
            "restore",
        ]
        .into_iter()
        .collect();
        if v.as_object()
            .is_none_or(|o| o.keys().map(String::as_str).collect::<BTreeSet<_>>() != allowed)
            || v["format"] != "gp200-studio.preset-movement-review"
            || v["schema"] != 1
            || v["policy"] != "preserve-inactive-current-and-globals"
            || v["backup"] != json!(j.original_backup)
            || v["origin"] != json!(j.original_origin)
            || v["operation_profile"] != json!(j.profile)
            || v["archive_hash"] != json!(j.archive_hash)
            || v["descriptor_hash"] != json!(j.descriptor_hash)
            || v["restore"] != serde_json::to_value(&plan).map_err(invalid)?
        {
            return Err(invalid("Original prepared movement preimage disagrees"));
        }
        let m = movement_from_value(&v, j.original_origin.identity.model)?;
        let interval: BTreeSet<_> = m
            .affected_interval()
            .expect("checked non-noop")
            .slots()
            .collect();
        let sentinels: BTreeSet<_> = m.sentinels().iter().copied().collect();
        let required: BTreeSet<_> = interval.union(&sentinels).copied().collect();
        if j.original_slots.keys().copied().collect::<BTreeSet<_>>() != required
            || j.sentinels != sentinels
            || plan
                .entries
                .iter()
                .zip(m.affected_mappings())
                .any(|(e, m)| {
                    e.selection.target != m.destination
                        || e.selection.source
                            != (RestoreSource::BackupStored {
                                backup: j.original_backup.clone(),
                                slot: m.source,
                            })
                })
            || plan.entries.len() != interval.len()
        {
            return Err(invalid(
                "Movement plan escaped the entire original interval",
            ));
        }
        let current_capture = backup
            .edit_buffer
            .as_ref()
            .ok_or_else(|| invalid("Original current absent"))?;
        let global_capture = backup
            .globals
            .as_ref()
            .ok_or_else(|| invalid("Original globals absent"))?;
        if v["current"] != json!(current_capture)
            || v["globals"] != json!(global_capture)
            || current_capture.raw_hash != j.original_current
            || global_capture.raw_hash != j.original_globals
        {
            return Err(invalid("Original current/global binding disagrees"));
        }
        let current = Arc::new(self.checked_captured_preset(
            &backup.origin,
            current_capture,
            PresetTarget::EditBuffer,
        )?);
        crate::native::require_encoded_projection(&current)?;
        let globals =
            GlobalRecords::from_transfer(parse_raw(&self.read_blob(j.original_globals.as_str())?)?)
                .map_err(invalid)?;
        let mut originals = BTreeMap::new();
        let mut floor_stored = BTreeMap::new();
        let slots: Vec<Value> = field(&v, "slots")?;
        if slots.len() != required.len() {
            return Err(invalid("Original review slot set differs"));
        }
        for (slot, row) in required.iter().zip(slots.iter()) {
            let capture = backup
                .presets
                .get(slot)
                .ok_or_else(|| invalid("Original stored row absent"))?;
            let role = if sentinels.contains(slot) {
                "Sentinel"
            } else {
                "Interval"
            };
            let revision: Revision = field(row, "observed_revision")?;
            if row.as_object().is_none_or(|o| o.len() != 3)
                || row["capture"] != json!(capture)
                || row["role"] != role
                || revision < capture.revision
                || revision.0 == 0
                || j.original_slots.get(slot) != Some(&capture.raw_hash)
            {
                return Err(invalid("Original slot/capture/floor binding disagrees"));
            }
            let original = Arc::new(self.checked_captured_preset(
                &backup.origin,
                capture,
                PresetTarget::Stored(*slot),
            )?);
            crate::native::require_encoded_projection(&original)?;
            originals.insert(*slot, original);
            floor_stored.insert(
                *slot,
                MovementRawObservation {
                    revision,
                    raw_hash: capture.raw_hash.clone(),
                },
            );
        }
        let mut expected = BTreeMap::new();
        for e in &plan.entries {
            let RestoreSource::BackupStored { slot: source, .. } = e.selection.source else {
                return Err(invalid("Movement source is not original BackupStored"));
            };
            let before = decode_snapshot(&self.read_blob(e.before_snapshot_hash.as_str())?)?;
            check_snapshot(
                &j.original_origin,
                &before,
                PresetTarget::Stored(e.selection.target),
            )?;
            if before.revision != e.before_revision
                || raw_bytes(&before.raw) != raw_bytes(&originals[&e.selection.target].raw)
                || e.before_raw_hash != j.original_slots[&e.selection.target]
                || e.before_revision != floor_stored[&e.selection.target].revision
            {
                return Err(invalid("Movement target before image changed"));
            }
            let snapshot = Arc::new(decode_snapshot(
                &self.read_blob(e.expected_snapshot_hash.as_str())?,
            )?);
            check_snapshot(
                &j.original_origin,
                &snapshot,
                PresetTarget::Stored(e.selection.target),
            )?;
            if snapshot.revision != e.before_revision {
                return Err(invalid("Original expected movement revision changed"));
            }
            let wire = full_stored_write_request(e.selection.target.get(), &originals[&source].raw)
                .map_err(invalid)?;
            let mut records = vec![wire.records()[0].clone()];
            records.extend_from_slice(&wire.records()[2..]);
            let derived =
                RawTransfer::from_records(originals[&e.selection.target].raw.kind(), &records)
                    .map_err(invalid)?;
            if raw_bytes(&derived) != raw_bytes(&snapshot.raw)
                || ContentHash::of(&raw_bytes(&derived)) != e.expected_raw_hash
            {
                return Err(invalid(
                    "Expected movement payload is not its immutable original source",
                ));
            }
            expected.insert(e.selection.target, snapshot);
        }
        self.validate_restore_blobs(&j.restore)?;
        let current_floor: Revision = field(&v, "current_observed_revision")?;
        let globals_floor: Revision = field(&v, "globals_observed_revision")?;
        if current_floor < current_capture.revision
            || globals_floor < global_capture.revision
            || current_floor.0 == 0
            || globals_floor.0 == 0
        {
            return Err(invalid("Invalid original movement revision floors"));
        }
        let floor = MovementRoundReceipt {
            schema: 1,
            execution_hash: j.execution_hash.clone(),
            previous: None,
            origin: j.original_origin.clone(),
            policy_hash: movement_policy_hash(),
            expected_slots: BTreeSet::new(),
            current: MovementRawObservation {
                revision: current_floor,
                raw_hash: j.original_current.clone(),
            },
            globals: MovementRawObservation {
                revision: globals_floor,
                raw_hash: j.original_globals.clone(),
            },
            stored: floor_stored,
            preserved_under_policy: true,
        };
        Ok(Protection {
            original: j.original_origin.clone(),
            profile: j.profile.clone(),
            floor,
            current,
            globals,
            originals,
            expected,
            sentinels,
        })
    }
    fn validate_movement_archive(
        &self,
        j: &MovementJournal,
        backup: &DeviceBackup,
    ) -> Result<(), ServiceError> {
        let name = &j.archive_basename;
        if name.len() > 255
            || !name.ends_with(".gp200backup")
            || name.len() <= 12
            || !name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        {
            return Err(invalid(
                "Movement archive requires one bounded ASCII basename",
            ));
        }
        let directory = self.root.join("movement-before");
        reject_symlink(&directory)?;
        if !fs::metadata(&directory).map_err(io)?.is_dir()
            || fs::canonicalize(&directory).map_err(io)? != directory
        {
            return Err(invalid("Movement archive directory is not canonical"));
        }
        let path = directory.join(name);
        let limits = BundleLimits::default();
        let hash = ContentHash::of(&read_bounded(&path, limits.compressed_bytes)?);
        let reopened = Self::preview_device_backup(&path, limits)?;
        if hash != j.archive_hash
            || serde_json::to_vec(&reopened).map_err(invalid)?
                != serde_json::to_vec(backup).map_err(invalid)?
            || ContentHash::of(&read_bounded(&path, limits.compressed_bytes)?) != hash
        {
            return Err(invalid(
                "Original movement archive changed or disagrees after reopening",
            ));
        }
        Ok(())
    }
    pub(crate) fn validate_movement_manifest(
        &self,
        manifest: &Manifest,
    ) -> Result<(), ServiceError> {
        let extension = decode_movement_extension(manifest)?;
        for j in extension.records.values() {
            let p = self.protection(manifest, j)?;
            let mut previous = p.floor.clone();
            let mut previous_hash = None;
            let mut unique = BTreeSet::new();
            for hash in &j.rounds {
                if !unique.insert(hash.clone()) {
                    return Err(invalid("Duplicate movement round reference"));
                }
                let receipt = self.read_movement_receipt(hash)?;
                if receipt.previous != previous_hash || receipt.execution_hash != j.execution_hash {
                    return Err(invalid("Movement receipt chain disagrees"));
                }
                let observed = observed_from_receipt(self, &receipt, &p)?;
                require_fresh_shape(&observed, &p, &receipt.origin, &previous)?;
                if receipt.preserved_under_policy
                    != preserved_round(&observed, &p, &receipt.expected_slots)
                {
                    return Err(invalid(
                        "Movement receipt preservation claim disagrees with raw frames",
                    ));
                }
                previous = receipt;
                previous_hash = Some(hash.clone());
            }
            if previous.origin != j.live_origin
                || j.failed_rounds.iter().collect::<BTreeSet<_>>().len() != j.failed_rounds.len()
                || j.failed_rounds.iter().any(|h| !unique.contains(h))
            {
                return Err(invalid(
                    "Movement latest live origin or failed receipt references disagree",
                ));
            }
            for hash in &j.failed_rounds {
                if self.read_movement_receipt(hash)?.preserved_under_policy {
                    return Err(invalid("Failed receipt incorrectly claims preservation"));
                }
            }
            for hash in &j.failed_actor_readbacks {
                decode_snapshot(&self.read_blob(hash.as_str())?)?;
            }
            let verified: BTreeSet<_> = j
                .restore
                .entries
                .iter()
                .filter_map(|e| {
                    matches!(e.state, RestoreEntryState::Verified { .. })
                        .then_some(e.plan.selection.target)
                })
                .collect();
            for e in &j.restore.entries {
                if e.observed_revision.0 == 0 {
                    return Err(invalid("Movement entry has no observation revision"));
                }
                let observed = parse_raw(&self.read_blob(e.observed_raw_hash.as_str())?)?;
                check_raw_preset(
                    &observed,
                    &j.live_origin,
                    PresetTarget::Stored(e.plan.selection.target),
                    e.observed_revision,
                )?;
                if let RestoreEntryState::Verified { readback_raw_hash } = &e.state {
                    let actual = parse_raw(&self.read_blob(readback_raw_hash.as_str())?)?;
                    if !crate::stored_readback_equal(
                        &p.expected[&e.plan.selection.target].raw,
                        &actual,
                    ) {
                        return Err(invalid(
                            "Private movement Verified payload differs from immutable expected source",
                        ));
                    }
                }
            }
            match &j.state {
                MovementRunState::Completed { .. }
                    if !previous.preserved_under_policy
                        || previous.expected_slots != p.expected.keys().copied().collect() =>
                {
                    return Err(invalid(
                        "Movement completion lacks complete final preserved round",
                    ));
                }
                MovementRunState::ReadyForFreshRound | MovementRunState::InFlight { .. }
                    if !previous.preserved_under_policy || previous.expected_slots != verified =>
                {
                    return Err(invalid(
                        "Movement ready/in-flight state lacks its mixed-bank receipt",
                    ));
                }
                _ => {}
            }
            if let Some(hash) = &j.reconciliation {
                let report: MovementReconciliation =
                    serde_json::from_slice(&self.read_blob(hash.as_str())?).map_err(invalid)?;
                if report.journal != j.id
                    || report.execution_hash != j.execution_hash
                    || report.sequence == 0
                    || report.sequence > j.sequence
                    || !same_device(&report.origin, &j.original_origin)
                    || !unique.contains(&report.round_hash)
                    || report.rows.keys().ne(p.expected.keys())
                {
                    return Err(invalid("Invalid movement reconciliation binding"));
                }
                if matches!(j.state, MovementRunState::PausedReconciled { .. }) {
                    let round = self.read_movement_receipt(&report.round_hash)?;
                    let actual = observed_from_receipt(self, &round, &p)?;
                    if report.sequence != j.sequence
                        || report.origin != j.live_origin
                        || j.last_round.as_ref() != Some(&report.round_hash)
                        || report.rows != classify_rows(&actual, &p, &j.restore)
                        || report.preservation_passed != preserved_objects(&actual, &p)
                    {
                        return Err(invalid(
                            "Current reconciliation report disagrees with complete raw round",
                        ));
                    }
                }
            }
        }
        Ok(())
    }
    fn read_movement_receipt(
        &self,
        hash: &ContentHash,
    ) -> Result<MovementRoundReceipt, ServiceError> {
        let bytes = self.read_blob(hash.as_str())?;
        if bytes.len() > 256 * 1024 {
            return Err(invalid("Movement receipt exceeds 256 KiB"));
        }
        let receipt: MovementRoundReceipt = serde_json::from_slice(&bytes).map_err(invalid)?;
        if receipt.schema != 1
            || receipt.policy_hash != movement_policy_hash()
            || receipt.stored.len() > 256
            || receipt.expected_slots.len() > 256
        {
            return Err(invalid("Invalid complete movement receipt"));
        }
        Ok(receipt)
    }
    fn retain_movement_round(
        &self,
        j: &mut MovementJournal,
        fresh: &PresetMovementObserved,
        p: &Protection,
        expected: BTreeSet<SlotId>,
    ) -> Result<ContentHash, ServiceError> {
        if j.rounds.len() >= MAX_ROUNDS {
            return Err(invalid(
                "Movement receipt capacity exhausted; export and make a new review",
            ));
        }
        let pass = preserved_round(fresh, p, &expected);
        if !pass && j.failed_rounds.len() >= MAX_FAILURES {
            return Err(invalid(
                "Movement failure receipt capacity exhausted; export and make a new review",
            ));
        }
        let mut stored = BTreeMap::new();
        for (slot, snapshot) in &fresh.stored {
            stored.insert(
                *slot,
                MovementRawObservation {
                    revision: snapshot.revision,
                    raw_hash: ContentHash::new(self.publish_blob(&raw_bytes(&snapshot.raw))?)?,
                },
            );
        }
        let receipt = MovementRoundReceipt {
            schema: 1,
            execution_hash: j.execution_hash.clone(),
            previous: j.last_round.clone(),
            origin: fresh.origin.clone(),
            policy_hash: movement_policy_hash(),
            expected_slots: expected,
            current: MovementRawObservation {
                revision: fresh.current_buffer.revision,
                raw_hash: ContentHash::new(
                    self.publish_blob(&raw_bytes(&fresh.current_buffer.raw))?,
                )?,
            },
            globals: MovementRawObservation {
                revision: fresh.globals.revision,
                raw_hash: ContentHash::new(
                    self.publish_blob(&raw_bytes(fresh.globals.records().raw()))?,
                )?,
            },
            stored,
            preserved_under_policy: pass,
        };
        let bytes = serde_json::to_vec(&receipt).map_err(invalid)?;
        if bytes.len() > 256 * 1024 {
            return Err(invalid("Movement receipt exceeds bounds"));
        }
        let hash = ContentHash::new(self.publish_blob(&bytes)?)?;
        j.rounds.push(hash.clone());
        if !pass {
            j.failed_rounds.push(hash.clone());
        }
        j.last_round = Some(hash.clone());
        j.live_origin = fresh.origin.clone();
        Ok(hash)
    }
    fn movement_protection_checked(
        &mut self,
        j: &MovementJournal,
    ) -> Result<Protection, ServiceError> {
        match self.protection(&self.manifest, j) {
            Ok(p) => Ok(p),
            Err(error) => {
                // An invalid durable source cannot be used to publish a replacement manifest.
                self.movement_publication_uncertain = true;
                Err(error)
            }
        }
    }
    pub fn bind_preset_movement_execution(
        &self,
        prepared: &PresetMovementReview,
        reviewed: ContentHash,
    ) -> Result<MovementExecutionReview, ServiceError> {
        self.require_movement_writer()?;
        if prepared.review_hash()? != reviewed {
            return Err(conflict("Prepared movement changed since review"));
        }
        let bytes = serde_json::to_vec(&prepared_value(prepared)).map_err(invalid)?;
        if ContentHash::of(&bytes) != reviewed {
            return Err(invalid(
                "Reconstructed movement preimage differs from unchanged review_hash",
            ));
        }
        let b = prepared.before();
        let hash = execution_hash(
            &reviewed,
            &prepared.restore_plan().review_hash()?,
            b.archive_hash(),
            b.descriptor_hash(),
            b.profile(),
        )?;
        let execution = MovementExecutionReview {
            prepared: prepared.clone(),
            prepared_bytes: bytes,
            prepared_hash: reviewed,
            execution_hash: hash,
        };
        let prototype = self.movement_prototype(&execution)?;
        self.protection(&self.manifest, &prototype)?;
        Ok(execution)
    }
    fn movement_prototype(
        &self,
        execution: &MovementExecutionReview,
    ) -> Result<MovementJournal, ServiceError> {
        let b = execution.prepared.before();
        let archive_basename = b
            .archive()
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| invalid("Invalid movement archive basename"))?
            .to_owned();
        if b.archive() != self.root.join("movement-before").join(&archive_basename) {
            return Err(invalid("Movement archive escaped this library"));
        }
        let prepared_hash = ContentHash::new(self.publish_blob(&execution.prepared_bytes)?)?;
        let plan = execution.prepared.restore_plan().clone();
        let plan_hash = plan.review_hash()?;
        if ContentHash::new(self.publish_blob(&serde_json::to_vec(&plan).map_err(invalid)?)?)?
            != plan_hash
        {
            return Err(invalid("Movement plan publication changed"));
        }
        let mut restore = self.prepare_restore_journal(
            plan,
            RestoreReview {
                plan_hash: plan_hash.clone(),
                accept_unknown_firmware: false,
            },
        )?;
        restore.id = "movement-private-preview".into();
        Ok(MovementJournal {
            schema: 1,
            id: "movement-preview".into(),
            sequence: 1,
            original_origin: b.origin().clone(),
            live_origin: b.origin().clone(),
            profile: b.profile().clone(),
            execution_hash: execution.execution_hash.clone(),
            prepared_hash,
            restore_plan_hash: plan_hash,
            policy_hash: movement_policy_hash(),
            original_backup: b.backup().into(),
            archive_basename,
            archive_hash: b.archive_hash().clone(),
            descriptor_hash: b.descriptor_hash().clone(),
            original_current: b.current().raw_hash.clone(),
            original_globals: b.globals().raw_hash.clone(),
            original_slots: b
                .slots()
                .iter()
                .map(|s| (s.capture().slot, s.capture().raw_hash.clone()))
                .collect(),
            sentinels: execution
                .prepared
                .movement()
                .sentinels()
                .iter()
                .copied()
                .collect(),
            rounds: vec![],
            failed_rounds: vec![],
            failed_actor_readbacks: vec![],
            last_round: None,
            reconciliation: None,
            state: MovementRunState::ReadyForFreshRound,
            restore,
        })
    }
    pub fn begin_preset_movement(
        &mut self,
        execution: &MovementExecutionReview,
        reviewed: ContentHash,
        fresh: PresetMovementObserved,
    ) -> Result<MovementJournal, ServiceError> {
        self.require_movement_writer()?;
        if reviewed != execution.execution_hash
            || execution.prepared.review_hash()? != execution.prepared_hash
            || ContentHash::of(&execution.prepared_bytes) != execution.prepared_hash
        {
            return Err(conflict("Movement execution review changed"));
        }
        let mut extension = self.movement_extension()?;
        if extension.records.len() >= 128 {
            return Err(invalid("Movement journal capacity exhausted"));
        }
        let b = execution.prepared.before();
        let archive_basename = b
            .archive()
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| invalid("Invalid movement archive filename"))?
            .to_owned();
        if b.archive() != self.root.join("movement-before").join(&archive_basename) {
            return Err(invalid("Movement archive is outside this library"));
        }
        let prepared_hash = ContentHash::new(self.publish_blob(&execution.prepared_bytes)?)?;
        let plan = execution.prepared.restore_plan().clone();
        let plan_hash = plan.review_hash()?;
        if ContentHash::new(self.publish_blob(&serde_json::to_vec(&plan).map_err(invalid)?)?)?
            != plan_hash
        {
            return Err(invalid("Movement plan publication changed"));
        }
        let mut restore = self.prepare_restore_journal(
            plan,
            RestoreReview {
                plan_hash: plan_hash.clone(),
                accept_unknown_firmware: false,
            },
        )?;
        let ordinary = self.workflows()?;
        for _ in 0..16 {
            let id = new_id("movement-private-restore")?;
            if !ordinary.restores.contains_key(&id)
                && extension
                    .records
                    .values()
                    .all(|j| j.restore.id != id && j.id != id)
            {
                restore.id = id;
                break;
            }
        }
        if !restore.id.starts_with("movement-private-restore-") {
            return Err(ServiceError::new(
                ErrorKind::Busy,
                "Could not reserve private movement identity",
            ));
        }
        let id = new_id("movement")?;
        if extension.records.contains_key(&id)
            || extension.records.values().any(|j| j.restore.id == id)
        {
            return Err(conflict("Movement identity collision"));
        }
        let mut j = MovementJournal {
            schema: 1,
            id,
            sequence: 1,
            original_origin: b.origin().clone(),
            live_origin: b.origin().clone(),
            profile: b.profile().clone(),
            execution_hash: execution.execution_hash.clone(),
            prepared_hash,
            restore_plan_hash: plan_hash,
            policy_hash: movement_policy_hash(),
            original_backup: b.backup().into(),
            archive_basename,
            archive_hash: b.archive_hash().clone(),
            descriptor_hash: b.descriptor_hash().clone(),
            original_current: b.current().raw_hash.clone(),
            original_globals: b.globals().raw_hash.clone(),
            original_slots: b
                .slots()
                .iter()
                .map(|s| (s.capture().slot, s.capture().raw_hash.clone()))
                .collect(),
            sentinels: execution
                .prepared
                .movement()
                .sentinels()
                .iter()
                .copied()
                .collect(),
            rounds: vec![],
            failed_rounds: vec![],
            failed_actor_readbacks: vec![],
            last_round: None,
            reconciliation: None,
            state: MovementRunState::ReadyForFreshRound,
            restore,
        };
        let p = self.movement_protection_checked(&j)?;
        require_live_round(&fresh, &p, &j.live_origin, &p.floor)?;
        factory_only(&p)?;
        if !preserved_round(&fresh, &p, &BTreeSet::new()) {
            return Err(conflict(
                "Movement originals/current/globals/sentinels changed before Begin",
            ));
        }
        self.retain_movement_round(&mut j, &fresh, &p, BTreeSet::new())?;
        extension.records.insert(j.id.clone(), j.clone());
        self.commit_movement_extension(extension)?;
        Ok(j)
    }
    pub fn movement_journals(&self) -> Result<Vec<MovementJournal>, ServiceError> {
        Ok(self.movement_extension()?.records.into_values().collect())
    }
    pub fn movement_journal(&self, id: &str) -> Result<MovementJournal, ServiceError> {
        self.movement_extension()?
            .records
            .remove(id)
            .ok_or_else(|| invalid("Movement journal does not exist"))
    }
    pub fn begin_preset_movement_entry(
        &mut self,
        id: &str,
        sequence: u64,
        fresh: PresetMovementObserved,
    ) -> Result<MovementDispatch, ServiceError> {
        self.require_movement_writer()?;
        let mut extension = self.movement_extension()?;
        let j = extension
            .records
            .get_mut(id)
            .ok_or_else(|| invalid("Movement journal does not exist"))?;
        require_sequence(j, sequence)?;
        if j.state != MovementRunState::ReadyForFreshRound || j.failed_rounds.len() >= MAX_FAILURES
        {
            return Err(conflict(
                "Movement needs an explicit fresh reconciliation/review",
            ));
        }
        let p = self.movement_protection_checked(j)?;
        factory_only(&p)?;
        let prior = self.read_movement_receipt(j.last_round.as_ref().expect("checked receipt"))?;
        let expected = verified_slots(&j.restore)?;
        if let Err(error) = require_live_round(&fresh, &p, &j.live_origin, &prior) {
            pause_journal(j, &error.message)?;
            bump(j)?;
            self.commit_movement_extension(extension)?;
            return Err(error);
        }
        if !preserved_round(&fresh, &p, &expected) {
            self.retain_movement_round(j, &fresh, &p, expected)?;
            pause_journal(
                j,
                "Complete mixed bank or preserved objects changed before dispatch",
            )?;
            bump(j)?;
            self.commit_movement_extension(extension)?;
            return Err(conflict("Movement preflight changed; no dispatch returned"));
        }
        let target = j
            .restore
            .entries
            .iter()
            .find(|e| e.state == RestoreEntryState::Planned)
            .ok_or_else(|| conflict("No planned movement destination remains"))?
            .plan
            .selection
            .target;
        let preflight = self.retain_movement_round(j, &fresh, &p, expected)?;
        let mut restore = self.mark_restore_entry_in_flight(
            &mut j.restore,
            target,
            fresh.stored[&target].clone(),
            fresh.current_buffer.clone(),
        )?;
        // Rebind only this ephemeral comparison projection. Archived originals/plan stay exact.
        let mut snapshot = restore.snapshot.as_ref().clone();
        snapshot.source = source_for(&j.live_origin);
        snapshot.revision = restore.expected_target_revision;
        if ContentHash::of(&raw_bytes(&snapshot.raw)) != restore.expected_raw_hash {
            return Err(invalid("Live epoch rebinding changed immutable payload"));
        }
        restore.snapshot = Arc::new(snapshot);
        j.state = MovementRunState::InFlight { target };
        bump(j)?;
        let result = MovementDispatch {
            journal: j.id.clone(),
            sequence: j.sequence,
            execution_hash: j.execution_hash.clone(),
            preflight,
            observed: fresh,
            restore,
        };
        self.commit_movement_extension(extension)?;
        Ok(result)
    }
    pub fn confirm_preset_movement_entry(
        &mut self,
        id: &str,
        sequence: u64,
        actor_verified_target_readback: Arc<PresetSnapshot>,
        post: PresetMovementObserved,
    ) -> Result<MovementJournal, ServiceError> {
        self.require_movement_writer()?;
        let mut extension = self.movement_extension()?;
        let j = extension
            .records
            .get_mut(id)
            .ok_or_else(|| invalid("Movement journal does not exist"))?;
        require_sequence(j, sequence)?;
        let MovementRunState::InFlight { target } = j.state else {
            return Err(conflict("Movement has no matching in-flight checkpoint"));
        };
        let p = self.movement_protection_checked(j)?;
        let prior = self.read_movement_receipt(j.last_round.as_ref().expect("checked receipt"))?;
        let mut expected = verified_slots_except_inflight(&j.restore)?;
        expected.insert(target);
        let validation = require_live_round(&post, &p, &j.live_origin, &prior).and_then(|()| {
            check_snapshot(
                &j.live_origin,
                &actor_verified_target_readback,
                PresetTarget::Stored(target),
            )?;
            crate::native::require_encoded_projection(&actor_verified_target_readback)?;
            let entry = j
                .restore
                .entries
                .iter()
                .find(|e| e.plan.selection.target == target)
                .expect("checked target");
            if actor_verified_target_readback.revision <= entry.observed_revision
                || post.stored[&target].revision <= actor_verified_target_readback.revision
                || !crate::stored_readback_equal(
                    &p.expected[&target].raw,
                    &actor_verified_target_readback.raw,
                )
                || !preserved_round(&post, &p, &expected)
            {
                return Err(conflict(
                    "Movement target/readback or complete postflight preservation differs",
                ));
            }
            Ok(())
        });
        if let Err(error) = validation {
            if j.failed_actor_readbacks.len() >= MAX_FAILURES {
                return Err(invalid("Movement failed readback capacity exhausted"));
            }
            let raw_snapshot =
                serde_json::to_vec(actor_verified_target_readback.as_ref()).map_err(invalid)?;
            decode_snapshot(&raw_snapshot)?;
            j.failed_actor_readbacks
                .push(ContentHash::new(self.publish_blob(&raw_snapshot)?)?);
            if require_live_round(&post, &p, &j.live_origin, &prior).is_ok() {
                self.retain_movement_round(j, &post, &p, expected)?;
            }
            pause_journal(j, &error.message)?;
            bump(j)?;
            self.commit_movement_extension(extension)?;
            return Err(error);
        }
        let round = self.retain_movement_round(j, &post, &p, expected)?;
        self.apply_restore_readback(&mut j.restore, actor_verified_target_readback)?;
        let entry = j
            .restore
            .entries
            .iter_mut()
            .find(|e| e.plan.selection.target == target)
            .expect("checked target");
        entry.observed_revision = post.stored[&target].revision;
        entry.observed_raw_hash = ContentHash::of(&raw_bytes(&post.stored[&target].raw));
        j.state = if j.restore.state == RestoreRunState::Completed {
            MovementRunState::Completed {
                final_round_hash: round,
            }
        } else {
            MovementRunState::ReadyForFreshRound
        };
        bump(j)?;
        let result = j.clone();
        self.commit_movement_extension(extension)?;
        Ok(result)
    }
    pub fn pause_preset_movement(
        &mut self,
        id: &str,
        sequence: u64,
        reason: &str,
    ) -> Result<MovementJournal, ServiceError> {
        self.require_movement_writer()?;
        let mut extension = self.movement_extension()?;
        let j = extension
            .records
            .get_mut(id)
            .ok_or_else(|| invalid("Movement journal does not exist"))?;
        require_sequence(j, sequence)?;
        if j.is_completed() {
            return Err(conflict("Movement is already completed"));
        }
        pause_journal(j, reason)?;
        bump(j)?;
        let result = j.clone();
        self.commit_movement_extension(extension)?;
        Ok(result)
    }
    pub fn reconcile_preset_movement(
        &mut self,
        id: &str,
        sequence: u64,
        fresh: PresetMovementObserved,
    ) -> Result<MovementReconciliation, ServiceError> {
        self.require_movement_writer()?;
        let mut extension = self.movement_extension()?;
        let j = extension
            .records
            .get_mut(id)
            .ok_or_else(|| invalid("Movement journal does not exist"))?;
        require_sequence(j, sequence)?;
        if j.is_completed() || matches!(j.state, MovementRunState::InFlight { .. }) {
            return Err(conflict("Pause movement before read-only reconciliation"));
        }
        let p = self.movement_protection_checked(j)?;
        let prior = self.read_movement_receipt(j.last_round.as_ref().expect("checked receipt"))?;
        if !same_device(&j.original_origin, &fresh.origin) {
            return Err(conflict("Reconciliation device identity differs"));
        }
        require_live_round(&fresh, &p, &fresh.origin, &prior)?;
        let rows = classify_rows(&fresh, &p, &j.restore);
        let expected = rows
            .iter()
            .filter_map(|(slot, state)| {
                (*state == MovementReconciledSlot::Expected).then_some(*slot)
            })
            .collect();
        let round_hash = self.retain_movement_round(j, &fresh, &p, expected)?;
        self.apply_restore_reconcile(
            &mut j.restore,
            fresh.origin.clone(),
            fresh
                .stored
                .iter()
                .filter(|(s, _)| p.expected.contains_key(s))
                .map(|(s, a)| (*s, a.clone()))
                .collect(),
        )?;
        for entry in &mut j.restore.entries {
            if rows[&entry.plan.selection.target] == MovementReconciledSlot::Conflict {
                entry.state = RestoreEntryState::Conflict {
                    actual_raw_hash: entry.observed_raw_hash.clone(),
                };
            }
        }
        let preservation_passed = preserved_objects(&fresh, &p);
        bump(j)?;
        let report = MovementReconciliation {
            journal: j.id.clone(),
            sequence: j.sequence,
            execution_hash: j.execution_hash.clone(),
            origin: fresh.origin,
            round_hash: round_hash.clone(),
            rows,
            preservation_passed,
        };
        let hash =
            ContentHash::new(self.publish_blob(&serde_json::to_vec(&report).map_err(invalid)?)?)?;
        j.reconciliation = Some(hash.clone());
        if preservation_passed
            && report
                .rows
                .values()
                .all(|s| *s == MovementReconciledSlot::Expected)
        {
            j.state = MovementRunState::Completed {
                final_round_hash: round_hash,
            };
            j.restore.state = RestoreRunState::Completed;
        } else {
            j.state = MovementRunState::PausedReconciled { report_hash: hash };
            j.restore.state=RestoreRunState::Paused{reason:"Explicit report-bound Continue/Retry and another complete fresh round are required".into()};
        }
        self.commit_movement_extension(extension)?;
        Ok(report)
    }
    pub fn continue_preset_movement(
        &mut self,
        id: &str,
        reviewed_report: ContentHash,
        retry_targets: BTreeSet<SlotId>,
        fresh: PresetMovementObserved,
    ) -> Result<MovementJournal, ServiceError> {
        self.require_movement_writer()?;
        let mut extension = self.movement_extension()?;
        let j = extension
            .records
            .get_mut(id)
            .ok_or_else(|| invalid("Movement journal does not exist"))?;
        if j.state
            != (MovementRunState::PausedReconciled {
                report_hash: reviewed_report.clone(),
            })
            || j.failed_rounds.len() >= MAX_FAILURES
        {
            return Err(conflict(
                "Continue requires the current reconciliation report",
            ));
        }
        let report: MovementReconciliation =
            serde_json::from_slice(&self.read_blob(reviewed_report.as_str())?).map_err(invalid)?;
        let required: BTreeSet<_> = report
            .rows
            .iter()
            .filter_map(|(s, state)| {
                (*state == MovementReconciledSlot::NeedsRetryReview).then_some(*s)
            })
            .collect();
        if report.sequence != j.sequence
            || !report.preservation_passed
            || report
                .rows
                .values()
                .any(|s| *s == MovementReconciledSlot::Conflict)
            || retry_targets != required
        {
            return Err(conflict(
                "Continue requires preserved objects, no conflicts and the exact explicitly reviewed retry targets",
            ));
        }
        let p = self.movement_protection_checked(j)?;
        factory_only(&p)?;
        let prior = self.read_movement_receipt(&report.round_hash)?;
        require_live_round(&fresh, &p, &j.live_origin, &prior)?;
        if classify_rows(&fresh, &p, &j.restore) != report.rows || !preserved_objects(&fresh, &p) {
            return Err(conflict("Bank changed after reconciliation review"));
        }
        let expected = report
            .rows
            .iter()
            .filter_map(|(s, state)| (*state == MovementReconciledSlot::Expected).then_some(*s))
            .collect();
        self.retain_movement_round(j, &fresh, &p, expected)?;
        for entry in &mut j.restore.entries {
            if entry.state == RestoreEntryState::NeedsRetryReview {
                entry.state = RestoreEntryState::Planned;
            }
        }
        j.restore.state = RestoreRunState::Ready;
        j.state = MovementRunState::ReadyForFreshRound;
        bump(j)?;
        let result = j.clone();
        self.commit_movement_extension(extension)?;
        Ok(result)
    }
    pub(crate) fn recover_open_movements(&mut self) -> Result<(), ServiceError> {
        let mut extension = self.movement_extension()?;
        let mut changed = false;
        for j in extension.records.values_mut() {
            if !j.is_completed() {
                pause_journal(
                    j,
                    "Application reopened; read the complete bank and preserved objects before continuing. No write was replayed.",
                )?;
                bump(j)?;
                changed = true;
            }
        }
        if changed {
            self.commit_movement_extension(extension)?;
            self.recovery_note = Some(match self.recovery_note.take() {
                Some(n) => format!("{n}; unfinished movement paused for complete reconciliation"),
                None => {
                    "Unfinished movement paused for complete reconciliation; no write was replayed"
                        .into()
                }
            });
        }
        Ok(())
    }
    /// All differences for one object, bounded by the established raw frame bound.
    /// Exact original/actual frames remain content-addressed even when exclusions pass.
    pub fn movement_round_differences(
        &self,
        id: &str,
        round_hash: &ContentHash,
        object: MovementObject,
    ) -> Result<MovementDifferenceReport, ServiceError> {
        let j = self.movement_journal(id)?;
        if !j.rounds.contains(round_hash) {
            return Err(invalid("Receipt is not in this movement"));
        }
        let p = self.protection(&self.manifest, &j)?;
        let receipt = self.read_movement_receipt(round_hash)?;
        let (reference, actual_hash, globals, written) = match object {
            MovementObject::Current => (
                p.current.raw.as_ref(),
                receipt.current.raw_hash.clone(),
                false,
                false,
            ),
            MovementObject::Globals => (
                p.globals.raw(),
                receipt.globals.raw_hash.clone(),
                true,
                false,
            ),
            MovementObject::Stored(slot) => {
                let actual = receipt
                    .stored
                    .get(&slot)
                    .ok_or_else(|| invalid("Slot is outside this movement receipt"))?;
                let written = receipt.expected_slots.contains(&slot);
                let reference = if written {
                    p.expected.get(&slot)
                } else {
                    p.originals.get(&slot)
                }
                .ok_or_else(|| invalid("Reference slot absent"))?;
                (
                    reference.raw.as_ref(),
                    actual.raw_hash.clone(),
                    false,
                    written,
                )
            }
        };
        let actual = parse_raw(&self.read_blob(actual_hash.as_str())?)?;
        let a = raw_bytes(reference);
        let b = raw_bytes(&actual);
        let excluded = excluded_raw_offsets(reference, &actual, globals, written);
        let differences = (0..a.len().max(b.len()))
            .filter_map(|offset| {
                (a.get(offset) != b.get(offset)).then_some(MovementByteDifference {
                    offset,
                    reference: a.get(offset).copied(),
                    actual: b.get(offset).copied(),
                    excluded_by_policy: excluded.contains(&offset),
                })
            })
            .collect();
        let pass = if globals {
            GlobalRecords::from_transfer(actual)
                .map_err(invalid)?
                .control_state_equal(&p.globals)
        } else {
            preset_equal(reference, &actual, written)
        };
        Ok(MovementDifferenceReport {
            object,
            reference_raw_hash: ContentHash::of(&a),
            actual_raw_hash: actual_hash,
            preserved_under_policy: pass,
            differences,
        })
    }
}
fn source_for(origin: &BackupOrigin) -> SnapshotSource {
    match origin.identity.mode {
        BackendMode::Hardware => SnapshotSource::Hardware {
            epoch: origin.epoch,
        },
        _ => SnapshotSource::Simulator,
    }
}
fn check_raw_preset(
    raw: &RawTransfer,
    origin: &BackupOrigin,
    target: PresetTarget,
    revision: Revision,
) -> Result<Arc<PresetSnapshot>, ServiceError> {
    let snapshot = Arc::new(PresetSnapshot::from_raw(
        Arc::new(raw.clone()),
        source_for(origin),
        Some(target),
        revision,
    ));
    check_snapshot(origin, &snapshot, target)?;
    crate::native::require_encoded_projection(&snapshot)?;
    Ok(snapshot)
}
fn observed_from_receipt(
    store: &LibraryStore,
    r: &MovementRoundReceipt,
    p: &Protection,
) -> Result<PresetMovementObserved, ServiceError> {
    let current = check_raw_preset(
        &parse_raw(&store.read_blob(r.current.raw_hash.as_str())?)?,
        &r.origin,
        PresetTarget::EditBuffer,
        r.current.revision,
    )?;
    let globals = Arc::new(
        DeviceGlobalState::from_transfer(
            r.globals.revision,
            r.origin.epoch,
            parse_raw(&store.read_blob(r.globals.raw_hash.as_str())?)?,
            vec![],
        )
        .map_err(invalid)?,
    );
    let mut stored = BTreeMap::new();
    for (slot, row) in &r.stored {
        stored.insert(
            *slot,
            check_raw_preset(
                &parse_raw(&store.read_blob(row.raw_hash.as_str())?)?,
                &r.origin,
                PresetTarget::Stored(*slot),
                row.revision,
            )?,
        );
    }
    // Descriptive reconstruction only. Its read-only capabilities cannot authorize dispatch.
    Ok(PresetMovementObserved {
        origin: r.origin.clone(),
        current_buffer: current,
        globals,
        stored,
        capabilities: Capabilities::gp200_read_only(),
        expected_profile: p.profile.clone(),
    })
}
fn require_fresh_shape(
    fresh: &PresetMovementObserved,
    p: &Protection,
    live: &BackupOrigin,
    prior: &MovementRoundReceipt,
) -> Result<(), ServiceError> {
    fresh.origin.validate()?;
    if !same_device(&fresh.origin, &p.original)
        || &fresh.origin != live
        || fresh.expected_profile != p.profile
        || fresh.stored.keys().ne(p.originals.keys())
    {
        return Err(conflict(
            "Movement origin/profile or complete interval/sentinel set changed",
        ));
    }
    check_snapshot(live, &fresh.current_buffer, PresetTarget::EditBuffer)?;
    crate::native::require_encoded_projection(&fresh.current_buffer)?;
    if fresh.globals.epoch != live.epoch
        || !fresh.globals.records().is_complete()
        || fresh.current_buffer.revision.0 == 0
        || fresh.globals.revision.0 == 0
    {
        return Err(invalid(
            "Movement current/global round is incomplete or stale",
        ));
    }
    if prior.origin.epoch == live.epoch
        && (fresh.current_buffer.revision <= prior.current.revision
            || fresh.globals.revision <= prior.globals.revision)
    {
        return Err(conflict(
            "Movement round reused current/globals observations",
        ));
    }
    for (slot, snapshot) in &fresh.stored {
        check_snapshot(live, snapshot, PresetTarget::Stored(*slot))?;
        crate::native::require_encoded_projection(snapshot)?;
        if snapshot.revision.0 == 0
            || (prior.origin.epoch == live.epoch
                && prior
                    .stored
                    .get(slot)
                    .is_none_or(|old| snapshot.revision <= old.revision))
        {
            return Err(conflict("Movement round reused a stored observation"));
        }
    }
    Ok(())
}
fn require_live_round(
    fresh: &PresetMovementObserved,
    p: &Protection,
    live: &BackupOrigin,
    prior: &MovementRoundReceipt,
) -> Result<(), ServiceError> {
    require_fresh_shape(fresh, p, live, prior)?;
    if !fresh
        .capabilities
        .allows(OperationKind::PresetImport, &fresh.origin.identity)
        || !matches!(fresh.capabilities.operations.get(&OperationKind::PresetImport),Some(FeatureSupport::Verified{profile,..}) if profile==&p.profile)
    {
        return Err(ServiceError::new(
            ErrorKind::Unverified,
            "Matching live full-stored profile is unavailable",
        ));
    }
    Ok(())
}
fn preset_equal(a: &RawTransfer, b: &RawTransfer, written: bool) -> bool {
    (written || a.kind() == b.kind()) && crate::stored_readback_equal(a, b)
}
fn preserved_objects(f: &PresetMovementObserved, p: &Protection) -> bool {
    preset_equal(&p.current.raw, &f.current_buffer.raw, false)
        && p.current.context == f.current_buffer.context
        && p.globals.control_state_equal(f.globals.records())
        && p.sentinels
            .iter()
            .all(|s| preset_equal(&p.originals[s].raw, &f.stored[s].raw, false))
        && f.current_buffer
            .context
            .is_some_and(|c| c.group == 0 && !p.expected.keys().any(|s| s.get() == c.selector))
}
fn preserved_round(
    f: &PresetMovementObserved,
    p: &Protection,
    expected: &BTreeSet<SlotId>,
) -> bool {
    expected.is_subset(&p.expected.keys().copied().collect())
        && preserved_objects(f, p)
        && p.expected.keys().all(|s| {
            if expected.contains(s) {
                preset_equal(&p.expected[s].raw, &f.stored[s].raw, true)
            } else {
                preset_equal(&p.originals[s].raw, &f.stored[s].raw, false)
            }
        })
}
fn factory_only(p: &Protection) -> Result<(), ServiceError> {
    for snapshot in p.expected.values() {
        let raw_view = PresetSnapshot::from_raw(
            snapshot.raw.clone(),
            snapshot.source.clone(),
            snapshot.target,
            snapshot.revision,
        );
        if raw_view
            .asset_refs
            .iter()
            .any(|r| !matches!(r, AssetRef::Factory { .. }))
        {
            return Err(ServiceError::new(
                ErrorKind::Unverified,
                "Movement dispatch currently supports factory assets only; new private live asset proofs are required for user assets",
            ));
        }
    }
    Ok(())
}
fn verified_slots(j: &RestoreJournal) -> Result<BTreeSet<SlotId>, ServiceError> {
    if j.entries.iter().any(|e| {
        !matches!(
            e.state,
            RestoreEntryState::Planned | RestoreEntryState::Verified { .. }
        )
    }) {
        return Err(conflict(
            "Reconcile uncertain/retry/conflict rows before dispatch",
        ));
    }
    Ok(j.entries
        .iter()
        .filter_map(|e| {
            matches!(e.state, RestoreEntryState::Verified { .. }).then_some(e.plan.selection.target)
        })
        .collect())
}
fn verified_slots_except_inflight(j: &RestoreJournal) -> Result<BTreeSet<SlotId>, ServiceError> {
    if j.entries.iter().any(|e| {
        !matches!(
            e.state,
            RestoreEntryState::Planned
                | RestoreEntryState::Verified { .. }
                | RestoreEntryState::InFlight
        )
    }) {
        return Err(conflict("Movement has an unresolved row"));
    }
    Ok(j.entries
        .iter()
        .filter_map(|e| {
            matches!(e.state, RestoreEntryState::Verified { .. }).then_some(e.plan.selection.target)
        })
        .collect())
}
fn classify_rows(
    f: &PresetMovementObserved,
    p: &Protection,
    j: &RestoreJournal,
) -> BTreeMap<SlotId, MovementReconciledSlot> {
    j.entries
        .iter()
        .map(|e| {
            let s = e.plan.selection.target;
            let state = if preset_equal(&p.expected[&s].raw, &f.stored[&s].raw, true) {
                MovementReconciledSlot::Expected
            } else if preset_equal(&p.originals[&s].raw, &f.stored[&s].raw, false) {
                if e.attempts == 0 {
                    MovementReconciledSlot::Original
                } else {
                    MovementReconciledSlot::NeedsRetryReview
                }
            } else {
                MovementReconciledSlot::Conflict
            };
            (s, state)
        })
        .collect()
}
fn bump(j: &mut MovementJournal) -> Result<(), ServiceError> {
    j.sequence = j
        .sequence
        .checked_add(1)
        .ok_or_else(|| invalid("Movement sequence exhausted"))?;
    Ok(())
}
fn require_sequence(j: &MovementJournal, expected: u64) -> Result<(), ServiceError> {
    if j.sequence != expected {
        return Err(conflict("Movement acknowledgement sequence changed"));
    }
    Ok(())
}
fn pause_journal(j: &mut MovementJournal, reason: &str) -> Result<(), ServiceError> {
    LibraryStore::apply_restore_pause(&mut j.restore, reason)?;
    j.state = MovementRunState::PausedReconcileRequired {
        reason: reason.into(),
    };
    Ok(())
}
fn excluded_raw_offsets(
    a: &RawTransfer,
    b: &RawTransfer,
    globals: bool,
    written: bool,
) -> BTreeSet<usize> {
    let mut excluded = BTreeSet::new();
    if written {
        excluded.insert(0);
    }
    if a.records().len() != b.records().len() {
        return excluded;
    }
    let mut base = 1;
    for (ar, br) in a.records().iter().zip(b.records()) {
        if ar.command != br.command || ar.body.len() != br.body.len() {
            return BTreeSet::new();
        }
        let offsets: Vec<usize> = if globals {
            match (ar.command, ar.body.len()) {
                (0x1007, 72) => vec![54, 55, 56],
                (0x1002, 8) => vec![6, 7],
                (0x000a, 20) => {
                    let mut o = vec![2, 3];
                    let an = ar.body[4..].iter().position(|b| *b == 0);
                    let bn = br.body[4..].iter().position(|b| *b == 0);
                    if an == bn
                        && let Some(n) = an
                    {
                        o.extend(5 + n..20);
                    }
                    o
                }
                _ => vec![],
            }
        } else {
            match (ar.command, ar.body.len()) {
                (2, 88) | (8, 16) => vec![15],
                (0x0f, 8) => vec![2, 3],
                _ => vec![],
            }
        };
        excluded.extend(offsets.into_iter().map(|o| base + 4 + o));
        base += 4 + ar.body.len();
    }
    excluded
}
#[cfg(test)]
#[path = "preset_movement_journal_tests.rs"]
pub(crate) mod tests;
