use gp200_catalog::Catalog;
use gp200_domain::*;
use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;

/// The edit buffer's relationship to persistent pedal memory.
/// Parameter readback is never evidence that a stored preset was saved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BufferPersistence {
    Unknown,
    Selected {
        slot: SlotId,
        revision: Revision,
        stored_matches_buffer: Option<bool>,
    },
    Unsaved,
    Saving {
        slot: SlotId,
        intent: IntentId,
    },
    Saved {
        slot: SlotId,
        revision: Revision,
    },
    Indeterminate {
        slot: SlotId,
        reason: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageSeverity {
    Info,
    Warning,
    Error,
}

#[derive(Debug, Clone)]
pub struct AppMessage {
    pub sequence: u64,
    pub severity: MessageSeverity,
    pub message: String,
    pub error: Option<ServiceError>,
}

#[derive(Debug, Clone, Default)]
pub struct LibraryState {
    pub entries: Vec<LibraryEntry>,
    pub versions: BTreeMap<LibraryVersionId, LibraryVersion>,
    pub last_conversion: Option<ConversionReport>,
    pub last_export: Option<std::path::PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ComparisonSlot {
    A,
    B,
}

#[derive(Debug, Clone, Default)]
pub struct ComparisonState {
    pub snapshots: BTreeMap<ComparisonSlot, PresetDraft>,
    pub last_recalled: Option<ComparisonSlot>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocumentStatus {
    pub local_draft: bool,
    pub changed_since_baseline: bool,
    pub pending: usize,
    pub conflicts: usize,
    pub verified_in_buffer: bool,
    pub persistence: Option<BufferPersistence>,
}

/// Public render state. Rendering must not mutate it to perform device operations;
/// submit intents to `AppController` instead.
#[derive(Debug, Clone)]
pub struct AppState {
    pub view: ViewId,
    /// Render-only firmware editor status, never a service or physical permit.
    pub firmware_editor: crate::FirmwareEditorStatus,
    pub preferences: AppPreferences,
    pub connection: ConnectionState,
    pub epoch: SessionEpoch,
    pub identity: Option<DeviceIdentity>,
    pub capabilities: Capabilities,
    pub ports: Vec<PortPairCandidate>,
    pub confirmed_buffer: Option<Arc<PresetSnapshot>>,
    pub bank: HardwareBank,
    pub documents: BTreeMap<DocumentId, EditorDocument>,
    pub active_document: Option<DocumentId>,
    pub preview_slot: Option<SlotId>,
    pub buffer_persistence: BufferPersistence,
    pub globals: Option<GlobalSnapshot>,
    pub device_globals: Option<Arc<DeviceGlobalState>>,
    pub device_assets: DeviceAssetInventory,
    pub library: LibraryState,
    pub assets: Vec<AssetInfo>,
    pub jobs: BTreeMap<JobId, JobProgress>,
    pub prepared_jobs: BTreeMap<JobId, PreparedDeviceJob>,
    pub device_job_session: Option<DeviceJobSession>,
    pub comparisons: BTreeMap<DocumentId, ComparisonState>,
    /// Restarted uncertainty is descriptive offline data, never queued work.
    pub recovered_uncertainty: BTreeMap<DocumentId, Vec<RecoveryUncertainty>>,
    pub recovered_session_uncertainty: Vec<String>,
    pub messages: VecDeque<AppMessage>,
    pub diagnostics: VecDeque<DiagnosticEvent>,
    pub monotonic_ms: u64,
}

impl AppState {
    pub fn new(preferences: AppPreferences) -> Self {
        Self {
            view: ViewId::Editor,
            firmware_editor: Default::default(),
            preferences,
            connection: ConnectionState::Offline,
            epoch: SessionEpoch(0),
            identity: None,
            capabilities: Capabilities::empty(DeviceModel::Gp200),
            ports: Vec::new(),
            confirmed_buffer: None,
            bank: HardwareBank::new(SessionEpoch(0)),
            documents: BTreeMap::new(),
            active_document: None,
            preview_slot: None,
            buffer_persistence: BufferPersistence::Unknown,
            globals: None,
            device_globals: None,
            device_assets: DeviceAssetInventory::new(SessionEpoch(0)),
            library: LibraryState::default(),
            assets: Vec::new(),
            jobs: BTreeMap::new(),
            prepared_jobs: BTreeMap::new(),
            device_job_session: None,
            comparisons: BTreeMap::new(),
            recovered_uncertainty: BTreeMap::new(),
            recovered_session_uncertainty: Vec::new(),
            messages: VecDeque::new(),
            diagnostics: VecDeque::new(),
            monotonic_ms: 0,
        }
    }
    pub fn active_document(&self) -> Option<&EditorDocument> {
        self.active_document
            .as_ref()
            .and_then(|id| self.documents.get(id))
    }
    pub fn document_status(&self, id: &DocumentId) -> Option<DocumentStatus> {
        let document = self.documents.get(id)?;
        let device = document.target == EditTarget::DeviceBuffer;
        let verified = device
            && self.confirmed_buffer.as_ref().is_some_and(|snapshot| {
                crate::semantic::equivalent(&document.draft, &PresetDraft::from_snapshot(snapshot))
            });
        Some(DocumentStatus {
            local_draft: !device || !verified,
            changed_since_baseline: !crate::semantic::equivalent(
                &document.draft,
                &PresetDraft::from_snapshot(&document.baseline),
            ),
            pending: document
                .pending
                .values()
                .filter(|edit| {
                    matches!(
                        edit.stage,
                        EditStage::Queued | EditStage::Sent | EditStage::AwaitingReadback
                    )
                })
                .count(),
            conflicts: document.conflicts.len(),
            verified_in_buffer: verified,
            persistence: device.then(|| self.buffer_persistence.clone()),
        })
    }
    pub fn operation_availability(&self, operation: OperationKind) -> OperationAvailability {
        if self.firmware_editor.phase != crate::FirmwareEditorPhase::Normal {
            return OperationAvailability::Blocked {
                reason: "Normal operations are paused for firmware preparation or recovery".into(),
            };
        }
        if self.connection != ConnectionState::Ready {
            return if matches!(
                operation,
                OperationKind::SetParameter
                    | OperationKind::SetModuleEnabled
                    | OperationKind::SetModel
                    | OperationKind::SetChain
                    | OperationKind::SetMetadata
                    | OperationKind::SetExpression
                    | OperationKind::SetFootswitch
                    | OperationKind::SetQuickControl
            ) {
                OperationAvailability::OfflineOnly
            } else {
                OperationAvailability::Blocked {
                    reason: "Connect and synchronize a pedal first".into(),
                }
            };
        }
        let Some(identity) = &self.identity else {
            return OperationAvailability::Blocked {
                reason: "Device identity is not established".into(),
            };
        };
        if self
            .jobs
            .values()
            .any(|job| job.total.is_none_or(|total| job.completed < total) && !job.cancellable)
        {
            return OperationAvailability::Blocked {
                reason: "An exclusive device operation is running".into(),
            };
        }
        if self.capabilities.allows(operation, identity) {
            OperationAvailability::Available
        } else {
            OperationAvailability::Blocked {
                reason: match self.capabilities.operations.get(&operation) {
                    Some(FeatureSupport::Unsupported { reason }) => reason.clone(),
                    Some(FeatureSupport::Unverified { research_id, .. }) => {
                        format!("Awaiting protocol validation ({research_id})")
                    }
                    _ => "This operation has no validated profile for the connected device".into(),
                },
            }
        }
    }
    pub(crate) fn message(
        &mut self,
        severity: MessageSeverity,
        message: impl Into<String>,
        error: Option<ServiceError>,
    ) {
        let sequence = self
            .messages
            .back()
            .map_or(1, |message| message.sequence + 1);
        if self.messages.len() >= 128 {
            self.messages.pop_front();
        }
        self.messages.push_back(AppMessage {
            sequence,
            severity,
            message: message.into(),
            error,
        });
    }
    pub(crate) fn error(&mut self, error: ServiceError) {
        self.message(MessageSeverity::Error, error.message.clone(), Some(error));
    }
    pub(crate) fn diagnostic(&mut self, diagnostic: DiagnosticEvent) {
        if self.diagnostics.len() >= 256 {
            self.diagnostics.pop_front();
        }
        self.diagnostics.push_back(diagnostic);
    }
}

#[derive(Debug, Clone)]
pub(crate) enum BoundModelPlan {
    Existing(DependentModelPlan),
    Factory {
        plan: FactoryAmpPlan,
        expected: Arc<RawTransfer>,
        reviewed_revision: Revision,
    },
}
impl BoundModelPlan {
    pub(crate) fn module(&self) -> ModuleId {
        match self {
            Self::Existing(plan) => plan.module,
            Self::Factory { plan, .. } => plan.module,
        }
    }
    pub(crate) fn previous(&self) -> ModelCode {
        match self {
            Self::Existing(plan) => plan.previous,
            Self::Factory { plan, .. } => plan.previous,
        }
    }
    pub(crate) fn target(&self) -> ModelCode {
        match self {
            Self::Existing(plan) => plan.target,
            Self::Factory { plan, .. } => plan.target,
        }
    }
    pub(crate) fn paired_cabinet(&self) -> Option<ModelCode> {
        match self {
            Self::Existing(plan) => plan.paired_cabinet,
            Self::Factory { plan, .. } => plan.paired_cabinet,
        }
    }
    pub(crate) fn readback_matches(&self, snapshot: &PresetSnapshot) -> bool {
        match self {
            Self::Existing(_) => true,
            Self::Factory {
                expected,
                reviewed_revision,
                ..
            } => {
                snapshot.revision > *reviewed_revision
                    && factory_amp_readback_matches(expected, &snapshot.raw)
            }
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct PendingDependentModel {
    pub(crate) plan: BoundModelPlan,
    pub(crate) before_globals: Arc<DeviceGlobalState>,
    pub(crate) saw_globals: bool,
}

#[derive(Debug, Clone)]
pub(crate) enum PendingRequest {
    Connect,
    Read(PresetTarget),
    Preview(SlotId),
    Select {
        slot: SlotId,
        saw_readback: bool,
        verified: bool,
    },
    WriteStored {
        slot: SlotId,
        source: Option<Arc<PresetSnapshot>>,
        saw_readback: bool,
        verified: bool,
    },
    Edit {
        document: DocumentId,
        patch: EditPatch,
        saw_readback: bool,
        verified: bool,
        dependent: Option<PendingDependentModel>,
    },
    Save {
        slot: SlotId,
        buffer: Arc<PresetSnapshot>,
        current_buffer: Option<Arc<PresetSnapshot>>,
        saw_readback: bool,
        verified: bool,
    },
    Global,
    ReadDeviceGlobals,
    ReadAssetMetadata(AssetSlot),
    GlobalControl {
        before: Arc<DeviceGlobalState>,
        patch: GlobalControlEdit,
        saw_readback: bool,
        verified: bool,
    },
    DeviceJob(JobId),
    Cancel,
}

pub struct AppController {
    pub(crate) firmware_barrier: Option<crate::firmware_control::ActiveFirmwareEditorBarrier>,
    pub(crate) next_firmware_generation: u64,
    pub(crate) pending_library_effects:
        BTreeMap<(IntentId, crate::firmware_control::LibraryCompletion), usize>,
    pub(crate) pending_asset_effects: BTreeMap<JobId, usize>,
    pub(crate) retained_document_openings: BTreeMap<IntentId, Arc<PresetSnapshot>>,
    pub(crate) state: AppState,
    pub(crate) catalog: Catalog,
    pub(crate) next_intent: u64,
    pub(crate) next_document: u64,
    pub(crate) requests: BTreeMap<IntentId, PendingRequest>,
    pub(crate) latest_reads: BTreeMap<PresetTarget, IntentId>,
    pub(crate) deferred: BTreeMap<DocumentId, Vec<EditPatch>>,
    pub(crate) loading_versions: BTreeMap<IntentId, LibraryVersionId>,
    pub(crate) blocked_after_uncertainty: bool,
    pub(crate) connect_intent: Option<IntentId>,
    pub(crate) conflict_fields:
        BTreeMap<(DocumentId, IntentId), Vec<crate::semantic::ConflictField>>,
    pub(crate) external_refresh: crate::external_changes::ExternalRefresh,
    pub(crate) external_reviews: BTreeMap<IntentId, crate::external_changes::ExternalReview>,
    pub(crate) history_continuation: Option<crate::external_changes::HistoryContinuation>,
}
