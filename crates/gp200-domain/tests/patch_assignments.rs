use gp200_domain::*;
use std::sync::Arc;

fn captured() -> PresetSnapshot {
    let raw = RawTransfer::from_payload(
        0x11,
        include_bytes!("../../../tests/fixtures/patch-assignments/current.payload.bin").to_vec(),
    )
    .unwrap();
    PresetSnapshot::from_raw(
        Arc::new(raw),
        SnapshotSource::Capture {
            capture_id: "root-current".into(),
        },
        Some(PresetTarget::EditBuffer),
        Revision(1),
    )
}
fn draft_edit(snapshot: &PresetSnapshot, edit: AssignmentEdit) -> PresetSnapshot {
    let mut draft = PresetDraft::from_snapshot(snapshot);
    draft.apply(&EditPatch::SetPatchAssignment(edit)).unwrap();
    draft
        .snapshot(snapshot, SnapshotSource::Simulator, Revision(2))
        .unwrap()
}
fn differences(before: &PresetSnapshot, after: &PresetSnapshot) -> Vec<(usize, usize)> {
    before
        .raw
        .records()
        .iter()
        .zip(after.raw.records())
        .enumerate()
        .flat_map(|(index, (a, b))| {
            assert_eq!(a.command, b.command);
            assert_eq!(a.body.len(), b.body.len());
            a.body
                .iter()
                .zip(&b.body)
                .enumerate()
                .filter_map(move |(offset, (a, b))| (a != b).then_some((index, offset)))
        })
        .collect()
}

#[test]
fn captured_sparse_slots_and_special_quick_reference_project_without_legacy_coercion() {
    let s = captured();
    let p = &s.view.patch_assignments;
    assert_eq!(
        (p.expressions.len(), p.ctrls.len(), p.quicks.len()),
        (9, 8, 3)
    );
    assert!(p.diagnostics.is_empty());
    assert_eq!(
        p.expressions
            .iter()
            .map(|x| x.record.slot.packed())
            .collect::<Vec<_>>(),
        [0, 1, 2, 16, 17, 18, 32, 33, 34]
    );
    assert_eq!(p.expressions[1].record.module_raw, 255);
    assert_eq!(p.expressions[1].record.maximum, F32Bits::from_f32(100.0));
    assert_eq!(
        PatchAssignments::resolve_quick(&p.quicks[2].record, &s.view.modules),
        ResolvedAssignmentReference::PatchVolume
    );
    assert_eq!(p.quicks[2].record.parameter_raw, 0);
    assert!(s.view.expressions.is_empty());
    assert!(s.view.footswitches.is_empty());
    assert!(s.view.quick_controls.is_empty());
    let draft = PresetDraft::from_snapshot(&s);
    assert_eq!(
        draft
            .snapshot(&s, SnapshotSource::Simulator, Revision(2))
            .unwrap()
            .raw,
        s.raw
    );
}

