use gp200_domain::*;
use gp200_library::NativeFile;
use std::sync::Arc;

#[test]
fn offline_edits_export_all_established_fields_and_preserve_independent_native_bytes() {
    // Constructed by the independent Python fixture generator from captured
    // preset bytes; this is not advertised as an original vendor export.
    let original = include_bytes!("../../../tests/fixtures/native/constructed-mode1.prst");
    let file = NativeFile::parse(original).unwrap();
    let baseline = PresetSnapshot::from_raw(
        file.raw().clone(),
        SnapshotSource::Simulator,
        None,
        Revision(0),
    );
    let mut draft = PresetDraft::from_snapshot(&baseline);
    draft
        .apply(&EditPatch::SetMetadata(MetadataPatch {
            name: Some("Linux Editor".into()),
            tempo: Some(137),
            level: Some(61),
            ..Default::default()
        }))
        .unwrap();
    draft
        .apply(&EditPatch::MoveModule {
            module: ModuleId::new(10).unwrap(),
            before: Some(ModuleId::new(0).unwrap()),
        })
        .unwrap();
    draft
        .apply(&EditPatch::SetParameter {
            module: ModuleId::new(5).unwrap(),
            model: baseline
                .view
                .module(ModuleId::new(5).unwrap())
                .unwrap()
                .model,
            model_index: ModelIndex(0),
            parameter: ParamId::new(1).unwrap(),
            value: F32Bits::from_f32(61.0),
        })
        .unwrap();
    let edited = draft
        .snapshot(&baseline, SnapshotSource::Simulator, Revision(1))
        .unwrap();
    let output = NativeFile::preset_only(&edited, Some(file.header())).unwrap();
    let parsed = NativeFile::parse(&output).unwrap();
    let mut expected = original.to_vec();
    // Independent byte-offset oracle: preset-only MRAP payload starts at48.
    // Walk original TLVs; do not obtain offsets from the production parser.
    let mut cursor = 48;
    while cursor < original.len() - 8 {
        let command = u16::from_le_bytes(original[cursor..cursor + 2].try_into().unwrap());
        let length = usize::from(u16::from_le_bytes(
            original[cursor + 2..cursor + 4].try_into().unwrap(),
        ));
        let body = &mut expected[cursor + 4..cursor + 4 + length];
        match command {
            2 => {
                body[2..4].copy_from_slice(&137u16.to_le_bytes());
                body[4..6].copy_from_slice(&61u16.to_le_bytes());
                body[16..32].copy_from_slice(b"Linux Editor\0\0\0\0");
            }
            8 => body[4..15].copy_from_slice(&[10, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9]),
            0x14 if body[0] == 5 => body[12..16].copy_from_slice(&61f32.to_le_bytes()),
            _ => {}
        }
        cursor += 4 + length;
    }
    let end = expected.len() - 4;
    let sum: u32 = expected[..end].iter().map(|byte| u32::from(*byte)).sum();
    expected[end..].copy_from_slice(&sum.to_be_bytes());
    assert_eq!(output, expected);
    let restored = PresetSnapshot::from_raw(
        parsed.raw().clone(),
        SnapshotSource::Simulator,
        None,
        Revision(1),
    );
    assert_eq!(
        restored.view.name.value().map(String::as_str),
        Some("Linux Editor")
    );
    assert_eq!(restored.view.tempo.value(), Some(&137));
    assert_eq!(restored.view.level.value(), Some(&61));
    assert_eq!(restored.view.chain.value(), edited.view.chain.value());
    assert_eq!(restored.view.modules, edited.view.modules);
    assert_eq!(file.header(), parsed.header());
}
#[test]
fn unverified_local_fields_cannot_silently_disappear_from_native_exports() {
    let file = NativeFile::parse(include_bytes!(
        "../../../tests/fixtures/native/constructed-mode1.prst"
    ))
    .unwrap();
    let baseline = PresetSnapshot::from_raw(
        file.raw().clone(),
        SnapshotSource::Simulator,
        None,
        Revision(0),
    );
    let mut draft = PresetDraft::from_snapshot(&baseline);
    draft
        .apply(&EditPatch::SetMetadata(MetadataPatch {
            name: Some("音色".into()),
            ..Default::default()
        }))
        .unwrap();
    let snapshot = draft
        .snapshot(&baseline, SnapshotSource::Simulator, Revision(1))
        .unwrap();
    assert_eq!(snapshot.raw, baseline.raw);
    assert!(
        NativeFile::preset_only(&snapshot, Some(file.header()))
            .unwrap_err()
            .message
            .contains("preset name")
    );
    // Local archival serialization retains the requested text losslessly.
    let bytes = serde_json::to_vec(&snapshot).unwrap();
    let restored: PresetSnapshot = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(restored.view.name.value().map(String::as_str), Some("音色"));
    let _retained = Arc::new(restored);
}
