//! Root-operated production acceptance. Dry by default; this process owns the
//! workspace hardware lock. The actor remains the sole source of write authority.
//! Only factory-only inactive slots 254/255, current source23, and a known changed
//! latch of one are accepted. No firmware, asset, clear, reset, or global writes.
use gp200_app::{AppController, BufferPersistence, ComparisonSlot, device_document_id};
use gp200_catalog::Catalog;
use gp200_domain::*;
use gp200_library::*;
use gp200_protocol::{RawTransfer, full_stored_write_request};
use gp200_transport::{MidiDeviceService, TransportConfig};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

const SOURCE: u16 = 23;
const NEGATIVE_RECEIPT_SHA: &str =
    "544b29cd8e8213e9ae12c4090532503ce09528f145d49fe5608090b06bfaa534";
const TARGETS: [u16; 2] = [254, 255];
const SOURCE_MODELS: [u32; 11] = [
    0x0a,
    0x0500_000a,
    0x0300_0026,
    0x0700_0015,
    0x1b,
    0x0a00_0010,
    0x0100_003c,
    0x0400_0015,
    0x0b00_000b,
    0x0c00_0002,
    0x0600_0003,
];
fn slot(n: u16) -> SlotId {
    SlotId::new(n).expect("fixed slot")
}
fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}
fn require(ok: bool, message: &str) -> Result<(), String> {
    if ok { Ok(()) } else { Err(message.into()) }
}
fn write_new(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(err)?;
    file.write_all(bytes).map_err(err)?;
    file.sync_all().map_err(err)?;
    File::open(path.parent().ok_or("Missing parent")?)
        .map_err(err)?
        .sync_all()
        .map_err(err)
}
fn json_new(path: &Path, value: &impl serde::Serialize) -> Result<(), String> {
    write_new(path, &serde_json::to_vec_pretty(value).map_err(err)?)
}
fn fresh_directory(path: &Path) -> Result<(), String> {
    require(path.is_absolute(), "Evidence directory must be absolute")?;
    for ancestor in path.ancestors().skip(1) {
        let metadata = fs::symlink_metadata(ancestor).map_err(err)?;
        require(
            metadata.is_dir() && !metadata.file_type().is_symlink(),
            "Evidence ancestors must be real directories",
        )?;
    }
    fs::DirBuilder::new()
        .mode(0o700)
        .create(path)
        .map_err(err)?;
    File::open(path.parent().ok_or("Missing parent")?)
        .map_err(err)?
        .sync_all()
        .map_err(err)
}
fn raw_hash(raw: &RawTransfer) -> ContentHash {
    let mut bytes = vec![raw.kind()];
    bytes.extend_from_slice(raw.payload());
    ContentHash::of(&bytes)
}
fn equal(a: &PresetSnapshot, b: &PresetSnapshot) -> bool {
    a.context == b.context && stored_readback_equal(&a.raw, &b.raw)
}
fn as_stored(
    source: &PresetSnapshot,
    target: SlotId,
    revision: Revision,
) -> Result<Arc<PresetSnapshot>, String> {
    let wire = full_stored_write_request(target.get(), &source.raw).map_err(err)?;
    let mut records = vec![wire.records()[0].clone()];
    records.extend_from_slice(&wire.records()[2..]);
    Ok(Arc::new(PresetSnapshot::from_raw(
        Arc::new(RawTransfer::from_records(source.raw.kind(), &records).map_err(err)?),
        source.source.clone(),
        Some(PresetTarget::Stored(target)),
        revision,
    )))
}
fn as_current(source: &PresetSnapshot, target: SlotId) -> Result<Arc<PresetSnapshot>, String> {
    let stored = as_stored(source, target, source.revision)?;
    let mut records = stored.raw.records().to_vec();
    records[0].body[..2].copy_from_slice(&0_u16.to_le_bytes());
    Ok(Arc::new(PresetSnapshot::from_raw(
        Arc::new(RawTransfer::from_records(source.raw.kind(), &records).map_err(err)?),
        source.source.clone(),
        Some(PresetTarget::EditBuffer),
        source.revision,
    )))
}
fn factory_only(snapshot: &PresetSnapshot, catalog: &Catalog) -> Result<(), String> {
    // Re-derive refs from raw; a mutable/deserialized projection cannot hide a user asset.
    let checked = PresetSnapshot::from_raw(
        snapshot.raw.clone(),
        snapshot.source.clone(),
        snapshot.target,
        snapshot.revision,
    );
    require(
        checked.view.modules.len() == 11 && checked.asset_refs.len() == 11,
        "Complete factory model inventory is required",
    )?;
    require(
        checked
            .asset_refs
            .iter()
            .all(|r| matches!(r, AssetRef::Factory { .. })),
        "A user/unresolved asset prevents this empty-proof acceptance",
    )?;
    require(
        checked
            .view
            .modules
            .iter()
            .all(|m| catalog.model_by_id(m.module.get(), m.model.0).is_some()),
        "All retained models must resolve in the recovered catalog",
    )?;
    full_stored_write_request(254, &snapshot.raw).map_err(err)?;
    Ok(())
}
fn position(snapshot: &PresetSnapshot) -> Result<F32Bits, String> {
    let module = snapshot
        .view
        .module(ModuleId::new(1).unwrap())
        .ok_or("P-Wah missing")?;
    require(
        module.model == ModelCode(0x0500_000a),
        "The supported overlay is P-Wah only",
    )?;
    let value = module
        .parameter(ParamId::new(3).unwrap())
        .ok_or("Position missing")?;
    require(
        value.as_f32().is_finite() && (0.0..=100.0).contains(&value.as_f32()),
        "Position must be finite and restorable",
    )?;
    Ok(value)
}
fn with_position(snapshot: &PresetSnapshot, value: F32Bits) -> Result<Arc<PresetSnapshot>, String> {
    position(snapshot)?;
    let mut records = snapshot.raw.records().to_vec();
    let record = records
        .iter_mut()
        .find(|r| r.command == 0x14 && r.body.len() == 68 && r.body[0] == 1)
        .ok_or("P-Wah raw record missing")?;
    record.body[20..24].copy_from_slice(&value.0.to_le_bytes());
    Ok(Arc::new(PresetSnapshot::from_raw(
        Arc::new(RawTransfer::from_records(snapshot.raw.kind(), &records).map_err(err)?),
        snapshot.source.clone(),
        snapshot.target,
        snapshot.revision,
    )))
}
fn overlay_only(expected: &PresetSnapshot, actual: &PresetSnapshot) -> Result<bool, String> {
    Ok(equal(
        expected,
        with_position(actual, position(expected)?)?.as_ref(),
    ))
}
fn level_patch(level: u16) -> EditPatch {
    EditPatch::SetMetadata(MetadataPatch {
        level: Some(level),
        ..Default::default()
    })
}
fn patched(snapshot: &PresetSnapshot, patch: &EditPatch) -> Result<Arc<PresetSnapshot>, String> {
    let mut draft = PresetDraft::from_snapshot(snapshot);
    draft.apply(patch).map_err(err)?;
    draft
        .snapshot(snapshot, snapshot.source.clone(), snapshot.revision)
        .map(Arc::new)
        .map_err(err)
}
fn latch(globals: &DeviceGlobalState) -> Result<bool, String> {
    globals
        .records()
        .runtime
        .as_ref()
        .and_then(|r| r.preset_changed())
        .ok_or("Known unique cmd1000 changed-latch state required".into())
}
fn request_failure(event: &DeviceEvent, waiting: &BTreeSet<IntentId>) -> Option<String> {
    match event {
        DeviceEvent::OperationFailed { stamp, error }
        | DeviceEvent::EditProgress {
            stamp,
            stage: EditStage::Failed(error) | EditStage::Indeterminate(error),
        } if waiting.contains(&stamp.intent) => {
            Some(format!("{:?}: {}", error.kind, error.message))
        }
        _ => None,
    }
}
fn validate_baseline(
    current: &PresetSnapshot,
    source: &PresetSnapshot,
    targets: &BTreeMap<SlotId, Arc<PresetSnapshot>>,
    globals: &DeviceGlobalState,
    catalog: &Catalog,
) -> Result<(), String> {
    require(
        current.context
            == Some(WireContext {
                group: 0,
                selector: SOURCE,
            }),
        "Current source must be slot23",
    )?;
    require(
        current.view.metadata.level == Some(50),
        "This acceptance uses established level50/51 only",
    )?;
    require(
        latch(globals)?,
        "Changed-latch baseline must be one; no guessed clear is available",
    )?;
    factory_only(current, catalog)?;
    factory_only(source, catalog)?;
    require(
        current
            .view
            .modules
            .iter()
            .all(|m| SOURCE_MODELS[m.module.get() as usize] == m.model.0),
        "Current factory models differ from the sampled source23 profile",
    )?;
    require(
        overlay_only(as_current(source, slot(SOURCE))?.as_ref(), current)?,
        "Current differs from stored source beyond the evidenced P-Wah Position overlay",
    )?;
    require(
        targets.len() == 2,
        "Both scratch before-images are required",
    )?;
    for n in TARGETS {
        let target = targets
            .get(&slot(n))
            .ok_or("Scratch before-image missing")?;
        require(
            target.target == Some(PresetTarget::Stored(slot(n)))
                && target.context
                    == Some(WireContext {
                        group: 1,
                        selector: n,
                    }),
            "Scratch context mismatch",
        )?;
        factory_only(target, catalog)?;
    }
    Ok(())
}

