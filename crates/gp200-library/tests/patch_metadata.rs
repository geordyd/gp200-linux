use gp200_domain::*;
use gp200_library::NativeFile;
use std::sync::Arc;

fn baseline() -> PresetSnapshot {
    let file = NativeFile::parse(include_bytes!(
        "../../../tests/fixtures/native/constructed-mode1.prst"
    ))
    .unwrap();
    PresetSnapshot::from_raw(
        Arc::clone(file.raw()),
        SnapshotSource::Simulator,
        None,
        Revision(0),
    )
}

#[test]
fn edited_native_pan_style_fx_fields_export_and_parse_with_exact_records() {
    let before = baseline();
    let mut draft = PresetDraft::from_snapshot(&before);
    draft
        .apply(&EditPatch::SetMetadata(MetadataPatch {
            native_pan: Some(PatchPan::new(-100).unwrap()),
            native_style: Some(NativeStyleId::new(19).unwrap()),
            fx_send_level: Some(100),
            fx_return_level: Some(99),
            fx_loop_mode: Some(PatchFxLoopMode::new(1).unwrap()),
            notes: Some("Local annotation".into()),
            style: Some("Local taxonomy".into()),
            ..Default::default()
        }))
        .unwrap();
    let snapshot = draft
        .snapshot(&before, SnapshotSource::Simulator, Revision(1))
        .unwrap();
    let exported = NativeFile::preset_only(&snapshot, None).unwrap();
    let parsed = NativeFile::parse(&exported).unwrap();
    assert_eq!(parsed.raw().records(), snapshot.raw.records());
    let native = PresetSnapshot::from_raw(
        Arc::clone(parsed.raw()),
        SnapshotSource::Simulator,
        None,
        Revision(1),
    );
    assert_eq!(
        native.view.metadata.native_pan,
        Some(PatchPan::from_raw(-100))
    );
    assert_eq!(
        native.view.metadata.native_style,
        Some(NativeStyleId::from_raw(19))
    );
    assert_eq!(native.view.metadata.notes, None);
    assert_eq!(
        snapshot.view.metadata.notes.as_deref(),
        Some("Local annotation")
    );
}

#[test]
fn absent_legacy_projections_are_retained_but_unencoded_explicit_edits_refuse_export() {
    let mut old = baseline();
    old.view.metadata.native_pan = None;
    old.view.metadata.native_style = None;
    old.view.metadata.fx_send_level = None;
    old.view.metadata.fx_return_level = None;
    old.view.metadata.fx_loop_mode = None;
    let exported = NativeFile::preset_only(&old, None).unwrap();
    assert_eq!(NativeFile::parse(&exported).unwrap().raw(), &old.raw);
    old.view.metadata.native_style = Some(NativeStyleId::new(19).unwrap());
    assert!(NativeFile::preset_only(&old, None).is_err());
}
