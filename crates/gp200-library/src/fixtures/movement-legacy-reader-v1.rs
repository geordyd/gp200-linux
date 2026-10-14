//! Compiled pre-movement reader fixture. Original field layouts/hash/old workflow decoder
//! are copied from pinned source-before inputs, not the new movement decoder.
use crate::LibraryStore;
use crate::store::{
    MAX_BLOB, MAX_MANIFEST, MAX_VERSIONS, StoredAsset, StoredEntry, StoredVersion, digest, invalid,
    nonce, valid_hash,
};
use crate::store::{atomic_write, read_bounded, validate_metadata};
use gp200_domain::*;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};
const SCHEMA_VERSION: u32 = 1;
const EXTENSION: &str = "gp200-studio.workflows";
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Workflows {
    pub schema_version: u32,
    pub backups: BTreeMap<String, crate::DeviceBackup>,
    pub restores: BTreeMap<String, crate::RestoreJournal>,
    pub setlists: BTreeMap<String, crate::Setlist>,
}
impl Default for Workflows {
    fn default() -> Self {
        Self {
            schema_version: 1,
            backups: BTreeMap::new(),
            restores: BTreeMap::new(),
            setlists: BTreeMap::new(),
        }
    }
}
pub(crate) fn valid_workflow_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}
pub(crate) fn new_id(prefix: &str) -> Result<String, ServiceError> {
    Ok(format!("{prefix}-{}", nonce()?))
}
pub(crate) fn decode_workflows(manifest: &Manifest) -> Result<Workflows, ServiceError> {
    let Some(value) = manifest.extensions.get(EXTENSION) else {
        return Ok(Workflows::default());
    };
    let workflows: Workflows = serde_json::from_value(value.clone()).map_err(invalid)?;
    if workflows.schema_version != 1 {
        return Err(ServiceError::unsupported(
            "Unsupported device workflows schema; no migration or rollback was attempted",
        ));
    }
    if workflows.backups.len() > 64
        || workflows.restores.len() > 128
        || workflows.setlists.len() > 256
    {
        return Err(invalid("Device workflows item limit exceeded"));
    }
    for (id, backup) in &workflows.backups {
        if id != &backup.id || !valid_workflow_id(id) {
            return Err(invalid("Invalid backup identity"));
        }
        backup.validate()?;
    }
    for (id, journal) in &workflows.restores {
        if id != &journal.id || !valid_workflow_id(id) {
            return Err(invalid("Invalid restore journal identity"));
        }
        journal.validate()?;
    }
    for (id, setlist) in &workflows.setlists {
        if id != &setlist.id || !valid_workflow_id(id) {
            return Err(invalid("Invalid setlist identity"));
        }
        setlist.validate()?;
    }
    Ok(workflows)
}
pub(crate) fn validate_graph(manifest: &Manifest) -> Result<(), ServiceError> {
    if manifest.schema_version != SCHEMA_VERSION {
        return Err(ServiceError::unsupported("Unsupported library schema"));
    }
    decode_workflows(manifest)?;
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
fn envelope_bytes(manifest: &Manifest) -> Result<Vec<u8>, ServiceError> {
    let sha256 = digest(&serde_json::to_vec(manifest).map_err(invalid)?);
    serde_json::to_vec(&ManifestEnvelope {
        format: "gp200-studio.library".into(),
        sha256,
        manifest: manifest.clone(),
    })
    .map_err(invalid)
}
pub struct Reader<'a> {
    pub store: &'a LibraryStore,
}
impl Reader<'_> {
    pub fn read_manifest(&self, path: &Path) -> Result<Manifest, ServiceError> {
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

    fn validate_manifest(&self, manifest: &Manifest) -> Result<(), ServiceError> {
        validate_graph(manifest)?;
        for version in manifest.versions.values() {
            self.store.load_stored(version)?;
        }
        for asset in manifest.assets.values() {
            if self.store.read_blob(&asset.hash)?.len() as u64 != asset.bytes {
                return Err(invalid("Asset length mismatch"));
            }
        }
        let workflows = decode_workflows(manifest)?;
        for backup in workflows.backups.values() {
            self.store.validate_backup_blobs(backup)?;
        }
        for journal in workflows.restores.values() {
            self.store.validate_restore_blobs(journal)?;
        }
        Ok(())
    }
    // The pre-feature open fallback decision, isolated from lock acquisition.
    pub fn read_with_fallback(&self) -> Result<(Manifest, bool), ServiceError> {
        match self.read_manifest(&self.store.root.join("manifest.json")) {
            Ok(manifest) => Ok((manifest, false)),
            Err(current_error) => {
                if current_error.kind == ErrorKind::Unsupported {
                    return Err(current_error);
                }
                let recovered = self.read_manifest(&self.store.root.join("manifest.previous.json"))
                    .map_err(|previous_error| invalid(format!("Library manifest is invalid ({current_error}); recovery manifest is also invalid ({previous_error})")))?;
                Ok((recovered, true))
            }
        }
    }
    pub fn rewrite_preserving(&self, mut next: Manifest) -> Result<Manifest, ServiceError> {
        let old = self.read_manifest(&self.store.root.join("manifest.json"))?;
        next.generation = old
            .generation
            .checked_add(1)
            .ok_or_else(|| invalid("Library generation exhausted"))?;
        validate_graph(&next)?;
        let bytes = envelope_bytes(&next)?;
        if bytes.len() as u64 > MAX_MANIFEST {
            return Err(invalid("Library manifest exceeds size limit"));
        }
        atomic_write(
            &self.store.root,
            &self.store.root.join("manifest.previous.json"),
            &envelope_bytes(&old)?,
        )?;
        atomic_write(
            &self.store.root,
            &self.store.root.join("manifest.json"),
            &bytes,
        )?;
        Ok(next)
    }
}