struct Lab {
    app: AppController,
    service: MidiDeviceService,
    output: PathBuf,
    phase: &'static str,
    sequence: u64,
    selection: Option<DeviceSelection>,
    snapshots: BTreeMap<PresetTarget, (u64, Arc<PresetSnapshot>)>,
    waiting_intents: BTreeSet<IntentId>,
    verified: bool,
    trace: File,
    tx: File,
    rx: File,
    commands: Value,
    captures: BTreeMap<String, ContentHash>,
}
impl Lab {
    fn effects(&mut self, effects: Vec<Effect>) -> Result<bool, String> {
        let sent = !effects.is_empty();
        for effect in effects {
            let Effect::Device(command) = effect else {
                return Err("Unexpected non-device effect".into());
            };
            let category = match &command {
                DeviceCommand::WriteStoredPreset { .. } => "full_stored_writes",
                DeviceCommand::SaveBufferTo { .. } => "save_commands",
                DeviceCommand::SelectStored { .. } => "select_commands",
                DeviceCommand::SubmitEdit { .. } => "buffer_edit_commands",
                DeviceCommand::SubmitGlobalEdit { .. }
                | DeviceCommand::SubmitGlobalControlEdit { .. }
                | DeviceCommand::ExecuteDeviceJob { .. }
                | DeviceCommand::ClearStoredPreset { .. } => {
                    return Err(
                        "Probe refuses global, asset, clear, firmware/reset commands".into(),
                    );
                }
                _ => "read_or_lifecycle_commands",
            };
            match &command {
                DeviceCommand::Connect { intent, .. } | DeviceCommand::Disconnect { intent } => {
                    self.waiting_intents.insert(*intent);
                }
                DeviceCommand::RefreshTarget { stamp, .. }
                | DeviceCommand::SubmitEdit { stamp, .. }
                | DeviceCommand::SaveBufferTo { stamp, .. }
                | DeviceCommand::WriteStoredPreset { stamp, .. }
                | DeviceCommand::SelectStored { stamp, .. }
                | DeviceCommand::ReadGlobals(stamp) => {
                    self.waiting_intents.insert(stamp.intent);
                }
                _ => {}
            }
            self.commands[category] = json!(self.commands[category].as_u64().unwrap_or(0) + 1);
            self.service
                .try_submit(command)
                .map_err(|e| format!("Submit rejected: {e:?}"))?;
        }
        Ok(sent)
    }
    fn input(&mut self, input: AppInput) -> Result<bool, String> {
        let effects = self.app.dispatch(input);
        self.effects(effects)
    }
    fn capture(&mut self, name: String, bytes: &[u8]) -> Result<(), String> {
        write_new(&self.output.join(&name), bytes)?;
        self.captures.insert(name, ContentHash::of(bytes));
        Ok(())
    }
    fn wait(&mut self, accepts: impl Fn(&Self) -> bool) -> Result<(), String> {
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            if let Some(event) = self.service.try_next_event() {
                self.sequence += 1;
                writeln!(self.trace, "{} {} {event:?}", self.phase, self.sequence).map_err(err)?;
                match &event {
                    DeviceEvent::PortsChanged(ports) => {
                        let matching: Vec<_> = ports
                            .iter()
                            .filter(|p| p.display_name.to_uppercase().contains("GP-200"))
                            .collect();
                        if matching.len() == 1 {
                            self.selection = Some(matching[0].selection.clone());
                        }
                    }
                    DeviceEvent::SnapshotReceived {
                        target,
                        snapshot,
                        stamp,
                    } => {
                        self.capture(
                            format!("{}-{}-preset.payload.bin", self.phase, self.sequence),
                            snapshot.raw.payload(),
                        )?;
                        if stamp.epoch == self.app.state().epoch
                            && self.waiting_intents.contains(&stamp.intent)
                        {
                            self.snapshots
                                .insert(*target, (self.sequence, snapshot.clone()));
                        }
                    }
                    DeviceEvent::GlobalStateReceived { state, .. } => {
                        self.capture(
                            format!("{}-{}-globals.payload.bin", self.phase, self.sequence),
                            state.records().raw().payload(),
                        )?;
                    }
                    DeviceEvent::EditProgress {
                        stamp,
                        stage: EditStage::Verified,
                    } if self.waiting_intents.contains(&stamp.intent) => self.verified = true,
                    DeviceEvent::Diagnostic(d)
                        if matches!(d.category.as_str(), "midi-tx" | "midi-rx") =>
                    {
                        if let Some(bytes) = &d.bytes {
                            (if d.category == "midi-tx" {
                                &mut self.tx
                            } else {
                                &mut self.rx
                            })
                            .write_all(bytes)
                            .map_err(err)?;
                        }
                    }
                    _ => {}
                }
                let failure = request_failure(&event, &self.waiting_intents);
                self.input(AppInput::Device(event))?;
                if let Some(failure) = failure {
                    return Err(failure);
                }
                if accepts(self) {
                    return Ok(());
                }
            } else {
                thread::sleep(Duration::from_millis(2));
            }
        }
        Err(format!("{} event deadline exceeded", self.phase))
    }
    fn connect(&mut self) -> Result<(), String> {
        self.waiting_intents.clear();
        let selection = self
            .selection
            .clone()
            .ok_or("Exactly one GP-200 endpoint required")?;
        let epoch = self.app.state().epoch;
        self.input(AppInput::Ui(UiIntent::Connect(selection)))?;
        self.wait(|lab| {
            lab.app.state().epoch != epoch
                && lab.app.state().connection == ConnectionState::Ready
                && lab.app.state().confirmed_buffer.is_some()
        })
    }
    fn read(&mut self, target: PresetTarget) -> Result<Arc<PresetSnapshot>, String> {
        self.waiting_intents.clear();
        let sequence = self.sequence;
        require(
            self.input(AppInput::Ui(UiIntent::Refresh(target)))?,
            "Read rejected by controller",
        )?;
        self.wait(|lab| {
            !lab.app.device_operation_pending()
                && lab
                    .snapshots
                    .get(&target)
                    .is_some_and(|(n, _)| *n > sequence)
        })?;
        Ok(self
            .snapshots
            .get(&target)
            .ok_or("Snapshot absent")?
            .1
            .clone())
    }
    fn globals(&mut self) -> Result<Arc<DeviceGlobalState>, String> {
        self.waiting_intents.clear();
        let revision = self
            .app
            .state()
            .device_globals
            .as_ref()
            .map_or(Revision(0), |s| s.revision);
        require(
            self.input(AppInput::Ui(UiIntent::RefreshDeviceGlobals))?,
            "Global read rejected",
        )?;
        self.wait(|lab| {
            !lab.app.device_operation_pending()
                && lab
                    .app
                    .state()
                    .device_globals
                    .as_ref()
                    .is_some_and(|s| s.revision > revision)
        })?;
        self.app
            .state()
            .device_globals
            .clone()
            .ok_or("Globals absent".into())
    }
    fn mutation(&mut self, intent: UiIntent) -> Result<(), String> {
        self.waiting_intents.clear();
        self.verified = false;
        if !self.input(AppInput::Ui(intent))? {
            let messages: Vec<_> = self
                .app
                .state()
                .messages
                .iter()
                .map(|message| {
                    json!({"sequence":message.sequence, "severity":format!("{:?}",message.severity), "message":message.message, "error":format!("{:?}",message.error)})
                })
                .collect();
            json_new(
                &self.output.join(format!(
                    "{}-{}-mutation-refused.json",
                    self.phase, self.sequence
                )),
                &json!({"phase":self.phase, "document_dirty":self.app.state().documents.get(&device_document_id()).map(EditorDocument::dirty), "confirmed_context":self.app.state().confirmed_buffer.as_ref().and_then(|snapshot|snapshot.context), "messages":messages, "send_occurred":false}),
            )?;
            return Err(format!(
                "Mutation refused by controller; no send occurred; messages: {}",
                serde_json::to_string(&messages).map_err(err)?
            ));
        }
        self.wait(|lab| lab.verified && !lab.app.device_operation_pending())
    }
    fn edit(&mut self, patch: EditPatch) -> Result<(), String> {
        self.mutation(UiIntent::ApplyEdit {
            document: device_document_id(),
            patch,
        })
    }
    fn recall(&mut self, slot: ComparisonSlot) -> Result<(), String> {
        self.waiting_intents.clear();
        self.verified = false;
        let effects = self.app.recall_comparison(&device_document_id(), slot);
        require(self.effects(effects)?, "A/B recall did not create an edit")?;
        self.wait(|lab| lab.verified && !lab.app.device_operation_pending())
    }
    fn origin(&self) -> Result<BackupOrigin, String> {
        let selection = self.selection.as_ref().ok_or("Live selection missing")?;
        let fingerprint = selection
            .fingerprint_hint
            .as_ref()
            .ok_or("Observed USB fingerprint required")?;
        require(
            fingerprint.vendor == Some(0x84ef) && fingerprint.product == Some(0x002a),
            "Expected GP-200 Normal USB fingerprint required",
        )?;
        let serial = fingerprint
            .serial
            .as_ref()
            .filter(|s| !s.is_empty())
            .ok_or("Observed serial required for durable target ownership")?;
        let identity = self
            .app
            .state()
            .identity
            .clone()
            .ok_or("Live identity missing")?;
        require(
            identity.model == DeviceModel::Gp200 && identity.mode == BackendMode::Hardware,
            "Actual Hardware GP-200 identity required",
        )?;
        Ok(BackupOrigin {
            identity,
            epoch: self.app.state().epoch,
            device_key: format!(
                "usb-{:04x}-{:04x}-{serial}",
                fingerprint.vendor.unwrap(),
                fingerprint.product.unwrap()
            ),
            evidence: vec![EvidenceRef::new(
                "Root production acceptance: freshly enumerated USB fingerprint and typed actor identity",
                EvidenceLevel::LiveVerified,
            )],
        })
    }
    fn target_reads(&mut self) -> Result<BTreeMap<SlotId, Arc<PresetSnapshot>>, String> {
        TARGETS
            .into_iter()
            .map(|n| {
                self.read(PresetTarget::Stored(slot(n)))
                    .map(|s| (slot(n), s))
            })
            .collect()
    }
}

