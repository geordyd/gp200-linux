use fs2::FileExt;
use gp200_catalog::Catalog;
use gp200_domain::*;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

pub const SCHEMA_VERSION: u32 = 1;
pub(crate) const MAX_MANIFEST: u64 = 16 * 1024 * 1024;
pub(crate) const MAX_BLOB: u64 = 64 * 1024 * 1024;
pub(crate) const MAX_VERSIONS: usize = 16384;
pub(crate) const MAX_SNAPSHOT: usize = 2 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OriginalSource {
    pub name: String,
    pub format: String,
    pub hash: String,
    pub bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredAsset {
    pub id: AssetId,
    pub kind: AssetKind,
    pub name: String,
    pub hash: String,
    pub bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct StoredVersion {
    pub id: LibraryVersionId,
    pub library: LibraryId,
    pub parent: Option<LibraryVersionId>,
    pub snapshot_hash: String,
    pub raw_hash: String,
    pub metadata: LibraryMetadata,
    pub created_at: String,
    #[serde(default)]
    pub originals: Vec<OriginalSource>,
    #[serde(default)]
    pub model_names: Vec<String>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct StoredEntry {
    pub id: LibraryId,
    pub latest: LibraryVersionId,
    pub versions: Vec<LibraryVersionId>,
    pub trashed: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Manifest {
    pub schema_version: u32,
    pub generation: u64,
    pub entries: BTreeMap<LibraryId, StoredEntry>,
    pub versions: BTreeMap<LibraryVersionId, StoredVersion>,
    #[serde(default)]
    pub assets: BTreeMap<AssetId, StoredAsset>,
    /// Room for future versioned local features; never silently dropped.
    #[serde(default)]
    pub extensions: BTreeMap<String, serde_json::Value>,
}
impl Default for Manifest {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            generation: 0,
            entries: BTreeMap::new(),
            versions: BTreeMap::new(),
            assets: BTreeMap::new(),
            extensions: BTreeMap::new(),
        }
    }
}
#[derive(Serialize, Deserialize)]
struct ManifestEnvelope {
    format: String,
    sha256: String,
    manifest: Manifest,
}

/// Single-writer immutable store. Keep the handle alive for the lifetime of the worker.
pub struct LibraryStore {
    pub(crate) root: PathBuf,
    pub(crate) manifest: Manifest,
    /// A failed publication may have reached disk. Never publish a stale candidate.
    pub(crate) movement_publication_uncertain: bool,
    pub(crate) firmware_publication_uncertain: bool,
    _lock: File,
    pub recovery_note: Option<String>,
    catalog: Catalog,
}

pub fn data_dir() -> Result<PathBuf, ServiceError> {
    if let Some(path) = std::env::var_os("GP200_STUDIO_DATA") {
        return absolute_data_root(PathBuf::from(path));
    }
    let root = if let Some(path) =
        std::env::var_os("XDG_DATA_HOME").filter(|path| Path::new(path).is_absolute())
    {
        PathBuf::from(path)
    } else {
        PathBuf::from(std::env::var_os("HOME").ok_or_else(|| {
            ServiceError::new(
                ErrorKind::Io,
                "HOME and XDG_DATA_HOME are unavailable; set GP200_STUDIO_DATA",
            )
        })?)
        .join(".local/share")
    };
    absolute_data_root(root.join("gp200-studio"))
}
fn absolute_data_root(path: PathBuf) -> Result<PathBuf, ServiceError> {
    if path.is_absolute() {
        Ok(path)
    } else {
        Err(ServiceError::invalid(
            "Library data directory must be absolute",
        ))
    }
}
pub(crate) fn io(error: impl std::fmt::Display) -> ServiceError {
    ServiceError::new(ErrorKind::Io, error.to_string())
}
pub(crate) fn invalid(error: impl std::fmt::Display) -> ServiceError {
    ServiceError::invalid(error.to_string())
}
pub(crate) fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
pub(crate) fn valid_hash(hash: &str) -> bool {
    hash.len() == 64
        && hash
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

impl LibraryStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, ServiceError> {
        let path = path.as_ref();
        fs::create_dir_all(path).map_err(io)?;
        let root = fs::canonicalize(path).map_err(io)?;
        let lock_path = root.join("writer.lock");
        reject_symlink(&lock_path)?;
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(lock_path)
            .map_err(io)?;
        lock.try_lock_exclusive().map_err(|error| {
            ServiceError::new(
                ErrorKind::Busy,
                format!("Library is already open by another writer: {error}"),
            )
        })?;
        let blob_dir = root.join("blobs");
        reject_symlink(&blob_dir)?;
        fs::create_dir_all(&blob_dir).map_err(io)?;
        let catalog = Catalog::embedded().map_err(invalid)?;
        let mut store = Self {
            root,
            manifest: Manifest::default(),
            movement_publication_uncertain: false,
            firmware_publication_uncertain: false,
            _lock: lock,
            recovery_note: None,
            catalog,
        };
        let current = store.root.join("manifest.json");
        let previous = store.root.join("manifest.previous.json");
        if current.exists() || previous.exists() {
            match store.read_manifest(&current) {
                Ok(manifest) => store.manifest = manifest,
                Err(current_error) => {
                    // A future schema is never silently rolled back to a stale manifest.
                    if current_error.kind == ErrorKind::Unsupported {
                        return Err(current_error);
                    }
                    let recovered = store.read_manifest(&previous).map_err(|previous_error| invalid(format!("Library manifest is invalid ({current_error}); recovery manifest is also invalid ({previous_error})")))?;
                    store.manifest = recovered;
                    store.recovery_note = Some(format!(
                        "Recovered previous committed manifest: {current_error}"
                    ));
                    let bytes = envelope_bytes(&store.manifest)?;
                    atomic_write(&store.root, &current, &bytes)?;
                }
            }
        } else {
            atomic_write(&store.root, &current, &envelope_bytes(&store.manifest)?)?;
        }
        store.recover_interrupted_restores()?;
        store.recover_open_movements()?;
        Ok(store)
    }
    pub fn open_xdg() -> Result<Self, ServiceError> {
        Self::open(data_dir()?)
    }
    pub fn root(&self) -> &Path {
        &self.root
    }
    pub fn generation(&self) -> u64 {
        self.manifest.generation
    }

    fn read_manifest(&self, path: &Path) -> Result<Manifest, ServiceError> {
        let bytes = read_bounded(path, MAX_MANIFEST)?;
        let envelope: ManifestEnvelope = serde_json::from_slice(&bytes).map_err(invalid)?;
        if envelope.format != "gp200-studio.library" {
            return Err(invalid("Not a GP-200 Studio library manifest"));
        }
        if envelope.manifest.schema_version != SCHEMA_VERSION {
            return Err(ServiceError::unsupported(format!(
                "Unsupported library schema {}; this build supports {}. No migration was attempted.",
                envelope.manifest.schema_version, SCHEMA_VERSION
            )));
        }
        let serialized = serde_json::to_vec(&envelope.manifest).map_err(invalid)?;
        if digest(&serialized) != envelope.sha256 {
            return Err(invalid("Library manifest hash mismatch"));
        }
        self.validate_manifest(&envelope.manifest)?;
        Ok(envelope.manifest)
    }

    pub(crate) fn validate_manifest(&self, manifest: &Manifest) -> Result<(), ServiceError> {
        validate_graph(manifest)?;
        for version in manifest.versions.values() {
            self.load_stored(version)?;
        }
        for asset in manifest.assets.values() {
            let bytes = self.read_blob(&asset.hash)?;
            if bytes.len() as u64 != asset.bytes {
                return Err(invalid("Asset length mismatch"));
            }
        }
        self.validate_workflow_blobs(manifest)?;
        self.validate_movement_manifest(manifest)?;
        Ok(())
    }
    pub(crate) fn commit(&mut self, mut next: Manifest) -> Result<(), ServiceError> {
        if self.movement_publication_uncertain || self.firmware_publication_uncertain {
            return Err(ServiceError::new(
                ErrorKind::Indeterminate,
                "Library publication was uncertain; reopen before another manifest write",
            ));
        }
        next.generation = self
            .manifest
            .generation
            .checked_add(1)
            .ok_or_else(|| invalid("Library generation exhausted"))?;
        validate_graph(&next)?;
        let movement_bearing = self
            .manifest
            .extensions
            .contains_key(crate::preset_movement_journal::MOVEMENT_EXTENSION)
            || next
                .extensions
                .contains_key(crate::preset_movement_journal::MOVEMENT_EXTENSION);
        if movement_bearing {
            self.validate_movement_manifest(&next)?;
        }
        let bytes = envelope_bytes(&next)?;
        if bytes.len() as u64 > MAX_MANIFEST {
            return Err(invalid("Library manifest exceeds size limit"));
        }
        let current = self.root.join("manifest.json");
        // Publish a valid recovery point before replacing the current manifest.
        let publication = (|| {
            atomic_write(
                &self.root,
                &self.root.join("manifest.previous.json"),
                &envelope_bytes(&self.manifest)?,
            )?;
            atomic_write(&self.root, &current, &bytes)?;
            Ok::<(), ServiceError>(())
        })();
        if let Err(error) = publication {
            if movement_bearing {
                self.movement_publication_uncertain = true;
            }
            return Err(error);
        }
        self.manifest = next;
        Ok(())
    }

    pub fn query(&self, query: &LibraryQuery) -> Vec<LibraryEntry> {
        self.query_with_trash(query, query.trashed_only)
    }
    pub fn query_trash(&self, query: &LibraryQuery) -> Vec<LibraryEntry> {
        self.query_with_trash(query, true)
    }
    fn query_with_trash(&self, query: &LibraryQuery, trash: bool) -> Vec<LibraryEntry> {
        let words: Vec<String> = query
            .text
            .to_lowercase()
            .split_whitespace()
            .map(str::to_owned)
            .collect();
        let tags: Vec<String> = query.tags.iter().map(|tag| tag.to_lowercase()).collect();
        let mut entries: Vec<LibraryEntry> = self
            .manifest
            .entries
            .values()
            .filter(|entry| entry.trashed == trash)
            .filter_map(|entry| {
                let latest = self.manifest.versions.get(&entry.latest)?;
                let metadata = &latest.metadata;
                if query.favorites_only && !metadata.favorite {
                    return None;
                }
                if !tags.iter().all(|tag| {
                    metadata
                        .tags
                        .iter()
                        .any(|stored| stored.to_lowercase() == *tag)
                }) {
                    return None;
                }
                let text = format!(
                    "{} {} {} {} {} {}",
                    metadata.name,
                    metadata.author,
                    metadata.style,
                    metadata.notes,
                    metadata.tags.join(" "),
                    latest.model_names.join(" ")
                )
                .to_lowercase();
                if !words.iter().all(|word| text.contains(word)) {
                    return None;
                }
                Some(LibraryEntry {
                    id: entry.id.clone(),
                    latest_version: entry.latest.clone(),
                    metadata: metadata.clone(),
                    version_count: entry.versions.len() as u32,
                    modified_at: latest.created_at.clone(),
                })
            })
            .collect();
        entries.sort_by(|a, b| {
            a.metadata
                .name
                .to_lowercase()
                .cmp(&b.metadata.name.to_lowercase())
                .then(a.id.cmp(&b.id))
        });
        entries
    }
    pub fn load_version(&self, id: &LibraryVersionId) -> Result<LibraryVersion, ServiceError> {
        self.load_stored(
            self.manifest
                .versions
                .get(id)
                .ok_or_else(|| invalid(format!("Unknown library revision {id}")))?,
        )
    }
    pub(crate) fn load_stored(
        &self,
        version: &StoredVersion,
    ) -> Result<LibraryVersion, ServiceError> {
        let bytes = self.read_blob(&version.snapshot_hash)?;
        let snapshot = decode_snapshot(&bytes)?;
        let raw_bytes = self.read_blob(&version.raw_hash)?;
        if raw_bytes.first() != Some(&snapshot.raw.kind())
            || raw_bytes.get(1..) != Some(snapshot.raw.payload())
        {
            return Err(invalid("Snapshot and authoritative raw blob disagree"));
        }
        for original in &version.originals {
            if self.read_blob(&original.hash)?.len() as u64 != original.bytes {
                return Err(invalid("Original file length mismatch"));
            }
        }
        Ok(LibraryVersion {
            id: version.id.clone(),
            library: version.library.clone(),
            parent: version.parent.clone(),
            snapshot: Arc::new(snapshot),
            metadata: version.metadata.clone(),
            created_at: version.created_at.clone(),
        })
    }
    pub fn list_versions(&self, library: &LibraryId) -> Result<Vec<LibraryVersion>, ServiceError> {
        let entry = self
            .manifest
            .entries
            .get(library)
            .ok_or_else(|| invalid("Unknown library preset"))?;
        entry
            .versions
            .iter()
            .map(|id| self.load_version(id))
            .collect()
    }
    pub fn store_snapshot(
        &mut self,
        snapshot: Arc<PresetSnapshot>,
        metadata: LibraryMetadata,
        parent: Option<LibraryVersionId>,
    ) -> Result<LibraryVersionId, ServiceError> {
        self.store_with_originals(snapshot, metadata, parent, &[])
    }
    /// File adapters can retain original bytes with the same atomic revision transaction.
    pub fn store_with_originals(
        &mut self,
        snapshot: Arc<PresetSnapshot>,
        metadata: LibraryMetadata,
        parent: Option<LibraryVersionId>,
        originals: &[(String, String, Vec<u8>)],
    ) -> Result<LibraryVersionId, ServiceError> {
        validate_metadata(&metadata)?;
        validate_snapshot(&snapshot)?;
        if self.manifest.versions.len() >= MAX_VERSIONS {
            return Err(invalid("Library revision limit reached"));
        }
        let mut next = self.manifest.clone();
        let library = if let Some(parent) = &parent {
            let previous = next
                .versions
                .get(parent)
                .ok_or_else(|| invalid("Parent revision does not exist"))?;
            let entry = next
                .entries
                .get(&previous.library)
                .ok_or_else(|| invalid("Parent entry is missing"))?;
            if entry.trashed {
                return Err(ServiceError::new(
                    ErrorKind::Conflict,
                    "Restore this preset from trash before editing it",
                ));
            }
            if entry.latest != *parent {
                return Err(ServiceError::new(
                    ErrorKind::Conflict,
                    "Preset changed since it was loaded; select its latest revision before saving",
                ));
            }
            previous.library.clone()
        } else {
            LibraryId(format!("preset-{}", nonce()?))
        };
        let id = LibraryVersionId(format!("revision-{}", nonce()?));
        let snapshot_bytes = serde_json::to_vec(snapshot.as_ref()).map_err(invalid)?;
        if snapshot_bytes.len() > MAX_SNAPSHOT {
            return Err(invalid("Preset snapshot exceeds its 2 MiB bound"));
        }
        let mut raw_bytes = Vec::with_capacity(1 + snapshot.raw.payload().len());
        raw_bytes.push(snapshot.raw.kind());
        raw_bytes.extend_from_slice(snapshot.raw.payload());
        let snapshot_hash = self.publish_blob(&snapshot_bytes)?;
        let raw_hash = self.publish_blob(&raw_bytes)?;
        let mut sources = if let Some(parent) = &parent {
            next.versions
                .get(parent)
                .map(|v| v.originals.clone())
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        for (name, format, bytes) in originals {
            if name.len() > 4096 || format.len() > 256 {
                return Err(invalid("Original file descriptor too long"));
            }
            let hash = self.publish_blob(bytes)?;
            let source = OriginalSource {
                name: name.clone(),
                format: format.clone(),
                hash,
                bytes: bytes.len() as u64,
            };
            if !sources.contains(&source) {
                sources.push(source);
            }
        }
        let model_names = snapshot
            .view
            .modules
            .iter()
            .filter_map(|m| {
                self.catalog
                    .model_by_id(m.module.get(), m.model.0)
                    .map(|model| model.name.clone())
            })
            .collect();
        let version = StoredVersion {
            id: id.clone(),
            library: library.clone(),
            parent,
            snapshot_hash,
            raw_hash,
            metadata,
            created_at: timestamp(),
            originals: sources,
            model_names,
        };
        next.versions.insert(id.clone(), version);
        let entry = next
            .entries
            .entry(library.clone())
            .or_insert_with(|| StoredEntry {
                id: library,
                latest: id.clone(),
                versions: Vec::new(),
                trashed: false,
            });
        entry.latest = id.clone();
        entry.versions.push(id.clone());
        self.commit(next)?;
        Ok(id)
    }
    pub fn update_metadata(
        &mut self,
        library: &LibraryId,
        metadata: LibraryMetadata,
    ) -> Result<LibraryVersionId, ServiceError> {
        let latest = self
            .manifest
            .entries
            .get(library)
            .ok_or_else(|| invalid("Unknown library preset"))?
            .latest
            .clone();
        let version = self.load_version(&latest)?;
        self.store_snapshot(version.snapshot, metadata, Some(latest))
    }
    pub fn delete_to_trash(&mut self, library: &LibraryId) -> Result<(), ServiceError> {
        self.set_trashed(library, true)
    }
    pub fn restore_trash(&mut self, library: &LibraryId) -> Result<(), ServiceError> {
        self.set_trashed(library, false)
    }
    fn set_trashed(&mut self, library: &LibraryId, trashed: bool) -> Result<(), ServiceError> {
        let mut next = self.manifest.clone();
        next.entries
            .get_mut(library)
            .ok_or_else(|| invalid("Unknown library preset"))?
            .trashed = trashed;
        self.commit(next)
    }
    pub fn put_asset(
        &mut self,
        kind: AssetKind,
        name: String,
        bytes: &[u8],
    ) -> Result<StoredAsset, ServiceError> {
        if name.len() > 4096 {
            return Err(invalid("Asset name exceeds limit"));
        }
        let hash = self.publish_blob(bytes)?;
        let id = AssetId(format!("sha256:{hash}"));
        let asset = StoredAsset {
            id: id.clone(),
            kind,
            name,
            hash,
            bytes: bytes.len() as u64,
        };
        let mut next = self.manifest.clone();
        if let Some(existing) = next.assets.get(&id)
            && existing.kind != kind
        {
            return Err(ServiceError::new(
                ErrorKind::Conflict,
                "The same content hash was registered with another asset format",
            ));
        }
        next.assets.insert(id, asset.clone());
        self.commit(next)?;
        Ok(asset)
    }
    pub fn assets(&self) -> &BTreeMap<AssetId, StoredAsset> {
        &self.manifest.assets
    }
    pub fn asset_bytes(&self, id: &AssetId) -> Result<Vec<u8>, ServiceError> {
        self.read_blob(
            &self
                .manifest
                .assets
                .get(id)
                .ok_or_else(|| invalid("Asset is unavailable"))?
                .hash,
        )
    }
    pub fn original_sources(
        &self,
        version: &LibraryVersionId,
    ) -> Result<&[OriginalSource], ServiceError> {
        Ok(&self
            .manifest
            .versions
            .get(version)
            .ok_or_else(|| invalid("Unknown library revision"))?
            .originals)
    }
    pub fn original_bytes(&self, source: &OriginalSource) -> Result<Vec<u8>, ServiceError> {
        self.read_blob(&source.hash)
    }
    pub(crate) fn read_blob(&self, hash: &str) -> Result<Vec<u8>, ServiceError> {
        if !valid_hash(hash) {
            return Err(invalid("Invalid content hash"));
        }
        let bytes = read_bounded(&self.root.join("blobs").join(hash), MAX_BLOB)?;
        if digest(&bytes) != hash {
            return Err(invalid(format!("Content hash mismatch for {hash}")));
        }
        Ok(bytes)
    }
    pub(crate) fn require_firmware_publication_certain(&self) -> Result<(), ServiceError> {
        if self.firmware_publication_uncertain {
            return Err(ServiceError::new(
                ErrorKind::Indeterminate,
                "Firmware publication or evidence was uncertain; reopen before another write",
            ));
        }
        Ok(())
    }
    pub(crate) fn publish_blob(&self, bytes: &[u8]) -> Result<String, ServiceError> {
        self.require_firmware_publication_certain()?;
        if bytes.len() as u64 > MAX_BLOB {
            return Err(invalid("Content exceeds the 64 MiB per-blob limit"));
        }
        let hash = digest(bytes);
        let dir = self.root.join("blobs");
        let path = dir.join(&hash);
        if path.exists() {
            if self.read_blob(&hash)? != bytes {
                return Err(invalid("Existing immutable content disagrees"));
            }
        } else {
            atomic_write(&dir, &path, bytes)?;
        }
        Ok(hash)
    }
}

pub(crate) fn decode_snapshot(bytes: &[u8]) -> Result<PresetSnapshot, ServiceError> {
    if bytes.len() > MAX_SNAPSHOT {
        return Err(invalid("Preset snapshot exceeds its 2 MiB bound"));
    }
    let snapshot: PresetSnapshot = serde_json::from_slice(bytes).map_err(invalid)?;
    validate_snapshot(&snapshot)?;
    Ok(snapshot)
}
fn validate_snapshot(snapshot: &PresetSnapshot) -> Result<(), ServiceError> {
    if snapshot.raw.payload().len() > 65536
        || snapshot.raw.records().len() > 1024
        || snapshot.view.modules.len() > 64
        || snapshot
            .view
            .modules
            .iter()
            .any(|module| module.parameters.len() > 256)
        || snapshot.asset_refs.len() > 256
        || snapshot.evidence.len() > 256
    {
        return Err(invalid(
            "Preset projection or raw record stream exceeds archival bounds",
        ));
    }
    Ok(())
}

pub(crate) fn validate_graph(manifest: &Manifest) -> Result<(), ServiceError> {
    if manifest.schema_version != SCHEMA_VERSION {
        return Err(ServiceError::unsupported("Unsupported library schema"));
    }
    crate::workflows::decode_workflows(manifest)?;
    crate::preset_movement_journal::decode_movement_extension(manifest)?;
    if manifest.versions.len() > MAX_VERSIONS || manifest.entries.len() > 4096 {
        return Err(invalid("Library item count exceeds bounds"));
    }
    let mut referenced = BTreeSet::new();
    for (id, entry) in &manifest.entries {
        if id != &entry.id
            || !valid_id(&id.0)
            || entry.versions.is_empty()
            || !entry.versions.contains(&entry.latest)
        {
            return Err(invalid("Invalid preset identity or revision list"));
        }
        let mut preceding = BTreeSet::new();
        for version_id in &entry.versions {
            if !referenced.insert(version_id.clone()) {
                return Err(invalid("Duplicate library revision reference"));
            }
            let version = manifest
                .versions
                .get(version_id)
                .ok_or_else(|| invalid("Missing library revision"))?;
            if &version.library != id || &version.id != version_id || !valid_id(&version_id.0) {
                return Err(invalid("Library revision identity mismatch"));
            }
            validate_metadata(&version.metadata)?;
            for hash in [&version.snapshot_hash, &version.raw_hash] {
                if !valid_hash(hash) {
                    return Err(invalid("Invalid revision blob hash"));
                }
            }
            for source in &version.originals {
                if !valid_hash(&source.hash)
                    || source.bytes > MAX_BLOB
                    || source.name.len() > 4096
                    || source.format.len() > 256
                {
                    return Err(invalid("Invalid original file descriptor"));
                }
            }
            if let Some(parent_id) = &version.parent {
                if !preceding.contains(parent_id) {
                    return Err(invalid(
                        "Parent revision must precede its child; cyclic or unordered lineage rejected",
                    ));
                }
                let ancestor = manifest
                    .versions
                    .get(parent_id)
                    .ok_or_else(|| invalid("Missing parent revision"))?;
                if ancestor.library != *id {
                    return Err(invalid("Parent revision belongs to another preset"));
                }
            }
            preceding.insert(version_id.clone());
        }
    }
    if referenced.len() != manifest.versions.len() {
        return Err(invalid("Unowned library revision"));
    }
    for (id, asset) in &manifest.assets {
        if id != &asset.id
            || id.0 != format!("sha256:{}", asset.hash)
            || !valid_hash(&asset.hash)
            || asset.bytes > MAX_BLOB
            || asset.name.len() > 4096
        {
            return Err(invalid("Invalid content-addressed asset"));
        }
    }
    Ok(())
}
fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}
pub(crate) fn validate_metadata(metadata: &LibraryMetadata) -> Result<(), ServiceError> {
    if metadata.name.len() > 4096
        || metadata.author.len() > 4096
        || metadata.style.len() > 4096
        || metadata.notes.len() > 65536
        || metadata.tags.len() > 128
        || metadata.tags.iter().any(|tag| tag.len() > 1024)
    {
        return Err(invalid("Library metadata exceeds bounds"));
    }
    Ok(())
}
fn envelope_bytes(manifest: &Manifest) -> Result<Vec<u8>, ServiceError> {
    let sha256 = digest(&serde_json::to_vec(manifest).map_err(invalid)?);
    serde_json::to_vec(&ManifestEnvelope {
        format: "gp200-studio.library".into(),
        sha256,
        manifest: manifest.clone(),
    })
    .map_err(invalid)
}
pub(crate) fn reject_symlink(path: &Path) -> Result<(), ServiceError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(invalid(format!(
            "Symbolic links are not accepted in library storage: {}",
            path.display()
        ))),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(io(error)),
    }
}
pub(crate) fn read_bounded(path: &Path, maximum: u64) -> Result<Vec<u8>, ServiceError> {
    reject_symlink(path)?;
    let file = File::open(path).map_err(io)?;
    let metadata = file.metadata().map_err(io)?;
    if !metadata.is_file() || metadata.len() > maximum {
        return Err(invalid(format!(
            "File exceeds limit or is not regular: {}",
            path.display()
        )));
    }
    let mut bytes = Vec::new();
    file.take(maximum + 1).read_to_end(&mut bytes).map_err(io)?;
    if bytes.len() as u64 > maximum {
        return Err(invalid("File expanded beyond its size limit"));
    }
    Ok(bytes)
}
pub(crate) fn atomic_write(
    directory: &Path,
    path: &Path,
    bytes: &[u8],
) -> Result<(), ServiceError> {
    reject_symlink(path)?;
    let temporary = directory.join(format!(".publish-{}.tmp", nonce()?));
    let result = (|| {
        #[cfg(test)]
        crate::preset_movement_journal::tests::publication_boundary(path, "before-create")?;
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)
            .map_err(io)?;
        file.write_all(bytes).map_err(io)?;
        #[cfg(test)]
        crate::preset_movement_journal::tests::publication_boundary(path, "after-write")?;
        file.sync_all().map_err(io)?;
        #[cfg(test)]
        crate::preset_movement_journal::tests::publication_boundary(path, "after-file-sync")?;
        fs::rename(&temporary, path).map_err(io)?;
        #[cfg(test)]
        crate::preset_movement_journal::tests::publication_boundary(path, "after-rename")?;
        File::open(directory)
            .and_then(|directory| directory.sync_all())
            .map_err(io)?;
        #[cfg(test)]
        crate::preset_movement_journal::tests::publication_boundary(path, "after-directory-sync")?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}
pub(crate) fn nonce() -> Result<String, ServiceError> {
    let mut bytes = [0u8; 16];
    File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut bytes))
        .map_err(io)?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}
fn timestamp() -> String {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    utc_timestamp(duration.as_secs(), duration.subsec_nanos())
}
fn utc_timestamp(seconds: u64, nanoseconds: u32) -> String {
    let mut days = seconds / 86400;
    let mut year = 1970u64;
    let leap = |year: u64| {
        year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400))
    };
    while days >= if leap(year) { 366 } else { 365 } {
        days -= if leap(year) { 366 } else { 365 };
        year += 1;
    }
    let months = [
        31,
        if leap(year) { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let mut month = 1;
    for length in months {
        if days < length {
            break;
        }
        days -= length;
        month += 1;
    }
    let day = days + 1;
    let within = seconds % 86400;
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{nanoseconds:09}Z",
        within / 3600,
        within % 3600 / 60,
        within % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn utc_dates_cover_leap_and_century_boundaries() {
        assert_eq!(utc_timestamp(0, 0), "1970-01-01T00:00:00.000000000Z");
        assert_eq!(
            utc_timestamp(951782400, 42),
            "2000-02-29T00:00:00.000000042Z"
        );
        assert_eq!(
            utc_timestamp(4107542400, 0),
            "2100-03-01T00:00:00.000000000Z"
        );
    }
}
