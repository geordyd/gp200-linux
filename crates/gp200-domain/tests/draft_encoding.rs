use gp200_domain::*;
use std::sync::Arc;

fn captured() -> PresetSnapshot {
    let raw = RawTransfer::from_payload(
        0x12,
        include_bytes!("../../../assets/demo/read-current-payload.bin").to_vec(),
    )
    .unwrap();
    PresetSnapshot::from_raw(
        Arc::new(raw),
        SnapshotSource::Simulator,
        Some(PresetTarget::EditBuffer),
        Revision(0),
    )
}
#[test]
fn established_edits_change_only_their_raw_fields() {
    let baseline = captured();
    let mut draft = PresetDraft::from_snapshot(&baseline);
    draft
        .apply(&EditPatch::SetMetadata(MetadataPatch {
            name: Some("Sixteen ByteName".into()),
            tempo: Some(137),
            level: Some(61),
            author: Some("Local author".into()),
            notes: Some("Local notes retained".into()),
            ..Default::default()
        }))
        .unwrap();
    draft
        .apply(&EditPatch::MoveModule {
            module: ModuleId::new(10).unwrap(),
            before: Some(ModuleId::new(0).unwrap()),
        })
        .unwrap();
    let snapshot = draft
        .snapshot(&baseline, SnapshotSource::Simulator, Revision(1))
        .unwrap();
    let mut expected = baseline.raw.records().to_vec();
    let metadata = expected
        .iter_mut()
        .find(|record| record.command == 2)
        .unwrap();
    metadata.body[2..4].copy_from_slice(&137u16.to_le_bytes());
    metadata.body[4..6].copy_from_slice(&61u16.to_le_bytes());
    metadata.body[16..32].copy_from_slice(&gp200_protocol::ascii_name("Sixteen ByteName").unwrap());
    let chain = expected
        .iter_mut()
        .find(|record| record.command == 8)
        .unwrap();
    chain.body[4..15].copy_from_slice(&[10, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9]);
    assert_eq!(snapshot.raw.records(), expected);
    let projected = PresetSnapshot::from_raw(
        snapshot.raw.clone(),
        SnapshotSource::Simulator,
        None,
        Revision(1),
    );
    assert_eq!(projected.view.name.value(), snapshot.view.name.value());
    assert_eq!(projected.view.tempo.value(), snapshot.view.tempo.value());
    assert_eq!(projected.view.level.value(), snapshot.view.level.value());
    assert_eq!(projected.view.chain.value(), snapshot.view.chain.value());
    assert_eq!(
        snapshot.view.metadata.author.as_deref(),
        Some("Local author")
    );
    assert_eq!(
        baseline
            .raw
            .records()
            .iter()
            .find(|record| record.command == 2)
            .unwrap()
            .body[32..],
        snapshot
            .raw
            .records()
            .iter()
            .find(|record| record.command == 2)
            .unwrap()
            .body[32..]
    );
}
#[test]
fn unverified_name_encodings_remain_local_and_untouched_names_are_byte_exact() {
    let baseline = captured();
    for name in ["Björk", "Name longer than sixteen", "Control\nCharacter"] {
        let mut draft = PresetDraft::from_snapshot(&baseline);
        draft
            .apply(&EditPatch::SetMetadata(MetadataPatch {
                name: Some(name.into()),
                ..Default::default()
            }))
            .unwrap();
        let snapshot = draft
            .snapshot(&baseline, SnapshotSource::Simulator, Revision(1))
            .unwrap();
        assert_eq!(snapshot.raw, baseline.raw);
        assert_eq!(snapshot.view.name.value().map(String::as_str), Some(name));
    }
    let mut records = baseline.raw.records().to_vec();
    let record = records
        .iter_mut()
        .find(|record| record.command == 2)
        .unwrap();
    record.body[16..32].copy_from_slice(&[
        0xe9, 0xa1, 0xff, 0, 19, 18, 17, 16, 15, 14, 13, 12, 11, 10, 9, 8,
    ]);
    let raw = Arc::new(RawTransfer::from_records(0x12, &records).unwrap());
    let original = PresetSnapshot::from_raw(raw, SnapshotSource::Simulator, None, Revision(0));
    let snapshot = PresetDraft::from_snapshot(&original)
        .snapshot(&original, SnapshotSource::Simulator, Revision(1))
        .unwrap();
    assert_eq!(snapshot.raw, original.raw);
}
#[test]
fn invalid_chain_and_module_aliasing_fail_before_serialization() {
    let baseline = captured();
    let mut draft = PresetDraft::from_snapshot(&baseline);
    draft.view.chain = Recovered::Candidate {
        value: vec![ModuleId::new(0).unwrap(); 11],
        evidence: Vec::new(),
    };
    assert!(
        draft
            .snapshot(&baseline, SnapshotSource::Simulator, Revision(1))
            .unwrap_err()
            .message
            .contains("exactly once")
    );
    let mut draft = PresetDraft::from_snapshot(&baseline);
    draft.view.modules[1].raw_record_index = draft.view.modules[0].raw_record_index;
    assert!(
        draft
            .snapshot(&baseline, SnapshotSource::Simulator, Revision(1))
            .is_err()
    );
    let mut draft = PresetDraft::from_snapshot(&baseline);
    draft.view.modules[0].parameters.pop();
    assert!(
        draft
            .snapshot(&baseline, SnapshotSource::Simulator, Revision(1))
            .is_err()
    );
}
#[test]
fn changed_metadata_requires_a_unique_established_record() {
    let baseline = captured();
    let mut records = baseline.raw.records().to_vec();
    records.push(
        records
            .iter()
            .find(|record| record.command == 2)
            .unwrap()
            .clone(),
    );
    let baseline = PresetSnapshot::from_raw(
        Arc::new(RawTransfer::from_records(0x12, &records).unwrap()),
        SnapshotSource::Simulator,
        None,
        Revision(0),
    );
    let mut draft = PresetDraft::from_snapshot(&baseline);
    draft
        .apply(&EditPatch::SetMetadata(MetadataPatch {
            tempo: Some(137),
            ..Default::default()
        }))
        .unwrap();
    assert!(
        draft
            .snapshot(&baseline, SnapshotSource::Simulator, Revision(1))
            .unwrap_err()
            .message
            .contains("ambiguous")
    );
}
#[test]
fn changed_model_recomputes_unresolved_asset_dependencies() {
    let baseline = captured();
    let mut draft = PresetDraft::from_snapshot(&baseline);
    draft
        .apply(&EditPatch::SetModel {
            module: ModuleId::new(5).unwrap(),
            model: ModelCode(0x0a10_0013),
        })
        .unwrap();
    let snapshot = draft
        .snapshot(&baseline, SnapshotSource::Simulator, Revision(1))
        .unwrap();
    assert!(snapshot.asset_refs.contains(&AssetRef::UserSlot {
        kind: AssetKind::ImpulseResponse,
        slot: 19,
        content: None
    }));
}

