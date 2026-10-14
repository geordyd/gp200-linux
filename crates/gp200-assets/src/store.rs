use crate::{
    MAX_ASSETS, MAX_SOURCE_BYTES, PrepareOptions, inspect_firmware, inspect_nam_with_size,
    inspect_snaptone, inspect_wav, invalid, prepare_audio, write_float_wav,
};
use fs2::FileExt;
use gp200_domain::*;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

const MAX_MANIFEST_BYTES: u64 = 16 * 1024 * 1024;
/// Physical storage is selected by the content's role, never by trying both
/// directories. This stays private so existing serialized manifests and their
/// full-manifest recipe IDs remain unchanged.
#[derive(Debug, Clone, Copy)]
enum BlobLocation {
    Original,
    Derived,
}
impl BlobLocation {
    fn directory(self) -> &'static str {
        match self {
            Self::Original => "originals",
            Self::Derived => "derived",
        }
    }
    fn content(asset: &AssetManifest) -> Self {
        if asset.derived_sha256.is_some() {
            Self::Derived
        } else {
            Self::Original
        }
    }
    fn dependency(asset: &AssetManifest, name: &str) -> Self {
        match (asset.conversion_profile.as_deref(), name) {
            (
                Some("independent-namtoclo-42c07456-rust-nam-v1"),
                "calibration_intermediate" | "trainer_input_f32" | "nam_response_f32",
            ) => Self::Derived,
            // input_wav and existing arbitrary legacy dependencies were always
            // retained as originals. New derived profiles need explicit roles.
            _ => Self::Original,
        }
    }
}
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
/// A unique, immutable conversion recipe independent of the deduplicated output content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConversionRecipe {
    pub id: String,
    pub manifest: AssetManifest,
}
impl ConversionRecipe {
    fn from_manifest(manifest: &AssetManifest) -> Result<Self, ServiceError> {
        let mut identity = manifest.clone();
        identity.created_at_unix_ms = 0;
        let id = digest(&serde_json::to_vec(&identity).map_err(io)?);
        Ok(Self {
            id,
            manifest: manifest.clone(),
        })
    }
}
fn private_directory(path: &Path) -> Result<(), ServiceError> {
    reject_symlink(path)?;
    fs::create_dir_all(path).map_err(io)?;
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(io)?;
    Ok(())
}
fn private_options() -> OpenOptions {
    let mut options = OpenOptions::new();
    #[cfg(unix)]
    options.mode(0o600);
    options
}
fn nam_size(options: &BTreeMap<String, String>) -> Result<f64, ServiceError> {
    let size = options
        .get("nam_size")
        .map(|value| {
            value
                .parse::<f64>()
                .map_err(|_| invalid("nam_size must be finite in 0–1"))
        })
        .transpose()?
        .unwrap_or(1.0);
    if !size.is_finite() || !(0.0..=1.0).contains(&size) {
        return Err(invalid("nam_size must be finite in 0–1"));
    }
    Ok(size)
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssetManifest {
    pub schema_version: u32,
    pub info: AssetInfo,
    pub original_sha256: String,
    pub original_name: String,
    pub derived_sha256: Option<String>,
    pub conversion_profile: Option<String>,
    pub options: BTreeMap<String, String>,
    #[serde(default)]
    pub dependencies: BTreeMap<String, String>,
    pub created_at_unix_ms: u128,
}
#[derive(Debug, Clone)]
pub struct LocalAssetResult {
    pub original: AssetInfo,
    pub derived: Option<AssetInfo>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoreManifest {
    version: u32,
    assets: BTreeMap<AssetId, AssetManifest>,
    #[serde(default)]
    recipes: BTreeMap<String, ConversionRecipe>,
}
impl Default for StoreManifest {
    fn default() -> Self {
        Self {
            version: 2,
            assets: BTreeMap::new(),
            recipes: BTreeMap::new(),
        }
    }
}
/// Single-writer immutable content-addressed asset store. Original files are never rewritten.
pub struct AssetStore {
    root: PathBuf,
    manifest: StoreManifest,
    _lock: File,
}
pub fn data_dir() -> Result<PathBuf, ServiceError> {
    let root = if let Some(path) = std::env::var_os("GP200_STUDIO_DATA") {
        PathBuf::from(path)
    } else if let Some(path) =
        std::env::var_os("XDG_DATA_HOME").filter(|path| Path::new(path).is_absolute())
    {
        PathBuf::from(path).join("gp200-studio")
    } else {
        PathBuf::from(std::env::var_os("HOME").ok_or_else(|| {
            ServiceError::new(
                ErrorKind::Io,
                "No XDG_DATA_HOME or HOME; set GP200_STUDIO_DATA",
            )
        })?)
        .join(".local/share/gp200-studio")
    };
    if !root.is_absolute() {
        return Err(invalid("Asset data directory must be absolute"));
    }
    Ok(root.join("assets"))
}
fn io(error: impl std::fmt::Display) -> ServiceError {
    ServiceError::new(ErrorKind::Io, error.to_string())
}
pub(crate) fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn valid_hash(hash: &str) -> bool {
    hash.len() == 64
        && hash
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}
fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}
fn reject_symlink(path: &Path) -> Result<(), ServiceError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(invalid(format!(
            "Asset store path is a symbolic link: {}",
            path.display()
        ))),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(io(error)),
    }
}
fn read_bounded(path: &Path, bound: u64) -> Result<Vec<u8>, ServiceError> {
    let file = File::open(path).map_err(io)?;
    let metadata = file.metadata().map_err(io)?;
    if !metadata.is_file() || metadata.len() == 0 || metadata.len() > bound {
        return Err(invalid(
            "Asset source must be a non-empty regular file within the local size bound",
        ));
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(bound + 1).read_to_end(&mut bytes).map_err(io)?;
    if bytes.len() as u64 > bound {
        return Err(invalid("Asset source grew beyond its local byte bound"));
    }
    Ok(bytes)
}
impl AssetStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, ServiceError> {
        let path = path.as_ref();
        private_directory(path)?;
        let root = fs::canonicalize(path).map_err(io)?;
        for directory in ["originals", "derived", "jobs"] {
            let path = root.join(directory);
            reject_symlink(&path)?;
            private_directory(&path)?;
        }
        let lock_path = root.join("writer.lock");
        reject_symlink(&lock_path)?;
        let lock = private_options()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(lock_path)
            .map_err(io)?;
        lock.try_lock_exclusive().map_err(|error| {
            ServiceError::new(
                ErrorKind::Busy,
                format!("Asset store already has a writer: {error}"),
            )
        })?;
        let manifest_path = root.join("manifest.json");
        reject_symlink(&manifest_path)?;
        let manifest: StoreManifest = if manifest_path.exists() {
            serde_json::from_slice(&read_bounded(&manifest_path, MAX_MANIFEST_BYTES)?)
                .map_err(|error| invalid(format!("Invalid asset manifest: {error}")))?
        } else {
            StoreManifest::default()
        };
        if ![1, 2].contains(&manifest.version) || manifest.assets.len() > MAX_ASSETS {
            return Err(invalid("Unsupported or oversized asset manifest"));
        }
        let mut store = Self {
            root,
            manifest,
            _lock: lock,
        };
        for (id, asset) in &store.manifest.assets {
            let hash = asset
                .derived_sha256
                .as_deref()
                .unwrap_or(&asset.original_sha256);
            if asset.schema_version != 1
                || !valid_hash(hash)
                || !valid_hash(&asset.original_sha256)
                || id.0 != format!("sha256:{hash}")
                || id != &asset.info.id
                || asset.info.bytes > MAX_SOURCE_BYTES as u64
                || asset.original_name.len() > 4096
                || asset.info.name.len() > 8192
                || asset.info.properties.len() > 64
                || asset
                    .info
                    .properties
                    .iter()
                    .any(|(key, value)| key.len() > 128 || value.len() > 32768)
                || asset.info.warnings.len() > 32
                || asset
                    .info
                    .warnings
                    .iter()
                    .any(|warning| warning.len() > 4096)
                || asset.options.len() > 32
                || asset
                    .options
                    .iter()
                    .any(|(key, value)| key.len() > 128 || value.len() > 4096)
                || asset.dependencies.len() > 16
                || asset
                    .dependencies
                    .iter()
                    .any(|(name, hash)| name.len() > 128 || !valid_hash(hash))
            {
                return Err(invalid("Invalid asset manifest identity or bounds"));
            }
            let expected = store.blob_path(hash, BlobLocation::content(asset));
            if expected != asset.info.path {
                return Err(invalid("Asset manifest points outside immutable storage"));
            }
            // Content hashes are checked before every read, not by scanning up to 256 GiB
            // on GUI startup. Startup validates location, type and byte bounds only.
            for (content_hash, location) in [
                (hash, BlobLocation::content(asset)),
                (&asset.original_sha256, BlobLocation::Original),
            ] {
                store.validate_blob_metadata(
                    content_hash,
                    location,
                    "Retained asset has an invalid file type or length",
                )?;
            }
            for (name, hash) in &asset.dependencies {
                store.validate_blob_metadata(
                    hash,
                    BlobLocation::dependency(asset, name),
                    "Retained asset has an invalid file type or length",
                )?;
            }
        }
        if store.manifest.recipes.len() > MAX_ASSETS {
            return Err(invalid("Conversion history exceeds its 4096-entry bound"));
        }
        for (id, recipe) in &store.manifest.recipes {
            let asset = &recipe.manifest;
            if asset.schema_version != 1
                || asset.info.bytes == 0
                || asset.info.bytes > MAX_SOURCE_BYTES as u64
                || asset.original_name.len() > 4096
                || asset.info.name.len() > 8192
                || asset.info.warnings.len() > 32
                || asset
                    .info
                    .warnings
                    .iter()
                    .any(|warning| warning.len() > 4096)
                || asset
                    .conversion_profile
                    .as_ref()
                    .is_none_or(|profile| profile.len() > 4096)
                || asset.derived_sha256.as_ref().is_none_or(|hash| {
                    !valid_hash(hash)
                        || asset.info.id.0 != format!("sha256:{hash}")
                        || asset.info.path != store.root.join("derived").join(hash)
                })
                || id != &recipe.id
                || id != &ConversionRecipe::from_manifest(asset)?.id
                || asset.derived_sha256.is_none()
                || !store.manifest.assets.contains_key(&asset.info.id)
                || !valid_hash(&asset.original_sha256)
                || asset.options.len() > 32
                || asset.dependencies.len() > 16
                || asset
                    .options
                    .iter()
                    .any(|(key, value)| key.len() > 128 || value.len() > 4096)
                || asset.info.properties.len() > 64
                || asset
                    .info
                    .properties
                    .iter()
                    .any(|(key, value)| key.len() > 128 || value.len() > 32768)
                || asset
                    .dependencies
                    .iter()
                    .any(|(name, hash)| name.len() > 128 || !valid_hash(hash))
            {
                return Err(invalid("Invalid conversion history identity or bounds"));
            }
            store.validate_blob_metadata(
                &asset.original_sha256,
                BlobLocation::Original,
                "Invalid conversion history source",
            )?;
            for (name, hash) in &asset.dependencies {
                store.validate_blob_metadata(
                    hash,
                    BlobLocation::dependency(asset, name),
                    "Invalid conversion history source",
                )?;
            }
        }
        // Legacy manifests retain their first known recipe. Later identical content gets separate recipes.
        let legacy: Vec<_> = store
            .manifest
            .assets
            .values()
            .filter(|asset| asset.derived_sha256.is_some())
            .cloned()
            .collect();
        let legacy_version = store.manifest.version == 1;
        let mut migrated = legacy_version;
        store.manifest.version = 2;
        for asset in legacy {
            let recipe = ConversionRecipe::from_manifest(&asset)?;
            if !store.manifest.recipes.contains_key(&recipe.id) {
                if !legacy_version {
                    return Err(invalid(
                        "Conversion history omitted the first retained recipe",
                    ));
                }
                if store.manifest.recipes.len() >= MAX_ASSETS {
                    return Err(invalid("Conversion history reached its 4096-entry bound"));
                }
                store.manifest.recipes.insert(recipe.id.clone(), recipe);
                migrated = true;
            }
        }
        if migrated {
            store.persist()?;
        }
        store.recover_journals()?;
        Ok(store)
    }
    pub fn inventory(&self) -> Vec<AssetInfo> {
        self.manifest
            .assets
            .values()
            .map(|asset| self.with_history(asset.info.clone()))
            .collect()
    }
    pub fn manifests(&self) -> &BTreeMap<AssetId, AssetManifest> {
        &self.manifest.assets
    }
    pub fn conversion_recipes(&self) -> &BTreeMap<String, ConversionRecipe> {
        &self.manifest.recipes
    }
    pub fn recipe_original_bytes(&self, recipe: &str) -> Result<Vec<u8>, ServiceError> {
        let recipe = self
            .manifest
            .recipes
            .get(recipe)
            .ok_or_else(|| invalid("Unknown conversion recipe"))?;
        self.verified_blob(&recipe.manifest.original_sha256, BlobLocation::Original)
    }
    pub fn recipe_dependency_bytes(
        &self,
        recipe: &str,
        dependency: &str,
    ) -> Result<Vec<u8>, ServiceError> {
        let recipe = self
            .manifest
            .recipes
            .get(recipe)
            .ok_or_else(|| invalid("Unknown conversion recipe"))?;
        let hash = recipe
            .manifest
            .dependencies
            .get(dependency)
            .ok_or_else(|| invalid("Unknown recipe dependency"))?;
        self.verified_blob(hash, BlobLocation::dependency(&recipe.manifest, dependency))
    }
    fn with_history(&self, mut info: AssetInfo) -> AssetInfo {
        let mut recipes: Vec<_> = self
            .manifest
            .recipes
            .values()
            .filter(|recipe| recipe.manifest.info.id == info.id)
            .collect();
        if recipes.is_empty() {
            return info;
        }
        recipes.sort_by_key(|recipe| (recipe.manifest.created_at_unix_ms, &recipe.id));
        info.properties
            .insert("conversion_recipe_count".into(), recipes.len().to_string());
        let mut preview = Vec::new();
        for recipe in recipes.into_iter().rev().take(8) {
            let manifest = &recipe.manifest;
            let entry = serde_json::json!({ "id":recipe.id, "created_at_unix_ms":manifest.created_at_unix_ms, "source_sha256":manifest.original_sha256, "source_name":manifest.original_name, "profile":manifest.conversion_profile, "options":manifest.options, "dependencies":manifest.dependencies, "nam_size":manifest.info.properties.get("nam_size"), "size_selection":manifest.info.properties.get("size_selection"), "activation":manifest.info.properties.get("activation"), "operations_per_sample":manifest.info.properties.get("operations_per_sample"), "estimated_operations":manifest.info.properties.get("estimated_operations") });
            preview.push(entry);
            if serde_json::to_vec(&preview).expect("history JSON").len() > 32768 {
                preview.pop();
                break;
            }
        }
        info.properties.insert(
            "conversion_history_json".into(),
            serde_json::to_string(&preview).expect("history JSON"),
        );
        info
    }
    pub fn root(&self) -> &Path {
        &self.root
    }
    pub fn original_bytes(&self, id: &AssetId) -> Result<Vec<u8>, ServiceError> {
        let asset = self
            .manifest
            .assets
            .get(id)
            .ok_or_else(|| invalid("Unknown retained asset"))?;
        self.verified_blob(&asset.original_sha256, BlobLocation::Original)
    }
    pub fn content_bytes(&self, id: &AssetId) -> Result<Vec<u8>, ServiceError> {
        let asset = self
            .manifest
            .assets
            .get(id)
            .ok_or_else(|| invalid("Unknown retained asset"))?;
        self.verified_blob(
            asset
                .derived_sha256
                .as_deref()
                .unwrap_or(&asset.original_sha256),
            BlobLocation::content(asset),
        )
    }
    pub fn dependency_bytes(&self, id: &AssetId, name: &str) -> Result<Vec<u8>, ServiceError> {
        let asset = self
            .manifest
            .assets
            .get(id)
            .ok_or_else(|| invalid("Unknown retained asset"))?;
        let hash = asset
            .dependencies
            .get(name)
            .ok_or_else(|| invalid("Unknown retained asset dependency"))?;
        self.verified_blob(hash, BlobLocation::dependency(asset, name))
    }
    fn blob_path(&self, hash: &str, location: BlobLocation) -> PathBuf {
        self.root.join(location.directory()).join(hash)
    }
    fn validate_blob_metadata(
        &self,
        hash: &str,
        location: BlobLocation,
        invalid_metadata: &str,
    ) -> Result<(), ServiceError> {
        if !valid_hash(hash) {
            return Err(invalid("Invalid asset hash"));
        }
        let path = self.blob_path(hash, location);
        reject_symlink(&path)?;
        let metadata = fs::metadata(path).map_err(io)?;
        if !metadata.is_file() || metadata.len() == 0 || metadata.len() > MAX_SOURCE_BYTES as u64 {
            return Err(invalid(invalid_metadata));
        }
        Ok(())
    }
    fn verified_blob(&self, hash: &str, location: BlobLocation) -> Result<Vec<u8>, ServiceError> {
        if !valid_hash(hash) {
            return Err(invalid("Invalid asset hash"));
        }
        let path = self.blob_path(hash, location);
        reject_symlink(&path)?;
        let bytes = read_bounded(&path, MAX_SOURCE_BYTES as u64)?;
        if digest(&bytes) != hash {
            return Err(invalid(
                "Retained asset hash mismatch; corrupt content was not used",
            ));
        }
        Ok(bytes)
    }
    fn put_blob(
        &self,
        bytes: &[u8],
        location: BlobLocation,
    ) -> Result<(String, PathBuf), ServiceError> {
        if bytes.is_empty() || bytes.len() > MAX_SOURCE_BYTES {
            return Err(invalid("Asset content is empty or oversized"));
        }
        let hash = digest(bytes);
        let path = self.blob_path(&hash, location);
        reject_symlink(&path)?;
        if path.exists() {
            self.verified_blob(&hash, location)?;
            return Ok((hash, path));
        }
        let temp = self.root.join(location.directory()).join(format!(
            ".{}-{}.tmp",
            std::process::id(),
            now_ms()
        ));
        let mut file = private_options()
            .write(true)
            .create_new(true)
            .open(&temp)
            .map_err(io)?;
        let write = file.write_all(bytes).and_then(|()| file.sync_all());
        drop(file);
        if let Err(error) = write {
            let _ = fs::remove_file(&temp);
            return Err(io(error));
        }
        // Hard link publishes complete content without replacing an existing hash path.
        let published = fs::hard_link(&temp, &path);
        let _ = fs::remove_file(&temp);
        match published {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                self.verified_blob(&hash, location)?;
            }
            Err(error) => return Err(io(error)),
        }
        File::open(path.parent().expect("blob directory"))
            .and_then(|directory| directory.sync_all())
            .map_err(io)?;
        Ok((hash, path))
    }
    fn persist(&self) -> Result<(), ServiceError> {
        let bytes = serde_json::to_vec_pretty(&self.manifest).map_err(io)?;
        if bytes.len() as u64 > MAX_MANIFEST_BYTES {
            return Err(invalid("Asset manifest exceeds its byte bound"));
        }
        self.atomic_file(&self.root.join("manifest.json"), &bytes)
    }
    fn atomic_file(&self, path: &Path, bytes: &[u8]) -> Result<(), ServiceError> {
        reject_symlink(path)?;
        if let Ok(metadata) = fs::symlink_metadata(path)
            && !metadata.is_file()
        {
            return Err(invalid(
                "Atomic asset-store destination must be a regular file",
            ));
        }
        let temp = path.with_extension(format!(
            "json.{}-{}.tmp",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let mut file = private_options()
            .write(true)
            .create_new(true)
            .open(&temp)
            .map_err(io)?;
        let written = file.write_all(bytes).and_then(|()| file.sync_all());
        drop(file);
        if let Err(error) = written {
            let _ = fs::remove_file(&temp);
            return Err(io(error));
        }
        if let Err(error) = fs::rename(&temp, path) {
            let _ = fs::remove_file(&temp);
            return Err(io(error));
        }
        File::open(path.parent().expect("store directory"))
            .and_then(|directory| directory.sync_all())
            .map_err(|error| ServiceError::new(ErrorKind::Indeterminate, format!("Asset metadata was published but directory durability could not be confirmed: {error}")))
    }
    fn insert(&mut self, asset: AssetManifest) -> Result<AssetInfo, ServiceError> {
        let existing = self.manifest.assets.get(&asset.info.id);
        if existing.is_none() && self.manifest.assets.len() >= MAX_ASSETS {
            return Err(invalid(
                "Asset inventory reached the local 4096-entry limit",
            ));
        }
        if existing.is_some_and(|existing| existing.info.kind != asset.info.kind) {
            return Err(invalid(
                "The same content is already retained with another asset kind",
            ));
        }
        let recipe = asset
            .derived_sha256
            .as_ref()
            .map(|_| ConversionRecipe::from_manifest(&asset))
            .transpose()?;
        let new_recipe = recipe.filter(|recipe| !self.manifest.recipes.contains_key(&recipe.id));
        if new_recipe.is_some() && self.manifest.recipes.len() >= MAX_ASSETS {
            return Err(invalid("Conversion history reached its 4096-entry bound"));
        }
        let id = asset.info.id.clone();
        let mut info = asset.info.clone();
        let new_asset = existing.is_none();
        let recipe_id = new_recipe.as_ref().map(|recipe| recipe.id.clone());
        if let Some(recipe) = &new_recipe {
            self.manifest
                .recipes
                .insert(recipe.id.clone(), recipe.clone());
        }
        if asset.derived_sha256.is_some() {
            info.properties.insert(
                "conversion_recipe_id".into(),
                ConversionRecipe::from_manifest(&asset)?.id,
            );
        }
        if new_asset {
            self.manifest.assets.insert(id.clone(), asset);
        }
        if (new_asset || new_recipe.is_some())
            && let Err(error) = self.persist()
        {
            // A post-rename durability error must keep the visible manifest state;
            // reverting only memory would overwrite a published recipe on the next job.
            if error.kind != ErrorKind::Indeterminate {
                if new_asset {
                    self.manifest.assets.remove(&id);
                }
                if let Some(recipe_id) = recipe_id {
                    self.manifest.recipes.remove(&recipe_id);
                }
            }
            return Err(error);
        }
        Ok(self.with_history(info))
    }
    /// Import retains bytes even if semantic validation later fails.
    pub fn inspect(&mut self, spec: &AssetJobSpec) -> Result<AssetInfo, ServiceError> {
        if spec.options.len() > 32
            || spec
                .options
                .iter()
                .any(|(key, value)| key.len() > 128 || value.len() > 4096)
        {
            return Err(invalid("Asset options exceed their local bounds"));
        }
        let bytes = read_bounded(&spec.source, MAX_SOURCE_BYTES as u64)?;
        let original_name = spec
            .source
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        if original_name.len() > 4096 {
            return Err(invalid("Asset source name exceeds the local bound"));
        }
        let (hash, path) = self.put_blob(&bytes, BlobLocation::Original)?;
        let mut info = AssetInfo {
            id: AssetId(format!("sha256:{hash}")),
            kind: spec.kind,
            name: original_name.clone(),
            path,
            bytes: bytes.len() as u64,
            properties: BTreeMap::from([
                ("sha256".into(), hash.clone()),
                ("source_retained".into(), "true".into()),
                ("device_compatible".into(), "unknown".into()),
                ("decoder".into(), "gp200-assets-0.1.0".into()),
            ]),
            warnings: vec![],
        };
        let validation = match spec.kind {
            AssetKind::ImpulseResponse => inspect_wav(&bytes).and_then(|audio| audio.inspect()).map(|audio| {
                for (key, value) in [("sample_rate", audio.sample_rate.to_string()), ("channels", audio.channels.to_string()), ("bits", audio.bits.to_string()), ("format", format!("{:?}", audio.encoding)), ("frames", audio.frames.to_string()), ("seconds", format!("{:.6}", audio.seconds)), ("peak", format!("{:.8}", audio.peak)), ("rms", format!("{:.8}", audio.rms)), ("dc", format!("{:.8}", audio.dc)), ("clipped_samples", audio.clipped_samples.to_string())] { info.properties.insert(key.into(), value); }
                info.properties.insert("waveform_json".into(), serde_json::to_string(&audio.waveform).expect("finite waveform"));
                if audio.clipped_samples > 0 { info.warnings.push(format!("{} samples reach or exceed full scale; inspect clipping before conversion", audio.clipped_samples)); }
                if audio.channels > 1 { info.warnings.push("Multi-channel source retained. Channel mixing occurs only when explicitly selected.".into()); }
                info.properties.insert("native_ir_sample_rate".into(), "44100".into());
                info.properties.insert("native_ir_samples".into(), "1024".into());
                info.warnings.push("Native IR packing uses mono PCM24/44100 and 1024 signed samples. Local WAV decoding and preview preparation do not verify pedal upload or native resampling equivalence.".into());
            }),
            AssetKind::Nam => inspect_nam_with_size(&bytes, nam_size(&spec.options)?).map(|nam| {
                info.properties.insert("architecture".into(), nam.architecture);
                info.properties.insert("nam_version".into(), nam.version);
                info.properties.insert("weight_count".into(), nam.weight_count.to_string());
                info.properties.insert("nam_size".into(), nam.requested_size.to_string());
                if let Some(plan) = nam.size_plan { info.properties.insert("size_plan_json".into(), serde_json::to_string(&plan).expect("size plan")); }
                if let Some(selection) = nam.size_selection { info.properties.insert("size_selection".into(), selection.to_string()); }
                if let Some(operations)=nam.operations_per_sample {info.properties.insert("operations_per_sample".into(),operations.to_string());}
                if let Some(profile)=nam.network_profile {info.properties.insert("network_profile".into(),profile);}
                info.properties.insert("sample_rate".into(), nam.render_sample_rate.map(|rate| rate.to_string()).or_else(|| nam.sample_rate.map(|rate| rate.to_string())).unwrap_or_else(|| "unspecified".into()));
                info.properties.insert("inference_available".into(), nam.supported_inference.to_string());
                info.warnings.push(nam.reason);
            }),
            AssetKind::SnapTone => inspect_snaptone(&bytes).map(|container| {
                let inspected = container.info();
                info.properties.insert("container_format".into(), "VTSI-clo-native".into());
                info.properties.insert("integrity".into(), "crc16-modbus-be-valid".into());
                info.properties.insert("coefficient_bytes".into(), inspected.coefficient_bytes.to_string());
                info.properties.insert("declared_bytes".into(), inspected.declared_bytes.to_string());
                info.properties.insert("container_profile".into(), inspected.container_profile.clone());
                info.properties.insert("snaptone_info_json".into(), serde_json::to_string(inspected).expect("raw-word metadata"));
                info.warnings.push("VTSI layout and CRC were recovered statically. A genuine .clo golden file and pedal upload/readback remain unverified; no upload ticket was created.".into());
            }),
            AssetKind::Firmware => inspect_firmware(&bytes).map(|image| {
                let inspected = image.info();
                info.properties.insert("container_format".into(), "FRMW-native".into());
                info.properties.insert("firmware_product".into(), inspected.model.name());
                info.properties.insert("firmware_image_version".into(), inspected.image_version.clone().unwrap_or_else(|| "unknown".into()));
                info.properties.insert("integrity".into(), "additive8-valid".into());
                info.properties.insert("firmware_info_json".into(), serde_json::to_string(inspected).expect("integer metadata"));
                info.warnings.push("The additive checksum detects corruption and is not a signature. Image version is not the connected pedal version. Bootloader transfer/recovery remain unverified; no update ticket was created.".into());
            }),
        };
        if let Err(error) = &validation {
            info.properties.insert("validation".into(), "failed".into());
            info.warnings.push(error.message.clone());
        } else {
            info.properties
                .insert("validation".into(), "inspected".into());
        }
        let result = self.insert(AssetManifest {
            schema_version: 1,
            info,
            original_sha256: hash,
            original_name,
            derived_sha256: None,
            conversion_profile: None,
            options: BTreeMap::new(),
            dependencies: BTreeMap::new(),
            created_at_unix_ms: now_ms(),
        })?;
        validation?;
        Ok(result)
    }
    pub fn prepare(
        &mut self,
        spec: &AssetJobSpec,
        is_cancelled: impl Fn() -> bool,
    ) -> Result<LocalAssetResult, ServiceError> {
        self.prepare_with_progress(spec, is_cancelled, |_, _| {})
    }
    pub fn prepare_with_progress(
        &mut self,
        spec: &AssetJobSpec,
        is_cancelled: impl Fn() -> bool,
        progress: impl FnMut(u64, u64),
    ) -> Result<LocalAssetResult, ServiceError> {
        let original = self.inspect(spec)?;
        if is_cancelled() {
            return Err(crate::cancelled());
        }
        if spec.kind == AssetKind::Nam {
            if spec.options.get("output_format").map(String::as_str) == Some("clo") {
                return self.calibrate_nam(spec, original, is_cancelled, progress);
            }
            return self.render_nam(spec, original, is_cancelled, progress);
        }
        if spec.kind != AssetKind::ImpulseResponse {
            return Err(ServiceError::new(
                ErrorKind::Unverified,
                match spec.kind {
                    AssetKind::Nam => {
                        "NAM model inference and calibration are not yet validated. The original is retained; no substituted network or SnapTone was produced."
                    }
                    AssetKind::Firmware => {
                        "Firmware inspection validates the FRMW container, product and additive checksum; the bootloader transfer/recovery profile is unverified, so no updater ticket can be produced."
                    }
                    _ => {
                        "SnapTone inspection validates the native VTSI container and CRC; the pedal transfer profile is unverified, so no upload ticket can be produced."
                    }
                },
            ));
        }
        let options = PrepareOptions::from_options(&spec.options)?;
        let bytes = self.original_bytes(&original.id)?;
        let audio = inspect_wav(&bytes)?;
        let prepared = prepare_audio(&audio, &options, &is_cancelled)?;
        let bytes = write_float_wav(&prepared)?;
        if is_cancelled() {
            return Err(crate::cancelled());
        }
        let (hash, path) = self.put_blob(&bytes, BlobLocation::Derived)?;
        let mut properties = original.properties.clone();
        let inspected = prepared.inspect()?;
        properties.insert("sha256".into(), hash.clone());
        properties.insert(
            "original_sha256".into(),
            original.id.0.trim_start_matches("sha256:").into(),
        );
        properties.insert("sample_rate".into(), prepared.sample_rate.to_string());
        properties.insert("channels".into(), prepared.channels.to_string());
        properties.insert("bits".into(), "32".into());
        properties.insert("format".into(), "Float".into());
        properties.insert("frames".into(), prepared.frames().to_string());
        properties.insert("seconds".into(), format!("{:.6}", inspected.seconds));
        properties.insert("peak".into(), format!("{:.8}", inspected.peak));
        properties.insert("rms".into(), format!("{:.8}", inspected.rms));
        properties.insert("dc".into(), format!("{:.8}", inspected.dc));
        properties.insert(
            "clipped_samples".into(),
            inspected.clipped_samples.to_string(),
        );
        properties.insert(
            "waveform_json".into(),
            serde_json::to_string(&inspected.waveform).expect("finite waveform"),
        );
        properties.insert("conversion_profile".into(), "local-wav-f32-sinc-v1".into());
        let info = AssetInfo { id: AssetId(format!("sha256:{hash}")), kind: spec.kind, name: format!("{} · prepared WAV", original.name), path, bytes: bytes.len() as u64, properties, warnings: vec!["Local float32 WAV prepared from retained original. This is not a GP-200 coefficient payload or upload profile.".into()] };
        let derived = self.insert(AssetManifest {
            schema_version: 1,
            info,
            original_sha256: original.id.0.trim_start_matches("sha256:").into(),
            original_name: original.name.clone(),
            derived_sha256: Some(hash),
            conversion_profile: Some("local-wav-f32-sinc-v1".into()),
            options: spec.options.clone(),
            dependencies: BTreeMap::new(),
            created_at_unix_ms: now_ms(),
        })?;
        if let Some(output) = &spec.output {
            self.export_bytes(output, &spec.source, &bytes)?;
        }
        Ok(LocalAssetResult {
            original,
            derived: Some(derived),
        })
    }
    fn calibrate_nam(
        &mut self,
        spec: &AssetJobSpec,
        original: AssetInfo,
        is_cancelled: impl Fn() -> bool,
        progress: impl FnMut(u64, u64),
    ) -> Result<LocalAssetResult, ServiceError> {
        for key in spec.options.keys() {
            if !matches!(
                key.as_str(),
                "target_slot"
                    | "input_wav"
                    | "input_channel"
                    | "activation"
                    | "max_operations"
                    | "output_format"
                    | "calibration_timeout_seconds"
            ) {
                return Err(invalid(format!("Unknown NAM calibration option: {key}")));
            }
        }
        let input_path = spec
            .options
            .get("input_wav")
            .filter(|path| !path.is_empty())
            .ok_or_else(|| {
                invalid("Explicitly select the verified GP-200 70-second calibration stimulus")
            })?;
        let input_bytes = read_bounded(Path::new(input_path), MAX_SOURCE_BYTES as u64)?;
        let (input_hash, _) = self.put_blob(&input_bytes, BlobLocation::Original)?;
        if input_hash != crate::GP200_STIMULUS_WAV_SHA256 {
            return Err(invalid(
                "Selected calibration WAV does not match the verified shipped GP-200 V1.8.1 stimulus",
            ));
        }
        let source = inspect_wav(&input_bytes)?;
        let channel = spec
            .options
            .get("input_channel")
            .ok_or_else(|| invalid("Select calibration input channel 0 or 1 explicitly"))?
            .parse::<usize>()
            .map_err(|_| invalid("Calibration input_channel must be 0 or 1"))?;
        if channel >= usize::from(source.channels) || channel > 1 {
            return Err(invalid("Calibration input channel is unavailable"));
        }
        let mono = crate::WaveAudio {
            channels: 1,
            sample_rate: source.sample_rate,
            source_encoding: source.source_encoding,
            source_bits: source.source_bits,
            samples: source
                .samples
                .chunks_exact(usize::from(source.channels))
                .map(|frame| frame[channel])
                .collect(),
        };
        let options = crate::CalibrationOptions {
            activation: match spec
                .options
                .get("activation")
                .map(String::as_str)
                .unwrap_or("fast")
            {
                "standard" => crate::ActivationMode::Standard,
                "fast" => crate::ActivationMode::Fast,
                _ => return Err(invalid("Calibration activation must be standard or fast")),
            },
            max_operations: spec
                .options
                .get("max_operations")
                .map(|value| {
                    value
                        .parse::<u64>()
                        .map_err(|_| invalid("NAM max_operations must be a positive integer"))
                })
                .transpose()?
                .unwrap_or(crate::DEFAULT_NAM_RENDER_OPERATIONS),
            timeout: std::time::Duration::from_secs(
                spec.options
                    .get("calibration_timeout_seconds")
                    .map(|value| {
                        value.parse::<u64>().map_err(|_| {
                            invalid("Calibration timeout must be an integer number of seconds")
                        })
                    })
                    .transpose()?
                    .unwrap_or(900),
            ),
        };
        let model_bytes = self.original_bytes(&original.id)?;
        let result = crate::calibrate_nam_to_clo(
            &model_bytes,
            &mono,
            &options,
            &self.root,
            &is_cancelled,
            progress,
        )?;
        if is_cancelled() {
            return Err(crate::cancelled());
        }
        // Retain both exact coefficient stages with immutable sources and recipes.
        let (intermediate_hash, intermediate_path) =
            self.put_blob(result.intermediate.raw(), BlobLocation::Derived)?;
        let profile = result.report.conversion_profile.clone();
        let (trainer_input_hash, _) =
            self.put_blob(&result.trainer_input_f32, BlobLocation::Derived)?;
        let (nam_response_hash, _) =
            self.put_blob(&result.nam_response_f32, BlobLocation::Derived)?;
        let mut properties = BTreeMap::from([
            ("conversion_profile".into(), profile.clone()),
            ("source_retained".into(), "true".into()),
            ("device_compatible".into(), "unverified".into()),
            ("input_sha256".into(), input_hash.clone()),
            ("original_sha256".into(), result.report.model_sha256.clone()),
            (
                "calibration_report_json".into(),
                serde_json::to_string(&result.report).map_err(io)?,
            ),
            (
                "validation".into(),
                "independent_calibration_complete".into(),
            ),
        ]);
        properties.insert("sha256".into(), intermediate_hash.clone());
        properties.insert(
            "declared_bytes".into(),
            result.intermediate.info().declared_bytes.to_string(),
        );
        properties.insert(
            "container_profile".into(),
            result.intermediate.info().container_profile.clone(),
        );
        self.insert(AssetManifest {
            schema_version: 1,
            info: AssetInfo { id: AssetId(format!("sha256:{intermediate_hash}")), kind: AssetKind::SnapTone, name: format!("{} · A128/B2048 intermediate", original.name), path: intermediate_path, bytes: result.intermediate.raw().len() as u64, properties: properties.clone(), warnings: vec!["Independent calibration intermediate retained for diagnosis. Hardware compatibility and original Windows converter equivalence are unverified.".into()] },
            original_sha256: result.report.model_sha256.clone(), original_name: original.name.clone(), derived_sha256: Some(intermediate_hash.clone()), conversion_profile: Some(profile.clone()), options: spec.options.clone(), dependencies: BTreeMap::from([("input_wav".into(), input_hash.clone())]), created_at_unix_ms: now_ms(),
        })?;
        let (hash, path) = self.put_blob(result.container.raw(), BlobLocation::Derived)?;
        properties.insert("sha256".into(), hash.clone());
        properties.insert(
            "declared_bytes".into(),
            result.container.info().declared_bytes.to_string(),
        );
        properties.insert(
            "coefficient_bytes".into(),
            result.container.info().coefficient_bytes.to_string(),
        );
        properties.insert(
            "container_profile".into(),
            result.container.info().container_profile.clone(),
        );
        properties.insert(
            "snaptone_info_json".into(),
            serde_json::to_string(result.container.info()).map_err(io)?,
        );
        let derived = self.insert(AssetManifest {
            schema_version: 1,
            info: AssetInfo { id: AssetId(format!("sha256:{hash}")), kind: AssetKind::SnapTone, name: format!("{} · calibrated A128/B1024 CLO", original.name), path, bytes: result.container.raw().len() as u64, properties, warnings: vec!["Independent MIT NamtoClo calibration profile with pinned Rust NAM inference. The final spectral score describes the local fit; it does not verify pedal sound or original converter equivalence.".into(), "Final VTSI is physically 8840 bytes with 4744 declared bytes, A128/B1024 and zero padding. No upload ticket was created.".into()] },
            original_sha256: result.report.model_sha256, original_name: original.name.clone(), derived_sha256: Some(hash), conversion_profile: Some(profile), options: spec.options.clone(), dependencies: BTreeMap::from([("input_wav".into(), input_hash), ("calibration_intermediate".into(), intermediate_hash), ("trainer_input_f32".into(), trainer_input_hash), ("nam_response_f32".into(), nam_response_hash)]), created_at_unix_ms: now_ms(),
        })?;
        if let Some(path) = &spec.output {
            self.export_bytes(path, &spec.source, result.container.raw())?;
        }
        Ok(LocalAssetResult {
            original,
            derived: Some(derived),
        })
    }
    fn render_nam(
        &mut self,
        spec: &AssetJobSpec,
        original: AssetInfo,
        is_cancelled: impl Fn() -> bool,
        progress: impl FnMut(u64, u64),
    ) -> Result<LocalAssetResult, ServiceError> {
        use crate::{ActivationMode, ChannelMode, NamModel, SampleProcessor};
        for key in spec.options.keys() {
            if !matches!(
                key.as_str(),
                "target_slot"
                    | "input_wav"
                    | "input_channel"
                    | "nam_size"
                    | "resample_input"
                    | "activation"
                    | "max_operations"
            ) {
                return Err(invalid(format!("Unknown NAM rendering option: {key}")));
            }
        }
        let bytes = self.original_bytes(&original.id)?;
        let mut model = NamModel::load(&bytes)?;
        let selected_size = nam_size(&spec.options)?;
        model.set_size(selected_size)?;
        model.set_activation(
            match spec
                .options
                .get("activation")
                .map(String::as_str)
                .unwrap_or("standard")
            {
                "standard" => ActivationMode::Standard,
                "fast" => ActivationMode::Fast,
                _ => return Err(invalid("NAM activation option must be standard or fast")),
            },
        );
        let input_path = spec.options.get("input_wav").filter(|path| !path.is_empty()).ok_or_else(|| invalid("Select an input WAV explicitly for local NAM rendering (input_wav option). The bundled vendor stimulus is not automatically substituted."))?;
        let input_bytes = read_bounded(Path::new(input_path), MAX_SOURCE_BYTES as u64)?;
        let (input_hash, _) = self.put_blob(&input_bytes, BlobLocation::Original)?;
        let input = inspect_wav(&input_bytes)?;
        let channel = match spec.options.get("input_channel").map(String::as_str) {
            Some("average") => ChannelMode::Average,
            Some(channel) => ChannelMode::Select(channel.parse().map_err(|_| {
                invalid("input_channel must be average or a zero-based channel index")
            })?),
            None if input.channels == 1 => ChannelMode::Preserve,
            None => {
                return Err(invalid(
                    "Multi-channel NAM input requires explicit input_channel selection or average",
                ));
            }
        };
        let resample = match spec
            .options
            .get("resample_input")
            .map(String::as_str)
            .unwrap_or("false")
        {
            "true" => true,
            "false" => false,
            _ => return Err(invalid("resample_input must be true or false")),
        };
        let mono = prepare_audio(
            &input,
            &PrepareOptions {
                channel,
                sample_rate: if resample {
                    Some(model.sample_rate())
                } else {
                    None
                },
                ..Default::default()
            },
            &is_cancelled,
        )?;
        let max_operations = match spec.options.get("max_operations") {
            None => crate::DEFAULT_NAM_RENDER_OPERATIONS,
            Some(value) => value
                .parse::<u64>()
                .map_err(|_| invalid("NAM max_operations must be a positive integer"))?,
        };
        let estimated_operations =
            model.validate_render_budget(mono.frames() as u64, max_operations)?;
        let (audio, report) = crate::render_host_stage(&mono, &mut model, &is_cancelled, progress)?;
        let output = crate::write_pcm16_wav(&audio)?;
        if is_cancelled() {
            return Err(crate::cancelled());
        }
        let (hash, path) = self.put_blob(&output, BlobLocation::Derived)?;
        let stats = inspect_wav(&output)?.inspect()?;
        let profile = if spec.options.contains_key("nam_size") {
            "local-nam-core-0b3d3c97-host-wav-size-v1"
        } else {
            "local-nam-core-0b3d3c97-host-wav-v2"
        };
        let mut properties = BTreeMap::from([
            ("nam_size".into(), selected_size.to_string()),
            ("size_selection".into(), model.size_selection().to_string()),
            ("sha256".into(), hash.clone()),
            (
                "original_sha256".into(),
                original.id.0.trim_start_matches("sha256:").into(),
            ),
            ("input_sha256".into(), input_hash.clone()),
            (
                "input_name".into(),
                Path::new(input_path)
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned(),
            ),
            ("conversion_profile".into(), profile.into()),
            (
                "activation".into(),
                spec.options
                    .get("activation")
                    .cloned()
                    .unwrap_or_else(|| "standard".into()),
            ),
            ("network_profile".into(), model.architecture().into()),
            (
                "estimated_operations".into(),
                estimated_operations.to_string(),
            ),
            ("max_operations".into(), max_operations.to_string()),
            (
                "operations_per_sample".into(),
                model.operations_per_sample().to_string(),
            ),
            (
                "state_reset".into(),
                "LSTM exported state; WaveNet zero delay state; no implicit prewarm".into(),
            ),
            ("format".into(), "Stereo PCM16 WAV".into()),
            ("sample_rate".into(), audio.sample_rate.to_string()),
            ("channels".into(), "2".into()),
            ("bits".into(), "16".into()),
            ("frames".into(), report.frames.to_string()),
            ("seconds".into(), format!("{:.6}", stats.seconds)),
            ("peak".into(), format!("{:.8}", stats.peak)),
            ("rms".into(), format!("{:.8}", stats.rms)),
            (
                "host_clamped_samples".into(),
                report.clamped_samples.to_string(),
            ),
            ("host_blocks".into(), report.blocks.to_string()),
            ("device_compatible".into(), "unknown".into()),
            ("source_retained".into(), "true".into()),
            ("validation".into(), "local_render_complete".into()),
        ]);
        properties.insert(
            "waveform_json".into(),
            serde_json::to_string(&stats.waveform).expect("finite waveform"),
        );
        let info = AssetInfo { id:AssetId(format!("sha256:{hash}")),kind:AssetKind::Nam,name:format!("{} · rendered WAV",original.name),path,bytes:output.len()as u64,properties,warnings:vec!["Local NAM inference uses pinned upstream equations and independent numerical fixtures. The selected Slim bin and its actual cost are retained in the conversion recipe. Native warmup, input handling and fast activation equivalence remain unverified.".into(),"Rendered stereo PCM16 WAV reproduces the observed 1024-frame clamp×0.31 host stage. This WAV contains audio, not calibrated coefficients; choose the independent experimental CLO workflow for local calibration. Pedal upload remains unavailable.".into()] };
        let derived = self.insert(AssetManifest {
            schema_version: 1,
            info,
            original_sha256: original.id.0.trim_start_matches("sha256:").into(),
            original_name: original.name.clone(),
            derived_sha256: Some(hash),
            conversion_profile: Some(profile.into()),
            options: spec.options.clone(),
            dependencies: BTreeMap::from([("input_wav".into(), input_hash)]),
            created_at_unix_ms: now_ms(),
        })?;
        if let Some(path) = &spec.output {
            self.export_bytes(path, &spec.source, &output)?;
        }
        Ok(LocalAssetResult {
            original,
            derived: Some(derived),
        })
    }
    fn export_bytes(&self, output: &Path, source: &Path, bytes: &[u8]) -> Result<(), ServiceError> {
        reject_symlink(output)?;
        if output == source || output.starts_with(&self.root) {
            return Err(invalid(
                "Export destination cannot replace the source or immutable store",
            ));
        }
        let temp = output.with_extension(format!("gp200-{}-{}.tmp", std::process::id(), now_ms()));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)
            .map_err(io)?;
        let write = file.write_all(bytes).and_then(|()| file.sync_all());
        drop(file);
        if let Err(error) = write {
            let _ = fs::remove_file(&temp);
            return Err(io(error));
        }
        let publish = fs::hard_link(&temp, output);
        let _ = fs::remove_file(&temp);
        publish.map_err(|error| {
            ServiceError::new(
                ErrorKind::Io,
                format!("Prepared output retained, but export failed: {error}"),
            )
        })
    }
    pub(crate) fn journal(
        &self,
        token: &str,
        spec: &AssetJobSpec,
        state: &str,
        error: Option<&str>,
    ) -> Result<(), ServiceError> {
        let path = self.root.join("jobs").join(format!("{token}.json"));
        if !path.exists()
            && fs::read_dir(self.root.join("jobs"))
                .map_err(io)?
                .take(MAX_ASSETS + 1)
                .count()
                >= MAX_ASSETS
        {
            return Err(invalid(
                "Local job journal reached its 4096-entry limit; archive completed job records before starting more jobs",
            ));
        }
        let record = serde_json::json!({ "schema_version":1, "job":spec, "state":state, "error":error, "updated_at_unix_ms":now_ms(), "pedal_transfer_started":false, "resume_policy":"Explicit restart from retained source; no automatic transfer" });
        self.atomic_file(&path, &serde_json::to_vec_pretty(&record).map_err(io)?)
    }
    fn recover_journals(&self) -> Result<(), ServiceError> {
        let mut count = 0;
        for entry in fs::read_dir(self.root.join("jobs")).map_err(io)? {
            let path = entry.map_err(io)?.path();
            count += 1;
            if count > MAX_ASSETS {
                return Err(invalid("Local job journal exceeds its entry bound"));
            }
            reject_symlink(&path)?;
            if path.extension().is_none_or(|extension| extension != "json") {
                continue;
            }
            let mut value: serde_json::Value =
                serde_json::from_slice(&read_bounded(&path, 256 * 1024)?)
                    .map_err(|error| invalid(format!("Invalid local job journal: {error}")))?;
            if value
                .get("schema_version")
                .and_then(serde_json::Value::as_u64)
                != Some(1)
            {
                return Err(invalid("Unsupported asset job journal schema"));
            }
            if matches!(
                value.get("state").and_then(serde_json::Value::as_str),
                Some("queued" | "validating" | "converting_locally")
            ) {
                value["state"] = "interrupted_local".into();
                value["error"] = "Previous process stopped. Restart local preparation explicitly; no hardware transfer was started.".into();
                self.atomic_file(&path, &serde_json::to_vec_pretty(&value).map_err(io)?)?;
            }
        }
        Ok(())
    }
}
