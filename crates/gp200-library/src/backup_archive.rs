use crate::{
    BundleLimits, ContentHash, DeviceBackup, LibraryStore,
    bundle::{audit_directory, normalized_entry},
    device_backup::validate_backup_contents,
    store::{self, invalid, io},
    workflows::new_id,
};
use gp200_domain::*;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::Path,
};
use zip::{CompressionMethod, ZipArchive, ZipWriter, write::SimpleFileOptions};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BackupArchive {
    format: String,
    schema_version: u32,
    backup: DeviceBackup,
    blobs: BTreeMap<String, u64>,
}
pub(crate) struct CheckedBackupArchive {
    pub(crate) backup: DeviceBackup,
    pub(crate) blobs: BTreeMap<ContentHash, Vec<u8>>,
}

impl LibraryStore {
    /// Independent application format. Native .prst remains a separate file adapter.
    pub fn export_device_backup(
        &self,
        id: &str,
        path: impl AsRef<Path>,
    ) -> Result<crate::BackupCompleteness, ServiceError> {
        self.export_device_backup_cancellable(id, path.as_ref(), &|| false)
    }
    pub fn export_device_backup_cancellable(
        &self,
        id: &str,
        path: &Path,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<crate::BackupCompleteness, ServiceError> {
        check_cancel(cancelled)?;
        if path
            .extension()
            .is_none_or(|extension| extension != "gp200backup")
        {
            return Err(invalid("Device bank archives use .gp200backup"));
        }
        let backup = self.device_backup(id)?;
        self.validate_backup_blobs(&backup)?;
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent).map_err(io)?;
        store::reject_symlink(path)?;
        let temporary = parent.join(format!(".device-backup-{}.tmp", store::nonce()?));
        let result = (|| {
            let file = write_archive_file(
                self,
                &backup,
                OpenOptions::new()
                    .create_new(true)
                    .write(true)
                    .open(&temporary)
                    .map_err(io)?,
                BundleLimits::default(),
                cancelled,
            )?;
            file.sync_all().map_err(io)?;
            check_cancel(cancelled)?;
            fs::rename(&temporary, path).map_err(io)?;
            File::open(parent)
                .and_then(|file| file.sync_all())
                .map_err(io)?;
            Ok(backup.completeness())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }
    pub fn preview_device_backup(
        path: impl AsRef<Path>,
        limits: BundleLimits,
    ) -> Result<DeviceBackup, ServiceError> {
        Ok(read_archive(path.as_ref(), limits, &|| false)?.backup)
    }
    pub fn import_device_backup(
        &mut self,
        path: impl AsRef<Path>,
    ) -> Result<DeviceBackup, ServiceError> {
        self.import_device_backup_cancellable(path.as_ref(), &|| false)
    }
    pub fn import_device_backup_cancellable(
        &mut self,
        path: &Path,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<DeviceBackup, ServiceError> {
        let checked = read_archive(path, BundleLimits::default(), cancelled)?;
        let mut backup = checked.backup;
        backup.id = new_id("backup")?;
        // Imported acquisitions are archival. Reconnect requires a new session-bound acquisition.
        if backup.acquisition == crate::BackupAcquisition::Acquiring {
            backup.acquisition = crate::BackupAcquisition::Cancelled;
        }
        for (hash, bytes) in checked.blobs {
            check_cancel(cancelled)?;
            if self.publish_blob(&bytes)? != hash.as_str() {
                return Err(invalid("Imported archive blob changed"));
            }
        }
        check_cancel(cancelled)?;
        let mut workflows = self.workflows()?;
        workflows.backups.insert(backup.id.clone(), backup.clone());
        self.commit_workflows(workflows)?;
        Ok(backup)
    }
}
fn check_cancel(cancelled: &dyn Fn() -> bool) -> Result<(), ServiceError> {
    if cancelled() {
        Err(ServiceError::new(
            ErrorKind::Cancelled,
            "Device archive cancelled; no new backup was published",
        ))
    } else {
        Ok(())
    }
}
fn read_archive(
    path: &Path,
    limits: BundleLimits,
    cancelled: &dyn Fn() -> bool,
) -> Result<CheckedBackupArchive, ServiceError> {
    check_cancel(cancelled)?;
    store::reject_symlink(path)?;
    read_archive_file(File::open(path).map_err(io)?, limits, cancelled)
}
pub(crate) fn read_archive_file(
    mut file: File,
    limits: BundleLimits,
    cancelled: &dyn Fn() -> bool,
) -> Result<CheckedBackupArchive, ServiceError> {
    check_cancel(cancelled)?;
    let metadata = file.metadata().map_err(io)?;
    if !metadata.is_file() || metadata.len() > limits.compressed_bytes {
        return Err(invalid("Device archive exceeds compressed bounds"));
    }
    audit_directory(&mut file, metadata.len(), limits)?;
    file.seek(SeekFrom::Start(0)).map_err(io)?;
    let mut archive = ZipArchive::new(file).map_err(invalid)?;
    if archive.len() > limits.entries {
        return Err(invalid("Too many device archive entries"));
    }
    let mut names = BTreeSet::new();
    let mut total = 0u64;
    for index in 0..archive.len() {
        let entry = archive.by_index(index).map_err(invalid)?;
        normalized_entry(entry.name())?;
        if !names.insert(entry.name().to_owned())
            || entry.is_dir()
            || entry
                .unix_mode()
                .is_some_and(|mode| mode & 0o170000 == 0o120000)
            || entry.size() > limits.entry_bytes
        {
            return Err(invalid(
                "Duplicate, symbolic-link or oversized device archive entry",
            ));
        }
        total = total
            .checked_add(entry.size())
            .ok_or_else(|| invalid("Device archive length overflow"))?;
        if total > limits.uncompressed_bytes {
            return Err(invalid("Device archive exceeds total decompression bound"));
        }
    }
    let read_entry = |archive: &mut ZipArchive<File>,
                      name: &str,
                      maximum: u64|
     -> Result<Vec<u8>, ServiceError> {
        check_cancel(cancelled)?;
        let entry = archive.by_name(name).map_err(invalid)?;
        if entry.size() > maximum {
            return Err(invalid("Device archive entry exceeds bound"));
        }
        let mut bytes = Vec::new();
        entry
            .take(maximum.saturating_add(1))
            .read_to_end(&mut bytes)
            .map_err(io)?;
        if bytes.len() as u64 > maximum {
            return Err(invalid("Device archive decompressed beyond its bound"));
        }
        Ok(bytes)
    };
    let manifest: BackupArchive = serde_json::from_slice(&read_entry(
        &mut archive,
        "manifest.json",
        limits.manifest_bytes,
    )?)
    .map_err(invalid)?;
    if manifest.format != "gp200-studio.device-backup" || manifest.schema_version != 1 {
        return Err(ServiceError::unsupported(
            "Unsupported device archive format/schema",
        ));
    }
    manifest.backup.validate()?;
    let referenced = manifest.backup.blob_hashes();
    if referenced.len() != manifest.blobs.len()
        || manifest
            .blobs
            .keys()
            .any(|hash| ContentHash::new(hash).is_err())
    {
        return Err(invalid("Device archive blob references disagree"));
    }
    let expected_names = referenced
        .iter()
        .map(|hash| format!("blobs/{}", hash.as_str()))
        .chain(std::iter::once("manifest.json".to_owned()))
        .collect::<BTreeSet<_>>();
    if names != expected_names {
        return Err(invalid(
            "Device archive has missing or unreferenced entries",
        ));
    }
    let mut blobs = BTreeMap::new();
    for hash in referenced {
        let length = *manifest
            .blobs
            .get(hash.as_str())
            .ok_or_else(|| invalid("Device archive is missing a referenced blob descriptor"))?;
        if length > limits.entry_bytes {
            return Err(invalid("Device archive blob exceeds bounds"));
        }
        let bytes = read_entry(&mut archive, &format!("blobs/{}", hash.as_str()), length)?;
        if bytes.len() as u64 != length || ContentHash::of(&bytes) != hash {
            return Err(invalid("Device archive blob length/hash mismatch"));
        }
        blobs.insert(hash, bytes);
    }
    validate_backup_contents(&manifest.backup, &|hash| {
        blobs
            .get(hash)
            .cloned()
            .ok_or_else(|| invalid("Missing checked archive blob"))
    })?;
    Ok(CheckedBackupArchive {
        backup: manifest.backup,
        blobs,
    })
}

pub(crate) fn write_archive_file(
    store: &LibraryStore,
    backup: &DeviceBackup,
    file: File,
    limits: BundleLimits,
    cancelled: &dyn Fn() -> bool,
) -> Result<File, ServiceError> {
    check_cancel(cancelled)?;
    store.validate_backup_blobs(backup)?;
    let mut blobs = BTreeMap::new();
    for hash in backup.blob_hashes() {
        blobs.insert(
            hash.as_str().to_owned(),
            store.read_blob(hash.as_str())?.len() as u64,
        );
    }
    let archive = BackupArchive {
        format: "gp200-studio.device-backup".into(),
        schema_version: 1,
        backup: backup.clone(),
        blobs,
    };
    let bytes = serde_json::to_vec(&archive).map_err(invalid)?;
    if bytes.len() as u64 > limits.manifest_bytes
        || archive.blobs.len() + 1 > limits.entries
        || archive
            .blobs
            .values()
            .try_fold(bytes.len() as u64, |total, length| {
                total.checked_add(*length)
            })
            .is_none_or(|total| total > limits.uncompressed_bytes)
    {
        return Err(invalid("Device archive exceeds export bounds"));
    }
    let mut writer = ZipWriter::new(file);
    let options = SimpleFileOptions::default()
        .compression_method(CompressionMethod::Deflated)
        .unix_permissions(0o600);
    writer
        .start_file("manifest.json", options)
        .map_err(invalid)?;
    writer.write_all(&bytes).map_err(io)?;
    for hash in archive.blobs.keys() {
        check_cancel(cancelled)?;
        writer
            .start_file(format!("blobs/{hash}"), options)
            .map_err(invalid)?;
        writer.write_all(&store.read_blob(hash)?).map_err(io)?;
    }
    let file = writer.finish().map_err(invalid)?;
    if file.metadata().map_err(io)?.len() > limits.compressed_bytes {
        return Err(invalid("Compressed device archive exceeds bounds"));
    }
    Ok(file)
}
