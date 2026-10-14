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
fn sync(enabled: bool, ordinary_restore: Option<F32Bits>) -> EditPatch {
    EditPatch::SetSync {
        module: ModuleId::new(7).unwrap(),
        model: ModelCode(0x04000015),
        model_index: ModelIndex(9),
        power_parameter: ParamId::new(2).unwrap(),
        value_parameter: ParamId::new(1).unwrap(),
        enabled,
        ordinary_restore,
    }
}

#[test]
fn sync_projection_preserves_unrelated_bytes_and_requires_ordinary_restore() {
    let baseline = baseline();
    let mut draft = PresetDraft::from_snapshot(&baseline);
    let before = draft.clone();
    assert!(draft.apply(&sync(false, None)).is_err());
    assert_eq!(draft, before);
    draft
        .apply(&sync(false, Some(F32Bits::from_f32(0.73))))
        .unwrap();
    let snapshot = draft
        .snapshot(&baseline, SnapshotSource::Simulator, Revision(1))
        .unwrap();
    for (old, new) in baseline.raw.records().iter().zip(snapshot.raw.records()) {
        if old.command == 0x14 && old.body[0] == 7 {
            assert_eq!(&old.body[..12], &new.body[..12]);
            assert_eq!(&new.body[12..16], &0.73f32.to_le_bytes());
            assert_eq!(&new.body[16..20], &0.0f32.to_le_bytes());
            assert_eq!(&old.body[20..], &new.body[20..]);
        } else {
            assert_eq!(old, new);
        }
    }
    draft.apply(&sync(true, None)).unwrap();
    let module = draft.view.module(ModuleId::new(7).unwrap()).unwrap();
    assert_eq!(module.parameters[1], F32Bits::from_f32(4.0));
    assert_eq!(module.parameters[2], F32Bits::from_f32(1.0));
}
