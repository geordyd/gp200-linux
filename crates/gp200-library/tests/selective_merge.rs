use gp200_catalog::Catalog;
use gp200_domain::*;
use gp200_library::*;
use gp200_protocol::CtrlAssignmentRecord;
use std::sync::Arc;

fn id(module: u8) -> ModuleId {
    ModuleId::new(module).unwrap()
}
fn capture() -> Arc<PresetSnapshot> {
    let mut raw = RawTransfer::from_payload(
        0x12,
        include_bytes!("../../../assets/demo/read-current-payload.bin").to_vec(),
    )
    .unwrap()
    .records()
    .to_vec();
    raw.push(RawRecord {
        command: 0xbeef,
        body: vec![0, 255, 5, 1],
    });
    raw.push(RawRecord {
        command: 0xbeef,
        body: vec![3],
    });
    from_records(&raw)
}
fn from_records(records: &[RawRecord]) -> Arc<PresetSnapshot> {
    Arc::new(PresetSnapshot::from_raw(
        Arc::new(RawTransfer::from_records(0x12, records).unwrap()),
        SnapshotSource::Capture {
            capture_id: "retained-merge-source".into(),
        },
        None,
        Revision(7),
    ))
}
fn document(source: Arc<PresetSnapshot>) -> EditorDocument {
    let document = DocumentId::from("offline-merge-test");
    EditorDocument::new(document.clone(), EditTarget::Offline(document), source)
}
fn edit_raw(
    source: &PresetSnapshot,
    edit: impl FnOnce(&mut Vec<RawRecord>),
) -> Arc<PresetSnapshot> {
    let mut records = source.raw.records().to_vec();
    edit(&mut records);
    from_records(&records)
}
fn module(records: &mut [RawRecord], value: u8) -> &mut RawRecord {
    records
        .iter_mut()
        .find(|record| record.command == 20 && record.body[0] == value)
        .unwrap()
}
fn assignment(records: &mut [RawRecord], command: u16, slot: u8) -> &mut RawRecord {
    records
        .iter_mut()
        .find(|record| record.command == command && record.body[0] == slot)
        .unwrap()
}
fn parameter(source: &PresetSnapshot, module_id: u8, slot: u8, bits: u32) -> Arc<PresetSnapshot> {
    edit_raw(source, |records| {
        let start = 8 + usize::from(slot) * 4;
        module(records, module_id).body[start..start + 4].copy_from_slice(&bits.to_le_bytes());
    })
}
fn field(module: u8, model: u32, parameter: u8) -> OfflineMergeField {
    OfflineMergeField::Parameter {
        module: id(module),
        expected_model: ModelCode(model),
        parameter: ParamId::new(parameter).unwrap(),
    }
}
fn review(
    document: &EditorDocument,
    source: Arc<PresetSnapshot>,
    fields: Vec<OfflineMergeField>,
) -> Result<OfflineMergeReview, ServiceError> {
    review_offline_merge(document, source, fields, &Catalog::embedded().unwrap())
}

#[test]
fn single_parameter_keeps_exact_nan_payload_and_every_unselected_byte_and_annotation() {
    let mut target = document(capture());
    target.draft.metadata.notes = Some("Keep my stage notes".into());
    target.draft.view.metadata.notes = target.draft.metadata.notes.clone();
    let source = parameter(&target.baseline, 3, 0, 0x7fc0_1234);
    let source = edit_raw(&source, |records| {
        records.last_mut().unwrap().body = vec![99, 12];
        module(records, 3).body[64..68].copy_from_slice(&0x8000_0000u32.to_le_bytes());
    });
    let immutable_source = (*source).clone();
    let prepared = review(&target, source.clone(), vec![field(3, 0x07000015, 0)]).unwrap();
    let draft =
        apply_reviewed_offline_merge(&target, &prepared.request(), &Catalog::embedded().unwrap())
            .unwrap();
    let actual = draft
        .snapshot(
            &target.baseline,
            target.baseline.source.clone(),
            Revision(8),
        )
        .unwrap();
    let expected = parameter(&target.baseline, 3, 0, 0x7fc0_1234);
    assert_eq!(actual.raw, expected.raw);
    assert_eq!(draft.metadata.notes, target.draft.metadata.notes);
    assert_eq!(
        draft.view.module(id(3)).unwrap().parameters[14],
        target.draft.view.module(id(3)).unwrap().parameters[14]
    );
    assert_eq!(*source, immutable_source);
    assert_eq!(prepared.before().raw, target.baseline.raw);
    assert_eq!(prepared.after().raw, expected.raw);
}

