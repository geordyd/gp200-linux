use crossbeam_channel::{Receiver, Sender, bounded};
use gp200_domain::*;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

pub fn captured_demo_snapshot() -> Arc<PresetSnapshot> {
    let raw = RawTransfer::from_payload(
        0x12,
        include_bytes!("../../../assets/demo/read-current-payload.bin").to_vec(),
    )
    .expect("checked embedded capture");
    let mut snapshot = PresetSnapshot::from_raw(
        Arc::new(raw),
        SnapshotSource::Simulator,
        Some(PresetTarget::EditBuffer),
        Revision(0),
    );
    snapshot.evidence.push(EvidenceRef {
        path: "assets/demo/provenance.json".into(),
        locator: Some("original capture and decoded payload SHA-256".into()),
        sha256: None,
        level: EvidenceLevel::LiveVerified,
    });
    Arc::new(snapshot)
}

/// Public-facing simulator preset. Only its name is synthetic; the embedded
/// capture and the byte-exact capture replay remain unchanged.
pub fn public_demo_snapshot() -> Arc<PresetSnapshot> {
    let captured = captured_demo_snapshot();
    let mut draft = PresetDraft::from_snapshot(&captured);
    draft
        .apply(&EditPatch::SetMetadata(MetadataPatch {
            name: Some("Clean Shimmer".into()),
            ..Default::default()
        }))
        .expect("valid synthetic demo name");
    let mut snapshot = draft
        .snapshot(&captured, SnapshotSource::Simulator, Revision(0))
        .expect("synthetic demo name encodes without changing other fields");
    let evidence = EvidenceRef {
        path: "crates/gp200-sim/src/device.rs".into(),
        locator: Some(
            "Synthetic public demo name; effects configuration derives from the retained capture"
                .into(),
        ),
        sha256: None,
        level: EvidenceLevel::Proposed,
    };
    snapshot.view.name = Recovered::Candidate {
        value: "Clean Shimmer".into(),
        evidence: vec![evidence.clone()],
    };
    snapshot.evidence.push(evidence);
    Arc::new(snapshot)
}

pub fn simulator_port() -> PortPairCandidate {
    PortPairCandidate {
        selection: DeviceSelection {
            input_id: "simulator:input".into(),
            output_id: "simulator:output".into(),
            fingerprint_hint: None,
        },
        display_name: "GP-200 simulator · captured preset".into(),
        mode_hint: Some(BackendMode::Simulator),
        matching_confidence: "Explicit simulator, no physical MIDI".into(),
    }
}

