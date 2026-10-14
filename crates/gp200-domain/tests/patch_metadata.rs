use gp200_domain::*;
use std::sync::Arc;
fn baseline() -> PresetSnapshot {
    PresetSnapshot::from_raw(
        Arc::new(
            RawTransfer::from_payload(
                0x12,
                include_bytes!("../../../assets/demo/read-current-payload.bin").to_vec(),
            )
            .unwrap(),
        ),
        SnapshotSource::Simulator,
        Some(PresetTarget::EditBuffer),
        Revision(0),
    )
}
#[test]
fn all_retained_pan_style_fx_values_survive_passive_projection_and_json() {
    for (pan, style, send, returned, mode) in [
        (-128i8, 65535u16, 65535u16, 65534u16, 255u8),
        (-101, 20, 101, 102, 2),
        (-100, 19, 100, 100, 1),
        (100, 0, 0, 0, 0),
        (127, 12, 99, 98, 42),
    ] {
        let base = baseline();
        let mut records = base.raw.records().to_vec();
        let body = &mut records.iter_mut().find(|r| r.command == 2).unwrap().body;
        body[7] = pan as u8;
        body[8..10].copy_from_slice(&style.to_le_bytes());
        body[10..12].copy_from_slice(&send.to_le_bytes());
        body[12..14].copy_from_slice(&returned.to_le_bytes());
        body[14] = mode;
        let raw = Arc::new(RawTransfer::from_records(0x12, &records).unwrap());
        let base =
            PresetSnapshot::from_raw(raw.clone(), SnapshotSource::Simulator, None, Revision(0));
        let draft = PresetDraft::from_snapshot(&base);
        assert_eq!(draft.metadata.native_pan.unwrap().raw(), pan);
        assert_eq!(draft.metadata.native_style.unwrap().raw(), style);
        assert_eq!(draft.metadata.fx_send_level, Some(send));
        assert_eq!(draft.metadata.fx_return_level, Some(returned));
        assert_eq!(draft.metadata.fx_loop_mode.unwrap().raw(), mode);
        let roundtrip: PresetDraft =
            serde_json::from_slice(&serde_json::to_vec(&draft).unwrap()).unwrap();
        assert_eq!(roundtrip, draft);
        assert_eq!(
            draft
                .snapshot(&base, SnapshotSource::Simulator, Revision(1))
                .unwrap()
                .raw,
            raw
        );
    }
}
#[test]
fn independent_offsets_change_only_selected_native_fields() {
    let base = baseline();
    let mut draft = PresetDraft::from_snapshot(&base);
    draft
        .apply(&EditPatch::SetMetadata(MetadataPatch {
            native_pan: Some(PatchPan::new(-100).unwrap()),
            native_style: Some(NativeStyleId::new(19).unwrap()),
            fx_send_level: Some(41),
            fx_return_level: Some(62),
            fx_loop_mode: Some(PatchFxLoopMode::new(1).unwrap()),
            author: Some("local only".into()),
            notes: Some("local note".into()),
            style: Some("My own genre".into()),
            ..Default::default()
        }))
        .unwrap();
    let changed = draft
        .snapshot(&base, SnapshotSource::Simulator, Revision(1))
        .unwrap();
    let mut expected = base.raw.records().to_vec();
    let body = &mut expected.iter_mut().find(|r| r.command == 2).unwrap().body;
    body[7] = 156;
    body[8..10].copy_from_slice(&19u16.to_le_bytes());
    body[10..12].copy_from_slice(&41u16.to_le_bytes());
    body[12..14].copy_from_slice(&62u16.to_le_bytes());
    body[14] = 1;
    assert_eq!(changed.raw.records(), expected);
    let native = PresetSnapshot::from_raw(
        changed.raw.clone(),
        SnapshotSource::Simulator,
        None,
        Revision(1),
    );
    assert_eq!(native.view.metadata.native_pan, draft.metadata.native_pan);
    assert_eq!(
        native.view.metadata.native_style,
        draft.metadata.native_style
    );
    assert_eq!(native.view.metadata.author, None);
    assert_eq!(changed.view.metadata.author.as_deref(), Some("local only"));
}
#[test]
fn edit_validation_rejects_unknown_values_without_normalizing_the_baseline() {
    let base = baseline();
    for patch in [
        MetadataPatch {
            native_pan: Some(PatchPan::from_raw(-101)),
            ..Default::default()
        },
        MetadataPatch {
            native_style: Some(NativeStyleId::from_raw(20)),
            ..Default::default()
        },
        MetadataPatch {
            fx_send_level: Some(101),
            ..Default::default()
        },
        MetadataPatch {
            fx_return_level: Some(65535),
            ..Default::default()
        },
        MetadataPatch {
            fx_loop_mode: Some(PatchFxLoopMode::from_raw(2)),
            ..Default::default()
        },
    ] {
        let mut draft = PresetDraft::from_snapshot(&base);
        let before = draft.clone();
        assert!(draft.apply(&EditPatch::SetMetadata(patch)).is_err());
        assert_eq!(draft, before);
    }
    assert!(PatchPan::new(-100).is_ok());
    assert!(PatchPan::new(100).is_ok());
    assert!(PatchPan::new(101).is_err());
    assert!(PatchPan::new(-129).is_err());
}
#[test]
fn old_json_metadata_migrates_from_raw_without_losing_annotations() {
    let mut base = baseline();
    base.view.metadata = MetadataPatch {
        notes: Some("old notes".into()),
        style: Some("local genre".into()),
        ..Default::default()
    };
    let draft = PresetDraft::from_snapshot(&base);
    assert_eq!(draft.metadata.native_pan, Some(PatchPan::from_raw(0)));
    assert_eq!(
        draft.metadata.native_style,
        Some(NativeStyleId::from_raw(5))
    );
    assert_eq!(draft.metadata.notes.as_deref(), Some("old notes"));
    let metadata: MetadataPatch =
        serde_json::from_str(r#"{"name":"Old","pan":null,"style":"Local"}"#).unwrap();
    assert!(metadata.native_pan.is_none());
}
#[test]
fn factory_and_observed_custom_names_use_exact_ids_not_freeform_tags() {
    for (id, name) in [
        "-", "Metal", "World", "Indie", "Country", "Rock", "Funk", "Pop", "Blues", "Jazz", "Bass",
        "Acoustic",
    ]
    .iter()
    .enumerate()
    {
        assert_eq!(NativeStyleId::new(id as u16).unwrap().label(None), *name);
    }
    let global = RawTransfer::from_payload(
        0x12,
        include_bytes!("../../../tests/fixtures/globals/baseline.payload.bin").to_vec(),
    )
    .unwrap();
    for id in 12..20 {
        assert_eq!(
            NativeStyleId::new(id).unwrap().label(Some(&global)),
            format!("User{}", id - 11)
        );
    }
    assert!(
        NativeStyleId::from_raw(600)
            .label(Some(&global))
            .contains("600")
    );
    assert!(
        NativeStyleId::new(12)
            .unwrap()
            .label(None)
            .contains("not read")
    );
}
#[test]
fn hardware_profile_does_not_promote_static_bounds_or_imported_ids() {
    let base = baseline().view.metadata;
    for patch in [
        MetadataPatch {
            native_pan: Some(PatchPan::new(1).unwrap()),
            ..Default::default()
        },
        MetadataPatch {
            native_style: Some(NativeStyleId::new(0).unwrap()),
            ..Default::default()
        },
    ] {
        assert!(patch.hardware_profile(&base).is_ok());
    }
    for patch in [
        MetadataPatch {
            native_pan: Some(PatchPan::new(-1).unwrap()),
            ..Default::default()
        },
        MetadataPatch {
            native_style: Some(NativeStyleId::new(12).unwrap()),
            ..Default::default()
        },
        MetadataPatch {
            fx_send_level: Some(1),
            ..Default::default()
        },
        MetadataPatch {
            fx_loop_mode: Some(PatchFxLoopMode::new(1).unwrap()),
            ..Default::default()
        },
    ] {
        assert_eq!(
            patch.hardware_profile(&base).unwrap_err().kind,
            ErrorKind::Unverified
        );
    }
}
#[test]
fn signed_pan_incremental_record_is_sign_extended_u16() {
    assert_eq!(
        gp200_protocol::metadata_write_record(
            6,
            i16::from(PatchPan::new(-100).unwrap().raw()) as u16
        )
        .unwrap()
        .body,
        [6, 0, 156, 255]
    );
    assert_eq!(
        gp200_protocol::metadata_write_record(2, 19).unwrap().body,
        [2, 0, 19, 0]
    );
}
#[test]
fn direct_draft_changes_cannot_encode_invalid_native_values() {
    let base = baseline();
    for patch in [
        MetadataPatch {
            native_pan: Some(PatchPan::from_raw(-101)),
            ..Default::default()
        },
        MetadataPatch {
            native_style: Some(NativeStyleId::from_raw(20)),
            ..Default::default()
        },
        MetadataPatch {
            fx_send_level: Some(101),
            ..Default::default()
        },
        MetadataPatch {
            fx_return_level: Some(101),
            ..Default::default()
        },
        MetadataPatch {
            fx_loop_mode: Some(PatchFxLoopMode::from_raw(2)),
            ..Default::default()
        },
    ] {
        let mut draft = PresetDraft::from_snapshot(&base);
        draft.metadata.overlay(&patch);
        assert!(
            draft
                .snapshot(&base, SnapshotSource::Simulator, Revision(1))
                .is_err()
        );
    }
}