#[test]
fn explicit_expression_clear_normalizes_only_the_selected_record() {
    let s = captured();
    let slot = ExpressionSlotId::new(1, 0).unwrap();
    let index = s.view.patch_assignments.expressions[3].raw_record_index;
    let next = draft_edit(
        &s,
        AssignmentEdit::ExpressionTarget {
            slot,
            target: ExpressionAssignmentTarget::Off,
        },
    );
    let record = &next.raw.records()[index];
    assert_eq!(record.body, [16, 255, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
    assert!(
        differences(&s, &next)
            .iter()
            .all(|&(i, off)| i == index && off > 0)
    );
    let mut draft = PresetDraft::from_snapshot(&s);
    let before = draft.clone();
    assert!(
        draft
            .apply(&EditPatch::SetPatchAssignment(
                AssignmentEdit::ExpressionTargetAndRange {
                    slot,
                    target: ExpressionAssignmentTarget::Off,
                    minimum: 0.0.into(),
                    maximum: 100.0.into(),
                }
            ))
            .is_err()
    );
    assert_eq!(draft, before);
    // Stale native OFF endpoints in other records retain their original bits.
    assert_eq!(
        next.view.patch_assignments.expressions[4].record.maximum,
        100.0.into()
    );
}

#[test]
fn reversed_range_edit_changes_only_exact_max_min_fields() {
    let s = captured();
    let slot = ExpressionSlotId::new(1, 2).unwrap();
    let next = draft_edit(
        &s,
        AssignmentEdit::ExpressionRange {
            slot,
            minimum: F32Bits::from_f32(87.0),
            maximum: F32Bits::from_f32(3.0),
        },
    );
    let index = s.view.patch_assignments.expressions[5].raw_record_index;
    assert!(
        differences(&s, &next)
            .iter()
            .all(|&(i, off)| i == index && (4..12).contains(&off))
    );
    let actual = PatchAssignments::from_raw(&next.raw).expressions[5]
        .record
        .clone();
    assert_eq!(actual.module_raw, 255);
    assert_eq!(actual.minimum.as_f32(), 87.0);
    assert_eq!(actual.maximum.as_f32(), 3.0);
    assert_eq!(
        next.view.patch_assignments,
        PatchAssignments::from_raw(&next.raw)
    );
}

#[test]
fn parameter_dependency_rejects_stale_model_and_preserves_off_slot_bounds() {
    let s = captured();
    let wah = s.view.module(ModuleId::new(1).unwrap()).unwrap();
    let target = ParameterAssignmentTarget {
        module: wah.module,
        expected_model: wah.model,
        parameter: ParamId::new(3).unwrap(),
    };
    let slot = ExpressionSlotId::new(2, 1).unwrap();
    let next = draft_edit(
        &s,
        AssignmentEdit::ExpressionTarget {
            slot,
            target: ExpressionAssignmentTarget::Parameter(target.clone()),
        },
    );
    let old = &s.view.patch_assignments.expressions[7];
    let new = &next.view.patch_assignments.expressions[7];
    assert_eq!(
        (new.record.maximum, new.record.minimum),
        (old.record.maximum, old.record.minimum)
    );
    assert!(
        differences(&s, &next)
            .iter()
            .all(|&(i, off)| i == old.raw_record_index && (1..4).contains(&off))
    );
    let mut wrong = target;
    wrong.expected_model.0 ^= 1;
    let edit = AssignmentEdit::ExpressionTarget {
        slot,
        target: ExpressionAssignmentTarget::Parameter(wrong),
    };
    let mut draft = PresetDraft::from_snapshot(&s);
    let original = draft.clone();
    assert!(draft.apply(&EditPatch::SetPatchAssignment(edit)).is_err());
    assert_eq!(draft, original);
}

#[test]
fn ctrl_changes_only_low_membership_bits_and_quick_has_distinct_special_targets() {
    let s = captured();
    let mut records = s.raw.records().to_vec();
    let index = s.view.patch_assignments.ctrls[7].raw_record_index;
    records[index].body[4..8].copy_from_slice(&0xdeadf800u32.to_le_bytes());
    let s = PresetSnapshot::from_raw(
        Arc::new(RawTransfer::from_records(0x11, &records).unwrap()),
        s.source,
        s.target,
        s.revision,
    );
    let next = draft_edit(
        &s,
        AssignmentEdit::CtrlModules {
            slot: 7,
            modules: vec![ModuleId::new(1).unwrap(), ModuleId::new(10).unwrap()],
        },
    );
    assert_eq!(
        &next.raw.records()[index].body[..4],
        &s.raw.records()[index].body[..4]
    );
    assert_eq!(
        next.view.patch_assignments.ctrls[7].record.module_mask_raw,
        0xdeadfc02
    );
    assert!(
        differences(&s, &next)
            .iter()
            .all(|&(i, off)| i == index && (4..8).contains(&off))
    );
    let next = draft_edit(
        &next,
        AssignmentEdit::QuickTarget {
            index: 2,
            target: QuickAssignmentTarget::PatchTempo,
        },
    );
    assert_eq!(
        next.view.patch_assignments.quicks[2]
            .record
            .to_record()
            .body,
        [2, 12, 0, 0]
    );
}

#[test]
fn ambiguous_identity_and_new_nan_endpoints_cannot_mutate_draft() {
    let s = captured();
    let mut records = s.raw.records().to_vec();
    records.push(s.view.patch_assignments.expressions[0].record.to_record());
    records.push(RawRecord {
        command: 12,
        body: vec![0; 11],
    });
    records.push(RawRecord {
        command: 0xbeef,
        body: vec![9, 8, 7],
    });
    let s = PresetSnapshot::from_raw(
        Arc::new(RawTransfer::from_records(0x11, &records).unwrap()),
        s.source,
        s.target,
        s.revision,
    );
    assert_eq!(s.view.patch_assignments.diagnostics.len(), 2);
    let mut draft = PresetDraft::from_snapshot(&s);
    let original = draft.clone();
    assert!(
        draft
            .apply(&EditPatch::SetPatchAssignment(
                AssignmentEdit::ExpressionRange {
                    slot: ExpressionSlotId::new(0, 0).unwrap(),
                    minimum: 0.0.into(),
                    maximum: 99.0.into()
                }
            ))
            .is_err()
    );
    assert_eq!(draft, original);
    assert_eq!(
        draft
            .snapshot(&s, SnapshotSource::Simulator, Revision(2))
            .unwrap()
            .raw,
        s.raw
    );
    assert!(
        draft
            .apply(&EditPatch::SetPatchAssignment(
                AssignmentEdit::ExpressionRange {
                    slot: ExpressionSlotId::new(2, 2).unwrap(),
                    minimum: F32Bits(0x7fc12345),
                    maximum: 0.0.into()
                }
            ))
            .is_err()
    );
}

#[test]
fn serde_validates_packed_slot_and_unknown_reference_remains_unresolved_and_lossless() {
    assert!(serde_json::from_str::<ExpressionSlotId>("19").is_err());
    let s = captured();
    let mut records = s.raw.records().to_vec();
    let index = s.view.patch_assignments.expressions[1].raw_record_index;
    records[index].body[1] = 201;
    records[index].body[2..4].copy_from_slice(&259u16.to_le_bytes());
    records[index].body[4..8].copy_from_slice(&0x7fc12345u32.to_le_bytes());
    records[index].body[8..12].copy_from_slice(&0x80000000u32.to_le_bytes());
    let s = PresetSnapshot::from_raw(
        Arc::new(RawTransfer::from_records(0x11, &records).unwrap()),
        s.source,
        s.target,
        s.revision,
    );
    assert_eq!(
        PatchAssignments::resolve_expression(
            &s.view.patch_assignments.expressions[1].record,
            &s.view.modules
        ),
        ResolvedAssignmentReference::Unresolved {
            module_raw: 201,
            parameter_raw: 259
        }
    );
    let draft = PresetDraft::from_snapshot(&s);
    assert_eq!(
        draft
            .snapshot(&s, SnapshotSource::Simulator, Revision(2))
            .unwrap()
            .raw,
        s.raw
    );
    let restored: PresetSnapshot =
        serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
    assert_eq!(restored.raw, s.raw);
    assert_eq!(restored.view.patch_assignments, s.view.patch_assignments);
}

#[test]
fn old_snapshot_json_without_projection_migrates_from_raw_without_changes() {
    let s = captured();
    let mut json = serde_json::to_value(&s).unwrap();
    json["view"]
        .as_object_mut()
        .unwrap()
        .remove("patch_assignments");
    let old: PresetSnapshot = serde_json::from_value(json).unwrap();
    assert_eq!(old.view.patch_assignments, PatchAssignments::default());
    let draft = PresetDraft::from_snapshot(&old);
    assert_eq!(draft.view.patch_assignments, s.view.patch_assignments);
    assert_eq!(
        draft
            .snapshot(&old, SnapshotSource::Simulator, Revision(2))
            .unwrap()
            .raw,
        s.raw
    );
    let next = draft_edit(
        &old,
        AssignmentEdit::QuickTarget {
            index: 1,
            target: QuickAssignmentTarget::PatchTempo,
        },
    );
    assert_eq!(next.view.patch_assignments.quicks[1].record.module_raw, 12);
}

#[test]
fn routing_moves_only_two_boundaries_and_rejects_crossed_or_out_of_range_pair() {
    let s = captured();
    let routing = s.view.patch_assignments.routing.as_ref().unwrap();
    let next = draft_edit(
        &s,
        AssignmentEdit::FxLoopPositions {
            send_boundary: 3,
            return_boundary: 4,
        },
    );
    assert_eq!(differences(&s, &next), [(routing.raw_record_index, 2)]);
    for (send_boundary, return_boundary) in [(5, 4), (0, 12)] {
        let mut draft = PresetDraft::from_snapshot(&s);
        let original = draft.clone();
        assert!(
            draft
                .apply(&EditPatch::SetPatchAssignment(
                    AssignmentEdit::FxLoopPositions {
                        send_boundary,
                        return_boundary
                    }
                ))
                .is_err()
        );
        assert_eq!(draft, original);
    }
}

#[test]
fn combined_expression_target_and_range_is_atomic_and_preserves_unrelated_bytes() {
    let s = captured();
    let slot = ExpressionSlotId::new(2, 2).unwrap();
    let wah = s.view.module(ModuleId::new(1).unwrap()).unwrap();
    let target = ExpressionAssignmentTarget::Parameter(ParameterAssignmentTarget {
        module: wah.module,
        expected_model: wah.model,
        parameter: ParamId::new(3).unwrap(),
    });
    let next = draft_edit(
        &s,
        AssignmentEdit::ExpressionTargetAndRange {
            slot,
            target,
            minimum: 80.0.into(),
            maximum: 20.0.into(),
        },
    );
    let index = s.view.patch_assignments.expressions[8].raw_record_index;
    assert!(
        differences(&s, &next)
            .iter()
            .all(|&(i, offset)| i == index && (1..12).contains(&offset))
    );
    let actual = &next.view.patch_assignments.expressions[8].record;
    assert_eq!(
        (
            actual.module_raw,
            actual.parameter_raw,
            actual.minimum,
            actual.maximum
        ),
        (1, 3, 80.0.into(), 20.0.into())
    );
}