/// Deterministic command processor; tests need no thread or wall-clock sleeps.
pub struct DemoDevice {
    pub epoch: SessionEpoch,
    pub connected: bool,
    pub current: Arc<PresetSnapshot>,
    pub stored: BTreeMap<SlotId, Arc<PresetSnapshot>>,
    revision: Revision,
}
impl Default for DemoDevice {
    fn default() -> Self {
        Self::new()
    }
}
impl DemoDevice {
    pub fn new() -> Self {
        Self {
            epoch: SessionEpoch(0),
            connected: false,
            current: captured_demo_snapshot(),
            stored: BTreeMap::new(),
            revision: Revision(0),
        }
    }
    fn identity(&self) -> DeviceIdentity {
        DeviceIdentity {
            model: DeviceModel::Gp200,
            mode: BackendMode::Simulator,
            firmware: None,
            display_name: "GP-200 simulator".into(),
        }
    }
    fn current_event(&self, stamp: RequestStamp) -> DeviceEvent {
        DeviceEvent::SnapshotReceived {
            stamp,
            target: PresetTarget::EditBuffer,
            snapshot: self.current.clone(),
        }
    }
    fn context_snapshot(
        &self,
        source: &PresetSnapshot,
        target: PresetTarget,
    ) -> Arc<PresetSnapshot> {
        let context = match target {
            PresetTarget::EditBuffer => WireContext {
                group: 0,
                selector: source.context.map_or(23, |c| c.selector),
            },
            PresetTarget::Stored(slot) => WireContext {
                group: 1,
                selector: slot.get(),
            },
        };
        let mut snapshot = source.clone();
        snapshot.target = Some(target);
        snapshot.source = SnapshotSource::Simulator;
        snapshot.context = Some(context);
        let mut records = source.raw.records().to_vec();
        if let Some(record) = records
            .iter_mut()
            .find(|record| record.command == 0 && record.body.len() == 4)
        {
            record.body[..2].copy_from_slice(&context.group.to_le_bytes());
            record.body[2..].copy_from_slice(&context.selector.to_le_bytes());
        }
        snapshot.raw = Arc::new(
            RawTransfer::from_records(source.raw.kind(), &records).expect("valid context rewrite"),
        );
        Arc::new(snapshot)
    }
    fn stamp(&self, command: &DeviceCommand) -> Option<RequestStamp> {
        match command {
            DeviceCommand::ReadIdentity(stamp)
            | DeviceCommand::ReadCurrent(stamp)
            | DeviceCommand::ReadGlobals(stamp) => Some(*stamp),
            DeviceCommand::ReadStored { stamp, .. }
            | DeviceCommand::ReadAssetMetadata { stamp, .. }
            | DeviceCommand::RefreshTarget { stamp, .. }
            | DeviceCommand::SelectStored { stamp, .. }
            | DeviceCommand::SubmitEdit { stamp, .. }
            | DeviceCommand::SubmitDependentModel { stamp, .. }
            | DeviceCommand::SubmitFactoryAmpModel { stamp, .. }
            | DeviceCommand::SubmitGlobalEdit { stamp, .. }
            | DeviceCommand::SubmitGlobalControlEdit { stamp, .. }
            | DeviceCommand::SaveBufferTo { stamp, .. }
            | DeviceCommand::WriteStoredPreset { stamp, .. }
            | DeviceCommand::ClearStoredPreset { stamp, .. }
            | DeviceCommand::ExecuteDeviceJob { stamp, .. }
            | DeviceCommand::CancelJob { stamp, .. } => Some(*stamp),
            _ => None,
        }
    }
    pub fn handle(&mut self, command: DeviceCommand) -> Vec<DeviceEvent> {
        if let Some(stamp) = self.stamp(&command)
            && (!self.connected || stamp.epoch != self.epoch)
        {
            return vec![DeviceEvent::OperationFailed {
                stamp,
                error: ServiceError::new(
                    ErrorKind::Disconnected,
                    "Simulator request is disconnected or from an old epoch",
                ),
            }];
        }
        match command {
            DeviceCommand::SubmitDependentModel { stamp, .. }
            | DeviceCommand::SubmitFactoryAmpModel { stamp, .. } => {
                vec![DeviceEvent::OperationFailed {
                    stamp,
                    error: ServiceError::unsupported(
                        "The simulator does not execute physical dependent-model trials",
                    ),
                }]
            }
            DeviceCommand::Discover => vec![DeviceEvent::PortsChanged(vec![simulator_port()])],
            DeviceCommand::Connect { intent, selection } => {
                if selection.input_id != "simulator:input"
                    || selection.output_id != "simulator:output"
                {
                    return vec![DeviceEvent::OperationFailed {
                        stamp: RequestStamp {
                            epoch: self.epoch,
                            intent,
                        },
                        error: ServiceError::invalid(
                            "Demo service only opens explicit simulator ports",
                        ),
                    }];
                }
                self.epoch.0 += 1;
                self.connected = true;
                vec![
                    DeviceEvent::ConnectionChanged {
                        epoch: self.epoch,
                        state: ConnectionState::Connecting,
                    },
                    DeviceEvent::SessionChanged {
                        epoch: self.epoch,
                        identity: self.identity(),
                        capabilities: Capabilities::simulator(),
                    },
                    DeviceEvent::ConnectionChanged {
                        epoch: self.epoch,
                        state: ConnectionState::Ready,
                    },
                    self.current_event(RequestStamp {
                        epoch: self.epoch,
                        intent,
                    }),
                ]
            }
            DeviceCommand::Disconnect { .. } => {
                self.epoch.0 += 1;
                self.connected = false;
                vec![DeviceEvent::ConnectionChanged {
                    epoch: self.epoch,
                    state: ConnectionState::Offline,
                }]
            }
            DeviceCommand::ReadIdentity(_) => vec![DeviceEvent::SessionChanged {
                epoch: self.epoch,
                identity: self.identity(),
                capabilities: Capabilities::simulator(),
            }],
            DeviceCommand::ReadCurrent(stamp)
            | DeviceCommand::RefreshTarget {
                stamp,
                target: PresetTarget::EditBuffer,
            } => vec![self.current_event(stamp)],
            DeviceCommand::ReadStored { stamp, slot }
            | DeviceCommand::RefreshTarget {
                stamp,
                target: PresetTarget::Stored(slot),
            } => {
                let snapshot = self.stored.get(&slot).cloned().unwrap_or_else(|| {
                    self.context_snapshot(&captured_demo_snapshot(), PresetTarget::Stored(slot))
                });
                vec![DeviceEvent::SnapshotReceived {
                    stamp,
                    target: PresetTarget::Stored(slot),
                    snapshot,
                }]
            }
            DeviceCommand::SelectStored { stamp, slot } => {
                let snapshot = self.stored.get(&slot).cloned().unwrap_or_else(|| {
                    self.context_snapshot(&captured_demo_snapshot(), PresetTarget::Stored(slot))
                });
                self.current = self.context_snapshot(&snapshot, PresetTarget::EditBuffer);
                vec![
                    DeviceEvent::EditProgress {
                        stamp,
                        stage: EditStage::Verified,
                    },
                    self.current_event(stamp),
                ]
            }
            DeviceCommand::SubmitEdit {
                stamp,
                target,
                base_revision,
                patch,
            } => {
                if target != PresetTarget::EditBuffer || base_revision != self.current.revision {
                    return vec![DeviceEvent::OperationFailed {
                        stamp,
                        error: ServiceError::new(
                            ErrorKind::Conflict,
                            "Simulator edit target/revision mismatch",
                        ),
                    }];
                }
                let mut draft = PresetDraft::from_snapshot(&self.current);
                match draft.apply(&patch).and_then(|()| {
                    self.revision.0 += 1;
                    draft.snapshot(&self.current, SnapshotSource::Simulator, self.revision)
                }) {
                    Ok(snapshot) => {
                        self.current = Arc::new(snapshot);
                        vec![
                            DeviceEvent::EditProgress {
                                stamp,
                                stage: EditStage::Sent,
                            },
                            DeviceEvent::EditProgress {
                                stamp,
                                stage: EditStage::AwaitingReadback,
                            },
                            self.current_event(stamp),
                            DeviceEvent::EditProgress {
                                stamp,
                                stage: EditStage::Verified,
                            },
                        ]
                    }
                    Err(error) => vec![DeviceEvent::OperationFailed { stamp, error }],
                }
            }
            DeviceCommand::SaveBufferTo {
                stamp,
                slot,
                expected_buffer_revision,
            } => {
                if expected_buffer_revision != self.current.revision {
                    return vec![DeviceEvent::OperationFailed {
                        stamp,
                        error: ServiceError::new(
                            ErrorKind::Conflict,
                            "Buffer changed before simulated save",
                        ),
                    }];
                }
                let snapshot = self.context_snapshot(&self.current, PresetTarget::Stored(slot));
                self.stored.insert(slot, snapshot.clone());
                vec![
                    DeviceEvent::EditProgress {
                        stamp,
                        stage: EditStage::Verified,
                    },
                    DeviceEvent::SnapshotReceived {
                        stamp,
                        target: PresetTarget::Stored(slot),
                        snapshot,
                    },
                ]
            }
            DeviceCommand::SubmitGlobalEdit { stamp, .. }
            | DeviceCommand::ReadAssetMetadata { stamp, .. }
            | DeviceCommand::ReadGlobals(stamp)
            | DeviceCommand::SubmitGlobalControlEdit { stamp, .. }
            | DeviceCommand::WriteStoredPreset { stamp, .. }
            | DeviceCommand::ClearStoredPreset { stamp, .. }
            | DeviceCommand::ExecuteDeviceJob { stamp, .. } => vec![DeviceEvent::OperationFailed {
                stamp,
                error: ServiceError::unsupported(
                    "Bootstrap demo service does not simulate globals, assets, or firmware",
                ),
            }],
            DeviceCommand::CancelJob { stamp, .. } => vec![DeviceEvent::OperationFailed {
                stamp,
                error: ServiceError::new(ErrorKind::Cancelled, "No running simulator job"),
            }],
            DeviceCommand::Shutdown => {
                self.connected = false;
                Vec::new()
            }
        }
    }
}

