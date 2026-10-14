use crate::{ContentHash, DeviceAssetKind, DeviceAssetSlot, LibraryStore};
use gp200_domain::*;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AssetDependencyOwner {
    LibraryRevision {
        version: LibraryVersionId,
        latest: bool,
        trashed: bool,
    },
    BackupStored {
        backup: String,
        slot: SlotId,
    },
    BackupEditBuffer {
        backup: String,
    },
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssetDependencyUse {
    pub owner: AssetDependencyOwner,
    pub preset_name: String,
    pub content_hash: Option<ContentHash>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssetReplacementImpact {
    pub slot: DeviceAssetSlot,
    pub uses: Vec<AssetDependencyUse>,
    /// Reports known references only; unread/unknown slots are never declared unaffected.
    pub missing_backup_slots: Vec<(String, SlotId)>,
    pub requires_hardware_revalidation: bool,
}
fn references(snapshot: &PresetSnapshot, slot: DeviceAssetSlot) -> bool {
    let projected = PresetSnapshot::from_raw(
        snapshot.raw.clone(),
        snapshot.source.clone(),
        snapshot.target,
        snapshot.revision,
    );
    projected.asset_refs.iter().any(|reference| matches!(reference, AssetRef::UserSlot { kind, slot: index, .. } if *kind == slot.kind().asset_kind() && *index == slot.index()))
}
impl LibraryStore {
    /// Pure, lossless dependency inspection. Historical revisions/archival backups are labeled.
    pub fn asset_replacement_impact(
        &self,
        slot: DeviceAssetSlot,
    ) -> Result<AssetReplacementImpact, ServiceError> {
        let mut uses = Vec::new();
        for version in self.manifest.versions.values() {
            let loaded = self.load_version(&version.id)?;
            if !references(&loaded.snapshot, slot) {
                continue;
            }
            let entry = &self.manifest.entries[&version.library];
            let content_hash =
                loaded
                    .snapshot
                    .asset_refs
                    .iter()
                    .find_map(|reference| match reference {
                        AssetRef::UserSlot {
                            kind,
                            slot: index,
                            content: Some(id),
                        } if *kind == slot.kind().asset_kind() && *index == slot.index() => self
                            .assets()
                            .get(id)
                            .and_then(|asset| ContentHash::new(&asset.hash).ok()),
                        _ => None,
                    });
            uses.push(AssetDependencyUse {
                owner: AssetDependencyOwner::LibraryRevision {
                    version: version.id.clone(),
                    latest: version.id == entry.latest,
                    trashed: entry.trashed,
                },
                preset_name: loaded.metadata.name,
                content_hash,
            });
        }
        let mut missing_backup_slots = Vec::new();
        for backup in self.device_backups()? {
            for index in 0..256 {
                let index = SlotId::new(index).expect("bounded bank");
                if !backup.presets.contains_key(&index) {
                    missing_backup_slots.push((backup.id.clone(), index));
                    continue;
                }
                let snapshot = self.load_backup_preset(&backup.id, PresetTarget::Stored(index))?;
                if references(&snapshot, slot) {
                    uses.push(AssetDependencyUse {
                        owner: AssetDependencyOwner::BackupStored {
                            backup: backup.id.clone(),
                            slot: index,
                        },
                        preset_name: snapshot.view.display_name().into(),
                        content_hash: backup
                            .assets
                            .get(&slot.key())
                            .and_then(|asset| asset.content.as_ref())
                            .filter(|content| content.origin.is_verified())
                            .map(|content| content.hash.clone()),
                    });
                }
            }
            if backup.edit_buffer.is_some() {
                let snapshot = self.load_backup_preset(&backup.id, PresetTarget::EditBuffer)?;
                if references(&snapshot, slot) {
                    uses.push(AssetDependencyUse {
                        owner: AssetDependencyOwner::BackupEditBuffer {
                            backup: backup.id.clone(),
                        },
                        preset_name: snapshot.view.display_name().into(),
                        content_hash: backup
                            .assets
                            .get(&slot.key())
                            .and_then(|asset| asset.content.as_ref())
                            .filter(|content| content.origin.is_verified())
                            .map(|content| content.hash.clone()),
                    });
                }
            }
        }
        Ok(AssetReplacementImpact {
            slot,
            uses,
            missing_backup_slots,
            requires_hardware_revalidation: true,
        })
    }
    /// Inventory of slots referenced by known raw module Codes, ignoring display-only projections.
    pub fn referenced_device_asset_slots(&self) -> Result<Vec<DeviceAssetSlot>, ServiceError> {
        let mut slots = BTreeSet::new();
        for version in self.manifest.versions.keys() {
            let snapshot = self.load_version(version)?.snapshot;
            let projected = PresetSnapshot::from_raw(
                snapshot.raw.clone(),
                snapshot.source.clone(),
                snapshot.target,
                snapshot.revision,
            );
            for reference in projected.asset_refs {
                if let AssetRef::UserSlot { kind, slot, .. } = reference {
                    let kind = match kind {
                        AssetKind::ImpulseResponse => DeviceAssetKind::Ir,
                        AssetKind::SnapTone => DeviceAssetKind::SnapTone,
                        _ => continue,
                    };
                    slots.insert(DeviceAssetSlot::new(kind, slot)?);
                }
            }
        }
        Ok(slots.into_iter().collect())
    }
}