#[test]
fn complete_module_copies_model_and_all_retained_floats_without_defaults_or_source_record_positions()
 {
    let target = document(capture());
    let source = edit_raw(&target.baseline, |records| {
        let record = module(records, 4);
        record.body[4..8].copy_from_slice(&29u32.to_le_bytes());
        record.body[1] = 1;
        for (index, bytes) in record.body[8..]
            .as_chunks_mut::<4>()
            .0
            .iter_mut()
            .enumerate()
        {
            bytes.copy_from_slice(&(0x7fc0_0000u32 + index as u32).to_le_bytes());
        }
        let a = records
            .iter()
            .position(|record| record.command == 20 && record.body[0] == 0)
            .unwrap();
        let b = records
            .iter()
            .position(|record| record.command == 20 && record.body[0] == 4)
            .unwrap();
        records.swap(a, b);
    });
    let prepared = review(
        &target,
        source.clone(),
        vec![OfflineMergeField::Module(id(4))],
    )
    .unwrap();
    let after = prepared.after();
    let incoming = source.view.module(id(4)).unwrap();
    let result = after.view.module(id(4)).unwrap();
    assert_eq!(result.model, ModelCode(29));
    assert_eq!(result.parameters, incoming.parameters);
    assert_eq!(
        result.raw_record_index,
        target.draft.view.module(id(4)).unwrap().raw_record_index
    );
    let expected = edit_raw(&target.baseline, |records| {
        let destination = module(records, 4);
        let raw_source = source
            .raw
            .records()
            .iter()
            .find(|record| record.command == 20 && record.body[0] == 4)
            .unwrap();
        destination.body[1] = raw_source.body[1];
        destination.body[4..].copy_from_slice(&raw_source.body[4..]);
    });
    assert_eq!(after.raw, expected.raw);
}

#[test]
fn model_changes_require_all_existing_dependent_assignments_and_preserve_raw_endpoint_bits() {
    let baseline = edit_raw(&capture(), |records| {
        let exp = assignment(records, 12, 0x10);
        exp.body[1] = 4;
        exp.body[2..4].copy_from_slice(&0u16.to_le_bytes());
        let quick = assignment(records, 16, 0);
        quick.body[1] = 4;
        quick.body[2..4].copy_from_slice(&0u16.to_le_bytes());
    });
    let target = document(baseline.clone());
    let source = edit_raw(&baseline, |records| {
        module(records, 4).body[4..8].copy_from_slice(&29u32.to_le_bytes());
        let exp = assignment(records, 12, 0x10);
        exp.body[4..8].copy_from_slice(&0x7fc0_0444u32.to_le_bytes());
        exp.body[8..12].copy_from_slice(&0x8000_0000u32.to_le_bytes());
    });
    let module = OfflineMergeField::Module(id(4));
    let expression = OfflineMergeField::Expression(ExpressionSlotId::from_packed(0x10).unwrap());
    let quick = OfflineMergeField::Quick(0);
    assert_eq!(
        review(&target, source.clone(), vec![module.clone()])
            .unwrap_err()
            .kind,
        ErrorKind::Conflict
    );
    assert_eq!(
        review(
            &target,
            source.clone(),
            vec![module.clone(), expression.clone()]
        )
        .unwrap_err()
        .kind,
        ErrorKind::Conflict
    );
    let prepared = review(&target, source.clone(), vec![quick, expression, module]).unwrap();
    let expected = edit_raw(&baseline, |records| {
        module_record_copy(records, &source, 4);
        assignment(records, 12, 0x10).body.clone_from(
            &source
                .raw
                .records()
                .iter()
                .find(|record| record.command == 12 && record.body[0] == 0x10)
                .unwrap()
                .body,
        );
    });
    assert_eq!(prepared.after().raw, expected.raw);
}
fn module_record_copy(records: &mut [RawRecord], source: &PresetSnapshot, module_id: u8) {
    let from = source
        .raw
        .records()
        .iter()
        .find(|record| record.command == 20 && record.body[0] == module_id)
        .unwrap();
    module(records, module_id).body.clone_from(&from.body);
}

