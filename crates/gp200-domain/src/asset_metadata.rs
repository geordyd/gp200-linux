//! Session-bound asset metadata. A name/checksum record never proves occupancy,
//! coefficient contents, an empty slot, or a recoverable backup.
use crate::{ErrorKind, RequestStamp, Revision, ServiceError, SessionEpoch, SnapshotSource};
pub use gp200_protocol::{AssetBank, AssetSlot};
use gp200_protocol::{AssetSlotMetadata, RawTransfer, WRITE_KIND};
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceAssetMetadata {
    stamp: RequestStamp,
    revision: Revision,
    source: SnapshotSource,
    captured_at_ms: u64,
    metadata: AssetSlotMetadata,
    raw: Arc<RawTransfer>,
}
impl DeviceAssetMetadata {
    pub fn from_transfer(
        slot: AssetSlot,
        stamp: RequestStamp,
        revision: Revision,
        source: SnapshotSource,
        captured_at_ms: u64,
        raw: RawTransfer,
    ) -> Result<Self, ServiceError> {
        if raw.kind() != WRITE_KIND || raw.records().len() != 1 {
            return Err(ServiceError::new(
                ErrorKind::Protocol,
                "Asset metadata requires one complete normal-mode reply record",
            ));
        }
        let metadata = AssetSlotMetadata::decode(slot.bank(), &raw.records()[0])
            .map_err(|error| ServiceError::new(ErrorKind::Protocol, error.to_string()))?;
        if metadata.slot != slot {
            return Err(ServiceError::new(
                ErrorKind::Conflict,
                "Asset metadata belongs to a different bank or slot",
            ));
        }
        if matches!(source, SnapshotSource::Hardware { epoch } if epoch != stamp.epoch) {
            return Err(ServiceError::new(
                ErrorKind::Conflict,
                "Asset metadata provenance belongs to a different session",
            ));
        }
        Ok(Self {
            stamp,
            revision,
            source,
            captured_at_ms,
            metadata,
            raw: Arc::new(raw),
        })
    }
    pub fn slot(&self) -> AssetSlot {
        self.metadata.slot
    }
    pub fn stamp(&self) -> RequestStamp {
        self.stamp
    }
    pub fn revision(&self) -> Revision {
        self.revision
    }
    pub fn source(&self) -> &SnapshotSource {
        &self.source
    }
    pub fn captured_at_ms(&self) -> u64 {
        self.captured_at_ms
    }
    pub fn metadata(&self) -> &AssetSlotMetadata {
        &self.metadata
    }
    pub fn raw(&self) -> &Arc<RawTransfer> {
        &self.raw
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AssetMetadataReadStatus {
    NeverRead,
    Reading {
        stamp: RequestStamp,
    },
    Ready,
    Failed {
        stamp: RequestStamp,
        error: ServiceError,
    },
    Stale,
}
#[derive(Debug, Clone)]
pub struct AssetMetadataEntry {
    pub slot: AssetSlot,
    pub status: AssetMetadataReadStatus,
    pub snapshot: Option<Arc<DeviceAssetMetadata>>,
}
#[derive(Debug, Clone)]
pub struct DeviceAssetInventory {
    epoch: SessionEpoch,
    entries: Vec<AssetMetadataEntry>,
}
impl DeviceAssetInventory {
    pub fn new(epoch: SessionEpoch) -> Self {
        Self {
            epoch,
            entries: [AssetBank::Ir, AssetBank::SnapTone]
                .into_iter()
                .flat_map(|bank| {
                    (0..bank.slots()).map(move |index| AssetMetadataEntry {
                        slot: AssetSlot::new(bank, index).expect("bounded slot"),
                        status: AssetMetadataReadStatus::NeverRead,
                        snapshot: None,
                    })
                })
                .collect(),
        }
    }
    pub fn epoch(&self) -> SessionEpoch {
        self.epoch
    }
    pub fn entries(&self, bank: AssetBank) -> impl Iterator<Item = &AssetMetadataEntry> {
        self.entries
            .iter()
            .filter(move |entry| entry.slot.bank() == bank)
    }
    pub fn entry(&self, slot: AssetSlot) -> &AssetMetadataEntry {
        &self.entries[Self::offset(slot)]
    }
    fn entry_mut(&mut self, slot: AssetSlot) -> &mut AssetMetadataEntry {
        &mut self.entries[Self::offset(slot)]
    }
    fn offset(slot: AssetSlot) -> usize {
        usize::from(slot.index())
            + if slot.bank() == AssetBank::Ir {
                0
            } else {
                usize::from(AssetBank::Ir.slots())
            }
    }
    pub fn begin(&mut self, slot: AssetSlot, stamp: RequestStamp) -> bool {
        if stamp.epoch != self.epoch {
            return false;
        }
        self.entry_mut(slot).status = AssetMetadataReadStatus::Reading { stamp };
        true
    }
    pub fn complete(&mut self, snapshot: Arc<DeviceAssetMetadata>) -> bool {
        let stamp = snapshot.stamp();
        if stamp.epoch != self.epoch {
            return false;
        }
        let entry = self.entry_mut(snapshot.slot());
        if entry.status != (AssetMetadataReadStatus::Reading { stamp })
            || entry.snapshot.as_ref().is_some_and(|old| {
                old.stamp().epoch == stamp.epoch && old.revision() >= snapshot.revision()
            })
        {
            return false;
        }
        entry.status = AssetMetadataReadStatus::Ready;
        entry.snapshot = Some(snapshot);
        true
    }
    pub fn fail(&mut self, slot: AssetSlot, stamp: RequestStamp, error: ServiceError) -> bool {
        if stamp.epoch != self.epoch
            || self.entry(slot).status != (AssetMetadataReadStatus::Reading { stamp })
        {
            return false;
        }
        self.entry_mut(slot).status = AssetMetadataReadStatus::Failed { stamp, error };
        true
    }
    pub fn invalidate(&mut self, epoch: SessionEpoch) {
        self.epoch = epoch;
        for entry in &mut self.entries {
            entry.status = if entry.snapshot.is_some() {
                AssetMetadataReadStatus::Stale
            } else {
                match &entry.status {
                    AssetMetadataReadStatus::Failed { .. } => entry.status.clone(),
                    AssetMetadataReadStatus::Reading { stamp } => AssetMetadataReadStatus::Failed {
                        stamp: *stamp,
                        error: ServiceError::new(
                            ErrorKind::Disconnected,
                            "The session ended before asset metadata arrived",
                        ),
                    },
                    _ => AssetMetadataReadStatus::NeverRead,
                }
            };
        }
    }
    pub fn complete_count(&self, bank: AssetBank) -> usize {
        self.entries(bank)
            .filter(|entry| entry.status == AssetMetadataReadStatus::Ready)
            .count()
    }
}
