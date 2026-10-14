use crate::semantic::{equivalent, merge_readback, patch_matches, same_field};
use crate::*;
use gp200_catalog::Catalog;
use gp200_domain::*;
use std::sync::Arc;

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
            if !factory_amp_restore_accepted(module, code, parameter) {
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

fn factory_amp_restore_accepted(
    module: u8,
    code: u32,
    parameter: &gp200_catalog::ParameterDefinition,
) -> bool {
    match gp200_catalog::control_semantics(parameter) {
        gp200_catalog::ControlSemantics::Numeric
            if parameter.minimum_is_off_raw.is_none()
                && !parameter.raw_attributes.contains_key("MinISMute") =>
        {
            true
        }
        gp200_catalog::ControlSemantics::Numeric => {
            module == 5
                && code == FACTORY_AMP_BASELINE_CAB.0
                && matches!(parameter.parameter_id, 5 | 6)
        }
        _ => false,
    }
}

pub fn device_document_id() -> DocumentId {
    DocumentId::from("device-buffer")
}

impl AppController {
    pub fn new(catalog: Catalog, preferences: AppPreferences) -> Self {
        Self {
            state: AppState::new(preferences),
            firmware_barrier: None,
            next_firmware_generation: 0,
            pending_library_effects: Default::default(),
            pending_asset_effects: Default::default(),
            retained_document_openings: Default::default(),
            catalog,
            next_intent: 1,
            next_document: 1,
            requests: Default::default(),
            latest_reads: Default::default(),
            deferred: Default::default(),
            loading_versions: Default::default(),
            blocked_after_uncertainty: false,
            connect_intent: None,
            conflict_fields: Default::default(),
            external_refresh: Default::default(),
            external_reviews: Default::default(),
            history_continuation: None,
        }
    }
    /// Starts with an offline capture document. Connecting never uploads it.
    pub fn with_demo(
        snapshot: Arc<PresetSnapshot>,
        preferences: AppPreferences,
    ) -> Result<Self, gp200_catalog::CatalogError> {
        let mut controller = Self::new(Catalog::embedded()?, preferences);
        controller.open_offline(snapshot);
        Ok(controller)
    }
    pub fn state(&self) -> &AppState {
        &self.state
    }
    pub fn catalog(&self) -> &Catalog {
        &self.catalog
    }
    pub fn active_document(&self) -> Option<&EditorDocument> {
        self.state.active_document()
    }
    pub fn active_document_id(&self) -> Option<&DocumentId> {
        self.state.active_document.as_ref()
    }
    pub fn activate_document(&mut self, id: &DocumentId) -> bool {
        if !self.admit_normal_mutation() {
            return false;
        }
        if self.state.documents.contains_key(id) {
            self.state.active_document = Some(id.clone());
            true
        } else {
            false
        }
    }
    pub fn operation_availability(&self, operation: OperationKind) -> OperationAvailability {
        if self.firmware_editor_blocks_normal_actions() {
            return OperationAvailability::Blocked {
                reason: Self::firmware_editor_busy().message,
            };
        }
        if crate::external_changes::mutation_operation(operation) && self.external_write_barrier() {
            return OperationAvailability::Blocked {
                reason: "Read and review the changed pedal state before another device action"
                    .into(),
            };
        }
        self.state.operation_availability(operation)
    }
    pub fn device_operation_pending(&self) -> bool {
        self.device_busy()
    }
    /// Explicitly stage an offline/library document in the current buffer.
    /// This is never called during connection or library loading.
    pub fn stage_document_in_buffer(&mut self, source: &DocumentId) -> Vec<Effect> {
        if !self.admit_normal_mutation() {
            return Vec::new();
        }
        if self.state.connection != ConnectionState::Ready || self.state.confirmed_buffer.is_none()
        {
            self.state.error(ServiceError::new(
                ErrorKind::Disconnected,
                "Read a connected device buffer before applying a local preset",
            ));
            return Vec::new();
        }
        if self.device_busy() || self.blocked_after_uncertainty || self.external_write_barrier() {
            self.state.error(ServiceError::new(
                ErrorKind::Busy,
                "Reconcile pending device operations before applying a local preset",
            ));
            return Vec::new();
        }
        let Some(document) = self.state.documents.get(source) else {
            self.state
                .error(ServiceError::invalid("Source document is absent"));
            return Vec::new();
        };
        if document.baseline.device_model
            != self
                .state
                .identity
                .as_ref()
                .map_or(DeviceModel::Gp200, |identity| identity.model)
        {
            self.state.error(ServiceError::new(
                ErrorKind::Unsupported,
                "The preset's device variant differs from the connected pedal",
            ));
            return Vec::new();
        }
        let mut draft = document.draft.clone();
        let Some(target) = self.state.documents.get(&device_document_id()) else {
            return Vec::new();
        };
        if draft.view.modules.len() != target.draft.view.modules.len()
            || draft
                .view
                .modules
                .iter()
                .map(|module| module.module)
                .collect::<std::collections::BTreeSet<_>>()
                .len()
                != draft.view.modules.len()
        {
            self.state.error(ServiceError::invalid(
                "Preset application requires the complete module set",
            ));
            return Vec::new();
        }
        for module in &mut draft.view.modules {
            let Some(destination) = target.draft.view.module(module.module) else {
                self.state.error(ServiceError::invalid(
                    "Preset application has an unknown or repeated module",
                ));
                return Vec::new();
            };
            let Some(definition) = self
                .catalog
                .model_by_id(module.module.get(), module.model.0)
            else {
                self.state.error(ServiceError::invalid(
                    "An unknown model cannot be applied to hardware",
                ));
                return Vec::new();
            };
            // Applying known controls is distinct from importing the lossless
            // source archive. Never copy source record offsets or undefined float
            // slots into an unrelated live raw transfer.
            let source_values = module.parameters.clone();
            module.parameters.clone_from(&destination.parameters);
            for control in &definition.parameters {
                let index = usize::from(control.parameter_id);
                let Some(value) = source_values.get(index) else {
                    self.state.error(ServiceError::invalid(
                        "The preset is missing a declared parameter slot",
                    ));
                    return Vec::new();
                };
                let Some(slot) = module.parameters.get_mut(index) else {
                    self.state.error(ServiceError::invalid(
                        "The device is missing a declared parameter slot",
                    ));
                    return Vec::new();
                };
                *slot = *value;
            }
            module.raw_record_index = destination.raw_record_index;
            module.slot_count_raw = destination.slot_count_raw;
        }
        let id = device_document_id();
        self.state.active_document = Some(id.clone());
        self.replace_draft(&id, draft)
    }
    pub fn invalid_assignments(&self, document: &DocumentId) -> Vec<String> {
        let Some(doc) = self.state.documents.get(document) else {
            return Vec::new();
        };
        let mut warnings = Vec::new();
        for expression in &doc.draft.expressions {
            for target in &expression.targets {
                if let Err(error) = self.validate_assignment(doc, target) {
                    warnings.push(format!(
                        "EXP {} bank {}: {}",
                        expression.controller + 1,
                        expression.bank + 1,
                        error.message
                    ));
                }
            }
        }
        for quick in &doc.draft.quick_controls {
            if let Some(target) = &quick.target
                && let Err(error) = self.validate_assignment(doc, target)
            {
                warnings.push(format!(
                    "Quick control {}: {}",
                    quick.index + 1,
                    error.message
                ));
            }
        }
        warnings
    }
    pub fn dispatch(&mut self, input: AppInput) -> Vec<Effect> {
        if let AppInput::Ui(intent) = &input
            && self.firmware_editor_blocks_normal_actions()
            && !matches!(intent, UiIntent::Navigate(_) | UiIntent::SetPreferences(_))
        {
            self.state.error(Self::firmware_editor_busy());
            return Vec::new();
        }
        // Original service events still settle their actual requests. A new
        // source generation needs explicit review after any received result.
        if matches!(&input, AppInput::Device(event) if !matches!(event, DeviceEvent::Diagnostic(_)))
            || matches!(&input, AppInput::Library(_) | AppInput::Assets(_))
        {
            self.note_firmware_source_event();
        }
        let refresh_event = match &input {
            AppInput::Device(event)
                if self
                    .firmware_barrier
                    .as_ref()
                    .is_some_and(|b| b.normal_refresh.is_some()) =>
            {
                Some(event.clone())
            }
            _ => None,
        };
        let effects = match input {
            AppInput::Ui(UiIntent::Undo(document)) => self.history(&document, false),
            AppInput::Ui(UiIntent::Redo(document)) => self.history(&document, true),
            AppInput::Ui(intent) => self.ui(intent),
            AppInput::Device(event) => self.device(event),
            AppInput::Library(event) => self.library(event),
            AppInput::Assets(event) => self.assets(event),
            AppInput::Tick(ms) => {
                self.state.monotonic_ms = self.state.monotonic_ms.max(ms);
                self.advance_external_reconciliation()
            }
        };
        if let Some(event) = refresh_event {
            self.observe_firmware_editor_refresh_event(&event);
        }
        self.track_local_effects(&effects);
        effects
    }
    fn intent(&mut self) -> IntentId {
        let id = IntentId(self.next_intent);
        self.next_intent += 1;
        id
    }
    /// Settle an original normal control which the runtime never offered.
    /// This does not manufacture a connection event or advance a session epoch.
    pub fn settle_unoffered_device_control(&mut self, command: DeviceCommand) {
        match command {
            DeviceCommand::Connect { intent, .. } if self.connect_intent == Some(intent) => {
                self.connect_intent = None;
                self.state.connection = ConnectionState::Offline;
            }
            DeviceCommand::Discover if self.state.connection == ConnectionState::Discovering => {
                self.state.connection = if self.state.identity.is_some() {
                    ConnectionState::Ready
                } else {
                    ConnectionState::Offline
                };
            }
            DeviceCommand::Disconnect { .. } | DeviceCommand::Shutdown => {}
            _ => return,
        }
        self.state.error(ServiceError::new(
            ErrorKind::Busy,
            "NotOffered: original normal control retired for firmware preparation",
        ));
    }
    fn stamp(&mut self) -> RequestStamp {
        RequestStamp {
            epoch: self.state.epoch,
            intent: self.intent(),
        }
    }
    pub(crate) fn register_firmware_refresh_connect(
        &mut self,
        selection: DeviceSelection,
    ) -> (IntentId, Vec<Effect>) {
        let intent = self.intent();
        self.connect_intent = Some(intent);
        self.requests.insert(intent, PendingRequest::Connect);
        self.state.connection = ConnectionState::Connecting;
        (
            intent,
            vec![Effect::Device(DeviceCommand::Connect { intent, selection })],
        )
    }
    pub(crate) fn register_firmware_refresh_reads(
        &mut self,
    ) -> (RequestStamp, RequestStamp, Vec<Effect>) {
        let current = self.stamp();
        let globals = self.stamp();
        self.latest_reads
            .insert(PresetTarget::EditBuffer, current.intent);
        self.requests.insert(
            current.intent,
            PendingRequest::Read(PresetTarget::EditBuffer),
        );
        self.requests
            .insert(globals.intent, PendingRequest::ReadDeviceGlobals);
        (
            current,
            globals,
            vec![
                Effect::Device(DeviceCommand::RefreshTarget {
                    stamp: current,
                    target: PresetTarget::EditBuffer,
                }),
                Effect::Device(DeviceCommand::ReadGlobals(globals)),
            ],
        )
    }
    fn advance_external_reconciliation(&mut self) -> Vec<Effect> {
        if self.firmware_editor_blocks_normal_actions() {
            return Vec::new();
        }
        if self.state.connection != ConnectionState::Ready || !self.requests.is_empty() {
            return Vec::new();
        }
        let Some(kind) = self.external_refresh.due(self.state.monotonic_ms) else {
            return Vec::new();
        };
        let effects = match kind {
            crate::external_changes::RefreshKind::Current => {
                self.read(PresetTarget::EditBuffer, false)
            }
            crate::external_changes::RefreshKind::Globals => {
                self.ui(UiIntent::RefreshDeviceGlobals)
            }
        };
        let stamp = effects.first().and_then(|effect| match effect {
            Effect::Device(
                DeviceCommand::RefreshTarget { stamp, .. } | DeviceCommand::ReadGlobals(stamp),
            ) => Some(*stamp),
            _ => None,
        });
        if let Some(stamp) = stamp {
            self.external_refresh
                .started(stamp, kind, self.state.monotonic_ms);
        } else {
            self.external_refresh.abandon(kind);
        }
        effects
    }
    fn require(&mut self, operation: OperationKind) -> bool {
        if !self.admit_normal_mutation() {
            return false;
        }
        if crate::external_changes::mutation_operation(operation) && self.external_write_barrier() {
            self.state.error(ServiceError::new(
                ErrorKind::Busy,
                "Read and review the changed pedal state before another device action",
            ));
            return false;
        }
        match self.operation_availability(operation) {
            OperationAvailability::Available => true,
            OperationAvailability::OfflineOnly => {
                self.state.error(ServiceError::new(
                    ErrorKind::Disconnected,
                    "This action requires a connected, synchronized device",
                ));
                false
            }
            OperationAvailability::Blocked { reason } => {
                self.state
                    .error(ServiceError::new(ErrorKind::Unverified, reason));
                false
            }
        }
    }
    pub(crate) fn open_offline(&mut self, snapshot: Arc<PresetSnapshot>) -> DocumentId {
        let id = DocumentId(format!("offline-{}", self.next_document));
        self.next_document += 1;
        self.state.documents.insert(
            id.clone(),
            EditorDocument::new(id.clone(), EditTarget::Offline(id.clone()), snapshot),
        );
        self.state.active_document = Some(id.clone());
        id
    }
    pub fn capture_comparison(
        &mut self,
        document: &DocumentId,
        slot: ComparisonSlot,
    ) -> Result<(), ServiceError> {
        if self.firmware_editor_blocks_normal_actions() {
            return Err(Self::firmware_editor_busy());
        }
        let draft = self
            .state
            .documents
            .get(document)
            .ok_or_else(|| ServiceError::invalid("Document is absent"))?
            .draft
            .clone();
        self.state
            .comparisons
            .entry(document.clone())
            .or_default()
            .snapshots
            .insert(slot, draft);
        Ok(())
    }
    pub fn recall_comparison(
        &mut self,
        document: &DocumentId,
        slot: ComparisonSlot,
    ) -> Vec<Effect> {
        if !self.admit_normal_mutation() {
            return Vec::new();
        }
        let Some(draft) = self
            .state
            .comparisons
            .get(document)
            .and_then(|comparison| comparison.snapshots.get(&slot))
            .cloned()
        else {
            self.state.error(ServiceError::invalid(
                "Capture this comparison slot before recalling it",
            ));
            return Vec::new();
        };
        self.state
            .comparisons
            .entry(document.clone())
            .or_default()
            .last_recalled = Some(slot);
        self.replace_draft(document, draft)
    }
    /// Snapshot export preserves opaque records and retains local projections.
    pub fn document_snapshot(
        &self,
        document: &DocumentId,
    ) -> Result<Arc<PresetSnapshot>, ServiceError> {
        let doc = self
            .state
            .documents
            .get(document)
            .ok_or_else(|| ServiceError::invalid("Document is absent"))?;
        let mut snapshot = doc.draft.snapshot(
            &doc.baseline,
            SnapshotSource::Capture {
                capture_id: document.0.clone(),
            },
            doc.local_revision,
        )?;
        let existing_refs = snapshot.asset_refs.clone();
        snapshot.asset_refs = snapshot
            .view
            .modules
            .iter()
            .map(|module| {
                match self
                    .catalog
                    .model_by_id(module.module.get(), module.model.0)
                    .map(|model| model.slot_kind())
                {
                    Some(gp200_catalog::SlotKind::UserIr { slot }) => AssetRef::UserSlot {
                        kind: AssetKind::ImpulseResponse,
                        slot,
                        content: existing_refs.iter().find_map(|reference| match reference {
                            AssetRef::UserSlot {
                                kind: AssetKind::ImpulseResponse,
                                slot: old_slot,
                                content,
                            } if *old_slot == slot => content.clone(),
                            _ => None,
                        }),
                    },
                    Some(gp200_catalog::SlotKind::SnapTone { slot }) => AssetRef::UserSlot {
                        kind: AssetKind::SnapTone,
                        slot,
                        content: existing_refs.iter().find_map(|reference| match reference {
                            AssetRef::UserSlot {
                                kind: AssetKind::SnapTone,
                                slot: old_slot,
                                content,
                            } if *old_slot == slot => content.clone(),
                            _ => None,
                        }),
                    },
                    Some(_) => AssetRef::Factory {
                        module: module.module,
                        model: module.model,
                    },
                    None => AssetRef::Unresolved {
                        description: format!(
                            "{} unknown model Code {:08x}",
                            module.module.name(),
                            module.model.0
                        ),
                    },
                }
            })
            .collect();
        Ok(Arc::new(snapshot))
    }
    pub fn execute_prepared_job(&mut self, id: JobId, reviewed_sha256: JobSha256) -> Vec<Effect> {
        if !self.admit_normal_mutation() {
            return Vec::new();
        }
        let Some(prepared) = self.state.prepared_jobs.get(&id).cloned() else {
            self.state
                .error(ServiceError::invalid("Prepared device job is absent"));
            return Vec::new();
        };
        if self.device_busy() || self.blocked_after_uncertainty {
            self.state.error(ServiceError::new(
                ErrorKind::Busy,
                "Finish or reconcile pending operations before applying an asset job",
            ));
            return Vec::new();
        }
        if !self.require(prepared.operation()) {
            return Vec::new();
        }
        let Some(identity) = &self.state.identity else {
            return Vec::new();
        };
        let Some(session) = &self.state.device_job_session else {
            self.state.error(ServiceError::new(
                ErrorKind::Conflict,
                "Device job needs a current actor session observation",
            ));
            return Vec::new();
        };
        let entry = self.state.device_assets.entry(prepared.target());
        let Some(baseline) = entry
            .snapshot
            .as_ref()
            .filter(|_| entry.status == AssetMetadataReadStatus::Ready)
        else {
            self.state.error(ServiceError::new(
                ErrorKind::Conflict,
                "Refresh this asset slot before device job review",
            ));
            return Vec::new();
        };
        match ValidatedDeviceJob::validate(
            prepared,
            reviewed_sha256,
            session,
            identity,
            &self.state.capabilities,
            baseline,
        ) {
            Ok(job) => {
                let stamp = self.stamp();
                self.requests
                    .insert(stamp.intent, PendingRequest::DeviceJob(id));
                vec![Effect::Device(DeviceCommand::ExecuteDeviceJob {
                    stamp,
                    job,
                })]
            }
            Err(error) => {
                self.state.error(error);
                Vec::new()
            }
        }
    }
    fn ui(&mut self, intent: UiIntent) -> Vec<Effect> {
        if self.firmware_editor_blocks_normal_actions()
            && !matches!(&intent, UiIntent::Navigate(_) | UiIntent::SetPreferences(_))
        {
            self.state.error(Self::firmware_editor_busy());
            return Vec::new();
        }
        match intent {
            UiIntent::Navigate(view) => self.state.view = view,
            UiIntent::DiscoverDevices => {
                self.state.connection = ConnectionState::Discovering;
                return vec![Effect::Device(DeviceCommand::Discover)];
            }
            UiIntent::Connect(selection) => {
                let intent = self.intent();
                self.connect_intent = Some(intent);
                self.requests.insert(intent, PendingRequest::Connect);
                self.state.connection = ConnectionState::Connecting;
                return vec![Effect::Device(DeviceCommand::Connect { intent, selection })];
            }
            UiIntent::Disconnect | UiIntent::EnterOfflineMode => {
                let intent = self.intent();
                self.connect_intent = None;
                self.state.connection = ConnectionState::Offline;
                self.reset_external_reconciliation(self.state.epoch);
                self.invalidate_pending("Disconnected while a device operation was pending");
                return vec![Effect::Device(DeviceCommand::Disconnect { intent })];
            }
            UiIntent::Refresh(target) => return self.read(target, false),
            UiIntent::PreviewStored(slot) => return self.read(PresetTarget::Stored(slot), true),
            UiIntent::AuditionStored(slot) => {
                if !self.require(OperationKind::SelectStoredPreset) {
                    return Vec::new();
                }
                if self.device_busy() {
                    self.state.error(ServiceError::new(
                        ErrorKind::Busy,
                        "Finish pending edits before selecting a preset",
                    ));
                    return Vec::new();
                }
                if self
                    .state
                    .documents
                    .get(&device_document_id())
                    .is_some_and(|document| document.dirty())
                {
                    self.state.error(ServiceError::new(
                        ErrorKind::Conflict,
                        "Save or explicitly discard the buffer draft before changing presets",
                    ));
                    return Vec::new();
                }
                let stamp = self.stamp();
                self.requests.insert(
                    stamp.intent,
                    PendingRequest::Select {
                        slot,
                        saw_readback: false,
                        verified: false,
                    },
                );
                self.latest_reads
                    .insert(PresetTarget::EditBuffer, stamp.intent);
                return vec![Effect::Device(DeviceCommand::SelectStored { stamp, slot })];
            }
            UiIntent::OpenLibraryVersion(version) => {
                let intent = self.intent();
                self.loading_versions.insert(intent, version.clone());
                return vec![Effect::Library(LibraryCommand::LoadVersion {
                    intent,
                    version,
                })];
            }
            UiIntent::NewOfflineFrom(snapshot) => {
                self.open_offline(snapshot);
            }
            UiIntent::BeginGesture { document, key } => {
                if let Some(doc) = self.state.documents.get_mut(&document) {
                    if let Some((active, _)) = &doc.undo.gesture.clone() {
                        doc.end_gesture(active);
                    }
                    doc.begin_gesture(key);
                }
            }
            UiIntent::EndGesture { document, key } => {
                if let Some(doc) = self.state.documents.get_mut(&document) {
                    doc.end_gesture(&key);
                }
                return self.flush(&document);
            }
            UiIntent::ApplyEdit { document, patch } => return self.apply_edit(&document, patch),
            UiIntent::ApplyOfflineMerge(request) => {
                let result = self
                    .state
                    .documents
                    .get(&request.document)
                    .ok_or_else(|| ServiceError::invalid("Merge target document is absent"))
                    .and_then(|document| {
                        gp200_library::apply_reviewed_offline_merge(
                            document,
                            &request,
                            &self.catalog,
                        )
                    });
                match result {
                    Ok(draft) => {
                        // The validator admits OFFLINE targets only. Keep this
                        // atomic history update independent of hardware delta,
                        // defaults, defer/flush and stage-in-buffer paths.
                        if let Some(document) = self.state.documents.get_mut(&request.document) {
                            document
                                .undo
                                .past
                                .push(std::mem::replace(&mut document.draft, draft));
                            document.undo.future.clear();
                            document.local_revision.0 += 1;
                        }
                    }
                    Err(error) => self.state.error(error),
                }
                return Vec::new();
            }
            UiIntent::Undo(document) => return self.history(&document, false),
            UiIntent::Redo(document) => return self.history(&document, true),
            UiIntent::DiscardDraft(document) => {
                if self.has_pending_edit(&document) {
                    self.state.error(ServiceError::new(
                        ErrorKind::Busy,
                        "Wait for pending edits before discarding the draft",
                    ));
                    return Vec::new();
                }
                if let Some(doc) = self.state.documents.get(&document) {
                    return self
                        .replace_draft(&document, PresetDraft::from_snapshot(&doc.baseline));
                }
            }
            UiIntent::SaveBufferTo {
                slot,
                expected_baseline,
            } => return self.save(slot, expected_baseline),
            UiIntent::WriteStoredPreset {
                slot,
                expected_target_revision,
                snapshot,
            } => {
                if !self.require(OperationKind::PresetImport) {
                    return Vec::new();
                }
                if self.device_busy() || self.blocked_after_uncertainty {
                    self.state.error(ServiceError::new(ErrorKind::Busy, "Finish or reconcile pending pedal operations before writing a stored preset"));
                    return Vec::new();
                }
                let Some(target) = self.state.bank.slots.get(&slot) else {
                    self.state.error(ServiceError::invalid(
                        "Read the destination slot before writing it",
                    ));
                    return Vec::new();
                };
                if !target.fresh
                    || target.snapshot.revision != expected_target_revision
                    || target.snapshot.context
                        != Some(WireContext {
                            group: 1,
                            selector: slot.get(),
                        })
                {
                    self.state.error(ServiceError::new(
                        ErrorKind::Conflict,
                        "Destination changed since the write preview. Read and review it again",
                    ));
                    return Vec::new();
                }
                if self
                    .state
                    .identity
                    .as_ref()
                    .is_none_or(|identity| identity.model != snapshot.device_model)
                {
                    self.state.error(ServiceError::invalid(
                        "Source preset variant differs from the connected pedal",
                    ));
                    return Vec::new();
                }
                let stamp = self.stamp();
                self.latest_reads
                    .insert(PresetTarget::Stored(slot), stamp.intent);
                self.requests.insert(
                    stamp.intent,
                    PendingRequest::WriteStored {
                        slot,
                        source: Some(snapshot.clone()),
                        saw_readback: false,
                        verified: false,
                    },
                );
                return vec![Effect::Device(DeviceCommand::WriteStoredPreset {
                    stamp,
                    slot,
                    expected_target_revision,
                    snapshot,
                })];
            }
            UiIntent::ApplyGlobalEdit(patch) => {
                if self.device_busy() || self.blocked_after_uncertainty {
                    self.state.error(ServiceError::new(
                        ErrorKind::Busy,
                        "Reconcile pending device operations before changing global settings",
                    ));
                    return Vec::new();
                }
                if !self.require(OperationKind::WriteGlobals) {
                    return Vec::new();
                }
                let Some(globals) = &self.state.globals else {
                    self.state.error(ServiceError::invalid(
                        "Read global settings before editing them",
                    ));
                    return Vec::new();
                };
                let base_revision = globals.revision;
                let stamp = self.stamp();
                self.requests.insert(stamp.intent, PendingRequest::Global);
                return vec![Effect::Device(DeviceCommand::SubmitGlobalEdit {
                    stamp,
                    base_revision,
                    patch,
                })];
            }
            UiIntent::RefreshDeviceGlobals => {
                if !self.require(OperationKind::ReadGlobals) {
                    return Vec::new();
                }
                if self.device_busy()
                    || self
                        .requests
                        .values()
                        .any(|r| matches!(r, PendingRequest::ReadDeviceGlobals))
                {
                    self.state.error(ServiceError::new(
                        ErrorKind::Busy,
                        "Wait for the pending device request before reading global settings",
                    ));
                    return Vec::new();
                }
                let stamp = self.stamp();
                self.requests
                    .insert(stamp.intent, PendingRequest::ReadDeviceGlobals);
                return vec![Effect::Device(DeviceCommand::ReadGlobals(stamp))];
            }
            UiIntent::RefreshAssetMetadata(slot) => {
                let operation = match slot.bank() {
                    AssetBank::Ir => OperationKind::IrRead,
                    AssetBank::SnapTone => OperationKind::SnapToneRead,
                };
                if !self.require(operation) {
                    return Vec::new();
                }
                if self.device_busy() {
                    self.state.error(ServiceError::new(
                        ErrorKind::Busy,
                        "Wait for the pending device request before reading asset metadata",
                    ));
                    return Vec::new();
                }
                let stamp = self.stamp();
                self.state.device_assets.begin(slot, stamp);
                self.requests
                    .insert(stamp.intent, PendingRequest::ReadAssetMetadata(slot));
                return vec![Effect::Device(DeviceCommand::ReadAssetMetadata {
                    stamp,
                    slot,
                })];
            }
            UiIntent::ApplyGlobalControlEdit(patch) => {
                if !self.require(OperationKind::WriteGlobals) {
                    return Vec::new();
                }
                if self.device_busy() || self.blocked_after_uncertainty {
                    self.state.error(ServiceError::new(ErrorKind::Busy, "Read and reconcile pending device operations before changing global settings"));
                    return Vec::new();
                }
                let Some(before) = self
                    .state
                    .device_globals
                    .clone()
                    .filter(|g| g.epoch == self.state.epoch)
                else {
                    self.state.error(ServiceError::invalid(
                        "Read global settings in this connection before editing them",
                    ));
                    return Vec::new();
                };
                if let Err(error) = before.expected_after(&patch) {
                    self.state.error(ServiceError::invalid(error.to_string()));
                    return Vec::new();
                }
                let stamp = self.stamp();
                self.requests.insert(
                    stamp.intent,
                    PendingRequest::GlobalControl {
                        before: before.clone(),
                        patch: patch.clone(),
                        saw_readback: false,
                        verified: false,
                    },
                );
                return vec![Effect::Device(DeviceCommand::SubmitGlobalControlEdit {
                    stamp,
                    base_revision: before.revision,
                    patch,
                })];
            }
            UiIntent::ClearStoredPreset {
                slot,
                expected_target_revision,
            } => {
                if !self.require(OperationKind::ClearPreset) {
                    return Vec::new();
                }
                if self.device_busy() || self.blocked_after_uncertainty {
                    self.state.error(ServiceError::new(
                        ErrorKind::Busy,
                        "Finish or reconcile pending pedal operations before clearing a slot",
                    ));
                    return Vec::new();
                }
                let Some(target) = self.state.bank.slots.get(&slot) else {
                    self.state.error(ServiceError::invalid(
                        "Read the destination slot before clearing it",
                    ));
                    return Vec::new();
                };
                if !target.fresh
                    || target.snapshot.revision != expected_target_revision
                    || target.snapshot.context
                        != Some(WireContext {
                            group: 1,
                            selector: slot.get(),
                        })
                {
                    self.state.error(ServiceError::new(
                        ErrorKind::Conflict,
                        "Destination changed since the clear preview. Read and review it again",
                    ));
                    return Vec::new();
                }
                let stamp = self.stamp();
                self.latest_reads
                    .insert(PresetTarget::Stored(slot), stamp.intent);
                self.requests.insert(
                    stamp.intent,
                    PendingRequest::WriteStored {
                        slot,
                        source: None,
                        saw_readback: false,
                        verified: false,
                    },
                );
                return vec![Effect::Device(DeviceCommand::ClearStoredPreset {
                    stamp,
                    slot,
                    expected_target_revision,
                })];
            }
            UiIntent::Library(command) => return vec![Effect::Library(command)],
            UiIntent::StartAssetJob(spec) => {
                return vec![Effect::Assets(AssetCommand::Prepare(spec))];
            }
            UiIntent::StartFirmwareJob(spec) => {
                return vec![Effect::Assets(AssetCommand::Prepare(AssetJobSpec {
                    id: spec.id,
                    kind: AssetKind::Firmware,
                    source: spec.source,
                    output: None,
                    options: Default::default(),
                }))];
            }
            UiIntent::CancelJob(id) => {
                let stamp = self.stamp();
                self.requests.insert(stamp.intent, PendingRequest::Cancel);
                return vec![
                    Effect::Assets(AssetCommand::Cancel(id)),
                    Effect::Library(LibraryCommand::Cancel(id)),
                    Effect::Device(DeviceCommand::CancelJob { stamp, job_id: id }),
                ];
            }
            UiIntent::ResolveConflict(resolution) => return self.resolve(resolution),
            UiIntent::SetPreferences(preferences) => {
                if !preferences.scale.is_finite() || !(0.5..=3.0).contains(&preferences.scale) {
                    self.state.error(ServiceError::invalid(
                        "Preferences need a finite scale from 0.5 to 3",
                    ));
                } else {
                    self.state.preferences = preferences.clone();
                    return vec![Effect::PersistSettings(preferences)];
                }
            }
        }
        Vec::new()
    }
    fn apply_edit(&mut self, document: &DocumentId, patch: EditPatch) -> Vec<Effect> {
        if !self.admit_normal_mutation() {
            return Vec::new();
        }
        let result = self
            .state
            .documents
            .get(document)
            .ok_or_else(|| ServiceError::invalid("Document is absent"))
            .and_then(|doc| self.validate_patch(doc, &patch));
        if let Err(error) = result {
            self.state.error(error);
            return Vec::new();
        }
        let dependent = match &patch {
            EditPatch::SetModel { module, model }
                if self
                    .state
                    .documents
                    .get(document)
                    .is_some_and(|doc| doc.target == EditTarget::DeviceBuffer) =>
            {
                match self.dependent_model_plan(*module, *model) {
                    Ok(plan) => plan,
                    Err(error) => {
                        self.state.error(error);
                        return Vec::new();
                    }
                }
            }
            _ => None,
        };
        if let Some((plan, _)) = &dependent
            && (self.device_busy()
                || self.state.documents.get(document).is_none_or(|doc| {
                    doc.draft
                        .view
                        .module(plan.module())
                        .is_none_or(|module| module.model != plan.previous())
                }))
        {
            self.state.error(ServiceError::new(ErrorKind::Busy, "Wait for the current operation and reconcile the confirmed dependent-model source first"));
            return Vec::new();
        }
        let Some(doc) = self.state.documents.get_mut(document) else {
            return Vec::new();
        };
        // Re-selecting the same Code is inert: it must not reset controls or
        // create a history step merely because this model has XML defaults.
        if let EditPatch::SetModel { module, model } = &patch
            && doc
                .draft
                .view
                .module(*module)
                .is_some_and(|current| current.model == *model)
        {
            return Vec::new();
        }
        let before = doc.draft.clone();
        if let Err(error) = doc.apply(&patch) {
            self.state.error(error);
            return Vec::new();
        }
        if let EditPatch::SetModel { module, model } = &patch
            && let Some(definition) = self.catalog.model_by_id(module.get(), model.0)
            && let Some(module) = doc
                .draft
                .view
                .modules
                .iter_mut()
                .find(|candidate| candidate.module == *module)
        {
            let mut bits: Vec<u32> = module.parameters.iter().map(|value| value.0).collect();
            gp200_catalog::ModelDefaultPlan::from_definition(definition)
                .apply_to_bits(&mut bits)
                .expect("validated model default slots");
            for (destination, bits) in module.parameters.iter_mut().zip(bits) {
                *destination = F32Bits(bits);
            }
        }
        if let Some((plan, _)) = &dependent
            && let Some(cabinet) = plan.paired_cabinet()
            && let Some(definition) = self.catalog.model_by_id(5, cabinet.0)
            && let Some(module) = doc
                .draft
                .view
                .modules
                .iter_mut()
                .find(|module| module.module.get() == 5)
        {
            module.model = cabinet;
            let mut bits: Vec<u32> = module.parameters.iter().map(|value| value.0).collect();
            gp200_catalog::ModelDefaultPlan::from_definition(definition)
                .apply_to_bits(&mut bits)
                .expect("validated paired cabinet defaults");
            for (destination, bits) in module.parameters.iter_mut().zip(bits) {
                *destination = F32Bits(bits);
            }
        }
        if equivalent(&before, &doc.draft) {
            return Vec::new();
        }
        if doc.target == EditTarget::DeviceBuffer {
            let patch = match patch {
                EditPatch::SetMetadata(metadata) => {
                    let native = metadata.device_fields();
                    if native == MetadataPatch::default() {
                        return Vec::new();
                    }
                    EditPatch::SetMetadata(native)
                }
                patch => patch,
            };
            self.state.buffer_persistence = BufferPersistence::Unsaved;
            self.defer(document, patch);
            return self.flush(document);
        }
        Vec::new()
    }
    fn defer(&mut self, document: &DocumentId, patch: EditPatch) {
        let queue = self.deferred.entry(document.clone()).or_default();
        if let EditPatch::SetModel { module, .. } = &patch {
            // Controls queued before this selection belong to the old Code
            // and XML Index. Their desired values are superseded by defaults.
            queue.retain(|queued| {
                !matches!(queued,
                EditPatch::SetParameter { module: affected, .. }
                | EditPatch::SetSync { module: affected, .. } if affected == module)
            });
        }
        if let Some(index) = queue.iter().position(|old| same_field(old, &patch)) {
            if let (EditPatch::SetMetadata(old), EditPatch::SetMetadata(newer)) =
                (&mut queue[index], &patch)
            {
                old.overlay(newer);
            } else {
                queue[index] = patch;
            }
        } else {
            queue.push(patch);
        }
    }
    pub(crate) fn dependent_model_plan(
        &self,
        module: ModuleId,
        model: ModelCode,
    ) -> Result<Option<(BoundModelPlan, Arc<DeviceGlobalState>)>, ServiceError> {
        if self.state.connection != ConnectionState::Ready
            || self
                .state
                .identity
                .as_ref()
                .is_none_or(|identity| identity.mode != BackendMode::Hardware)
        {
            return Ok(None);
        }
        let Some(snapshot) = self.state.confirmed_buffer.as_ref() else {
            return Ok(None);
        };
        if snapshot.view.module(module).is_none_or(|previous| {
            !dependent_model_pair(module, previous.model, model)
                && !factory_amp_pair(module, previous.model, model)
        }) {
            return Ok(None);
        }
        let globals = self.state.device_globals.clone().ok_or_else(|| {
            ServiceError::new(
                ErrorKind::Conflict,
                "Read complete globals before dependent model selection",
            )
        })?;
        if globals.epoch != self.state.epoch {
            return Err(ServiceError::new(
                ErrorKind::Conflict,
                "Dependent model globals belong to a different session",
            ));
        }
        let plan = if snapshot
            .view
            .module(module)
            .is_some_and(|previous| factory_amp_pair(module, previous.model, model))
        {
            let plan = FactoryAmpPlan::from_snapshot(snapshot, &globals, model)?;
            if !plan.profile.normal_build_accepted() && !cfg!(feature = "factory-amp-trials") {
                return Err(ServiceError::new(
                    ErrorKind::Unverified,
                    "This exact factory AMP candidate awaits complete bidirectional production acceptance",
                ));
            }
            validate_factory_amp_catalog(&self.catalog, &plan, snapshot)?;
            let expected = Arc::new(plan.projected_raw(snapshot)?);
            BoundModelPlan::Factory {
                plan,
                expected,
                reviewed_revision: snapshot.revision,
            }
        } else {
            let plan = DependentModelPlan::from_snapshot(snapshot, &globals, module, model)?;
            if !plan.profile.normal_build_accepted() && !cfg!(feature = "dependent-model-trials") {
                return Err(ServiceError::new(
                    ErrorKind::Unverified,
                    "Dependent model candidate awaits root's bidirectional production acceptance",
                ));
            }
            BoundModelPlan::Existing(plan)
        };
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
            if !gp200_catalog::ModelDefaultPlan::from_definition(definition)
                .unknown_defaults
                .is_empty()
            {
                return Err(ServiceError::new(
                    ErrorKind::Unverified,
                    "Dependent model target has unrecovered defaults",
                ));
            }
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
                    "Dependent AMP paired cabinet differs from the catalog",
                ));
            }
        }
        Ok(Some((plan, globals)))
    }
    fn has_pending_edit(&self, document: &DocumentId) -> bool {
        self.requests.values().any(|request| matches!(request, PendingRequest::Edit { document: id, .. } if id == document))
    }
    fn device_busy(&self) -> bool {
        self.requests.values().any(|request| {
            matches!(
                request,
                PendingRequest::Edit { .. }
                    | PendingRequest::Save { .. }
                    | PendingRequest::WriteStored { .. }
                    | PendingRequest::Select { .. }
                    | PendingRequest::DeviceJob(_)
                    | PendingRequest::Global
                    | PendingRequest::GlobalControl { .. }
                    | PendingRequest::ReadDeviceGlobals
                    | PendingRequest::ReadAssetMetadata(_)
            )
        })
    }
    pub(crate) fn flush(&mut self, document: &DocumentId) -> Vec<Effect> {
        if self.firmware_editor_blocks_normal_actions() {
            return Vec::new();
        }
        if self.device_busy()
            || self.blocked_after_uncertainty
            || self.external_write_barrier()
            || self.state.connection != ConnectionState::Ready
        {
            return Vec::new();
        }
        let Some(doc) = self.state.documents.get(document) else {
            return Vec::new();
        };
        if doc.target != EditTarget::DeviceBuffer || !doc.conflicts.is_empty() {
            return Vec::new();
        }
        let Some(confirmed) = self.state.confirmed_buffer.clone() else {
            return Vec::new();
        };
        let Some(queue) = self.deferred.get_mut(document) else {
            return Vec::new();
        };
        while !queue.is_empty() && patch_matches(&queue[0], &confirmed) {
            queue.remove(0);
        }
        let Some(patch) = queue.first().cloned() else {
            return Vec::new();
        };
        if self.operation_availability(patch.operation()) != OperationAvailability::Available {
            return Vec::new(); // Local draft remains usable; actor receives no unverified write.
        }
        // A queued control may have become invalid after model selection/physical changes.
        if let Some(doc) = self.state.documents.get(document)
            && let Err(error) = self.validate_patch(doc, &patch)
        {
            self.state.error(error);
            return Vec::new();
        }
        let dependent = match &patch {
            EditPatch::SetModel { module, model } => {
                match self.dependent_model_plan(*module, *model) {
                    Ok(plan) => plan,
                    Err(error) => {
                        self.state.error(error);
                        return Vec::new();
                    }
                }
            }
            _ => None,
        };
        self.deferred
            .get_mut(document)
            .expect("queue exists")
            .remove(0);
        let stamp = self.stamp();
        if self
            .history_continuation
            .as_ref()
            .is_some_and(|continuation| {
                continuation.document == *document
                    && (continuation.owned.is_some()
                        || continuation.remaining.first() != Some(&patch))
            })
        {
            self.history_continuation = None;
        }
        if let Some(continuation) = &mut self.history_continuation
            && continuation.document == *document
        {
            // Only a newly verified history step can schedule its next declared
            // parameter. The token never retries a submitted/uncertain step.
            continuation.owned = Some(stamp.intent);
            continuation.remaining.remove(0);
        }
        let doc = self
            .state
            .documents
            .get_mut(document)
            .expect("document exists");
        doc.pending.insert(
            stamp.intent,
            PendingEdit {
                patch: patch.clone(),
                stage: EditStage::Queued,
                revision: doc.local_revision,
            },
        );
        self.requests.insert(
            stamp.intent,
            PendingRequest::Edit {
                document: document.clone(),
                patch: patch.clone(),
                saw_readback: false,
                verified: false,
                dependent: dependent
                    .as_ref()
                    .map(|(plan, globals)| PendingDependentModel {
                        plan: plan.clone(),
                        before_globals: globals.clone(),
                        saw_globals: false,
                    }),
            },
        );
        if let Some((plan, before_globals)) = dependent {
            let command = match plan {
                BoundModelPlan::Existing(plan) => DeviceCommand::SubmitDependentModel {
                    stamp,
                    base_revision: confirmed.revision,
                    module: plan.module,
                    model: plan.target,
                    before_globals,
                    expected_auto_cab: plan.auto_cab,
                },
                BoundModelPlan::Factory { plan, .. } => DeviceCommand::SubmitFactoryAmpModel {
                    stamp,
                    base_revision: confirmed.revision,
                    model: plan.target,
                    profile: plan.profile,
                    before_globals,
                    expected_auto_cab: plan.auto_cab,
                },
            };
            return vec![Effect::Device(command)];
        }
        vec![Effect::Device(DeviceCommand::SubmitEdit {
            stamp,
            target: PresetTarget::EditBuffer,
            base_revision: confirmed.revision,
            patch,
        })]
    }
    fn read(&mut self, target: PresetTarget, preview: bool) -> Vec<Effect> {
        let operation = match target {
            PresetTarget::EditBuffer => OperationKind::ReadEditBuffer,
            PresetTarget::Stored(_) => OperationKind::ReadStoredPreset,
        };
        if !self.require(operation) {
            return Vec::new();
        }
        let stamp = self.stamp();
        self.latest_reads.insert(target, stamp.intent);
        self.requests.insert(
            stamp.intent,
            match (preview, target) {
                (true, PresetTarget::Stored(slot)) => PendingRequest::Preview(slot),
                _ => PendingRequest::Read(target),
            },
        );
        vec![Effect::Device(DeviceCommand::RefreshTarget {
            stamp,
            target,
        })]
    }
    fn save(&mut self, slot: SlotId, expected: Revision) -> Vec<Effect> {
        if !self.require(OperationKind::SavePreset) {
            return Vec::new();
        }
        let Some(buffer) = self.state.confirmed_buffer.clone() else {
            self.state.error(ServiceError::invalid(
                "Read the current buffer before saving",
            ));
            return Vec::new();
        };
        let Some(doc) = self.state.documents.get(&device_document_id()) else {
            return Vec::new();
        };
        if expected != buffer.revision
            || self.device_busy()
            || self.blocked_after_uncertainty
            || !doc.conflicts.is_empty()
            || !equivalent(&doc.draft, &PresetDraft::from_snapshot(&buffer))
        {
            self.state.error(ServiceError::new(ErrorKind::Conflict, "Save requires the exact verified buffer revision, with no pending edits or local-only changes"));
            return Vec::new();
        }
        let stamp = self.stamp();
        self.requests.insert(
            stamp.intent,
            PendingRequest::Save {
                slot,
                buffer: buffer.clone(),
                current_buffer: None,
                saw_readback: false,
                verified: false,
            },
        );
        self.state.buffer_persistence = BufferPersistence::Saving {
            slot,
            intent: stamp.intent,
        };
        vec![Effect::Device(DeviceCommand::SaveBufferTo {
            stamp,
            slot,
            expected_buffer_revision: buffer.revision,
        })]
    }
    fn replace_draft(&mut self, document: &DocumentId, draft: PresetDraft) -> Vec<Effect> {
        if !self.admit_normal_mutation() {
            return Vec::new();
        }
        let Some(doc) = self.state.documents.get_mut(document) else {
            return Vec::new();
        };
        if equivalent(&doc.draft, &draft) {
            return Vec::new();
        }
        doc.undo.past.push(std::mem::replace(&mut doc.draft, draft));
        doc.undo.future.clear();
        doc.local_revision.0 += 1;
        if doc.target == EditTarget::DeviceBuffer {
            self.queue_delta(document);
            if self
                .deferred
                .get(document)
                .is_some_and(|queue| !queue.is_empty())
            {
                self.state.buffer_persistence = BufferPersistence::Unsaved;
            }
            return self.flush(document);
        }
        Vec::new()
    }
    fn queue_delta(&mut self, document: &DocumentId) {
        let (Some(doc), Some(confirmed)) = (
            self.state.documents.get(document),
            &self.state.confirmed_buffer,
        ) else {
            return;
        };
        let patches = self.delta(&doc.draft, confirmed);
        self.deferred.insert(document.clone(), patches);
    }
    fn history(&mut self, document: &DocumentId, redo: bool) -> Vec<Effect> {
        if !self.admit_normal_mutation() {
            return Vec::new();
        }
        if self
            .history_continuation
            .as_ref()
            .is_some_and(|continuation| continuation.document == *document)
        {
            self.history_continuation = None;
        }
        let Some(doc) = self.state.documents.get_mut(document) else {
            return Vec::new();
        };
        if let Some((key, _)) = doc.undo.gesture.clone() {
            doc.end_gesture(&key);
        }
        let changed = if redo { doc.redo() } else { doc.undo() };
        if changed && doc.target == EditTarget::DeviceBuffer {
            self.queue_delta(document);
            if self
                .deferred
                .get(document)
                .is_some_and(|queue| !queue.is_empty())
            {
                self.state.buffer_persistence = BufferPersistence::Unsaved;
            }
            let effects = self.flush(document);
            self.begin_history_continuation(document, &effects);
            return effects;
        }
        Vec::new()
    }
    pub(crate) fn delta(&self, draft: &PresetDraft, confirmed: &PresetSnapshot) -> Vec<EditPatch> {
        let mut patches = Vec::new();
        for module in &draft.view.modules {
            let Some(actual) = confirmed.view.module(module.module) else {
                continue;
            };
            if module.model != actual.model {
                patches.push(EditPatch::SetModel {
                    module: module.module,
                    model: module.model,
                });
                // Readback establishes the new model first. Only then derive
                // follow-up controls using its current XML Index and defaults.
                continue;
            }
            if module.state_raw <= 1 && module.state_raw != actual.state_raw {
                patches.push(EditPatch::SetEnabled {
                    module: module.module,
                    enabled: module.state_raw == 1,
                });
            }
            if let Some(model) = self
                .catalog
                .model_by_id(module.module.get(), module.model.0)
            {
                // History restores the paired Power transition before its
                // numeric control. Sync On initializes 1/4; an older different
                // subdivision therefore needs a subsequent numeric write.
                let mut paired_values = std::collections::BTreeMap::new();
                for control in &model.parameters {
                    if !matches!(
                        gp200_catalog::control_semantics(control),
                        gp200_catalog::ControlSemantics::SyncPower { .. }
                    ) {
                        continue;
                    }
                    let Ok(power_parameter) = ParamId::new(control.parameter_id) else {
                        continue;
                    };
                    let Ok(value_id) =
                        gp200_catalog::sync_value_parameter(model, control.parameter_id)
                    else {
                        continue;
                    };
                    let Ok(value_parameter) = ParamId::new(value_id) else {
                        continue;
                    };
                    let (Some(power), Some(value)) = (
                        module.parameter(power_parameter),
                        module.parameter(value_parameter),
                    ) else {
                        continue;
                    };
                    if (power.as_f32() == 0.0 || power.as_f32() == 1.0)
                        && (module.model != actual.model
                            || actual.parameter(power_parameter) != Some(power))
                    {
                        let enabled = power.as_f32() == 1.0;
                        patches.push(EditPatch::SetSync {
                            module: module.module,
                            model: module.model,
                            model_index: ModelIndex(u32::from(model.xml_index)),
                            power_parameter,
                            value_parameter,
                            enabled,
                            ordinary_restore: (!enabled).then_some(value),
                        });
                        paired_values.insert(
                            value_parameter,
                            if enabled {
                                F32Bits::from_f32(4.0)
                            } else {
                                value
                            },
                        );
                    }
                }
                for control in &model.parameters {
                    if matches!(
                        gp200_catalog::control_semantics(control),
                        gp200_catalog::ControlSemantics::SyncPower { .. }
                    ) {
                        continue;
                    }
                    let Ok(parameter) = ParamId::new(control.parameter_id) else {
                        continue;
                    };
                    let expected = paired_values
                        .get(&parameter)
                        .copied()
                        .or_else(|| actual.parameter(parameter));
                    if let Some(value) = module.parameter(parameter)
                        && ((module.model != actual.model
                            && !paired_values.contains_key(&parameter))
                            || expected != Some(value))
                    {
                        patches.push(EditPatch::SetParameter {
                            module: module.module,
                            model: module.model,
                            model_index: ModelIndex(u32::from(model.xml_index)),
                            parameter,
                            value,
                        });
                    }
                }
            }
        }
        let mut metadata = MetadataPatch::default();
        if draft.view.name.value() != confirmed.view.name.value() {
            metadata.name = draft.view.name.value().cloned();
        }
        if draft.view.tempo.value() != confirmed.view.tempo.value() {
            metadata.tempo = draft.view.tempo.value().copied();
        }
        if draft.view.level.value() != confirmed.view.level.value() {
            metadata.level = draft.view.level.value().copied();
        }
        macro_rules! native_delta { ($($field:ident),+) => { $(if draft.metadata.$field != confirmed.view.metadata.$field { metadata.$field = draft.metadata.$field; })+ }; }
        native_delta!(
            native_pan,
            native_style,
            fx_send_level,
            fx_return_level,
            fx_loop_mode
        );
        if metadata != MetadataPatch::default() {
            patches.push(EditPatch::SetMetadata(metadata));
        }
        if draft.view.chain.value() != confirmed.view.chain.value()
            && let Some(chain) = draft.view.chain.value()
        {
            for (index, module) in chain.iter().enumerate().rev() {
                patches.push(EditPatch::MoveModule {
                    module: *module,
                    before: chain.get(index + 1).copied(),
                });
            }
        }
        for value in &draft.expressions {
            if !confirmed.view.expressions.contains(value) {
                patches.push(EditPatch::SetExpression(value.clone()));
            }
        }
        for value in &draft.footswitches {
            if !confirmed.view.footswitches.contains(value) {
                patches.push(EditPatch::SetFootswitch(value.clone()));
            }
        }
        for value in &draft.quick_controls {
            if !confirmed.view.quick_controls.contains(value) {
                patches.push(EditPatch::SetQuickControl(value.clone()));
            }
        }
        if let (Some(new), Some(old)) = (
            &draft.view.patch_assignments.routing,
            &confirmed.view.patch_assignments.routing,
        ) && new.record != old.record
        {
            patches.push(EditPatch::SetPatchAssignment(
                AssignmentEdit::FxLoopPositions {
                    send_boundary: new.record.send_boundary,
                    return_boundary: new.record.return_boundary,
                },
            ));
        }
        for new in &draft.view.patch_assignments.expressions {
            let Some(old) = confirmed
                .view
                .patch_assignments
                .expressions
                .iter()
                .find(|entry| entry.record.slot == new.record.slot)
            else {
                continue;
            };
            if new.record == old.record {
                continue;
            }
            let target =
                match PatchAssignments::resolve_expression(&new.record, &draft.view.modules) {
                    ResolvedAssignmentReference::Off => ExpressionAssignmentTarget::Off,
                    ResolvedAssignmentReference::Parameter(target) => {
                        ExpressionAssignmentTarget::Parameter(target)
                    }
                    _ => continue,
                };
            let edit = if (new.record.module_raw, new.record.parameter_raw)
                != (old.record.module_raw, old.record.parameter_raw)
            {
                AssignmentEdit::ExpressionTargetAndRange {
                    slot: new.record.slot,
                    target,
                    minimum: new.record.minimum,
                    maximum: new.record.maximum,
                }
            } else {
                AssignmentEdit::ExpressionRange {
                    slot: new.record.slot,
                    minimum: new.record.minimum,
                    maximum: new.record.maximum,
                }
            };
            patches.push(EditPatch::SetPatchAssignment(edit));
        }
        for new in &draft.view.patch_assignments.ctrls {
            let Some(old) = confirmed
                .view
                .patch_assignments
                .ctrls
                .iter()
                .find(|entry| entry.record.slot == new.record.slot)
            else {
                continue;
            };
            if (new.record.module_mask_raw & 0x7ff) != (old.record.module_mask_raw & 0x7ff) {
                patches.push(EditPatch::SetPatchAssignment(AssignmentEdit::CtrlModules {
                    slot: new.record.slot,
                    modules: ModuleId::all()
                        .filter(|module| new.record.module_mask_raw & (1 << module.get()) != 0)
                        .collect(),
                }));
            }
            if new.record.state_raw != old.record.state_raw && new.record.state_raw <= 1 {
                patches.push(EditPatch::SetPatchAssignment(AssignmentEdit::CtrlState {
                    slot: new.record.slot,
                    enabled: new.record.state_raw == 1,
                }));
            }
        }
        for new in &draft.view.patch_assignments.quicks {
            let Some(old) = confirmed
                .view
                .patch_assignments
                .quicks
                .iter()
                .find(|entry| entry.record.index == new.record.index)
            else {
                continue;
            };
            if new.record == old.record {
                continue;
            }
            let target = match PatchAssignments::resolve_quick(&new.record, &draft.view.modules) {
                ResolvedAssignmentReference::Off => QuickAssignmentTarget::Off,
                ResolvedAssignmentReference::PatchVolume => QuickAssignmentTarget::PatchVolume,
                ResolvedAssignmentReference::PatchTempo => QuickAssignmentTarget::PatchTempo,
                ResolvedAssignmentReference::Parameter(target) => {
                    QuickAssignmentTarget::Parameter(target)
                }
                _ => continue,
            };
            patches.push(EditPatch::SetPatchAssignment(AssignmentEdit::QuickTarget {
                index: new.record.index,
                target,
            }));
        }
        patches
    }
    fn adopt_epoch(&mut self, epoch: SessionEpoch) {
        if epoch == self.state.epoch {
            return;
        }
        self.invalidate_pending("The device session changed before completion");
        self.reset_external_reconciliation(epoch);
        self.state.epoch = epoch;
        self.state.identity = None;
        self.state.device_job_session = None;
        self.state.capabilities = Capabilities::empty(DeviceModel::Gp200);
        self.state.confirmed_buffer = None;
        self.state.globals = None;
        self.state.device_globals = None;
        self.state.device_assets.invalidate(epoch);
        self.state.buffer_persistence = BufferPersistence::Unknown;
        self.state.bank.epoch = epoch;
        for cached in self.state.bank.slots.values_mut() {
            cached.fresh = false;
        }
        self.latest_reads.clear();
        if !self.firmware_editor_blocks_normal_actions() {
            self.deferred.clear();
            self.blocked_after_uncertainty = false;
            self.conflict_fields.clear();
        }
        if let Some(mut document) = self.state.documents.remove(&device_document_id()) {
            let id = DocumentId(format!("detached-{}-{}", epoch.0, self.next_document));
            self.next_document += 1;
            document.id = id.clone();
            document.target = EditTarget::Offline(id.clone());
            if self.firmware_editor_blocks_normal_actions() {
                if let Some(queue) = self.deferred.remove(&device_document_id()) {
                    self.deferred.insert(id.clone(), queue);
                }
                let fields = std::mem::take(&mut self.conflict_fields);
                self.conflict_fields = fields
                    .into_iter()
                    .map(|((document, intent), fields)| {
                        (
                            (
                                if document == device_document_id() {
                                    id.clone()
                                } else {
                                    document
                                },
                                intent,
                            ),
                            fields,
                        )
                    })
                    .collect();
            }
            if let Some(comparison) = self.state.comparisons.remove(&device_document_id()) {
                self.state.comparisons.insert(id.clone(), comparison);
            }
            if let Some(uncertainty) = self
                .state
                .recovered_uncertainty
                .remove(&device_document_id())
            {
                self.state
                    .recovered_uncertainty
                    .insert(id.clone(), uncertainty);
            }
            if self.state.active_document == Some(device_document_id()) {
                self.state.active_document = Some(id.clone());
            }
            self.state.documents.insert(id, document);
        }
    }
    fn invalidate_pending(&mut self, reason: &str) {
        // Non-document writes also lose correlation at an epoch/connection
        // boundary. Preserve their exact original stamp descriptions before
        // the ordinary request map is retired; they cannot become readiness.
        if self.firmware_editor_blocks_normal_actions() {
            for uncertainty in self.recovery_session().session_uncertainty {
                if !self
                    .state
                    .recovered_session_uncertainty
                    .contains(&uncertainty)
                {
                    self.state.recovered_session_uncertainty.push(uncertainty);
                }
            }
        }
        let mut error_seen = false;
        for document in self.state.documents.values_mut() {
            for pending in document.pending.values_mut() {
                if matches!(pending.stage, EditStage::Sent | EditStage::AwaitingReadback) {
                    pending.stage = EditStage::Indeterminate(ServiceError::new(
                        ErrorKind::Indeterminate,
                        reason,
                    ));
                    error_seen = true;
                } else if matches!(pending.stage, EditStage::Queued) {
                    pending.stage =
                        EditStage::Failed(ServiceError::new(ErrorKind::Cancelled, reason));
                }
            }
        }
        if let BufferPersistence::Saving { slot, .. } = self.state.buffer_persistence {
            self.state.buffer_persistence = BufferPersistence::Indeterminate {
                slot,
                reason: reason.into(),
            };
        }
        self.requests.clear();
        if error_seen {
            self.state.message(MessageSeverity::Warning, reason, None);
        }
    }
    fn device(&mut self, event: DeviceEvent) -> Vec<Effect> {
        match event {
            DeviceEvent::PortsChanged(ports) => {
                self.state.ports = ports;
                if self.state.connection == ConnectionState::Discovering {
                    self.state.connection = if self.state.identity.is_some() {
                        ConnectionState::Ready
                    } else {
                        ConnectionState::Offline
                    };
                }
            }
            DeviceEvent::ConnectionChanged { epoch, state } => {
                if epoch < self.state.epoch {
                    return Vec::new();
                }
                self.adopt_epoch(epoch);
                self.state.connection = state;
                if matches!(
                    state,
                    ConnectionState::Disconnected
                        | ConnectionState::Offline
                        | ConnectionState::Recovering
                ) {
                    self.reset_external_reconciliation(epoch);
                    self.state.device_job_session = None;
                    self.state.device_assets.invalidate(epoch);
                    self.invalidate_pending("The connection ended while requests were pending");
                }
            }
            DeviceEvent::SessionChanged {
                epoch,
                identity,
                capabilities,
            } => {
                if epoch < self.state.epoch {
                    return Vec::new();
                }
                self.adopt_epoch(epoch);
                self.state.identity = Some(identity);
                self.state.capabilities = capabilities;
                self.state.device_job_session = None;
            }
            DeviceEvent::DeviceJobSessionObserved(session) => {
                if session.epoch() == self.state.epoch
                    && self.state.identity.as_ref().is_some_and(|identity| {
                        session.matches_capabilities(identity, &self.state.capabilities)
                    })
                {
                    self.state.device_job_session = Some(session);
                }
            }
            DeviceEvent::SnapshotReceived {
                stamp,
                target,
                snapshot,
            } => return self.snapshot(stamp, target, snapshot),
            DeviceEvent::GlobalsReceived { stamp, globals } => {
                if stamp.epoch != self.state.epoch {
                    return Vec::new();
                }
                self.state.globals = Some(globals);
                if !matches!(
                    self.requests.get(&stamp.intent),
                    Some(PendingRequest::Edit {
                        dependent: Some(_),
                        ..
                    })
                ) {
                    self.requests.remove(&stamp.intent);
                }
            }
            DeviceEvent::GlobalStateReceived { stamp, state } => {
                if stamp.epoch != self.state.epoch || state.epoch != stamp.epoch {
                    return Vec::new();
                }
                let Some(request) = self.requests.get(&stamp.intent).cloned() else {
                    return Vec::new();
                };
                if self
                    .state
                    .device_globals
                    .as_ref()
                    .is_some_and(|current| state.revision <= current.revision)
                {
                    return Vec::new();
                }
                match request {
                    PendingRequest::ReadDeviceGlobals => {
                        self.state.device_globals = Some(state);
                        self.requests.remove(&stamp.intent);
                        self.external_refresh.finished(stamp, true);
                        if !self.firmware_editor_blocks_normal_actions() {
                            self.deferred.remove(&device_document_id());
                        }
                        if !self.state.documents.get(&device_document_id()).is_some_and(
                            |document| {
                                document.pending.values().any(|pending| {
                                    matches!(
                                        pending.stage,
                                        EditStage::Sent
                                            | EditStage::AwaitingReadback
                                            | EditStage::Indeterminate(_)
                                    )
                                })
                            },
                        ) {
                            self.blocked_after_uncertainty = false;
                        }
                        return self.resume_history_continuation();
                    }
                    PendingRequest::Edit {
                        document,
                        dependent: Some(dependent),
                        verified,
                        ..
                    } => {
                        let postflight = self
                            .state
                            .documents
                            .get(&document)
                            .and_then(|document| document.pending.get(&stamp.intent))
                            .is_some_and(|pending| {
                                matches!(
                                    pending.stage,
                                    EditStage::Sent | EditStage::AwaitingReadback
                                )
                            });
                        let matches = postflight
                            && dependent_globals_match(
                                dependent.before_globals.records().raw(),
                                state.records().raw(),
                                Some(1),
                            )
                            .unwrap_or(false);
                        self.state.device_globals = Some(state);
                        if matches {
                            if let Some(PendingRequest::Edit {
                                dependent: Some(dependent),
                                ..
                            }) = self.requests.get_mut(&stamp.intent)
                            {
                                dependent.saw_globals = true;
                            }
                            if verified {
                                return self.progress(stamp, EditStage::Verified);
                            }
                        }
                    }
                    PendingRequest::GlobalControl {
                        before,
                        patch,
                        verified,
                        ..
                    } => {
                        if state.revision <= before.revision
                            || !before.verifies_expected(&patch, &state).unwrap_or(false)
                        {
                            self.fail_request(stamp.intent, ServiceError::new(ErrorKind::Indeterminate, "Global settings readback differs from the exact expected change; refresh before another edit"));
                            return Vec::new();
                        }
                        self.state.device_globals = Some(state);
                        if let Some(PendingRequest::GlobalControl { saw_readback, .. }) =
                            self.requests.get_mut(&stamp.intent)
                        {
                            *saw_readback = true;
                        }
                        if verified {
                            return self.finish_global_control(stamp.intent);
                        }
                    }
                    _ => {}
                }
            }
            DeviceEvent::AssetMetadataReceived { stamp, metadata } => {
                if stamp.epoch != self.state.epoch || metadata.stamp() != stamp {
                    return Vec::new();
                }
                let source_matches = self.state.identity.as_ref().is_some_and(|identity| {
                    match (identity.mode, metadata.source()) {
                        (BackendMode::Hardware, SnapshotSource::Hardware { epoch }) => {
                            *epoch == stamp.epoch
                        }
                        (BackendMode::Simulator, SnapshotSource::Simulator) => true,
                        (BackendMode::CaptureReplay, SnapshotSource::Capture { .. }) => true,
                        _ => false,
                    }
                });
                if !source_matches {
                    return Vec::new();
                }
                if !matches!(self.requests.get(&stamp.intent), Some(PendingRequest::ReadAssetMetadata(slot)) if *slot == metadata.slot())
                {
                    return Vec::new();
                }
                if self.state.device_assets.complete(metadata) {
                    self.requests.remove(&stamp.intent);
                    let documents: Vec<_> = self.deferred.keys().cloned().collect();
                    return documents
                        .into_iter()
                        .flat_map(|document| self.flush(&document))
                        .collect();
                }
            }
            DeviceEvent::EditProgress { stamp, stage } => return self.progress(stamp, stage),
            DeviceEvent::OperationFailed { stamp, error } => {
                if stamp.epoch != self.state.epoch {
                    return Vec::new();
                }
                self.fail_request(stamp.intent, error);
            }
            DeviceEvent::IncomingNotification { epoch, raw } => {
                if epoch != self.state.epoch {
                    return Vec::new();
                }
                self.state.diagnostic(DiagnosticEvent {
                    category: "notification".into(),
                    message:
                        "Incoming device data retained; invalidation hints require complete reads"
                            .into(),
                    bytes: Some(raw.payload().to_vec()),
                    epoch: Some(epoch),
                });
                if matches!(
                    self.state.connection,
                    ConnectionState::Connecting
                        | ConnectionState::Identifying
                        | ConnectionState::Synchronizing
                        | ConnectionState::Ready
                ) && self
                    .external_refresh
                    .note(epoch, &raw, self.state.monotonic_ms)
                    && !self.firmware_editor_blocks_normal_actions()
                {
                    self.deferred.remove(&device_document_id());
                }
            }
            DeviceEvent::JobProgress {
                epoch,
                job_id,
                progress,
            } => {
                if epoch == self.state.epoch {
                    let complete = progress
                        .total
                        .is_some_and(|total| progress.completed >= total);
                    self.state.jobs.insert(job_id, progress);
                    if complete {
                        self.requests.retain(|_, request| !matches!(request, PendingRequest::DeviceJob(id) if *id == job_id));
                    }
                }
            }
            DeviceEvent::Diagnostic(diagnostic) => self.state.diagnostic(diagnostic),
        }
        Vec::new()
    }
    fn snapshot(
        &mut self,
        stamp: RequestStamp,
        target: PresetTarget,
        snapshot: Arc<PresetSnapshot>,
    ) -> Vec<Effect> {
        if stamp.epoch != self.state.epoch
            || snapshot.target.is_some_and(|actual| actual != target)
            || matches!(snapshot.source, SnapshotSource::Hardware { epoch } if epoch != self.state.epoch)
        {
            return Vec::new();
        }
        // A stored read's context can never replace a live edit-buffer context.
        if snapshot.context.is_some_and(|context| match target {
            PresetTarget::EditBuffer => context.group != 0,
            PresetTarget::Stored(slot) => context.group != 1 || context.selector != slot.get(),
        }) {
            self.state.error(ServiceError::new(
                ErrorKind::Protocol,
                "Snapshot context does not match its target",
            ));
            return Vec::new();
        }
        if let Some(latest) = self.latest_reads.get(&target)
            && stamp.intent < *latest
            && matches!(
                self.requests.get(&stamp.intent),
                Some(PendingRequest::Read(_) | PendingRequest::Preview(_))
            )
        {
            self.requests.remove(&stamp.intent);
            return Vec::new();
        }
        let request = self.requests.get(&stamp.intent).cloned();
        if request.is_none() && self.connect_intent != Some(stamp.intent) {
            return Vec::new();
        }
        let expected_target = match &request {
            Some(PendingRequest::Read(target)) => Some(*target),
            Some(PendingRequest::Preview(slot)) => Some(PresetTarget::Stored(*slot)),
            Some(PendingRequest::Save { slot, .. }) => match target {
                PresetTarget::Stored(actual) if actual == *slot => Some(target),
                PresetTarget::EditBuffer => Some(target),
                _ => return Vec::new(),
            },
            Some(PendingRequest::WriteStored { slot, .. }) => Some(PresetTarget::Stored(*slot)),
            Some(
                PendingRequest::Connect
                | PendingRequest::Select { .. }
                | PendingRequest::Edit { .. },
            ) => Some(PresetTarget::EditBuffer),
            _ => None,
        };
        if expected_target.is_some_and(|expected| expected != target) {
            return Vec::new();
        }
        if target == PresetTarget::EditBuffer
            && matches!(
                &request,
                Some(PendingRequest::Read(PresetTarget::EditBuffer))
            )
            && self.factory_history_read_is_stale(&snapshot)
        {
            return Vec::new();
        }
        if target == PresetTarget::EditBuffer
            && self
                .state
                .confirmed_buffer
                .as_ref()
                .is_some_and(|current| snapshot.revision < current.revision)
        {
            return Vec::new();
        }
        if target == PresetTarget::EditBuffer
            && self.reconcile_external_buffer(stamp, snapshot.clone())
        {
            return self.resume_history_continuation();
        }
        match target {
            PresetTarget::Stored(slot) => {
                self.state.bank.slots.insert(
                    slot,
                    CachedSlot {
                        snapshot: snapshot.clone(),
                        fresh: true,
                    },
                );
                match request {
                    Some(PendingRequest::WriteStored {
                        source, verified, ..
                    }) => {
                        if source.as_ref().is_some_and(|source| {
                            !equivalent(
                                &PresetDraft::from_snapshot(source),
                                &PresetDraft::from_snapshot(&snapshot),
                            )
                        }) {
                            self.fail_request(
                                stamp.intent,
                                ServiceError::new(
                                    ErrorKind::Conflict,
                                    "Stored preset readback differs from the reviewed source",
                                ),
                            );
                        } else {
                            if let Some(PendingRequest::WriteStored { saw_readback, .. }) =
                                self.requests.get_mut(&stamp.intent)
                            {
                                *saw_readback = true;
                            }
                            if verified {
                                self.finish_stored_write(stamp.intent);
                            }
                        }
                    }
                    Some(PendingRequest::Preview(_)) => {
                        self.state.preview_slot = Some(slot);
                        if self.firmware_editor_blocks_normal_actions() {
                            self.retained_document_openings
                                .insert(stamp.intent, snapshot);
                        } else {
                            self.open_offline(snapshot);
                        }
                        self.requests.remove(&stamp.intent);
                    }
                    Some(PendingRequest::Save {
                        buffer, verified, ..
                    }) => {
                        if !equivalent(
                            &PresetDraft::from_snapshot(&buffer),
                            &PresetDraft::from_snapshot(&snapshot),
                        ) {
                            self.fail_request(stamp.intent, ServiceError::new(ErrorKind::Conflict, "Stored readback differs from the verified buffer; save was not confirmed"));
                        } else {
                            if let Some(PendingRequest::Save { saw_readback, .. }) =
                                self.requests.get_mut(&stamp.intent)
                            {
                                *saw_readback = true;
                            }
                            if verified {
                                self.finish_save(stamp.intent);
                            }
                        }
                    }
                    _ => {
                        self.requests.remove(&stamp.intent);
                    }
                }
            }
            PresetTarget::EditBuffer => {
                if let Some(confirmed) = &self.state.confirmed_buffer
                    && snapshot.revision < confirmed.revision
                {
                    return Vec::new();
                }
                let previous = self.state.confirmed_buffer.replace(snapshot.clone());
                let id = device_document_id();
                if let Some(PendingRequest::Save {
                    slot,
                    saw_readback,
                    verified,
                    ..
                }) = request.clone()
                {
                    if snapshot
                        .context
                        .is_none_or(|context| context.group != 0 || context.selector != slot.get())
                    {
                        self.state.confirmed_buffer = previous;
                        self.fail_request(
                            stamp.intent,
                            ServiceError::new(
                                ErrorKind::Protocol,
                                "Save readback did not establish the new edit-buffer context",
                            ),
                        );
                        return Vec::new();
                    }
                    if let Some(document) = self.state.documents.get_mut(&id) {
                        let annotations = document.draft.metadata.clone();
                        document.draft = PresetDraft::from_snapshot(&snapshot);
                        document.draft.metadata.author = annotations.author;
                        document.draft.metadata.style = annotations.style;
                        document.draft.metadata.notes = annotations.notes;
                        document.draft.metadata.pan = annotations.pan;
                        document.draft.view.metadata = document.draft.metadata.clone();
                        document.local_revision.0 += 1;
                    }
                    if let Some(PendingRequest::Save { current_buffer, .. }) =
                        self.requests.get_mut(&stamp.intent)
                    {
                        *current_buffer = Some(snapshot);
                    }
                    if saw_readback && verified {
                        self.finish_save(stamp.intent);
                    }
                    return Vec::new();
                } else if matches!(request, Some(PendingRequest::Select { .. })) {
                    self.state.documents.insert(
                        id.clone(),
                        EditorDocument::new(id.clone(), EditTarget::DeviceBuffer, snapshot.clone()),
                    );
                    self.state.buffer_persistence = BufferPersistence::Unknown;
                    if let Some(PendingRequest::Select { slot, verified, .. }) = request {
                        if snapshot.context
                            != Some(WireContext {
                                group: 0,
                                selector: slot.get(),
                            })
                        {
                            self.fail_request(stamp.intent, ServiceError::new(ErrorKind::Conflict, "Pedal selection readback did not confirm the requested slot. Actual buffer was retained"));
                        } else {
                            if let Some(PendingRequest::Select { saw_readback, .. }) =
                                self.requests.get_mut(&stamp.intent)
                            {
                                *saw_readback = true;
                            }
                            if verified {
                                self.finish_selection(stamp.intent);
                            }
                        }
                    }
                } else if let Some(document) = self.state.documents.get_mut(&id) {
                    if let Some(previous) = previous {
                        let acknowledged = match &request {
                            Some(PendingRequest::Edit {
                                patch, dependent, ..
                            }) if patch_matches(patch, &snapshot)
                                && dependent.as_ref().is_none_or(|pending| {
                                    pending.plan.readback_matches(&snapshot)
                                })
                                && (dependent.is_none()
                                    || document.pending.get(&stamp.intent).is_some_and(
                                        |pending| {
                                            matches!(
                                                pending.stage,
                                                EditStage::Sent | EditStage::AwaitingReadback
                                            )
                                        },
                                    )) =>
                            {
                                Some(patch)
                            }
                            _ => None,
                        };
                        let fields = merge_readback(
                            &mut document.draft,
                            &previous,
                            &snapshot,
                            acknowledged,
                            &self.catalog,
                            match &request {
                                Some(PendingRequest::Edit {
                                    dependent: Some(dependent),
                                    ..
                                }) if acknowledged.is_some() => Some(&dependent.plan),
                                _ => None,
                            },
                        );
                        if !fields.is_empty() {
                            let description = fields
                                .iter()
                                .map(|field| field.description())
                                .collect::<Vec<_>>()
                                .join("; ");
                            self.conflict_fields
                                .insert((id.clone(), stamp.intent), fields);
                            document
                                .conflicts
                                .retain(|conflict| conflict.intent != stamp.intent);
                            document.conflicts.push(EditConflict {
                                intent: stamp.intent,
                                description,
                                actual: snapshot.clone(),
                            });
                        }
                        if !equivalent(
                            &PresetDraft::from_snapshot(&previous),
                            &PresetDraft::from_snapshot(&snapshot),
                        ) && !matches!(
                            self.state.buffer_persistence,
                            BufferPersistence::Saving { .. }
                        ) {
                            self.state.buffer_persistence = BufferPersistence::Unsaved;
                        }
                    }
                } else {
                    self.state.documents.insert(
                        id.clone(),
                        EditorDocument::new(id.clone(), EditTarget::DeviceBuffer, snapshot.clone()),
                    );
                }
                if self.state.active_document.is_none() {
                    self.state.active_document = Some(id.clone());
                }
                if self.connect_intent == Some(stamp.intent) {
                    self.connect_intent = None;
                }
                match request {
                    Some(PendingRequest::Edit {
                        patch,
                        verified,
                        dependent,
                        ..
                    }) => {
                        if dependent.is_some()
                            && self
                                .state
                                .documents
                                .get(&id)
                                .and_then(|document| document.pending.get(&stamp.intent))
                                .is_some_and(|pending| matches!(pending.stage, EditStage::Queued))
                        {
                            // A failed preflight refresh precedes the actor's
                            // definite no-write failure, never target readback.
                            return Vec::new();
                        }
                        if dependent.as_ref().is_some_and(|pending| {
                            !patch_matches(&patch, &snapshot)
                                || !pending.plan.readback_matches(&snapshot)
                        }) {
                            // Retain the pending physical authority until the
                            // actor has read globals and reports uncertainty.
                            // Early local Conflict would lose its failure and
                            // allow unsent controls to escape the failed batch.
                            return Vec::new();
                        }
                        if patch_matches(&patch, &snapshot) {
                            if let Some(PendingRequest::Edit { saw_readback, .. }) =
                                self.requests.get_mut(&stamp.intent)
                            {
                                *saw_readback = true;
                            }
                            if verified
                                && dependent
                                    .as_ref()
                                    .is_none_or(|dependent| dependent.saw_globals)
                            {
                                if let Some(pending) = self
                                    .state
                                    .documents
                                    .get_mut(&id)
                                    .and_then(|doc| doc.pending.get_mut(&stamp.intent))
                                {
                                    pending.stage = EditStage::Verified;
                                }
                                self.requests.remove(&stamp.intent);
                                if self.history_step_verified(stamp) {
                                    return self.resume_history_continuation();
                                }
                                if matches!(patch, EditPatch::SetModel { .. }) {
                                    self.queue_delta(&id);
                                }
                                return self.flush(&id);
                            }
                        } else {
                            self.fail_request(stamp.intent, ServiceError::new(ErrorKind::Conflict, "The affected field did not match the requested value on readback"));
                        }
                    }
                    Some(PendingRequest::Read(_)) => {
                        self.requests.remove(&stamp.intent);
                        self.blocked_after_uncertainty = false;
                        // Explicit refresh reconciles uncertainty; never replay the failed edit.
                    }
                    Some(PendingRequest::Connect) => {
                        self.requests.remove(&stamp.intent);
                    }
                    _ => {}
                }
            }
        }
        Vec::new()
    }
    fn progress(&mut self, stamp: RequestStamp, stage: EditStage) -> Vec<Effect> {
        if stamp.epoch != self.state.epoch {
            return Vec::new();
        }
        let Some(request) = self.requests.get(&stamp.intent).cloned() else {
            return Vec::new();
        };
        match request {
            PendingRequest::GlobalControl { saw_readback, .. } => match stage {
                EditStage::Verified => {
                    if let Some(PendingRequest::GlobalControl { verified, .. }) =
                        self.requests.get_mut(&stamp.intent)
                    {
                        *verified = true;
                    }
                    if saw_readback {
                        return self.finish_global_control(stamp.intent);
                    }
                }
                EditStage::Failed(error) => self.fail_request(stamp.intent, error),
                EditStage::Indeterminate(mut error) => {
                    error.kind = ErrorKind::Indeterminate;
                    self.fail_request(stamp.intent, error);
                }
                _ => {}
            },
            PendingRequest::Select { saw_readback, .. } => match stage {
                EditStage::Verified => {
                    if let Some(PendingRequest::Select { verified, .. }) =
                        self.requests.get_mut(&stamp.intent)
                    {
                        *verified = true;
                    }
                    if saw_readback {
                        self.finish_selection(stamp.intent);
                    }
                }
                EditStage::Failed(error) | EditStage::Indeterminate(error) => {
                    self.fail_request(stamp.intent, error)
                }
                _ => {}
            },
            PendingRequest::WriteStored { saw_readback, .. } => match stage {
                EditStage::Verified => {
                    if let Some(PendingRequest::WriteStored { verified, .. }) =
                        self.requests.get_mut(&stamp.intent)
                    {
                        *verified = true;
                    }
                    if saw_readback {
                        self.finish_stored_write(stamp.intent);
                    }
                }
                EditStage::Failed(error) | EditStage::Indeterminate(error) => {
                    self.fail_request(stamp.intent, error)
                }
                _ => {}
            },
            PendingRequest::Edit {
                document,
                patch,
                saw_readback,
                dependent,
                ..
            } => {
                let complete = saw_readback
                    && dependent
                        .as_ref()
                        .is_none_or(|dependent| dependent.saw_globals);
                if matches!(stage, EditStage::Verified)
                    && let Some(PendingRequest::Edit { verified, .. }) =
                        self.requests.get_mut(&stamp.intent)
                {
                    *verified = true;
                }
                if let Some(pending) = self
                    .state
                    .documents
                    .get_mut(&document)
                    .and_then(|doc| doc.pending.get_mut(&stamp.intent))
                {
                    pending.stage = if matches!(stage, EditStage::Verified) && !complete {
                        EditStage::AwaitingReadback
                    } else {
                        stage.clone()
                    };
                }
                match stage {
                    EditStage::Verified if complete => {
                        self.requests.remove(&stamp.intent);
                        if self.history_step_verified(stamp) {
                            return self.resume_history_continuation();
                        }
                        if matches!(patch, EditPatch::SetModel { .. }) {
                            self.queue_delta(&document);
                        }
                        return self.flush(&document);
                    }
                    EditStage::Failed(error) => self.fail_request(stamp.intent, error),
                    EditStage::Indeterminate(mut error) => {
                        error.kind = ErrorKind::Indeterminate;
                        self.fail_request(stamp.intent, error);
                    }
                    _ => {}
                }
            }
            PendingRequest::Save { saw_readback, .. } => match stage {
                EditStage::Verified => {
                    if let Some(PendingRequest::Save { verified, .. }) =
                        self.requests.get_mut(&stamp.intent)
                    {
                        *verified = true;
                    }
                    if saw_readback {
                        self.finish_save(stamp.intent);
                    }
                }
                EditStage::Failed(error) => self.fail_request(stamp.intent, error),
                EditStage::Indeterminate(mut error) => {
                    error.kind = ErrorKind::Indeterminate;
                    self.fail_request(stamp.intent, error);
                }
                _ => {}
            },
            PendingRequest::DeviceJob(id) if matches!(stage, EditStage::Verified) => {
                self.requests.remove(&stamp.intent);
                self.state.prepared_jobs.remove(&id);
            }
            _ => {}
        }
        Vec::new()
    }
    fn finish_global_control(&mut self, intent: IntentId) -> Vec<Effect> {
        if let Some(PendingRequest::GlobalControl {
            saw_readback: true,
            verified: true,
            ..
        }) = self.requests.remove(&intent)
        {
            self.state.message(
                MessageSeverity::Info,
                "Global setting changed and verified on the pedal",
                None,
            );
        }
        let documents: Vec<_> = self.deferred.keys().cloned().collect();
        documents
            .into_iter()
            .flat_map(|document| self.flush(&document))
            .collect()
    }
    fn finish_selection(&mut self, intent: IntentId) {
        if let Some(PendingRequest::Select {
            slot,
            saw_readback: true,
            verified: true,
        }) = self.requests.remove(&intent)
        {
            if let Some(snapshot) = &self.state.confirmed_buffer {
                self.state.buffer_persistence = BufferPersistence::Selected {
                    slot,
                    revision: snapshot.revision,
                    stored_matches_buffer: self.state.bank.slots.get(&slot).map(|cached| {
                        equivalent(
                            &PresetDraft::from_snapshot(&cached.snapshot),
                            &PresetDraft::from_snapshot(snapshot),
                        )
                    }),
                };
            }
            self.state.message(
                MessageSeverity::Info,
                format!("Loaded pedal slot {slot}; current readback verified"),
                None,
            );
        }
    }
    fn finish_save(&mut self, intent: IntentId) {
        if !matches!(
            self.requests.get(&intent),
            Some(PendingRequest::Save {
                saw_readback: true,
                verified: true,
                current_buffer: Some(_),
                ..
            })
        ) {
            return;
        }
        if let Some(PendingRequest::Save {
            slot,
            current_buffer: Some(buffer),
            saw_readback: true,
            verified: true,
            ..
        }) = self.requests.remove(&intent)
        {
            let stored_matches = self.state.bank.slots.get(&slot).is_some_and(|cached| {
                equivalent(
                    &PresetDraft::from_snapshot(&cached.snapshot),
                    &PresetDraft::from_snapshot(&buffer),
                )
            });
            self.state.buffer_persistence = if stored_matches {
                BufferPersistence::Saved {
                    slot,
                    revision: buffer.revision,
                }
            } else {
                BufferPersistence::Unsaved
            };
            if let Some(document) = self.state.documents.get_mut(&device_document_id()) {
                document.baseline = buffer; // History survives a save; unsaved baseline is advanced only here.
            }
            self.state.message(
                MessageSeverity::Info,
                format!("Saved and verified preset slot {}", slot.get()),
                None,
            );
        }
    }
    fn finish_stored_write(&mut self, intent: IntentId) {
        if let Some(PendingRequest::WriteStored {
            slot,
            saw_readback: true,
            verified: true,
            ..
        }) = self.requests.remove(&intent)
        {
            self.state.message(
                MessageSeverity::Info,
                format!("Wrote and verified stored preset slot {slot}"),
                None,
            );
        }
    }
    fn fail_request(&mut self, intent: IntentId, mut error: ServiceError) {
        if !self.firmware_editor_blocks_normal_actions()
            && self
                .history_continuation
                .as_ref()
                .is_some_and(|continuation| {
                    continuation.owned == Some(intent)
                        || matches!(
                            self.requests.get(&intent),
                            Some(
                                PendingRequest::Read(PresetTarget::EditBuffer)
                                    | PendingRequest::ReadDeviceGlobals
                            )
                        )
                })
        {
            self.history_continuation = None;
            self.deferred.remove(&device_document_id());
        }
        self.external_refresh.finished(
            RequestStamp {
                epoch: self.state.epoch,
                intent,
            },
            false,
        );
        let request = self.requests.remove(&intent);
        if let Some(PendingRequest::Edit { document, .. }) = &request
            && matches!(
                error.kind,
                ErrorKind::Timeout | ErrorKind::Io | ErrorKind::Disconnected
            )
            && self
                .state
                .documents
                .get(document)
                .and_then(|doc| doc.pending.get(&intent))
                .is_some_and(|pending| {
                    matches!(pending.stage, EditStage::Sent | EditStage::AwaitingReadback)
                })
        {
            error.kind = ErrorKind::Indeterminate;
        }
        if matches!(
            &request,
            Some(PendingRequest::Save { .. } | PendingRequest::WriteStored { .. })
                | Some(PendingRequest::GlobalControl { .. })
        ) && matches!(
            error.kind,
            ErrorKind::Timeout | ErrorKind::Io | ErrorKind::Disconnected
        ) {
            // Save has no per-preset pending stage DTO. A transport failure after
            // submission cannot establish whether persistent memory was written.
            error.kind = ErrorKind::Indeterminate;
        }
        let uncertain = matches!(error.kind, ErrorKind::Indeterminate);
        if let Some(PendingRequest::Edit { document, .. }) = request {
            if let Some(pending) = self
                .state
                .documents
                .get_mut(&document)
                .and_then(|doc| doc.pending.get_mut(&intent))
            {
                pending.stage = if uncertain {
                    EditStage::Indeterminate(error.clone())
                } else {
                    EditStage::Failed(error.clone())
                };
            }
            // Conflicting values need a user rebase; even a harmless unsent failure
            // must not loop forever resending the same invalid command.
            if !self.firmware_editor_blocks_normal_actions() {
                self.deferred.remove(&document);
            }
            self.blocked_after_uncertainty = uncertain;
        } else if let Some(PendingRequest::WriteStored { slot, .. }) = request {
            if let Some(cached) = self.state.bank.slots.get_mut(&slot) {
                cached.fresh = false;
            }
            self.blocked_after_uncertainty = uncertain;
        } else if let Some(PendingRequest::Save { slot, .. }) = request {
            self.state.buffer_persistence = if uncertain {
                BufferPersistence::Indeterminate {
                    slot,
                    reason: error.message.clone(),
                }
            } else {
                BufferPersistence::Unsaved
            };
            self.blocked_after_uncertainty = uncertain;
        } else if matches!(request, Some(PendingRequest::GlobalControl { .. })) {
            self.blocked_after_uncertainty = uncertain;
            if uncertain {
                self.state.device_globals = None;
            }
        } else if let Some(PendingRequest::ReadAssetMetadata(slot)) = request {
            self.state.device_assets.fail(
                slot,
                RequestStamp {
                    epoch: self.state.epoch,
                    intent,
                },
                error.clone(),
            );
        }
        self.state.error(error);
    }
    fn resolve(&mut self, resolution: ConflictResolution) -> Vec<Effect> {
        if !self.admit_normal_mutation() {
            return Vec::new();
        }
        if let Some(effects) = self.resolve_external_change(&resolution) {
            return effects;
        }
        let intent = match resolution {
            ConflictResolution::AcceptDevice(intent) | ConflictResolution::KeepLocal(intent) => {
                intent
            }
        };
        let id = self
            .state
            .documents
            .iter()
            .find(|(_, document)| {
                document
                    .conflicts
                    .iter()
                    .any(|conflict| conflict.intent == intent)
            })
            .map(|(id, _)| id.clone());
        let Some(id) = id else {
            return Vec::new();
        };
        let actual = self
            .state
            .documents
            .get(&id)
            .and_then(|document| {
                document
                    .conflicts
                    .iter()
                    .find(|conflict| conflict.intent == intent)
            })
            .map(|conflict| conflict.actual.clone());
        if let Some(document) = self.state.documents.get_mut(&id) {
            document
                .conflicts
                .retain(|conflict| conflict.intent != intent);
        }
        match resolution {
            ConflictResolution::AcceptDevice(_) => {
                if let Some(actual) = actual
                    && let Some(fields) = self.conflict_fields.remove(&(id.clone(), intent))
                {
                    let mut draft = self
                        .state
                        .documents
                        .get(&id)
                        .expect("document exists")
                        .draft
                        .clone();
                    for field in fields {
                        field.accept(&mut draft, &actual);
                    }
                    return self.replace_draft(&id, draft);
                }
            }
            ConflictResolution::KeepLocal(_) => {
                self.conflict_fields.remove(&(id.clone(), intent));
                self.queue_delta(&id);
                return self.flush(&id);
            }
        }
        Vec::new()
    }
    fn library(&mut self, event: LibraryEvent) -> Vec<Effect> {
        self.settle_library_effect(&event);
        match event {
            LibraryEvent::EntryChanged { library, .. } => self.state.message(
                MessageSeverity::Info,
                format!("Library entry updated: {library}"),
                None,
            ),
            LibraryEvent::QueryResult { entries, .. } => self.state.library.entries = entries,
            LibraryEvent::VersionLoaded { intent, version } => {
                let requested = self.loading_versions.remove(&intent);
                self.state
                    .library
                    .versions
                    .insert(version.id.clone(), version.clone());
                if requested.as_ref() == Some(&version.id) {
                    if self.firmware_editor_blocks_normal_actions() {
                        self.retained_document_openings
                            .insert(intent, version.snapshot);
                    } else {
                        self.open_offline(version.snapshot);
                    }
                }
            }
            LibraryEvent::VersionStored { version, .. } => self.state.message(
                MessageSeverity::Info,
                format!("Library revision saved: {version}"),
                None,
            ),
            LibraryEvent::VersionsListed { versions, .. } => {
                for version in versions {
                    self.state
                        .library
                        .versions
                        .insert(version.id.clone(), version);
                }
            }
            LibraryEvent::Imported { report, .. } => {
                self.state.library.last_conversion = Some(report)
            }
            LibraryEvent::Exported { path, report, .. } => {
                self.state.library.last_export = Some(path);
                self.state.library.last_conversion = Some(report);
            }
            LibraryEvent::JobProgress { job_id, progress } => {
                self.state.jobs.insert(job_id, progress);
            }
            LibraryEvent::Failed { intent, error } => {
                self.loading_versions.remove(&intent);
                self.state.error(error);
            }
        }
        Vec::new()
    }
    fn assets(&mut self, event: AssetEvent) -> Vec<Effect> {
        self.settle_asset_effect(&event);
        match event {
            AssetEvent::Inventory(assets) => self.state.assets = assets,
            AssetEvent::Inspected { job, info } | AssetEvent::Prepared { job, info } => {
                self.state.assets.retain(|existing| existing.id != info.id);
                self.state.assets.push(info);
                self.state.jobs.remove(&job);
            }
            AssetEvent::DeviceJobPrepared(job) => {
                self.state.message(
                    MessageSeverity::Info,
                    "Device payload prepared; review it before applying",
                    None,
                );
                self.state.prepared_jobs.insert(job.id(), job);
            }
            AssetEvent::JobProgress { job_id, progress } => {
                self.state.jobs.insert(job_id, progress);
            }
            AssetEvent::Failed { job, error } => {
                self.state.jobs.remove(&job);
                self.state.error(error);
            }
        }
        Vec::new()
    }
}
