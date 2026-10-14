//! Lossless selected merges, confined to local documents. A retained immutable
//! source and exact target draft/baseline are hashed before review and rechecked
//! on Apply. This module has no service, filesystem, or transport handle.
use crate::{PresetDiff, compare_presets};
use gp200_catalog::Catalog;
use gp200_domain::*;
use gp200_protocol::CtrlAssignmentRecord;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{collections::BTreeSet, sync::Arc};

const MAX_MERGE_FIELDS: usize = 256;
const MAX_MERGE_PAYLOAD: usize = 1024 * 1024;

#[derive(Debug, Clone)]
pub struct OfflineMergeReview {
    request: OfflineMergeRequest,
    result: PresetDraft,
    before: Arc<PresetSnapshot>,
    after: Arc<PresetSnapshot>,
    diff: PresetDiff,
}
impl OfflineMergeReview {
    pub fn request(&self) -> OfflineMergeRequest {
        self.request.clone()
    }
    pub fn before(&self) -> &PresetSnapshot {
        &self.before
    }
    pub fn after(&self) -> &PresetSnapshot {
        &self.after
    }
    pub fn diff(&self) -> &PresetDiff {
        &self.diff
    }
    pub fn fields(&self) -> &[OfflineMergeField] {
        &self.request.fields
    }
    pub fn source_sha256(&self) -> [u8; 32] {
        self.request.source_sha256
    }
    pub fn target_sha256(&self) -> [u8; 32] {
        self.request.target_sha256
    }
}

fn hash<T: Serialize>(label: &[u8], value: &T) -> Result<[u8; 32], ServiceError> {
    let encoded = serde_json::to_vec(value).map_err(|error| {
        ServiceError::invalid(format!("Merge snapshot encoding failed: {error}"))
    })?;
    let mut digest = Sha256::new();
    digest.update(label);
    digest.update((encoded.len() as u64).to_le_bytes());
    digest.update(encoded);
    Ok(digest.finalize().into())
}

fn eligible(document: &EditorDocument) -> Result<(), ServiceError> {
    if !matches!(&document.target, EditTarget::Offline(id) if id == &document.id) {
        return Err(ServiceError::new(
            ErrorKind::Unsupported,
            "Selected merge requires an offline document; open a local copy first",
        ));
    }
    if !document.pending.is_empty()
        || !document.conflicts.is_empty()
        || document.undo.gesture.is_some()
    {
        return Err(ServiceError::new(
            ErrorKind::Busy,
            "Finish the edit gesture and resolve pending edits or conflicts before merge review",
        ));
    }
    if document.baseline.raw.payload().len() > MAX_MERGE_PAYLOAD {
        return Err(ServiceError::invalid(
            "Merge target exceeds the 1 MiB retained payload bound",
        ));
    }
    Ok(())
}

/// Exact draft plus retained baseline, variant and target. Provenance and opaque
/// bytes are included; an equal local revision alone cannot authorize a merge.
pub fn offline_merge_target_sha256(document: &EditorDocument) -> Result<[u8; 32], ServiceError> {
    eligible(document)?;
    hash(
        b"GP200-OFFLINE-MERGE-TARGET-1",
        &(
            &document.id,
            &document.target,
            document.local_revision,
            &document.baseline,
            &document.draft,
        ),
    )
}

