use gp200_catalog::{
    Catalog, ControlRequest, ControlSemantics, OrdinaryRestore, control_semantics,
    plan_control_request, sync_value_parameter,
};
use gp200_domain::*;
use gp200_protocol::{
    DrumSetting, F32Bits, GlobalRecords, Identity, ParameterWrite, RawRecord, RawTransfer,
    Reassembler, StreamDecoder, StreamEvent, WireDeviceModel, WireModelIndex, chain_write_record,
    contextual_write_request, enabled_write_record, encode_transfer, identify_request,
    metadata_write_record, parameter_write_request, read_current_request, read_globals_request,
    read_stored_request, rename_write_record,
};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct TransportConfig {
    pub command_capacity: usize,
    pub event_capacity: usize,
    pub callback_capacity: usize,
    pub request_timeout: Duration,
    pub reassembly_timeout: Duration,
    pub write_settle: Duration,
    pub packet_spacing: Duration,
    /// Minimum spacing between logical request starts, independent of packet
    /// spacing. The initial 25 ms bank loop dropped a request after 130 slots.
    pub minimum_request_spacing: Duration,
    pub discovery_interval: Duration,
    /// Opt-in raw wire diagnostics for research/CLI capture, disabled in GUI.
    pub trace_bytes: bool,
    /// Research diagnostic switch. Production retains hotplug discovery.
    pub poll_ports_while_connected: bool,
}
impl Default for TransportConfig {
    fn default() -> Self {
        Self {
            command_capacity: 32,
            event_capacity: 128,
            callback_capacity: 64,
            request_timeout: Duration::from_millis(1500),
            reassembly_timeout: Duration::from_millis(1000),
            write_settle: Duration::from_millis(40),
            packet_spacing: Duration::from_millis(5),
            minimum_request_spacing: Duration::from_millis(500),
            discovery_interval: Duration::from_secs(2),
            trace_bytes: false,
            poll_ports_while_connected: true,
        }
    }
}
impl TransportConfig {
    /// Linux raw-MIDI profile: all 256 slots passed with active discovery at
    /// this spacing. Midir retains the separate conservative default profile.
    pub fn alsa_raw() -> Self {
        Self {
            minimum_request_spacing: Duration::from_millis(100),
            ..Self::default()
        }
    }
}
pub enum TransportAction {
    Enumerate,
    Open {
        selection: DeviceSelection,
        epoch: SessionEpoch,
    },
    Close,
    Send {
        packets: Vec<Vec<u8>>,
    },
    Event(DeviceEvent),
    CaptureRead {
        reservation: String,
        sequence: u64,
    },
}

/// Coarse operation support is further restricted by catalog/context/revision
/// gates. Samples establish the common ordinary numeric path in all 11 modules;
/// they do not claim hardware coverage of every control/model combination.
pub fn verified_capabilities() -> Capabilities {
    let mut result = Capabilities::gp200_read_only();
    result.operations.insert(
        OperationKind::ReadGlobals,
        FeatureSupport::Verified {
            profile: ValidationProfileId("gp200-global-read-2026-10-10".into()),
            mode: BackendMode::Hardware,
            firmware: None,
            evidence: vec![EvidenceRef::new(
                "analysis/implementation/global-baseline/result.json",
                EvidenceLevel::LiveVerified,
            )],
        },
    );
    result.operations.insert(
        OperationKind::WriteGlobals,
        FeatureSupport::Verified {
            profile: ValidationProfileId(
                "gp200-general-eq-inactive-switch-storage-2026-10-10".into(),
            ),
            mode: BackendMode::Hardware,
            firmware: None,
            evidence: [
                "display-mode",
                "drum-tempo",
                "input-level",
                "eq-band-gain",
                "stopped-drum-volume",
                "stopped-drum-pattern",
                "quiet-drum-play",
                "inactive-footswitch",
            ]
            .into_iter()
            .map(|name| {
                EvidenceRef::new(
                    format!("analysis/implementation/global-{name}/result.json"),
                    EvidenceLevel::LiveVerified,
                )
            })
            .chain(
                [
                    "input-mode",
                    "no-cab-left",
                    "no-cab-right",
                    "recording-level",
                    "recording-mode-left",
                    "recording-mode-right",
                    "aux-to-usb",
                    "monitor-level",
                    "external-mode",
                    "midi-clock-source",
                    "midi-input-usb",
                    "midi-input-din",
                    "midi-output-usb",
                    "midi-output-din",
                    "midi-input-source",
                    "midi-clock-out-usb",
                    "midi-clock-out-din",
                    "footswitch-template",
                    "external-footswitch1",
                    "external-footswitch2",
                    "footswitch-mode",
                    "auto-cab-match",
                ]
                .into_iter()
                .map(|name| {
                    EvidenceRef::new(
                        format!("analysis/implementation/global-general-{name}/result.json"),
                        EvidenceLevel::LiveVerified,
                    )
                }),
            )
            .chain((0..31).map(|index| {
                EvidenceRef::new(
                    format!("analysis/implementation/global-eq-field-{index}/result.json"),
                    EvidenceLevel::LiveVerified,
                )
            }))
            .chain(std::iter::once(EvidenceRef::new(
                "analysis/implementation/footswitch-matrix-acceptance.json",
                EvidenceLevel::LiveVerified,
            )))
            .collect(),
        },
    );
    result.operations.insert(
        OperationKind::SetFootswitch,
        FeatureSupport::Verified {
            profile: ValidationProfileId("gp200-ctrl-module-membership-current-2026-10-10".into()),
            mode: BackendMode::Hardware,
            firmware: None,
            evidence: vec![EvidenceRef::new(
                "analysis/implementation/assignment-ctrl-modules-roundtrip/result.json",
                EvidenceLevel::LiveVerified,
            )],
        },
    );
    result.operations.insert(
        OperationKind::SetQuickControl,
        FeatureSupport::Verified {
            profile: ValidationProfileId("gp200-quick-references-current-2026-10-10".into()),
            mode: BackendMode::Hardware,
            firmware: None,
            evidence: [
                "assignment-quick-parameter",
                "assignment-quick-volume-index0",
                "assignment-quick-tempo-index0",
                "assignment-quick-off-index0",
            ]
            .into_iter()
            .map(|path| {
                EvidenceRef::new(
                    format!("analysis/implementation/{path}-roundtrip/result.json"),
                    EvidenceLevel::LiveVerified,
                )
            })
            .collect(),
        },
    );
    result.operations.insert(
        OperationKind::SetExpression,
        FeatureSupport::Verified {
            profile: ValidationProfileId(
                "gp200-exp-independent-pwah-assignments-current-2026-10-10".into(),
            ),
            mode: BackendMode::Hardware,
            firmware: None,
            evidence: vec![
                EvidenceRef::new(
                    "analysis/implementation/assignment-expression-endpoint-roundtrip/result.json",
                    EvidenceLevel::LiveVerified,
                ),
                EvidenceRef::new(
                    "analysis/implementation/assignment-expression-minimum-roundtrip/result.json",
                    EvidenceLevel::LiveVerified,
                ),
                EvidenceRef::new(
                    "analysis/implementation/assignment-expression-paired-roundtrip/result.json",
                    EvidenceLevel::LiveVerified,
                ),
                EvidenceRef::new(
                    "analysis/implementation/assignment-expression-target-native/result.json",
                    EvidenceLevel::LiveVerified,
                ),
                EvidenceRef::new(
                    "analysis/implementation/assignment-expression-clear-wire-return/result.json",
                    EvidenceLevel::LiveVerified,
                ),
            ],
        },
    );
    result.operations.insert(
        OperationKind::SetParameter,
        FeatureSupport::Verified {
            profile: ValidationProfileId("gp200-catalog-controls-current-2026-10-10".into()),
            mode: BackendMode::Hardware,
            firmware: None,
            evidence: vec![
                EvidenceRef::new(
                    "analysis/implementation/parameter-roundtrip/result.json",
                    EvidenceLevel::LiveVerified,
                ),
                EvidenceRef::new(
                    "analysis/implementation/ordinary-parameter-matrix/result.json",
                    EvidenceLevel::LiveVerified,
                ),
                EvidenceRef::new(
                    "analysis/implementation/ordinary-parameter-matrix-tail/result.json",
                    EvidenceLevel::LiveVerified,
                ),
                EvidenceRef::new(
                    "analysis/implementation/rust-raw-gain-roundtrip/result.json",
                    EvidenceLevel::LiveVerified,
                ),
                EvidenceRef::new(
                    "analysis/implementation/control-mod-sync-paired/result.json",
                    EvidenceLevel::LiveVerified,
                ),
                EvidenceRef::new(
                    "analysis/implementation/control-dly-sync-paired/result.json",
                    EvidenceLevel::LiveVerified,
                ),
                EvidenceRef::new(
                    "analysis/implementation/control-cab-low/result.json",
                    EvidenceLevel::LiveVerified,
                ),
                EvidenceRef::new(
                    "analysis/implementation/control-cab-high/result.json",
                    EvidenceLevel::LiveVerified,
                ),
                EvidenceRef::new(
                    "analysis/implementation/control-wah-eq/result.json",
                    EvidenceLevel::LiveVerified,
                ),
                EvidenceRef::new(
                    "analysis/implementation/control-dly-trail/result.json",
                    EvidenceLevel::LiveVerified,
                ),
                EvidenceRef::new(
                    "analysis/implementation/rust-raw-mod-sync/result.json",
                    EvidenceLevel::LiveVerified,
                ),
                EvidenceRef::new(
                    "analysis/implementation/rust-raw-dly-sync/result.json",
                    EvidenceLevel::LiveVerified,
                ),
            ],
        },
    );
    result.operations.insert(
        OperationKind::SetModuleEnabled,
        FeatureSupport::Verified {
            profile: ValidationProfileId("gp200-enabled-current-eleven-modules-2026-10-10".into()),
            mode: BackendMode::Hardware,
            firmware: None,
            evidence: (0..11)
                .map(|module| {
                    EvidenceRef::new(
                        format!("analysis/implementation/enabled-{module}-roundtrip/result.json"),
                        EvidenceLevel::LiveVerified,
                    )
                })
                .collect(),
        },
    );
    for (operation, name) in [
        (OperationKind::SetChain, "chain"),
        (OperationKind::SetMetadata, "metadata"),
        (OperationKind::PresetImport, "stored-name"),
    ] {
        let paths: Vec<_> = if name == "metadata" {
            vec!["name", "tempo", "level", "metadata-pan", "metadata-style"]
        } else if name == "chain" {
            vec!["chain", "assignment-fx-positions"]
        } else {
            vec![name]
        };
        result.operations.insert(
            operation,
            FeatureSupport::Verified {
                profile: ValidationProfileId(format!("gp200-{name}-2026-10-10")),
                mode: BackendMode::Hardware,
                firmware: None,
                evidence: paths
                    .into_iter()
                    .map(|path| {
                        EvidenceRef::new(
                            format!("analysis/implementation/{path}-roundtrip/result.json"),
                            EvidenceLevel::LiveVerified,
                        )
                    })
                    .collect(),
            },
        );
    }
    for operation in OperationKind::ALL {
        result
            .operations
            .entry(operation)
            .or_insert_with(|| FeatureSupport::Unverified {
                evidence: vec![EvidenceRef::new(
                    "plans/research-backlog.json",
                    EvidenceLevel::Proposed,
                )],
                research_id: match operation {
                    OperationKind::SetModuleEnabled | OperationKind::SetModel => "R-008",
                    OperationKind::SetChain => "R-009",
                    OperationKind::SetMetadata => "R-010",
                    OperationKind::SetExpression => "R-011",
                    OperationKind::SetFootswitch => "R-012",
                    OperationKind::SetQuickControl => "R-013",
                    OperationKind::ReadGlobals | OperationKind::WriteGlobals => "R-015",
                    OperationKind::IrRead | OperationKind::IrWrite => "R-022",
                    OperationKind::SnapToneRead | OperationKind::SnapToneWrite => "R-024",
                    OperationKind::FirmwareUpdate => "R-025",
                    OperationKind::FactoryReset => "R-018",
                    _ => "R-007",
                }
                .into(),
            });
    }
    for (operation, path) in [
        (OperationKind::SelectStoredPreset, "select-runtime-exp"),
        (OperationKind::SavePreset, "save-as-runtime-exp"),
    ] {
        result.operations.insert(
            operation,
            FeatureSupport::Verified {
                profile: ValidationProfileId(format!("gp200-{path}-2026-10-10")),
                mode: BackendMode::Hardware,
                firmware: None,
                evidence: vec![EvidenceRef::new(
                    format!("analysis/implementation/{path}/result.json"),
                    EvidenceLevel::LiveVerified,
                )],
            },
        );
    }
    result.operations.insert(
        OperationKind::SetModel,
        FeatureSupport::Verified {
            profile: ValidationProfileId(
                "gp200-sparse-unassigned-model-transitions-2026-10-10".into(),
            ),
            mode: BackendMode::Hardware,
            firmware: None,
            evidence: [
                "analysis/implementation/model-nr-roundtrip/result.json",
                "analysis/implementation/rust-model-nr/result.json",
                "analysis/implementation/rust-model-dly/result.json",
            ]
            .into_iter()
            .map(|path| EvidenceRef::new(path, EvidenceLevel::LiveVerified))
            .collect(),
        },
    );
    result.operations.insert(
        OperationKind::ClearPreset,
        FeatureSupport::Verified {
            profile: ValidationProfileId(
                "gp200-clear-inactive-stored-device-default-2026-10-10".into(),
            ),
            mode: BackendMode::Hardware,
            firmware: None,
            evidence: vec![EvidenceRef::new(
                "analysis/implementation/clear-changed-scratch-roundtrip/result.json",
                EvidenceLevel::LiveVerified,
            )],
        },
    );
    result.operations.insert(
        OperationKind::IrRead,
        FeatureSupport::Verified {
            profile: ValidationProfileId("gp200-ir-metadata-all20-2026-10-10".into()),
            mode: BackendMode::Hardware,
            firmware: None,
            evidence: vec![EvidenceRef::new(
                "analysis/implementation/assets-ir-bank/result.json",
                EvidenceLevel::LiveVerified,
            )],
        },
    );
    result.operations.insert(OperationKind::SnapToneRead, FeatureSupport::Unverified {
        evidence: vec![EvidenceRef::new("analysis/implementation/assets-snaptone-slot0/result.json", EvidenceLevel::LiveVerified)],
        research_id: "R-024: slot 0 returned no reply within the captured 1-second observation; feature availability remains unknown".into(),
    });
    result
}

/// Codec-backed offline sessions expose only operations this actor implements.
/// Their profiles carry their own provenance, never physical restoration proof.
pub fn capabilities_for_mode(mode: BackendMode) -> Capabilities {
    if mode == BackendMode::Hardware {
        return verified_capabilities();
    }
    let mut result = Capabilities::empty(DeviceModel::Gp200);
    for operation in OperationKind::ALL {
        let readable = matches!(
            operation,
            OperationKind::Identify
                | OperationKind::ReadEditBuffer
                | OperationKind::ReadStoredPreset
                | OperationKind::ReadGlobals
                | OperationKind::IrRead
                | OperationKind::SnapToneRead
        );
        let implemented = readable
            || matches!(
                operation,
                OperationKind::SetParameter
                    | OperationKind::SetModuleEnabled
                    | OperationKind::SetModel
                    | OperationKind::SetChain
                    | OperationKind::SetMetadata
                    | OperationKind::SetExpression
                    | OperationKind::SetFootswitch
                    | OperationKind::SetQuickControl
                    | OperationKind::PresetImport
                    | OperationKind::SelectStoredPreset
                    | OperationKind::SavePreset
                    | OperationKind::ClearPreset
                    | OperationKind::WriteGlobals
            );
        if !implemented || (mode == BackendMode::CaptureReplay && !readable) {
            continue;
        }
        result.operations.insert(
            operation,
            FeatureSupport::Verified {
                profile: ValidationProfileId(format!("codec-{mode:?}-{operation:?}")),
                mode,
                firmware: None,
                evidence: vec![EvidenceRef::new(
                    "crates/gp200-transport/tests/actor.rs",
                    EvidenceLevel::Proposed,
                )],
            },
        );
    }
    result
}

