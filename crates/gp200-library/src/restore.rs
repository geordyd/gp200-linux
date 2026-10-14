//! Reviewed, persistent restore planning. No method sends MIDI or approves a protocol capability.
use crate::{
    BackupOrigin, ContentHash, DeviceAssetSlot, DeviceBackup, LibraryStore, PresetDiff,
    compare_presets,
    device_backup::{check_snapshot, raw_bytes},
    store::{decode_snapshot, invalid},
    workflows::new_id,
};
use gp200_catalog::Catalog;
use gp200_domain::*;
use gp200_protocol::{RawTransfer, full_stored_write_request};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RestoreSource {
    BackupStored {
        backup: String,
        slot: SlotId,
    },
    /// A reviewed copy into a stored slot. It never writes the current buffer.
    BackupEditBuffer {
        backup: String,
    },
    LibraryVersion(LibraryVersionId),
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestoreSelection {
    pub source: RestoreSource,
    pub target: SlotId,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestoreAssetBinding {
    pub source: DeviceAssetSlot,
    pub target: DeviceAssetSlot,
    /// Exact device-ready content; a filename or metadata checksum is insufficient.
    pub content_hash: ContentHash,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetAssetProof {
    slot: DeviceAssetSlot,
    content_hash: ContentHash,
    epoch: SessionEpoch,
    device_key: String,
    profile: ValidationProfileId,
    mode: BackendMode,
}
impl TargetAssetProof {
    /// The caller must obtain `capabilities` from the trusted live profile registry and
    /// call this only after the actor confirms this transfer and readback. Imported
    /// evidence/metadata cannot deserialize a proof or establish these device bytes.
    pub fn from_verified_transfer(
        slot: DeviceAssetSlot,
        origin: &BackupOrigin,
        device_bytes: &[u8],
        capabilities: &Capabilities,
        expected_profile: &ValidationProfileId,
    ) -> Result<Self, ServiceError> {
        origin.validate()?;
        let operation = match slot.kind() {
            crate::DeviceAssetKind::Ir => OperationKind::IrWrite,
            crate::DeviceAssetKind::SnapTone => OperationKind::SnapToneWrite,
        };
        if !capabilities.allows(operation, &origin.identity)
            || !matches!(capabilities.operations.get(&operation), Some(FeatureSupport::Verified { profile, .. }) if profile == expected_profile)
        {
            return Err(ServiceError::new(
                ErrorKind::Unverified,
                "Matching live asset-transfer profile is unavailable; metadata/provenance claims do not recover content",
            ));
        }
        if expected_profile.0.is_empty()
            || expected_profile.0.len() > 128
            || match slot.kind() {
                crate::DeviceAssetKind::Ir => device_bytes.len() != 4096,
                crate::DeviceAssetKind::SnapTone => !matches!(device_bytes.len(), 8192 | 8840),
            }
        {
            return Err(invalid(
                "Device asset proof needs an established payload size and registered profile",
            ));
        }
        Ok(Self {
            slot,
            content_hash: ContentHash::of(device_bytes),
            epoch: origin.epoch,
            device_key: origin.device_key.clone(),
            profile: expected_profile.clone(),
            mode: origin.identity.mode,
        })
    }
    pub fn slot(&self) -> DeviceAssetSlot {
        self.slot
    }
    pub fn content_hash(&self) -> &ContentHash {
        &self.content_hash
    }
    pub fn profile(&self) -> &ValidationProfileId {
        &self.profile
    }
}
#[derive(Debug, Clone)]
pub struct RestoreRequest {
    pub target: BackupOrigin,
    pub current_buffer: Arc<PresetSnapshot>,
    pub selections: Vec<RestoreSelection>,
    pub observed_targets: BTreeMap<SlotId, Arc<PresetSnapshot>>,
    pub asset_bindings: Vec<RestoreAssetBinding>,
    /// A worker supplies proofs only after an independently verified upload/readback.
    pub target_assets: Vec<TargetAssetProof>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestoreExclusion {
    pub selection: RestoreSelection,
    pub reasons: Vec<String>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RestorePlanEntry {
    pub selection: RestoreSelection,
    pub before_revision: Revision,
    pub before_snapshot_hash: ContentHash,
    pub before_raw_hash: ContentHash,
    pub expected_snapshot_hash: ContentHash,
    pub expected_raw_hash: ContentHash,
    pub differences: PresetDiff,
    pub compatibility_review_required: bool,
    pub notes: Vec<String>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RestorePlan {
    pub target: BackupOrigin,
    pub current_buffer_raw_hash: ContentHash,
    pub entries: Vec<RestorePlanEntry>,
    pub excluded: Vec<RestoreExclusion>,
    pub notes: Vec<String>,
}
impl RestorePlan {
    /// Review token covers mapping, preconditions, raw payload hashes and exclusions.
    pub fn review_hash(&self) -> Result<ContentHash, ServiceError> {
        Ok(ContentHash::of(&serde_json::to_vec(self).map_err(invalid)?))
    }
    pub(crate) fn validate(&self) -> Result<(), ServiceError> {
        self.target.validate()?;
        if self.entries.len() + self.excluded.len() > 256
            || self.entries.is_empty()
            || self.notes.len() > 128
        {
            return Err(invalid(
                "Restore plan has no applicable entries or exceeds its bounds",
            ));
        }
        let mut targets = BTreeSet::new();
        for entry in &self.entries {
            if !targets.insert(entry.selection.target) || entry.notes.len() > 128 {
                return Err(invalid("Duplicate target or oversized restore entry"));
            }
        }
        for exclusion in &self.excluded {
            if exclusion.reasons.is_empty() || exclusion.reasons.len() > 128 {
                return Err(invalid("Invalid restore exclusion"));
            }
        }
        Ok(())
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoreReview {
    pub plan_hash: ContentHash,
    pub accept_unknown_firmware: bool,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RestoreEntryState {
    Planned,
    InFlight,
    Verified {
        readback_raw_hash: ContentHash,
    },
    Uncertain {
        reason: String,
    },
    /// Readback found the original state. Retrying still requires explicit review.
    NeedsRetryReview,
    Conflict {
        actual_raw_hash: ContentHash,
    },
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RestoreRunState {
    Ready,
    Paused { reason: String },
    Completed,
    Cancelled,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RestoreJournalEntry {
    pub plan: RestorePlanEntry,
    pub state: RestoreEntryState,
    pub attempts: u32,
    pub observed_revision: Revision,
    pub observed_raw_hash: ContentHash,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RestoreJournal {
    pub id: String,
    pub plan_hash: ContentHash,
    pub target: BackupOrigin,
    pub current_buffer_raw_hash: ContentHash,
    pub entries: Vec<RestoreJournalEntry>,
    pub excluded: Vec<RestoreExclusion>,
    pub state: RestoreRunState,
}
impl RestoreJournal {
    pub(crate) fn validate(&self) -> Result<(), ServiceError> {
        self.target.validate()?;
        if self.entries.is_empty()
            || self.entries.len() + self.excluded.len() > 256
            || self
                .entries
                .iter()
                .filter(|entry| entry.state == RestoreEntryState::InFlight)
                .count()
                > 1
        {
            return Err(invalid("Invalid restore journal entry count"));
        }
        let mut targets = BTreeSet::new();
        for entry in &self.entries {
            if !targets.insert(entry.plan.selection.target)
                || entry.plan.notes.len() > 128
                || matches!(&entry.state, RestoreEntryState::Uncertain { reason } if reason.len() > 4096)
            {
                return Err(invalid("Invalid restore journal entry"));
            }
            if entry.state == RestoreEntryState::InFlight && entry.attempts == 0 {
                return Err(invalid("In-flight entry has no persisted dispatch attempt"));
            }
        }
        if self.state == RestoreRunState::Completed
            && self
                .entries
                .iter()
                .any(|entry| !matches!(entry.state, RestoreEntryState::Verified { .. }))
        {
            return Err(invalid("Completed restore contains unverified entries"));
        }
        Ok(())
    }
    pub fn verified_count(&self) -> usize {
        self.entries
            .iter()
            .filter(|entry| matches!(entry.state, RestoreEntryState::Verified { .. }))
            .count()
    }
}
/// Send this typed snapshot through the normal validated actor, then confirm its fresh readback.
#[derive(Debug, Clone)]
pub struct RestoreDispatch {
    pub journal: String,
    pub epoch: SessionEpoch,
    pub target: SlotId,
    pub expected_target_revision: Revision,
    pub snapshot: Arc<PresetSnapshot>,
    pub expected_raw_hash: ContentHash,
}

impl LibraryStore {
    /// Dry run. Immutable before/expected blobs are published, but no journal or device write begins.
    pub fn plan_restore(&self, request: &RestoreRequest) -> Result<RestorePlan, ServiceError> {
        request.target.validate()?;
        check_snapshot(
            &request.target,
            &request.current_buffer,
            PresetTarget::EditBuffer,
        )?;
        if request.selections.is_empty()
            || request.selections.len() > 256
            || request.asset_bindings.len() > 30
            || request.target_assets.len() > 30
        {
            return Err(invalid(
                "Restore request exceeds bounds or has no selections",
            ));
        }
        let mut asset_sources = BTreeSet::new();
        for binding in &request.asset_bindings {
            if binding.source.kind() != binding.target.kind()
                || !asset_sources.insert(binding.source)
            {
                return Err(invalid(
                    "Asset bindings have mismatched kinds or duplicate sources",
                ));
            }
        }
        let mut asset_targets = BTreeSet::new();
        for proof in &request.target_assets {
            if !asset_targets.insert(proof.slot)
                || proof.epoch != request.target.epoch
                || proof.device_key != request.target.device_key
                || proof.mode != request.target.identity.mode
            {
                return Err(invalid(
                    "Duplicate, stale or wrong-device target asset proof",
                ));
            }
        }
        let active_slot = request
            .current_buffer
            .context
            .expect("checked current context")
            .selector;
        let mut seen = BTreeSet::new();
        for selection in &request.selections {
            if !seen.insert(selection.target) {
                return Err(invalid(
                    "Restore maps more than one source to a target slot",
                ));
            }
        }
        let mut plan = RestorePlan { target: request.target.clone(), current_buffer_raw_hash: ContentHash::of(&raw_bytes(&request.current_buffer.raw)), entries: Vec::new(), excluded: Vec::new(), notes: vec!["Only reviewed inactive stored slots are planned. The current edit buffer remains a separate object.".into(), "Globals and asset uploads are excluded from this preset transaction; metadata does not recover original WAV/NAM files.".into(), "Retained provenance is not hardware authorization. The device actor must independently validate bytes, session and the registered operation profile.".into()] };
        // Retain the current-buffer before image before any future consequential operation.
        self.publish_blob(&raw_bytes(&request.current_buffer.raw))?;
        let catalog = Catalog::embedded().map_err(invalid)?;
        for selection in &request.selections {
            let mut reasons = Vec::new();
            if selection.target.get() == active_slot {
                reasons.push("The current hardware profile writes inactive stored slots; the selected slot must be handled separately".into());
            }
            let before = request.observed_targets.get(&selection.target);
            if before.is_none() {
                reasons.push("Target slot has not been freshly read in this session".into());
            }
            if let Some(before) = before {
                check_snapshot(
                    &request.target,
                    before,
                    PresetTarget::Stored(selection.target),
                )?;
            }
            let (source, backup) = self.restore_source(&selection.source)?;
            if source.device_model != request.target.identity.model {
                reasons.push("Source and target device variants differ".into());
            }
            crate::native::require_encoded_projection(&source)?;
            let mut records = source.raw.records().to_vec();
            let source_view = PresetSnapshot::from_raw(
                source.raw.clone(),
                source.source.clone(),
                source.target,
                source.revision,
            );
            for reference in &source_view.asset_refs {
                let AssetRef::UserSlot { kind, slot, .. } = reference else {
                    continue;
                };
                let kind = match kind {
                    AssetKind::ImpulseResponse => crate::DeviceAssetKind::Ir,
                    AssetKind::SnapTone => crate::DeviceAssetKind::SnapTone,
                    _ => {
                        reasons.push("Unsupported user asset kind".into());
                        continue;
                    }
                };
                let source_slot = DeviceAssetSlot::new(kind, *slot)?;
                let known_hash = backup
                    .as_ref()
                    .and_then(|backup| backup.assets.get(&source_slot.key()))
                    .and_then(|asset| asset.content.as_ref())
                    .filter(|content| content.origin.is_verified())
                    .map(|content| &content.hash)
                    .cloned()
                    .or_else(|| {
                        source
                            .asset_refs
                            .iter()
                            .find_map(|reference| match reference {
                                AssetRef::UserSlot {
                                    kind: original_kind,
                                    slot: original_slot,
                                    content: Some(id),
                                } if *original_kind == kind.asset_kind()
                                    && original_slot == slot =>
                                {
                                    self.manifest
                                        .assets
                                        .get(id)
                                        .filter(|asset| asset.kind == *original_kind)
                                        .and_then(|asset| ContentHash::new(&asset.hash).ok())
                                }
                                _ => None,
                            })
                    });
                let binding = request
                    .asset_bindings
                    .iter()
                    .find(|binding| binding.source == source_slot);
                let Some(binding) = binding else {
                    reasons.push(format!(
                        "No reviewed content binding for {:?} slot {}",
                        kind, slot
                    ));
                    continue;
                };
                if binding.source.kind() != binding.target.kind()
                    || known_hash.as_ref() != Some(&binding.content_hash)
                {
                    reasons.push(format!("Source content for {:?} slot {} is unknown or disagrees with the reviewed hash", kind, slot));
                    continue;
                }
                if !request.target_assets.iter().any(|proof| {
                    proof.slot == binding.target && proof.content_hash == binding.content_hash
                }) {
                    reasons.push(format!(
                        "Target {:?} slot {} lacks independently verified matching content",
                        binding.target.kind(),
                        binding.target.index()
                    ));
                    continue;
                }
                if binding.target != source_slot {
                    for record in &mut records {
                        if record.command != 0x14 || record.body.len() != 68 {
                            continue;
                        }
                        let code =
                            u32::from_le_bytes(record.body[4..8].try_into().expect("model code"));
                        let old = match kind {
                            crate::DeviceAssetKind::Ir => 0x0a10_0000 + u32::from(*slot),
                            crate::DeviceAssetKind::SnapTone => 0x0f00_0000 + u32::from(*slot),
                        };
                        if code == old
                            && (kind != crate::DeviceAssetKind::Ir || record.body[0] == 5)
                        {
                            let new = match kind {
                                crate::DeviceAssetKind::Ir => 0x0a10_0000,
                                crate::DeviceAssetKind::SnapTone => 0x0f00_0000,
                            } + u32::from(binding.target.index());
                            record.body[4..8].copy_from_slice(&new.to_le_bytes());
                        }
                    }
                }
            }
            let raw = RawTransfer::from_records(source.raw.kind(), &records).map_err(invalid)?;
            let wire = match full_stored_write_request(selection.target.get(), &raw) {
                Ok(wire) => Some(wire),
                Err(error) => {
                    reasons.push(format!("Source record shape cannot be sent by the established full-preset codec: {error}"));
                    None
                }
            };
            if !reasons.is_empty() {
                plan.excluded.push(RestoreExclusion {
                    selection: selection.clone(),
                    reasons,
                });
                continue;
            }
            let before = before.expect("missing target is excluded");
            let wire = wire.expect("unsupported codec is excluded");
            let mut expected_records = vec![wire.records()[0].clone()];
            expected_records.extend_from_slice(&wire.records()[2..]);
            let expected_raw = Arc::new(
                RawTransfer::from_records(before.raw.kind(), &expected_records).map_err(invalid)?,
            );
            let mut expected = PresetSnapshot::from_raw(
                expected_raw,
                before.source.clone(),
                Some(PresetTarget::Stored(selection.target)),
                before.revision,
            );
            expected.firmware = request.target.identity.firmware.clone();
            let same_device = backup.as_ref().is_some_and(|backup| {
                backup.origin.device_key == request.target.device_key
                    && backup.origin.identity.mode == request.target.identity.mode
                    && backup.origin.identity.firmware == request.target.identity.firmware
            });
            let source_firmware = backup
                .as_ref()
                .and_then(|backup| backup.origin.identity.firmware.as_ref())
                .or(source.firmware.as_ref());
            let firmware_matches = source_firmware
                .zip(request.target.identity.firmware.as_ref())
                .is_some_and(|(a, b)| a.version == b.version);
            let compatibility_review_required = !same_device && !firmware_matches;
            let entry = RestorePlanEntry {
                selection: selection.clone(),
                before_revision: before.revision,
                before_snapshot_hash: ContentHash::new(
                    self.publish_blob(&serde_json::to_vec(before.as_ref()).map_err(invalid)?)?,
                )?,
                before_raw_hash: ContentHash::new(self.publish_blob(&raw_bytes(&before.raw))?)?,
                expected_snapshot_hash: ContentHash::new(
                    self.publish_blob(&serde_json::to_vec(&expected).map_err(invalid)?)?,
                )?,
                expected_raw_hash: ContentHash::new(self.publish_blob(&raw_bytes(&expected.raw))?)?,
                differences: compare_presets(before, &expected, &catalog),
                compatibility_review_required,
                notes: if compatibility_review_required {
                    vec!["Firmware equivalence is unknown; an explicit compatibility review is required before starting this plan".into()]
                } else {
                    vec![]
                },
            };
            plan.entries.push(entry);
        }
        // A completely excluded preview is still useful; creating a journal separately rejects it.
        Ok(plan)
    }
    fn restore_source(
        &self,
        source: &RestoreSource,
    ) -> Result<(Arc<PresetSnapshot>, Option<DeviceBackup>), ServiceError> {
        match source {
            RestoreSource::BackupStored { backup, slot } => Ok((
                self.load_backup_preset(backup, PresetTarget::Stored(*slot))?,
                Some(self.device_backup(backup)?),
            )),
            RestoreSource::BackupEditBuffer { backup } => Ok((
                self.load_backup_preset(backup, PresetTarget::EditBuffer)?,
                Some(self.device_backup(backup)?),
            )),
            RestoreSource::LibraryVersion(version) => {
                let version = self.load_version(version)?;
                if self
                    .manifest
                    .entries
                    .get(&version.library)
                    .is_none_or(|entry| entry.trashed)
                {
                    return Err(ServiceError::new(
                        ErrorKind::Conflict,
                        "Restore source is in trash or missing",
                    ));
                }
                Ok((version.snapshot, None))
            }
        }
    }
    /// Review is explicit; durable before-images already exist before the journal becomes dispatchable.
    pub fn begin_restore(
        &mut self,
        plan: RestorePlan,
        review: RestoreReview,
    ) -> Result<RestoreJournal, ServiceError> {
        let journal = self.prepare_restore_journal(plan, review)?;
        let mut workflows = self.workflows()?;
        workflows
            .restores
            .insert(journal.id.clone(), journal.clone());
        self.commit_workflows(workflows)?;
        Ok(journal)
    }
    pub(crate) fn prepare_restore_journal(
        &self,
        plan: RestorePlan,
        review: RestoreReview,
    ) -> Result<RestoreJournal, ServiceError> {
        plan.validate()?;
        if plan.review_hash()? != review.plan_hash {
            return Err(ServiceError::new(
                ErrorKind::Conflict,
                "Reviewed plan changed",
            ));
        }
        if !review.accept_unknown_firmware
            && plan
                .entries
                .iter()
                .any(|entry| entry.compatibility_review_required)
        {
            return Err(ServiceError::new(
                ErrorKind::Unverified,
                "Review unknown firmware compatibility before starting",
            ));
        }
        let journal = RestoreJournal {
            id: new_id("restore")?,
            plan_hash: review.plan_hash,
            target: plan.target,
            current_buffer_raw_hash: plan.current_buffer_raw_hash,
            entries: plan
                .entries
                .into_iter()
                .map(|plan| RestoreJournalEntry {
                    observed_revision: plan.before_revision,
                    observed_raw_hash: plan.before_raw_hash.clone(),
                    plan,
                    state: RestoreEntryState::Planned,
                    attempts: 0,
                })
                .collect(),
            excluded: plan.excluded,
            state: RestoreRunState::Ready,
        };
        self.validate_restore_blobs(&journal)?;
        Ok(journal)
    }
    pub fn restore_journal(&self, id: &str) -> Result<RestoreJournal, ServiceError> {
        self.workflows()?
            .restores
            .remove(id)
            .ok_or_else(|| invalid("Restore journal does not exist"))
    }
    pub fn restore_journals(&self) -> Result<Vec<RestoreJournal>, ServiceError> {
        Ok(self.workflows()?.restores.into_values().collect())
    }
    pub fn begin_restore_entry(
        &mut self,
        id: &str,
        target: SlotId,
        current: Arc<PresetSnapshot>,
        current_buffer: Arc<PresetSnapshot>,
    ) -> Result<RestoreDispatch, ServiceError> {
        let mut workflows = self.workflows()?;
        let journal = workflows
            .restores
            .get_mut(id)
            .ok_or_else(|| invalid("Restore journal does not exist"))?;
        let dispatch =
            self.mark_restore_entry_in_flight(journal, target, current, current_buffer)?;
        self.commit_workflows(workflows)?;
        Ok(dispatch)
    }
    pub(crate) fn mark_restore_entry_in_flight(
        &self,
        journal: &mut RestoreJournal,
        target: SlotId,
        current: Arc<PresetSnapshot>,
        current_buffer: Arc<PresetSnapshot>,
    ) -> Result<RestoreDispatch, ServiceError> {
        if journal.state != RestoreRunState::Ready
            || journal
                .entries
                .iter()
                .any(|entry| entry.state == RestoreEntryState::InFlight)
        {
            return Err(ServiceError::new(
                ErrorKind::Conflict,
                "Restore is paused, finished or already has an in-flight write",
            ));
        }
        check_snapshot(&journal.target, &current, PresetTarget::Stored(target))?;
        check_snapshot(&journal.target, &current_buffer, PresetTarget::EditBuffer)?;
        if current_buffer.context.expect("checked buffer").selector == target.get() {
            return Err(ServiceError::new(
                ErrorKind::Conflict,
                "Target became the selected patch after review; make a separate active-buffer plan",
            ));
        }
        let entry = journal
            .entries
            .iter_mut()
            .find(|entry| entry.plan.selection.target == target)
            .ok_or_else(|| invalid("Target is not in this restore plan"))?;
        let reviewed_before =
            decode_snapshot(&self.read_blob(entry.plan.before_snapshot_hash.as_str())?)?;
        if entry.state != RestoreEntryState::Planned
            || current.revision < entry.observed_revision
            || !stored_readback_equal(&reviewed_before.raw, &current.raw)
        {
            return Err(ServiceError::new(
                ErrorKind::Conflict,
                "Restore target changed, is uncertain or was already completed; refresh and reconcile",
            ));
        }
        let expected = Arc::new(decode_snapshot(
            &self.read_blob(entry.plan.expected_snapshot_hash.as_str())?,
        )?);
        let dispatch = RestoreDispatch {
            journal: journal.id.clone(),
            epoch: journal.target.epoch,
            target,
            expected_target_revision: current.revision,
            snapshot: expected,
            expected_raw_hash: entry.plan.expected_raw_hash.clone(),
        };
        // A fresh read advances actor revision and can change only observed padding.
        // Retain the actual preflight before publishing the dispatch checkpoint.
        entry.observed_raw_hash = ContentHash::new(self.publish_blob(&raw_bytes(&current.raw))?)?;
        entry.observed_revision = current.revision;
        entry.attempts = entry
            .attempts
            .checked_add(1)
            .ok_or_else(|| invalid("Restore attempt counter exhausted"))?;
        entry.state = RestoreEntryState::InFlight;
        Ok(dispatch)
    }
    pub fn confirm_restore_readback(
        &mut self,
        id: &str,
        actual: Arc<PresetSnapshot>,
    ) -> Result<RestoreJournal, ServiceError> {
        let mut workflows = self.workflows()?;
        let journal = workflows
            .restores
            .get_mut(id)
            .ok_or_else(|| invalid("Restore journal does not exist"))?;
        self.apply_restore_readback(journal, actual)?;
        let result = journal.clone();
        self.commit_workflows(workflows)?;
        Ok(result)
    }
    pub(crate) fn apply_restore_readback(
        &self,
        journal: &mut RestoreJournal,
        actual: Arc<PresetSnapshot>,
    ) -> Result<(), ServiceError> {
        let target = match actual.target {
            Some(PresetTarget::Stored(slot)) => slot,
            _ => {
                return Err(invalid(
                    "Restore confirmation requires a stored-slot readback",
                ));
            }
        };
        check_snapshot(&journal.target, &actual, PresetTarget::Stored(target))?;
        let entry = journal
            .entries
            .iter_mut()
            .find(|entry| entry.plan.selection.target == target)
            .ok_or_else(|| invalid("Target is not in this restore plan"))?;
        if entry.state != RestoreEntryState::InFlight {
            return Err(ServiceError::new(
                ErrorKind::Conflict,
                "No write is awaiting this confirmation",
            ));
        }
        if actual.revision <= entry.observed_revision {
            return Err(ServiceError::new(
                ErrorKind::Conflict,
                "Restore confirmation must be a fresh readback after the dispatch precondition",
            ));
        }
        let expected =
            decode_snapshot(&self.read_blob(entry.plan.expected_snapshot_hash.as_str())?)?;
        let hash = ContentHash::new(self.publish_blob(&raw_bytes(&actual.raw))?)?;
        if stored_readback_equal(&expected.raw, &actual.raw) {
            entry.state = RestoreEntryState::Verified {
                readback_raw_hash: hash,
            };
            if journal
                .entries
                .iter()
                .all(|entry| matches!(entry.state, RestoreEntryState::Verified { .. }))
            {
                journal.state = RestoreRunState::Completed;
            }
        } else {
            entry.state = RestoreEntryState::Conflict {
                actual_raw_hash: hash,
            };
            journal.state = RestoreRunState::Paused {
                reason: "Readback differs from the reviewed expected payload; no automatic replay"
                    .into(),
            };
        }
        Ok(())
    }
    pub fn pause_restore(
        &mut self,
        id: &str,
        reason: &str,
    ) -> Result<RestoreJournal, ServiceError> {
        let mut workflows = self.workflows()?;
        let journal = workflows
            .restores
            .get_mut(id)
            .ok_or_else(|| invalid("Restore journal does not exist"))?;
        Self::apply_restore_pause(journal, reason)?;
        let result = journal.clone();
        self.commit_workflows(workflows)?;
        Ok(result)
    }
    pub(crate) fn apply_restore_pause(
        journal: &mut RestoreJournal,
        reason: &str,
    ) -> Result<(), ServiceError> {
        if reason.is_empty() || reason.len() > 4096 {
            return Err(invalid("Invalid restore pause reason"));
        }
        if matches!(
            journal.state,
            RestoreRunState::Completed | RestoreRunState::Cancelled
        ) {
            return Err(ServiceError::new(
                ErrorKind::Conflict,
                "Restore is already closed",
            ));
        }
        for entry in &mut journal.entries {
            if entry.state == RestoreEntryState::InFlight {
                entry.state = RestoreEntryState::Uncertain {
                    reason: reason.into(),
                };
            }
        }
        journal.state = RestoreRunState::Paused {
            reason: reason.into(),
        };
        Ok(())
    }
    /// A new session must read every planned target. Applied uncertain writes become verified;
    /// unapplied uncertain writes require explicit retry review, and conflicts stay paused.
    pub fn reconcile_restore(
        &mut self,
        id: &str,
        origin: BackupOrigin,
        readbacks: BTreeMap<SlotId, Arc<PresetSnapshot>>,
    ) -> Result<RestoreJournal, ServiceError> {
        let mut workflows = self.workflows()?;
        let journal = workflows
            .restores
            .get_mut(id)
            .ok_or_else(|| invalid("Restore journal does not exist"))?;
        self.apply_restore_reconcile(journal, origin, readbacks)?;
        let result = journal.clone();
        self.commit_workflows(workflows)?;
        Ok(result)
    }
    pub(crate) fn apply_restore_reconcile(
        &self,
        journal: &mut RestoreJournal,
        origin: BackupOrigin,
        readbacks: BTreeMap<SlotId, Arc<PresetSnapshot>>,
    ) -> Result<(), ServiceError> {
        origin.validate()?;
        if matches!(
            journal.state,
            RestoreRunState::Completed | RestoreRunState::Cancelled
        ) {
            return Err(ServiceError::new(
                ErrorKind::Conflict,
                "Restore is already closed",
            ));
        }
        if origin.device_key != journal.target.device_key
            || origin.identity.model != journal.target.identity.model
            || origin.identity.mode != journal.target.identity.mode
            || origin.identity.firmware != journal.target.identity.firmware
        {
            return Err(ServiceError::new(
                ErrorKind::Conflict,
                "Reconnect identity/firmware differs from the reviewed target; make a new plan",
            ));
        }
        if readbacks.len() != journal.entries.len() {
            return Err(invalid(
                "Read every planned target before reconciling a restore",
            ));
        }
        for entry in &mut journal.entries {
            let target = entry.plan.selection.target;
            let actual = readbacks
                .get(&target)
                .ok_or_else(|| invalid("Missing target readback during reconciliation"))?;
            check_snapshot(&origin, actual, PresetTarget::Stored(target))?;
            let expected =
                decode_snapshot(&self.read_blob(entry.plan.expected_snapshot_hash.as_str())?)?;
            let before =
                decode_snapshot(&self.read_blob(entry.plan.before_snapshot_hash.as_str())?)?;
            let actual_hash = ContentHash::new(self.publish_blob(&raw_bytes(&actual.raw))?)?;
            if stored_readback_equal(&expected.raw, &actual.raw) {
                entry.state = RestoreEntryState::Verified {
                    readback_raw_hash: actual_hash.clone(),
                };
            } else if stored_readback_equal(&before.raw, &actual.raw) {
                entry.state = if entry.attempts == 0 {
                    RestoreEntryState::Planned
                } else {
                    RestoreEntryState::NeedsRetryReview
                };
            } else {
                entry.state = RestoreEntryState::Conflict {
                    actual_raw_hash: actual_hash.clone(),
                };
            }
            entry.observed_raw_hash = actual_hash;
            entry.observed_revision = actual.revision;
        }
        journal.target = origin;
        journal.state = reconciled_state(&journal.entries);
        Ok(())
    }
    /// Explicit retry choice after reconciliation proved the target still has its original state.
    pub fn approve_restore_retry(
        &mut self,
        id: &str,
        target: SlotId,
        observed_hash: ContentHash,
    ) -> Result<RestoreJournal, ServiceError> {
        let mut workflows = self.workflows()?;
        let journal = workflows
            .restores
            .get_mut(id)
            .ok_or_else(|| invalid("Restore journal does not exist"))?;
        let entry = journal
            .entries
            .iter_mut()
            .find(|entry| entry.plan.selection.target == target)
            .ok_or_else(|| invalid("Unknown restore target"))?;
        if entry.state != RestoreEntryState::NeedsRetryReview
            || entry.observed_raw_hash != observed_hash
        {
            return Err(ServiceError::new(
                ErrorKind::Conflict,
                "Retry requires the reviewed unchanged readback hash",
            ));
        }
        entry.state = RestoreEntryState::Planned;
        journal.state = reconciled_state(&journal.entries);
        let result = journal.clone();
        self.commit_workflows(workflows)?;
        Ok(result)
    }
    pub(crate) fn validate_restore_blobs(
        &self,
        journal: &RestoreJournal,
    ) -> Result<(), ServiceError> {
        self.read_blob(journal.current_buffer_raw_hash.as_str())?;
        for entry in &journal.entries {
            for (snapshot_hash, raw_hash) in [
                (
                    &entry.plan.before_snapshot_hash,
                    &entry.plan.before_raw_hash,
                ),
                (
                    &entry.plan.expected_snapshot_hash,
                    &entry.plan.expected_raw_hash,
                ),
            ] {
                let snapshot = decode_snapshot(&self.read_blob(snapshot_hash.as_str())?)?;
                let slot = entry.plan.selection.target;
                let raw_context = gp200_protocol::WireContext::from_transfer(&snapshot.raw);
                if snapshot.target != Some(PresetTarget::Stored(slot))
                    || snapshot.context
                        != Some(gp200_domain::WireContext {
                            group: 1,
                            selector: slot.get(),
                        })
                    || raw_context
                        != Some(gp200_protocol::WireContext {
                            group: 1,
                            selector: slot.get(),
                        })
                    || raw_bytes(&snapshot.raw) != self.read_blob(raw_hash.as_str())?
                {
                    return Err(invalid(
                        "Restore before/expected snapshot, addressing and raw hash disagree",
                    ));
                }
                if snapshot
                    .raw
                    .records()
                    .iter()
                    .filter(|record| matches!(record.command, 2 | 8))
                    .any(|record| record.body.get(..2) != Some(slot.get().to_le_bytes().as_slice()))
                {
                    return Err(invalid(
                        "Restore preset embedded slot fields differ from its target",
                    ));
                }
                crate::native::require_encoded_projection(&snapshot)?;
                full_stored_write_request(entry.plan.selection.target.get(), &snapshot.raw)
                    .map_err(invalid)?;
            }
            self.read_blob(entry.observed_raw_hash.as_str())?;
            match &entry.state {
                RestoreEntryState::Verified { readback_raw_hash } => {
                    self.read_blob(readback_raw_hash.as_str())?;
                }
                RestoreEntryState::Conflict { actual_raw_hash } => {
                    self.read_blob(actual_raw_hash.as_str())?;
                }
                _ => {}
            }
        }
        Ok(())
    }
}
fn reconciled_state(entries: &[RestoreJournalEntry]) -> RestoreRunState {
    if entries
        .iter()
        .all(|entry| matches!(entry.state, RestoreEntryState::Verified { .. }))
    {
        RestoreRunState::Completed
    } else if entries.iter().any(|entry| {
        !matches!(
            entry.state,
            RestoreEntryState::Planned | RestoreEntryState::Verified { .. }
        )
    }) {
        RestoreRunState::Paused {
            reason: "Review unapplied uncertain entries or conflicts before continuing".into(),
        }
    } else {
        RestoreRunState::Ready
    }
}
/// Compare every raw byte except the narrowly observed device padding. Transfer kinds may
/// differ between write and reply; context, slot fields, unknown records and float bits must match.
pub fn stored_readback_equal(expected: &RawTransfer, actual: &RawTransfer) -> bool {
    let a = expected.records();
    let b = actual.records();
    a.len() == b.len()
        && a.iter().zip(b).all(|(a, b)| {
            a.command == b.command
                && a.body.len() == b.body.len()
                && a.body
                    .iter()
                    .zip(&b.body)
                    .enumerate()
                    .all(|(offset, (a_byte, b_byte))| {
                        matches!(
                            (a.command, a.body.len(), offset),
                            (2, 88, 15) | (8, 16, 15) | (0x0f, 8, 2 | 3)
                        ) || a_byte == b_byte
                    })
        })
}
