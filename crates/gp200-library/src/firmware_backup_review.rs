//! Local archival assembly. The sealed receipt certifies published local bytes only.
use crate::{
    BackupAcquisition, BackupOrigin, BundleLimits, ContentHash, DeviceBackup, LibraryStore,
    backup_archive::{read_archive_file, write_archive_file},
    device_backup::{check_snapshot, parse_raw},
    store::{self, invalid, io},
};
use gp200_domain::*;
use gp200_protocol::{AssetBank, AssetSlotMetadata, GlobalRecords, RawRecord};
use serde::{Deserialize, Serialize};
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::{
    collections::BTreeSet,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::Arc,
    time::Instant,
};
const MAX: usize = FIRMWARE_BACKUP_MATERIAL_LIMIT;
fn limits() -> BundleLimits {
    BundleLimits {
        compressed_bytes: MAX as u64,
        uncompressed_bytes: MAX as u64,
        entry_bytes: 256 * 1024,
        entries: 600,
        manifest_bytes: 2 * 1024 * 1024,
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FirmwareArchiveObservation {
    pub object: FirmwareBackupObject,
    pub pass: FirmwareObservationPass,
    pub epoch: SessionEpoch,
    pub revision: Revision,
    pub sequence: u64,
    pub monotonic_millis: u64,
    pub wall_clock_millis: u64,
    /// Kind-prefixed transfers; metadata is the exact serialized RawRecord.
    pub bytes: Vec<u8>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FirmwareArchiveBoundary {
    pub epoch: SessionEpoch,
    pub identity: DeviceIdentity,
    pub device_key: String,
    pub sequence: u64,
    pub monotonic_millis: u64,
    pub wall_clock_millis: u64,
    pub raw_identity: Vec<u8>,
}
impl FirmwareArchiveBoundary {
    fn validate(&self, origin: &BackupOrigin) -> Result<(), ServiceError> {
        if self.epoch != origin.epoch
            || self.identity != origin.identity
            || self.device_key != origin.device_key
            || self.raw_identity.is_empty()
            || self.raw_identity.len() > 2049
            || self.sequence == 0
        {
            return Err(invalid(
                "Archival identity boundary differs or exceeds bounds",
            ));
        }
        Ok(())
    }
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Sidecar {
    schema: u32,
    equality_policy: String,
    acquisition: String,
    origin: BackupOrigin,
    context: FirmwareBackupReviewContext,
    opening: FirmwareArchiveBoundary,
    closing: FirmwareArchiveBoundary,
    observations: Vec<FirmwareArchiveObservation>,
    archive_sha256: String,
    backup_descriptor_sha256: String,
    summary: FirmwareBackupReviewSummary,
}
/// Non-clone working assembly; caller observations remain archival assertions.
pub struct FirmwareBackupReviewDraft {
    id: String,
    context: FirmwareBackupReviewContext,
    origin: BackupOrigin,
    opening: FirmwareArchiveBoundary,
    closing: Option<FirmwareArchiveBoundary>,
    observations: Vec<FirmwareArchiveObservation>,
    cumulative: usize,
    closed: bool,
    started: Instant,
}
impl FirmwareBackupReviewDraft {
    pub fn acquisition(&self) -> &str {
        &self.id
    }
    pub(crate) fn is_closed(&self) -> bool {
        self.closed
    }
}
/// Non-clone, non-deserializable proof of exact local publication, never physical acquisition.
pub struct PublishedFirmwareBackupReview {
    archive: PathBuf,
    sidecar: PathBuf,
    archive_hash: ContentHash,
    sidecar_hash: ContentHash,
    summary: FirmwareBackupReviewSummary,
    acquisition: String,
    origin_hash: ContentHash,
}
impl PublishedFirmwareBackupReview {
    pub fn summary(&self) -> &FirmwareBackupReviewSummary {
        &self.summary
    }
    pub fn archive_path(&self) -> &Path {
        &self.archive
    }
    pub fn sidecar_path(&self) -> &Path {
        &self.sidecar
    }
    pub fn acquisition(&self) -> &str {
        &self.acquisition
    }
    pub fn proposal(&self) -> ProposedFirmwareLossAllowance {
        ProposedFirmwareLossAllowance {
            review: self.summary.clone(),
            archive_sha256: self.archive_hash.as_str().into(),
            sidecar_sha256: self.sidecar_hash.as_str().into(),
            acquisition: self.acquisition.clone(),
            backup_origin_sha256: self.origin_hash.as_str().into(),
        }
    }
    /// No receipt is reconstructed. Reopening checks local bytes against this exact receipt.
    pub fn revalidate(&self) -> Result<(), ServiceError> {
        let (summary, archive, sidecar) = reopen(&self.archive, &self.sidecar)?;
        if archive != self.archive_hash || sidecar != self.sidecar_hash || summary != self.summary {
            return Err(invalid(
                "Published firmware review changed; create a new review",
            ));
        }
        Ok(())
    }
}
/// Independently validated imported/reopened summary. It never returns a local publication receipt.
pub fn reopen_firmware_backup_review(
    archive: &Path,
    sidecar: &Path,
) -> Result<FirmwareBackupReviewSummary, ServiceError> {
    Ok(reopen(archive, sidecar)?.0)
}
fn private_file(path: &Path) -> Result<File, ServiceError> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    options.open(path).map_err(io)
}
fn read_file(path: &Path, max: usize) -> Result<Vec<u8>, ServiceError> {
    store::reject_symlink(path)?;
    let file = File::open(path).map_err(io)?;
    if !file.metadata().map_err(io)?.is_file() || file.metadata().map_err(io)?.len() > max as u64 {
        return Err(invalid("Firmware review file exceeds bounds"));
    }
    let mut data = Vec::new();
    file.take(max as u64 + 1)
        .read_to_end(&mut data)
        .map_err(io)?;
    if data.len() > max {
        return Err(invalid("Firmware review file expanded beyond bounds"));
    }
    Ok(data)
}
fn sync_directory(path: &Path) -> Result<(), ServiceError> {
    File::open(path).and_then(|f| f.sync_all()).map_err(io)
}
fn no_clobber(
    temp: &Path,
    final_path: &Path,
    fault: &dyn Fn(&str) -> Result<(), ServiceError>,
) -> Result<(), ServiceError> {
    store::reject_symlink(final_path)?;
    fs::hard_link(temp, final_path).map_err(io)?;
    fault("before-link-directory-sync")?;
    sync_directory(final_path.parent().expect("private package parent"))?;
    fault("before-temporary-cleanup")?;
    fs::remove_file(temp).map_err(io)?;
    fault("before-cleanup-directory-sync")?;
    sync_directory(final_path.parent().expect("private package parent"))
}
impl LibraryStore {
    pub fn begin_firmware_backup_review(
        &mut self,
        origin: BackupOrigin,
        context: FirmwareBackupReviewContext,
        opening: FirmwareArchiveBoundary,
    ) -> Result<FirmwareBackupReviewDraft, ServiceError> {
        context.validate()?;
        origin.validate()?;
        opening.validate(&origin)?;
        // Automatic normal identity never acquires a manual version claim.
        if origin.identity.firmware.is_some() {
            return Err(invalid(
                "Automatic normal identity must retain unknown firmware",
            ));
        }
        let backup = self.begin_device_backup(origin.clone())?;
        reread_committed_descriptor(self)?;
        Ok(FirmwareBackupReviewDraft {
            id: backup.id,
            context,
            origin,
            opening,
            closing: None,
            observations: vec![],
            cumulative: 0,
            closed: false,
            started: Instant::now(),
        })
    }
    pub fn store_firmware_backup_observation(
        &mut self,
        draft: &mut FirmwareBackupReviewDraft,
        observation: FirmwareArchiveObservation,
    ) -> Result<ContentHash, ServiceError> {
        if draft.closed || draft.started.elapsed().as_secs() > 600 {
            return Err(invalid("Archival capture closed or soft deadline exceeded"));
        }
        validate_observation(&draft.origin, &observation)?;
        let last = draft.observations.last();
        if observation.sequence <= last.map_or(draft.opening.sequence, |v| v.sequence)
            || observation.monotonic_millis
                < last.map_or(draft.opening.monotonic_millis, |v| v.monotonic_millis)
        {
            return Err(invalid("Non-monotonic archival observation sequence/time"));
        }
        if draft
            .observations
            .iter()
            .any(|v| v.object == observation.object && v.pass == observation.pass)
            || draft.observations.len() >= 576
        {
            return Err(invalid("Duplicate or excess archival object"));
        }
        let matching = draft.observations.iter().find(|v| {
            v.object == observation.object && v.pass == FirmwareObservationPass::Baseline
        });
        if observation.pass == FirmwareObservationPass::Comparison && matching.is_none()
            || observation.pass == FirmwareObservationPass::Baseline
                && draft
                    .observations
                    .iter()
                    .any(|v| v.pass == FirmwareObservationPass::Comparison)
        {
            return Err(invalid(
                "Comparison has no baseline or baseline follows comparison",
            ));
        }
        let size = serde_json::to_vec(&observation).map_err(invalid)?.len();
        let cumulative = draft
            .cumulative
            .checked_add(size)
            .ok_or_else(|| invalid("Material size overflow"))?;
        if cumulative > MAX {
            return Err(invalid("Firmware observation material exceeds budget"));
        }
        let hash = ContentHash::of(&observation.bytes);
        if observation.pass == FirmwareObservationPass::Baseline {
            store_baseline(self, draft, &observation)?;
        }
        // Both passes durable and reread before acknowledging. Descriptor sidecar is published on close.
        if self.publish_blob(&observation.bytes)? != hash.as_str()
            || self.read_blob(hash.as_str())? != observation.bytes
        {
            return Err(invalid("Stored observation reread differs"));
        }
        reread_committed_descriptor(self)?;
        draft.cumulative = cumulative;
        draft.observations.push(observation);
        Ok(hash)
    }
    pub fn close_firmware_backup_review(
        &mut self,
        draft: &mut FirmwareBackupReviewDraft,
        closing: FirmwareArchiveBoundary,
        outcome: BackupAcquisition,
    ) -> Result<(), ServiceError> {
        closing.validate(&draft.origin)?;
        if draft.closed
            || closing.sequence
                <= draft
                    .observations
                    .last()
                    .map_or(draft.opening.sequence, |v| v.sequence)
            || closing.monotonic_millis
                < draft
                    .observations
                    .last()
                    .map_or(draft.opening.monotonic_millis, |v| v.monotonic_millis)
        {
            return Err(invalid("Invalid close boundary/order"));
        }
        if outcome == BackupAcquisition::Finished
            && !coverage_complete(&draft.observations, &draft.context)
        {
            return Err(invalid("Finished requires both exact coverage passes"));
        }
        self.finish_device_backup(&draft.id, outcome)?;
        reread_committed_descriptor(self)?;
        draft.closing = Some(closing);
        draft.closed = true;
        Ok(())
    }
    pub fn publish_firmware_backup_review(
        &self,
        draft: FirmwareBackupReviewDraft,
    ) -> Result<PublishedFirmwareBackupReview, ServiceError> {
        self.publish_firmware_backup_review_inner(draft, &|| false, &|_| Ok(()))
    }
    pub(crate) fn publish_firmware_backup_review_inner(
        &self,
        draft: FirmwareBackupReviewDraft,
        cancelled: &dyn Fn() -> bool,
        fault: &dyn Fn(&str) -> Result<(), ServiceError>,
    ) -> Result<PublishedFirmwareBackupReview, ServiceError> {
        let closing = draft
            .closing
            .ok_or_else(|| invalid("Close capture before publication"))?;
        if !draft.closed {
            return Err(invalid("Capture is not closed"));
        }
        let backup = self.device_backup(&draft.id)?;
        if backup.origin != draft.origin || backup.acquisition == BackupAcquisition::Acquiring {
            return Err(invalid("Backup origin/state changed"));
        }
        validate_baseline_links(&backup, &draft.observations)?;
        let summary = build_summary(&draft.context, &draft.observations, &backup)?;
        let base = self.root.join("firmware-backup-reviews");
        store::reject_symlink(&base)?;
        fs::create_dir_all(&base).map_err(io)?;
        #[cfg(unix)]
        fs::set_permissions(&base, fs::Permissions::from_mode(0o700)).map_err(io)?;
        fault("before-authority-root-directory-sync")?;
        sync_directory(&self.root)?;
        let directory = base.join(store::nonce()?);
        fs::create_dir(&directory).map_err(io)?;
        #[cfg(unix)]
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).map_err(io)?;
        fault("before-package-parent-directory-sync")?;
        sync_directory(&base)?;
        let archive_temp = directory.join("archive.tmp");
        let archive = directory.join("baseline.gp200backup");
        let sidecar_temp = directory.join("review.tmp");
        let sidecar = directory.join("review.json");
        let check = |point: &str| {
            if cancelled() {
                return Err(ServiceError::new(
                    ErrorKind::Cancelled,
                    "Firmware archival publication stopped; partial files retained",
                ));
            }
            fault(point)
        };
        check("before-archive-write")?;
        let file = write_archive_file(
            self,
            &backup,
            private_file(&archive_temp)?,
            limits(),
            cancelled,
        )?;
        check("before-archive-sync")?;
        file.sync_all().map_err(io)?;
        check("before-archive-reread")?;
        let archive_bytes = read_file(&archive_temp, MAX)?;
        let checked =
            read_archive_file(File::open(&archive_temp).map_err(io)?, limits(), cancelled)?;
        if serde_json::to_vec(&checked.backup).map_err(invalid)?
            != serde_json::to_vec(&backup).map_err(invalid)?
        {
            return Err(invalid("Independent archive descriptor differs"));
        }
        let archive_hash = ContentHash::of(&archive_bytes);
        let origin_hash = ContentHash::of(&serde_json::to_vec(&backup.origin).map_err(invalid)?);
        let manifest = Sidecar {
            schema: 1,
            equality_policy: "gp200-fw04-a-archival-exact-bytes-v1".into(),
            acquisition: draft.id.clone(),
            origin: draft.origin,
            context: draft.context,
            opening: draft.opening,
            closing,
            observations: draft.observations,
            archive_sha256: archive_hash.as_str().into(),
            backup_descriptor_sha256: ContentHash::of(
                &serde_json::to_vec(&backup).map_err(invalid)?,
            )
            .as_str()
            .into(),
            summary: summary.clone(),
        };
        let sidecar_bytes = serde_json::to_vec(&manifest).map_err(invalid)?;
        if archive_bytes
            .len()
            .checked_add(sidecar_bytes.len())
            .is_none_or(|v| v > MAX)
        {
            return Err(invalid("Authority archive plus transcript exceeds budget"));
        }
        check("before-sidecar-write")?;
        let mut file = private_file(&sidecar_temp)?;
        file.write_all(&sidecar_bytes).map_err(io)?;
        check("before-sidecar-sync")?;
        file.sync_all().map_err(io)?;
        check("before-sidecar-reread")?;
        if read_file(&sidecar_temp, MAX)? != sidecar_bytes {
            return Err(invalid("Sidecar reread differs"));
        }
        check("before-archive-publication")?;
        no_clobber(&archive_temp, &archive, fault)?;
        check("before-sidecar-publication")?;
        no_clobber(&sidecar_temp, &sidecar, fault)?;
        check("before-final-directory-sync")?;
        sync_directory(&directory)?;
        check("before-final-reread")?;
        let (independent, ah, sh) = reopen(&archive, &sidecar)?;
        if independent != summary || ah != archive_hash || sh != ContentHash::of(&sidecar_bytes) {
            return Err(invalid("Final publication pins differ"));
        }
        Ok(PublishedFirmwareBackupReview {
            archive,
            sidecar,
            archive_hash,
            sidecar_hash: sh,
            summary,
            acquisition: draft.id,
            origin_hash,
        })
    }
}

/// Reopen the existing writer's committed descriptor, never a second writer/store.
fn reread_committed_descriptor(store: &LibraryStore) -> Result<(), ServiceError> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Envelope {
        format: String,
        sha256: String,
        manifest: crate::store::Manifest,
    }
    let bytes = read_file(
        &store.root.join("manifest.json"),
        crate::store::MAX_MANIFEST as usize,
    )?;
    let e: Envelope = serde_json::from_slice(&bytes).map_err(invalid)?;
    let actual = serde_json::to_vec(&e.manifest).map_err(invalid)?;
    if e.format != "gp200-studio.library"
        || e.manifest.schema_version != crate::SCHEMA_VERSION
        || e.sha256 != ContentHash::of(&actual).as_str()
        || actual != serde_json::to_vec(&store.manifest).map_err(invalid)?
    {
        return Err(ServiceError::new(
            ErrorKind::Indeterminate,
            "Committed archival descriptor changed during reread",
        ));
    }
    Ok(())
}
fn validate_observation(
    origin: &BackupOrigin,
    o: &FirmwareArchiveObservation,
) -> Result<(), ServiceError> {
    if !o.object.valid()
        || o.epoch != origin.epoch
        || o.sequence == 0
        || o.bytes.is_empty()
        || o.bytes.len() > 65537
    {
        return Err(invalid("Invalid bounded archival object/epoch"));
    }
    match o.object {
        FirmwareBackupObject::Stored(_) | FirmwareBackupObject::Current => {
            let raw = parse_raw(&o.bytes)?;
            if raw.kind() != 0x12 {
                return Err(invalid("Expected read reply kind 0x12"));
            }
            let target = match o.object {
                FirmwareBackupObject::Stored(index) => {
                    PresetTarget::Stored(SlotId::new(index).map_err(invalid)?)
                }
                _ => PresetTarget::EditBuffer,
            };
            let source = match origin.identity.mode {
                BackendMode::Simulator => SnapshotSource::Simulator,
                _ => SnapshotSource::Hardware { epoch: o.epoch },
            };
            let snapshot =
                PresetSnapshot::from_raw(Arc::new(raw), source, Some(target), o.revision);
            check_snapshot(origin, &snapshot, target)?;
            if serde_json::to_vec(&snapshot).map_err(invalid)?.len() > 256 * 1024 {
                return Err(invalid("Snapshot JSON exceeds bound"));
            }
        }
        FirmwareBackupObject::Globals => {
            let raw = parse_raw(&o.bytes)?;
            if raw.kind() != 0x12 {
                return Err(invalid("Expected global read reply kind"));
            }
            let records = GlobalRecords::from_transfer(raw).map_err(invalid)?;
            if !records.is_complete()
                || records
                    .footswitch_tables
                    .iter()
                    .map(|v| v.table_raw())
                    .collect::<BTreeSet<_>>()
                    != BTreeSet::from([0, 1, 2])
                || records.footswitch_tables.len() != 3
            {
                return Err(invalid("Globals missing complete mapping tables 0/1/2"));
            }
        }
        FirmwareBackupObject::IrMetadata(i) | FirmwareBackupObject::SnapToneMetadata(i) => {
            if o.bytes.len() > 8192 {
                return Err(invalid("Metadata exceeds 8 KiB"));
            }
            let raw: RawRecord = serde_json::from_slice(&o.bytes).map_err(invalid)?;
            if serde_json::to_vec(&raw).map_err(invalid)? != o.bytes {
                return Err(invalid(
                    "Metadata observation must use exact canonical RawRecord serialization",
                ));
            }
            let bank = match o.object {
                FirmwareBackupObject::IrMetadata(_) => AssetBank::Ir,
                _ => AssetBank::SnapTone,
            };
            let m = AssetSlotMetadata::decode(bank, &raw).map_err(invalid)?;
            if m.slot.index() != u16::from(i) {
                return Err(invalid("Metadata slot differs"));
            }
        }
    }
    Ok(())
}
fn store_baseline(
    store: &mut LibraryStore,
    draft: &FirmwareBackupReviewDraft,
    o: &FirmwareArchiveObservation,
) -> Result<(), ServiceError> {
    match o.object {
        FirmwareBackupObject::Stored(_) | FirmwareBackupObject::Current => {
            let target = if let FirmwareBackupObject::Stored(i) = o.object {
                PresetTarget::Stored(SlotId::new(i).map_err(invalid)?)
            } else {
                PresetTarget::EditBuffer
            };
            let source = if draft.origin.identity.mode == BackendMode::Simulator {
                SnapshotSource::Simulator
            } else {
                SnapshotSource::Hardware { epoch: o.epoch }
            };
            store.capture_backup_preset(
                &draft.id,
                Arc::new(PresetSnapshot::from_raw(
                    Arc::new(parse_raw(&o.bytes)?),
                    source,
                    Some(target),
                    o.revision,
                )),
            )?;
        }
        FirmwareBackupObject::Globals => store.capture_backup_globals(
            &draft.id,
            &DeviceGlobalState::from_transfer(o.revision, o.epoch, parse_raw(&o.bytes)?, vec![])
                .map_err(invalid)?,
        )?,
        _ => {
            let record: RawRecord = serde_json::from_slice(&o.bytes).map_err(invalid)?;
            let bank = if matches!(o.object, FirmwareBackupObject::IrMetadata(_)) {
                AssetBank::Ir
            } else {
                AssetBank::SnapTone
            };
            store.capture_backup_asset_metadata(
                &draft.id,
                o.epoch,
                &AssetSlotMetadata::decode(bank, &record).map_err(invalid)?,
            )?;
        }
    }
    Ok(())
}
fn expected(context: &FirmwareBackupReviewContext) -> BTreeSet<FirmwareBackupObject> {
    (0..256)
        .map(FirmwareBackupObject::Stored)
        .chain([FirmwareBackupObject::Current, FirmwareBackupObject::Globals])
        .chain((0..20).map(FirmwareBackupObject::IrMetadata))
        .chain(
            if matches!(
                context.snaptone_applicability(),
                FirmwareAssetApplicability::ConditionalNotApplicable { .. }
            ) {
                vec![]
            } else {
                (0..10)
                    .map(FirmwareBackupObject::SnapToneMetadata)
                    .collect()
            },
        )
        .collect()
}
fn coverage_complete(
    observations: &[FirmwareArchiveObservation],
    context: &FirmwareBackupReviewContext,
) -> bool {
    expected(context).into_iter().all(|key| {
        let a = observations
            .iter()
            .find(|o| o.pass == FirmwareObservationPass::Baseline && o.object == key);
        let b = observations
            .iter()
            .find(|o| o.pass == FirmwareObservationPass::Comparison && o.object == key);
        a.zip(b).is_some_and(|(a, b)| a.bytes == b.bytes)
    })
}
fn validate_baseline_links(
    backup: &DeviceBackup,
    obs: &[FirmwareArchiveObservation],
) -> Result<(), ServiceError> {
    let mut seen = BTreeSet::new();
    for o in obs
        .iter()
        .filter(|o| o.pass == FirmwareObservationPass::Baseline)
    {
        if !seen.insert(o.object) {
            return Err(invalid("Duplicate baseline object"));
        }
        let hash = ContentHash::of(&o.bytes);
        let expected = match o.object {
            FirmwareBackupObject::Stored(i) => backup
                .presets
                .get(&SlotId::new(i).map_err(invalid)?)
                .map(|p| &p.raw_hash),
            FirmwareBackupObject::Current => backup.edit_buffer.as_ref().map(|p| &p.raw_hash),
            FirmwareBackupObject::Globals => backup.globals.as_ref().map(|g| &g.raw_hash),
            FirmwareBackupObject::IrMetadata(i) => backup
                .assets
                .get(&format!("ir-{i:02}"))
                .map(|v| &v.metadata_hash),
            FirmwareBackupObject::SnapToneMetadata(i) => backup
                .assets
                .get(&format!("snaptone-{i:02}"))
                .map(|v| &v.metadata_hash),
        };
        match o.object {
            FirmwareBackupObject::Stored(i) => {
                let capture = backup
                    .presets
                    .get(&SlotId::new(i).map_err(invalid)?)
                    .ok_or_else(|| invalid("Missing stored descriptor"))?;
                if capture.revision != o.revision || capture.slot.get() != i {
                    return Err(invalid(
                        "Stored archival projection differs from observation",
                    ));
                }
            }
            FirmwareBackupObject::Current => {
                let capture = backup
                    .edit_buffer
                    .as_ref()
                    .ok_or_else(|| invalid("Missing current descriptor"))?;
                let raw = parse_raw(&o.bytes)?;
                let context = gp200_protocol::WireContext::from_transfer(&raw)
                    .ok_or_else(|| invalid("Current observation has no unique context"))?;
                if capture.slot.get() != context.selector || capture.revision != o.revision {
                    return Err(invalid(
                        "Current archival projection differs from observation",
                    ));
                }
            }
            FirmwareBackupObject::Globals
                if backup
                    .globals
                    .as_ref()
                    .is_none_or(|g| g.revision != o.revision) =>
            {
                return Err(invalid(
                    "Global archival projection differs from observation",
                ));
            }
            _ => {}
        }
        if expected != Some(&hash) {
            return Err(invalid(
                "Baseline raw pin disagrees with independent archive",
            ));
        }
    }
    if seen.len()
        != backup.presets.len()
            + usize::from(backup.edit_buffer.is_some())
            + usize::from(backup.globals.is_some())
            + backup.assets.len()
    {
        return Err(invalid(
            "Archive contains objects outside baseline transcript",
        ));
    }
    Ok(())
}
fn build_summary(
    context: &FirmwareBackupReviewContext,
    obs: &[FirmwareArchiveObservation],
    backup: &DeviceBackup,
) -> Result<FirmwareBackupReviewSummary, ServiceError> {
    context.validate()?;
    let mut coverage = Vec::new();
    let mut diagnostics = backup
        .read_errors
        .iter()
        .map(|(key, value)| format!("{key}: {value}"))
        .collect::<Vec<_>>();
    let mut references = Vec::new();
    let metadata = gp200_catalog::CheckedEmbeddedMetadata::embedded().map_err(invalid)?;
    let profile = context
        .manual_version
        .as_ref()
        .and_then(|v| v.suggested_metadata_profile(&context.device));
    for object in (0..256)
        .map(FirmwareBackupObject::Stored)
        .chain([FirmwareBackupObject::Current, FirmwareBackupObject::Globals])
        .chain((0..20).map(FirmwareBackupObject::IrMetadata))
        .chain((0..10).map(FirmwareBackupObject::SnapToneMetadata))
    {
        let a = obs
            .iter()
            .find(|o| o.object == object && o.pass == FirmwareObservationPass::Baseline);
        let b = obs
            .iter()
            .find(|o| o.object == object && o.pass == FirmwareObservationPass::Comparison);
        let asset = matches!(
            object,
            FirmwareBackupObject::IrMetadata(_) | FirmwareBackupObject::SnapToneMetadata(_)
        );
        let applicability = if matches!(object, FirmwareBackupObject::SnapToneMetadata(_)) {
            let mut applicability = context.snaptone_applicability();
            if let FirmwareAssetApplicability::ConditionalNotApplicable { source, .. } =
                &mut applicability
            {
                let note = metadata.release_note_source();
                let facts = metadata
                    .vendor_minima()
                    .iter()
                    .filter(|fact| {
                        (0x0f00_0000..=0x0f00_0009).contains(&fact.key().code)
                            && fact.minimum()
                                == FirmwareVersion::new(1, 7, 4).expect("fixed version")
                    })
                    .count();
                if facts != 10 {
                    return Err(invalid("SnapTone metadata minimum evidence changed"));
                }
                *source = format!(
                    "{}; SHA-256 {}; ten AMP/DST metadata minima at GP-200 V1.7.4 NAM note; physical version unknown",
                    note.file(),
                    note.sha256()
                );
            }
            applicability
        } else {
            FirmwareAssetApplicability::Applicable
        };
        let exact_equal = a.zip(b).is_some_and(|(a, b)| a.bytes == b.bytes);
        if a.zip(b).is_some() && !exact_equal {
            diagnostics.push(format!(
                "{object:?}: raw comparison changed; no tolerance profile applied"
            ));
        }
        let restoration = if matches!(
            applicability,
            FirmwareAssetApplicability::ConditionalNotApplicable { .. }
        ) {
            FirmwareRestorationStatus::ConditionalNotApplicable
        } else if asset || a.is_none() {
            FirmwareRestorationStatus::UnavailableSource
        } else if context
            .manual_version
            .as_ref()
            .is_none_or(|v| v.version() != context.destination.version)
        {
            FirmwareRestorationStatus::DestinationMigrationRequired
        } else {
            FirmwareRestorationStatus::MissingRestorationAcceptance
        };
        coverage.push(FirmwareBackupCoverageItem {
            object,
            baseline_sha256: a.map(|v| ContentHash::of(&v.bytes).as_str().into()),
            comparison_sha256: b.map(|v| ContentHash::of(&v.bytes).as_str().into()),
            exact_equal,
            installed_content_available: false,
            applicability,
            restoration,
        });
        if let Some(a) = a
            && matches!(
                object,
                FirmwareBackupObject::Stored(_) | FirmwareBackupObject::Current
            )
        {
            let raw = parse_raw(&a.bytes)?;
            for record in raw.records().iter().filter(|v| v.command == 0x14) {
                let module = record.body[0];
                let code =
                    u32::from_le_bytes(record.body[4..8].try_into().expect("validated module"));
                let classification = if module == 5 && (0x0a10_0000..=0x0a10_0013).contains(&code) {
                    format!("User IR {}", code - 0x0a10_0000 + 1)
                } else if module == 3 && (0x0f00_0000..=0x0f00_0004).contains(&code)
                    || module == 2 && (0x0f00_0005..=0x0f00_0009).contains(&code)
                {
                    format!("SnapTone {}", code - 0x0f00_0000 + 1)
                } else if profile.is_some_and(|p| {
                    metadata
                        .view(p)
                        .model(gp200_catalog::ModelKey { module, code })
                        .is_some()
                }) {
                    "Known starting metadata code; wire restoration unaccepted".into()
                } else {
                    "Unknown starting model reference".into()
                };
                references.push(FirmwareBackupAssetReference {
                    object,
                    module,
                    code,
                    classification,
                });
            }
        }
    }
    if diagnostics.len() > 512 || diagnostics.iter().any(|v| v.len() > 4096) {
        return Err(invalid("Review diagnostics exceed bound"));
    }
    let comparison_complete = coverage_complete(obs, context)
        && backup.read_errors.is_empty()
        && backup.acquisition == BackupAcquisition::Finished;
    Ok(FirmwareBackupReviewSummary{context:context.clone(),stored_presets:backup.presets.len(),current_buffer:backup.edit_buffer.is_some(),globals_and_mappings:backup.globals.is_some(),comparison_complete,label:if comparison_complete{"Stored presets, current buffer and globals archived; IR audio unavailable"}else{"Partial archival capture; IR audio unavailable"}.into(),non_atomic_limitation:"Two matching observations of a non-atomic device cannot exclude unseen A-to-B-to-A changes or establish a simultaneous bank snapshot; supplied observations remain archival claims".into(),coverage,references,diagnostics})
}
fn reopen(
    archive: &Path,
    sidecar: &Path,
) -> Result<(FirmwareBackupReviewSummary, ContentHash, ContentHash), ServiceError> {
    let bytes = read_file(archive, MAX)?;
    let side = read_file(sidecar, MAX)?;
    if bytes.len().checked_add(side.len()).is_none_or(|v| v > MAX) {
        return Err(invalid("Review material exceeds bound"));
    }
    let manifest: Sidecar = serde_json::from_slice(&side).map_err(invalid)?;
    if manifest.schema != 1
        || manifest.equality_policy != "gp200-fw04-a-archival-exact-bytes-v1"
        || manifest.observations.len() > 576
    {
        return Err(invalid("Unsupported or oversized review sidecar"));
    }
    manifest.origin.validate()?;
    manifest.context.validate()?;
    manifest.opening.validate(&manifest.origin)?;
    manifest.closing.validate(&manifest.origin)?;
    let mut previous = manifest.opening.sequence;
    let mut time = manifest.opening.monotonic_millis;
    let mut seen = BTreeSet::new();
    let mut comparison = false;
    for o in &manifest.observations {
        validate_observation(&manifest.origin, o)?;
        if o.sequence <= previous
            || o.monotonic_millis < time
            || !seen.insert((o.pass, o.object))
            || o.pass == FirmwareObservationPass::Baseline && comparison
            || o.pass == FirmwareObservationPass::Comparison
                && !seen.contains(&(FirmwareObservationPass::Baseline, o.object))
        {
            return Err(invalid("Sidecar observation order/coverage invalid"));
        }
        comparison |= o.pass == FirmwareObservationPass::Comparison;
        previous = o.sequence;
        time = o.monotonic_millis;
    }
    if manifest.closing.sequence <= previous || manifest.closing.monotonic_millis < time {
        return Err(invalid("Sidecar close boundary invalid"));
    }
    let checked = read_archive_file(File::open(archive).map_err(io)?, limits(), &|| false)?;
    if read_file(archive, MAX)? != bytes || read_file(sidecar, MAX)? != side {
        return Err(invalid("Review file changed during independent reopen"));
    }
    validate_baseline_links(&checked.backup, &manifest.observations)?;
    let descriptor = serde_json::to_vec(&checked.backup).map_err(invalid)?;
    if ContentHash::of(&bytes).as_str() != manifest.archive_sha256
        || ContentHash::of(&descriptor).as_str() != manifest.backup_descriptor_sha256
        || checked.backup.origin != manifest.origin
        || checked.backup.id != manifest.acquisition
        || checked.backup.acquisition == BackupAcquisition::Acquiring
    {
        return Err(invalid("Sidecar/archive descriptor or file pins disagree"));
    }
    for o in manifest
        .observations
        .iter()
        .filter(|o| o.pass == FirmwareObservationPass::Baseline)
    {
        let hash = ContentHash::of(&o.bytes);
        if checked.blobs.get(&hash) != Some(&o.bytes) {
            return Err(invalid("Archive baseline bytes differ"));
        }
    }
    let summary = build_summary(&manifest.context, &manifest.observations, &checked.backup)?;
    if summary != manifest.summary {
        return Err(invalid("Derived coverage/restoration summary differs"));
    }
    Ok((summary, ContentHash::of(&bytes), ContentHash::of(&side)))
}

#[cfg(test)]
mod fault_tests {
    // Actual boundary fault tests are housed here so fault hooks remain crate-private.
    use super::*;
    #[test]
    fn no_clobber_keeps_existing_bytes() {
        let dir = std::env::temp_dir().join(format!("fw04-link-{}", store::nonce().unwrap()));
        fs::create_dir(&dir).unwrap();
        let temp = dir.join("temp");
        let final_path = dir.join("final");
        fs::write(&temp, b"new").unwrap();
        fs::write(&final_path, b"old").unwrap();
        assert!(no_clobber(&temp, &final_path, &|_| Ok(())).is_err());
        assert_eq!(fs::read(&final_path).unwrap(), b"old");
        fs::remove_dir_all(dir).unwrap();
    }
    fn draft(store: &mut LibraryStore) -> FirmwareBackupReviewDraft {
        let identity = DeviceIdentity {
            model: DeviceModel::Gp200,
            mode: BackendMode::Simulator,
            firmware: None,
            display_name: "Fixture".into(),
        };
        let origin = BackupOrigin {
            identity: identity.clone(),
            epoch: SessionEpoch(1),
            device_key: "fixture".into(),
            evidence: vec![],
        };
        let context = FirmwareBackupReviewContext {
            device: FirmwareDeviceAssociation::named_local_record("GP-200", "fixture").unwrap(),
            manual_version: None,
            destination: FirmwareBackupDestination {
                model: "GP-200".into(),
                version: FirmwareVersion::new(1, 8, 2).unwrap(),
                image_sha256: "a".repeat(64),
                restoration_profile: None,
            },
            local_material: vec![],
            preservation_uncertainty: "unknown".into(),
            recovery_limitations: "unknown".into(),
        };
        let boundary = |sequence| FirmwareArchiveBoundary {
            epoch: SessionEpoch(1),
            identity: identity.clone(),
            device_key: "fixture".into(),
            sequence,
            monotonic_millis: sequence,
            wall_clock_millis: sequence,
            raw_identity: vec![1],
        };
        let mut d = store
            .begin_firmware_backup_review(origin, context, boundary(1))
            .unwrap();
        store
            .close_firmware_backup_review(&mut d, boundary(2), BackupAcquisition::Cancelled)
            .unwrap();
        d
    }
    #[test]
    fn every_publication_boundary_failure_withholds_receipt_and_retains_files() {
        for point in [
            "before-authority-root-directory-sync",
            "before-package-parent-directory-sync",
            "before-archive-write",
            "before-archive-sync",
            "before-archive-reread",
            "before-sidecar-write",
            "before-sidecar-sync",
            "before-sidecar-reread",
            "before-archive-publication",
            "before-sidecar-publication",
            "before-link-directory-sync",
            "before-temporary-cleanup",
            "before-cleanup-directory-sync",
            "before-final-directory-sync",
            "before-final-reread",
        ] {
            let dir = std::env::temp_dir().join(format!("fw04-fault-{}", store::nonce().unwrap()));
            let mut store = LibraryStore::open(&dir).unwrap();
            let d = draft(&mut store);
            let hit = std::cell::Cell::new(false);
            let result = store.publish_firmware_backup_review_inner(d, &|| false, &|p| {
                if p == point {
                    hit.set(true);
                    Err(ServiceError::new(
                        ErrorKind::Io,
                        "Injected publication I/O failure",
                    ))
                } else {
                    Ok(())
                }
            });
            assert!(hit.get(), "{point}");
            assert!(result.is_err(), "{point}");
            assert_eq!(store.device_backups().unwrap().len(), 1);
            drop(store);
            fs::remove_dir_all(dir).unwrap();
        }
    }
    #[cfg(unix)]
    #[test]
    fn no_clobber_rejects_symbolic_destination() {
        use std::os::unix::fs::symlink;
        let dir = std::env::temp_dir().join(format!("fw04-link-{}", store::nonce().unwrap()));
        fs::create_dir(&dir).unwrap();
        fs::write(dir.join("temp"), b"new").unwrap();
        fs::write(dir.join("original"), b"old").unwrap();
        symlink(dir.join("original"), dir.join("final")).unwrap();
        assert!(no_clobber(&dir.join("temp"), &dir.join("final"), &|_| Ok(())).is_err());
        assert_eq!(fs::read(dir.join("original")).unwrap(), b"old");
        fs::remove_dir_all(dir).unwrap();
    }
}
