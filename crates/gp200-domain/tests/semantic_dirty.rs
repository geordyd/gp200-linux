// Private capture acceptance suite; excluded from public source builds.
// Run with RUSTFLAGS="--cfg gp200_private_evidence" only in the retained lab checkout.
#![cfg(gp200_private_evidence)]

use gp200_domain::*;
use std::sync::Arc;

const CONNECT: &[u8] = include_bytes!(
    "../../../analysis/implementation/inactive-restore-recovery/probe/cleanup-fresh-session-24-preset.payload.bin"
);
const REFRESH: &[u8] = include_bytes!(
    "../../../analysis/implementation/inactive-restore-recovery/probe/cleanup-fresh-session-28-preset.payload.bin"
);

fn snapshot(bytes: &[u8]) -> Arc<PresetSnapshot> {
    Arc::new(PresetSnapshot::from_raw(
        Arc::new(RawTransfer::from_payload(0x12, bytes.to_vec()).unwrap()),
        SnapshotSource::Hardware {
            epoch: SessionEpoch(1),
        },
        Some(PresetTarget::EditBuffer),
        Revision(1),
    ))
}

fn document() -> EditorDocument {
    EditorDocument::new(
        DocumentId("semantic-dirty-test".into()),
        EditTarget::DeviceBuffer,
        snapshot(CONNECT),
    )
}

#[test]
fn actual_connect_refresh_padding_is_clean_and_stays_lossless() {
    let mut doc = document();
    let before = doc.baseline.clone();
    let incoming = snapshot(REFRESH);
    assert_ne!(before.raw, incoming.raw);
    assert_ne!(
        before.view.patch_assignments,
        incoming.view.patch_assignments
    );
    doc.draft = PresetDraft::from_snapshot(&incoming);
    let exact_before_check = serde_json::to_vec(&doc.draft).unwrap();
    assert!(!doc.dirty());
    assert_eq!(serde_json::to_vec(&doc.draft).unwrap(), exact_before_check);
    assert_eq!(*doc.baseline, *before);
    let encoded = doc
        .draft
        .snapshot(&incoming, incoming.source.clone(), Revision(2))
        .unwrap();
    // The codec retains the chosen raw before-image's original CTRL padding;
    // the semantic predicate itself cannot rewrite either captured payload.
    assert_eq!(encoded.raw, incoming.raw);
    assert_eq!(incoming.raw.payload(), REFRESH);
    let exported = doc
        .draft
        .snapshot(&before, before.source.clone(), Revision(2))
        .unwrap();
    assert_eq!(exported.raw, before.raw);
    assert_eq!(serde_json::to_vec(&doc.draft).unwrap(), exact_before_check);
}

#[test]
fn all_retained_parameter_bits_and_slot_counts_still_make_the_document_dirty() {
    let clean = document();
    for (module_index, module) in clean.draft.view.modules.iter().enumerate() {
        for parameter_index in 0..module.parameters.len() {
            let mut changed = clean.clone();
            changed.draft.view.modules[module_index].parameters[parameter_index].0 ^= 1;
            assert!(
                changed.dirty(),
                "module {module_index}, slot {parameter_index}"
            );
        }
    }
    for (a, b) in [(0x0000_0000, 0x8000_0000), (0x7fc0_1234, 0x7fc0_1235)] {
        let mut first = clean.clone();
        first.draft.view.modules[0].parameters[0] = F32Bits(a);
        let mut second = first.draft.clone();
        second.view.modules[0].parameters[0] = F32Bits(b);
        assert!(!first.draft.semantically_equal(&second));
    }
    let mut changed = clean.clone();
    changed.draft.view.modules[0].slot_count_raw ^= 0x8000;
    assert!(changed.dirty());
    changed = clean;
    changed.draft.view.modules[0].state_raw ^= 0x80;
    assert!(changed.dirty());
}

#[test]
fn assignment_unknown_bits_raw_values_and_diagnostics_are_meaningful() {
    let clean = document();
    let mut variants = Vec::new();
    let mut changed = clean.clone();
    changed.draft.view.patch_assignments.ctrls[0]
        .record
        .module_mask_raw ^= 1 << 31;
    variants.push(changed);
    let mut changed = clean.clone();
    changed.draft.view.patch_assignments.ctrls[0]
        .record
        .state_raw ^= 0x80;
    variants.push(changed);
    let mut changed = clean.clone();
    changed.draft.view.patch_assignments.expressions[0]
        .record
        .parameter_raw ^= 0x8000;
    variants.push(changed);
    let mut changed = clean.clone();
    changed.draft.view.patch_assignments.expressions[0]
        .record
        .minimum
        .0 ^= 1;
    variants.push(changed);
    let mut changed = clean.clone();
    changed.draft.view.patch_assignments.quicks[0]
        .record
        .module_raw ^= 0x80;
    variants.push(changed);
    let mut changed = clean;
    changed
        .draft
        .view
        .patch_assignments
        .diagnostics
        .push(AssignmentDiagnostic {
            raw_record_index: 0,
            command: 0xbeef,
            description: "Unknown retained assignment value".into(),
        });
    variants.push(changed);
    for changed in variants {
        assert!(changed.dirty());
    }
}

