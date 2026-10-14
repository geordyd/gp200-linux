//! Complete, inactive preset movement reviews. This module never dispatches.
//! Ready is a durable prepared review, not a live hardware permission.

use crate::{
    BackupAcquisition, BackupOrigin, BundleLimits, CapturedGlobals, CapturedPreset, ContentHash,
    DeviceBackup, LibraryStore, PresetMovementOperation, PresetMovementPlan, PresetSlotMapping,
    RestoreAssetBinding, RestoreExclusion, RestorePlan, RestoreRequest, RestoreSelection,
    RestoreSource, TargetAssetProof,
    device_backup::{check_snapshot, raw_bytes},
    store::{invalid, io, reject_symlink},
    workflows::new_id,
};
use gp200_domain::*;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::Read,
    path::{Path, PathBuf},
    sync::Arc,
};

/// Supplied by a live coordinator after correlated reads. These values cannot
/// make the actor trust an imported capability claim or send any device write.
#[derive(Debug, Clone)]
pub struct PresetMovementObserved {
    pub origin: BackupOrigin,
    pub current_buffer: Arc<PresetSnapshot>,
    pub globals: Arc<DeviceGlobalState>,
    pub stored: BTreeMap<SlotId, Arc<PresetSnapshot>>,
    pub capabilities: Capabilities,
    pub expected_profile: ValidationProfileId,
}

#[derive(Debug, Clone)]
pub struct PresetMovementRequest {
    pub movement: PresetMovementPlan,
    pub before_backup: String,
    pub observed: PresetMovementObserved,
    /// Movement preserves the original physical asset references. Bindings may
    /// establish identical content in the same slot, never remap an asset.
    pub asset_bindings: Vec<RestoreAssetBinding>,
    pub target_assets: Vec<TargetAssetProof>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresetMovementBeforeRole {
    Interval,
    Sentinel,
}

#[derive(Debug, Clone)]
pub struct PresetMovementBeforeSlot {
    role: PresetMovementBeforeRole,
    capture: CapturedPreset,
    observed_revision: Revision,
}

impl PresetMovementBeforeSlot {
    pub const fn role(&self) -> PresetMovementBeforeRole {
        self.role
    }
    pub fn capture(&self) -> &CapturedPreset {
        &self.capture
    }
    pub const fn observed_revision(&self) -> Revision {
        self.observed_revision
    }
}

/// Immutable originals were published and independently reopened before the
/// restore preview. No replay/dispatch authority can be deserialized here.
#[derive(Debug, Clone)]
pub struct PresetMovementBeforeReview {
    backup: String,
    origin: BackupOrigin,
    profile: ValidationProfileId,
    archive: PathBuf,
    archive_hash: ContentHash,
    descriptor_hash: ContentHash,
    current: CapturedPreset,
    current_observed_revision: Revision,
    globals: CapturedGlobals,
    globals_observed_revision: Revision,
    slots: Vec<PresetMovementBeforeSlot>,
}

impl PresetMovementBeforeReview {
    pub fn backup(&self) -> &str {
        &self.backup
    }
    pub fn origin(&self) -> &BackupOrigin {
        &self.origin
    }
    pub fn profile(&self) -> &ValidationProfileId {
        &self.profile
    }
    pub fn archive(&self) -> &Path {
        &self.archive
    }
    pub fn archive_hash(&self) -> &ContentHash {
        &self.archive_hash
    }
    pub fn descriptor_hash(&self) -> &ContentHash {
        &self.descriptor_hash
    }
    pub fn current(&self) -> &CapturedPreset {
        &self.current
    }
    pub const fn current_observed_revision(&self) -> Revision {
        self.current_observed_revision
    }
    pub fn globals(&self) -> &CapturedGlobals {
        &self.globals
    }
    pub const fn globals_observed_revision(&self) -> Revision {
        self.globals_observed_revision
    }
    pub fn slots(&self) -> &[PresetMovementBeforeSlot] {
        &self.slots
    }
}

#[derive(Debug, Clone)]
pub struct PresetMovementBlocked {
    movement: PresetMovementPlan,
    before: PresetMovementBeforeReview,
    excluded: Vec<RestoreExclusion>,
    compatible_rows: usize,
}

impl PresetMovementBlocked {
    pub fn movement(&self) -> &PresetMovementPlan {
        &self.movement
    }
    pub fn before(&self) -> &PresetMovementBeforeReview {
        &self.before
    }
    pub fn excluded(&self) -> &[RestoreExclusion] {
        &self.excluded
    }
    pub const fn compatible_rows(&self) -> usize {
        self.compatible_rows
    }
    // Deliberately no RestorePlan getter: compatible subsets are not movement.
}

#[derive(Debug, Clone)]
pub struct PresetMovementReview {
    movement: PresetMovementPlan,
    before: PresetMovementBeforeReview,
    restore: RestorePlan,
}

impl PresetMovementReview {
    pub fn movement(&self) -> &PresetMovementPlan {
        &self.movement
    }
    pub fn before(&self) -> &PresetMovementBeforeReview {
        &self.before
    }
    /// Always the entire destination-ordered interval with no exclusions.
    /// A later coordinator must recheck all preservation pins and journal it;
    /// passing this to generic restore does not implement movement orchestration.
    pub fn restore_plan(&self) -> &RestorePlan {
        &self.restore
    }

