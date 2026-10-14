//! Reproduce the v2 layout emitted by the calibrated NAM writer without running
//! the expensive external calibrator. These are immutable storage/history tests,
//! not evidence of calibration sound quality or pedal compatibility.
use gp200_assets::{AssetManifest, AssetStore, ConversionRecipe, write_float_wav};
use gp200_domain::{AssetId, AssetInfo, AssetKind};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

const PROFILE: &str = "independent-namtoclo-42c07456-rust-nam-v1";
const FINAL: &[u8] = include_bytes!("../../../tests/fixtures/assets/clo/upstream-final1024.clo");
const INTERMEDIATE: &[u8] =
    include_bytes!("../../../tests/fixtures/assets/clo/upstream-source2048.clo");
const MODEL: &[u8] = include_bytes!("../../../tests/fixtures/assets/upstream-lstm-0.5.4.nam");
static NEXT: AtomicU64 = AtomicU64::new(0);

struct Fixture {
    root: PathBuf,
    final_id: AssetId,
    final_recipe: String,
    intermediate_recipe: String,
    dependencies: BTreeMap<String, Vec<u8>>,
    hashes: BTreeMap<String, String>,
}

fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn recipe(manifest: &AssetManifest) -> ConversionRecipe {
    // The existing on-disk format hashes serialized AssetManifest with its
    // timestamp cleared. Keep that format, rather than asking a new writer to
    // generate the regression fixture or changing the serialized schema.
    let mut identity = manifest.clone();
    identity.created_at_unix_ms = 0;
    ConversionRecipe {
        id: hash(&serde_json::to_vec(&identity).unwrap()),
        manifest: manifest.clone(),
    }
}

fn retained(
    root: &Path,
    original: &str,
    bytes: &[u8],
    derived: bool,
    dependencies: BTreeMap<String, String>,
) -> AssetManifest {
    let digest = hash(bytes);
    let location = if derived { "derived" } else { "originals" };
    AssetManifest {
        schema_version: 1,
        info: AssetInfo {
            id: AssetId(format!("sha256:{digest}")),
            kind: if derived {
                AssetKind::SnapTone
            } else {
                AssetKind::Nam
            },
            name: if derived {
                "synthetic CLO"
            } else {
                "model.nam"
            }
            .into(),
            path: root.join(location).join(&digest),
            bytes: bytes.len() as u64,
            properties: BTreeMap::new(),
            warnings: Vec::new(),
        },
        original_sha256: original.into(),
        original_name: "model.nam".into(),
        derived_sha256: derived.then_some(digest),
        conversion_profile: derived.then(|| PROFILE.into()),
        options: BTreeMap::from([("activation".into(), "standard".into())]),
        dependencies,
        created_at_unix_ms: 123_456,
    }
}

impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "gp200-calibration-history-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        let root = fs::canonicalize(root).unwrap();
        for name in ["originals", "derived", "jobs"] {
            fs::create_dir(root.join(name)).unwrap();
        }
        let stimulus = write_float_wav(&gp200_assets::WaveAudio {
            channels: 1,
            sample_rate: 48_000,
            source_encoding: gp200_assets::WaveEncoding::Float,
            source_bits: 32,
            samples: vec![0.125, -0.5, -0.0, 0.25],
        })
        .unwrap();
        let dependencies: BTreeMap<String, Vec<u8>> = BTreeMap::from([
            ("input_wav".into(), stimulus),
            ("calibration_intermediate".into(), INTERMEDIATE.to_vec()),
            (
                "trainer_input_f32".into(),
                [0.125_f32, -0.5, -0.0, 0.25]
                    .into_iter()
                    .flat_map(f32::to_le_bytes)
                    .collect(),
            ),
            (
                "nam_response_f32".into(),
                [0.225_f32, -0.3, 0.05, -0.0]
                    .into_iter()
                    .flat_map(f32::to_le_bytes)
                    .collect(),
            ),
        ]);
        let hashes: BTreeMap<String, String> = dependencies
            .iter()
            .map(|(name, bytes)| (name.clone(), hash(bytes)))
            .collect();
        fs::write(root.join("originals").join(hash(MODEL)), MODEL).unwrap();
        fs::write(root.join("derived").join(hash(FINAL)), FINAL).unwrap();
        for (name, bytes) in &dependencies {
            let location = if name == "input_wav" {
                "originals"
            } else {
                "derived"
            };
            fs::write(root.join(location).join(&hashes[name]), bytes).unwrap();
        }
        let source = retained(&root, &hash(MODEL), MODEL, false, BTreeMap::new());
        let intermediate = retained(
            &root,
            &hash(MODEL),
            INTERMEDIATE,
            true,
            BTreeMap::from([("input_wav".into(), hashes["input_wav"].clone())]),
        );
        let final_asset = retained(&root, &hash(MODEL), FINAL, true, hashes.clone());
        let intermediate_recipe = recipe(&intermediate);
        let final_recipe = recipe(&final_asset);
        let assets = BTreeMap::from([
            (source.info.id.clone(), source),
            (intermediate.info.id.clone(), intermediate),
            (final_asset.info.id.clone(), final_asset.clone()),
        ]);
        let recipes = BTreeMap::from([
            (intermediate_recipe.id.clone(), intermediate_recipe.clone()),
            (final_recipe.id.clone(), final_recipe.clone()),
        ]);
        fs::write(
            root.join("manifest.json"),
            serde_json::to_vec_pretty(&json!({"version": 2, "assets": assets, "recipes": recipes}))
                .unwrap(),
        )
        .unwrap();
        Self {
            root,
            final_id: final_asset.info.id,
            final_recipe: final_recipe.id,
            intermediate_recipe: intermediate_recipe.id,
            dependencies,
            hashes,
        }
    }

    fn dependency(&self, name: &str) -> PathBuf {
        self.root
            .join(if name == "input_wav" {
                "originals"
            } else {
                "derived"
            })
            .join(&self.hashes[name])
    }

    fn edit_manifest(&self, edit: impl FnOnce(&mut Value)) {
        let path = self.root.join("manifest.json");
        let mut manifest: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        edit(&mut manifest);
        fs::write(path, serde_json::to_vec_pretty(&manifest).unwrap()).unwrap();
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn calibrated_v2_history_reopens_without_rewriting_manifest_or_recipe_ids() {
    let fixture = Fixture::new();
    let before = fs::read(fixture.root.join("manifest.json")).unwrap();
    for _ in 0..2 {
        let store = AssetStore::open(&fixture.root).unwrap();
        assert_eq!(store.inventory().len(), 3);
        assert_eq!(store.conversion_recipes().len(), 2);
        assert_eq!(store.content_bytes(&fixture.final_id).unwrap(), FINAL);
        assert_eq!(store.original_bytes(&fixture.final_id).unwrap(), MODEL);
        assert_eq!(
            store.recipe_original_bytes(&fixture.final_recipe).unwrap(),
            MODEL
        );
        for (name, expected) in &fixture.dependencies {
            assert_eq!(
                store.dependency_bytes(&fixture.final_id, name).unwrap(),
                *expected
            );
            assert_eq!(
                store
                    .recipe_dependency_bytes(&fixture.final_recipe, name)
                    .unwrap(),
                *expected
            );
        }
        assert_eq!(
            store
                .recipe_dependency_bytes(&fixture.intermediate_recipe, "input_wav")
                .unwrap(),
            fixture.dependencies["input_wav"]
        );
    }
    assert_eq!(
        fs::read(fixture.root.join("manifest.json")).unwrap(),
        before
    );
}

#[test]
fn v1_history_migration_preserves_existing_asset_identity_and_infers_typed_roles() {
    let fixture = Fixture::new();
    fixture.edit_manifest(|manifest| {
        manifest["version"] = json!(1);
        manifest.as_object_mut().unwrap().remove("recipes");
    });
    let store = AssetStore::open(&fixture.root).unwrap();
    assert!(
        store
            .conversion_recipes()
            .contains_key(&fixture.final_recipe)
    );
    assert!(
        store
            .conversion_recipes()
            .contains_key(&fixture.intermediate_recipe)
    );
    assert_eq!(
        store
            .recipe_dependency_bytes(&fixture.final_recipe, "trainer_input_f32")
            .unwrap(),
        fixture.dependencies["trainer_input_f32"]
    );
    drop(store);
    let persisted: Value =
        serde_json::from_slice(&fs::read(fixture.root.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(persisted["version"], 2);
    assert_eq!(persisted["recipes"].as_object().unwrap().len(), 2);
    AssetStore::open(&fixture.root).unwrap();
}

#[test]
fn missing_derived_dependency_cannot_fall_back_to_valid_original_copy() {
    for name in [
        "trainer_input_f32",
        "nam_response_f32",
        "calibration_intermediate",
    ] {
        let fixture = Fixture::new();
        fs::rename(
            fixture.dependency(name),
            fixture.root.join("originals").join(&fixture.hashes[name]),
        )
        .unwrap();
        assert!(
            AssetStore::open(&fixture.root).is_err(),
            "accepted {name} in originals"
        );
    }
}

#[test]
fn dependency_roles_require_the_exact_known_conversion_profile() {
    let fixture = Fixture::new();
    fixture.edit_manifest(|manifest| {
        // Remove recipes to exercise the v1 asset-dependency path directly.
        manifest["version"] = json!(1);
        manifest.as_object_mut().unwrap().remove("recipes");
        manifest["assets"][&fixture.final_id.0]["conversion_profile"] =
            json!("independent-namtoclo-unknown-v9");
    });
    assert!(AssetStore::open(&fixture.root).is_err());
}

#[test]
fn historical_arbitrary_original_dependency_names_keep_their_existing_location() {
    let fixture = Fixture::new();
    fixture.edit_manifest(|manifest| {
        manifest["version"] = json!(1);
        manifest.as_object_mut().unwrap().remove("recipes");
        let final_asset = &mut manifest["assets"][&fixture.final_id.0];
        final_asset["conversion_profile"] = json!("historical-local-profile-v0");
        // A known calibration role name in an unrelated historical profile
        // retains the pre-existing Originals meaning instead of guessing.
        final_asset["dependencies"] = json!({
            "legacy_sidecar": fixture.hashes["input_wav"],
            "trainer_input_f32": fixture.hashes["input_wav"],
        });
    });
    let store = AssetStore::open(&fixture.root).unwrap();
    for name in ["legacy_sidecar", "trainer_input_f32"] {
        assert_eq!(
            store.dependency_bytes(&fixture.final_id, name).unwrap(),
            fixture.dependencies["input_wav"]
        );
        let recipe = store
            .conversion_recipes()
            .values()
            .find(|recipe| recipe.manifest.info.id == fixture.final_id)
            .unwrap();
        assert_eq!(
            store.recipe_dependency_bytes(&recipe.id, name).unwrap(),
            fixture.dependencies["input_wav"]
        );
    }
}

#[test]
fn startup_remains_metadata_only_and_defers_dependency_hashes_to_reads() {
    let fixture = Fixture::new();
    fs::write(
        fixture.dependency("trainer_input_f32"),
        b"same-role corruption",
    )
    .unwrap();
    // Opening an otherwise valid store does not scan large float responses.
    // The actual use must verify the retained SHA before returning any bytes.
    let store = AssetStore::open(&fixture.root).unwrap();
    assert!(
        store
            .dependency_bytes(&fixture.final_id, "trainer_input_f32")
            .unwrap_err()
            .message
            .contains("hash mismatch")
    );
    assert!(
        store
            .recipe_dependency_bytes(&fixture.final_recipe, "trainer_input_f32")
            .unwrap_err()
            .message
            .contains("hash mismatch")
    );
}

#[test]
fn healthy_duplicate_location_is_unambiguous_but_cannot_mask_expected_blob_corruption() {
    let fixture = Fixture::new();
    let name = "trainer_input_f32";
    fs::write(
        fixture.root.join("originals").join(&fixture.hashes[name]),
        &fixture.dependencies[name],
    )
    .unwrap();
    let store = AssetStore::open(&fixture.root).unwrap();
    assert_eq!(
        store.dependency_bytes(&fixture.final_id, name).unwrap(),
        fixture.dependencies[name]
    );
    fs::write(fixture.dependency(name), b"corrupt derived content").unwrap();
    for error in [
        store.dependency_bytes(&fixture.final_id, name).unwrap_err(),
        store
            .recipe_dependency_bytes(&fixture.final_recipe, name)
            .unwrap_err(),
    ] {
        assert!(error.message.contains("hash mismatch"), "{}", error.message);
    }
}

#[test]
fn missing_dependency_after_open_is_rejected_by_both_read_apis() {
    let fixture = Fixture::new();
    let store = AssetStore::open(&fixture.root).unwrap();
    fs::remove_file(fixture.dependency("nam_response_f32")).unwrap();
    assert!(
        store
            .dependency_bytes(&fixture.final_id, "nam_response_f32")
            .is_err()
    );
    assert!(
        store
            .recipe_dependency_bytes(&fixture.final_recipe, "nam_response_f32")
            .is_err()
    );
}

#[cfg(unix)]
#[test]
fn symlink_dependency_is_rejected_at_startup_and_after_open() {
    for before_open in [true, false] {
        let fixture = Fixture::new();
        let store = (!before_open).then(|| AssetStore::open(&fixture.root).unwrap());
        let dependency = fixture.dependency("trainer_input_f32");
        let other = fixture.root.join("matching-but-untrusted");
        fs::write(&other, &fixture.dependencies["trainer_input_f32"]).unwrap();
        fs::remove_file(&dependency).unwrap();
        std::os::unix::fs::symlink(other, dependency).unwrap();
        if let Some(store) = store {
            assert!(
                store
                    .dependency_bytes(&fixture.final_id, "trainer_input_f32")
                    .is_err()
            );
            assert!(
                store
                    .recipe_dependency_bytes(&fixture.final_recipe, "trainer_input_f32")
                    .is_err()
            );
        } else {
            assert!(AssetStore::open(&fixture.root).is_err());
        }
    }
}

#[test]
fn startup_dependency_validation_keeps_regular_file_and_size_bounds() {
    for invalid_type in ["directory", "empty", "oversized"] {
        let fixture = Fixture::new();
        let path = fixture.dependency("trainer_input_f32");
        fs::remove_file(&path).unwrap();
        match invalid_type {
            "directory" => fs::create_dir(&path).unwrap(),
            "empty" => fs::write(&path, b"").unwrap(),
            "oversized" => fs::File::create(&path)
                .unwrap()
                .set_len(gp200_assets::MAX_SOURCE_BYTES as u64 + 1)
                .unwrap(),
            _ => unreachable!(),
        }
        assert!(
            AssetStore::open(&fixture.root).is_err(),
            "accepted {invalid_type}"
        );
    }
}

#[test]
fn changing_recipe_source_options_or_dependency_is_still_rejected() {
    for field in ["original_sha256", "options", "dependencies"] {
        let fixture = Fixture::new();
        fixture.edit_manifest(|manifest| {
            let recipe = &mut manifest["recipes"][&fixture.final_recipe]["manifest"];
            match field {
                "original_sha256" => recipe[field] = json!(hash(b"another source")),
                "options" => recipe[field]["activation"] = json!("fast"),
                "dependencies" => {
                    recipe[field]["trainer_input_f32"] = json!(hash(b"other samples"))
                }
                _ => unreachable!(),
            }
        });
        assert!(
            AssetStore::open(&fixture.root).is_err(),
            "accepted altered {field}"
        );
    }
}