#[test]
fn existing_verified_user_asset_hash_survives_an_ordinary_edit() {
    let mut baseline = captured();
    let mut draft = PresetDraft::from_snapshot(&baseline);
    draft
        .apply(&EditPatch::SetModel {
            module: ModuleId::new(5).unwrap(),
            model: ModelCode(0x0a10_0013),
        })
        .unwrap();
    baseline = draft
        .snapshot(&baseline, SnapshotSource::Simulator, Revision(1))
        .unwrap();
    let asset = AssetId("verified-content-hash".into());
    baseline.asset_refs = vec![AssetRef::UserSlot {
        kind: AssetKind::ImpulseResponse,
        slot: 19,
        content: Some(asset.clone()),
    }];
    let mut draft = PresetDraft::from_snapshot(&baseline);
    draft
        .apply(&EditPatch::SetMetadata(MetadataPatch {
            tempo: Some(137),
            ..Default::default()
        }))
        .unwrap();
    let changed = draft
        .snapshot(&baseline, SnapshotSource::Simulator, Revision(2))
        .unwrap();
    assert!(changed.asset_refs.contains(&AssetRef::UserSlot {
        kind: AssetKind::ImpulseResponse,
        slot: 19,
        content: Some(asset)
    }));
    assert_eq!(changed.view.metadata.tempo, Some(137));
}