    /// Pin operation, original identities, every preservation object, session,
    /// exact before archive and the entire existing restore payload review.
    pub fn review_hash(&self) -> Result<ContentHash, ServiceError> {
        let operation = match self.movement.operation() {
            PresetMovementOperation::Insertion => "insertion".to_owned(),
            PresetMovementOperation::Selection(action) => format!("selection-{action:?}"),
        };
        let steps: Vec<_> = self
            .movement
            .insertion_steps()
            .iter()
            .map(|step| {
                json!({ "source": step.source, "boundary": step.boundary.get(),
                    "destination": step.destination })
            })
            .collect();
        let mappings: Vec<_> = self
            .movement
            .affected_mappings()
            .map(|mapping| json!({ "source": mapping.source, "destination": mapping.destination }))
            .collect();
        let slots: Vec<_> = self
            .before
            .slots
            .iter()
            .map(|slot| {
                json!({ "role": format!("{:?}", slot.role), "capture": slot.capture,
                    "observed_revision": slot.observed_revision })
            })
            .collect();
        let value = json!({
            "format": "gp200-studio.preset-movement-review", "schema": 1,
            "policy": "preserve-inactive-current-and-globals",
            "operation": operation, "steps": steps, "mappings": mappings,
            "original_selection": self.movement.original_selection(),
            "final_selection": self.movement.final_selection(),
            "current_before": self.movement.current_before(),
            "original_reselection_destination_descriptive_only":
                self.movement.current_after_original_reselection(),
            "backup": self.before.backup, "origin": self.before.origin,
            "operation_profile": self.before.profile,
            "archive_hash": self.before.archive_hash,
            "descriptor_hash": self.before.descriptor_hash,
            "current": self.before.current,
            "current_observed_revision": self.before.current_observed_revision,
            "globals": self.before.globals,
            "globals_observed_revision": self.before.globals_observed_revision,
            "slots": slots, "restore": self.restore,
        });
        Ok(ContentHash::of(
            &serde_json::to_vec(&value).map_err(invalid)?,
        ))
    }
}

#[derive(Debug, Clone)]
pub enum PresetMovementPreparation {
    Noop(PresetMovementPlan),
    Blocked(Box<PresetMovementBlocked>),
    Ready(Box<PresetMovementReview>),
}

impl LibraryStore {
    /// No journal, job or device write is created. Every source is read from
    /// one immutable finished before archive; an exclusion blocks all rows.
    pub fn prepare_preset_movement(
        &self,
        request: &PresetMovementRequest,
    ) -> Result<PresetMovementPreparation, ServiceError> {
        let observed = &request.observed;
        observed.origin.validate()?;
        if request.movement.model() != observed.origin.identity.model {
            return Err(conflict("Movement bank variant differs from this device"));
        }
        if request.movement.is_noop() {
            return Ok(PresetMovementPreparation::Noop(request.movement.clone()));
        }
        check_snapshot(
            &observed.origin,
            &observed.current_buffer,
            PresetTarget::EditBuffer,
        )?;
        crate::native::require_encoded_projection(&observed.current_buffer)?;
        let active = SlotId::new(
            observed
                .current_buffer
                .context
                .expect("checked context")
                .selector,
        )
        .map_err(invalid)?;
        if request
            .movement
            .current_before()
            .is_some_and(|slot| slot != active)
        {
            return Err(conflict(
                "Movement's descriptive current sound differs from the live current slot",
            ));
        }
        if !observed
            .capabilities
            .allows(OperationKind::PresetImport, &observed.origin.identity)
            || !matches!(observed.capabilities.operations.get(&OperationKind::PresetImport),
                Some(FeatureSupport::Verified { profile, .. }) if profile == &observed.expected_profile)
            || observed.expected_profile.0.is_empty()
            || observed.expected_profile.0.len() > 128
        {
            return Err(ServiceError::new(
                ErrorKind::Unverified,
                "Matching live full-stored profile is unavailable; movement remains a preview",
            ));
        }
        if request
            .asset_bindings
            .iter()
            .any(|binding| binding.source != binding.target)
        {
            return Err(invalid(
                "Movement preserves asset slot references; review asset remapping separately",
            ));
        }
        if request.asset_bindings.len() > 30 || request.target_assets.len() > 30 {
            return Err(invalid(
                "Movement asset proof/binding count exceeds established bounds",
            ));
        }
        let backup = self.device_backup(&request.before_backup)?;
        if backup.acquisition != BackupAcquisition::Finished
            || !backup.read_errors.is_empty()
            || backup.origin != observed.origin
        {
            return Err(conflict(
                "Movement needs one finished, error-free before archive from this exact origin and session",
            ));
        }
        self.validate_backup_blobs(&backup)?;
        let current = backup
            .edit_buffer
            .as_ref()
            .ok_or_else(|| invalid("Before archive lacks the current buffer"))?;
        self.load_backup_preset(&backup.id, PresetTarget::EditBuffer)?;
        if observed.current_buffer.revision < current.revision
            || ContentHash::of(&raw_bytes(&observed.current_buffer.raw)) != current.raw_hash
        {
            return Err(conflict(
                "Current buffer changed or its observation predates the before archive",
            ));
        }
        let globals = backup
            .globals
            .as_ref()
            .ok_or_else(|| invalid("Before archive lacks complete globals"))?;
        self.load_backup_globals(&backup.id)?;
        if observed.globals.epoch != observed.origin.epoch
            || !observed.globals.records().is_complete()
            || observed.globals.revision < globals.revision
            || ContentHash::of(&raw_bytes(observed.globals.records().raw())) != globals.raw_hash
        {
            return Err(conflict(
                "Globals changed, are incomplete or belong to an older/different session",
            ));
        }
        let interval = request
            .movement
            .affected_interval()
            .expect("non-noop interval");
        let mut slots = Vec::new();
        for (slot, role) in interval
            .slots()
            .map(|slot| (slot, PresetMovementBeforeRole::Interval))
            .chain(
                request
                    .movement
                    .sentinels()
                    .iter()
                    .copied()
                    .map(|slot| (slot, PresetMovementBeforeRole::Sentinel)),
            )
        {
            let capture = backup.presets.get(&slot).ok_or_else(|| {
                invalid(format!(
                    "Before archive lacks {:?} slot {}",
                    role,
                    slot.get()
                ))
            })?;
            self.load_backup_preset(&backup.id, PresetTarget::Stored(slot))?;
            let actual = observed.stored.get(&slot).ok_or_else(|| {
                invalid(format!(
                    "Fresh movement observation lacks {:?} slot {}",
                    role,
                    slot.get()
                ))
            })?;
            check_snapshot(&observed.origin, actual, PresetTarget::Stored(slot))?;
            crate::native::require_encoded_projection(actual)?;
            if actual.revision < capture.revision
                || ContentHash::of(&raw_bytes(&actual.raw)) != capture.raw_hash
            {
                return Err(conflict(format!(
                    "Stored {:?} slot {} changed or its observation predates the before archive",
                    role,
                    slot.get()
                )));
            }
            slots.push(PresetMovementBeforeSlot {
                role,
                capture: capture.clone(),
                observed_revision: actual.revision,
            });
        }
        slots.sort_by_key(|slot| slot.capture.slot);
        let descriptor = serde_json::to_vec(&backup).map_err(invalid)?;
        let descriptor_hash = ContentHash::of(&descriptor);
        let (archive, archive_hash) = self.publish_movement_before(&backup, &descriptor)?;
        let before = PresetMovementBeforeReview {
            backup: backup.id.clone(),
            origin: backup.origin.clone(),
            profile: observed.expected_profile.clone(),
            archive,
            archive_hash,
            descriptor_hash,
            current: current.clone(),
            current_observed_revision: observed.current_buffer.revision,
            globals: globals.clone(),
            globals_observed_revision: observed.globals.revision,
            slots,
        };
        let selections =
            complete_restore_selections(&backup.id, request.movement.affected_mappings().copied());
        let restore = self.plan_restore(&RestoreRequest {
            target: observed.origin.clone(),
            current_buffer: observed.current_buffer.clone(),
            selections: selections.clone(),
            observed_targets: observed.stored.clone(),
            asset_bindings: request.asset_bindings.clone(),
            target_assets: request.target_assets.clone(),
        })?;
        if !restore.excluded.is_empty() {
            return Ok(PresetMovementPreparation::Blocked(Box::new(
                PresetMovementBlocked {
                    movement: request.movement.clone(),
                    before,
                    compatible_rows: restore.entries.len(),
                    excluded: restore.excluded,
                },
            )));
        }
        if restore.entries.len() != selections.len()
            || restore
                .entries
                .iter()
                .zip(&selections)
                .any(|(entry, selection)| &entry.selection != selection)
        {
            return Err(invalid(
                "Restore preview did not retain the entire intended movement interval",
            ));
        }
        Ok(PresetMovementPreparation::Ready(Box::new(
            PresetMovementReview {
                movement: request.movement.clone(),
                before,
                restore,
            },
        )))
    }