#[test]
fn incoming_assignment_cannot_reinterpret_source_parameter_on_another_target_model() {
    let target = document(capture());
    let source = edit_raw(&target.baseline, |records| {
        module(records, 4).body[4..8].copy_from_slice(&29u32.to_le_bytes());
        let quick = assignment(records, 16, 2);
        quick.body[1] = 4;
        quick.body[2..4].copy_from_slice(&1u16.to_le_bytes());
    });
    assert_eq!(
        review(&target, source.clone(), vec![OfflineMergeField::Quick(2)])
            .unwrap_err()
            .kind,
        ErrorKind::Conflict
    );
    assert!(
        review(
            &target,
            source,
            vec![
                OfflineMergeField::Quick(2),
                OfflineMergeField::Module(id(4))
            ]
        )
        .is_ok()
    );
}

#[test]
fn ctrl_merge_preserves_target_padding_and_unknown_high_mask_bits() {
    let baseline = edit_raw(&capture(), |records| {
        let ctrl = assignment(records, 15, 0);
        ctrl.body[2..4].copy_from_slice(&[0x99, 0x77]);
        ctrl.body[4..8].copy_from_slice(&0xa000_0001u32.to_le_bytes());
    });
    let target = document(baseline.clone());
    let source = edit_raw(&baseline, |records| {
        let ctrl = assignment(records, 15, 0);
        ctrl.body[1] = 1;
        ctrl.body[2..4].copy_from_slice(&[0, 0]);
        ctrl.body[4..8].copy_from_slice(&0xb000_0030u32.to_le_bytes());
    });
    let prepared = review(&target, source, vec![OfflineMergeField::Ctrl(0)]).unwrap();
    let record = prepared
        .after()
        .view
        .patch_assignments
        .ctrls
        .iter()
        .find(|item| item.record.slot == 0)
        .unwrap();
    assert_eq!(record.record.retained_padding, [0x99, 0x77]);
    assert_eq!(record.record.module_mask_raw, 0xa000_0030);
    assert_eq!(
        record.record.module_mask_raw & CtrlAssignmentRecord::MODULE_MASK,
        0x30
    );
    let expected = edit_raw(&baseline, |records| {
        let ctrl = assignment(records, 15, 0);
        ctrl.body[1] = 1;
        ctrl.body[4..8].copy_from_slice(&0xa000_0030u32.to_le_bytes());
    });
    assert_eq!(prepared.after().raw, expected.raw);
}

#[test]
fn chain_merge_keeps_fx_positions_context_and_opaque_chain_tail() {
    let target = document(capture());
    let source = edit_raw(&target.baseline, |records| {
        let context = records
            .iter_mut()
            .find(|record| record.command == 0)
            .unwrap();
        context.body = vec![1, 0, 255, 0];
        let chain = records
            .iter_mut()
            .find(|record| record.command == 8)
            .unwrap();
        chain.body[4..15].reverse();
        chain.body[2] = 1;
        chain.body[3] = 10;
        chain.body[15] ^= 0xff;
    });
    let prepared = review(&target, source, vec![OfflineMergeField::Chain]).unwrap();
    let expected = edit_raw(&target.baseline, |records| {
        records
            .iter_mut()
            .find(|record| record.command == 8)
            .unwrap()
            .body[4..15]
            .reverse();
    });
    assert_eq!(prepared.after().raw, expected.raw);
    assert_eq!(prepared.after().context, target.baseline.context);
}