#[test]
fn recovered_evidence_and_record_provenance_are_descriptive_only() {
    let mut changed = document();
    let name = changed.draft.view.name.value().unwrap().clone();
    changed.draft.view.name = Recovered::Confirmed {
        value: name,
        evidence: vec![EvidenceRef::new("readback", EvidenceLevel::LiveVerified)],
    };
    changed.draft.view.modules[0].raw_record_index += 1;
    changed.draft.view.patch_assignments.ctrls[0].raw_record_index += 1;
    assert!(!changed.dirty());
    // Equality never grants encoding or mutation authority: a forged module
    // record index still fails the codec's separate retained-record guard.
    assert!(
        changed
            .draft
            .snapshot(&changed.baseline, SnapshotSource::Simulator, Revision(2),)
            .is_err()
    );
    changed.draft.view.name = Recovered::Unknown;
    assert!(changed.dirty());
}

#[test]
fn returning_to_the_original_value_is_clean_and_history_retains_exact_drafts() {
    let mut doc = document();
    let original = doc.draft.clone();
    let level = *doc.draft.view.level.value().unwrap();
    doc.apply(&EditPatch::SetMetadata(MetadataPatch {
        level: Some(level + 1),
        ..Default::default()
    }))
    .unwrap();
    let edited = doc.draft.clone();
    assert!(doc.dirty());
    doc.apply(&EditPatch::SetMetadata(MetadataPatch {
        level: Some(level),
        ..Default::default()
    }))
    .unwrap();
    assert_ne!(
        doc.draft, original,
        "edit evidence and metadata mirrors differ"
    );
    assert!(!doc.dirty());
    assert_eq!(doc.undo.past, [original.clone(), edited.clone()]);
    assert!(doc.undo());
    assert_eq!(doc.draft, edited);
    assert!(doc.dirty());
    assert!(doc.undo());
    assert_eq!(doc.draft, original);
    assert!(!doc.dirty());
    assert!(doc.redo());
    assert!(doc.dirty());
}

#[test]
fn local_annotations_remain_dirty_and_unknown_raw_records_survive() {
    let mut records = snapshot(CONNECT).raw.records().to_vec();
    records.push(RawRecord {
        command: 0xbeef,
        body: vec![0, 255, 42, 128],
    });
    let raw = Arc::new(RawTransfer::from_records(0x12, &records).unwrap());
    let base = Arc::new(PresetSnapshot::from_raw(
        raw.clone(),
        SnapshotSource::Simulator,
        Some(PresetTarget::EditBuffer),
        Revision(1),
    ));
    let mut doc = EditorDocument::new(DocumentId("opaque".into()), EditTarget::DeviceBuffer, base);
    doc.apply(&EditPatch::SetMetadata(MetadataPatch {
        notes: Some("Keep this take".into()),
        ..Default::default()
    }))
    .unwrap();
    assert!(doc.dirty());
    let encoded = doc
        .draft
        .snapshot(&doc.baseline, SnapshotSource::Simulator, Revision(2))
        .unwrap();
    assert_eq!(encoded.raw, raw);
    assert!(doc.undo());
    assert!(!doc.dirty());
}

#[test]
fn padding_reconciliation_preserves_other_opaque_bytes_and_encoder_guards() {
    let incoming = snapshot(REFRESH);
    let draft = PresetDraft::from_snapshot(&incoming);
    let mut records = snapshot(CONNECT).raw.records().to_vec();
    // This unprojected metadata field and completely unknown TLV have no
    // semantic mask. They must survive the chosen raw before-image verbatim.
    records.iter_mut().find(|r| r.command == 2).unwrap().body[32] ^= 0x80;
    records.push(RawRecord {
        command: 0xbeef,
        body: vec![1, 2, 255, 128],
    });
    let chosen_raw = Arc::new(RawTransfer::from_records(0x12, &records).unwrap());
    let chosen = PresetSnapshot::from_raw(
        chosen_raw.clone(),
        SnapshotSource::Simulator,
        Some(PresetTarget::EditBuffer),
        Revision(1),
    );
    assert_eq!(
        draft
            .snapshot(&chosen, SnapshotSource::Simulator, Revision(2))
            .unwrap()
            .raw,
        chosen_raw
    );
    for variation in 0..3 {
        let mut forged = draft.clone();
        let ctrl = &mut forged.view.patch_assignments.ctrls[0];
        match variation {
            0 => ctrl.record.module_mask_raw ^= 1 << 31,
            1 => ctrl.raw_record_index += 1,
            _ => ctrl.record.slot ^= 1,
        }
        assert!(
            forged
                .snapshot(&chosen, SnapshotSource::Simulator, Revision(2))
                .is_err()
        );
    }
    for bits in [0x8000_0000, 0x7fc0_1234, 0x7fc0_1235] {
        let mut edited = draft.clone();
        edited.view.modules[0].parameters[14] = F32Bits(bits);
        assert!(!edited.semantically_equal(&draft));
        let encoded = edited
            .snapshot(&chosen, SnapshotSource::Simulator, Revision(2))
            .unwrap();
        assert_eq!(encoded.view.modules[0].parameters[14], F32Bits(bits));
        assert_eq!(encoded.raw.records().last(), chosen_raw.records().last());
        assert_eq!(
            encoded
                .raw
                .records()
                .iter()
                .find(|r| r.command == 2)
                .unwrap(),
            chosen_raw
                .records()
                .iter()
                .find(|r| r.command == 2)
                .unwrap()
        );
    }
}