#[derive(Clone)]
struct ParameterEdit {
    stamp: RequestStamp,
    base_revision: Revision,
    module: ModuleId,
    model: ModelCode,
    model_index: ModelIndex,
    parameter: ParamId,
    value: F32Bits,
}
impl ParameterEdit {
    fn same_control(&self, other: &Self) -> bool {
        self.stamp.epoch == other.stamp.epoch
            && self.module == other.module
            && self.model == other.model
            && self.model_index == other.model_index
            && self.parameter == other.parameter
    }
}
#[derive(Clone)]
enum BoundModelPlan {
    Existing(DependentModelPlan),
    Factory(FactoryAmpPlan),
}
impl BoundModelPlan {
    fn auto_cab(&self) -> bool {
        match self {
            Self::Existing(plan) => plan.auto_cab,
            Self::Factory(plan) => plan.auto_cab,
        }
    }
    fn paired_cabinet(&self) -> Option<ModelCode> {
        match self {
            Self::Existing(plan) => plan.paired_cabinet,
            Self::Factory(plan) => plan.paired_cabinet,
        }
    }
    fn enabled(&self) -> bool {
        match self {
            Self::Existing(plan) => dependent_profile_enabled(plan.profile),
            Self::Factory(plan) => {
                plan.profile.normal_build_accepted() || cfg!(feature = "factory-amp-trials")
            }
        }
    }
}
enum BoundModelSelection {
    Existing {
        module: ModuleId,
        model: ModelCode,
    },
    Factory {
        model: ModelCode,
        profile: FactoryAmpProfileId,
    },
}
#[derive(Clone)]
struct DependentMutation {
    before: Arc<PresetSnapshot>,
    before_globals: Arc<DeviceGlobalState>,
    plan: BoundModelPlan,
    request: RawTransfer,
    expected: RawTransfer,
}
enum RequestKind {
    Identity {
        synchronize: bool,
    },
    Read {
        target: PresetTarget,
        ready_after: bool,
    },
    Write(ParameterEdit),
    Mutation(ValidatedMutation),
    MetadataPreflight {
        before: Arc<PresetSnapshot>,
        request: RawTransfer,
        mutation: ValidatedMutation,
    },
    ReadGlobals,
    DependentGlobalsPreflight(DependentMutation),
    DependentCurrentPreflight(DependentMutation),
    DependentModelMutation(DependentMutation),
    DependentGlobalsPostflight {
        mutation: DependentMutation,
        preset_verified: bool,
    },
    ReadAssetMetadata(AssetSlot),
    GlobalPreflight {
        before: Arc<DeviceGlobalState>,
        patch: GlobalControlEdit,
    },
    GlobalMutation {
        before: Arc<DeviceGlobalState>,
        patch: GlobalControlEdit,
    },
}
impl RequestKind {
    fn is_mutation(&self) -> bool {
        matches!(
            self,
            Self::Write(_)
                | Self::Mutation(_)
                | Self::GlobalMutation { .. }
                | Self::DependentModelMutation(_)
                | Self::DependentGlobalsPostflight { .. }
        )
    }
}
#[derive(Clone)]
struct ValidatedMutation {
    target: PresetTarget,
    expected: RawTransfer,
    policy: MutationPolicy,
}
#[derive(Clone)]
enum MutationPolicy {
    Exact,
    Selection,
    SaveThenCurrent { current_expected: RawTransfer },
    SavedCurrent,
}
struct Pending {
    stamp: RequestStamp,
    kind: RequestKind,
    sent: bool,
    deadline: Option<Duration>,
    readback_at: Option<Duration>,
    readback_sent: bool,
    readback_queued: bool,
    mutation_attempted: bool,
}