#[test]
fn metadata_annotations_can_be_cleared_independently() {
    let mut target = document(capture());
    target.draft.metadata.author = Some("Old author".into());
    target.draft.metadata.notes = Some("Keep notes".into());
    target.draft.view.metadata = target.draft.metadata.clone();
    let prepared = review(
        &target,
        capture(),
        vec![OfflineMergeField::Metadata(MergeMetadataField::Author)],
    )
    .unwrap();
    assert_eq!(prepared.after().view.metadata.author, None);
    assert_eq!(
        prepared.after().view.metadata.notes,
        Some("Keep notes".into())
    );
    assert_eq!(prepared.before().raw, prepared.after().raw);
}

#[test]
fn duplicate_ambiguous_assignment_blocks_only_its_selected_field_and_is_never_dropped() {
    let source = edit_raw(&capture(), |records| {
        let duplicate = assignment(records, 16, 0).clone();
        records.push(duplicate);
    });
    let target = document(capture());
    assert!(review(&target, source.clone(), vec![OfflineMergeField::Quick(0)]).is_err());
    let source = parameter(&source, 3, 0, 0x8000_0000);
    let prepared = review(&target, source, vec![field(3, 0x07000015, 0)]).unwrap();
    assert_eq!(
        prepared.after().raw.records().len(),
        target.baseline.raw.records().len()
    );
}

#[test]
fn unknown_float_slots_are_retained_but_not_offered_as_individual_catalog_parameters() {
    let target = document(capture());
    let source = parameter(&target.baseline, 4, 14, 0x7fc0_9876);
    assert!(review(&target, source.clone(), vec![field(4, 27, 14)]).is_err());
    let prepared = review(&target, source, vec![OfflineMergeField::Module(id(4))]).unwrap();
    assert_eq!(
        prepared.after().view.module(id(4)).unwrap().parameters[14],
        F32Bits(0x7fc0_9876)
    );
}

#[test]
fn changed_source_result_selection_and_same_revision_draft_or_baseline_all_invalidate_review() {
    let target = document(capture());
    let source = parameter(&target.baseline, 3, 0, 0x8000_0000);
    let prepared = review(&target, source, vec![field(3, 0x07000015, 0)]).unwrap();
    let catalog = Catalog::embedded().unwrap();
    let mut changed = prepared.request();
    Arc::make_mut(&mut changed.source).revision.0 += 1;
    assert!(apply_reviewed_offline_merge(&target, &changed, &catalog).is_err());
    let mut changed = prepared.request();
    changed.result_sha256[0] ^= 1;
    assert!(apply_reviewed_offline_merge(&target, &changed, &catalog).is_err());
    let mut changed = prepared.request();
    changed.fields = vec![OfflineMergeField::Module(id(3))];
    // This broader module currently has the same resulting settings; the exact
    // selections must nevertheless remain bound to the reviewed request.
    assert!(apply_reviewed_offline_merge(&target, &changed, &catalog).is_err());
    let mut changed_target = target.clone();
    changed_target.draft.metadata.notes = Some("Changed during review".into());
    assert!(apply_reviewed_offline_merge(&changed_target, &prepared.request(), &catalog).is_err());
    let mut changed_target = target.clone();
    changed_target.baseline = edit_raw(&target.baseline, |records| {
        records.last_mut().unwrap().body[0] ^= 1
    });
    assert!(apply_reviewed_offline_merge(&changed_target, &prepared.request(), &catalog).is_err());
}

#[test]
fn duplicate_overlap_empty_noop_and_different_variant_are_rejected_without_mutation() {
    let target = document(capture());
    let before = target.draft.clone();
    let source = parameter(&target.baseline, 3, 0, 0x8000_0000);
    assert!(review(&target, source.clone(), vec![]).is_err());
    let item = field(3, 0x07000015, 0);
    assert!(review(&target, source.clone(), vec![item.clone(), item.clone()]).is_err());
    assert!(
        review(
            &target,
            source.clone(),
            vec![item, OfflineMergeField::Module(id(3))]
        )
        .is_err()
    );
    assert!(review(&target, capture(), vec![OfflineMergeField::Module(id(3))]).is_err());
    let mut source = (*source).clone();
    source.device_model = DeviceModel::Gp200Lt;
    assert!(
        review(
            &target,
            Arc::new(source),
            vec![OfflineMergeField::Module(id(3))]
        )
        .is_err()
    );
    assert_eq!(target.draft, before);
}