    fn publish_movement_before(
        &self,
        backup: &DeviceBackup,
        descriptor: &[u8],
    ) -> Result<(PathBuf, ContentHash), ServiceError> {
        let directory = self.root().join("movement-before");
        reject_symlink(&directory)?;
        if !directory.exists() {
            fs::create_dir(&directory).map_err(io)?;
            File::open(self.root())
                .and_then(|file| file.sync_all())
                .map_err(io)?;
        }
        let archive = directory.join(format!("{}.gp200backup", new_id("movement-before")?));
        self.export_device_backup(&backup.id, &archive)?;
        let before_hash = movement_archive_hash(&archive)?;
        let reopened = Self::preview_device_backup(&archive, BundleLimits::default())?;
        if serde_json::to_vec(&reopened).map_err(invalid)? != descriptor
            || movement_archive_hash(&archive)? != before_hash
        {
            return Err(conflict(
                "Published movement before archive changed or disagrees after reopening",
            ));
        }
        Ok((archive, before_hash))
    }
}

// Includes every row, including a hypothetical unchanged interior. It does not
// construct/validate a movement plan or supply any operation authority.
fn complete_restore_selections(
    backup: &str,
    mappings: impl IntoIterator<Item = PresetSlotMapping>,
) -> Vec<RestoreSelection> {
    mappings
        .into_iter()
        .map(|mapping| RestoreSelection {
            source: RestoreSource::BackupStored {
                backup: backup.to_owned(),
                slot: mapping.source,
            },
            target: mapping.destination,
        })
        .collect()
}

fn movement_archive_hash(path: &Path) -> Result<ContentHash, ServiceError> {
    reject_symlink(path)?;
    let mut input = File::open(path).map_err(io)?;
    let metadata = input.metadata().map_err(io)?;
    if !metadata.is_file() || metadata.len() > BundleLimits::default().compressed_bytes {
        return Err(invalid("Movement archive must be a bounded regular file"));
    }
    let mut sha = Sha256::new();
    let mut buffer = [0; 8192];
    let mut length = 0u64;
    loop {
        let read = input.read(&mut buffer).map_err(io)?;
        if read == 0 {
            break;
        }
        length = length
            .checked_add(read as u64)
            .ok_or_else(|| invalid("Archive length overflow"))?;
        if length > BundleLimits::default().compressed_bytes {
            return Err(invalid("Movement before archive exceeds bounds"));
        }
        sha.update(&buffer[..read]);
    }
    ContentHash::new(format!("{:x}", sha.finalize()))
}

fn conflict(message: impl Into<String>) -> ServiceError {
    ServiceError::new(ErrorKind::Conflict, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn complete_interval_helper_keeps_a_synthetic_unchanged_interior() {
        // Two disjoint swaps are not currently produced by the public insertion
        // constructors. This tests only coverage, without forging a checked plan.
        let mappings = [(2, 1), (1, 2), (3, 3), (5, 4), (4, 5)].map(|(source, destination)| {
            PresetSlotMapping {
                source: SlotId::new(source).unwrap(),
                destination: SlotId::new(destination).unwrap(),
            }
        });
        let actual = complete_restore_selections("immutable-originals", mappings);
        assert_eq!(actual.len(), 5);
        for (entry, (source, destination)) in
            actual.iter().zip([(2, 1), (1, 2), (3, 3), (5, 4), (4, 5)])
        {
            assert_eq!(entry.target.get(), destination);
            assert_eq!(
                entry.source,
                RestoreSource::BackupStored {
                    backup: "immutable-originals".into(),
                    slot: SlotId::new(source).unwrap()
                }
            );
        }
    }
}