/// Deterministic state machine. Caller supplies a monotonic clock, MIDI bytes,
/// OS send/open results and executes the returned actions on a single worker.
pub struct DeviceActor {
    pub(crate) capture: Option<crate::firmware_backup_capture::CaptureRun>,
    config: TransportConfig,
    mode: BackendMode,
    catalog: Catalog,
    epoch: SessionEpoch,
    state: ConnectionState,
    identity: Option<DeviceIdentity>,
    job_selection: Option<DeviceSelection>,
    job_session: Option<DeviceJobSession>,
    identity_revision: Revision,
    job_asset_baselines: Vec<Arc<DeviceAssetMetadata>>,
    pending: Option<Pending>,
    queued_edit: Option<ParameterEdit>,
    connecting_intent: Option<IntentId>,
    snapshots: BTreeMap<PresetTarget, Arc<PresetSnapshot>>,
    revisions: BTreeMap<PresetTarget, Revision>,
    globals: Option<Arc<DeviceGlobalState>>,
    global_revision: Revision,
    asset_metadata_revision: Revision,
    decoder: StreamDecoder,
    assembler: Reassembler,
    actions: VecDeque<TransportAction>,
    now: Duration,
    next_send_allowed: Duration,
    scheduled_packets: Option<Vec<Vec<u8>>>,
    notification_invalidates_current: bool,
    notification_invalidates_globals: bool,
    notification_invalidates_stored: BTreeSet<SlotId>,
}
impl DeviceActor {
    pub fn new(config: TransportConfig) -> Self {
        Self::with_mode(config, BackendMode::Hardware)
    }
    pub fn with_mode(config: TransportConfig, mode: BackendMode) -> Self {
        Self {
            capture: None,
            assembler: Reassembler::new(config.reassembly_timeout),
            config,
            mode,
            catalog: Catalog::embedded().expect("validated embedded catalog"),
            epoch: SessionEpoch(0),
            state: ConnectionState::Disconnected,
            identity: None,
            job_selection: None,
            job_session: None,
            identity_revision: Revision(0),
            job_asset_baselines: Vec::new(),
            pending: None,
            queued_edit: None,
            connecting_intent: None,
            snapshots: BTreeMap::new(),
            revisions: BTreeMap::new(),
            globals: None,
            global_revision: Revision(0),
            asset_metadata_revision: Revision(0),
            decoder: StreamDecoder::new(),
            actions: VecDeque::new(),
            now: Duration::ZERO,
            next_send_allowed: Duration::ZERO,
            scheduled_packets: None,
            notification_invalidates_current: false,
            notification_invalidates_globals: false,
            notification_invalidates_stored: BTreeSet::new(),
        }
    }
    pub(crate) fn set_fresh_epoch_before_connect(
        &mut self,
        epoch: SessionEpoch,
    ) -> Result<(), ServiceError> {
        if self.state != ConnectionState::Disconnected
            || self.epoch != SessionEpoch(0)
            || epoch.0 == 0
        {
            return Err(ServiceError::invalid(
                "Fresh normal actor epoch cannot replace existing state",
            ));
        }
        self.epoch = SessionEpoch(epoch.0 - 1);
        Ok(())
    }
    pub fn epoch(&self) -> SessionEpoch {
        self.epoch
    }
    pub fn state(&self) -> ConnectionState {
        self.state
    }
    pub fn is_busy(&self) -> bool {
        self.pending.is_some() || self.connecting_intent.is_some() || self.capture.is_some()
    }
    pub(crate) fn firmware_quiescence(&self) -> Result<(), ServiceError> {
        if self.capture.as_ref().is_some_and(|c| !c.closed())
            || self.state != ConnectionState::Ready
            || self.identity.is_none()
            || self.job_session.is_none()
            || self.job_selection.is_none()
            || self.identity_revision.0 == 0
            || !self.snapshots.contains_key(&PresetTarget::EditBuffer)
            || self.globals.is_none()
            || self.pending.is_some()
            || self.connecting_intent.is_some()
            || self.queued_edit.is_some()
            || self.scheduled_packets.is_some()
            || !self.actions.is_empty()
            || self.decoder.is_incomplete()
            || self.assembler.progress().is_some()
            || self.notification_invalidates_current
            || self.notification_invalidates_globals
            || !self.notification_invalidates_stored.is_empty()
        {
            return Err(ServiceError::new(
                ErrorKind::Busy,
                "Normal actor has pending bytes, reads, edits, framing or undelivered results",
            ));
        }
        if self.epoch.0 == u64::MAX {
            return Err(ServiceError::invalid("Normal epoch exhausted"));
        }
        Ok(())
    }
    #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
    pub(crate) fn firmware_recovery_quiescence(&self) -> Result<(), ServiceError> {
        if self.state != ConnectionState::Disconnected
            || self.capture.is_some()
            || self.pending.is_some()
            || self.connecting_intent.is_some()
            || self.queued_edit.is_some()
            || self.scheduled_packets.is_some()
            || !self.actions.is_empty()
            || self.decoder.is_incomplete()
            || self.assembler.progress().is_some()
            || self.notification_invalidates_current
            || self.notification_invalidates_globals
            || !self.notification_invalidates_stored.is_empty()
        {
            return Err(ServiceError::new(
                ErrorKind::Busy,
                "Fresh recovery requires a disconnected actor with no pending reads, edits, framing, capture or undelivered results",
            ));
        }
        if self.epoch.0 == u64::MAX {
            return Err(ServiceError::invalid("Recovery epoch exhausted"));
        }
        Ok(())
    }
    pub(crate) fn capture_begin(
        &mut self,
        binding: crate::FirmwareNormalCaptureObservation,
        context: FirmwareBackupReviewContext,
        source: Arc<crate::backend::NormalCallbackSource>,
        processed: u64,
        shared: Arc<crate::firmware_backup_capture::CaptureShared>,
        now: Duration,
    ) -> Result<(), ServiceError> {
        self.firmware_quiescence()?;
        if self.capture.is_some() {
            return Err(ServiceError::new(
                ErrorKind::Busy,
                "Capture reservation cannot restart/replay",
            ));
        }
        self.capture = Some(crate::firmware_backup_capture::CaptureRun::new(
            crate::firmware_backup_capture::CaptureAdmission {
                epoch: self.epoch,
                mode: self.mode,
                config: &self.config,
                binding,
                context,
                source,
                processed,
                shared,
                now,
            },
        )?);
        Ok(())
    }
    pub(crate) fn capture_abort(&mut self, error: ServiceError) {
        if let Some(run) = self.capture.as_mut() {
            let processed = run.processed();
            run.fail(error, processed);
        }
    }
    pub(crate) fn firmware_handed_off(&mut self) {
        self.epoch.0 += 1;
        self.state = ConnectionState::Disconnected;
        self.identity = None;
        self.job_selection = None;
        self.job_session = None;
        self.job_asset_baselines.clear();
        self.snapshots.clear();
        self.revisions.clear();
        self.globals = None;
        self.identity_revision = Revision(0);
        self.global_revision = Revision(0);
        self.asset_metadata_revision = Revision(0);
        self.decoder.reset();
        self.assembler.reset();
    }
    pub fn snapshot(&self, target: PresetTarget) -> Option<&Arc<PresetSnapshot>> {
        self.snapshots.get(&target)
    }
    pub fn global_state(&self) -> Option<&Arc<DeviceGlobalState>> {
        self.globals.as_ref()
    }
    fn snapshot_source(&self) -> SnapshotSource {
        match self.mode {
            BackendMode::Hardware => SnapshotSource::Hardware { epoch: self.epoch },
            BackendMode::Simulator => SnapshotSource::Simulator,
            BackendMode::CaptureReplay => SnapshotSource::Capture {
                capture_id: format!("transport-replay-epoch-{}", self.epoch.0),
            },
        }
    }
    pub fn next_action(&mut self) -> Option<TransportAction> {
        self.actions.pop_front()
    }
    fn event(&mut self, event: DeviceEvent) {
        self.actions.push_back(TransportAction::Event(event));
    }
    fn state_changed(&mut self, state: ConnectionState) {
        self.state = state;
        self.event(DeviceEvent::ConnectionChanged {
            epoch: self.epoch,
            state,
        });
    }
    fn diagnostic(&mut self, category: &str, message: impl Into<String>, bytes: Option<Vec<u8>>) {
        self.event(DeviceEvent::Diagnostic(DiagnosticEvent {
            category: category.into(),
            message: message.into(),
            bytes,
            epoch: Some(self.epoch),
        }));
    }
    pub fn trace_wire(&mut self, category: &str, bytes: &[u8], now: Duration) {
        if self.config.trace_bytes {
            self.diagnostic(
                category,
                format!("monotonic_us={}", now.as_micros()),
                Some(bytes.to_vec()),
            );
        }
    }
    fn fail(&mut self, stamp: RequestStamp, error: ServiceError) {
        self.event(DeviceEvent::OperationFailed { stamp, error });
    }
    fn stamp_ok(&mut self, stamp: RequestStamp) -> bool {
        if stamp.epoch != self.epoch {
            self.fail(
                stamp,
                ServiceError::new(
                    ErrorKind::Conflict,
                    "Request belongs to an old device session",
                ),
            );
            return false;
        }
        if self.identity.is_none()
            || matches!(
                self.state,
                ConnectionState::Disconnected
                    | ConnectionState::Recovering
                    | ConnectionState::Unsupported
            )
        {
            self.fail(
                stamp,
                ServiceError::new(ErrorKind::Disconnected, "No identified GP-200 session"),
            );
            return false;
        }
        true
    }
    fn idle(&mut self, stamp: RequestStamp) -> bool {
        if self.is_busy() {
            self.fail(
                stamp,
                ServiceError::new(ErrorKind::Busy, "Another request owns this device session"),
            );
            false
        } else {
            true
        }
    }
    pub fn command(&mut self, command: DeviceCommand, now: Duration) {
        self.now = now;
        if self.capture.is_some()
            && !matches!(&command, DeviceCommand::Discover | DeviceCommand::Shutdown)
        {
            let error = ServiceError::new(
                ErrorKind::Busy,
                "Read-only firmware capture owns the normal actor",
            );
            if let Some(stamp) = command_stamp(&command) {
                self.fail(stamp, error);
            } else {
                self.diagnostic("capture-exclusion", error.message, None);
            }
            return;
        }
        match command {
            DeviceCommand::Discover => {
                if self.state == ConnectionState::Disconnected {
                    self.state_changed(ConnectionState::Discovering);
                }
                self.actions.push_back(TransportAction::Enumerate);
            }
            DeviceCommand::Connect { intent, selection } => {
                self.end_session(
                    ServiceError::new(
                        ErrorKind::Cancelled,
                        "Session replaced by explicit connection",
                    ),
                    false,
                );
                self.connecting_intent = Some(intent);
                self.job_selection = Some(selection.clone());
                self.state_changed(ConnectionState::Connecting);
                self.actions.push_back(TransportAction::Open {
                    selection,
                    epoch: self.epoch,
                });
            }
            DeviceCommand::Disconnect { .. } | DeviceCommand::Shutdown => {
                self.end_session(
                    ServiceError::new(ErrorKind::Cancelled, "Device disconnected"),
                    false,
                );
                self.state_changed(ConnectionState::Disconnected);
            }
            DeviceCommand::ReadIdentity(stamp) => {
                if self.stamp_ok(stamp) && self.idle(stamp) {
                    self.begin(
                        stamp,
                        RequestKind::Identity { synchronize: false },
                        identify_request(),
                    );
                }
            }
            DeviceCommand::ReadCurrent(stamp) => self.read(stamp, PresetTarget::EditBuffer),
            DeviceCommand::ReadGlobals(stamp) => {
                if self.stamp_ok(stamp) && self.idle(stamp) {
                    self.begin(
                        stamp,
                        RequestKind::ReadGlobals,
                        read_globals_request().expect("static query"),
                    );
                }
            }
            DeviceCommand::ReadAssetMetadata { stamp, slot } => {
                if self.stamp_ok(stamp) && self.idle(stamp) {
                    let operation = match slot.bank() {
                        AssetBank::Ir => OperationKind::IrRead,
                        AssetBank::SnapTone => OperationKind::SnapToneRead,
                    };
                    match self.require_operation(operation) {
                        Ok(()) => self.begin(
                            stamp,
                            RequestKind::ReadAssetMetadata(slot),
                            gp200_protocol::asset_metadata_read_candidate(slot),
                        ),
                        Err(error) => self.fail(stamp, error),
                    }
                }
            }
            DeviceCommand::SubmitGlobalControlEdit {
                stamp,
                base_revision,
                patch,
            } => {
                if self.stamp_ok(stamp) && self.idle(stamp) {
                    self.start_global_edit(stamp, base_revision, patch);
                }
            }
            DeviceCommand::ReadStored { stamp, slot } => {
                self.read(stamp, PresetTarget::Stored(slot))
            }
            DeviceCommand::RefreshTarget { stamp, target } => self.read(stamp, target),
            DeviceCommand::SubmitDependentModel {
                stamp,
                base_revision,
                module,
                model,
                before_globals,
                expected_auto_cab,
            } => {
                if self.stamp_ok(stamp) && self.idle(stamp) {
                    self.start_bound_model(
                        stamp,
                        base_revision,
                        BoundModelSelection::Existing { module, model },
                        before_globals,
                        expected_auto_cab,
                    );
                }
            }
            DeviceCommand::SubmitFactoryAmpModel {
                stamp,
                base_revision,
                model,
                profile,
                before_globals,
                expected_auto_cab,
            } => {
                if self.stamp_ok(stamp) && self.idle(stamp) {
                    self.start_bound_model(
                        stamp,
                        base_revision,
                        BoundModelSelection::Factory { model, profile },
                        before_globals,
                        expected_auto_cab,
                    );
                }
            }
            DeviceCommand::SubmitEdit {
                stamp,
                target,
                base_revision,
                patch,
            } => {
                if !self.stamp_ok(stamp) {
                    return;
                }
                let EditPatch::SetParameter {
                    module,
                    model,
                    model_index,
                    parameter,
                    value,
                } = patch
                else {
                    if self.idle(stamp) {
                        self.start_mutation(stamp, target, base_revision, patch);
                    }
                    return;
                };
                let edit = ParameterEdit {
                    stamp,
                    base_revision,
                    module,
                    model,
                    model_index,
                    parameter,
                    value,
                };
                if let Err(error) = self.validate_edit(target, &edit) {
                    self.fail(stamp, error);
                    return;
                }
                if let Some(pending) = &self.pending {
                    if let RequestKind::Write(active) = &pending.kind
                        && active.same_control(&edit)
                        && active.base_revision == edit.base_revision
                    {
                        if !pending.mutation_attempted {
                            let previous = pending.stamp;
                            self.pending = None;
                            self.scheduled_packets = None;
                            self.actions
                                .retain(|action| !matches!(action, TransportAction::Send { .. }));
                            self.edit_failure(
                                previous,
                                ServiceError::new(
                                    ErrorKind::Cancelled,
                                    "Unsent parameter value superseded before transmission",
                                ),
                                false,
                            );
                            self.start_edit(edit);
                            return;
                        }
                        if let Some(previous) = self.queued_edit.replace(edit.clone()) {
                            self.edit_failure(
                                previous.stamp,
                                ServiceError::new(
                                    ErrorKind::Cancelled,
                                    "Unsent parameter value superseded by the final value",
                                ),
                                false,
                            );
                        }
                        self.event(DeviceEvent::EditProgress {
                            stamp,
                            stage: EditStage::Queued,
                        });
                    } else {
                        self.fail(
                            stamp,
                            ServiceError::new(
                                ErrorKind::Busy,
                                "Wait for the outstanding request/readback",
                            ),
                        );
                    }
                    return;
                }
                self.start_edit(edit);
            }
            DeviceCommand::WriteStoredPreset {
                stamp,
                slot,
                expected_target_revision,
                snapshot,
            } => {
                if self.stamp_ok(stamp) && self.idle(stamp) {
                    self.start_full_write(stamp, slot, expected_target_revision, &snapshot);
                }
            }
            DeviceCommand::SelectStored { stamp, slot } => {
                if self.stamp_ok(stamp) && self.idle(stamp) {
                    self.start_selection(stamp, slot);
                }
            }
            DeviceCommand::SaveBufferTo {
                stamp,
                slot,
                expected_buffer_revision,
            } => {
                if self.stamp_ok(stamp) && self.idle(stamp) {
                    self.start_save(stamp, slot, expected_buffer_revision);
                }
            }
            DeviceCommand::ClearStoredPreset {
                stamp,
                slot,
                expected_target_revision,
            } => {
                if self.stamp_ok(stamp) && self.idle(stamp) {
                    self.start_clear(stamp, slot, expected_target_revision);
                }
            }
            DeviceCommand::ExecuteDeviceJob { stamp, job } => {
                if self.stamp_ok(stamp) && self.idle(stamp) {
                    let validation = (|| {
                        let session = self.job_session.as_ref().ok_or_else(|| {
                            ServiceError::new(
                                ErrorKind::Conflict,
                                "Device job has no current actor session observation",
                            )
                        })?;
                        let identity = self.identity.as_ref().ok_or_else(|| {
                            ServiceError::new(
                                ErrorKind::Disconnected,
                                "Device job requires a connected identity",
                            )
                        })?;
                        let baseline = self
                            .job_asset_baselines
                            .iter()
                            .find(|metadata| metadata.slot() == job.prepared().target())
                            .ok_or_else(|| {
                                ServiceError::new(
                                    ErrorKind::Conflict,
                                    "Device job requires actor-owned current slot metadata",
                                )
                            })?;
                        job.recheck_before_submit(
                            session,
                            identity,
                            &capabilities_for_mode(self.mode),
                            baseline,
                        )
                    })();
                    let error = validation.err().unwrap_or_else(|| ServiceError::new(ErrorKind::Unverified,
                        "Device asset executor awaits a complete verified transfer and recovery profile"));
                    // A successful structural check still cannot send: no asset
                    // write/restore executor profile has been registered.
                    self.fail(stamp, error);
                }
            }
            unsupported => {
                if let Some(stamp) = command_stamp(&unsupported)
                    && self.stamp_ok(stamp)
                {
                    self.fail(stamp, ServiceError::new(ErrorKind::Unverified,
                            "Hardware select/save/global/asset operation awaits its recorded research and validation gate"));
                }
            }
        }
        self.tick(now);
    }
    fn read(&mut self, stamp: RequestStamp, target: PresetTarget) {
        if self.stamp_ok(stamp) && self.idle(stamp) {
            let request = match target {
                PresetTarget::EditBuffer => read_current_request(),
                PresetTarget::Stored(slot) => {
                    read_stored_request(slot.get()).expect("bounded slot")
                }
            };
            self.begin(
                stamp,
                RequestKind::Read {
                    target,
                    ready_after: false,
                },
                request,
            );
        }
    }
    fn start_global_edit(
        &mut self,
        stamp: RequestStamp,
        revision: Revision,
        patch: GlobalControlEdit,
    ) {
        let planned = (|| {
            let identity = self.identity.as_ref().ok_or_else(|| {
                ServiceError::new(
                    ErrorKind::Disconnected,
                    "Identify the device before editing globals",
                )
            })?;
            if !capabilities_for_mode(self.mode).allows(OperationKind::WriteGlobals, identity) {
                return Err(ServiceError::new(
                    ErrorKind::Unverified,
                    "Backend profile does not permit global writes",
                ));
            }
            let before = self
                .globals
                .as_ref()
                .ok_or_else(|| {
                    ServiceError::new(ErrorKind::Conflict, "Read device globals before editing")
                })?
                .clone();
            if before.epoch != stamp.epoch || before.revision != revision {
                return Err(ServiceError::new(
                    ErrorKind::Conflict,
                    "Global revision changed; refresh before editing",
                ));
            }
            patch
                .request()
                .map_err(|e| ServiceError::invalid(e.to_string()))?;
            before
                .expected_after(&patch)
                .map_err(|e| ServiceError::invalid(e.to_string()))?;
            if self.mode == BackendMode::Hardware && !hardware_global_verified(&patch, &before) {
                return Err(ServiceError::new(
                    ErrorKind::Unverified,
                    "This global control has no matching connected validation profile",
                ));
            }
            Ok(before)
        })();
        match planned {
            Ok(before) => {
                self.event(DeviceEvent::EditProgress {
                    stamp,
                    stage: EditStage::Queued,
                });
                // The codec has no transaction identifier. Refresh all globals
                // before the write, and abort if any stable baseline byte changed.
                self.begin(
                    stamp,
                    RequestKind::GlobalPreflight { before, patch },
                    read_globals_request().expect("static query"),
                );
            }
            Err(error) => self.fail(stamp, error),
        }
    }
    fn begin(&mut self, stamp: RequestStamp, kind: RequestKind, raw: RawTransfer) {
        self.begin_packets(
            stamp,
            kind,
            encode_transfer(&raw).expect("validated request"),
        );
    }
    fn begin_packets(&mut self, stamp: RequestStamp, kind: RequestKind, packets: Vec<Vec<u8>>) {
        self.pending = Some(Pending {
            stamp,
            kind,
            sent: false,
            deadline: None,
            readback_at: None,
            readback_sent: false,
            readback_queued: false,
            mutation_attempted: false,
        });
        self.queue_send(packets);
    }
    fn queue_send(&mut self, packets: Vec<Vec<u8>>) {
        if self.now >= self.next_send_allowed {
            self.actions.push_back(TransportAction::Send { packets });
        } else {
            self.scheduled_packets = Some(packets);
        }
    }
    pub fn enumerated(&mut self, result: Result<Vec<PortPairCandidate>, ServiceError>) {
        match result {
            Ok(ports) => self.event(DeviceEvent::PortsChanged(ports)),
            Err(error) => self.diagnostic("discovery", error.message, None),
        }
        if self.state == ConnectionState::Discovering {
            self.state_changed(ConnectionState::Disconnected);
        }
    }
    pub fn opened(&mut self, result: Result<(), ServiceError>, now: Duration) {
        self.now = now;
        let Some(intent) = self.connecting_intent.take() else {
            return;
        };
        let stamp = RequestStamp {
            epoch: self.epoch,
            intent,
        };
        match result {
            Ok(()) => {
                self.state_changed(ConnectionState::Identifying);
                self.begin(
                    stamp,
                    RequestKind::Identity { synchronize: true },
                    identify_request(),
                );
            }
            Err(error) => {
                self.fail(stamp, error);
                self.actions.push_back(TransportAction::Close);
                self.state_changed(ConnectionState::Disconnected);
            }
        }
        self.tick(now);
    }
    /// Called only after all packets were attempted. Any OS error after starting
    /// a mutation is uncertain: a partial successful send cannot be undone here.
    pub fn sent(&mut self, result: Result<(), ServiceError>, now: Duration) {
        self.now = now;
        self.next_send_allowed = now + self.config.minimum_request_spacing;
        if let Err(error) = result {
            if let Some(pending) = &mut self.pending {
                // send() was attempted; even an error may follow a partial send.
                if pending.kind.is_mutation() {
                    pending.mutation_attempted = true;
                }
            }
            self.lost(error);
            return;
        }
        let Some(pending) = &mut self.pending else {
            return;
        };
        pending.sent = true;
        pending.deadline = Some(now + self.config.request_timeout);
        if pending.kind.is_mutation() && pending.readback_queued {
            pending.readback_sent = true;
        } else if pending.kind.is_mutation() {
            pending.mutation_attempted = true;
            pending.readback_at = Some(now + self.config.write_settle);
            pending.deadline = Some(now + self.config.write_settle + self.config.request_timeout);
            let stamp = pending.stamp;
            self.event(DeviceEvent::EditProgress {
                stamp,
                stage: EditStage::Sent,
            });
            self.event(DeviceEvent::EditProgress {
                stamp,
                stage: EditStage::AwaitingReadback,
            });
        }
    }
    fn validate_edit(
        &self,
        target: PresetTarget,
        edit: &ParameterEdit,
    ) -> Result<(), ServiceError> {
        let identity = self.identity.as_ref().ok_or_else(|| {
            ServiceError::new(
                ErrorKind::Disconnected,
                "Identify the device before editing",
            )
        })?;
        if !capabilities_for_mode(self.mode).allows(OperationKind::SetParameter, identity) {
            return Err(ServiceError::new(
                ErrorKind::Unverified,
                "Backend profile does not permit parameter writes",
            ));
        }
        if target != PresetTarget::EditBuffer {
            return Err(ServiceError::new(
                ErrorKind::Unverified,
                "Parameter-write profile covers the edit buffer only, never a stored slot",
            ));
        }
        let snapshot = self.snapshots.get(&target).ok_or_else(|| {
            ServiceError::new(
                ErrorKind::Conflict,
                "Refresh the edit buffer before writing",
            )
        })?;
        if snapshot.revision != edit.base_revision {
            return Err(ServiceError::new(
                ErrorKind::Conflict,
                "Edit is based on a stale confirmed revision",
            ));
        }
        let context = snapshot.context.ok_or_else(|| {
            ServiceError::new(
                ErrorKind::Protocol,
                "Snapshot has no immutable write context",
            )
        })?;
        if context.group != 0 || context.selector > 255 {
            return Err(ServiceError::new(
                ErrorKind::Protocol,
                "Current-buffer context is not validated",
            ));
        }
        let module = snapshot
            .view
            .module(edit.module)
            .ok_or_else(|| ServiceError::invalid("Module absent"))?;
        if module.model != edit.model || module.parameter(edit.parameter).is_none() {
            return Err(ServiceError::new(
                ErrorKind::Conflict,
                "Module/model/parameter no longer matches the confirmed snapshot",
            ));
        }
        let definition = self
            .catalog
            .model_by_id(edit.module.get(), edit.model.0)
            .ok_or_else(|| {
                ServiceError::new(
                    ErrorKind::Unverified,
                    "Unknown model has no catalog write profile",
                )
            })?;
        if u32::from(definition.xml_index) != edit.model_index.0 {
            return Err(ServiceError::new(
                ErrorKind::Conflict,
                "Parameter XML model Index does not match its Code/module",
            ));
        }
        let parameter = definition.parameter(edit.parameter.get()).ok_or_else(|| {
            ServiceError::new(
                ErrorKind::Unverified,
                "Parameter ID is not declared by this model",
            )
        })?;
        if self.mode == BackendMode::Hardware
            && !hardware_control_verified(edit.module.get(), definition.code, parameter)
        {
            return Err(ServiceError::new(
                ErrorKind::Unverified,
                "This control requires its specific sync, sentinel or choice hardware profile",
            ));
        }
        let bits: Vec<_> = module.parameters.iter().map(|v| v.0).collect();
        plan_control_request(
            definition,
            edit.parameter.get(),
            &bits,
            ControlRequest::RawBits(edit.value.0),
        )
        .map_err(|error| ServiceError::invalid(error.to_string()))?;
        // XML step describes the editor gesture grid. Physical EXP sends
        // fractional raw values, which must remain restorable without rounding.
        Ok(())
    }
    fn start_bound_model(
        &mut self,
        stamp: RequestStamp,
        revision: Revision,
        selection: BoundModelSelection,
        before_globals: Arc<DeviceGlobalState>,
        expected_auto_cab: bool,
    ) {
        let prepared = (|| {
            self.require_operation(OperationKind::SetModel)?;
            if self.mode != BackendMode::Hardware {
                return Err(ServiceError::unsupported(
                    "Dependent model trials require the physical normal-mode actor",
                ));
            }
            let before = self
                .snapshots
                .get(&PresetTarget::EditBuffer)
                .cloned()
                .ok_or_else(|| {
                    ServiceError::new(
                        ErrorKind::Conflict,
                        "Read the current buffer before dependent model selection",
                    )
                })?;
            if before.revision != revision {
                return Err(ServiceError::new(
                    ErrorKind::Conflict,
                    "Dependent model selection uses a stale confirmed preset revision",
                ));
            }
            let cached = self.globals.as_ref().ok_or_else(|| {
                ServiceError::new(
                    ErrorKind::Conflict,
                    "Read complete globals before dependent model selection",
                )
            })?;
            if before_globals.epoch != self.epoch
                || cached.epoch != self.epoch
                || before_globals.revision != cached.revision
                || before_globals.records().raw().payload() != cached.records().raw().payload()
            {
                return Err(ServiceError::new(
                    ErrorKind::Conflict,
                    "Dependent model global baseline differs from the actor's exact confirmed authority",
                ));
            }
            let (module, model, plan) = match selection {
                BoundModelSelection::Existing { module, model } => (
                    module,
                    model,
                    BoundModelPlan::Existing(DependentModelPlan::from_snapshot(
                        &before,
                        &before_globals,
                        module,
                        model,
                    )?),
                ),
                BoundModelSelection::Factory { model, profile } => {
                    let plan = FactoryAmpPlan::from_snapshot(&before, &before_globals, model)?;
                    if plan.profile != profile {
                        return Err(ServiceError::new(
                            ErrorKind::Conflict,
                            "Factory AMP requested profile differs from the actor's exact baseline",
                        ));
                    }
                    validate_factory_amp_catalog(&self.catalog, &plan, &before)?;
                    (plan.module, model, BoundModelPlan::Factory(plan))
                }
            };
            if expected_auto_cab != plan.auto_cab() {
                return Err(ServiceError::new(
                    ErrorKind::Conflict,
                    "Requested dependent model Auto CAB prediction differs from its bound global baseline",
                ));
            }
            if !plan.enabled() {
                return Err(ServiceError::new(
                    ErrorKind::Unverified,
                    "Dependent model candidate awaits root's bidirectional production-path acceptance",
                ));
            }
            let mut records = before.raw.records().to_vec();
            for (affected, target) in std::iter::once((module, model)).chain(
                plan.paired_cabinet()
                    .map(|cabinet| (ModuleId::new(5).unwrap(), cabinet)),
            ) {
                let definition = self
                    .catalog
                    .model_by_id(affected.get(), target.0)
                    .ok_or_else(|| {
                        ServiceError::invalid("Dependent model target is absent from the catalog")
                    })?;
                if affected == module
                    && plan.paired_cabinet().is_some()
                    && definition
                        .raw_attributes
                        .get("CABCode")
                        .and_then(|value| value.parse::<u32>().ok())
                        != plan.paired_cabinet().map(|value| value.0)
                {
                    return Err(ServiceError::new(
                        ErrorKind::Unverified,
                        "Dependent AMP paired cabinet differs from the recovered catalog",
                    ));
                }
                let defaults = gp200_catalog::ModelDefaultPlan::from_definition(definition);
                if !defaults.unknown_defaults.is_empty() {
                    return Err(ServiceError::new(
                        ErrorKind::Unverified,
                        "Dependent model target has unrecovered defaults",
                    ));
                }
                defaults
                    .apply_to_module_body(&mut records[3 + usize::from(affected.get())].body)
                    .map_err(ServiceError::invalid)?;
            }
            let context = before.context.expect("profile validates context");
            let command = gp200_protocol::model_write_record(module.get(), model.0)
                .map_err(mutation_error)?;
            let request = contextual_write_request(
                gp200_protocol::WireContext {
                    group: context.group,
                    selector: context.selector,
                },
                &[command],
            )
            .map_err(mutation_error)?;
            let expected = RawTransfer::from_records(before.raw.kind(), &records)
                .map_err(|error| ServiceError::invalid(error.to_string()))?;
            if let BoundModelPlan::Factory(candidate) = &plan
                && candidate.projected_raw(&before)?.payload() != expected.payload()
            {
                return Err(ServiceError::new(
                    ErrorKind::Unverified,
                    "Factory AMP descriptor projection differs from recovered catalog defaults",
                ));
            }
            Ok(DependentMutation {
                before,
                before_globals,
                plan,
                request,
                expected,
            })
        })();
        match prepared {
            Ok(mutation) => {
                self.event(DeviceEvent::EditProgress {
                    stamp,
                    stage: EditStage::Queued,
                });
                self.begin(
                    stamp,
                    RequestKind::DependentGlobalsPreflight(mutation),
                    read_globals_request().expect("static query"),
                );
            }
            Err(error) => self.fail(stamp, error),
        }
    }
    fn start_mutation(
        &mut self,
        stamp: RequestStamp,
        target: PresetTarget,
        revision: Revision,
        patch: EditPatch,
    ) {
        if matches!(patch, EditPatch::SetSync { .. }) {
            self.start_sync(stamp, target, revision, patch);
            return;
        }
        let native_metadata_preflight = self.mode == BackendMode::Hardware
            && matches!(&patch, EditPatch::SetMetadata(metadata) if metadata.native_pan.is_some() || metadata.native_style.is_some() || metadata.fx_send_level.is_some() || metadata.fx_return_level.is_some() || metadata.fx_loop_mode.is_some());
        match self.prepare_mutation(target, revision, patch) {
            Ok((request, expected)) => {
                self.event(DeviceEvent::EditProgress {
                    stamp,
                    stage: EditStage::Queued,
                });
                let mutation = ValidatedMutation {
                    target,
                    expected,
                    policy: MutationPolicy::Exact,
                };
                if native_metadata_preflight {
                    let before = self.snapshots[&target].clone();
                    self.begin(
                        stamp,
                        RequestKind::MetadataPreflight {
                            before,
                            request,
                            mutation,
                        },
                        read_current_request(),
                    );
                    return;
                }
                self.begin(stamp, RequestKind::Mutation(mutation), request);
            }
            Err(error) => self.fail(stamp, error),
        }
    }
    fn start_sync(
        &mut self,
        stamp: RequestStamp,
        target: PresetTarget,
        revision: Revision,
        patch: EditPatch,
    ) {
        let prepared = (|| {
            self.require_operation(OperationKind::SetParameter)?;
            let EditPatch::SetSync {
                module,
                model,
                model_index,
                power_parameter,
                value_parameter,
                enabled,
                ordinary_restore,
            } = patch
            else {
                return Err(ServiceError::invalid("Expected explicit Sync action"));
            };
            if target != PresetTarget::EditBuffer {
                return Err(ServiceError::new(
                    ErrorKind::Unverified,
                    "Sync profile covers the current buffer only",
                ));
            }
            let snapshot = self.snapshots.get(&target).ok_or_else(|| {
                ServiceError::new(ErrorKind::Conflict, "Refresh current buffer before Sync")
            })?;
            if snapshot.revision != revision {
                return Err(ServiceError::new(
                    ErrorKind::Conflict,
                    "Sync action uses a stale confirmed revision",
                ));
            }
            let context = snapshot
                .context
                .filter(|c| c.group == 0 && c.selector <= 255)
                .ok_or_else(|| {
                    ServiceError::new(
                        ErrorKind::Protocol,
                        "Sync needs a validated immutable context",
                    )
                })?;
            let module_snapshot = snapshot
                .view
                .module(module)
                .filter(|m| m.model == model)
                .ok_or_else(|| {
                    ServiceError::new(ErrorKind::Conflict, "Sync module/model changed")
                })?;
            let definition = self
                .catalog
                .model_by_id(module.get(), model.0)
                .ok_or_else(|| ServiceError::new(ErrorKind::Unverified, "Sync model is unknown"))?;
            if u32::from(definition.xml_index) != model_index.0
                || sync_value_parameter(definition, power_parameter.get())
                    .map_err(|e| ServiceError::invalid(e.to_string()))?
                    != value_parameter.get()
            {
                return Err(ServiceError::new(
                    ErrorKind::Conflict,
                    "Sync model Index or paired XML ID differs from the catalog",
                ));
            }
            if self.mode == BackendMode::Hardware
                && !hardware_sync_verified(
                    module.get(),
                    model.0,
                    power_parameter.get(),
                    value_parameter.get(),
                )
            {
                return Err(ServiceError::new(
                    ErrorKind::Unverified,
                    "This exact Sync pair awaits its hardware restoration profile",
                ));
            }
            let bits: Vec<_> = module_snapshot.parameters.iter().map(|v| v.0).collect();
            let plan = plan_control_request(
                definition,
                power_parameter.get(),
                &bits,
                ControlRequest::Sync {
                    enabled,
                    ordinary_restore: ordinary_restore
                        .map(|v| OrdinaryRestore::ExplicitUserValue(v.0)),
                },
            )
            .map_err(|e| ServiceError::invalid(e.to_string()))?;
            let mut packets = Vec::new();
            let mut expected_records = snapshot.raw.records().to_vec();
            let expected_module = expected_records
                .iter_mut()
                .find(|r| r.command == 0x14 && r.body.len() == 68 && r.body[0] == module.get())
                .ok_or_else(|| ServiceError::invalid("Sync module raw record missing"))?;
            for step in plan.steps {
                let request = parameter_write_request(
                    gp200_protocol::WireContext {
                        group: context.group,
                        selector: context.selector,
                    },
                    ParameterWrite {
                        module_id: module.get(),
                        xml_param_id: step.parameter_id,
                        model_index: WireModelIndex(model_index.0),
                        value: F32Bits(step.raw_bits),
                    },
                )
                .map_err(|error| ServiceError::invalid(error.to_string()))?;
                // Independent framed transfers preserve the vendor's Power
                // first/value second order, with the backend's packet pacing.
                packets.extend(
                    encode_transfer(&request).map_err(|e| ServiceError::invalid(e.to_string()))?,
                );
                let offset = 8 + usize::from(step.parameter_id) * 4;
                expected_module.body[offset..offset + 4]
                    .copy_from_slice(&step.raw_bits.to_le_bytes());
            }
            let expected = RawTransfer::from_records(snapshot.raw.kind(), &expected_records)
                .map_err(|e| ServiceError::invalid(e.to_string()))?;
            Ok((packets, expected))
        })();
        match prepared {
            Ok((packets, expected)) => {
                self.event(DeviceEvent::EditProgress {
                    stamp,
                    stage: EditStage::Queued,
                });
                self.begin_packets(
                    stamp,
                    RequestKind::Mutation(ValidatedMutation {
                        target,
                        expected,
                        policy: MutationPolicy::Exact,
                    }),
                    packets,
                );
            }
            Err(error) => self.fail(stamp, error),
        }
    }
    fn prepare_mutation(
        &self,
        target: PresetTarget,
        revision: Revision,
        patch: EditPatch,
    ) -> Result<(RawTransfer, RawTransfer), ServiceError> {
        if target != PresetTarget::EditBuffer {
            return Err(ServiceError::new(
                ErrorKind::Unverified,
                "Incremental writes cover the current buffer only",
            ));
        }
        let snapshot = self.snapshots.get(&target).ok_or_else(|| {
            ServiceError::new(ErrorKind::Conflict, "Refresh the target before editing")
        })?;
        if snapshot.revision != revision {
            return Err(ServiceError::new(
                ErrorKind::Conflict,
                "Edit is based on a stale confirmed revision",
            ));
        }
        let context = snapshot
            .context
            .filter(|c| c.group == 0 && c.selector <= 255)
            .ok_or_else(|| {
                ServiceError::new(
                    ErrorKind::Protocol,
                    "Current target has no validated immutable context",
                )
            })?;
        let identity = self.identity.as_ref().ok_or_else(|| {
            ServiceError::new(
                ErrorKind::Disconnected,
                "Identify the device before editing",
            )
        })?;
        if !capabilities_for_mode(self.mode).allows(patch.operation(), identity) {
            return Err(ServiceError::new(
                ErrorKind::Unverified,
                "This edit still awaits its operation-specific live restoration proof",
            ));
        }
        let mut expected = snapshot.raw.records().to_vec();
        let mut commands = Vec::<RawRecord>::new();
        match patch {
            EditPatch::SetPatchAssignment(edit) => {
                if self.mode == BackendMode::Hardware
                    && let Some(reason) = hardware_assignment_blocked_reason(snapshot, &edit)
                {
                    return Err(ServiceError::new(ErrorKind::Unverified, reason));
                }
                validate_assignment_reference(&self.catalog, snapshot, &edit)?;
                let record = PatchAssignments::edited_record(snapshot, &edit)?;
                let matching: Vec<_> = expected
                    .iter()
                    .enumerate()
                    .filter(|(_, r)| {
                        r.command == record.command && r.body.first() == record.body.first()
                    })
                    .map(|(index, _)| index)
                    .collect();
                if matching.len() != 1 {
                    return Err(ServiceError::invalid(
                        "Assignment identity is absent or ambiguous",
                    ));
                }
                if matches!(edit, AssignmentEdit::FxLoopPositions { .. }) {
                    if u16::from_le_bytes([record.body[0], record.body[1]]) != context.selector {
                        return Err(ServiceError::invalid(
                            "FX routing embedded slot differs from its current context",
                        ));
                    }
                } else {
                    gp200_protocol::patch_assignment_write_request(
                        gp200_protocol::WireContext {
                            group: context.group,
                            selector: context.selector,
                        },
                        &record,
                    )
                    .map_err(mutation_error)?;
                }
                expected[matching[0]] = record.clone();
                if let AssignmentEdit::ExpressionRange {
                    slot,
                    minimum,
                    maximum,
                } = edit
                {
                    let previous = gp200_protocol::ExpressionAssignmentRecord::decode(
                        &snapshot.raw.records()[matching[0]],
                    )
                    .map_err(|error| ServiceError::invalid(error.to_string()))?;
                    // Matching assignments may be coupled by the native endpoint
                    // path; this profile covers an independent reference only.
                    if snapshot
                        .view
                        .patch_assignments
                        .expressions
                        .iter()
                        .any(|entry| {
                            entry.record.slot != slot
                                && entry.record.slot.group() == slot.group()
                                && entry.record.module_raw == previous.module_raw
                                && entry.record.parameter_raw == previous.parameter_raw
                        })
                    {
                        return Err(ServiceError::new(
                            ErrorKind::Unverified,
                            "Coupled EXP endpoint references need a dedicated readback profile",
                        ));
                    }
                    for (endpoint, before, value) in [
                        (
                            gp200_protocol::ExpressionEndpoint::Minimum,
                            previous.minimum,
                            minimum,
                        ),
                        (
                            gp200_protocol::ExpressionEndpoint::Maximum,
                            previous.maximum,
                            maximum,
                        ),
                    ] {
                        if value != before {
                            commands.push(
                                gp200_protocol::expression_endpoint_record(
                                    slot, endpoint, value, [0; 2],
                                )
                                .map_err(mutation_error)?,
                            );
                        }
                    }
                    if commands.is_empty() {
                        return Err(ServiceError::invalid("EXP endpoints did not change"));
                    }
                } else {
                    let mut wire_record = record;
                    if matches!(
                        edit,
                        AssignmentEdit::ExpressionTarget {
                            target: ExpressionAssignmentTarget::Off,
                            ..
                        } | AssignmentEdit::ExpressionTargetAndRange {
                            target: ExpressionAssignmentTarget::Off,
                            ..
                        }
                    ) {
                        // Native OFF sends parameter FF; hardware normalizes
                        // its readback to zero. Expected state remains canonical.
                        wire_record.body[2..4].copy_from_slice(&255u16.to_le_bytes());
                    }
                    commands.push(wire_record);
                }
            }
            EditPatch::SetModel { module, model } => {
                if self.mode == BackendMode::Hardware
                    && let Some(reason) = hardware_model_blocked_reason(snapshot, module, model)
                {
                    return Err(ServiceError::new(ErrorKind::Unverified, reason));
                }
                let definition = self
                    .catalog
                    .model_by_id(module.get(), model.0)
                    .ok_or_else(|| ServiceError::invalid("Requested model absent from catalog"))?;
                let plan = gp200_catalog::ModelDefaultPlan::from_definition(definition);
                if self.mode == BackendMode::Hardware && !plan.unknown_defaults.is_empty() {
                    return Err(ServiceError::new(
                        ErrorKind::Unverified,
                        "This model has unrecovered defaults; exact hardware readback cannot be predicted",
                    ));
                }
                let record = expected
                    .iter_mut()
                    .find(|r| r.command == 0x14 && r.body.len() == 68 && r.body[0] == module.get())
                    .ok_or_else(|| ServiceError::invalid("Module record absent"))?;
                plan.apply_to_module_body(&mut record.body)
                    .map_err(ServiceError::invalid)?;
                commands.push(
                    gp200_protocol::model_write_record(module.get(), model.0)
                        .map_err(mutation_error)?,
                );
            }
            EditPatch::SetEnabled { module, enabled } => {
                let r = expected
                    .iter_mut()
                    .find(|r| r.command == 0x14 && r.body.len() == 68 && r.body[0] == module.get())
                    .ok_or_else(|| ServiceError::invalid("Module record absent"))?;
                if r.body[1] > 1 {
                    return Err(ServiceError::new(
                        ErrorKind::Unverified,
                        "Unknown enabled-state representation",
                    ));
                }
                r.body[1] = u8::from(enabled);
                commands.push(enabled_write_record(module.get(), enabled).map_err(mutation_error)?);
            }
            EditPatch::MoveModule { module, before } => {
                let r = expected
                    .iter_mut()
                    .find(|r| r.command == 8 && r.body.len() == 16)
                    .ok_or_else(|| ServiceError::invalid("Chain record absent"))?;
                if u16::from_le_bytes([r.body[0], r.body[1]]) != context.selector {
                    return Err(ServiceError::new(
                        ErrorKind::Conflict,
                        "Chain embedded slot differs from its immutable context",
                    ));
                }
                let mut order = r.body[4..15].to_vec();
                let mut sorted = order.clone();
                sorted.sort_unstable();
                if sorted != (0..11).collect::<Vec<_>>() {
                    return Err(ServiceError::invalid(
                        "Chain is not a complete module permutation",
                    ));
                }
                if before != Some(module) {
                    order.retain(|id| *id != module.get());
                    let pos = before
                        .and_then(|id| order.iter().position(|candidate| *candidate == id.get()))
                        .unwrap_or(order.len());
                    order.insert(pos, module.get());
                }
                let order: [u8; 11] = order
                    .try_into()
                    .map_err(|_| ServiceError::invalid("Invalid chain length"))?;
                *r = chain_write_record(r, &order).map_err(mutation_error)?;
                commands.push(r.clone());
            }
            EditPatch::SetMetadata(metadata) => {
                metadata.validate_native()?;
                if metadata.author.is_some()
                    || metadata.style.is_some()
                    || metadata.notes.is_some()
                    || metadata.pan.is_some()
                {
                    return Err(ServiceError::new(
                        ErrorKind::Unverified,
                        "Author, notes, freeform style and legacy float Pan are local annotations",
                    ));
                }
                let r = expected
                    .iter_mut()
                    .find(|r| r.command == 2 && r.body.len() == 88)
                    .ok_or_else(|| ServiceError::invalid("Metadata record absent"))?;
                if u16::from_le_bytes([r.body[0], r.body[1]]) != context.selector {
                    return Err(ServiceError::new(
                        ErrorKind::Conflict,
                        "Metadata embedded slot differs from its immutable context",
                    ));
                }
                if self.mode == BackendMode::Hardware {
                    let baseline = MetadataPatch {
                        native_pan: Some(PatchPan::from_raw(r.body[7] as i8)),
                        native_style: Some(NativeStyleId::from_raw(u16::from_le_bytes([
                            r.body[8], r.body[9],
                        ]))),
                        ..Default::default()
                    };
                    metadata.hardware_profile(&baseline)?;
                }
                if let Some(name) = metadata.name {
                    let rename =
                        rename_write_record(context.selector, &name).map_err(mutation_error)?;
                    r.body[16..32].copy_from_slice(&rename.body[4..20]);
                    commands.push(rename);
                }
                if let Some(tempo) = metadata.tempo {
                    if !(20..=300).contains(&tempo) {
                        return Err(ServiceError::invalid("Tempo must be 20..300 BPM"));
                    }
                    r.body[2..4].copy_from_slice(&tempo.to_le_bytes());
                    commands.push(metadata_write_record(1, tempo).map_err(mutation_error)?);
                }
                if let Some(level) = metadata.level {
                    if level > 100 {
                        return Err(ServiceError::invalid("Preset level must be 0..100"));
                    }
                    r.body[4..6].copy_from_slice(&level.to_le_bytes());
                    commands.push(metadata_write_record(0, level).map_err(mutation_error)?);
                }
                if let Some(pan) = metadata.native_pan {
                    r.body[7] = pan.raw() as u8;
                    commands.push(
                        metadata_write_record(6, i16::from(pan.raw()) as u16)
                            .map_err(mutation_error)?,
                    );
                }
                if let Some(style) = metadata.native_style {
                    r.body[8..10].copy_from_slice(&style.raw().to_le_bytes());
                    commands.push(metadata_write_record(2, style.raw()).map_err(mutation_error)?);
                }
                for (selector, value, offset) in [
                    (3, metadata.fx_send_level, 10),
                    (4, metadata.fx_return_level, 12),
                ] {
                    if let Some(value) = value {
                        r.body[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
                        commands
                            .push(metadata_write_record(selector, value).map_err(mutation_error)?);
                    }
                }
                if let Some(mode) = metadata.fx_loop_mode {
                    r.body[14] = mode.raw();
                    commands.push(
                        metadata_write_record(5, u16::from(mode.raw())).map_err(mutation_error)?,
                    );
                }
                if commands.is_empty() {
                    return Err(ServiceError::invalid(
                        "Metadata edit has no supported changes",
                    ));
                }
            }
            _ => {
                return Err(ServiceError::new(
                    ErrorKind::Unverified,
                    "This edit still awaits its operation-specific live restoration proof",
                ));
            }
        }
        let request = contextual_write_request(
            gp200_protocol::WireContext {
                group: context.group,
                selector: context.selector,
            },
            &commands,
        )
        .map_err(mutation_error)?;
        let expected = RawTransfer::from_records(snapshot.raw.kind(), &expected)
            .map_err(|e| ServiceError::invalid(e.to_string()))?;
        Ok((request, expected))
    }
    fn start_full_write(
        &mut self,
        stamp: RequestStamp,
        slot: SlotId,
        revision: Revision,
        source: &PresetSnapshot,
    ) {
        let target = PresetTarget::Stored(slot);
        let result = (|| {
            let identity = self.identity.as_ref().ok_or_else(|| {
                ServiceError::new(
                    ErrorKind::Disconnected,
                    "Identify the device before writing",
                )
            })?;
            if !capabilities_for_mode(self.mode).allows(OperationKind::PresetImport, identity)
                || source.device_model != DeviceModel::Gp200
            {
                return Err(ServiceError::new(
                    ErrorKind::Unverified,
                    "Full-preset profile requires GP-200 Normal mode and a GP-200 snapshot",
                ));
            }
            if self.mode == BackendMode::Hardware {
                let current = self
                    .snapshots
                    .get(&PresetTarget::EditBuffer)
                    .ok_or_else(|| {
                        ServiceError::new(
                            ErrorKind::Conflict,
                            "Refresh the current buffer before a persistent full write",
                        )
                    })?;
                if !current
                    .context
                    .is_some_and(|c| c.group == 0 && c.selector <= 255 && c.selector != slot.get())
                {
                    return Err(ServiceError::new(
                        ErrorKind::Unverified,
                        "Full-stored hardware profile currently covers an inactive target; active-buffer side effects need separate proof",
                    ));
                }
            }
            let previous = self.snapshots.get(&target).ok_or_else(|| {
                ServiceError::new(
                    ErrorKind::Conflict,
                    "Read and review the stored target before overwriting it",
                )
            })?;
            if previous.revision != revision
                || previous.context
                    != Some(WireContext {
                        group: 1,
                        selector: slot.get(),
                    })
            {
                return Err(ServiceError::new(
                    ErrorKind::Conflict,
                    "Stored target/context revision changed before the write",
                ));
            }
            let request = gp200_protocol::full_stored_write_request(slot.get(), &source.raw)
                .map_err(mutation_error)?;
            let mut expected_records = vec![request.records()[0].clone()];
            expected_records.extend_from_slice(&request.records()[2..]);
            let expected = RawTransfer::from_records(gp200_protocol::WRITE_KIND, &expected_records)
                .map_err(|e| ServiceError::invalid(e.to_string()))?;
            Ok((request, expected))
        })();
        match result {
            Ok((request, expected)) => {
                self.event(DeviceEvent::EditProgress {
                    stamp,
                    stage: EditStage::Queued,
                });
                self.begin(
                    stamp,
                    RequestKind::Mutation(ValidatedMutation {
                        target,
                        expected,
                        policy: MutationPolicy::Exact,
                    }),
                    request,
                );
            }
            Err(error) => self.fail(stamp, error),
        }
    }
    fn start_clear(&mut self, stamp: RequestStamp, slot: SlotId, revision: Revision) {
        let prepared = (|| {
            self.require_operation(OperationKind::ClearPreset)?;
            let current = self
                .snapshots
                .get(&PresetTarget::EditBuffer)
                .ok_or_else(|| {
                    ServiceError::new(
                        ErrorKind::Conflict,
                        "Refresh the current buffer before clearing a stored slot",
                    )
                })?;
            if !current
                .context
                .is_some_and(|c| c.group == 0 && c.selector <= 255 && c.selector != slot.get())
            {
                return Err(ServiceError::new(
                    ErrorKind::Unverified,
                    "Clear profile covers an inactive stored slot only",
                ));
            }
            let previous = self
                .snapshots
                .get(&PresetTarget::Stored(slot))
                .ok_or_else(|| {
                    ServiceError::new(
                        ErrorKind::Conflict,
                        "Read and review the stored target before clearing it",
                    )
                })?;
            if previous.revision != revision
                || previous.context
                    != Some(WireContext {
                        group: 1,
                        selector: slot.get(),
                    })
            {
                return Err(ServiceError::new(
                    ErrorKind::Conflict,
                    "Stored target/context revision changed before clear",
                ));
            }
            let default = RawTransfer::from_payload(
                gp200_protocol::WRITE_KIND,
                include_bytes!("fixtures/device-cleared.payload.bin").to_vec(),
            )
            .expect("verified device-default fixture");
            let expected = retarget_preset(&default, 1, slot.get())?;
            let request =
                gp200_protocol::clear_stored_request(slot.get()).map_err(mutation_error)?;
            Ok((request, expected))
        })();
        match prepared {
            Ok((request, expected)) => {
                self.event(DeviceEvent::EditProgress {
                    stamp,
                    stage: EditStage::Queued,
                });
                self.begin(
                    stamp,
                    RequestKind::Mutation(ValidatedMutation {
                        target: PresetTarget::Stored(slot),
                        expected,
                        policy: MutationPolicy::Exact,
                    }),
                    request,
                );
            }
            Err(error) => self.fail(stamp, error),
        }
    }
    fn require_operation(&self, operation: OperationKind) -> Result<(), ServiceError> {
        let identity = self.identity.as_ref().ok_or_else(|| {
            ServiceError::new(ErrorKind::Disconnected, "Identify the device first")
        })?;
        if !capabilities_for_mode(self.mode).allows(operation, identity) {
            return Err(ServiceError::new(
                ErrorKind::Unverified,
                "Backend validation profile does not permit this operation",
            ));
        }
        Ok(())
    }
    fn start_selection(&mut self, stamp: RequestStamp, slot: SlotId) {
        let prepared = (|| {
            self.require_operation(OperationKind::SelectStoredPreset)?;
            let source = self
                .snapshots
                .get(&PresetTarget::Stored(slot))
                .ok_or_else(|| {
                    ServiceError::new(
                        ErrorKind::Conflict,
                        "Read and review the stored target before auditioning",
                    )
                })?;
            if source.context
                != Some(WireContext {
                    group: 1,
                    selector: slot.get(),
                })
            {
                return Err(ServiceError::new(
                    ErrorKind::Conflict,
                    "Stored target context is invalid",
                ));
            }
            let expected = retarget_preset(&source.raw, 0, slot.get())?;
            let request =
                gp200_protocol::select_stored_request(slot.get()).map_err(mutation_error)?;
            Ok((request, expected))
        })();
        match prepared {
            Ok((request, expected)) => {
                self.event(DeviceEvent::EditProgress {
                    stamp,
                    stage: EditStage::Queued,
                });
                self.begin(
                    stamp,
                    RequestKind::Mutation(ValidatedMutation {
                        target: PresetTarget::EditBuffer,
                        expected,
                        policy: MutationPolicy::Selection,
                    }),
                    request,
                );
            }
            Err(error) => self.fail(stamp, error),
        }
    }
    fn start_save(&mut self, stamp: RequestStamp, slot: SlotId, revision: Revision) {
        let prepared = (|| {
            self.require_operation(OperationKind::SavePreset)?;
            let buffer = self
                .snapshots
                .get(&PresetTarget::EditBuffer)
                .ok_or_else(|| {
                    ServiceError::new(
                        ErrorKind::Conflict,
                        "Refresh the current buffer before saving",
                    )
                })?;
            if buffer.revision != revision
                || !buffer
                    .context
                    .is_some_and(|c| c.group == 0 && c.selector <= 255)
            {
                return Err(ServiceError::new(
                    ErrorKind::Conflict,
                    "Save requires the exact confirmed current-buffer revision/context",
                ));
            }
            let previous_target =
                self.snapshots
                    .get(&PresetTarget::Stored(slot))
                    .ok_or_else(|| {
                        ServiceError::new(
                            ErrorKind::Conflict,
                            "Read and review the stored target before overwriting it",
                        )
                    })?;
            if previous_target.context
                != Some(WireContext {
                    group: 1,
                    selector: slot.get(),
                })
            {
                return Err(ServiceError::new(
                    ErrorKind::Conflict,
                    "Stored target context is invalid",
                ));
            }
            let metadata = buffer
                .raw
                .records()
                .iter()
                .find(|r| r.command == 2 && r.body.len() == 88)
                .ok_or_else(|| ServiceError::invalid("Current metadata record is missing"))?;
            let name: [u8; 16] = metadata.body[16..32].try_into().expect("bounded name");
            let request =
                gp200_protocol::save_buffer_request(slot.get(), name).map_err(mutation_error)?;
            let expected = retarget_preset(&buffer.raw, 1, slot.get())?;
            let current_expected = retarget_preset(&buffer.raw, 0, slot.get())?;
            Ok((request, expected, current_expected))
        })();
        match prepared {
            Ok((request, expected, current_expected)) => {
                self.event(DeviceEvent::EditProgress {
                    stamp,
                    stage: EditStage::Queued,
                });
                self.begin(
                    stamp,
                    RequestKind::Mutation(ValidatedMutation {
                        target: PresetTarget::Stored(slot),
                        expected,
                        policy: MutationPolicy::SaveThenCurrent { current_expected },
                    }),
                    request,
                );
            }
            Err(error) => self.fail(stamp, error),
        }
    }
    fn start_edit(&mut self, edit: ParameterEdit) {
        let context = self.snapshots[&PresetTarget::EditBuffer]
            .context
            .expect("validated context");
        let raw = parameter_write_request(
            gp200_protocol::WireContext {
                group: context.group,
                selector: context.selector,
            },
            ParameterWrite {
                module_id: edit.module.get(),
                xml_param_id: edit.parameter.get(),
                model_index: WireModelIndex(edit.model_index.0),
                value: edit.value,
            },
        )
        .expect("validated parameter");
        self.event(DeviceEvent::EditProgress {
            stamp: edit.stamp,
            stage: EditStage::Queued,
        });
        self.begin(edit.stamp, RequestKind::Write(edit), raw);
    }
    pub fn receive(&mut self, epoch: SessionEpoch, bytes: &[u8], now: Duration) {
        self.now = now;
        if self.capture.is_some() {
            self.capture_abort(ServiceError::new(
                ErrorKind::Conflict,
                "Unstamped normal receive bypassed actual capture source",
            ));
            return;
        }
        if epoch != self.epoch {
            self.diagnostic(
                "stale-midi",
                "Ignored callback from an old MIDI connection",
                None,
            );
            return;
        }
        self.trace_wire("midi-rx", bytes, now);
        let mut other_midi = Vec::new();
        let mut realtime = Vec::new();
        for event in self.decoder.feed(bytes) {
            match event {
                StreamEvent::Packet(packet) => {
                    // A whole one-packet notification can arrive between chunks
                    // of a snapshot. Decode it separately rather than replacing
                    // or corrupting the incomplete snapshot assembly.
                    if packet.offset == 0
                        && packet.payload.len() == packet.total
                        && self.assembler.progress().is_some()
                    {
                        match RawTransfer::from_payload(packet.kind, packet.payload) {
                            Ok(raw) => self.transfer(raw, now),
                            Err(error) => self.diagnostic("records", error.to_string(), None),
                        }
                        continue;
                    }
                    match self.assembler.add(&packet, now) {
                        Ok(Some(raw)) => self.transfer(raw, now),
                        Ok(None) => {}
                        Err(error) => {
                            self.diagnostic("reassembly", error.to_string(), None);
                            if self.pending.is_some() {
                                self.lost(ServiceError::new(
                                    ErrorKind::Protocol,
                                    format!("Contradictory or invalid reply assembly: {error}"),
                                ));
                                return;
                            }
                        }
                    }
                }
                StreamEvent::Realtime(byte) => {
                    // Clock/active sensing do not wake the GUI for every tick.
                    if byte != 0xf8 && byte != 0xfe {
                        realtime.push(byte);
                    }
                }
                StreamEvent::OtherMidi(byte) => other_midi.push(byte),
                StreamEvent::UnrecognizedSysex(bytes) => self.diagnostic(
                    "foreign-sysex",
                    "Non-normal SysEx retained; no firmware/legacy interpretation",
                    Some(bytes),
                ),
                StreamEvent::Error(error) => {
                    self.diagnostic("midi-parser", error.to_string(), None);
                    if self.pending.is_some() {
                        self.lost(ServiceError::new(
                            ErrorKind::Protocol,
                            format!("Malformed reply during an owned request: {error}"),
                        ));
                        return;
                    }
                }
            }
        }
        if !other_midi.is_empty() {
            self.diagnostic(
                "other-midi",
                format!("{} non-SysEx MIDI bytes", other_midi.len()),
                Some(other_midi),
            );
        }
        if !realtime.is_empty() {
            self.diagnostic(
                "midi-realtime",
                format!("{} realtime MIDI bytes", realtime.len()),
                Some(realtime),
            );
        }
        self.tick(now);
    }
    fn transfer(&mut self, raw: RawTransfer, _now: Duration) {
        if raw.kind() != gp200_protocol::WRITE_KIND {
            self.notification(raw);
            return;
        }
        let Some(pending) = &self.pending else {
            self.notification(raw);
            return;
        };
        if !pending.sent {
            self.notification(raw);
            return;
        }
        match &pending.kind {
            RequestKind::ReadAssetMetadata(slot) => {
                let slot = *slot;
                let stamp = pending.stamp;
                // Status/rename and other-bank replies never satisfy this read.
                if !raw
                    .records()
                    .iter()
                    .any(|record| record.command == slot.bank().metadata_command())
                {
                    self.notification(raw);
                    return;
                }
                // Wrong-slot notifications may precede the actual reply; retain
                // them without accepting or applying their metadata.
                if raw.records().len() == 1 && raw.records()[0].body.len() == 24 {
                    let record = &raw.records()[0];
                    let index = u16::from_le_bytes([record.body[0], record.body[1]]);
                    if index != slot.index() {
                        self.notification(raw);
                        return;
                    }
                }
                let revision = Revision(self.asset_metadata_revision.0 + 1);
                let parsed = DeviceAssetMetadata::from_transfer(
                    slot,
                    stamp,
                    revision,
                    self.snapshot_source(),
                    self.now.as_millis().min(u128::from(u64::MAX)) as u64,
                    raw,
                );
                match parsed {
                    Ok(metadata) => {
                        self.asset_metadata_revision = revision;
                        self.pending = None;
                        let metadata = Arc::new(metadata);
                        self.job_asset_baselines
                            .retain(|old| old.slot() != metadata.slot());
                        self.job_asset_baselines.push(Arc::clone(&metadata));
                        self.event(DeviceEvent::AssetMetadataReceived { stamp, metadata });
                    }
                    Err(error) => self.lost(error),
                }
            }
            RequestKind::ReadGlobals => {
                if !valid_globals(&raw) {
                    self.notification(raw);
                    return;
                }
                let stamp = pending.stamp;
                self.pending = None;
                self.store_globals(raw, stamp);
            }
            RequestKind::DependentGlobalsPreflight(mutation) => {
                if !valid_globals(&raw) {
                    self.notification(raw);
                    return;
                }
                let stamp = pending.stamp;
                let mutation = mutation.clone();
                let matching =
                    dependent_globals_match(mutation.before_globals.records().raw(), &raw, None)
                        .unwrap_or(false)
                        && dependent_auto_cab(&raw).ok() == Some(mutation.plan.auto_cab());
                self.pending = None;
                if !matching {
                    self.store_globals(raw, stamp);
                    self.edit_failure(stamp, ServiceError::new(ErrorKind::Conflict, "Dependent model globals changed during preflight; no model command was sent"), false);
                    return;
                }
                // Successful preflights are internal: a current/source Code
                // must not be delivered as the pending target's readback.
                self.begin(
                    stamp,
                    RequestKind::DependentCurrentPreflight(mutation),
                    read_current_request(),
                );
            }
            RequestKind::DependentCurrentPreflight(mutation) => {
                if !valid_preset(&raw, PresetTarget::EditBuffer) {
                    self.notification(raw);
                    return;
                }
                let stamp = pending.stamp;
                let mutation = mutation.clone();
                let matching = dependent_preset_shape(&raw).is_ok()
                    && preset_records_equal(&mutation.before.raw, &raw);
                self.pending = None;
                if !matching {
                    self.store_snapshot(raw, PresetTarget::EditBuffer, stamp);
                    self.edit_failure(stamp, ServiceError::new(ErrorKind::Conflict, "Dependent model current buffer changed during preflight; no model command was sent"), false);
                    return;
                }
                let request = mutation.request.clone();
                self.begin(
                    stamp,
                    RequestKind::DependentModelMutation(mutation),
                    request,
                );
            }
            RequestKind::DependentModelMutation(mutation) => {
                if !pending.readback_sent || !valid_preset(&raw, PresetTarget::EditBuffer) {
                    self.notification(raw);
                    return;
                }
                let stamp = pending.stamp;
                let mutation = mutation.clone();
                let preset_verified = dependent_preset_shape(&raw).is_ok()
                    && preset_records_equal(&mutation.expected, &raw);
                self.pending = None;
                self.store_snapshot(raw, PresetTarget::EditBuffer, stamp);
                self.begin(
                    stamp,
                    RequestKind::DependentGlobalsPostflight {
                        mutation,
                        preset_verified,
                    },
                    read_globals_request().expect("static query"),
                );
                // This is readback of an already attempted mutation, never a
                // second send. Loss here must remain indeterminate.
                let pending = self.pending.as_mut().expect("new postflight");
                pending.readback_queued = true;
                pending.mutation_attempted = true;
            }
            RequestKind::DependentGlobalsPostflight {
                mutation,
                preset_verified,
            } => {
                if !pending.readback_sent || !valid_globals(&raw) {
                    self.notification(raw);
                    return;
                }
                let stamp = pending.stamp;
                let preset_verified = *preset_verified;
                let globals_verified =
                    dependent_globals_match(mutation.before_globals.records().raw(), &raw, Some(1))
                        .unwrap_or(false)
                        && dependent_auto_cab(&raw).ok() == Some(mutation.plan.auto_cab());
                self.pending = None;
                self.store_globals(raw, stamp);
                if preset_verified && globals_verified {
                    self.event(DeviceEvent::EditProgress {
                        stamp,
                        stage: EditStage::Verified,
                    });
                } else {
                    self.edit_failure(stamp, ServiceError::new(ErrorKind::Indeterminate, "Dependent model complete preset/global readback differs; actual state retained and no follow-up may be replayed"), true);
                    self.cancel_queued(
                        "Dependent model readback mismatch cancelled unsent controls",
                    );
                }
            }
            RequestKind::GlobalPreflight { before, patch } => {
                if !valid_globals(&raw) {
                    self.notification(raw);
                    return;
                }
                let before = before.clone();
                let patch = patch.clone();
                let stamp = pending.stamp;
                let fresh = DeviceGlobalState::from_transfer(
                    before.revision,
                    self.epoch,
                    raw.clone(),
                    before.evidence.clone(),
                )
                .expect("validated globals");
                self.pending = None;
                if !before.records().control_state_equal(fresh.records()) {
                    self.store_globals(raw, stamp);
                    self.edit_failure(
                        stamp,
                        ServiceError::new(
                            ErrorKind::Conflict,
                            "Global settings changed before the write; refreshed actual state",
                        ),
                        false,
                    );
                    return;
                }
                if self.mode == BackendMode::Hardware && !hardware_global_verified(&patch, &fresh) {
                    self.store_globals(raw, stamp);
                    self.edit_failure(
                        stamp,
                        ServiceError::new(
                            ErrorKind::Unverified,
                            "Fresh global state no longer meets this control's hardware profile",
                        ),
                        false,
                    );
                    return;
                }
                let request = patch.request().expect("validated edit");
                self.begin(
                    stamp,
                    RequestKind::GlobalMutation {
                        before: Arc::new(fresh),
                        patch,
                    },
                    request,
                );
            }
            RequestKind::GlobalMutation { before, patch } => {
                if !pending.readback_sent || !valid_globals(&raw) {
                    self.notification(raw);
                    return;
                }
                let stamp = pending.stamp;
                let before = before.clone();
                let patch = patch.clone();
                self.pending = None;
                let actual = self.store_globals(raw, stamp);
                if before.verifies_expected(&patch, &actual).unwrap_or(false) {
                    self.event(DeviceEvent::EditProgress {
                        stamp,
                        stage: EditStage::Verified,
                    });
                } else {
                    self.edit_failure(stamp, ServiceError::new(ErrorKind::Conflict, "Global readback differs from the requested field or other stable settings; actual state refreshed"), false);
                }
            }
            RequestKind::Identity { synchronize } => {
                let Some(wire) = Identity::from_transfer(&raw) else {
                    self.notification(raw);
                    return;
                };
                let synchronize = *synchronize;
                let stamp = pending.stamp;
                self.pending = None;
                let model = match wire.model() {
                    WireDeviceModel::Gp200 => DeviceModel::Gp200,
                    WireDeviceModel::Gp200Lt => DeviceModel::Gp200Lt,
                    WireDeviceModel::Gp200Jr => DeviceModel::Gp200Jr,
                    WireDeviceModel::Unknown(id) => DeviceModel::Unknown(id),
                };
                let identity = DeviceIdentity {
                    model,
                    mode: self.mode,
                    firmware: None,
                    display_name: model.name(),
                };
                let capabilities = if model == DeviceModel::Gp200 && wire.state_raw == 1 {
                    capabilities_for_mode(self.mode)
                } else {
                    Capabilities::empty(model)
                };
                self.identity = Some(identity.clone());
                self.identity_revision.0 += 1;
                self.job_session = self.job_selection.clone().and_then(|selection| {
                    DeviceJobSession::observe_normal(
                        self.epoch,
                        selection,
                        self.identity_revision,
                        &raw,
                        &identity,
                        &capabilities,
                    )
                    .ok()
                });
                self.event(DeviceEvent::SessionChanged {
                    epoch: self.epoch,
                    identity,
                    capabilities,
                });
                if let Some(session) = self.job_session.clone() {
                    self.event(DeviceEvent::DeviceJobSessionObserved(session));
                }
                if model != DeviceModel::Gp200 || wire.state_raw != 1 {
                    self.fail(
                        stamp,
                        ServiceError::unsupported(format!(
                            "Detected {} state {}; this profile supports GP-200 state 1",
                            model.name(),
                            wire.state_raw
                        )),
                    );
                    self.state_changed(ConnectionState::Unsupported);
                } else if synchronize {
                    self.state_changed(ConnectionState::Synchronizing);
                    self.begin(
                        stamp,
                        RequestKind::Read {
                            target: PresetTarget::EditBuffer,
                            ready_after: true,
                        },
                        read_current_request(),
                    );
                }
            }
            RequestKind::Read {
                target,
                ready_after,
            } => {
                let target = *target;
                let ready_after = *ready_after;
                if !valid_preset(&raw, target) {
                    self.notification(raw);
                    return;
                }
                let stamp = pending.stamp;
                self.pending = None;
                self.store_snapshot(raw, target, stamp);
                if ready_after {
                    self.state_changed(ConnectionState::Ready);
                }
            }
            RequestKind::Write(edit) => {
                if !pending.readback_sent || !valid_preset(&raw, PresetTarget::EditBuffer) {
                    self.notification(raw);
                    return;
                }
                let edit = edit.clone();
                // Keep target/context/model and exact IEEE bits in the proof.
                let previous = &self.snapshots[&PresetTarget::EditBuffer];
                let mut expected_records = previous.raw.records().to_vec();
                let expected_module = expected_records
                    .iter_mut()
                    .find(|r| {
                        r.command == 0x14 && r.body.len() == 68 && r.body[0] == edit.module.get()
                    })
                    .expect("validated module");
                let offset = 8 + usize::from(edit.parameter.get()) * 4;
                expected_module.body[offset..offset + 4]
                    .copy_from_slice(&edit.value.0.to_le_bytes());
                let expected = RawTransfer::from_records(previous.raw.kind(), &expected_records)
                    .expect("retained valid records");
                let verified = preset_records_equal(&expected, &raw);
                self.pending = None;
                self.store_snapshot(raw, PresetTarget::EditBuffer, edit.stamp);
                if verified {
                    self.event(DeviceEvent::EditProgress {
                        stamp: edit.stamp,
                        stage: EditStage::Verified,
                    });
                    if let Some(mut next_edit) = self.queued_edit.take() {
                        next_edit.base_revision =
                            self.snapshots[&PresetTarget::EditBuffer].revision;
                        if let Err(error) = self.validate_edit(PresetTarget::EditBuffer, &next_edit)
                        {
                            self.edit_failure(next_edit.stamp, error, false);
                        } else {
                            self.start_edit(next_edit);
                        }
                    }
                } else {
                    self.edit_failure(edit.stamp, ServiceError::new(ErrorKind::Conflict, "Readback differs from the requested parameter/context/model; device state was refreshed"), false);
                    self.cancel_queued(
                        "Final queued value was not sent after a conflicting readback",
                    );
                }
            }
            RequestKind::MetadataPreflight {
                before,
                request,
                mutation,
            } => {
                if !valid_preset(&raw, PresetTarget::EditBuffer) {
                    self.notification(raw);
                    return;
                }
                let stamp = pending.stamp;
                let same = preset_records_equal(&before.raw, &raw);
                let request = request.clone();
                let mutation = mutation.clone();
                self.pending = None;
                if !same {
                    self.store_snapshot(raw, PresetTarget::EditBuffer, stamp);
                    self.fail(
                        stamp,
                        ServiceError::new(
                            ErrorKind::Conflict,
                            "Patch metadata baseline changed during preflight; no edit was sent",
                        ),
                    );
                    return;
                }
                self.begin(stamp, RequestKind::Mutation(mutation), request);
            }
            RequestKind::Mutation(mutation) => {
                if !pending.readback_sent || !valid_preset(&raw, mutation.target) {
                    self.notification(raw);
                    return;
                }
                let mutation = mutation.clone();
                let stamp = pending.stamp;
                let exact = preset_records_equal(&mutation.expected, &raw);
                let overlays = if matches!(
                    mutation.policy,
                    MutationPolicy::Selection | MutationPolicy::SavedCurrent
                ) {
                    verified_expression_overlays(&mutation.expected, &raw)
                } else {
                    None
                };
                let verified = exact || overlays.is_some();
                if !exact && let Some(changes) = overlays {
                    self.diagnostic("runtime-expression-overlay", changes.join("; "), None);
                }
                self.pending = None;
                self.store_snapshot(raw, mutation.target, stamp);
                if verified {
                    if let MutationPolicy::SaveThenCurrent { current_expected } = mutation.policy {
                        self.begin(
                            stamp,
                            RequestKind::Mutation(ValidatedMutation {
                                target: PresetTarget::EditBuffer,
                                expected: current_expected,
                                policy: MutationPolicy::SavedCurrent,
                            }),
                            read_current_request(),
                        );
                        // The persistent write has already been attempted. This
                        // is a second readback, never another mutation send.
                        let pending = self.pending.as_mut().expect("new readback");
                        pending.readback_queued = true;
                        pending.mutation_attempted = true;
                        return;
                    }
                    self.event(DeviceEvent::EditProgress {
                        stamp,
                        stage: EditStage::Verified,
                    });
                } else {
                    self.edit_failure(stamp, ServiceError::new(ErrorKind::Conflict, "Mutation readback differs from expected fields; actual state was refreshed"), false);
                }
            }
        }
    }
    fn store_snapshot(&mut self, raw: RawTransfer, target: PresetTarget, stamp: RequestStamp) {
        let source = self.snapshot_source();
        let revision = self.revisions.entry(target).or_insert(Revision(0));
        revision.0 += 1;
        let mut snapshot = PresetSnapshot::from_raw(Arc::new(raw), source, Some(target), *revision);
        snapshot.evidence.push(EvidenceRef::new(
            "crates/gp200-transport",
            if self.mode == BackendMode::Hardware {
                EvidenceLevel::LiveVerified
            } else {
                EvidenceLevel::Proposed
            },
        ));
        let snapshot = Arc::new(snapshot);
        self.snapshots.insert(target, snapshot.clone());
        self.event(DeviceEvent::SnapshotReceived {
            stamp,
            target,
            snapshot,
        });
    }
    fn store_globals(&mut self, raw: RawTransfer, stamp: RequestStamp) -> Arc<DeviceGlobalState> {
        self.global_revision.0 += 1;
        let state = Arc::new(
            DeviceGlobalState::from_transfer(
                self.global_revision,
                self.epoch,
                raw,
                vec![EvidenceRef::new(
                    "crates/gp200-transport/global-readback",
                    if self.mode == BackendMode::Hardware {
                        EvidenceLevel::LiveVerified
                    } else {
                        EvidenceLevel::Proposed
                    },
                )],
            )
            .expect("validated complete global transfer"),
        );
        self.globals = Some(state.clone());
        self.event(DeviceEvent::GlobalStateReceived {
            stamp,
            state: state.clone(),
        });
        state
    }
    fn notification(&mut self, raw: RawTransfer) {
        let (current, globals) = notification_hints(&raw);
        self.notification_invalidates_current |= current;
        self.notification_invalidates_globals |= globals;
        if raw.kind() == gp200_protocol::WRITE_KIND {
            for record in raw.records().iter().filter(|record| {
                matches!(
                    (record.command, record.body.len()),
                    (0x2003, 20) | (0x2009, 4)
                )
            }) {
                if let Ok(slot) = SlotId::new(u16::from_le_bytes([record.body[0], record.body[1]]))
                {
                    self.notification_invalidates_stored.insert(slot);
                }
            }
        }
        if current || globals {
            // Retain the in-flight operation's internal baseline through its
            // own full verification, but never send queued controls afterward.
            self.cancel_queued("Incoming change hint cancelled unsent controls; read complete state before another edit");
        }
        self.invalidate_notification_caches_when_idle();
        self.event(DeviceEvent::IncomingNotification {
            epoch: self.epoch,
            raw: Arc::new(raw),
        });
    }
    fn invalidate_notification_caches_when_idle(&mut self) {
        if self.pending.is_some() || self.connecting_intent.is_some() {
            return;
        }
        if std::mem::take(&mut self.notification_invalidates_current) {
            self.snapshots.remove(&PresetTarget::EditBuffer);
        }
        if std::mem::take(&mut self.notification_invalidates_globals) {
            self.globals = None;
        }
        for slot in std::mem::take(&mut self.notification_invalidates_stored) {
            self.snapshots.remove(&PresetTarget::Stored(slot));
        }
    }
    pub fn tick(&mut self, now: Duration) {
        self.now = now;
        if let Some(run) = self.capture.as_mut()
            && let Some((reservation, sequence)) = run.queue_offer(now)
        {
            self.actions.push_back(TransportAction::CaptureRead {
                reservation,
                sequence,
            });
        }
        self.invalidate_notification_caches_when_idle();
        if now >= self.next_send_allowed
            && let Some(packets) = self.scheduled_packets.take()
        {
            self.actions.push_back(TransportAction::Send { packets });
        }
        if let Some(abandoned) = self.assembler.expire(now) {
            self.diagnostic(
                "reassembly-timeout",
                format!(
                    "Abandoned {} of {} bytes after {:?}",
                    abandoned.incomplete.received, abandoned.incomplete.total, abandoned.age
                ),
                None,
            );
        }
        if self
            .pending
            .as_ref()
            .is_some_and(|pending| pending.deadline.is_some_and(|deadline| now >= deadline))
        {
            self.lost(ServiceError::new(ErrorKind::Timeout, "Device request timed out; session closed so a late reply cannot satisfy another request"));
            return;
        }
        if let Some(pending) = &mut self.pending
            && pending.readback_at.is_some_and(|at| now >= at)
            && !pending.readback_queued
        {
            pending.readback_queued = true;
            pending.sent = false;
            pending.readback_at = None;
            let raw = match &pending.kind {
                RequestKind::GlobalMutation { .. } => read_globals_request().expect("static query"),
                RequestKind::Mutation(ValidatedMutation {
                    target: PresetTarget::Stored(slot),
                    ..
                }) => read_stored_request(slot.get()).expect("bounded slot"),
                _ => read_current_request(),
            };
            self.queue_send(encode_transfer(&raw).expect("validated request"));
        }
    }
    pub fn lost(&mut self, error: ServiceError) {
        self.capture_abort(error.clone());
        self.end_session(error, true);
        self.state_changed(ConnectionState::Recovering);
    }
    fn edit_failure(&mut self, stamp: RequestStamp, error: ServiceError, uncertain: bool) {
        let stage = if uncertain {
            EditStage::Indeterminate(error.clone())
        } else {
            EditStage::Failed(error.clone())
        };
        self.event(DeviceEvent::EditProgress { stamp, stage });
        self.fail(stamp, error);
    }
    fn cancel_queued(&mut self, reason: &str) {
        if let Some(edit) = self.queued_edit.take() {
            self.edit_failure(
                edit.stamp,
                ServiceError::new(ErrorKind::Cancelled, reason),
                false,
            );
        }
    }
    fn end_session(&mut self, error: ServiceError, physical_loss: bool) {
        // Commands can arrive in a batch before actions execute. Cancel unsent
        // actions too; a queued Open/Send must not survive a later Disconnect.
        self.actions.retain(|action| {
            !matches!(
                action,
                TransportAction::Open { .. }
                    | TransportAction::Send { .. }
                    | TransportAction::CaptureRead { .. }
            )
        });
        if let Some(pending) = self.pending.take() {
            if pending.kind.is_mutation() && pending.mutation_attempted {
                self.edit_failure(pending.stamp, ServiceError::new(ErrorKind::Indeterminate,
                    format!("Write may have reached the pedal; reconnect and read before another action: {}", error.message)), true);
            } else {
                self.fail(pending.stamp, error.clone());
            }
        }
        self.cancel_queued(
            "Unsent edit cancelled with its device session; it will not be replayed",
        );
        self.connecting_intent = None;
        self.scheduled_packets = None;
        self.next_send_allowed = Duration::ZERO;
        self.actions.push_back(TransportAction::Close);
        self.epoch.0 += 1;
        self.identity = None;
        self.job_selection = None;
        self.job_session = None;
        self.identity_revision = Revision(0);
        self.job_asset_baselines.clear();
        self.snapshots.clear();
        self.revisions.clear();
        self.globals = None;
        self.global_revision = Revision(0);
        self.asset_metadata_revision = Revision(0);
        self.notification_invalidates_current = false;
        self.notification_invalidates_globals = false;
        self.notification_invalidates_stored.clear();
        self.decoder.reset();
        self.assembler.reset();
        if physical_loss {
            self.diagnostic("session-lost", error.message, None);
        }
    }
}

fn notification_hints(raw: &RawTransfer) -> (bool, bool) {
    if raw.kind() != gp200_protocol::WRITE_KIND {
        return (false, false);
    }
    raw.records()
        .iter()
        .fold((false, false), |(current, globals), record| {
            let preset = match (record.command, record.body.len()) {
                (2, 88) | (8, 16) => u16::from_le_bytes([record.body[0], record.body[1]]) < 256,
                (0x0c, 12) | (0x0f, 8) | (0x10, 4) => true,
                (5, 12) => record.body[0] < 11 && record.body[1] < 15,
                (0x14, 68) | (0x15, 4) | (0x16, 8) => record.body[0] < 11,
                (6, 4) => u16::from_le_bytes([record.body[0], record.body[1]]) <= 6,
                (7 | 0x2003, 20) | (0x2009, 4) => {
                    u16::from_le_bytes([record.body[0], record.body[1]]) < 256
                }
                (0x1008, 4) => record.body[..2] == [0, 0],
                _ => false,
            };
            let global = matches!(
                (record.command, record.body.len()),
                (0x1000, 6)
                    | (0x1001, 4)
                    | (0x1002, 8)
                    | (0x1004, 124)
                    | (0x1005, 8)
                    | (0x1007, 72)
                    | (0x1008, 4)
                    | (0x100e, 40)
                    | (0x100f, 4)
                    | (0x1010, 4)
                    | (0x1011, 4)
            ) || (record.command == 0x000a
                && record.body.len() == 20
                && u16::from_le_bytes([record.body[0], record.body[1]]) < 20);
            (current || preset, globals || global)
        })
}

fn valid_globals(raw: &RawTransfer) -> bool {
    !raw.records().iter().any(|record| record.command == 0)
        && GlobalRecords::from_transfer(raw.clone()).is_ok_and(|records| records.is_complete())
}

/// Field-specific connected profiles, independent of coarse WriteGlobals.
/// Drum parameter edits were verified while stopped. The common boolean
/// play/stop command has no native volume precondition; its live proof was
/// muted and does not measure audible output. Independent reversible captures
/// cover all 24 General selectors and all 31 EQ indices, including retained
/// shelf fields. This establishes the typed control path within native bounds,
/// not power-cycle persistence or measured audio/MIDI routing. The separate
/// footswitch storage proof covers all 20 event addresses in inactive table 2,
/// all native function IDs at tap/hold/combined representatives, and table 0/1
/// routes. These are compositional samples, not an exhaustive Cartesian matrix
/// or physical action validation. Only a freshly observed inactive table and
/// native-menu current/proposed values qualify.
pub fn hardware_global_verified(edit: &GlobalControlEdit, state: &DeviceGlobalState) -> bool {
    match edit {
        GlobalControlEdit::General { field, value } => field.validate(*value).is_ok(),
        GlobalControlEdit::Eq { parameter, value } => parameter.validate(value.as_f32()).is_ok(),
        GlobalControlEdit::Drum { field, .. } => {
            matches!(
                field,
                DrumSetting::Tempo | DrumSetting::Volume | DrumSetting::Pattern
            ) && state
                .records()
                .runtime
                .as_ref()
                .and_then(|r| r.drum_playing())
                == Some(false)
        }
        GlobalControlEdit::DrumPlay(_) => state
            .records()
            .runtime
            .as_ref()
            .and_then(|r| r.drum_playing())
            .is_some(),
        GlobalControlEdit::Footswitch { .. } => {
            ValidatedInactiveFootswitchEdit::validate(edit, state).is_ok()
        }
    }
}

/// Exact live-proven special controls. Generic ordinary numeric values retain
/// the established finite-range profile; simulator plans use no hardware proof.
pub fn hardware_control_verified(
    module: u8,
    code: u32,
    parameter: &gp200_catalog::ParameterDefinition,
) -> bool {
    match control_semantics(parameter) {
        ControlSemantics::Numeric
            if parameter.minimum_is_off_raw.is_none()
                && !parameter.raw_attributes.contains_key("MinISMute") =>
        {
            true
        }
        ControlSemantics::Numeric => {
            module == 5 && code == 0x0a000010 && matches!(parameter.parameter_id, 5 | 6)
        }
        ControlSemantics::Switch => {
            (module == 1 && code == 0x0500000a && parameter.parameter_id == 4)
                || (module == 8 && code == 0x0b00000b && parameter.parameter_id == 4)
        }
        ControlSemantics::SyncNumeric { power_parameter } => {
            hardware_sync_verified(module, code, power_parameter, parameter.parameter_id)
        }
        _ => false,
    }
}

pub fn hardware_sync_verified(module: u8, code: u32, power: u8, value: u8) -> bool {
    matches!(
        (module, code, power, value),
        (7, 0x04000015, 2, 1) | (8, 0x0b00000b, 3, 1)
    )
}

fn valid_preset(raw: &RawTransfer, target: PresetTarget) -> bool {
    let contexts = raw
        .records()
        .iter()
        .filter(|record| record.command == 0)
        .collect::<Vec<_>>();
    if contexts.len() != 1 || contexts[0].body.len() != 4 {
        return false;
    }
    let Some(context) = gp200_protocol::WireContext::from_transfer(raw) else {
        return false;
    };
    if match target {
        PresetTarget::EditBuffer => context.group != 0 || context.selector > 255,
        PresetTarget::Stored(slot) => context.group != 1 || context.selector != slot.get(),
    } {
        return false;
    }
    let mut modules = BTreeSet::new();
    for record in raw.records().iter().filter(|record| record.command == 0x14) {
        if record.body.len() != 68
            || record.body[0] > 10
            || u16::from_le_bytes([record.body[2], record.body[3]]) != 15
            || !modules.insert(record.body[0])
        {
            return false;
        }
    }
    modules.len() == 11
}
fn mutation_error(error: gp200_protocol::MutationError) -> ServiceError {
    ServiceError::invalid(error.to_string())
}

fn retarget_preset(
    source: &RawTransfer,
    group: u16,
    slot: u16,
) -> Result<RawTransfer, ServiceError> {
    let envelope =
        gp200_protocol::full_stored_write_request(slot, source).map_err(mutation_error)?;
    let mut records = vec![RawRecord {
        command: 0,
        body: [group.to_le_bytes(), slot.to_le_bytes()].concat(),
    }];
    records.extend_from_slice(&envelope.records()[2..]);
    RawTransfer::from_records(source.kind(), &records)
        .map_err(|e| ServiceError::invalid(e.to_string()))
}

/// Selection immediately applies a connected expression pedal. Admit only the
/// two sampled model/parameter profiles with their retained matching assignment.
/// Other mappings stay explicit conflicts until the assignment phase proves them.
fn verified_expression_overlays(
    expected: &RawTransfer,
    actual: &RawTransfer,
) -> Option<Vec<String>> {
    if expected.records().len() != actual.records().len() {
        return None;
    }
    let mut corrected = actual.records().to_vec();
    let mut changes = Vec::new();
    for (module, code, parameter, assignment) in [
        (1_u8, 0x0500000a_u32, 3_u8, 0x10_u8),
        (10, 0x06000003, 0, 0),
    ] {
        let assigned = expected.records().iter().any(|r| {
            r.command == 0xc
                && r.body.len() == 12
                && r.body[..3] == [assignment, module, parameter]
                && r.body[4..8] == 100_f32.to_le_bytes()
                && r.body[8..12] == 0_f32.to_le_bytes()
        });
        if !assigned {
            continue;
        }
        let index = expected
            .records()
            .iter()
            .position(|r| r.command == 0x14 && r.body.len() == 68 && r.body[0] == module)?;
        let before = &expected.records()[index];
        let after = &mut corrected[index];
        if after.command != before.command
            || after.body.len() != 68
            || after.body[..8] != before.body[..8]
            || u32::from_le_bytes(before.body[4..8].try_into().ok()?) != code
        {
            continue;
        }
        let offset = 8 + usize::from(parameter) * 4;
        let value = f32::from_le_bytes(after.body[offset..offset + 4].try_into().ok()?);
        if !value.is_finite() || !(0.0..=100.0).contains(&value) {
            return None;
        }
        if after.body[offset..offset + 4] != before.body[offset..offset + 4] {
            changes.push(format!(
                "module={module} code={code:08x} xml_id={parameter} raw_bits={:08x}",
                value.to_bits()
            ));
            after.body[offset..offset + 4].copy_from_slice(&before.body[offset..offset + 4]);
        }
    }
    let corrected = RawTransfer::from_records(actual.kind(), &corrected).ok()?;
    (!changes.is_empty() && preset_records_equal(expected, &corrected)).then_some(changes)
}

/// All retained bytes match, except exact unnamed bytes corroborated by host
/// copy constructors and root's repeated/current/bank captures. Retain them;
/// they are neither normalized nor described as established padding.
fn preset_records_equal(expected: &RawTransfer, actual: &RawTransfer) -> bool {
    expected.records().len() == actual.records().len()
        && expected
            .records()
            .iter()
            .zip(actual.records())
            .all(|(a, b)| {
                a.command == b.command
                    && a.body.len() == b.body.len()
                    && a.body.iter().zip(&b.body).enumerate().all(|(i, (x, y))| {
                        x == y
                            || match a.command {
                                2 if a.body.len() == 88 => i == 15,
                                8 if a.body.len() == 16 => i == 15,
                                0xf if a.body.len() == 8 => matches!(i, 2 | 3),
                                _ => false,
                            }
                    })
            })
}
/// Exact model selections whose complete sparse-default behavior was captured.
/// Assigned parameter targets and AMP Auto CAB side effects require separate
/// profiles. A successful selection of one model does not promote its family.
pub fn hardware_model_blocked_reason(
    snapshot: &PresetSnapshot,
    module: ModuleId,
    target: ModelCode,
) -> Option<String> {
    if snapshot
        .context
        .is_none_or(|context| context.group != 0 || context.selector > 255)
    {
        return Some("Model selection requires a confirmed current-buffer context".into());
    }
    let mut records = snapshot.raw.records().iter().filter(|record| {
        record.command == 0x14 && record.body.len() == 68 && record.body[0] == module.get()
    });
    let Some(record) = records.next() else {
        return Some("The selected module has no complete native record".into());
    };
    if records.next().is_some()
        || record.body[1] > 1
        || u16::from_le_bytes([record.body[2], record.body[3]]) != 15
    {
        return Some("The selected module shape or enabled state is unknown".into());
    }
    let previous = u32::from_le_bytes(record.body[4..8].try_into().expect("four-byte Code"));
    if previous == target.0 {
        return Some("This model is already selected; its controls stay unchanged".into());
    }
    if snapshot.raw.records().iter().any(|record| {
        matches!(record.command, 0x0c | 0x10)
            && record.body.len() >= 2
            && record.body[1] == module.get()
    }) {
        return Some("This module has EXP or Quick Access targets; their model-reset behavior awaits validation".into());
    }
    let verified = crate::model_profiles::captured_transition(module.get(), previous, target.0);
    (!verified).then(|| "This exact model transition has no complete hardware/default profile. Open an offline draft to explore it".into())
}

fn validate_factory_amp_catalog(
    catalog: &Catalog,
    plan: &FactoryAmpPlan,
    before: &PresetSnapshot,
) -> Result<(), ServiceError> {
    let descriptor = factory_amp_descriptor(plan.profile.candidate()).expect("safe profile");
    for (code, index, cab, expected) in [
        (
            descriptor.model,
            descriptor.xml_index,
            descriptor.paired_cabinet,
            descriptor.defaults,
        ),
        (
            FACTORY_AMP_BASELINE,
            13,
            FACTORY_AMP_BASELINE_CAB,
            FACTORY_AMP_MATCH_DEFAULTS,
        ),
    ] {
        let definition = catalog.model_by_id(3, code.0).ok_or_else(|| {
            ServiceError::invalid("Factory AMP descriptor is absent from the catalog")
        })?;
        let defaults = gp200_catalog::ModelDefaultPlan::from_definition(definition);
        if definition.xml_index != index
            || !defaults.unknown_defaults.is_empty()
            || defaults.declared != expected
            || definition
                .raw_attributes
                .get("CABCode")
                .and_then(|value| value.parse::<u32>().ok())
                != Some(cab.0)
        {
            return Err(ServiceError::new(
                ErrorKind::Unverified,
                "Factory AMP Code, XML Index, CAB or sparse defaults differ from the fixed descriptor",
            ));
        }
        let cabinet = catalog
            .model_by_id(5, cab.0)
            .ok_or_else(|| ServiceError::invalid("Factory AMP paired factory cabinet is absent"))?;
        let defaults = gp200_catalog::ModelDefaultPlan::from_definition(cabinet);
        if !matches!(cabinet.slot_kind(), gp200_catalog::SlotKind::Factory)
            || !defaults.unknown_defaults.is_empty()
            || defaults.declared != FACTORY_AMP_CAB_DEFAULTS
        {
            return Err(ServiceError::new(
                ErrorKind::Unverified,
                "Factory AMP paired cabinet defaults differ from the fixed ordinary descriptor",
            ));
        }
    }
    // A model selection must not make original declared values impossible to Undo.
    // A blocked choice already at its exact default needs no scalar restoration.
    for module in std::iter::once(3u8).chain(plan.auto_cab.then_some(5)) {
        let body = &before.raw.records()[3 + usize::from(module)].body;
        let code = u32::from_le_bytes(body[4..8].try_into().expect("profile shape"));
        let definition = catalog.model_by_id(module, code).ok_or_else(|| {
            ServiceError::invalid("Factory AMP original source is absent from catalog")
        })?;
        let values: Vec<u32> = body[8..]
            .as_chunks::<4>()
            .0
            .iter()
            .map(|word| u32::from_le_bytes(*word))
            .collect();
        for parameter in &definition.parameters {
            let bits = values[usize::from(parameter.parameter_id)];
            if parameter
                .default
                .is_some_and(|value| value.to_bits() == bits)
            {
                continue;
            }
            if !hardware_control_verified(module, code, parameter) {
                return Err(ServiceError::new(
                    ErrorKind::Unverified,
                    "Factory AMP original non-default control lacks a reversible scalar profile",
                ));
            }
            gp200_catalog::plan_control_request(
                definition,
                parameter.parameter_id,
                &values,
                gp200_catalog::ControlRequest::RawBits(bits),
            )
            .map_err(|error| ServiceError::invalid(error.to_string()))?;
        }
    }
    Ok(())
}

fn dependent_profile_enabled(profile: DependentModelProfile) -> bool {
    profile.normal_build_accepted() || cfg!(feature = "dependent-model-trials")
}

/// Shared picker gate for the explicitly global-bound dependent command. The
/// generic SubmitEdit model path remains restricted to independent profiles.
pub fn hardware_model_blocked_reason_with_globals(
    snapshot: &PresetSnapshot,
    globals: Option<&DeviceGlobalState>,
    module: ModuleId,
    target: ModelCode,
) -> Option<String> {
    let Some(previous) = snapshot.view.module(module).map(|module| module.model) else {
        return hardware_model_blocked_reason(snapshot, module, target);
    };
    if factory_amp_pair(module, previous, target) {
        let Some(globals) = globals else {
            return Some(
                "Read complete pedal globals before selecting this factory AMP candidate".into(),
            );
        };
        return match FactoryAmpPlan::from_snapshot(snapshot, globals, target) {
            Err(error) => Some(error.message),
            Ok(plan) if !plan.profile.normal_build_accepted() && !cfg!(feature = "factory-amp-trials") => Some("This exact factory AMP candidate awaits complete bidirectional production acceptance".into()),
            Ok(plan) => Catalog::embedded().map_err(|error| ServiceError::invalid(error.to_string())).and_then(|catalog| validate_factory_amp_catalog(&catalog, &plan, snapshot)).err().map(|error| error.message),
        };
    }
    if !dependent_model_pair(module, previous, target) {
        return hardware_model_blocked_reason(snapshot, module, target);
    }
    let Some(globals) = globals else {
        return Some("Read complete pedal globals before selecting this dependent model".into());
    };
    match DependentModelPlan::from_snapshot(snapshot, globals, module, target) {
        Err(error) => Some(error.message),
        Ok(plan) if !dependent_profile_enabled(plan.profile) => Some(
            "This dependent model candidate awaits complete bidirectional production acceptance"
                .into(),
        ),
        Ok(_) => None,
    }
}

/// Field-specific hardware proof shared by actor and native widgets. This does
/// not replace epoch, revision, catalog and complete readback checks.
pub fn hardware_assignment_blocked_reason(
    snapshot: &PresetSnapshot,
    edit: &AssignmentEdit,
) -> Option<String> {
    let record = match PatchAssignments::edited_record(snapshot, edit) {
        Ok(record) => record,
        Err(error) => return Some(error.message),
    };
    let Some(context) = snapshot
        .context
        .filter(|c| c.group == 0 && c.selector <= 255)
    else {
        return Some("Assignment writes require a confirmed current-buffer context".into());
    };
    if matches!(edit, AssignmentEdit::FxLoopPositions { .. }) {
        if u16::from_le_bytes([record.body[0], record.body[1]]) != context.selector {
            return Some("FX routing embedded slot differs from the current context".into());
        }
    } else if let Err(error) = gp200_protocol::patch_assignment_write_request(
        gp200_protocol::WireContext {
            group: context.group,
            selector: context.selector,
        },
        &record,
    ) {
        return Some(error.to_string());
    }
    match edit {
        AssignmentEdit::CtrlModules { .. } | AssignmentEdit::FxLoopPositions { .. } | AssignmentEdit::QuickTarget { .. } => None,
        AssignmentEdit::CtrlState { .. } => Some("CTRL activation has no verified command; the captured full-record state change was ignored by the pedal".into()),
        AssignmentEdit::ExpressionRange { slot, .. } => {
            let Some(entry) = snapshot.view.patch_assignments.expressions.iter().find(|entry| entry.record.slot == *slot) else {
                return Some("EXP assignment is absent from the confirmed baseline".into());
            };
            if entry.record.module_raw != 1 || entry.record.parameter_raw != 3
                || snapshot.view.module(ModuleId::new(1).unwrap()).is_none_or(|m| m.model.0 != 0x0500000a) {
                return Some("EXP range writes are verified for the current P-Wah Position assignment only".into());
            }
            if snapshot.view.patch_assignments.expressions.iter().any(|peer| peer.record.slot != *slot
                && peer.record.slot.group() == slot.group() && peer.record.module_raw == entry.record.module_raw
                && peer.record.parameter_raw == entry.record.parameter_raw) {
                return Some("Matching EXP targets in this bank are coupled; their endpoint updates await validation".into());
            }
            None
        }
        AssignmentEdit::ExpressionTargetAndRange { slot, target: ExpressionAssignmentTarget::Parameter(target), minimum, maximum }
            if matches!(slot.packed(), 0x10 | 0x22) && target.module.get() == 1 && target.expected_model.0 == 0x0500000a
                && target.parameter.get() == 3 && *minimum == F32Bits::from_f32(0.) && *maximum == F32Bits::from_f32(100.)
                && snapshot.view.patch_assignments.expressions.iter().any(|e| e.record.slot == *slot && e.record.module_raw == 255) => None,
        AssignmentEdit::ExpressionTarget { slot, target: ExpressionAssignmentTarget::Off }
        | AssignmentEdit::ExpressionTargetAndRange { slot, target: ExpressionAssignmentTarget::Off, .. }
            if slot.packed() == 0x10
                && snapshot.view.module(ModuleId::new(1).unwrap()).is_some_and(|m| m.model.0 == 0x0500000a)
                && snapshot.view.patch_assignments.expressions.iter().any(|e| e.record.slot == *slot
                    && e.record.module_raw == 1 && e.record.parameter_raw == 3
                    && e.record.minimum == F32Bits::from_f32(0.) && e.record.maximum == F32Bits::from_f32(100.)) => None,
        _ => Some("This EXP target change awaits exact device readback and restoration proof".into()),
    }
}

fn validate_assignment_reference(
    catalog: &Catalog,
    snapshot: &PresetSnapshot,
    edit: &AssignmentEdit,
) -> Result<(), ServiceError> {
    let mut projection = PatchAssignments::from_raw(&snapshot.raw);
    projection.apply(&snapshot.view.modules, edit)?;
    let (target, endpoints) = match edit {
        AssignmentEdit::ExpressionTarget { slot, .. }
        | AssignmentEdit::ExpressionTargetAndRange { slot, .. }
        | AssignmentEdit::ExpressionRange { slot, .. } => {
            let record = &projection
                .expressions
                .iter()
                .find(|e| e.record.slot == *slot)
                .ok_or_else(|| ServiceError::invalid("EXP assignment record absent"))?
                .record;
            let target = match PatchAssignments::resolve_expression(record, &snapshot.view.modules)
            {
                ResolvedAssignmentReference::Parameter(t) => Some(t),
                ResolvedAssignmentReference::Off
                    if !matches!(edit, AssignmentEdit::ExpressionRange { .. }) =>
                {
                    None
                }
                _ => {
                    return Err(ServiceError::invalid(
                        "EXP endpoint editing requires a resolved parameter target",
                    ));
                }
            };
            (target, Some((record.minimum, record.maximum)))
        }
        AssignmentEdit::QuickTarget {
            target: QuickAssignmentTarget::Parameter(target),
            ..
        } => (Some(target.clone()), None),
        _ => (None, None),
    };
    if let Some(target) = target {
        let definition = catalog
            .model_by_id(target.module.get(), target.expected_model.0)
            .and_then(|m| m.parameter(target.parameter.get()))
            .ok_or_else(|| {
                ServiceError::invalid(
                    "Assignment parameter ID is not declared by its current model",
                )
            })?;
        if let Some((minimum, maximum)) = endpoints {
            let (low, high, _) = definition
                .numeric_bounds()
                .ok_or_else(|| ServiceError::invalid("EXP numeric bounds are unresolved"))?;
            if [minimum, maximum]
                .iter()
                .any(|v| !v.as_f32().is_finite() || v.as_f32() < low || v.as_f32() > high)
            {
                return Err(ServiceError::invalid(
                    "EXP endpoints are outside the selected parameter range",
                ));
            }
        }
    }
    Ok(())
}
fn command_stamp(command: &DeviceCommand) -> Option<RequestStamp> {
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