#[test]
fn device_document_and_unsealed_gesture_are_never_eligible() {
    let mut target = document(capture());
    let source = parameter(&target.baseline, 3, 0, 0x8000_0000);
    target.target = EditTarget::DeviceBuffer;
    assert_eq!(
        review(&target, source.clone(), vec![field(3, 0x07000015, 0)])
            .unwrap_err()
            .kind,
        ErrorKind::Unsupported
    );
    target.target = EditTarget::Offline(target.id.clone());
    target.begin_gesture(GestureKey {
        module: id(3),
        model: ModelCode(0x07000015),
        parameter: ParamId::new(0).unwrap(),
    });
    assert_eq!(
        review(&target, source, vec![field(3, 0x07000015, 0)])
            .unwrap_err()
            .kind,
        ErrorKind::Busy
    );
}

#[test]
fn selected_user_module_rejects_a_different_known_asset_content() {
    let baseline = edit_raw(&capture(), |records| {
        module(records, 5).body[4..8].copy_from_slice(&0x0a10_0000u32.to_le_bytes());
    });
    let mut baseline = (*baseline).clone();
    baseline.asset_refs.iter_mut().for_each(|reference| {
        if let AssetRef::UserSlot {
            kind: AssetKind::ImpulseResponse,
            slot: 0,
            content,
        } = reference
        {
            *content = Some(AssetId::from("sha256:target-asset"));
        }
    });
    let target = document(Arc::new(baseline));
    let mut source = (*parameter(&target.baseline, 5, 0, 0x8000_0000)).clone();
    source.asset_refs.iter_mut().for_each(|reference| {
        if let AssetRef::UserSlot {
            kind: AssetKind::ImpulseResponse,
            slot: 0,
            content,
        } = reference
        {
            *content = Some(AssetId::from("sha256:different-source-asset"));
        }
    });
    assert_eq!(
        review(
            &target,
            Arc::new(source),
            vec![OfflineMergeField::Module(id(5))]
        )
        .unwrap_err()
        .kind,
        ErrorKind::Unsupported
    );
}

#[test]
fn malformed_assignment_dependency_prevents_a_model_change_but_not_a_same_model_parameter_merge() {
    let baseline = edit_raw(&capture(), |records| {
        records.push(RawRecord {
            command: 12,
            body: vec![0x10, 4, 0, 0, 1],
        })
    });
    let target = document(baseline.clone());
    let changed_model = edit_raw(&baseline, |records| {
        module(records, 4).body[4..8].copy_from_slice(&29u32.to_le_bytes())
    });
    assert!(
        review(
            &target,
            changed_model,
            vec![OfflineMergeField::Module(id(4))]
        )
        .is_err()
    );
    let source = parameter(&baseline, 4, 0, 0x8000_0000);
    let prepared = review(&target, source, vec![field(4, 27, 0)]).unwrap();
    assert_eq!(
        prepared.after().raw.records().last(),
        target.baseline.raw.records().last()
    );
}

#[test]
fn an_unselected_retained_projection_is_not_normalized_as_a_side_effect() {
    let mut target = document(capture());
    // Older retained local views may carry descriptive mirrors independently.
    // Copying one parameter must not rewrite an unrelated field.
    target.draft.view.metadata.notes = Some("Retained descriptive mirror".into());
    target.draft.metadata.notes = Some("Current annotation".into());
    let source = parameter(&target.baseline, 3, 0, 0x8000_0000);
    let prepared = review(&target, source, vec![field(3, 0x07000015, 0)]).unwrap();
    let actual =
        apply_reviewed_offline_merge(&target, &prepared.request(), &Catalog::embedded().unwrap())
            .unwrap();
    assert_eq!(actual.view.metadata.notes, target.draft.view.metadata.notes);
    assert_eq!(actual.metadata.notes, target.draft.metadata.notes);
}
