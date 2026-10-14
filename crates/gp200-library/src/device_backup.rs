use crate::{
    ContentHash, LibraryStore,
    store::{decode_snapshot, invalid},
    workflows::new_id,
};
use gp200_domain::*;
use gp200_protocol::{
    AssetBank, AssetSlotMetadata, GlobalRecords, RawRecord, RawTransfer,
    WireContext as ProtocolContext,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackupOrigin {
    pub identity: DeviceIdentity,
    pub epoch: SessionEpoch,
    /// Caller-observed USB serial/device fingerprint. Never inferred from a preset name.
    pub device_key: String,
    pub evidence: Vec<EvidenceRef>,
}
impl BackupOrigin {
    pub(crate) fn validate(&self) -> Result<(), ServiceError> {
        if self.identity.model != DeviceModel::Gp200
            || self.device_key.is_empty()
            || self.device_key.len() > 512
            || self.identity.display_name.len() > 1024
            || self.evidence.len() > 128
            || self.identity.firmware.as_ref().is_some_and(|f| {
                f.version.len() > 256 || f.raw.len() > 65536 || f.evidence.len() > 128
            })
        {
            return Err(invalid("Invalid or unsupported GP-200 backup origin"));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum DeviceAssetKind {
    Ir,
    SnapTone,
}
impl DeviceAssetKind {
    pub fn slots(self) -> u8 {
        match self {
            Self::Ir => 20,
            Self::SnapTone => 10,
        }
    }
    pub fn asset_kind(self) -> AssetKind {
        match self {
            Self::Ir => AssetKind::ImpulseResponse,
            Self::SnapTone => AssetKind::SnapTone,
        }
    }
    pub(crate) fn bank(self) -> AssetBank {
        match self {
            Self::Ir => AssetBank::Ir,
            Self::SnapTone => AssetBank::SnapTone,
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DeviceAssetSlot {
    kind: DeviceAssetKind,
    index: u8,
}
impl DeviceAssetSlot {
    pub fn new(kind: DeviceAssetKind, index: u8) -> Result<Self, ServiceError> {
        if index >= kind.slots() {
            return Err(invalid("Device asset slot exceeds recovered bank capacity"));
        }
        Ok(Self { kind, index })
    }
    pub fn kind(self) -> DeviceAssetKind {
        self.kind
    }
    pub fn index(self) -> u8 {
        self.index
    }
    pub(crate) fn key(self) -> String {
        format!(
            "{}-{:02}",
            match self.kind {
                DeviceAssetKind::Ir => "ir",
                DeviceAssetKind::SnapTone => "snaptone",
            },
            self.index
        )
    }
}
impl<'de> Deserialize<'de> for DeviceAssetSlot {
    fn deserialize<D: serde::Deserializer<'de>>(decoder: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            kind: DeviceAssetKind,
            index: u8,
        }
        let wire = Wire::deserialize(decoder)?;
        Self::new(wire.kind, wire.index).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum BackupAcquisition {
    Acquiring,
    Finished,
    Cancelled,
    Failed { reason: String },
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedPreset {
    pub slot: SlotId,
    pub revision: Revision,
    pub snapshot_hash: ContentHash,
    /// Hash includes the transfer kind byte followed by the exact decoded payload.
    pub raw_hash: ContentHash,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedGlobals {
    pub revision: Revision,
    pub raw_hash: ContentHash,
    pub footswitch_tables: Vec<u16>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AssetContentOrigin {
    /// Exact device bytes obtained through an independently verified read pipeline.
    DeviceReadback { evidence: Vec<EvidenceRef> },
    /// A local device-ready payload was independently matched to this device slot.
    VerifiedLocalMatch { evidence: Vec<EvidenceRef> },
    /// Retained source material; it does not establish what is stored on the pedal.
    LocalUnverified,
}
impl AssetContentOrigin {
    pub fn is_verified(&self) -> bool {
        match self {
            Self::DeviceReadback { evidence } | Self::VerifiedLocalMatch { evidence } => {
                !evidence.is_empty()
                    && evidence
                        .iter()
                        .any(|item| item.level == EvidenceLevel::LiveVerified)
            }
            Self::LocalUnverified => false,
        }
    }
    fn validate(&self) -> Result<(), ServiceError> {
        let count = match self {
            Self::DeviceReadback { evidence } | Self::VerifiedLocalMatch { evidence } => {
                evidence.len()
            }
            Self::LocalUnverified => 0,
        };
        if count > 128 {
            return Err(invalid("Asset content evidence exceeds bounds"));
        }
        Ok(())
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedAssetContent {
    pub hash: ContentHash,
    pub bytes: u64,
    /// Device format description, e.g. raw coefficient bytes. Never called original WAV by inference.
    pub format: String,
    pub origin: AssetContentOrigin,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedDeviceAsset {
    pub slot: DeviceAssetSlot,
    pub metadata_hash: ContentHash,
    pub name: String,
    pub content: Option<CapturedAssetContent>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeviceBackup {
    pub id: String,
    pub origin: BackupOrigin,
    pub acquisition: BackupAcquisition,
    pub presets: BTreeMap<SlotId, CapturedPreset>,
    /// The current edit buffer is separate and never counts as a stored slot.
    pub edit_buffer: Option<CapturedPreset>,
    pub globals: Option<CapturedGlobals>,
    pub assets: BTreeMap<String, CapturedDeviceAsset>,
    pub read_errors: BTreeMap<String, String>,
}
impl DeviceBackup {
    pub(crate) fn blob_hashes(&self) -> BTreeSet<ContentHash> {
        let mut hashes = BTreeSet::new();
        for preset in self.presets.values().chain(self.edit_buffer.iter()) {
            hashes.insert(preset.snapshot_hash.clone());
            hashes.insert(preset.raw_hash.clone());
        }
        if let Some(globals) = &self.globals {
            hashes.insert(globals.raw_hash.clone());
        }
        for asset in self.assets.values() {
            hashes.insert(asset.metadata_hash.clone());
            if let Some(content) = &asset.content {
                hashes.insert(content.hash.clone());
            }
        }
        hashes
    }
    pub(crate) fn validate(&self) -> Result<(), ServiceError> {
        self.origin.validate()?;
        if self.presets.len() > 256
            || self.assets.len() > 30
            || self.read_errors.len() > 512
            || self
                .read_errors
                .iter()
                .any(|(key, value)| key.len() > 128 || value.len() > 4096)
            || matches!(&self.acquisition, BackupAcquisition::Failed { reason } if reason.len() > 4096)
        {
            return Err(invalid("Backup descriptor exceeds bounds"));
        }
        for (slot, preset) in &self.presets {
            if slot != &preset.slot {
                return Err(invalid("Backup stored-slot identity mismatch"));
            }
        }
        if self.globals.as_ref().is_some_and(|globals| {
            globals.footswitch_tables.len() > 16
                || globals
                    .footswitch_tables
                    .iter()
                    .copied()
                    .collect::<BTreeSet<_>>()
                    .len()
                    != globals.footswitch_tables.len()
        }) {
            return Err(invalid("Invalid backup global mapping coverage"));
        }
        for (key, asset) in &self.assets {
            if key != &asset.slot.key() || asset.name.len() > 128 {
                return Err(invalid("Invalid backup asset descriptor"));
            }
            if let Some(content) = &asset.content {
                if content.bytes == 0
                    || content.bytes > crate::store::MAX_BLOB
                    || content.format.is_empty()
                    || content.format.len() > 256
                {
                    return Err(invalid("Invalid backup asset content descriptor"));
                }
                content.origin.validate()?;
            }
        }
        Ok(())
    }
    pub fn completeness(&self) -> BackupCompleteness {
        let missing_presets = (0..256)
            .map(|slot| SlotId::new(slot).expect("bounded bank"))
            .filter(|slot| !self.presets.contains_key(slot))
            .collect::<Vec<_>>();
        let expected_assets = [DeviceAssetKind::Ir, DeviceAssetKind::SnapTone]
            .into_iter()
            .flat_map(|kind| (0..kind.slots()).map(move |index| DeviceAssetSlot { kind, index }));
        let mut missing_asset_metadata = Vec::new();
        let mut missing_asset_content = Vec::new();
        for slot in expected_assets {
            match self.assets.get(&slot.key()) {
                None => {
                    missing_asset_metadata.push(slot);
                    missing_asset_content.push(slot);
                }
                Some(asset)
                    if asset
                        .content
                        .as_ref()
                        .is_none_or(|content| !content.origin.is_verified()) =>
                {
                    missing_asset_content.push(slot)
                }
                Some(_) => {}
            }
        }
        let global_mappings_captured = self.globals.as_ref().is_some_and(|globals| {
            globals
                .footswitch_tables
                .iter()
                .copied()
                .collect::<BTreeSet<_>>()
                == BTreeSet::from([0, 1, 2])
        });
        let complete = missing_presets.is_empty()
            && self.edit_buffer.is_some()
            && self.globals.is_some()
            && global_mappings_captured
            && missing_asset_metadata.is_empty()
            && missing_asset_content.is_empty();
        BackupCompleteness {
            label: if complete {
                "Stored presets, current buffer, globals and device asset content captured"
            } else if missing_presets.is_empty()
                && self.globals.is_some()
                && global_mappings_captured
                && missing_asset_metadata.is_empty()
                && missing_asset_content.is_empty()
            {
                "Stored presets, globals and assets captured; current buffer missing"
            } else if missing_presets.is_empty() && self.globals.is_some() {
                "256 stored presets and globals; device asset content incomplete"
            } else if missing_presets.is_empty() {
                "256 stored presets; globals and device assets incomplete"
            } else {
                "Partial stored-preset acquisition"
            }
            .into(),
            stored_presets: self.presets.len(),
            missing_presets,
            globals_captured: self.globals.is_some(),
            global_mappings_captured,
            asset_metadata: self.assets.len(),
            missing_asset_metadata,
            missing_asset_content,
            complete_device_state: complete,
            requires_hardware_revalidation: true,
            edit_buffer_captured: self.edit_buffer.is_some(),
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackupCompleteness {
    pub label: String,
    pub stored_presets: usize,
    pub missing_presets: Vec<SlotId>,
    pub globals_captured: bool,
    pub global_mappings_captured: bool,
    pub asset_metadata: usize,
    pub missing_asset_metadata: Vec<DeviceAssetSlot>,
    pub missing_asset_content: Vec<DeviceAssetSlot>,
    pub complete_device_state: bool,
    /// Archival provenance never authorizes a device transaction, even if all objects are present.
    pub requires_hardware_revalidation: bool,
    pub edit_buffer_captured: bool,
}

pub(crate) fn raw_bytes(raw: &RawTransfer) -> Vec<u8> {
    [vec![raw.kind()], raw.payload().to_vec()].concat()
}
pub(crate) fn parse_raw(bytes: &[u8]) -> Result<RawTransfer, ServiceError> {
    let (kind, payload) = bytes
        .split_first()
        .ok_or_else(|| invalid("Empty raw-transfer blob"))?;
    if bytes.len() > 65537 {
        return Err(invalid("Raw transfer exceeds backup bound"));
    }
    RawTransfer::from_payload(*kind, payload.to_vec()).map_err(invalid)
}
pub(crate) fn check_snapshot(
    origin: &BackupOrigin,
    snapshot: &PresetSnapshot,
    target: PresetTarget,
) -> Result<(), ServiceError> {
    let source_matches = match (&snapshot.source, origin.identity.mode) {
        (SnapshotSource::Hardware { epoch }, BackendMode::Hardware) => epoch == &origin.epoch,
        (SnapshotSource::Simulator, BackendMode::Simulator) => true,
        _ => false,
    };
    let context = ProtocolContext::from_transfer(&snapshot.raw)
        .ok_or_else(|| invalid("Snapshot has no unique established context"))?;
    let expected_context = match target {
        PresetTarget::Stored(slot) => (1, slot.get()),
        PresetTarget::EditBuffer => (0, context.selector),
    };
    if !source_matches
        || snapshot.device_model != origin.identity.model
        || snapshot.target != Some(target)
        || snapshot.context
            != Some(gp200_domain::WireContext {
                group: context.group,
                selector: context.selector,
            })
        || context.group != expected_context.0
        || context.selector != expected_context.1
        || context.selector > 255
        || snapshot
            .raw
            .records()
            .iter()
            .filter(|record| record.command == 0)
            .count()
            != 1
    {
        return Err(ServiceError::new(
            ErrorKind::Conflict,
            "Capture source, session, model or buffer/slot context does not match the backup origin",
        ));
    }
    let metadata = snapshot
        .raw
        .records()
        .iter()
        .filter(|record| record.command == 2)
        .collect::<Vec<_>>();
    let chain = snapshot
        .raw
        .records()
        .iter()
        .filter(|record| record.command == 8)
        .collect::<Vec<_>>();
    if metadata.len() != 1
        || metadata[0].body.len() != 88
        || chain.len() != 1
        || chain[0].body.len() != 16
        || metadata[0].body[..2] != context.selector.to_le_bytes()
        || chain[0].body[..2] != context.selector.to_le_bytes()
        || snapshot.raw.payload().len() > 65536
    {
        return Err(invalid(
            "Snapshot lacks a complete, correctly addressed preset header and chain",
        ));
    }
    let modules = snapshot
        .raw
        .records()
        .iter()
        .filter(|record| record.command == 0x14)
        .collect::<Vec<_>>();
    if modules.len() != 11
        || modules.iter().any(|record| record.body.len() != 68)
        || modules
            .iter()
            .map(|record| record.body[0])
            .collect::<BTreeSet<_>>()
            != (0..11).collect()
    {
        return Err(invalid(
            "Backup capture is missing one or more complete module records",
        ));
    }
    let assignments = PatchAssignments::from_raw(&snapshot.raw);
    if assignments.expressions.len() != 9
        || assignments.ctrls.len() != 8
        || assignments.quicks.len() != 3
        || !assignments.diagnostics.is_empty()
    {
        return Err(invalid(
            "Backup capture is missing complete, uniquely addressed assignment records",
        ));
    }
    Ok(())
}

impl LibraryStore {
    pub fn begin_device_backup(
        &mut self,
        origin: BackupOrigin,
    ) -> Result<DeviceBackup, ServiceError> {
        origin.validate()?;
        let mut workflows = self.workflows()?;
        let backup = DeviceBackup {
            id: new_id("backup")?,
            origin,
            acquisition: BackupAcquisition::Acquiring,
            presets: BTreeMap::new(),
            edit_buffer: None,
            globals: None,
            assets: BTreeMap::new(),
            read_errors: BTreeMap::new(),
        };
        workflows.backups.insert(backup.id.clone(), backup.clone());
        self.commit_workflows(workflows)?;
        Ok(backup)
    }
    pub fn device_backups(&self) -> Result<Vec<DeviceBackup>, ServiceError> {
        Ok(self.workflows()?.backups.into_values().collect())
    }
    pub fn device_backup(&self, id: &str) -> Result<DeviceBackup, ServiceError> {
        self.workflows()?
            .backups
            .remove(id)
            .ok_or_else(|| invalid("Device backup does not exist"))
    }
    pub fn capture_backup_preset(
        &mut self,
        id: &str,
        snapshot: Arc<PresetSnapshot>,
    ) -> Result<BackupCompleteness, ServiceError> {
        let mut workflows = self.workflows()?;
        let backup = acquiring(&mut workflows, id)?;
        let target = snapshot
            .target
            .ok_or_else(|| invalid("Backup capture has no explicit target"))?;
        check_snapshot(&backup.origin, &snapshot, target)?;
        // Reproject from the authoritative bytes; a forged/stale display model cannot enter the bank archive.
        let mut canonical = PresetSnapshot::from_raw(
            snapshot.raw.clone(),
            snapshot.source.clone(),
            Some(target),
            snapshot.revision,
        );
        canonical.device_model = snapshot.device_model;
        canonical.firmware = backup.origin.identity.firmware.clone();
        canonical.evidence = snapshot.evidence.clone();
        let bytes = serde_json::to_vec(&canonical).map_err(invalid)?;
        decode_snapshot(&bytes)?;
        let captured = CapturedPreset {
            slot: SlotId::new(canonical.context.expect("checked context").selector)
                .map_err(invalid)?,
            revision: snapshot.revision,
            snapshot_hash: ContentHash::new(self.publish_blob(&bytes)?)?,
            raw_hash: ContentHash::new(self.publish_blob(&raw_bytes(&snapshot.raw))?)?,
        };
        match target {
            PresetTarget::Stored(slot) => {
                if let Some(previous) = backup.presets.get(&slot)
                    && previous.raw_hash != captured.raw_hash
                {
                    return Err(ServiceError::new(
                        ErrorKind::Conflict,
                        "Stored slot changed during acquisition; retain this partial backup and begin a new acquisition",
                    ));
                }
                backup.presets.insert(slot, captured);
                backup.read_errors.remove(&format!("preset-{}", slot.get()));
            }
            PresetTarget::EditBuffer => {
                if let Some(previous) = &backup.edit_buffer
                    && previous.raw_hash != captured.raw_hash
                {
                    return Err(ServiceError::new(
                        ErrorKind::Conflict,
                        "Current edit buffer changed during acquisition",
                    ));
                }
                backup.edit_buffer = Some(captured);
                backup.read_errors.remove("edit-buffer");
            }
        }
        let completeness = backup.completeness();
        self.commit_workflows(workflows)?;
        Ok(completeness)
    }
    pub fn load_backup_preset(
        &self,
        id: &str,
        target: PresetTarget,
    ) -> Result<Arc<PresetSnapshot>, ServiceError> {
        let backup = self.device_backup(id)?;
        let captured = match target {
            PresetTarget::Stored(slot) => backup.presets.get(&slot),
            PresetTarget::EditBuffer => backup.edit_buffer.as_ref(),
        }
        .ok_or_else(|| invalid("Requested backup object was not acquired"))?;
        Ok(Arc::new(self.checked_captured_preset(
            &backup.origin,
            captured,
            target,
        )?))
    }
    pub fn capture_backup_globals(
        &mut self,
        id: &str,
        state: &DeviceGlobalState,
    ) -> Result<(), ServiceError> {
        let mut workflows = self.workflows()?;
        let backup = acquiring(&mut workflows, id)?;
        if state.epoch != backup.origin.epoch || !state.records().is_complete() {
            return Err(ServiceError::new(
                ErrorKind::Conflict,
                "Global state belongs to a different session or is incomplete",
            ));
        }
        let captured = CapturedGlobals {
            revision: state.revision,
            raw_hash: ContentHash::new(self.publish_blob(&raw_bytes(state.records().raw()))?)?,
            footswitch_tables: state
                .records()
                .footswitch_tables
                .iter()
                .map(|table| table.table_raw())
                .collect(),
        };
        if let Some(previous) = &backup.globals
            && previous.raw_hash != captured.raw_hash
        {
            return Err(ServiceError::new(
                ErrorKind::Conflict,
                "Globals changed during acquisition",
            ));
        }
        backup.globals = Some(captured);
        backup.read_errors.remove("globals");
        self.commit_workflows(workflows)
    }
    pub fn load_backup_globals(&self, id: &str) -> Result<RawTransfer, ServiceError> {
        let backup = self.device_backup(id)?;
        let globals = backup
            .globals
            .ok_or_else(|| invalid("Globals were not acquired"))?;
        parse_raw(&self.read_blob(globals.raw_hash.as_str())?)
    }
    pub fn capture_backup_asset_metadata(
        &mut self,
        id: &str,
        epoch: SessionEpoch,
        metadata: &AssetSlotMetadata,
    ) -> Result<(), ServiceError> {
        let mut workflows = self.workflows()?;
        let backup = acquiring(&mut workflows, id)?;
        if epoch != backup.origin.epoch {
            return Err(ServiceError::new(
                ErrorKind::Conflict,
                "Asset metadata belongs to a different session",
            ));
        }
        let decoded =
            AssetSlotMetadata::decode(metadata.slot.bank(), metadata.raw()).map_err(invalid)?;
        let kind = match decoded.slot.bank() {
            AssetBank::Ir => DeviceAssetKind::Ir,
            AssetBank::SnapTone => DeviceAssetKind::SnapTone,
        };
        let slot = DeviceAssetSlot::new(kind, decoded.slot.index() as u8)?;
        let hash = ContentHash::new(
            self.publish_blob(&serde_json::to_vec(decoded.raw()).map_err(invalid)?)?,
        )?;
        let previous = backup.assets.get(&slot.key());
        if previous.is_some_and(|asset| asset.metadata_hash != hash) {
            return Err(ServiceError::new(
                ErrorKind::Conflict,
                "Asset metadata changed during acquisition",
            ));
        }
        let asset = CapturedDeviceAsset {
            slot,
            metadata_hash: hash,
            name: decoded.display_name(),
            content: previous.and_then(|asset| asset.content.clone()),
        };
        backup.assets.insert(slot.key(), asset);
        backup.read_errors.remove(&slot.key());
        self.commit_workflows(workflows)
    }
    pub fn capture_backup_asset_content(
        &mut self,
        id: &str,
        epoch: SessionEpoch,
        slot: DeviceAssetSlot,
        bytes: &[u8],
        format: &str,
        origin: AssetContentOrigin,
    ) -> Result<(), ServiceError> {
        let mut workflows = self.workflows()?;
        let backup = acquiring(&mut workflows, id)?;
        if epoch != backup.origin.epoch {
            return Err(ServiceError::new(
                ErrorKind::Conflict,
                "Asset content belongs to a different session",
            ));
        }
        let asset = backup
            .assets
            .get_mut(&slot.key())
            .ok_or_else(|| invalid("Acquire slot metadata before attaching content"))?;
        let content = CapturedAssetContent {
            hash: ContentHash::new(self.publish_blob(bytes)?)?,
            bytes: bytes.len() as u64,
            format: format.into(),
            origin,
        };
        if content.origin.is_verified()
            && match slot.kind() {
                DeviceAssetKind::Ir => bytes.len() != 4096,
                DeviceAssetKind::SnapTone => !matches!(bytes.len(), 8192 | 8840),
            }
        {
            return Err(invalid(
                "Verified device asset content has no established coefficient/container length",
            ));
        }
        if asset
            .content
            .as_ref()
            .is_some_and(|previous| previous != &content)
        {
            return Err(ServiceError::new(
                ErrorKind::Conflict,
                "Asset content changed during acquisition",
            ));
        }
        asset.content = Some(content);
        self.commit_workflows(workflows)
    }
    pub fn record_backup_read_error(
        &mut self,
        id: &str,
        object: &str,
        message: &str,
    ) -> Result<(), ServiceError> {
        let mut workflows = self.workflows()?;
        acquiring(&mut workflows, id)?
            .read_errors
            .insert(object.into(), message.into());
        self.commit_workflows(workflows)
    }
    pub fn finish_device_backup(
        &mut self,
        id: &str,
        outcome: BackupAcquisition,
    ) -> Result<DeviceBackup, ServiceError> {
        if outcome == BackupAcquisition::Acquiring {
            return Err(invalid("Finish requires a terminal acquisition outcome"));
        }
        let mut workflows = self.workflows()?;
        let backup = acquiring(&mut workflows, id)?;
        backup.acquisition = outcome;
        let result = backup.clone();
        self.commit_workflows(workflows)?;
        Ok(result)
    }
    pub(crate) fn checked_captured_preset(
        &self,
        origin: &BackupOrigin,
        capture: &CapturedPreset,
        target: PresetTarget,
    ) -> Result<PresetSnapshot, ServiceError> {
        let snapshot = decode_snapshot(&self.read_blob(capture.snapshot_hash.as_str())?)?;
        check_snapshot(origin, &snapshot, target)?;
        if snapshot.revision != capture.revision
            || raw_bytes(&snapshot.raw) != self.read_blob(capture.raw_hash.as_str())?
        {
            return Err(invalid("Backup snapshot and raw blob disagree"));
        }
        Ok(snapshot)
    }
    pub(crate) fn validate_backup_blobs(&self, backup: &DeviceBackup) -> Result<(), ServiceError> {
        validate_backup_contents(backup, &|hash| self.read_blob(hash.as_str()))
    }
}
pub(crate) fn validate_backup_contents(
    backup: &DeviceBackup,
    load: &dyn Fn(&ContentHash) -> Result<Vec<u8>, ServiceError>,
) -> Result<(), ServiceError> {
    backup.validate()?;
    for (slot, capture) in backup
        .presets
        .iter()
        .map(|(slot, capture)| (PresetTarget::Stored(*slot), capture))
        .chain(
            backup
                .edit_buffer
                .iter()
                .map(|capture| (PresetTarget::EditBuffer, capture)),
        )
    {
        let snapshot = decode_snapshot(&load(&capture.snapshot_hash)?)?;
        check_snapshot(&backup.origin, &snapshot, slot)?;
        if snapshot.revision != capture.revision
            || raw_bytes(&snapshot.raw) != load(&capture.raw_hash)?
        {
            return Err(invalid("Backup snapshot and raw blob disagree"));
        }
    }
    if let Some(globals) = &backup.globals {
        let raw = parse_raw(&load(&globals.raw_hash)?)?;
        let records = GlobalRecords::from_transfer(raw).map_err(invalid)?;
        if !records.is_complete()
            || records
                .footswitch_tables
                .iter()
                .map(|table| table.table_raw())
                .collect::<Vec<_>>()
                != globals.footswitch_tables
        {
            return Err(invalid(
                "Backup globals are incomplete or mapping coverage disagrees",
            ));
        }
    }
    for asset in backup.assets.values() {
        let record: RawRecord =
            serde_json::from_slice(&load(&asset.metadata_hash)?).map_err(invalid)?;
        let metadata =
            AssetSlotMetadata::decode(asset.slot.kind.bank(), &record).map_err(invalid)?;
        if metadata.slot.index() != u16::from(asset.slot.index)
            || metadata.display_name() != asset.name
        {
            return Err(invalid("Backup asset metadata projection mismatch"));
        }
        if let Some(content) = &asset.content
            && load(&content.hash)?.len() as u64 != content.bytes
        {
            return Err(invalid("Backup asset content length mismatch"));
        }
    }
    Ok(())
}
fn acquiring<'a>(
    workflows: &'a mut crate::workflows::Workflows,
    id: &str,
) -> Result<&'a mut DeviceBackup, ServiceError> {
    let backup = workflows
        .backups
        .get_mut(id)
        .ok_or_else(|| invalid("Device backup does not exist"))?;
    if backup.acquisition != BackupAcquisition::Acquiring {
        return Err(ServiceError::new(
            ErrorKind::Conflict,
            "This acquisition is closed; partial data is retained and cannot be overwritten",
        ));
    }
    Ok(backup)
}