pub fn review_offline_merge(
    document: &EditorDocument,
    source: Arc<PresetSnapshot>,
    fields: Vec<OfflineMergeField>,
    catalog: &Catalog,
) -> Result<OfflineMergeReview, ServiceError> {
    let target_sha256 = offline_merge_target_sha256(document)?;
    if source.raw.payload().len() > MAX_MERGE_PAYLOAD {
        return Err(ServiceError::invalid(
            "Merge source exceeds the 1 MiB retained payload bound",
        ));
    }
    if source.device_model != document.baseline.device_model {
        return Err(ServiceError::new(
            ErrorKind::Unsupported,
            "Merge source and target device variants differ",
        ));
    }
    let selected: BTreeSet<_> = fields.iter().cloned().collect();
    if fields.is_empty() || fields.len() > MAX_MERGE_FIELDS || selected.len() != fields.len() {
        return Err(ServiceError::invalid("Select 1..256 distinct merge fields"));
    }
    let source_sha256 = hash(b"GP200-OFFLINE-MERGE-SOURCE-1", &source)?;
    let source_draft = PresetDraft::from_snapshot(&source);
    let mut next = document.draft.clone();
    // Entire modules go first, so assignment validation observes the resulting
    // model set regardless of checkbox order. No catalog defaults are applied.
    let mut changed_models = BTreeSet::new();
    for field in &fields {
        if let OfflineMergeField::Module(id) = field {
            let from = unique_module(&source_draft.view.modules, *id)?;
            let to = unique_module_mut(&mut next.view.modules, *id)?;
            if catalog.model_by_id(id.get(), from.model.0).is_none()
                || from.parameters.len() != to.parameters.len()
                || from.slot_count_raw != to.slot_count_raw
            {
                return Err(ServiceError::invalid(
                    "Selected module has an unknown model or incompatible retained slot layout",
                ));
            }
            if from.model != to.model {
                changed_models.insert(*id);
            }
            validate_asset_identity(&source, &document.baseline, *id, from.model, catalog)?;
            to.state_raw = from.state_raw;
            to.model = from.model;
            to.parameters.clone_from(&from.parameters);
        }
    }
    for field in &fields {
        match field {
            OfflineMergeField::Module(_) => {}
            OfflineMergeField::Enabled(id) => {
                if selected.contains(&OfflineMergeField::Module(*id)) {
                    return Err(overlap());
                }
                unique_module_mut(&mut next.view.modules, *id)?.state_raw =
                    unique_module(&source_draft.view.modules, *id)?.state_raw;
            }
            OfflineMergeField::Parameter {
                module,
                expected_model,
                parameter,
            } => {
                if selected.contains(&OfflineMergeField::Module(*module)) {
                    return Err(overlap());
                }
                let from = unique_module(&source_draft.view.modules, *module)?;
                let to = unique_module_mut(&mut next.view.modules, *module)?;
                if from.model != *expected_model
                    || to.model != *expected_model
                    || !declared_parameter(catalog, *module, *expected_model, *parameter)
                {
                    return Err(ServiceError::new(
                        ErrorKind::Conflict,
                        "Individual parameter merge requires the same reviewed model and a declared parameter",
                    ));
                }
                validate_asset_identity(&source, &document.baseline, *module, from.model, catalog)?;
                let value = from
                    .parameter(*parameter)
                    .ok_or_else(|| ServiceError::invalid("Source retained parameter is absent"))?;
                let target = to
                    .parameters
                    .get_mut(usize::from(parameter.get()))
                    .ok_or_else(|| ServiceError::invalid("Target retained parameter is absent"))?;
                *target = value;
            }
            OfflineMergeField::Metadata(field) => copy_metadata(&mut next, &source_draft, *field),
            OfflineMergeField::Chain => {
                next.view.chain = source_draft.view.chain.clone();
            }
            OfflineMergeField::Expression(slot) => {
                let from = unique(&source_draft.view.patch_assignments.expressions, |item| {
                    item.record.slot == *slot
                })?;
                validate_reference_asset(
                    &source,
                    &document.baseline,
                    from.record.module_raw,
                    catalog,
                )?;
                check_reference(
                    from.record.module_raw,
                    from.record.parameter_raw,
                    false,
                    &source_draft,
                    &next,
                    catalog,
                )?;
                let to = unique_mut(&mut next.view.patch_assignments.expressions, |item| {
                    item.record.slot == *slot
                })?;
                to.record = from.record.clone();
            }
            OfflineMergeField::Quick(index) => {
                let from = unique(&source_draft.view.patch_assignments.quicks, |item| {
                    item.record.index == *index
                })?;
                validate_reference_asset(
                    &source,
                    &document.baseline,
                    from.record.module_raw,
                    catalog,
                )?;
                check_reference(
                    from.record.module_raw,
                    from.record.parameter_raw,
                    true,
                    &source_draft,
                    &next,
                    catalog,
                )?;
                let to = unique_mut(&mut next.view.patch_assignments.quicks, |item| {
                    item.record.index == *index
                })?;
                to.record = from.record.clone();
            }
            OfflineMergeField::Ctrl(slot) => {
                let from = unique(&source_draft.view.patch_assignments.ctrls, |item| {
                    item.record.slot == *slot
                })?;
                if from.record.state_raw > 1 {
                    return Err(ServiceError::invalid("Selected CTRL state is unrecognized"));
                }
                let to = unique_mut(&mut next.view.patch_assignments.ctrls, |item| {
                    item.record.slot == *slot
                })?;
                to.record.state_raw = from.record.state_raw;
                to.record.module_mask_raw = (to.record.module_mask_raw
                    & !CtrlAssignmentRecord::MODULE_MASK)
                    | (from.record.module_mask_raw & CtrlAssignmentRecord::MODULE_MASK);
            }
            OfflineMergeField::FxLoopPositions => {
                let from = source_draft
                    .view
                    .patch_assignments
                    .routing
                    .as_ref()
                    .ok_or_else(|| {
                        ServiceError::invalid("Source FX routing is absent or ambiguous")
                    })?;
                if from.record.send_boundary > from.record.return_boundary
                    || from.record.return_boundary > 11
                {
                    return Err(ServiceError::invalid(
                        "Selected FX routing boundaries are unrecognized",
                    ));
                }
                let to = next
                    .view
                    .patch_assignments
                    .routing
                    .as_mut()
                    .ok_or_else(|| {
                        ServiceError::invalid("Target FX routing is absent or ambiguous")
                    })?;
                to.record = from.record.clone();
            }
            OfflineMergeField::LegacyExpressions => {
                for target in source_draft
                    .expressions
                    .iter()
                    .flat_map(|entry| &entry.targets)
                {
                    check_legacy_target(target, &source_draft, &next, catalog)?;
                }
                next.expressions.clone_from(&source_draft.expressions);
                next.view
                    .expressions
                    .clone_from(&source_draft.view.expressions);
            }
            OfflineMergeField::LegacyFootswitches => {
                next.footswitches.clone_from(&source_draft.footswitches);
                next.view
                    .footswitches
                    .clone_from(&source_draft.view.footswitches);
            }
            OfflineMergeField::LegacyQuickControls => {
                for target in source_draft
                    .quick_controls
                    .iter()
                    .filter_map(|entry| entry.target.as_ref())
                {
                    check_legacy_target(target, &source_draft, &next, catalog)?;
                }
                next.quick_controls.clone_from(&source_draft.quick_controls);
                next.view
                    .quick_controls
                    .clone_from(&source_draft.view.quick_controls);
            }
        }
    }
    check_unselected_dependencies(document, &selected, &changed_models)?;
    if next.semantically_equal(&document.draft) {
        return Err(ServiceError::invalid(
            "Selected fields contain no local setting changes",
        ));
    }
    let before = Arc::new(document.draft.snapshot(
        &document.baseline,
        document.baseline.source.clone(),
        document.local_revision,
    )?);
    let after = Arc::new(
        next.snapshot(
            &document.baseline,
            document.baseline.source.clone(),
            Revision(
                document
                    .local_revision
                    .0
                    .checked_add(1)
                    .ok_or_else(|| ServiceError::invalid("Local revision is exhausted"))?,
            ),
        )?,
    );
    let result_sha256 = hash(b"GP200-OFFLINE-MERGE-RESULT-1", &next)?;
    let review_sha256 = hash(
        b"GP200-OFFLINE-MERGE-REVIEW-1",
        &(
            &document.id,
            document.local_revision,
            target_sha256,
            source_sha256,
            &fields,
            result_sha256,
        ),
    )?;
    let diff = compare_presets(&before, &after, catalog);
    Ok(OfflineMergeReview {
        request: OfflineMergeRequest {
            document: document.id.clone(),
            expected_revision: document.local_revision,
            target_sha256,
            source,
            source_sha256,
            fields,
            result_sha256,
            review_sha256,
        },
        result: next,
        before,
        after,
        diff,
    })
}

