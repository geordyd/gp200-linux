use gp200_assets::*;
use gp200_domain::*;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, Instant},
};
static NEXT: AtomicU64 = AtomicU64::new(0);
struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "gp200-nam-size-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn source(&self, name: &str, bytes: &[u8]) -> PathBuf {
        let path = self.0.join(name);
        fs::write(&path, bytes).unwrap();
        path
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn fixture(name: &str) -> Vec<u8> {
    fs::read(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/assets")
            .join(name),
    )
    .unwrap()
}
fn wave(samples: Vec<f32>) -> Vec<u8> {
    write_float_wav(&WaveAudio {
        sample_rate: 48_000,
        channels: 1,
        source_encoding: WaveEncoding::Float,
        source_bits: 32,
        samples,
    })
    .unwrap()
}
fn spec(source: PathBuf, input: &std::path::Path, size: Option<f64>) -> AssetJobSpec {
    let mut options = BTreeMap::from([
        ("input_wav".into(), input.to_string_lossy().into_owned()),
        ("max_operations".into(), "1000000000000".into()),
    ]);
    if let Some(size) = size {
        options.insert("nam_size".into(), size.to_string());
    }
    AssetJobSpec {
        id: JobId(1),
        kind: AssetKind::Nam,
        source,
        output: None,
        options,
    }
}
#[test]
fn selected_worker_output_matches_independent_slim_golden_and_exact_cost() {
    let scratch = Scratch::new();
    let source_bytes = fixture("upstream-slimmable_wavenet.nam");
    let reference: Value =
        serde_json::from_slice(&fixture("upstream-slimmable_wavenet-kernel-reference.json"))
            .unwrap();
    let input: Vec<f32> = reference["input"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_f64().unwrap() as f32)
        .collect();
    let source = scratch.source("slim.nam", &source_bytes);
    let input_path = scratch.source("input.wav", &wave(input));
    let mut store = AssetStore::open(scratch.0.join("store")).unwrap();
    for size in [0.0, 1.0 / 3.0, 2.0 / 3.0] {
        let request = spec(source.clone(), &input_path, Some(size));
        let output = store.prepare(&request, || false).unwrap().derived.unwrap();
        let mut model = NamModel::load(&source_bytes).unwrap();
        model.set_size(size).unwrap();
        assert_eq!(
            output.properties["operations_per_sample"],
            model.operations_per_sample().to_string()
        );
        assert_eq!(
            output.properties["size_selection"],
            model.size_selection().to_string()
        );
        assert_eq!(
            output.properties["estimated_operations"],
            model.estimated_operations(2057).unwrap().to_string()
        );
        assert_eq!(
            output.properties["conversion_profile"],
            "local-nam-core-0b3d3c97-host-wav-size-v1"
        );
        let audio = inspect_wav(&store.content_bytes(&output.id).unwrap()).unwrap();
        let expected = reference["sizes"]
            .as_object()
            .unwrap()
            .iter()
            .find(|(key, _)| {
                key.parse::<f64>()
                    .is_ok_and(|number| (number - size).abs() < 1e-12)
            })
            .unwrap()
            .1["standard"]
            .as_array()
            .unwrap();
        assert_eq!(audio.frames(), expected.len());
        for (frame, expected) in audio.samples.as_chunks::<2>().0.iter().zip(expected) {
            let expected = (expected.as_f64().unwrap() as f32).clamp(-1.0, 1.0) * 0.31;
            // Independent float kernel tolerance plus one PCM16 quantization step.
            assert!((frame[0] - expected).abs() <= 1.0 / 32768.0 + 1e-6 + 2e-5 * expected.abs());
            assert_eq!(frame[0].to_bits(), frame[1].to_bits());
        }
        let recipe = &store.conversion_recipes()[&output.properties["conversion_recipe_id"]];
        assert_eq!(recipe.manifest.options["nam_size"], size.to_string());
        assert_eq!(
            store.recipe_original_bytes(&recipe.id).unwrap(),
            source_bytes
        );
        assert_eq!(
            store
                .recipe_dependency_bytes(&recipe.id, "input_wav")
                .unwrap(),
            fs::read(&input_path).unwrap()
        );
    }
    let full = store
        .prepare(&spec(source, &input_path, None), || false)
        .unwrap()
        .derived
        .unwrap();
    assert_eq!(full.properties["nam_size"], "1");
    assert_eq!(
        full.properties["conversion_profile"],
        "local-nam-core-0b3d3c97-host-wav-v2"
    );
}
#[test]
fn all_reported_bin_midpoints_select_the_reported_channels_or_child() {
    for name in [
        "upstream-slimmable_wavenet.nam",
        "slim-multi-array.nam",
        "slim-mixed-fixed-0.nam",
        "upstream-slimmable_container.nam",
    ] {
        let source = fixture(name);
        let info = inspect_nam(&source).unwrap();
        let plan = info.size_plan.unwrap();
        let mut previous = None;
        for (size, selection) in plan.choices() {
            assert!(previous.is_none_or(|previous| size > previous));
            previous = Some(size);
            let selected = inspect_nam_with_size(&source, size).unwrap();
            assert!(
                selected.supported_inference,
                "{name} {size}: {}",
                selected.reason
            );
            assert_eq!(selected.size_selection.as_ref(), Some(&selection));
            let mut model = NamModel::load(&source).unwrap();
            model.set_size(size).unwrap();
            assert_eq!(
                selected.operations_per_sample,
                Some(model.operations_per_sample())
            );
        }
    }
    assert!(
        NamSizePlan::Children { thresholds: vec![] }
            .choices()
            .is_empty()
    );
    assert!(
        NamSizePlan::Channels {
            allowed: vec![vec![1]],
            full: vec![]
        }
        .choices()
        .is_empty()
    );
}
#[test]
fn size_preflight_budget_uses_selected_kernel_and_rejects_invalid_or_fixed_sizes() {
    let scratch = Scratch::new();
    let source_bytes = fixture("upstream-slimmable_wavenet.nam");
    let source = scratch.source("slim.nam", &source_bytes);
    let input = scratch.source("input.wav", &wave(vec![0.1; 1024]));
    let mut model = NamModel::load(&source_bytes).unwrap();
    model.set_size(0.0).unwrap();
    let cost = model.estimated_operations(1024).unwrap();
    let mut store = AssetStore::open(scratch.0.join("store")).unwrap();
    let mut request = spec(source, &input, Some(0.0));
    request
        .options
        .insert("max_operations".into(), cost.to_string());
    assert!(store.prepare(&request, || false).is_ok());
    request
        .options
        .insert("max_operations".into(), (cost - 1).to_string());
    assert!(store.prepare(&request, || false).is_err());
    for size in ["NaN", "inf", "-0.01", "1.01", "narrow"] {
        request.options.insert("nam_size".into(), size.into());
        assert!(store.prepare(&request, || false).is_err());
    }
    request.source = scratch.source("fixed.nam", &fixture("linear-one.nam"));
    request
        .options
        .insert("max_operations".into(), "1000000000".into());
    request.options.insert("nam_size".into(), "0.5".into());
    assert!(store.prepare(&request, || false).is_err());
    request.options.insert("nam_size".into(), "1".into());
    assert!(store.prepare(&request, || false).is_ok());
    request.options.insert("output_format".into(), "clo".into());
    assert!(
        store
            .prepare(&request, || false)
            .unwrap_err()
            .message
            .contains("Unknown NAM calibration option: nam_size")
    );
}
#[test]
fn distinct_sources_and_options_keep_two_recipes_for_one_immutable_output_after_reopen() {
    let scratch = Scratch::new();
    let mut model: Value = serde_json::from_slice(&fixture("linear-one.nam")).unwrap();
    for weight in model["weights"].as_array_mut().unwrap() {
        *weight = json!(0.0);
    }
    model["metadata"] = json!({"name":"first source"});
    let first = serde_json::to_vec(&model).unwrap();
    model["metadata"] = json!({"name":"second source"});
    let second = serde_json::to_vec(&model).unwrap();
    let input = scratch.source("input.wav", &wave(vec![0.25; 33]));
    let first_path = scratch.source("first.nam", &first);
    let second_path = scratch.source("second.nam", &second);
    let root = scratch.0.join("store");
    let mut store = AssetStore::open(&root).unwrap();
    let mut request = spec(first_path, &input, None);
    request
        .options
        .insert("activation".into(), "standard".into());
    let a = store.prepare(&request, || false).unwrap().derived.unwrap();
    let content = store.content_bytes(&a.id).unwrap();
    request.source = second_path;
    request.options.insert("activation".into(), "fast".into());
    let b = store.prepare(&request, || false).unwrap().derived.unwrap();
    assert_eq!(a.id, b.id);
    assert_ne!(
        a.properties["conversion_recipe_id"],
        b.properties["conversion_recipe_id"]
    );
    assert_eq!(b.properties["activation"], "fast");
    assert_eq!(store.content_bytes(&a.id).unwrap(), content);
    assert_eq!(store.conversion_recipes().len(), 2);
    assert_eq!(store.original_bytes(&a.id).unwrap(), first);
    assert_eq!(
        store
            .recipe_original_bytes(&b.properties["conversion_recipe_id"])
            .unwrap(),
        second
    );
    assert_eq!(
        store
            .prepare(&request, || false)
            .unwrap()
            .derived
            .unwrap()
            .properties["conversion_recipe_id"],
        b.properties["conversion_recipe_id"]
    );
    assert_eq!(store.conversion_recipes().len(), 2);
    drop(store);
    let reopened = AssetStore::open(&root).unwrap();
    assert_eq!(reopened.conversion_recipes().len(), 2);
    let output = reopened
        .inventory()
        .into_iter()
        .find(|item| item.id == a.id)
        .unwrap();
    assert_eq!(output.properties["conversion_recipe_count"], "2");
    let history: Vec<Value> =
        serde_json::from_str(&output.properties["conversion_history_json"]).unwrap();
    assert_eq!(history.len(), 2);
    assert!(
        history
            .iter()
            .any(|item| item["options"]["activation"] == "standard")
    );
    assert!(
        history
            .iter()
            .any(|item| item["options"]["activation"] == "fast")
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&root).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(root.join("manifest.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(&output.path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
#[test]
fn legacy_manifest_migration_keeps_first_recipe_and_corrupt_history_is_rejected() {
    let scratch = Scratch::new();
    let input = scratch.source("input.wav", &wave(vec![0.1; 16]));
    let source = scratch.source("linear.nam", &fixture("linear-one.nam"));
    let root = scratch.0.join("store");
    let mut store = AssetStore::open(&root).unwrap();
    store
        .prepare(&spec(source, &input, None), || false)
        .unwrap();
    drop(store);
    let manifest_path = root.join("manifest.json");
    let mut manifest: Value = serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
    manifest["version"] = json!(1);
    manifest.as_object_mut().unwrap().remove("recipes");
    fs::write(&manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();
    let store = AssetStore::open(&root).unwrap();
    assert_eq!(store.conversion_recipes().len(), 1);
    assert_eq!(
        serde_json::from_slice::<Value>(&fs::read(&manifest_path).unwrap()).unwrap()["version"],
        2
    );
    drop(store);
    let mut manifest: Value = serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
    let recipe = manifest["recipes"]
        .as_object_mut()
        .unwrap()
        .values_mut()
        .next()
        .unwrap();
    recipe["manifest"]["options"]["activation"] = json!("altered");
    fs::write(&manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();
    assert!(
        AssetStore::open(&root)
            .err()
            .unwrap()
            .message
            .contains("conversion history")
    );
}
fn terminal(worker: &AssetWorker, job: JobId) -> AssetInfo {
    let until = Instant::now() + Duration::from_secs(10);
    while Instant::now() < until {
        if let Some(event) = worker.try_next_event() {
            match event {
                AssetEvent::Inspected { job: id, info }
                | AssetEvent::Prepared { job: id, info }
                    if id == job =>
                {
                    return info;
                }
                AssetEvent::Failed { job: id, error } if id == job => panic!("{}", error.message),
                AssetEvent::DeviceJobPrepared(_) => panic!("local job generated a device ticket"),
                _ => {}
            }
        } else {
            thread::sleep(Duration::from_millis(1));
        }
    }
    panic!("worker timeout")
}
#[test]
fn actual_worker_inspection_render_and_restart_keep_selected_size_recipe() {
    let scratch = Scratch::new();
    let input = scratch.source("input.wav", &wave(vec![0.1; 1051]));
    let source = scratch.source("slim.nam", &fixture("upstream-slimmable_wavenet.nam"));
    let root = scratch.0.join("store");
    let worker = AssetWorker::open(&root, std::sync::Arc::new(|| {})).unwrap();
    let request = spec(source, &input, Some(0.0));
    worker
        .try_submit(AssetCommand::Inspect(request.clone()))
        .unwrap();
    let inspected = terminal(&worker, JobId(1));
    assert_eq!(inspected.properties["nam_size"], "0");
    assert_eq!(inspected.properties["size_selection"], "Channels 1");
    worker.try_submit(AssetCommand::Convert(request)).unwrap();
    let output = terminal(&worker, JobId(1));
    assert_eq!(output.properties["nam_size"], "0");
    assert_eq!(output.properties["conversion_recipe_count"], "1");
    drop(worker);
    let reopened = AssetStore::open(&root).unwrap();
    let recipe = &reopened.conversion_recipes()[&output.properties["conversion_recipe_id"]];
    assert_eq!(
        recipe.manifest.info.properties["size_selection"],
        "Channels 1"
    );
    assert_eq!(
        inspect_wav(&reopened.content_bytes(&output.id).unwrap())
            .unwrap()
            .frames(),
        1051
    );
}
#[test]
fn cancelled_render_does_not_publish_a_conversion_recipe() {
    let scratch = Scratch::new();
    let input = scratch.source("input.wav", &wave(vec![0.1; 4096]));
    let source = scratch.source("slim.nam", &fixture("upstream-slimmable_wavenet.nam"));
    let mut store = AssetStore::open(scratch.0.join("store")).unwrap();
    let checks = std::cell::Cell::new(0);
    let error = store
        .prepare(&spec(source, &input, Some(0.0)), || {
            checks.set(checks.get() + 1);
            checks.get() >= 5
        })
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::Cancelled);
    assert!(store.conversion_recipes().is_empty());
    assert!(
        store
            .inventory()
            .iter()
            .all(|info| !info.properties.contains_key("conversion_recipe_count"))
    );
}
#[cfg(unix)]
#[test]
fn failed_atomic_history_commit_rolls_back_and_does_not_follow_manifest_symlink() {
    let scratch = Scratch::new();
    let input = scratch.source("input.wav", &wave(vec![0.1; 16]));
    let source = scratch.source("linear.nam", &fixture("linear-one.nam"));
    let root = scratch.0.join("store");
    let mut store = AssetStore::open(&root).unwrap();
    let request = spec(source, &input, None);
    store.inspect(&request).unwrap();
    let before = fs::read(root.join("manifest.json")).unwrap();
    fs::remove_file(root.join("manifest.json")).unwrap();
    let external = scratch.source("external", b"preserved");
    std::os::unix::fs::symlink(&external, root.join("manifest.json")).unwrap();
    assert!(store.prepare(&request, || false).is_err());
    assert!(store.conversion_recipes().is_empty());
    assert_eq!(fs::read(external).unwrap(), b"preserved");
    assert!(fs::read_dir(&root).unwrap().all(|entry| {
        !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .ends_with(".tmp")
    }));
    fs::remove_file(root.join("manifest.json")).unwrap();
    fs::write(root.join("manifest.json"), before).unwrap();
    drop(store);
    assert!(
        AssetStore::open(&root)
            .unwrap()
            .conversion_recipes()
            .is_empty()
    );
}
