use crate::{
    LibraryStore, StoredAsset, dependency_report,
    store::{
        self, Manifest, StoredEntry, StoredVersion, digest, invalid, io, valid_hash, validate_graph,
    },
};
use gp200_domain::*;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Component, Path},
};
use zip::{CompressionMethod, ZipArchive, ZipWriter, write::SimpleFileOptions};

#[derive(Debug, Clone, Copy)]
pub struct BundleLimits {
    pub compressed_bytes: u64,
    pub uncompressed_bytes: u64,
    pub entry_bytes: u64,
    pub entries: usize,
    pub manifest_bytes: u64,
}
impl Default for BundleLimits {
    fn default() -> Self {
        Self {
            compressed_bytes: 256 * 1024 * 1024,
            uncompressed_bytes: 512 * 1024 * 1024,
            entry_bytes: store::MAX_BLOB,
            entries: 32768,
            manifest_bytes: store::MAX_MANIFEST,
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BundlePreview {
    pub scope: String,
    pub preset_count: usize,
    pub version_count: usize,
    pub included_assets: usize,
    pub models: Vec<DeviceModel>,
    pub missing_assets: Vec<AssetRef>,
    pub notes: Vec<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BundleManifest {
    format: String,
    schema_version: u32,
    scope: String,
    entries: BTreeMap<LibraryId, StoredEntry>,
    versions: BTreeMap<LibraryVersionId, StoredVersion>,
    assets: BTreeMap<AssetId, StoredAsset>,
    /// ZIP entry contents are addressed by SHA-256, with uncompressed lengths.
    blobs: BTreeMap<String, u64>,
    missing_assets: Vec<AssetRef>,
}
struct CheckedBundle {
    manifest: BundleManifest,
    blobs: BTreeMap<String, Vec<u8>>,
    preview: BundlePreview,
}

impl LibraryStore {
    pub fn preview_bundle(
        path: impl AsRef<Path>,
        limits: BundleLimits,
    ) -> Result<BundlePreview, ServiceError> {
        Ok(read_bundle(path.as_ref(), limits, &|| false)?.preview)
    }
    pub fn export_bundle(&self, request: &ExportRequest) -> Result<ConversionReport, ServiceError> {
        self.export_bundle_cancellable(request, false, &|| false)
    }
    pub fn backup(&self, request: &BackupRequest) -> Result<ConversionReport, ServiceError> {
        self.export_bundle_cancellable(
            &ExportRequest {
                path: request.path.clone(),
                versions: request.versions.clone(),
                include_assets: true,
            },
            true,
            &|| false,
        )
    }
    pub(crate) fn export_bundle_cancellable(
        &self,
        request: &ExportRequest,
        backup: bool,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<ConversionReport, ServiceError> {
        check_cancel(cancelled)?;
        if request
            .path
            .extension()
            .is_none_or(|extension| extension != "gp200bundle")
        {
            return Err(invalid(
                "Portable bundles must use .gp200bundle; native .prst uses a separate format adapter",
            ));
        }
        let selected: BTreeSet<LibraryId> = if request.versions.is_empty() {
            self.manifest
                .entries
                .values()
                .filter(|entry| backup || !entry.trashed)
                .map(|entry| entry.id.clone())
                .collect()
        } else {
            request
                .versions
                .iter()
                .map(|id| {
                    self.manifest
                        .versions
                        .get(id)
                        .map(|version| version.library.clone())
                        .ok_or_else(|| invalid(format!("Unknown export revision {id}")))
                })
                .collect::<Result<_, _>>()?
        };
        let mut entries = BTreeMap::new();
        let mut versions = BTreeMap::new();
        let mut blobs = BTreeMap::new();
        let mut available = BTreeMap::new();
        for id in selected {
            let entry = self
                .manifest
                .entries
                .get(&id)
                .ok_or_else(|| invalid("Export entry missing"))?;
            entries.insert(id, entry.clone());
            // Every revision of a selected preset travels, not only its head.
            for version_id in &entry.versions {
                check_cancel(cancelled)?;
                let stored = self
                    .manifest
                    .versions
                    .get(version_id)
                    .ok_or_else(|| invalid("Export revision missing"))?;
                let version = self.load_stored(stored)?;
                for hash in [&stored.snapshot_hash, &stored.raw_hash] {
                    blobs.insert(hash.clone(), self.read_blob(hash)?.len() as u64);
                }
                for original in &stored.originals {
                    blobs.insert(original.hash.clone(), original.bytes);
                }
                if request.include_assets {
                    for reference in &version.snapshot.asset_refs {
                        let content = match reference {
                            AssetRef::Content { id, .. }
                            | AssetRef::UserSlot {
                                content: Some(id), ..
                            } => Some(id),
                            _ => None,
                        };
                        if let Some(id) = content
                            && let Some(asset) = self.manifest.assets.get(id)
                        {
                            self.read_blob(&asset.hash)?;
                            available.insert(id.clone(), asset.clone());
                            blobs.insert(asset.hash.clone(), asset.bytes);
                        }
                    }
                }
                versions.insert(version_id.clone(), stored.clone());
            }
        }
        let mut missing_assets = Vec::new();
        for version in versions.values() {
            for reference in
                dependency_report(&self.load_stored(version)?.snapshot, &available).missing
            {
                if !missing_assets.contains(&reference) {
                    missing_assets.push(reference);
                }
            }
        }
        let scope = if available.is_empty() {
            "preset-only"
        } else if missing_assets.is_empty() {
            "presets-with-assets"
        } else {
            "presets-with-available-assets"
        };
        let manifest = BundleManifest {
            format: "gp200-studio.bundle".into(),
            schema_version: 1,
            scope: scope.into(),
            entries,
            versions,
            assets: available,
            blobs,
            missing_assets: missing_assets.clone(),
        };
        let manifest_bytes = serde_json::to_vec(&manifest).map_err(invalid)?;
        let limits = BundleLimits::default();
        if manifest_bytes.len() as u64 > limits.manifest_bytes
            || manifest.blobs.len() + 1 > limits.entries
            || manifest.blobs.values().sum::<u64>() + manifest_bytes.len() as u64
                > limits.uncompressed_bytes
        {
            return Err(invalid("Export exceeds portable bundle limits"));
        }
        let parent = request
            .path
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent).map_err(io)?;
        store::reject_symlink(&request.path)?;
        let temporary = parent.join(format!(".bundle-{}.tmp", store::nonce()?));
        let result = (|| {
            let file = OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&temporary)
                .map_err(io)?;
            let mut writer = ZipWriter::new(file);
            let options = SimpleFileOptions::default()
                .compression_method(CompressionMethod::Deflated)
                .unix_permissions(0o600);
            writer
                .start_file("manifest.json", options)
                .map_err(invalid)?;
            writer.write_all(&manifest_bytes).map_err(io)?;
            for hash in manifest.blobs.keys() {
                check_cancel(cancelled)?;
                writer
                    .start_file(format!("blobs/{hash}"), options)
                    .map_err(invalid)?;
                writer.write_all(&self.read_blob(hash)?).map_err(io)?;
            }
            let file = writer.finish().map_err(invalid)?;
            file.sync_all().map_err(io)?;
            if file.metadata().map_err(io)?.len() > limits.compressed_bytes {
                return Err(invalid("Compressed export exceeds bundle limit"));
            }
            check_cancel(cancelled)?;
            fs::rename(&temporary, &request.path).map_err(io)?;
            File::open(parent)
                .and_then(|file| file.sync_all())
                .map_err(io)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result?;
        Ok(bundle_report(
            scope,
            missing_assets,
            manifest.entries.len(),
            manifest.versions.len(),
            backup,
        ))
    }
    pub fn import_bundle(
        &mut self,
        request: &ImportRequest,
    ) -> Result<(Vec<LibraryVersionId>, ConversionReport), ServiceError> {
        self.import_bundle_cancellable(request, &|| false)
    }
    pub(crate) fn import_bundle_cancellable(
        &mut self,
        request: &ImportRequest,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<(Vec<LibraryVersionId>, ConversionReport), ServiceError> {
        let checked = read_bundle(&request.path, BundleLimits::default(), cancelled)?;
        let mut next = self.manifest.clone();
        let mut imported = Vec::new();
        let mut copies = 0;
        for entry in checked.manifest.entries.values() {
            check_cancel(cancelled)?;
            if let Some(existing) = next.entries.get(&entry.id)
                && existing == entry
                && entry
                    .versions
                    .iter()
                    .all(|id| next.versions.get(id) == checked.manifest.versions.get(id))
            {
                imported.extend(entry.versions.iter().cloned());
                continue;
            }
            let collides = next.entries.contains_key(&entry.id)
                || entry
                    .versions
                    .iter()
                    .any(|id| next.versions.contains_key(id));
            let new_library = if collides {
                copies += 1;
                LibraryId(format!("preset-{}", store::nonce()?))
            } else {
                entry.id.clone()
            };
            let mut mapping = BTreeMap::new();
            for id in &entry.versions {
                mapping.insert(
                    id.clone(),
                    if collides {
                        LibraryVersionId(format!("revision-{}", store::nonce()?))
                    } else {
                        id.clone()
                    },
                );
            }
            let mut new_entry = entry.clone();
            new_entry.id = new_library.clone();
            new_entry.latest = mapping
                .get(&entry.latest)
                .ok_or_else(|| invalid("Imported latest revision missing"))?
                .clone();
            new_entry.versions = entry
                .versions
                .iter()
                .map(|id| {
                    mapping
                        .get(id)
                        .cloned()
                        .ok_or_else(|| invalid("Imported revision mapping missing"))
                })
                .collect::<Result<_, _>>()?;
            for id in &entry.versions {
                let mut version = checked
                    .manifest
                    .versions
                    .get(id)
                    .ok_or_else(|| invalid("Imported revision missing"))?
                    .clone();
                version.id = mapping
                    .get(id)
                    .ok_or_else(|| invalid("Imported revision mapping missing"))?
                    .clone();
                version.library = new_library.clone();
                version.parent = version
                    .parent
                    .as_ref()
                    .map(|id| {
                        mapping
                            .get(id)
                            .cloned()
                            .ok_or_else(|| invalid("Imported parent missing"))
                    })
                    .transpose()?;
                imported.push(version.id.clone());
                next.versions.insert(version.id.clone(), version);
            }
            next.entries.insert(new_library, new_entry);
        }
        if next.versions.len() > store::MAX_VERSIONS {
            return Err(invalid("Import would exceed library revision limit"));
        }
        for (id, asset) in &checked.manifest.assets {
            if let Some(existing) = next.assets.get(id)
                && existing.kind != asset.kind
            {
                return Err(ServiceError::new(
                    ErrorKind::Conflict,
                    "Bundle asset format conflicts with an existing content hash",
                ));
            }
            next.assets
                .entry(id.clone())
                .or_insert_with(|| asset.clone());
        }
        validate_graph(&next)?;
        // No manifest changes occur until the entire archive, all hashes, snapshots
        // and references are checked. Abandoned blobs never become visible items.
        for (hash, bytes) in &checked.blobs {
            check_cancel(cancelled)?;
            if self.publish_blob(bytes)? != *hash {
                return Err(invalid("Publication hash mismatch"));
            }
        }
        check_cancel(cancelled)?;
        self.commit(next)?;
        let mut report = bundle_report(
            &checked.manifest.scope,
            checked.preview.missing_assets,
            checked.preview.preset_count,
            checked.preview.version_count,
            false,
        );
        if copies > 0 {
            report.notes.push(format!("Kept {copies} conflicting imported preset(s) as separate local copies; existing tags and revisions were preserved."));
        }
        Ok((imported, report))
    }
}

fn bundle_report(
    scope: &str,
    missing: Vec<AssetRef>,
    presets: usize,
    versions: usize,
    backup: bool,
) -> ConversionReport {
    ConversionReport {
        level: if missing.is_empty() {
            CompatibilityLevel::Lossless
        } else {
            CompatibilityLevel::Partial
        },
        notes: vec![
            format!(
                "GP-200 Studio portable ZIP: {presets} presets, {versions} complete local revisions; scope {scope}."
            ),
            if backup {
                "This is a local library backup. Device globals, firmware and unreadable pedal assets are not included.".into()
            } else {
                "This is the app's .gp200bundle format. Native vendor .prst interoperability is separate.".into()
            },
        ],
        unresolved_assets: missing,
    }
}
fn check_cancel(cancelled: &dyn Fn() -> bool) -> Result<(), ServiceError> {
    if cancelled() {
        Err(ServiceError::new(
            ErrorKind::Cancelled,
            "Library operation cancelled; no partial manifest was published",
        ))
    } else {
        Ok(())
    }
}
pub(crate) fn normalized_entry(name: &str) -> Result<(), ServiceError> {
    if name.is_empty()
        || name.len() > 1024
        || name.contains('\\')
        || name.contains(':')
        || name.contains('\0')
        || name.starts_with('/')
        || name.ends_with('/')
    {
        return Err(invalid(format!("Unsafe bundle entry path: {name:?}")));
    }
    if !Path::new(name)
        .components()
        .all(|component| matches!(component, Component::Normal(_)))
        || name
            .split('/')
            .any(|component| component.is_empty() || component == "." || component == "..")
    {
        return Err(invalid(format!("Unsafe bundle entry path: {name:?}")));
    }
    Ok(())
}
fn read_bundle(
    path: &Path,
    limits: BundleLimits,
    cancelled: &dyn Fn() -> bool,
) -> Result<CheckedBundle, ServiceError> {
    check_cancel(cancelled)?;
    store::reject_symlink(path)?;
    let mut file = File::open(path).map_err(io)?;
    let metadata = file.metadata().map_err(io)?;
    if !metadata.is_file() || metadata.len() > limits.compressed_bytes {
        return Err(invalid(
            "Bundle exceeds compressed size limit or is not a regular file",
        ));
    }
    audit_directory(&mut file, metadata.len(), limits)?;
    file.seek(SeekFrom::Start(0)).map_err(io)?;
    let mut archive = ZipArchive::new(file).map_err(invalid)?;
    if archive.len() > limits.entries {
        return Err(invalid("Bundle entry count exceeds bounds"));
    }
    let mut names = BTreeSet::new();
    let mut total = 0u64;
    // Reject duplicates and traversal before decompressing any entry.
    for index in 0..archive.len() {
        let entry = archive.by_index(index).map_err(invalid)?;
        normalized_entry(entry.name())?;
        if !names.insert(entry.name().to_owned()) {
            return Err(invalid(format!("Duplicate ZIP entry {}", entry.name())));
        }
        if entry.is_dir()
            || entry
                .unix_mode()
                .is_some_and(|mode| mode & 0o170000 == 0o120000)
        {
            return Err(invalid(
                "Bundle directories and symbolic links are not accepted",
            ));
        }
        if entry.size() > limits.entry_bytes {
            return Err(invalid("Bundle entry exceeds uncompressed size limit"));
        }
        total = total
            .checked_add(entry.size())
            .ok_or_else(|| invalid("Bundle size overflow"))?;
        if total > limits.uncompressed_bytes {
            return Err(invalid("Bundle exceeds total decompression limit"));
        }
    }
    let manifest_bytes = read_zip_entry(
        &mut archive,
        "manifest.json",
        limits.manifest_bytes,
        cancelled,
    )?;
    let manifest: BundleManifest = serde_json::from_slice(&manifest_bytes).map_err(|error| {
        ServiceError::unsupported(format!("Invalid or unsupported portable manifest: {error}"))
    })?;
    if manifest.format != "gp200-studio.bundle" || manifest.schema_version != 1 {
        return Err(ServiceError::unsupported(format!(
            "Unsupported bundle format {} schema {}",
            manifest.format, manifest.schema_version
        )));
    }
    if !matches!(
        manifest.scope.as_str(),
        "preset-only" | "presets-with-assets" | "presets-with-available-assets"
    ) {
        return Err(invalid("Unknown bundle completeness scope"));
    }
    let graph = Manifest {
        entries: manifest.entries.clone(),
        versions: manifest.versions.clone(),
        assets: manifest.assets.clone(),
        ..Manifest::default()
    };
    validate_graph(&graph)?;
    let mut expected = BTreeSet::from(["manifest.json".to_owned()]);
    let mut blobs = BTreeMap::new();
    for (hash, length) in &manifest.blobs {
        check_cancel(cancelled)?;
        if !valid_hash(hash) || *length > limits.entry_bytes {
            return Err(invalid("Invalid bundle blob descriptor"));
        }
        let name = format!("blobs/{hash}");
        expected.insert(name.clone());
        let bytes = read_zip_entry(&mut archive, &name, *length, cancelled)?;
        if bytes.len() as u64 != *length || digest(&bytes) != *hash {
            return Err(invalid(format!(
                "Bundle content hash or length mismatch: {name}"
            )));
        }
        blobs.insert(hash.clone(), bytes);
    }
    if names != expected {
        let unknown: Vec<_> = names.difference(&expected).cloned().collect();
        let absent: Vec<_> = expected.difference(&names).cloned().collect();
        return Err(ServiceError::unsupported(format!(
            "Unsupported extra bundle entries {unknown:?}; missing entries {absent:?}. No files were imported."
        )));
    }
    let mut used = BTreeSet::new();
    let mut models = BTreeSet::new();
    let mut missing = Vec::new();
    for version in manifest.versions.values() {
        let snapshot_bytes = blobs
            .get(&version.snapshot_hash)
            .ok_or_else(|| invalid("Bundle snapshot blob missing"))?;
        let snapshot = store::decode_snapshot(snapshot_bytes)?;
        let raw_bytes = blobs
            .get(&version.raw_hash)
            .ok_or_else(|| invalid("Bundle raw blob missing"))?;
        if raw_bytes.first() != Some(&snapshot.raw.kind())
            || raw_bytes.get(1..) != Some(snapshot.raw.payload())
        {
            return Err(invalid(
                "Bundle snapshot disagrees with authoritative raw payload",
            ));
        }
        used.insert(version.snapshot_hash.clone());
        used.insert(version.raw_hash.clone());
        models.insert(snapshot.device_model);
        for original in &version.originals {
            if blobs
                .get(&original.hash)
                .is_none_or(|bytes| bytes.len() as u64 != original.bytes)
            {
                return Err(invalid("Bundle original file missing or truncated"));
            }
            used.insert(original.hash.clone());
        }
        for reference in dependency_report(&snapshot, &manifest.assets).missing {
            if !missing.contains(&reference) {
                missing.push(reference);
            }
        }
    }
    for asset in manifest.assets.values() {
        if blobs
            .get(&asset.hash)
            .is_none_or(|bytes| bytes.len() as u64 != asset.bytes)
        {
            return Err(invalid("Bundle asset bytes missing or truncated"));
        }
        used.insert(asset.hash.clone());
    }
    if used.len() != blobs.len() {
        return Err(ServiceError::unsupported(
            "Bundle includes unreferenced blob extensions; no files were imported",
        ));
    }
    // The declared completeness is checked rather than trusted.
    if missing != manifest.missing_assets {
        return Err(invalid(
            "Bundle dependency report disagrees with actual included assets",
        ));
    }
    if manifest.scope == "preset-only" && !manifest.assets.is_empty()
        || manifest.scope == "presets-with-assets"
            && (manifest.assets.is_empty() || !missing.is_empty())
        || manifest.scope == "presets-with-available-assets"
            && (manifest.assets.is_empty() || missing.is_empty())
    {
        return Err(invalid(
            "Bundle scope overstates or mislabels included asset contents",
        ));
    }
    let preview = BundlePreview {
        scope: manifest.scope.clone(),
        preset_count: manifest.entries.len(),
        version_count: manifest.versions.len(),
        included_assets: manifest.assets.len(),
        models: models.into_iter().collect(),
        missing_assets: missing,
        notes: vec![
            "All hashes and lengths checked. Import affects the local library only.".into(),
        ],
    };
    Ok(CheckedBundle {
        manifest,
        blobs,
        preview,
    })
}

/// The zip crate intentionally deduplicates central names in an IndexMap.
/// Audit the original directory first so duplicates cannot disappear before
/// validation. Our bounded format never needs ZIP64 or split/prefixed archives.
pub(crate) fn audit_directory(
    file: &mut File,
    length: u64,
    limits: BundleLimits,
) -> Result<(), ServiceError> {
    let tail_length = length.min(65557) as usize;
    file.seek(SeekFrom::End(-(tail_length as i64)))
        .map_err(io)?;
    let mut tail = vec![0; tail_length];
    file.read_exact(&mut tail).map_err(io)?;
    let position = (0..tail.len().saturating_sub(21))
        .rev()
        .find(|&index| {
            tail[index..].starts_with(b"PK\x05\x06")
                && index
                    + 22
                    + usize::from(u16::from_le_bytes([tail[index + 20], tail[index + 21]]))
                    == tail.len()
        })
        .ok_or_else(|| invalid("ZIP end directory missing or has trailing data"))?;
    let end = &tail[position..position + 22];
    let u16_at = |offset| u16::from_le_bytes([end[offset], end[offset + 1]]);
    let u32_at =
        |offset| u32::from_le_bytes(end[offset..offset + 4].try_into().expect("directory field"));
    let count = usize::from(u16_at(10));
    let size = u64::from(u32_at(12));
    let start = u64::from(u32_at(16));
    if u16_at(4) != 0
        || u16_at(6) != 0
        || u16_at(8) != u16_at(10)
        || count == 65535
        || count > limits.entries
        || size > 4 * 1024 * 1024
        || start.checked_add(size) != Some(length - tail_length as u64 + position as u64)
    {
        return Err(invalid(
            "Split, prefixed, ZIP64, oversized or inconsistent ZIP directory is unsupported",
        ));
    }
    file.seek(SeekFrom::Start(start)).map_err(io)?;
    let mut names = BTreeSet::new();
    let mut consumed = 0u64;
    let mut total = 0u64;
    for _ in 0..count {
        let mut header = [0u8; 46];
        file.read_exact(&mut header).map_err(invalid)?;
        consumed += 46;
        if !header.starts_with(b"PK\x01\x02") {
            return Err(invalid("Invalid ZIP central header"));
        }
        let word = |offset| u16::from_le_bytes([header[offset], header[offset + 1]]);
        let long =
            |offset| u32::from_le_bytes(header[offset..offset + 4].try_into().expect("ZIP field"));
        let flags = word(8);
        let method = word(10);
        let uncompressed = u64::from(long(24));
        let name_len = usize::from(word(28));
        let extra_len = usize::from(word(30));
        let comment_len = usize::from(word(32));
        if flags & 1 != 0
            || !matches!(method, 0 | 8)
            || name_len == 0
            || name_len > 1024
            || word(34) != 0
            || uncompressed > limits.entry_bytes
            || long(20) == u32::MAX
            || long(24) == u32::MAX
            || long(42) == u32::MAX
            || u64::from(long(42)) >= start
        {
            return Err(invalid("Unsupported or unbounded ZIP entry"));
        }
        total = total
            .checked_add(uncompressed)
            .ok_or_else(|| invalid("ZIP uncompressed size overflow"))?;
        if total > limits.uncompressed_bytes {
            return Err(invalid("ZIP total decompression limit exceeded"));
        }
        let addition = (name_len + extra_len + comment_len) as u64;
        if consumed + addition > size {
            return Err(invalid("ZIP directory length mismatch"));
        }
        let mut name = vec![0; name_len];
        file.read_exact(&mut name).map_err(invalid)?;
        // Portable bundle entries are ASCII content hashes. Unicode aliases
        // would add another naming authority and are outside this format.
        if !name.is_ascii() {
            return Err(ServiceError::unsupported(
                "Non-ASCII ZIP entry names are not part of the portable bundle format",
            ));
        }
        let name = String::from_utf8(name).map_err(invalid)?;
        normalized_entry(&name)?;
        if !names.insert(name.clone()) {
            return Err(invalid(format!("Duplicate ZIP entry {name}")));
        }
        let mut extra = vec![0; extra_len];
        file.read_exact(&mut extra).map_err(invalid)?;
        let mut offset = 0;
        while offset < extra.len() {
            if extra.len() - offset < 4 {
                return Err(invalid("Truncated ZIP extra field"));
            }
            let tag = u16::from_le_bytes([extra[offset], extra[offset + 1]]);
            let field_len = usize::from(u16::from_le_bytes([extra[offset + 2], extra[offset + 3]]));
            offset += 4;
            if offset + field_len > extra.len() || matches!(tag, 0x0001 | 0x7075 | 0x9901) {
                return Err(ServiceError::unsupported(
                    "ZIP64, Unicode path aliases, encrypted or malformed ZIP extensions are unsupported",
                ));
            }
            offset += field_len;
        }
        file.seek(SeekFrom::Current(comment_len as i64))
            .map_err(io)?;
        consumed += addition;
    }
    if consumed != size {
        return Err(invalid("ZIP directory contains uncounted data"));
    }
    Ok(())
}
fn read_zip_entry(
    archive: &mut ZipArchive<File>,
    name: &str,
    maximum: u64,
    cancelled: &dyn Fn() -> bool,
) -> Result<Vec<u8>, ServiceError> {
    check_cancel(cancelled)?;
    let mut entry = archive.by_name(name).map_err(invalid)?;
    if entry.size() > maximum {
        return Err(invalid(format!("ZIP entry {name} exceeds limit")));
    }
    let mut bytes = Vec::new();
    let mut buffer = [0u8; 32768];
    loop {
        check_cancel(cancelled)?;
        let count = entry.read(&mut buffer).map_err(invalid)?;
        if count == 0 {
            break;
        }
        if bytes.len() as u64 + count as u64 > maximum {
            return Err(invalid("ZIP decompressed data exceeds declared bounds"));
        }
        bytes.extend_from_slice(&buffer[..count]);
    }
    Ok(bytes)
}