/// Recompute the entire reviewed result. A stale source, draft, baseline,
/// revision, selection, or result is rejected before any document mutation.
pub fn apply_reviewed_offline_merge(
    document: &EditorDocument,
    request: &OfflineMergeRequest,
    catalog: &Catalog,
) -> Result<PresetDraft, ServiceError> {
    if document.id != request.document
        || document.local_revision != request.expected_revision
        || offline_merge_target_sha256(document)? != request.target_sha256
    {
        return Err(ServiceError::new(
            ErrorKind::Conflict,
            "Merge target changed since review; select and review the changes again",
        ));
    }
    let review = review_offline_merge(
        document,
        request.source.clone(),
        request.fields.clone(),
        catalog,
    )?;
    if review.request.source_sha256 != request.source_sha256
        || review.request.result_sha256 != request.result_sha256
        || review.request.review_sha256 != request.review_sha256
    {
        return Err(ServiceError::new(
            ErrorKind::Conflict,
            "Merge source or selected result changed since review; create a new review",
        ));
    }
    Ok(review.result)
}

fn overlap() -> ServiceError {
    ServiceError::invalid("Select either the full module or its individual fields")
}
fn unique<T>(items: &[T], test: impl Fn(&T) -> bool) -> Result<&T, ServiceError> {
    let mut matches = items.iter().filter(|item| test(item));
    let result = matches
        .next()
        .ok_or_else(|| ServiceError::invalid("Selected retained field is absent"))?;
    if matches.next().is_some() {
        return Err(ServiceError::invalid(
            "Selected retained field is ambiguous",
        ));
    }
    Ok(result)
}
fn unique_mut<T>(items: &mut [T], test: impl Fn(&T) -> bool) -> Result<&mut T, ServiceError> {
    let mut matches = items.iter_mut().filter(|item| test(item));
    let result = matches
        .next()
        .ok_or_else(|| ServiceError::invalid("Selected retained field is absent"))?;
    if matches.next().is_some() {
        return Err(ServiceError::invalid(
            "Selected retained field is ambiguous",
        ));
    }
    Ok(result)
}
fn unique_module(
    modules: &[ModuleSnapshot],
    id: ModuleId,
) -> Result<&ModuleSnapshot, ServiceError> {
    unique(modules, |module| module.module == id)
}
fn unique_module_mut(
    modules: &mut [ModuleSnapshot],
    id: ModuleId,
) -> Result<&mut ModuleSnapshot, ServiceError> {
    unique_mut(modules, |module| module.module == id)
}
fn declared_parameter(
    catalog: &Catalog,
    module: ModuleId,
    model: ModelCode,
    parameter: ParamId,
) -> bool {
    catalog
        .model_by_id(module.get(), model.0)
        .is_some_and(|definition| {
            definition
                .parameters
                .iter()
                .any(|item| item.parameter_id == parameter.get())
        })
}
fn copy_metadata(to: &mut PresetDraft, from: &PresetDraft, field: MergeMetadataField) {
    match field {
        MergeMetadataField::Name => {
            to.view.name = from.view.name.clone();
            to.metadata.name.clone_from(&from.metadata.name);
        }
        MergeMetadataField::Tempo => {
            to.view.tempo = from.view.tempo.clone();
            to.metadata.tempo = from.metadata.tempo;
        }
        MergeMetadataField::Level => {
            to.view.level = from.view.level.clone();
            to.metadata.level = from.metadata.level;
        }
        MergeMetadataField::Author => to.metadata.author.clone_from(&from.metadata.author),
        MergeMetadataField::Style => to.metadata.style.clone_from(&from.metadata.style),
        MergeMetadataField::Notes => to.metadata.notes.clone_from(&from.metadata.notes),
        MergeMetadataField::PanAnnotation => to.metadata.pan = from.metadata.pan,
        MergeMetadataField::NativePan => to.metadata.native_pan = from.metadata.native_pan,
        MergeMetadataField::NativeStyle => to.metadata.native_style = from.metadata.native_style,
        MergeMetadataField::FxSendLevel => to.metadata.fx_send_level = from.metadata.fx_send_level,
        MergeMetadataField::FxReturnLevel => {
            to.metadata.fx_return_level = from.metadata.fx_return_level
        }
        MergeMetadataField::FxLoopMode => to.metadata.fx_loop_mode = from.metadata.fx_loop_mode,
    }
    // Only the selected field's mirror changes. Legacy retained projections in
    // unrelated fields must not be normalized by a merge side effect.
    macro_rules! mirror {
        ($field:ident) => {
            to.view.metadata.$field.clone_from(&to.metadata.$field)
        };
    }
    match field {
        MergeMetadataField::Name => mirror!(name),
        MergeMetadataField::Tempo => mirror!(tempo),
        MergeMetadataField::Level => mirror!(level),
        MergeMetadataField::Author => mirror!(author),
        MergeMetadataField::Style => mirror!(style),
        MergeMetadataField::Notes => mirror!(notes),
        MergeMetadataField::PanAnnotation => mirror!(pan),
        MergeMetadataField::NativePan => mirror!(native_pan),
        MergeMetadataField::NativeStyle => mirror!(native_style),
        MergeMetadataField::FxSendLevel => mirror!(fx_send_level),
        MergeMetadataField::FxReturnLevel => mirror!(fx_return_level),
        MergeMetadataField::FxLoopMode => mirror!(fx_loop_mode),
    }
}
fn check_reference(
    module_raw: u8,
    parameter_raw: u16,
    quick: bool,
    source: &PresetDraft,
    next: &PresetDraft,
    catalog: &Catalog,
) -> Result<(), ServiceError> {
    if module_raw == 0xff || (quick && matches!(module_raw, 11 | 12)) {
        return Ok(());
    }
    let module = ModuleId::new(module_raw).map_err(|_| {
        ServiceError::invalid("Selected assignment has an unresolved module reference")
    })?;
    let parameter = u8::try_from(parameter_raw)
        .ok()
        .and_then(|id| ParamId::new(id).ok())
        .ok_or_else(|| {
            ServiceError::invalid("Selected assignment has an unresolved parameter reference")
        })?;
    let from = unique_module(&source.view.modules, module)?;
    let to = unique_module(&next.view.modules, module)?;
    if from.model != to.model
        || to.parameter(parameter).is_none()
        || !declared_parameter(catalog, module, from.model, parameter)
    {
        return Err(ServiceError::new(
            ErrorKind::Conflict,
            "Selected assignment depends on a different model; include its complete module in the review",
        ));
    }
    Ok(())
}
fn check_legacy_target(
    target: &AssignmentTarget,
    source: &PresetDraft,
    next: &PresetDraft,
    catalog: &Catalog,
) -> Result<(), ServiceError> {
    if unique_module(&source.view.modules, target.module)?.model != target.model {
        return Err(ServiceError::invalid(
            "Selected local assignment has a stale model reference",
        ));
    }
    check_reference(
        target.module.get(),
        u16::from(target.parameter.get()),
        false,
        source,
        next,
        catalog,
    )
}
fn check_unselected_dependencies(
    document: &EditorDocument,
    selected: &BTreeSet<OfflineMergeField>,
    changed: &BTreeSet<ModuleId>,
) -> Result<(), ServiceError> {
    if !changed.is_empty()
        && document
            .draft
            .view
            .patch_assignments
            .diagnostics
            .iter()
            .any(|diagnostic| matches!(diagnostic.command, 12 | 16))
    {
        return Err(ServiceError::new(
            ErrorKind::Conflict,
            "Opaque or ambiguous assignment records prevent validating this model change; merge same-model fields instead",
        ));
    }
    for module in changed {
        let assignments = &document.draft.view.patch_assignments;
        if assignments.expressions.iter().any(|item| {
            item.record.module_raw == module.get()
                && !selected.contains(&OfflineMergeField::Expression(item.record.slot))
        }) || assignments.quicks.iter().any(|item| {
            item.record.module_raw == module.get()
                && !selected.contains(&OfflineMergeField::Quick(item.record.index))
        }) || (!selected.contains(&OfflineMergeField::LegacyExpressions)
            && document
                .draft
                .expressions
                .iter()
                .flat_map(|entry| &entry.targets)
                .any(|target| target.module == *module))
            || (!selected.contains(&OfflineMergeField::LegacyQuickControls)
                && document
                    .draft
                    .quick_controls
                    .iter()
                    .filter_map(|entry| entry.target.as_ref())
                    .any(|target| target.module == *module))
        {
            return Err(ServiceError::new(
                ErrorKind::Conflict,
                "Model change would reinterpret an unselected assignment; include every affected assignment in the review",
            ));
        }
    }
    Ok(())
}
fn validate_reference_asset(
    source: &PresetSnapshot,
    target: &PresetSnapshot,
    module_raw: u8,
    catalog: &Catalog,
) -> Result<(), ServiceError> {
    if let Ok(module) = ModuleId::new(module_raw) {
        let model = unique_module(&source.view.modules, module)?.model;
        validate_asset_identity(source, target, module, model, catalog)?;
    }
    Ok(())
}
fn validate_asset_identity(
    source: &PresetSnapshot,
    target: &PresetSnapshot,
    module: ModuleId,
    model: ModelCode,
    catalog: &Catalog,
) -> Result<(), ServiceError> {
    let slot = match catalog
        .model_by_id(module.get(), model.0)
        .map(|model| model.slot_kind())
    {
        Some(gp200_catalog::SlotKind::UserIr { slot }) => Some((AssetKind::ImpulseResponse, slot)),
        Some(gp200_catalog::SlotKind::SnapTone { slot }) => Some((AssetKind::SnapTone, slot)),
        _ => None,
    };
    if let Some((kind, slot)) = slot {
        let content = |snapshot: &PresetSnapshot| {
            snapshot
                .asset_refs
                .iter()
                .find_map(|reference| match reference {
                    AssetRef::UserSlot {
                        kind: k,
                        slot: s,
                        content,
                    } if *k == kind && *s == slot => content.clone(),
                    _ => None,
                })
        };
        if content(source).is_some() && content(source) != content(target) {
            return Err(ServiceError::new(
                ErrorKind::Unsupported,
                "Selected module needs a different known asset content; selected merge cannot replace assets",
            ));
        }
    }
    Ok(())
}
