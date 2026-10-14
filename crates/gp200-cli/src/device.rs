//! Only typed read commands reach the production actor. No raw-send interface.
use crate::{Options, files};
use gp200_domain::*;
use gp200_library::{BackupAcquisition, BackupOrigin, LibraryStore};
use gp200_transport::{MidiDeviceService, TransportConfig};
use serde_json::{Value, json};
use std::{
    fs::{self, File, OpenOptions},
    path::Path,
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

fn lock_file(path: &Path) -> Result<File, ServiceError> {
    files::reject_links(path)?;
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path).map_err(files::io)?;
    file.try_lock().map_err(|_| ServiceError::new(ErrorKind::Busy, "Another GP200 session owns the hardware lock; close it before running a device command"))?;
    Ok(file)
}
struct HardwareLocks {
    _workspace: File,
    _shared: File,
}
impl HardwareLocks {
    fn acquire() -> Result<Self, ServiceError> {
        let workspace = std::env::current_dir()
            .map_err(files::io)?
            .join(".hardware-session.lock");
        let root = gp200_library::data_dir()?;
        files::reject_links(&root)?;
        fs::create_dir_all(&root).map_err(files::io)?;
        let shared = root.join("hardware-session.lock");
        // Same-directory root lab and all supported CLI sessions coordinate.
        // Shared data-root lock also coordinates CLI processes in different directories.
        let workspace_lock = lock_file(&workspace)?;
        let shared_lock = if shared == workspace {
            return Err(ServiceError::invalid("Hardware lock paths coincide"));
        } else {
            lock_file(&shared)?
        };
        Ok(Self {
            _workspace: workspace_lock,
            _shared: shared_lock,
        })
    }
}
struct Session {
    service: MidiDeviceService,
    epoch: SessionEpoch,
    identity: Option<DeviceIdentity>,
    capabilities: Option<Capabilities>,
    current: Option<Arc<PresetSnapshot>>,
    timeout: Duration,
    next_intent: u64,
}
impl Drop for Session {
    fn drop(&mut self) {
        self.service.shutdown();
        self.service.wait_stopped(Duration::from_secs(2));
    }
}
impl Session {
    fn new(timeout: Duration) -> Result<Self, ServiceError> {
        #[cfg(target_os = "linux")]
        let (backend, config): (Box<dyn gp200_transport::TransportBackend>, _) = (
            Box::new(gp200_transport::AlsaRawBackend::new()),
            TransportConfig::alsa_raw(),
        );
        #[cfg(not(target_os = "linux"))]
        let (backend, config): (Box<dyn gp200_transport::TransportBackend>, _) = (
            Box::new(gp200_transport::MidirBackend::new()),
            TransportConfig::default(),
        );
        Ok(Self {
            service: MidiDeviceService::try_with_backend(backend, config, no_wake())?,
            epoch: SessionEpoch(0),
            identity: None,
            capabilities: None,
            current: None,
            timeout,
            next_intent: 2,
        })
    }
    fn submit(&self, command: DeviceCommand) -> Result<(), ServiceError> {
        self.service.try_submit(command).map_err(|e| match e {
            SubmitError::Busy(_) => {
                ServiceError::new(ErrorKind::Busy, "Device command queue is full")
            }
            SubmitError::Disconnected(_) => {
                ServiceError::new(ErrorKind::Disconnected, "Device service stopped")
            }
        })
    }
    fn stamp(&mut self) -> RequestStamp {
        let intent = self.next_intent;
        self.next_intent += 1;
        RequestStamp {
            epoch: self.epoch,
            intent: IntentId(intent),
        }
    }
    fn wait(
        &mut self,
        intent: Option<IntentId>,
        accepts: impl Fn(&DeviceEvent) -> bool,
    ) -> Result<DeviceEvent, ServiceError> {
        let deadline = Instant::now() + self.timeout;
        while Instant::now() < deadline {
            if let Some(event) = self.service.try_next_event() {
                match &event {
                    DeviceEvent::ConnectionChanged { epoch, .. } => self.epoch = *epoch,
                    DeviceEvent::SessionChanged {
                        epoch,
                        identity,
                        capabilities,
                    } => {
                        self.epoch = *epoch;
                        self.identity = Some(identity.clone());
                        self.capabilities = Some(capabilities.clone());
                    }
                    DeviceEvent::SnapshotReceived {
                        target: PresetTarget::EditBuffer,
                        snapshot,
                        ..
                    } => self.current = Some(snapshot.clone()),
                    DeviceEvent::OperationFailed { stamp, error }
                        if intent.is_none() || intent == Some(stamp.intent) =>
                    {
                        return Err(error.clone());
                    }
                    _ => {}
                }
                if accepts(&event) {
                    return Ok(event);
                }
            } else {
                if self.service.stopped() {
                    return Err(ServiceError::new(
                        ErrorKind::Disconnected,
                        "Device service stopped before the requested reply",
                    ));
                }
                thread::sleep(Duration::from_millis(2));
            }
        }
        Err(ServiceError::new(
            ErrorKind::Timeout,
            "Device command deadline exceeded; no partial result was accepted",
        ))
    }
    fn connect(&mut self, selection: DeviceSelection) -> Result<(), ServiceError> {
        self.submit(DeviceCommand::Connect {
            intent: IntentId(1),
            selection,
        })?;
        self.wait(Some(IntentId(1)), |e| {
            matches!(
                e,
                DeviceEvent::ConnectionChanged {
                    state: ConnectionState::Ready,
                    ..
                }
            )
        })?;
        if self
            .identity
            .as_ref()
            .is_none_or(|i| i.mode != BackendMode::Hardware || i.model != DeviceModel::Gp200)
            || self.current.is_none()
        {
            return Err(ServiceError::new(
                ErrorKind::Unverified,
                "A complete physical GP-200 normal-mode session was not established",
            ));
        }
        Ok(())
    }
    fn stored(&mut self, slot: SlotId) -> Result<Arc<PresetSnapshot>, ServiceError> {
        let stamp = self.stamp();
        self.submit(DeviceCommand::ReadStored { stamp, slot })?;
        match self.wait(Some(stamp.intent), |e| matches!(e, DeviceEvent::SnapshotReceived { stamp: reply, target: PresetTarget::Stored(s), .. } if *reply == stamp && *s == slot))? {
            DeviceEvent::SnapshotReceived { snapshot, .. } => Ok(snapshot), _ => unreachable!(),
        }
    }
    fn globals(&mut self) -> Result<Arc<DeviceGlobalState>, ServiceError> {
        let stamp = self.stamp();
        self.submit(DeviceCommand::ReadGlobals(stamp))?;
        match self.wait(Some(stamp.intent), |e| matches!(e, DeviceEvent::GlobalStateReceived { stamp: reply, .. } if *reply == stamp))? {
            DeviceEvent::GlobalStateReceived { state, .. } => Ok(state), _ => unreachable!(),
        }
    }
    fn ir_metadata(&mut self, index: u16) -> Result<Arc<DeviceAssetMetadata>, ServiceError> {
        let stamp = self.stamp();
        let slot = AssetSlot::new(AssetBank::Ir, index)
            .map_err(|e| ServiceError::invalid(e.to_string()))?;
        self.submit(DeviceCommand::ReadAssetMetadata { stamp, slot })?;
        match self.wait(Some(stamp.intent), |e| matches!(e, DeviceEvent::AssetMetadataReceived { stamp: reply, metadata } if *reply == stamp && metadata.slot() == slot))? {
            DeviceEvent::AssetMetadataReceived { metadata, .. } => Ok(metadata), _ => unreachable!(),
        }
    }
}
fn choose_pair(
    options: &Options,
    ports: &[PortPairCandidate],
) -> Result<DeviceSelection, ServiceError> {
    if options.has("--unique") {
        if options.has("--input-port") || options.has("--output-port") {
            return Err(ServiceError::invalid(
                "Choose --unique or two exact port IDs, not both",
            ));
        }
        let candidates = ports
            .iter()
            .filter(|pair| {
                let name = pair.display_name.to_ascii_uppercase();
                name.contains("GP-200")
                    || name.contains("GP200")
                    || pair
                        .selection
                        .fingerprint_hint
                        .as_ref()
                        .is_some_and(|f| f.vendor == Some(0x84ef) && f.product == Some(0x002a))
            })
            .collect::<Vec<_>>();
        if candidates.len() != 1 {
            return Err(ServiceError::new(
                ErrorKind::Conflict,
                format!(
                    "Found {} GP-200 candidate pairs; use device ports and select exact input/output IDs",
                    candidates.len()
                ),
            ));
        }
        return Ok(candidates[0].selection.clone());
    }
    let input = options.required_text("--input-port")?;
    let output = options.required_text("--output-port")?;
    if input.len() > 1024 || output.len() > 1024 {
        return Err(ServiceError::invalid("Port ID exceeds its bound"));
    }
    let candidates = ports
        .iter()
        .filter(|p| p.selection.input_id == input && p.selection.output_id == output)
        .collect::<Vec<_>>();
    if candidates.len() != 1 {
        return Err(ServiceError::new(
            ErrorKind::Conflict,
            "Exact input/output pair is absent or ambiguous; rediscover ports",
        ));
    }
    Ok(candidates[0].selection.clone())
}
pub(crate) fn run(options: &Options) -> Result<Value, ServiceError> {
    let timeout = Duration::from_secs(options.number("--timeout-seconds", 15, 1, 60)?);
    if options.action != "ports" {
        if !options.has("--unique") {
            options.required_text("--input-port")?;
            options.required_text("--output-port")?;
        }
        if options.has("--unique") && (options.has("--input-port") || options.has("--output-port"))
        {
            return Err(ServiceError::invalid("Choose --unique or exact port IDs"));
        }
    }
    let slot = if matches!(options.action.as_str(), "stored" | "ir-metadata") {
        options.required_text("--slot")?;
        Some(options.number(
            "--slot",
            0,
            0,
            if options.action == "ir-metadata" {
                19
            } else {
                255
            },
        )? as u16)
    } else {
        None
    };
    let output = options.optional_path("--output");
    if let Some(path) = &output {
        files::fresh_output(path)?;
    }
    if matches!(options.action.as_str(), "identity" | "doctor") && output.is_some() {
        return Err(ServiceError::invalid(
            "This command has no raw-payload export; omit --output",
        ));
    }
    let mut store = if options.action == "bank" {
        Some(LibraryStore::open(crate::offline::store_path(options)?)?)
    } else {
        None
    };
    let _locks = HardwareLocks::acquire()?;
    let mut session = Session::new(timeout)?;
    session.submit(DeviceCommand::Discover)?;
    let ports = match session.wait(None, |e| matches!(e, DeviceEvent::PortsChanged(_)))? {
        DeviceEvent::PortsChanged(ports) => ports,
        _ => unreachable!(),
    };
    if options.action == "ports" {
        return Ok(json!({"ports": ports, "pedal_ports_opened": false, "pedal_mutations": 0}));
    }
    let selection = choose_pair(options, &ports)?;
    session.connect(selection.clone())?;
    let identity = session.identity.clone().expect("checked complete session");
    let base = json!({"identity": identity, "selection": selection, "epoch": session.epoch, "capabilities": session.capabilities, "read_only": true, "pedal_mutations": 0, "firmware_note": "Connected firmware remains unknown unless observed; an image version is not installed firmware"});
    let mut result = base.as_object().expect("object").clone();
    match options.action.as_str() {
        "identity" => {}
        "doctor" => {
            result.insert("normal_mode_ready".into(), json!(true));
            result.insert("identity_and_current_verified".into(), json!(true));
            result.insert(
                "installed_firmware_known".into(),
                json!(
                    session
                        .identity
                        .as_ref()
                        .is_some_and(|i| i.firmware.is_some())
                ),
            );
            result.insert("diagnosis".into(), json!("Selected GP-200 identified and synchronized through the production normal-mode actor"));
            result.insert("network_requests".into(), json!(0));
        }
        "current" | "stored" => {
            let snapshot = if let Some(slot) = slot {
                session.stored(SlotId::new(slot).map_err(files::invalid)?)?
            } else {
                session.current.clone().expect("checked complete session")
            };
            if let Some(path) = output {
                files::write_new(&path, snapshot.raw.payload())?;
            }
            result.insert(
                "payload_sha256".into(),
                json!(files::sha256(snapshot.raw.payload())),
            );
            result.insert("snapshot".into(), json!(snapshot));
        }
        "globals" => {
            let globals = session.globals()?;
            if let Some(path) = output {
                files::write_new(&path, globals.records().raw().payload())?;
            }
            result.insert(
                "payload_sha256".into(),
                json!(files::sha256(globals.records().raw().payload())),
            );
            result.insert("globals".into(), json!(globals));
        }
        "ir-metadata" => {
            let metadata = session.ir_metadata(slot.expect("validated slot"))?;
            if let Some(path) = output {
                files::write_new(&path, metadata.raw().payload())?;
            }
            result.insert("metadata".into(), json!(metadata.metadata()));
            result.insert("content_captured".into(), json!(false));
            result.insert("occupancy_proven".into(), json!(false));
        }
        "bank" => {
            let store = store.as_mut().expect("bank store");
            let backup = store.begin_device_backup(BackupOrigin {
                identity,
                epoch: session.epoch,
                device_key: serde_json::to_string(&selection).map_err(files::invalid)?,
                evidence: vec![EvidenceRef::new(
                    "Production read-only CLI session",
                    EvidenceLevel::LiveVerified,
                )],
            })?;
            let acquired = (|| {
                store.capture_backup_preset(
                    &backup.id,
                    session.current.clone().expect("checked complete session"),
                )?;
                for index in 0..256 {
                    let snapshot = session.stored(SlotId::new(index).map_err(files::invalid)?)?;
                    store.capture_backup_preset(&backup.id, snapshot)?;
                }
                let globals = session.globals()?;
                store.capture_backup_globals(&backup.id, &globals)?;
                for index in 0..20 {
                    let metadata = session.ir_metadata(index)?;
                    store.capture_backup_asset_metadata(
                        &backup.id,
                        session.epoch,
                        metadata.metadata(),
                    )?;
                }
                for index in 0..10 {
                    store.record_backup_read_error(
                        &backup.id,
                        &format!("snaptone-{index:02}"),
                        "Metadata read is not verified for this device profile; no query sent",
                    )?;
                }
                Ok::<(), ServiceError>(())
            })();
            if let Err(mut error) = acquired {
                let finish = store.finish_device_backup(
                    &backup.id,
                    BackupAcquisition::Failed {
                        reason: error.message.clone(),
                    },
                );
                error.message = format!(
                    "{}; partial acquisition {} retained{}",
                    error.message,
                    backup.id,
                    if finish.is_err() {
                        "; terminal journal update failed"
                    } else {
                        ""
                    }
                );
                return Err(error);
            }
            let backup = store.finish_device_backup(&backup.id, BackupAcquisition::Finished)?;
            result.insert("completeness".into(), json!(backup.completeness()));
            result.insert("backup".into(), json!(backup));
            result.insert("complete_device_backup".into(), json!(false));
        }
        _ => return Err(ServiceError::invalid("Unknown device command")),
    }
    Ok(Value::Object(result))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn options(args: &[&str]) -> Options {
        crate::parse(args.iter().map(std::ffi::OsString::from).collect()).unwrap()
    }
    fn pair(input: &str, output: &str) -> PortPairCandidate {
        PortPairCandidate {
            selection: DeviceSelection {
                input_id: input.into(),
                output_id: output.into(),
                fingerprint_hint: None,
            },
            display_name: "GP-200".into(),
            mode_hint: Some(BackendMode::Hardware),
            matching_confidence: "Test descriptors only; no backend".into(),
        }
    }
    #[test]
    fn unique_does_not_choose_first_of_identically_named_devices() {
        let ports = vec![pair("in-a", "out-a"), pair("in-b", "out-b")];
        let unique = options(&["device", "current", "--unique"]);
        assert_eq!(
            choose_pair(&unique, &ports).unwrap_err().kind,
            ErrorKind::Conflict
        );
        let exact = options(&[
            "device",
            "current",
            "--input-port",
            "in-b",
            "--output-port",
            "out-b",
        ]);
        assert_eq!(choose_pair(&exact, &ports).unwrap(), ports[1].selection);
        let crossed = options(&[
            "device",
            "current",
            "--input-port",
            "in-a",
            "--output-port",
            "out-b",
        ]);
        assert_eq!(
            choose_pair(&crossed, &ports).unwrap_err().kind,
            ErrorKind::Conflict
        );
    }
    #[test]
    fn exact_pair_requires_one_current_descriptor() {
        let exact = options(&[
            "device",
            "identity",
            "--input-port",
            "in-a",
            "--output-port",
            "out-a",
        ]);
        assert_eq!(
            choose_pair(&exact, &[]).unwrap_err().kind,
            ErrorKind::Conflict
        );
        let duplicate = pair("in-a", "out-a");
        assert_eq!(
            choose_pair(&exact, &[duplicate.clone(), duplicate])
                .unwrap_err()
                .kind,
            ErrorKind::Conflict
        );
    }
}