/// Bounded asynchronous wrapper. One consumer drains events; receiver clones are not broadcast.
pub struct DemoDeviceService {
    commands: Sender<DeviceCommand>,
    events: Receiver<DeviceEvent>,
    shutdown: ShutdownToken,
}
impl DemoDeviceService {
    pub fn new(wake: WakeUi) -> Self {
        let (commands, command_rx) = bounded(64);
        let (event_tx, events) = bounded(256);
        let shutdown = ShutdownToken::default();
        let worker_shutdown = shutdown.clone();
        std::thread::Builder::new()
            .name("gp200-demo".into())
            .spawn(move || {
                let mut device = DemoDevice::new();
                while !worker_shutdown.cancelled() {
                    let Ok(command) = command_rx.recv_timeout(Duration::from_millis(25)) else {
                        continue;
                    };
                    if matches!(command, DeviceCommand::Shutdown) {
                        break;
                    }
                    for event in device.handle(command) {
                        let mut pending = event;
                        loop {
                            match event_tx.send_timeout(pending, Duration::from_millis(25)) {
                                Ok(()) => {
                                    wake();
                                    break;
                                }
                                Err(crossbeam_channel::SendTimeoutError::Timeout(event))
                                    if !worker_shutdown.cancelled() =>
                                {
                                    pending = event
                                }
                                Err(_) => return,
                            }
                        }
                    }
                }
            })
            .expect("create bounded simulator worker");
        Self {
            commands,
            events,
            shutdown,
        }
    }
}
impl DeviceService for DemoDeviceService {
    fn try_submit(&self, command: DeviceCommand) -> Result<(), SubmitError<DeviceCommand>> {
        self.commands
            .try_send(command)
            .map_err(|error| match error {
                crossbeam_channel::TrySendError::Full(command) => SubmitError::Busy(command),
                crossbeam_channel::TrySendError::Disconnected(command) => {
                    SubmitError::Disconnected(command)
                }
            })
    }
    fn try_next_event(&self) -> Option<DeviceEvent> {
        self.events.try_recv().ok()
    }
    fn shutdown(&self) {
        self.shutdown.cancel();
        let _ = self.commands.try_send(DeviceCommand::Shutdown);
    }
}
impl Drop for DemoDeviceService {
    fn drop(&mut self) {
        self.shutdown();
    }
}