struct Baseline {
    current: Arc<PresetSnapshot>,
    source: Arc<PresetSnapshot>,
    targets: BTreeMap<SlotId, Arc<PresetSnapshot>>,
    globals: Arc<DeviceGlobalState>,
    backup: String,
    origin: BackupOrigin,
}
fn bounded_file(path: &Path, maximum: u64) -> Result<Vec<u8>, String> {
    let before = fs::symlink_metadata(path).map_err(err)?;
    require(
        before.is_file()
            && !before.file_type().is_symlink()
            && before.nlink() == 1
            && before.len() <= maximum,
        "Retained evidence must be a bounded regular file with one link",
    )?;
    let file = File::open(path).map_err(err)?;
    let opened = file.metadata().map_err(err)?;
    require(
        before.dev() == opened.dev() && before.ino() == opened.ino(),
        "Evidence changed while opening",
    )?;
    let mut bytes = Vec::new();
    file.take(maximum + 1)
        .read_to_end(&mut bytes)
        .map_err(err)?;
    require(
        bytes.len() as u64 <= maximum,
        "Retained evidence exceeds read bound",
    )?;
    Ok(bytes)
}
fn check_ancestors(path: &Path) -> Result<(), String> {
    require(
        path.is_absolute(),
        "Retained evidence path must be absolute",
    )?;
    for ancestor in path.ancestors() {
        let metadata = fs::symlink_metadata(ancestor).map_err(err)?;
        require(
            metadata.is_dir() && !metadata.file_type().is_symlink(),
            "Retained evidence ancestors must be real directories",
        )?;
    }
    Ok(())
}
fn retained_baseline(directory: &Path) -> Result<Baseline, String> {
    check_ancestors(directory)?;
    let receipt_bytes = bounded_file(&directory.join("result.json"), 256 * 1024)?;
    require(
        ContentHash::of(&receipt_bytes).as_str() == NEGATIVE_RECEIPT_SHA,
        "Cleanup is pinned to the retained reviewed negative receipt",
    )?;
    let receipt: Value = serde_json::from_slice(&receipt_bytes).map_err(err)?;
    require(
        receipt["schema"] == 1
            && receipt["source_sha256"]
                == "21bb99ed7ccb591f21654c5fcff7d79d1e56a72cdf2c959b7cc27aa43c4bce1f"
            && receipt["binary_sha256"]
                == "612fffdb818e9a54bfb02423b7e58edd21001c5f099497e9028206d8a6c3bc50",
        "Retained source/binary identity differs",
    )?;
    require(
        receipt["accepted"] == false
            && receipt["terminal_phase"] == "cleanup-select-source23"
            && receipt["save_as_dual_readbacks_verified"] == true,
        "This cleanup mode only supports the reviewed selection-refusal negative",
    )?;
    let origin: BackupOrigin =
        serde_json::from_value(receipt["initial_origin"].clone()).map_err(err)?;
    let backup = receipt["before_image_backup"]
        .as_str()
        .ok_or("Backup identifier missing")?
        .to_string();
    require(
        backup.starts_with("backup-")
            && backup.len() <= 64
            && backup
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-'),
        "Invalid retained backup identifier",
    )?;
    let root = directory
        .parent()
        .ok_or("Independent root capture directory missing")?;
    let root_receipt: Value =
        serde_json::from_slice(&bounded_file(&root.join("result.json"), 512 * 1024)?)
            .map_err(err)?;
    require(
        root_receipt["schema"] == 1
            && root_receipt["source_sha256"] == receipt["source_sha256"]
            && root_receipt["probe_sha256"] == receipt["binary_sha256"],
        "Independent producer receipt differs",
    )?;
    let mut loaded = BTreeMap::new();
    for (name, snapshot_file, raw_file, target) in [
        (
            "current",
            "baseline-current.snapshot.json",
            "current-before.payload.bin",
            PresetTarget::EditBuffer,
        ),
        (
            "source23",
            "baseline-source23.snapshot.json",
            "source23-before.payload.bin",
            PresetTarget::Stored(slot(SOURCE)),
        ),
        (
            "scratch254",
            "baseline-scratch254.snapshot.json",
            "scratch254-before.payload.bin",
            PresetTarget::Stored(slot(254)),
        ),
        (
            "scratch255",
            "baseline-scratch255.snapshot.json",
            "scratch255-before.payload.bin",
            PresetTarget::Stored(slot(255)),
        ),
    ] {
        let imported: PresetSnapshot =
            serde_json::from_slice(&bounded_file(&directory.join(snapshot_file), 128 * 1024)?)
                .map_err(err)?;
        require(
            raw_hash(&imported.raw).as_str()
                == receipt["baseline_raw_hashes"][name]
                    .as_str()
                    .ok_or("Retained raw hash missing")?,
            "Retained preset before-image hash differs",
        )?;
        // Retained JSON is data, never device authority; re-derive all projections.
        let snapshot = Arc::new(PresetSnapshot::from_raw(
            imported.raw.clone(),
            SnapshotSource::Hardware {
                epoch: origin.epoch,
            },
            Some(target),
            imported.revision,
        ));
        let independent_bytes = bounded_file(&root.join(raw_file), 16 * 1024)?;
        require(
            ContentHash::of(&independent_bytes).as_str()
                == root_receipt["before_payload_hashes"][name]
                    .as_str()
                    .ok_or("Independent payload hash missing")?,
            "Independent root before-payload hash differs",
        )?;
        let independent = PresetSnapshot::from_raw(
            Arc::new(RawTransfer::from_payload(0x12, independent_bytes).map_err(err)?),
            SnapshotSource::Hardware {
                epoch: origin.epoch,
            },
            Some(target),
            imported.revision,
        );
        require(
            equal(&snapshot, &independent),
            "Independent root preset differs from retained baseline",
        )?;
        loaded.insert(name, snapshot);
    }
    let globals: DeviceGlobalState = serde_json::from_slice(&bounded_file(
        &directory.join("baseline-globals.json"),
        128 * 1024,
    )?)
    .map_err(err)?;
    require(
        raw_hash(globals.records().raw()).as_str()
            == receipt["baseline_raw_hashes"]["globals"]
                .as_str()
                .ok_or("Global hash missing")?,
        "Retained globals before-image hash differs",
    )?;
    let independent_bytes = bounded_file(&root.join("globals-before.payload.bin"), 16 * 1024)?;
    require(
        ContentHash::of(&independent_bytes).as_str()
            == root_receipt["before_payload_hashes"]["globals"]
                .as_str()
                .ok_or("Independent globals hash missing")?,
        "Independent root globals payload differs",
    )?;
    let independent = gp200_protocol::GlobalRecords::from_transfer(
        RawTransfer::from_payload(0x12, independent_bytes).map_err(err)?,
    )
    .map_err(err)?;
    require(
        globals.records().control_state_equal(&independent),
        "Independent root globals differ from retained baseline",
    )?;
    let baseline = Baseline {
        current: loaded.remove("current").unwrap(),
        source: loaded.remove("source23").unwrap(),
        targets: BTreeMap::from([
            (slot(254), loaded.remove("scratch254").unwrap()),
            (slot(255), loaded.remove("scratch255").unwrap()),
        ]),
        globals: Arc::new(globals),
        backup,
        origin,
    };
    validate_baseline(
        &baseline.current,
        &baseline.source,
        &baseline.targets,
        &baseline.globals,
        &Catalog::embedded().map_err(err)?,
    )?;
    Ok(baseline)
}
fn clone_retained_library(source: &Path, destination: &Path) -> Result<(), String> {
    check_ancestors(source)?;
    fresh_directory(destination)?;
    fresh_directory(&destination.join("blobs"))?;
    for name in ["manifest.json", "manifest.previous.json"] {
        let path = source.join(name);
        if path.exists() {
            write_new(
                &destination.join(name),
                &bounded_file(&path, 4 * 1024 * 1024)?,
            )?;
        }
    }
    let blobs = source.join("blobs");
    check_ancestors(&blobs)?;
    let mut count = 0;
    let mut total = 0_u64;
    for entry in fs::read_dir(blobs).map_err(err)? {
        let entry = entry.map_err(err)?;
        count += 1;
        require(count <= 128, "Retained library exceeds file-count bound")?;
        let name = entry.file_name();
        let name = name.to_str().ok_or("Invalid blob filename")?;
        require(
            name.len() == 64
                && name
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "Invalid immutable blob hash filename",
        )?;
        let bytes = bounded_file(&entry.path(), 16 * 1024 * 1024)?;
        total += bytes.len() as u64;
        require(
            total <= 64 * 1024 * 1024,
            "Retained library exceeds total-size bound",
        )?;
        require(
            ContentHash::of(&bytes).as_str() == name,
            "Retained immutable blob hash differs",
        )?;
        write_new(&destination.join("blobs").join(name), &bytes)?;
    }
    Ok(())
}
fn reviewed_plan(
    store: &mut LibraryStore,
    lab: &mut Lab,
    backup: &str,
    restore_originals: bool,
) -> Result<RestoreJournal, String> {
    let current = lab.read(PresetTarget::EditBuffer)?;
    let observed = lab.target_reads()?;
    let plan = store
        .plan_restore(&RestoreRequest {
            target: lab.origin()?,
            current_buffer: current,
            selections: TARGETS
                .into_iter()
                .map(|n| RestoreSelection {
                    source: RestoreSource::BackupStored {
                        backup: backup.into(),
                        slot: slot(if restore_originals { n } else { SOURCE }),
                    },
                    target: slot(n),
                })
                .collect(),
            observed_targets: observed,
            asset_bindings: vec![],
            target_assets: vec![],
        })
        .map_err(err)?;
    require(
        plan.entries.len() == 2 && plan.excluded.is_empty(),
        "Both entries must pass the existing durable plan gates",
    )?;
    json_new(
        &lab.output.join(format!("{}-review.json", lab.phase)),
        &plan,
    )?;
    let journal = store
        .begin_restore(
            plan.clone(),
            RestoreReview {
                plan_hash: plan.review_hash().map_err(err)?,
                accept_unknown_firmware: true,
            },
        )
        .map_err(err)?;
    json_new(
        &lab.output.join(format!("{}-journal-ready.json", lab.phase)),
        &journal,
    )?;
    Ok(journal)
}
fn apply_entry(
    store: &mut LibraryStore,
    lab: &mut Lab,
    id: &str,
    target: SlotId,
    omit_confirmation: bool,
) -> Result<(), String> {
    let current = lab.read(PresetTarget::EditBuffer)?;
    let before = lab.read(PresetTarget::Stored(target))?;
    let dispatch = store
        .begin_restore_entry(id, target, before, current)
        .map_err(err)?;
    // Persisted InFlight is reopened by the library before this dispatcher is used.
    json_new(
        &lab.output
            .join(format!("{}-{}-inflight.json", lab.phase, target.get())),
        &store.restore_journal(id).map_err(err)?,
    )?;
    lab.mutation(UiIntent::WriteStoredPreset {
        slot: target,
        expected_target_revision: dispatch.expected_target_revision,
        snapshot: dispatch.snapshot.clone(),
    })?;
    let actual = lab.read(PresetTarget::Stored(target))?;
    require(
        stored_readback_equal(&dispatch.snapshot.raw, &actual.raw),
        "Independent fresh read differs from the reviewed source",
    )?;
    if !omit_confirmation {
        store.confirm_restore_readback(id, actual).map_err(err)?;
    }
    Ok(())
}
fn experiment(
    lab: &mut Lab,
    baseline: &Baseline,
    store: &mut Option<LibraryStore>,
    report: &mut Value,
) -> Result<(), String> {
    lab.phase = "copy-two-inactive";
    let journal = reviewed_plan(
        store.as_mut().ok_or("Store missing")?,
        lab,
        &baseline.backup,
        false,
    )?;
    report["copy_journal"] = json!(journal.id);
    report["mutation_attempted"] = true.into();
    apply_entry(store.as_mut().unwrap(), lab, &journal.id, slot(254), false)?;
    apply_entry(store.as_mut().unwrap(), lab, &journal.id, slot(255), true)?;
    report["actor_verified_two_slot_copy"] = true.into();
    let paused = store.as_mut().unwrap().pause_restore(&journal.id, "Intentional lost library confirmation after actor Verified; no MIDI ACK was suppressed").map_err(err)?;
    json_new(&lab.output.join("copy-paused-journal.json"), &paused)?;
    require(
        paused.verified_count() == 1
            && matches!(paused.entries[1].state, RestoreEntryState::Uncertain { .. }),
        "Only the unconfirmed second entry may be uncertain",
    )?;
    drop(store.take());
    *store = Some(LibraryStore::open(lab.output.join("durable-library")).map_err(err)?);
    lab.phase = "reconcile-read-only";
    let before_commands = lab.commands.clone();
    let readbacks = lab.target_reads()?;
    let reconciled = store
        .as_mut()
        .unwrap()
        .reconcile_restore(&journal.id, lab.origin()?, readbacks)
        .map_err(err)?;
    json_new(
        &lab.output.join("copy-reconciled-journal.json"),
        &reconciled,
    )?;
    require(
        reconciled.state == RestoreRunState::Completed && reconciled.verified_count() == 2,
        "Fresh readback reconciliation did not establish both entries",
    )?;
    require(
        lab.commands["full_stored_writes"] == before_commands["full_stored_writes"]
            && lab.commands["save_commands"] == before_commands["save_commands"]
            && lab.commands["buffer_edit_commands"] == before_commands["buffer_edit_commands"],
        "Reconciliation sent a mutation",
    )?;
    report["journal_confirmation_loss_recovered_without_replay"] = true.into();
    require(
        equal(
            &baseline.current,
            lab.read(PresetTarget::EditBuffer)?.as_ref(),
        ),
        "Inactive copy changed the current preset",
    )?;
    require(
        baseline
            .globals
            .records()
            .control_state_equal(lab.globals()?.records()),
        "Inactive copy changed globals or the changed latch",
    )?;

    lab.phase = "comparison-a-b";
    lab.app
        .capture_comparison(&device_document_id(), ComparisonSlot::A)
        .map_err(err)?;
    let expected_b = patched(&baseline.current, &level_patch(51))?;
    lab.edit(level_patch(51))?;
    require(
        equal(&expected_b, lab.read(PresetTarget::EditBuffer)?.as_ref()),
        "B level edit mismatch",
    )?;
    lab.app
        .capture_comparison(&device_document_id(), ComparisonSlot::B)
        .map_err(err)?;
    lab.recall(ComparisonSlot::A)?;
    require(
        equal(
            &baseline.current,
            lab.read(PresetTarget::EditBuffer)?.as_ref(),
        ),
        "Recall A mismatch",
    )?;
    lab.recall(ComparisonSlot::B)?;
    require(
        equal(&expected_b, lab.read(PresetTarget::EditBuffer)?.as_ref()),
        "Recall B mismatch",
    )?;
    lab.mutation(UiIntent::Undo(device_document_id()))?;
    require(
        equal(
            &baseline.current,
            lab.read(PresetTarget::EditBuffer)?.as_ref(),
        ),
        "Production Undo after B did not restore A",
    )?;
    report["comparison_a_b_undo_verified"] = true.into();
    let copied = lab.target_reads()?;
    for n in TARGETS {
        require(
            equal(
                as_stored(&baseline.source, slot(n), copied[&slot(n)].revision)?.as_ref(),
                &copied[&slot(n)],
            ),
            "A/B changed a stored target",
        )?;
    }
    report["comparison_stored_targets_unchanged"] = true.into();

    lab.phase = "save-as-255";
    let current = lab.read(PresetTarget::EditBuffer)?;
    let target = lab.read(PresetTarget::Stored(slot(255)))?;
    let save_expected = as_stored(&current, slot(255), target.revision)?;
    json_new(
        &lab.output.join("save-before-dispatch.json"),
        &json!({
            "schema":1, "state":"InFlight descriptive checkpoint; actor still validates authority",
            "epoch":lab.app.state().epoch, "source_current_raw_hash":raw_hash(&current.raw),
            "target_before_raw_hash":raw_hash(&target.raw), "expected_stored_raw_hash":raw_hash(&save_expected.raw),
            "before_image_backup":baseline.backup, "slot":255, "automatic_replay":false,
        }),
    )?;
    lab.mutation(UiIntent::SaveBufferTo {
        slot: slot(255),
        expected_baseline: current.revision,
    })?;
    report["document_dirty_immediately_after_save"] = json!(
        lab.app
            .state()
            .documents
            .get(&device_document_id())
            .map(EditorDocument::dirty)
    );
    require(
        matches!(lab.app.state().buffer_persistence, BufferPersistence::Saved { slot: s, .. } if s == slot(255)),
        "Controller did not confirm both Save readbacks",
    )?;
    let stored_after = lab.read(PresetTarget::Stored(slot(255)))?;
    let current_after = lab.read(PresetTarget::EditBuffer)?;
    report["document_dirty_after_post_save_fresh_read"] = json!(
        lab.app
            .state()
            .documents
            .get(&device_document_id())
            .map(EditorDocument::dirty)
    );
    require(
        equal(&save_expected, &stored_after),
        "Saved stored payload mismatch",
    )?;
    require(
        overlay_only(as_current(&current, slot(255))?.as_ref(), &current_after)?,
        "Save selected buffer has unexpected changes",
    )?;
    json_new(
        &lab.output.join("save-verified.json"),
        &json!({"stored_raw_hash":raw_hash(&stored_after.raw), "current_raw_hash":raw_hash(&current_after.raw), "changed_latch_observed":latch(lab.globals()?.as_ref())?, "automatic_replay":false}),
    )?;
    report["save_as_dual_readbacks_verified"] = true.into();
    Ok(())
}
fn cleanup(
    lab: &mut Lab,
    baseline: &Baseline,
    store: &mut Option<LibraryStore>,
    report: &mut Value,
) -> Result<(), String> {
    // A fresh session retires application uncertainty and deferred intents. This
    // does not replay the interrupted operation or mutate the current preset.
    lab.phase = "cleanup-fresh-session";
    report["retired_probe_document_dirty_before_cleanup"] = json!(
        lab.app
            .state()
            .documents
            .get(&device_document_id())
            .map(EditorDocument::dirty)
    );
    // Connect preserves user documents by design. This root-only probe owns
    // its experiment documents and retires them before establishing new live
    // cleanup authority; the real service still creates a fresh actor session.
    lab.app = AppController::new(Catalog::embedded().map_err(err)?, AppPreferences::default());
    lab.snapshots.clear();
    lab.connect()?;
    require(
        lab.origin()?.device_key == baseline.origin.device_key
            && lab.origin()?.identity.model == baseline.origin.identity.model
            && lab.origin()?.identity.mode == baseline.origin.identity.mode
            && lab.origin()?.identity.firmware == baseline.origin.identity.firmware,
        "Cleanup live device differs from the retained before-image owner",
    )?;
    let actual = lab.read(PresetTarget::EditBuffer)?;
    let context = actual.context.ok_or("Cleanup current context missing")?;
    require(
        context.group == 0 && [SOURCE, 255].contains(&context.selector),
        "Unexpected selected slot; retained before-images require root review",
    )?;
    let expected = as_current(&baseline.current, slot(context.selector))?;
    let expected_b = patched(&expected, &level_patch(51))?;
    require(
        overlay_only(&expected, &actual)? || overlay_only(&expected_b, &actual)?,
        "Unexpected current edits; automatic cleanup refused",
    )?;
    if actual.view.metadata.level != Some(50) {
        lab.phase = "cleanup-level-before-select";
        lab.edit(level_patch(50))?;
        lab.app = AppController::new(Catalog::embedded().map_err(err)?, AppPreferences::default());
        lab.snapshots.clear();
        lab.connect()?;
    }
    let current = lab.read(PresetTarget::EditBuffer)?;
    if current.context != baseline.current.context {
        report["document_dirty_after_cleanup_current_refresh"] = json!(
            lab.app
                .state()
                .documents
                .get(&device_document_id())
                .map(EditorDocument::dirty)
        );
        // Ordinary Refresh can alter retained CTRL padding in the draft. Retire
        // only this probe's controller once more, validate Connect's fresh
        // current snapshot, then read the stored source and select immediately.
        // No intervening current Refresh may recreate that descriptive dirtiness.
        lab.phase = "cleanup-clean-selection-session";
        lab.app = AppController::new(Catalog::embedded().map_err(err)?, AppPreferences::default());
        lab.snapshots.clear();
        lab.connect()?;
        let connected = lab
            .app
            .state()
            .confirmed_buffer
            .clone()
            .ok_or("Fresh selection-session current is missing")?;
        require(
            equal(&current, &connected),
            "Fresh selection-session current differs from the recognized state",
        )?;
        require(
            lab.origin()?.device_key == baseline.origin.device_key
                && lab.origin()?.identity.model == baseline.origin.identity.model
                && lab.origin()?.identity.mode == baseline.origin.identity.mode
                && lab.origin()?.identity.firmware == baseline.origin.identity.firmware,
            "Fresh selection-session owner differs from retained before-images",
        )?;
        lab.phase = "cleanup-select-source23";
        let source = lab.read(PresetTarget::Stored(slot(SOURCE)))?;
        require(
            equal(&baseline.source, &source),
            "Stored source23 changed; selection refused",
        )?;
        report["document_dirty_immediately_before_selection"] = json!(
            lab.app
                .state()
                .documents
                .get(&device_document_id())
                .map(EditorDocument::dirty)
        );
        lab.mutation(UiIntent::AuditionStored(slot(SOURCE)))?;
    }
    lab.phase = "cleanup-position";
    let current = lab.read(PresetTarget::EditBuffer)?;
    require(
        overlay_only(&baseline.current, &current)?,
        "Source selection produced changes beyond known Position",
    )?;
    if position(&current)? != position(&baseline.current)? {
        lab.edit(EditPatch::SetParameter {
            module: ModuleId::new(1).unwrap(),
            model: ModelCode(0x0500_000a),
            model_index: ModelIndex(2),
            parameter: ParamId::new(3).unwrap(),
            value: position(&baseline.current)?,
        })?;
    }
    require(
        equal(
            &baseline.current,
            lab.read(PresetTarget::EditBuffer)?.as_ref(),
        ),
        "Captured original current F32 bits not restored",
    )?;
    let actual_targets = lab.target_reads()?;
    for n in TARGETS {
        let actual = &actual_targets[&slot(n)];
        let copy = as_stored(&baseline.source, slot(n), actual.revision)?;
        let saved = as_stored(&baseline.current, slot(n), actual.revision)?;
        require(
            equal(&baseline.targets[&slot(n)], actual)
                || equal(&copy, actual)
                || (n == 255 && equal(&saved, actual)),
            "Scratch has an unrecognized payload; automatic cleanup refused",
        )?;
    }
    if store.is_none() {
        *store = Some(LibraryStore::open(lab.output.join("durable-library")).map_err(err)?);
    }
    // A new review uses the retained originals as sources and actual fresh
    // scratch before-images. No old serialized dispatch is trusted or replayed.
    lab.phase = "cleanup-restore-originals";
    let journal = reviewed_plan(store.as_mut().unwrap(), lab, &baseline.backup, true)?;
    report["cleanup_journal"] = json!(journal.id);
    for n in TARGETS {
        apply_entry(store.as_mut().unwrap(), lab, &journal.id, slot(n), false)?;
    }
    let journal = store
        .as_ref()
        .unwrap()
        .restore_journal(&journal.id)
        .map_err(err)?;
    require(
        journal.state == RestoreRunState::Completed && journal.verified_count() == 2,
        "Original-slot restore journal not complete",
    )?;
    json_new(&lab.output.join("cleanup-completed-journal.json"), &journal)?;

    lab.phase = "cleanup-changed-latch-one";
    let globals = lab.globals()?;
    if !latch(&globals)? {
        // Selection legitimately clears the native changed latch. A real
        // reversible level edit and production Undo return it to baseline one.
        lab.edit(level_patch(51))?;
        lab.mutation(UiIntent::Undo(device_document_id()))?;
        report["changed_latch_reestablished_by_level_undo"] = true.into();
    }
    lab.phase = "final-read-only";
    let current = lab.read(PresetTarget::EditBuffer)?;
    let source = lab.read(PresetTarget::Stored(slot(SOURCE)))?;
    let targets = lab.target_reads()?;
    let globals = lab.globals()?;
    report["current_restored"] = equal(&baseline.current, &current).into();
    report["stored_source23_unchanged"] = equal(&baseline.source, &source).into();
    report["scratch254_restored"] =
        equal(&baseline.targets[&slot(254)], &targets[&slot(254)]).into();
    report["scratch255_restored"] =
        equal(&baseline.targets[&slot(255)], &targets[&slot(255)]).into();
    report["globals_restored_including_changed_latch"] = baseline
        .globals
        .records()
        .control_state_equal(globals.records())
        .into();
    report["final_changed_latch"] = latch(&globals)?.into();
    for key in [
        "current_restored",
        "stored_source23_unchanged",
        "scratch254_restored",
        "scratch255_restored",
        "globals_restored_including_changed_latch",
    ] {
        require(
            report[key] == true,
            "Final restoration mismatch; root must inspect retained raw evidence",
        )?;
    }
    Ok(())
}
fn run() -> Result<(), String> {
    let mut execute = false;
    let mut output = None;
    let mut cleanup_from = None;
    let mut args = std::env::args_os().skip(1);
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--execute") => execute = true,
            Some("--cleanup-from") => {
                cleanup_from = Some(PathBuf::from(
                    args.next()
                        .ok_or("--cleanup-from requires retained probe evidence")?,
                ))
            }
            Some("--output") => {
                output = Some(PathBuf::from(
                    args.next()
                        .ok_or("--output requires a fresh absolute directory")?,
                ))
            }
            _ => {
                return Err(
                    "Usage: inactive_restore_probe [--execute --output FRESH_ABSOLUTE_DIRECTORY]"
                        .into(),
                );
            }
        }
    }
    // Validate retained inputs before constructing or connecting a MIDI service.
    let mut retained = cleanup_from.as_deref().map(retained_baseline).transpose()?;
    if !execute {
        if cleanup_from.is_some() {
            println!(
                "Dry cleanup: retained negative receipt and independent before-images verified; no MIDI opened and no library copied."
            );
            return Ok(());
        }
        println!(
            "Dry: factory-only source23/current latch1; fsynced current/source23/254/255/globals; durable two-slot copy; intentionally lost library confirmation and read-only reconciliation; A/B+Undo; production Save As255; source selection+exact Position restoration; second durable journal restores both originals; strict final globals including changed latch. No MIDI opened."
        );
        return Ok(());
    }
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(".hardware-session.lock")
        .map_err(err)?;
    lock.try_lock()
        .map_err(|_| "Another hardware session owns the workspace")?;
    let output = output.ok_or("--execute requires --output")?;
    fresh_directory(&output)?;
    let mut lab = Lab {
        app: AppController::new(Catalog::embedded().map_err(err)?, AppPreferences::default()),
        service: MidiDeviceService::with_backend(
            Box::new(gp200_transport::AlsaRawBackend::new()),
            TransportConfig {
                trace_bytes: true,
                ..TransportConfig::alsa_raw()
            },
            no_wake(),
        ),
        trace: OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(output.join("trace.txt"))
            .map_err(err)?,
        tx: OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(output.join("tx.bin"))
            .map_err(err)?,
        rx: OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(output.join("rx.bin"))
            .map_err(err)?,
        output: output.clone(),
        phase: "discover",
        sequence: 0,
        selection: None,
        snapshots: BTreeMap::new(),
        waiting_intents: BTreeSet::new(),
        verified: false,
        commands: json!({}),
        captures: BTreeMap::new(),
    };
    let executable = std::env::current_exe().map_err(err)?;
    require(
        fs::metadata(&executable).map_err(err)?.len() <= 128 * 1024 * 1024,
        "Executable hash bound exceeded",
    )?;
    let mut report = json!({"schema":1, "profile":"production-inactive-restore-copy-save-ab-v1", "source_sha256":ContentHash::of(include_bytes!("inactive_restore_probe.rs")), "binary_sha256":ContentHash::of(&fs::read(executable).map_err(err)?),
        "mutation_attempted":false, "actor_verified_two_slot_copy":false, "journal_confirmation_loss_recovered_without_replay":false,
        "comparison_a_b_undo_verified":false, "comparison_stored_targets_unchanged":false, "save_as_dual_readbacks_verified":false,
        "current_restored":false, "stored_source23_unchanged":false, "scratch254_restored":false, "scratch255_restored":false,
        "globals_restored_including_changed_latch":false, "changed_latch_reestablished_by_level_undo":false,
        "device_ack_loss_tested":false, "native_gui_clicks_tested":false, "power_cycle_persistence_tested":false,
        "asset_bindings":[], "target_asset_proofs":[], "asset_global_firmware_clear_reset_writes":0,
        "backup_completeness":"Targeted partial acquisition: current, stored23/254/255 and globals. Asset contents unavailable; other253 presets unread.",
    });
    let mut baseline = None;
    let mut store = None;
    let result = (|| {
        lab.input(AppInput::Ui(UiIntent::DiscoverDevices))?;
        lab.wait(|lab| lab.selection.is_some())?;
        lab.phase = "connect";
        lab.connect()?;
        if let Some(loaded) = retained.take() {
            let input = cleanup_from.as_ref().ok_or("Cleanup source missing")?;
            clone_retained_library(
                &input.join("durable-library"),
                &output.join("durable-library"),
            )?;
            let library = LibraryStore::open(output.join("durable-library")).map_err(err)?;
            let stored_origin = library.device_backup(&loaded.backup).map_err(err)?.origin;
            require(
                stored_origin == loaded.origin,
                "Retained backup owner differs",
            )?;
            for snapshot in [loaded.current.clone(), loaded.source.clone()]
                .into_iter()
                .chain(loaded.targets.values().cloned())
            {
                let original = library
                    .load_backup_preset(
                        &loaded.backup,
                        snapshot.target.ok_or("Original target missing")?,
                    )
                    .map_err(err)?;
                require(
                    original.raw == snapshot.raw,
                    "Cloned durable before-image differs from reviewed raw hash",
                )?;
            }
            report["profile"] = "production-inactive-restoration-only-v1".into();
            report["cleanup_only"] = true.into();
            report["retained_negative_receipt_sha256"] = NEGATIVE_RECEIPT_SHA.into();
            report["before_image_backup"] = json!(loaded.backup);
            report["before_images_reopened_exact"] = true.into();
            report["mutation_attempted"] = true.into();
            baseline = Some(loaded);
            store = Some(library);
            return Ok(());
        }
        lab.phase = "baseline-current";
        let current = lab.read(PresetTarget::EditBuffer)?;
        lab.phase = "baseline-source23";
        let source = lab.read(PresetTarget::Stored(slot(SOURCE)))?;
        lab.phase = "baseline-scratch";
        let targets = lab.target_reads()?;
        lab.phase = "baseline-globals";
        let globals = lab.globals()?;
        validate_baseline(&current, &source, &targets, &globals, lab.app.catalog())?;
        report["initial_origin"] = json!(lab.origin()?);
        report["baseline_raw_hashes"] = json!({
            "current":raw_hash(&current.raw), "source23":raw_hash(&source.raw),
            "scratch254":raw_hash(&targets[&slot(254)].raw), "scratch255":raw_hash(&targets[&slot(255)].raw),
            "globals":raw_hash(globals.records().raw()),
        });
        json_new(
            &output.join("baseline-current.snapshot.json"),
            current.as_ref(),
        )?;
        json_new(
            &output.join("baseline-source23.snapshot.json"),
            source.as_ref(),
        )?;
        for n in TARGETS {
            json_new(
                &output.join(format!("baseline-scratch{n}.snapshot.json")),
                targets[&slot(n)].as_ref(),
            )?;
        }
        json_new(&output.join("baseline-globals.json"), globals.as_ref())?;
        let mut library = LibraryStore::open(output.join("durable-library")).map_err(err)?;
        let backup = library.begin_device_backup(lab.origin()?).map_err(err)?;
        library
            .capture_backup_preset(&backup.id, current.clone())
            .map_err(err)?;
        library
            .capture_backup_preset(&backup.id, source.clone())
            .map_err(err)?;
        for snapshot in targets.values() {
            library
                .capture_backup_preset(&backup.id, snapshot.clone())
                .map_err(err)?;
        }
        library
            .capture_backup_globals(&backup.id, &globals)
            .map_err(err)?;
        let backup = library
            .finish_device_backup(&backup.id, BackupAcquisition::Cancelled)
            .map_err(err)?;
        json_new(
            &output.join("partial-backup-before-mutations.json"),
            &backup,
        )?;
        drop(library);
        let library = LibraryStore::open(output.join("durable-library")).map_err(err)?;
        for snapshot in [current.clone(), source.clone()]
            .into_iter()
            .chain(targets.values().cloned())
        {
            let retained = library
                .load_backup_preset(&backup.id, snapshot.target.ok_or("Before target missing")?)
                .map_err(err)?;
            require(
                retained.raw == snapshot.raw,
                "Reopened before-image changed exact raw bits",
            )?;
        }
        report["before_images_reopened_exact"] = true.into();
        report["before_image_backup"] = json!(backup.id);
        baseline = Some(Baseline {
            current,
            source,
            targets,
            globals,
            backup: backup.id,
            origin: lab.origin()?,
        });
        store = Some(library);
        experiment(
            &mut lab,
            baseline.as_ref().unwrap(),
            &mut store,
            &mut report,
        )
    })();
    report["experiment_error"] = json!(result.as_ref().err());
    let restoration = if report["mutation_attempted"] == true {
        cleanup(
            &mut lab,
            baseline.as_ref().ok_or("Mutation without before-images")?,
            &mut store,
            &mut report,
        )
    } else {
        Ok(())
    };
    report["cleanup_error"] = json!(restoration.as_ref().err());
    report["terminal_phase"] = lab.phase.into();
    report["commands"] = lab.commands.clone();
    report["captures"] = json!(lab.captures);
    lab.trace.sync_all().map_err(err)?;
    lab.tx.sync_all().map_err(err)?;
    lab.rx.sync_all().map_err(err)?;
    lab.service.shutdown();
    let stopped = lab.service.wait_stopped(Duration::from_secs(3));
    report["service_stopped"] = stopped.into();
    report["accepted"] =
        (result.is_ok() && restoration.is_ok() && stopped && report["mutation_attempted"] == true)
            .into();
    json_new(&output.join("result.json"), &report)?;
    println!("{report}");
    result.and(restoration)?;
    require(stopped, "Service did not release the owned session")
}
fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            std::process::ExitCode::FAILURE
        }
    }
}

