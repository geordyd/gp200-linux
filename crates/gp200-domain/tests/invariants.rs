use gp200_domain::*;
use std::sync::Arc;

fn snapshot() -> Arc<PresetSnapshot> {
    let mut module_body = vec![5, 1, 15, 0];
    module_body.extend_from_slice(&0x0a00_0010u32.to_le_bytes());
    for bits in [0x8000_0000, 0x7fc0_1234, 0x7f80_0000, 0x3f80_0000]
        .into_iter()
        .chain(std::iter::repeat_n(0, 11))
    {
        module_body.extend_from_slice(&u32::to_le_bytes(bits));
    }
    let raw = RawTransfer::from_records(
        0x12,
        &[
            RawRecord {
                command: 0,
                body: vec![0, 0, 23, 0],
            },
            RawRecord {
                command: 0xbeef,
                body: vec![0, 255, 42],
            },
            RawRecord {
                command: 0x14,
                body: module_body,
            },
        ],
    )
    .unwrap();
    Arc::new(PresetSnapshot::from_raw(
        Arc::new(raw),
        SnapshotSource::Simulator,
        Some(PresetTarget::EditBuffer),
        Revision(0),
    ))
}
#[test]
fn validates_ids_in_constructor_and_deserializer() {
    assert!(SlotId::new(256).is_err());
    assert!(ModuleId::new(11).is_err());
    assert!(ParamId::new(15).is_err());
    assert!(serde_json::from_str::<ModuleId>("11").is_err());
}
#[test]
fn snapshot_json_preserves_nonfinite_float_bits_and_unknown_records() {
    let original = snapshot();
    let json = serde_json::to_vec(&original).unwrap();
    let decoded: PresetSnapshot = serde_json::from_slice(&json).unwrap();
    assert_eq!(decoded, *original);
    assert_eq!(decoded.raw.payload(), original.raw.payload());
    assert_eq!(decoded.view.modules[0].parameters[1], F32Bits(0x7fc0_1234));
}
#[test]
fn full_width_name_does_not_consume_the_adjacent_vendor_field() {
    let mut body = vec![0; 48];
    body[16..32].copy_from_slice(b"1234567890ABCDEF");
    body[32..48].copy_from_slice(b"opaque-neighbor!");
    let raw = RawTransfer::from_records(0x12, &[RawRecord { command: 2, body }]).unwrap();
    let snapshot =
        PresetSnapshot::from_raw(Arc::new(raw), SnapshotSource::Simulator, None, Revision(0));
    assert_eq!(snapshot.view.display_name(), "1234567890ABCDEF");
    assert_eq!(&snapshot.raw.records()[0].body[32..48], b"opaque-neighbor!");
}
#[test]
fn draft_parameter_uses_xml_id_and_preserves_other_bytes() {
    let original = snapshot();
    let mut draft = PresetDraft::from_snapshot(&original);
    draft
        .apply(&EditPatch::SetParameter {
            module: ModuleId::new(5).unwrap(),
            model: ModelCode(0x0a00_0010),
            model_index: ModelIndex(16),
            parameter: ParamId::new(1).unwrap(),
            value: F32Bits::from_f32(60.0),
        })
        .unwrap();
    let changed = draft
        .snapshot(&original, SnapshotSource::Simulator, Revision(1))
        .unwrap();
    assert_eq!(changed.raw.records()[1], original.raw.records()[1]);
    assert_eq!(changed.view.modules[0].parameters[0], F32Bits(0x8000_0000));
    assert_eq!(changed.view.modules[0].parameters[1].as_f32(), 60.0);
}
#[test]
fn simulated_profiles_cannot_enable_hardware_or_other_models() {
    let caps = Capabilities::simulator();
    let identity = DeviceIdentity {
        model: DeviceModel::Gp200,
        mode: BackendMode::Hardware,
        firmware: None,
        display_name: String::new(),
    };
    assert!(!caps.allows(OperationKind::SetParameter, &identity));
    let other = DeviceIdentity {
        model: DeviceModel::Gp200Lt,
        mode: BackendMode::Simulator,
        ..identity
    };
    assert!(!caps.allows(OperationKind::SetParameter, &other));
}
#[test]
fn gesture_history_groups_changes_and_round_trips_assignments() {
    let baseline = snapshot();
    let mut doc = EditorDocument::new(
        DocumentId::from("offline"),
        EditTarget::Offline(DocumentId::from("offline")),
        baseline,
    );
    let key = GestureKey {
        module: ModuleId::new(5).unwrap(),
        model: ModelCode(0x0a00_0010),
        parameter: ParamId::new(1).unwrap(),
    };
    doc.begin_gesture(key.clone());
    for value in [30.0, 40.0, 60.0] {
        doc.apply(&EditPatch::SetParameter {
            module: key.module,
            model: key.model,
            model_index: ModelIndex(16),
            parameter: key.parameter,
            value: F32Bits::from_f32(value),
        })
        .unwrap();
    }
    doc.end_gesture(&key);
    assert_eq!(doc.undo.past.len(), 1);
    assert!(doc.dirty());
    assert!(doc.undo());
    assert!(!doc.dirty());
    assert!(doc.redo());
    let expression = ExpressionPatch {
        controller: 1,
        bank: 0,
        targets: vec![AssignmentTarget {
            module: key.module,
            model: key.model,
            parameter: key.parameter,
            minimum: F32Bits::from_f32(0.0),
            maximum: F32Bits::from_f32(100.0),
        }],
    };
    doc.apply(&EditPatch::SetExpression(expression.clone()))
        .unwrap();
    let changed = doc
        .draft
        .snapshot(&doc.baseline, SnapshotSource::Simulator, doc.local_revision)
        .unwrap();
    assert_eq!(
        PresetDraft::from_snapshot(&changed).expressions,
        vec![expression]
    );
}
