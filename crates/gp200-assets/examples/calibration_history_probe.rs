//! Root-operated, bounded existing-store verifier. No calibrator, MIDI actor,
//! network client, or job is constructed. Run against a private namespace mount
//! preserving the prior writer's absolute canonical paths and recipe IDs.
use gp200_assets::AssetStore;
use gp200_domain::AssetId;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, fs, path::PathBuf};

const PROFILE: &str = "independent-namtoclo-42c07456-rust-nam-v1";

fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn verify() -> Result<Value, Box<dyn std::error::Error>> {
    let mut arguments = std::env::args().skip(1);
    let mut options = BTreeMap::new();
    while let Some(key) = arguments.next() {
        if !matches!(
            key.as_str(),
            "--store"
                | "--expected-manifest-sha256"
                | "--expected-final-sha256"
                | "--expected-final-recipe"
                | "--expected-intermediate-recipe"
        ) {
            return Err(format!("Unknown verifier option: {key}").into());
        }
        let value = arguments
            .next()
            .ok_or("Each verifier option needs a value")?;
        if options.insert(key, value).is_some() {
            return Err("Duplicate verifier option".into());
        }
    }
    let required = |name: &str| -> Result<&str, Box<dyn std::error::Error>> {
        options
            .get(name)
            .map(String::as_str)
            .ok_or_else(|| format!("Missing {name}").into())
    };
    let root = PathBuf::from(required("--store")?);
    if !root.is_absolute() {
        return Err("Store must be an absolute private mounted path".into());
    }
    let expected_manifest = required("--expected-manifest-sha256")?;
    let expected_final = required("--expected-final-sha256")?;
    let expected_final_recipe = required("--expected-final-recipe")?;
    let expected_intermediate_recipe = required("--expected-intermediate-recipe")?;
    for digest in [
        expected_manifest,
        expected_final,
        expected_final_recipe,
        expected_intermediate_recipe,
    ] {
        if digest.len() != 64
            || !digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err("Expected identity must be lowercase SHA-256 hex".into());
        }
    }
    let manifest_path = root.join("manifest.json");
    let metadata = fs::symlink_metadata(&manifest_path)?;
    if !metadata.file_type().is_file() || metadata.len() > 16 * 1024 * 1024 {
        return Err("Existing manifest must be a bounded regular file".into());
    }
    let before = fs::read(&manifest_path)?;
    if hash(&before) != expected_manifest {
        return Err("Existing manifest does not match the expected frozen identity".into());
    }
    let manifest: Value = serde_json::from_slice(&before)?;
    if manifest["version"] != 2 {
        return Err("Verifier requires v2; it must not migrate a manifest".into());
    }
    if fs::read_dir(root.join("jobs"))?.next().is_some() {
        return Err("Verifier requires no pending journals or jobs".into());
    }
    let store = AssetStore::open(&root)?;
    let final_id = AssetId(format!("sha256:{expected_final}"));
    let final_manifest = store
        .manifests()
        .get(&final_id)
        .ok_or("Expected final asset is absent")?;
    if final_manifest.derived_sha256.as_deref() != Some(expected_final)
        || final_manifest.conversion_profile.as_deref() != Some(PROFILE)
    {
        return Err("Expected final asset is not the exact calibrated profile".into());
    }
    let final_recipe = store
        .conversion_recipes()
        .get(expected_final_recipe)
        .ok_or("Expected final recipe is absent")?;
    let intermediate_recipe = store
        .conversion_recipes()
        .get(expected_intermediate_recipe)
        .ok_or("Expected intermediate recipe is absent")?;
    if final_recipe.manifest.info.id != final_id
        || final_recipe.manifest.conversion_profile.as_deref() != Some(PROFILE)
        || intermediate_recipe.manifest.conversion_profile.as_deref() != Some(PROFILE)
        || final_recipe
            .manifest
            .dependencies
            .get("calibration_intermediate")
            != intermediate_recipe.manifest.derived_sha256.as_ref()
    {
        return Err("Expected stage identities or calibration relationship differ".into());
    }
    let mut assets = BTreeMap::new();
    let mut asset_dependency_reads = 0;
    for (id, manifest) in store.manifests() {
        let bytes = store.content_bytes(id)?;
        let expected = manifest
            .derived_sha256
            .as_ref()
            .unwrap_or(&manifest.original_sha256);
        if hash(&bytes) != *expected || bytes.len() as u64 != manifest.info.bytes {
            return Err("Asset content changed or its recorded length differs".into());
        }
        let original = store.original_bytes(id)?;
        if hash(&original) != manifest.original_sha256 {
            return Err("Original source changed".into());
        }
        let mut dependencies = BTreeMap::new();
        for (name, expected) in &manifest.dependencies {
            let bytes = store.dependency_bytes(id, name)?;
            if hash(&bytes) != *expected {
                return Err(format!("Asset dependency changed: {name}").into());
            }
            asset_dependency_reads += 1;
            dependencies.insert(name, json!({"sha256": expected, "bytes": bytes.len()}));
        }
        assets.insert(&id.0, json!({"content_sha256": expected, "bytes": bytes.len(), "original_sha256": manifest.original_sha256, "dependencies": dependencies}));
    }
    let mut recipes = BTreeMap::new();
    let mut recipe_dependency_reads = 0;
    for (id, recipe) in store.conversion_recipes() {
        let original = store.recipe_original_bytes(id)?;
        if hash(&original) != recipe.manifest.original_sha256 {
            return Err("Recipe original source changed".into());
        }
        let mut dependencies = BTreeMap::new();
        for (name, expected) in &recipe.manifest.dependencies {
            let bytes = store.recipe_dependency_bytes(id, name)?;
            if hash(&bytes) != *expected {
                return Err(format!("Recipe dependency changed: {name}").into());
            }
            recipe_dependency_reads += 1;
            dependencies.insert(name, json!({"sha256": expected, "bytes": bytes.len()}));
        }
        recipes.insert(id, json!({"output_sha256": recipe.manifest.derived_sha256, "original_sha256": recipe.manifest.original_sha256, "dependencies": dependencies}));
    }
    let report = json!({"schema": 1, "store": root, "manifest_sha256": expected_manifest,
        "expected_final_sha256": expected_final, "expected_final_recipe": expected_final_recipe,
        "expected_intermediate_recipe": expected_intermediate_recipe,
        "assets": assets, "recipes": recipes, "asset_dependency_reads": asset_dependency_reads,
        "recipe_dependency_reads": recipe_dependency_reads, "manifest_bytes_unchanged": true,
        "device_access": false, "calibration_rerun": false, "accepted": true});
    drop(store);
    if fs::read(&manifest_path)? != before {
        return Err("Opening or reading rewrote the manifest or immutable identities".into());
    }
    Ok(report)
}

fn main() {
    match verify() {
        Ok(report) => println!(
            "{}",
            serde_json::to_string_pretty(&report).expect("JSON report")
        ),
        Err(error) => {
            eprintln!("Calibration history validation failed: {error}");
            std::process::exit(2);
        }
    }
}