#[cfg(all(test, gp200_private_evidence))]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct Temporary(PathBuf);
    impl Temporary {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "gp200-inactive-probe-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Temporary {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn captured(epoch: u64, revision: u64) -> Arc<PresetSnapshot> {
        Arc::new(PresetSnapshot::from_raw(
            Arc::new(
                RawTransfer::from_payload(
                    0x12,
                    include_bytes!("../../../assets/demo/read-current-payload.bin").to_vec(),
                )
                .unwrap(),
            ),
            SnapshotSource::Hardware {
                epoch: SessionEpoch(epoch),
            },
            Some(PresetTarget::EditBuffer),
            Revision(revision),
        ))
    }
    fn change_record(
        snapshot: &PresetSnapshot,
        command: u16,
        offset: usize,
        value: u8,
    ) -> Arc<PresetSnapshot> {
        let mut records = snapshot.raw.records().to_vec();
        records
            .iter_mut()
            .find(|r| r.command == command)
            .unwrap()
            .body[offset] = value;
        Arc::new(PresetSnapshot::from_raw(
            Arc::new(RawTransfer::from_records(0x12, &records).unwrap()),
            snapshot.source.clone(),
            snapshot.target,
            snapshot.revision,
        ))
    }
    fn global(flag: u16) -> DeviceGlobalState {
        let mut records = RawTransfer::from_payload(
            0x12,
            include_bytes!("../../../tests/fixtures/globals/baseline.payload.bin").to_vec(),
        )
        .unwrap()
        .records()
        .to_vec();
        records
            .iter_mut()
            .find(|r| r.command == 0x1000)
            .unwrap()
            .body[2..4]
            .copy_from_slice(&flag.to_le_bytes());
        DeviceGlobalState::from_transfer(
            Revision(1),
            SessionEpoch(1),
            RawTransfer::from_records(0x12, &records).unwrap(),
            vec![],
        )
        .unwrap()
    }
    fn origin(epoch: u64) -> BackupOrigin {
        BackupOrigin {
            identity: DeviceIdentity {
                model: DeviceModel::Gp200,
                mode: BackendMode::Hardware,
                firmware: None,
                display_name: "Synthetic offline test, no MIDI".into(),
            },
            epoch: SessionEpoch(epoch),
            device_key: "offline-synthetic-owner".into(),
            evidence: vec![EvidenceRef::new(
                "offline test fixture; no physical result",
                EvidenceLevel::Proposed,
            )],
        }
    }
    fn epoch_revision(snapshot: &PresetSnapshot, epoch: u64, revision: u64) -> Arc<PresetSnapshot> {
        let mut copy = snapshot.clone();
        copy.source = SnapshotSource::Hardware {
            epoch: SessionEpoch(epoch),
        };
        copy.revision = Revision(revision);
        Arc::new(copy)
    }
    fn fixture_plan(
        store: &mut LibraryStore,
    ) -> (
        RestoreJournal,
        Arc<PresetSnapshot>,
        BTreeMap<SlotId, Arc<PresetSnapshot>>,
    ) {
        let current = captured(1, 1);
        let source = as_stored(
            &patched(&current, &level_patch(51)).unwrap(),
            slot(SOURCE),
            Revision(2),
        )
        .unwrap();
        let observed = TARGETS
            .into_iter()
            .map(|n| {
                (
                    slot(n),
                    as_stored(&current, slot(n), Revision(3 + u64::from(n))).unwrap(),
                )
            })
            .collect::<BTreeMap<_, _>>();
        let backup = store.begin_device_backup(origin(1)).unwrap();
        store
            .capture_backup_preset(&backup.id, current.clone())
            .unwrap();
        store.capture_backup_preset(&backup.id, source).unwrap();
        for snapshot in observed.values() {
            store
                .capture_backup_preset(&backup.id, snapshot.clone())
                .unwrap();
        }
        store
            .finish_device_backup(&backup.id, BackupAcquisition::Cancelled)
            .unwrap();
        let plan = store
            .plan_restore(&RestoreRequest {
                target: origin(1),
                current_buffer: current.clone(),
                selections: TARGETS
                    .into_iter()
                    .map(|n| RestoreSelection {
                        source: RestoreSource::BackupStored {
                            backup: backup.id.clone(),
                            slot: slot(SOURCE),
                        },
                        target: slot(n),
                    })
                    .collect(),
                observed_targets: observed.clone(),
                asset_bindings: vec![],
                target_assets: vec![],
            })
            .unwrap();
        assert_eq!(plan.entries.len(), 2);
        assert!(plan.excluded.is_empty());
        let review = RestoreReview {
            plan_hash: plan.review_hash().unwrap(),
            accept_unknown_firmware: true,
        };
        (
            store.begin_restore(plan, review).unwrap(),
            current,
            observed,
        )
    }

    #[test]
    fn recorded_root_guard_current_source_and_globals_pass_the_scoped_preflight() {
        // Actual root read-only capture inputs; scratch254 here is a synthetic
        // retarget of captured255. Execution still freshly reads both real slots.
        let read = |bytes: &[u8], target| {
            Arc::new(PresetSnapshot::from_raw(
                Arc::new(RawTransfer::from_payload(0x12, bytes.to_vec()).unwrap()),
                SnapshotSource::Hardware {
                    epoch: SessionEpoch(1),
                },
                Some(target),
                Revision(1),
            ))
        };
        let current = read(
            include_bytes!(
                "../../../analysis/implementation/shared-session-guard-hardware/current-before.payload.bin"
            ),
            PresetTarget::EditBuffer,
        );
        let source = read(
            include_bytes!(
                "../../../analysis/implementation/shared-session-guard-hardware/source-before.payload.bin"
            ),
            PresetTarget::Stored(slot(SOURCE)),
        );
        let scratch = read(
            include_bytes!(
                "../../../analysis/implementation/shared-session-guard-hardware/scratch-before.payload.bin"
            ),
            PresetTarget::Stored(slot(255)),
        );
        let globals = DeviceGlobalState::from_transfer(Revision(1), SessionEpoch(1), RawTransfer::from_payload(0x12,
            include_bytes!("../../../analysis/implementation/shared-session-guard-hardware/globals-before.payload.bin").to_vec()).unwrap(), vec![]).unwrap();
        let targets = BTreeMap::from([
            (
                slot(254),
                as_stored(&scratch, slot(254), Revision(2)).unwrap(),
            ),
            (slot(255), scratch),
        ]);
        validate_baseline(
            &current,
            &source,
            &targets,
            &globals,
            &Catalog::embedded().unwrap(),
        )
        .unwrap();
        assert_eq!(position(&current).unwrap(), F32Bits(0x42c6_b5c9));
        assert_eq!(position(&source).unwrap(), F32Bits::from_f32(100.0));
    }

    #[test]
    fn stale_retired_request_failures_do_not_abort_a_fresh_cleanup_connection() {
        let event = DeviceEvent::OperationFailed {
            stamp: RequestStamp {
                epoch: SessionEpoch(1),
                intent: IntentId(3),
            },
            error: ServiceError::new(
                ErrorKind::Cancelled,
                "Prior request retired by explicit reconnect",
            ),
        };
        assert!(request_failure(&event, &BTreeSet::from([IntentId(4)])).is_none());
        assert!(request_failure(&event, &BTreeSet::from([IntentId(3)])).is_some());
    }

    fn controller_with_current() -> AppController {
        let mut app = AppController::new(Catalog::embedded().unwrap(), AppPreferences::default());
        let effects = app.dispatch(AppInput::Ui(UiIntent::Connect(DeviceSelection {
            input_id: "offline-simulator-in".into(),
            output_id: "offline-simulator-out".into(),
            fingerprint_hint: None,
        })));
        let Effect::Device(DeviceCommand::Connect { intent, .. }) = effects[0] else {
            panic!("Connect expected");
        };
        app.dispatch(AppInput::Device(DeviceEvent::ConnectionChanged {
            epoch: SessionEpoch(1),
            state: ConnectionState::Connecting,
        }));
        app.dispatch(AppInput::Device(DeviceEvent::SessionChanged {
            epoch: SessionEpoch(1),
            identity: DeviceIdentity {
                model: DeviceModel::Gp200,
                mode: BackendMode::Simulator,
                firmware: None,
                display_name: "Explicit offline reducer fixture".into(),
            },
            capabilities: Capabilities::simulator(),
        }));
        app.dispatch(AppInput::Device(DeviceEvent::ConnectionChanged {
            epoch: SessionEpoch(1),
            state: ConnectionState::Ready,
        }));
        app.dispatch(AppInput::Device(DeviceEvent::SnapshotReceived {
            stamp: RequestStamp {
                epoch: SessionEpoch(1),
                intent,
            },
            target: PresetTarget::EditBuffer,
            snapshot: captured(1, 1),
        }));
        app
    }
    fn reconnect_fixture(app: &mut AppController, epoch: u64, current: Arc<PresetSnapshot>) {
        let effects = app.dispatch(AppInput::Ui(UiIntent::Connect(DeviceSelection {
            input_id: "offline-in".into(),
            output_id: "offline-out".into(),
            fingerprint_hint: None,
        })));
        let Effect::Device(DeviceCommand::Connect { intent, .. }) = effects[0] else {
            panic!();
        };
        app.dispatch(AppInput::Device(DeviceEvent::ConnectionChanged {
            epoch: SessionEpoch(epoch),
            state: ConnectionState::Connecting,
        }));
        app.dispatch(AppInput::Device(DeviceEvent::SessionChanged {
            epoch: SessionEpoch(epoch),
            identity: DeviceIdentity {
                model: DeviceModel::Gp200,
                mode: BackendMode::Simulator,
                firmware: None,
                display_name: "Offline cleanup regression".into(),
            },
            capabilities: Capabilities::simulator(),
        }));
        app.dispatch(AppInput::Device(DeviceEvent::ConnectionChanged {
            epoch: SessionEpoch(epoch),
            state: ConnectionState::Ready,
        }));
        app.dispatch(AppInput::Device(DeviceEvent::SnapshotReceived {
            stamp: RequestStamp {
                epoch: SessionEpoch(epoch),
                intent,
            },
            target: PresetTarget::EditBuffer,
            snapshot: epoch_revision(&current, epoch, 5),
        }));
    }
    #[test]
    fn probe_owned_dirty_document_is_retired_before_cleanup_selection() {
        let mut experiment = controller_with_current();
        let effects = experiment.dispatch(AppInput::Ui(UiIntent::ApplyEdit {
            document: device_document_id(),
            patch: level_patch(51),
        }));
        finish_controller_edit(&mut experiment, effects);
        let current255 = as_current(&captured(1, 1), slot(255)).unwrap();
        reconnect_fixture(&mut experiment, 2, current255.clone());
        assert!(
            experiment.state().documents.values().any(|d| d.dirty()),
            "Normal reconnect must retain dirty user documents"
        );
        let preserved = experiment.state().documents.len();
        let mut cleanup =
            AppController::new(Catalog::embedded().unwrap(), AppPreferences::default());
        reconnect_fixture(&mut cleanup, 3, current255);
        assert!(cleanup.state().documents.values().all(|d| !d.dirty()));
        assert!(cleanup.state().documents.len() < preserved);
        let effects = cleanup.dispatch(AppInput::Ui(UiIntent::AuditionStored(slot(SOURCE))));
        assert!(
            matches!(
                effects.as_slice(),
                [Effect::Device(DeviceCommand::SelectStored { .. })]
            ),
            "Fresh probe-only cleanup controller must permit an explicit source selection"
        );
        // The actor still separately requires a freshly read stored source and
        // verifies the requested current context; these effects grant no MIDI authority.
    }
    #[test]
    fn actual_refresh_padding_stays_clean_and_permits_explicit_selection() {
        let snapshot = |bytes: &[u8], target, revision| {
            Arc::new(PresetSnapshot::from_raw(
                Arc::new(RawTransfer::from_payload(0x12, bytes.to_vec()).unwrap()),
                SnapshotSource::Hardware {
                    epoch: SessionEpoch(3),
                },
                Some(target),
                Revision(revision),
            ))
        };
        let connected = snapshot(
            include_bytes!(
                "../../../analysis/implementation/inactive-restore-recovery/probe/cleanup-fresh-session-24-preset.payload.bin"
            ),
            PresetTarget::EditBuffer,
            5,
        );
        let refreshed = snapshot(
            include_bytes!(
                "../../../analysis/implementation/inactive-restore-recovery/probe/cleanup-fresh-session-28-preset.payload.bin"
            ),
            PresetTarget::EditBuffer,
            6,
        );
        assert!(equal(&connected, &refreshed));
        assert_ne!(connected.raw, refreshed.raw);
        let mut app = AppController::new(Catalog::embedded().unwrap(), AppPreferences::default());
        reconnect_fixture(&mut app, 3, connected);
        assert!(!app.state().documents[&device_document_id()].dirty());
        let effects = app.dispatch(AppInput::Ui(UiIntent::Refresh(PresetTarget::EditBuffer)));
        let [Effect::Device(DeviceCommand::RefreshTarget { stamp, target })] = effects.as_slice()
        else {
            panic!("Read expected");
        };
        app.dispatch(AppInput::Device(DeviceEvent::SnapshotReceived {
            stamp: *stamp,
            target: *target,
            snapshot: refreshed.clone(),
        }));
        assert!(!app.state().documents[&device_document_id()].dirty());
        assert!(matches!(
            app.dispatch(AppInput::Ui(UiIntent::AuditionStored(slot(SOURCE))))
                .as_slice(),
            [Effect::Device(DeviceCommand::SelectStored { .. })]
        ));

        // This is an offline reducer sequence using actual recorded bytes. The
        // fresh controller's Connect current is its baseline; only a stored
        // read intervenes before selection. Actor authority remains separate.
        app = AppController::new(Catalog::embedded().unwrap(), AppPreferences::default());
        reconnect_fixture(&mut app, 4, refreshed);
        let source = snapshot(
            include_bytes!(
                "../../../analysis/implementation/inactive-restore-recovery/probe/cleanup-select-source23-35-preset.payload.bin"
            ),
            PresetTarget::Stored(slot(SOURCE)),
            7,
        );
        let effects = app.dispatch(AppInput::Ui(UiIntent::Refresh(PresetTarget::Stored(slot(
            SOURCE,
        )))));
        let [Effect::Device(DeviceCommand::RefreshTarget { stamp, target })] = effects.as_slice()
        else {
            panic!("Stored read expected");
        };
        app.dispatch(AppInput::Device(DeviceEvent::SnapshotReceived {
            stamp: *stamp,
            target: *target,
            snapshot: epoch_revision(&source, 4, 7),
        }));
        assert!(!app.state().documents[&device_document_id()].dirty());
        assert!(matches!(
            app.dispatch(AppInput::Ui(UiIntent::AuditionStored(slot(SOURCE))))
                .as_slice(),
            [Effect::Device(DeviceCommand::SelectStored { .. })]
        ));
    }
    #[test]
    fn retained_negative_baseline_and_cloned_library_are_checked_without_midi() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../analysis/implementation/inactive-restore-hardware/probe");
        let root = fs::canonicalize(root).unwrap();
        let baseline = retained_baseline(&root).unwrap();
        assert_eq!(baseline.origin.device_key, "usb-84ef-002a-20220118");
        let temp = Temporary::new();
        clone_retained_library(&root.join("durable-library"), &temp.0.join("clone")).unwrap();
        let library = LibraryStore::open(temp.0.join("clone")).unwrap();
        assert_eq!(
            library
                .load_backup_preset(&baseline.backup, PresetTarget::Stored(slot(254)))
                .unwrap()
                .raw,
            baseline.targets[&slot(254)].raw
        );
    }
    #[test]
    fn retained_input_bounds_symlinks_hash_names_and_receipt_tampering_are_rejected() {
        use std::os::unix::fs::symlink;
        let temp = Temporary::new();
        write_new(&temp.0.join("large"), &[0; 1025]).unwrap();
        assert!(bounded_file(&temp.0.join("large"), 1024).is_err());
        symlink(temp.0.join("large"), temp.0.join("alias")).unwrap();
        assert!(bounded_file(&temp.0.join("alias"), 2048).is_err());
        write_new(&temp.0.join("result.json"), b"{\"schema\":1}").unwrap();
        assert!(retained_baseline(&temp.0).is_err());
        let source = temp.0.join("library");
        fresh_directory(&source).unwrap();
        fresh_directory(&source.join("blobs")).unwrap();
        write_new(&source.join("blobs").join("0".repeat(64)), b"corrupt blob").unwrap();
        assert!(clone_retained_library(&source, &temp.0.join("invalid-clone")).is_err());
    }
    fn finish_controller_edit(app: &mut AppController, effects: Vec<Effect>) {
        assert_eq!(effects.len(), 1);
        let Effect::Device(DeviceCommand::SubmitEdit {
            stamp,
            target,
            patch,
            ..
        }) = &effects[0]
        else {
            panic!("A typed buffer edit is expected");
        };
        assert_eq!(*target, PresetTarget::EditBuffer);
        let before = app.state().confirmed_buffer.as_ref().unwrap();
        let actual = epoch_revision(&patched(before, patch).unwrap(), 1, before.revision.0 + 1);
        app.dispatch(AppInput::Device(DeviceEvent::EditProgress {
            stamp: *stamp,
            stage: EditStage::Sent,
        }));
        app.dispatch(AppInput::Device(DeviceEvent::SnapshotReceived {
            stamp: *stamp,
            target: *target,
            snapshot: actual,
        }));
        assert!(
            app.dispatch(AppInput::Device(DeviceEvent::EditProgress {
                stamp: *stamp,
                stage: EditStage::Verified
            }))
            .is_empty()
        );
        assert!(!app.device_operation_pending());
    }
    #[test]
    fn production_comparison_sequence_undo_returns_a_and_never_emits_stored_commands() {
        let mut app = controller_with_current();
        let id = device_document_id();
        let baseline = app.state().confirmed_buffer.clone().unwrap();
        app.capture_comparison(&id, ComparisonSlot::A).unwrap();
        let effects = app.dispatch(AppInput::Ui(UiIntent::ApplyEdit {
            document: id.clone(),
            patch: level_patch(51),
        }));
        finish_controller_edit(&mut app, effects);
        app.capture_comparison(&id, ComparisonSlot::B).unwrap();
        let effects = app.recall_comparison(&id, ComparisonSlot::A);
        finish_controller_edit(&mut app, effects);
        assert!(equal(
            &baseline,
            app.state().confirmed_buffer.as_ref().unwrap()
        ));
        let effects = app.recall_comparison(&id, ComparisonSlot::B);
        finish_controller_edit(&mut app, effects);
        assert_eq!(
            app.state()
                .confirmed_buffer
                .as_ref()
                .unwrap()
                .view
                .metadata
                .level,
            Some(51)
        );
        let effects = app.dispatch(AppInput::Ui(UiIntent::Undo(id.clone())));
        finish_controller_edit(&mut app, effects);
        assert!(equal(
            &baseline,
            app.state().confirmed_buffer.as_ref().unwrap()
        ));
        assert!(app.state().bank.slots.is_empty());
        let effects = app.dispatch(AppInput::Ui(UiIntent::WriteStoredPreset {
            slot: slot(255),
            expected_target_revision: Revision(999),
            snapshot: baseline,
        }));
        assert!(
            effects.is_empty(),
            "An imported snapshot does not establish a read target or write authority"
        );
    }

    #[test]
    fn physical_preflight_requires_changed_latch_one_and_only_position_overlay() {
        let current = captured(1, 1);
        let source = as_stored(&current, slot(SOURCE), Revision(2)).unwrap();
        let targets = TARGETS
            .into_iter()
            .map(|n| (slot(n), as_stored(&current, slot(n), Revision(3)).unwrap()))
            .collect();
        let catalog = Catalog::embedded().unwrap();
        validate_baseline(&current, &source, &targets, &global(1), &catalog).unwrap();
        validate_baseline(
            &with_position(&current, F32Bits(0x42c6_b5c9)).unwrap(),
            &source,
            &targets,
            &global(1),
            &catalog,
        )
        .unwrap();
        for flag in [0, 2, u16::MAX] {
            assert!(
                validate_baseline(&current, &source, &targets, &global(flag), &catalog).is_err()
            );
        }
        assert!(
            validate_baseline(
                &patched(&current, &level_patch(51)).unwrap(),
                &source,
                &targets,
                &global(1),
                &catalog
            )
            .is_err()
        );
        let hidden = change_record(&current, 0x14, 67, 0xff);
        assert!(validate_baseline(&hidden, &source, &targets, &global(1), &catalog).is_err());
        assert!(
            validate_baseline(&current, &source, &BTreeMap::new(), &global(1), &catalog).is_err()
        );
    }

    #[test]
    fn raw_user_asset_cannot_hide_behind_a_forged_factory_projection() {
        let current = captured(1, 1);
        let mut records = current.raw.records().to_vec();
        records
            .iter_mut()
            .find(|r| r.command == 0x14 && r.body[0] == 5)
            .unwrap()
            .body[4..8]
            .copy_from_slice(&0x0a10_0000_u32.to_le_bytes());
        let mut forged = current.as_ref().clone();
        forged.raw = Arc::new(RawTransfer::from_records(0x12, &records).unwrap());
        assert!(
            forged
                .asset_refs
                .iter()
                .all(|r| matches!(r, AssetRef::Factory { .. }))
        );
        assert!(factory_only(&forged, &Catalog::embedded().unwrap()).is_err());
    }

    #[test]
    fn comparison_is_strict_for_unknown_bytes_f32_bits_and_wrong_context() {
        let current = captured(1, 1);
        let fractional = with_position(&current, F32Bits(0x42c6_b5c9)).unwrap();
        let next_bit = with_position(&current, F32Bits(0x42c6_b5ca)).unwrap();
        assert!(!equal(&fractional, &next_bit));
        assert!(overlay_only(&fractional, &next_bit).unwrap());
        assert!(!equal(&current, &change_record(&current, 0x14, 67, 1)));
        assert!(!equal(&current, &as_current(&current, slot(255)).unwrap()));
        let nan = with_position(&current, F32Bits(0x7fc0_0123)).unwrap();
        assert!(position(&nan).is_err());
    }

    #[test]
    fn two_slot_crash_reconciliation_never_redispatches_verified_entries() {
        let root = Temporary::new();
        let mut store = LibraryStore::open(root.0.join("store")).unwrap();
        let (journal, current, before) = fixture_plan(&mut store);
        let first = store
            .begin_restore_entry(
                &journal.id,
                slot(254),
                before[&slot(254)].clone(),
                current.clone(),
            )
            .unwrap();
        let first_actual = epoch_revision(&first.snapshot, 1, before[&slot(254)].revision.0 + 1);
        store
            .confirm_restore_readback(&journal.id, first_actual.clone())
            .unwrap();
        let second = store
            .begin_restore_entry(&journal.id, slot(255), before[&slot(255)].clone(), current)
            .unwrap();
        // The second dispatched write was applied, but this test intentionally
        // loses the consumer confirmation and simulates a process restart.
        drop(store);
        let mut store = LibraryStore::open(root.0.join("store")).unwrap();
        let reopened = store.restore_journal(&journal.id).unwrap();
        assert_eq!(reopened.verified_count(), 1);
        assert!(matches!(reopened.state, RestoreRunState::Paused { .. }));
        assert!(matches!(
            reopened.entries[1].state,
            RestoreEntryState::Uncertain { .. }
        ));
        let readbacks = BTreeMap::from([
            (slot(254), epoch_revision(&first.snapshot, 2, 1)),
            (slot(255), epoch_revision(&second.snapshot, 2, 2)),
        ]);
        let complete = store
            .reconcile_restore(&journal.id, origin(2), readbacks.clone())
            .unwrap();
        assert_eq!(complete.state, RestoreRunState::Completed);
        assert_eq!(complete.verified_count(), 2);
        assert_eq!(
            complete
                .entries
                .iter()
                .map(|e| e.attempts)
                .collect::<Vec<_>>(),
            vec![1, 1]
        );
        assert!(
            store
                .begin_restore_entry(
                    &journal.id,
                    slot(255),
                    readbacks[&slot(255)].clone(),
                    captured(2, 3)
                )
                .is_err()
        );
        assert_eq!(
            store.restore_journal(&journal.id).unwrap().entries[1].attempts,
            1
        );
        assert_eq!(first_actual.raw, first.snapshot.raw);
    }

    #[test]
    fn stale_readback_and_unapplied_cleanup_need_exact_fresh_review() {
        let root = Temporary::new();
        let mut store = LibraryStore::open(root.0.join("store")).unwrap();
        let (journal, current, before) = fixture_plan(&mut store);
        let dispatch = store
            .begin_restore_entry(
                &journal.id,
                slot(254),
                before[&slot(254)].clone(),
                current.clone(),
            )
            .unwrap();
        assert!(
            store
                .confirm_restore_readback(&journal.id, dispatch.snapshot.clone())
                .is_err()
        );
        assert!(
            store
                .begin_restore_entry(&journal.id, slot(255), before[&slot(255)].clone(), current)
                .is_err()
        );
        store
            .pause_restore(&journal.id, "Offline unsent/lost-result test")
            .unwrap();
        let readbacks = before
            .iter()
            .map(|(slot, s)| (*slot, epoch_revision(s, 2, 5)))
            .collect();
        let paused = store
            .reconcile_restore(&journal.id, origin(2), readbacks)
            .unwrap();
        assert_eq!(paused.entries[0].state, RestoreEntryState::NeedsRetryReview);
        assert!(
            store
                .approve_restore_retry(&journal.id, slot(254), ContentHash::of(b"wrong review"))
                .is_err()
        );
        store
            .approve_restore_retry(
                &journal.id,
                slot(254),
                paused.entries[0].observed_raw_hash.clone(),
            )
            .unwrap();
        let wrong_epoch = before[&slot(254)].clone();
        assert!(
            store
                .begin_restore_entry(&journal.id, slot(254), wrong_epoch, captured(2, 6))
                .is_err()
        );
        assert_eq!(
            store.restore_journal(&journal.id).unwrap().entries[0].attempts,
            1
        );
    }

    #[test]
    fn final_global_comparison_keeps_changed_latch_and_other_runtime_words_strict() {
        assert!(!global(1).records().control_state_equal(global(0).records()));
        let mut records = global(1).records().raw().records().to_vec();
        records
            .iter_mut()
            .find(|r| r.command == 0x1000)
            .unwrap()
            .body[4] ^= 1;
        let unknown = DeviceGlobalState::from_transfer(
            Revision(2),
            SessionEpoch(1),
            RawTransfer::from_records(0x12, &records).unwrap(),
            vec![],
        )
        .unwrap();
        assert!(!global(1).records().control_state_equal(unknown.records()));
        assert!(latch(&global(1)).unwrap());
    }

    #[test]
    fn evidence_publication_refuses_existing_files_and_symlinked_ancestors() {
        use std::os::unix::fs::symlink;
        let root = Temporary::new();
        let path = root.0.join("evidence");
        fresh_directory(&path).unwrap();
        write_new(&path.join("before.bin"), b"original").unwrap();
        assert!(write_new(&path.join("before.bin"), b"changed").is_err());
        assert_eq!(fs::read(path.join("before.bin")).unwrap(), b"original");
        assert!(fresh_directory(&path).is_err());
        symlink(&path, root.0.join("alias")).unwrap();
        assert!(fresh_directory(&root.0.join("alias").join("new")).is_err());
    }
}
