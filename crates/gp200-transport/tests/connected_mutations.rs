use gp200_catalog::{Catalog, ControlKind};
use gp200_domain::*;
use gp200_protocol::{
    F32Bits, RawTransfer, Reassembler, StreamDecoder, StreamEvent, encode_transfer,
    full_stored_write_request,
};
use gp200_transport::{
    DeviceActor, TransportAction, TransportConfig, capabilities_for_mode, verified_capabilities,
};
use std::time::Duration;

const CURRENT: &[u8] = include_bytes!("../../gp200-protocol/tests/fixtures/current-baseline.bin");
fn raw(bytes: &[u8]) -> RawTransfer {
    let mut stream = StreamDecoder::new();
    let mut reassembler = Reassembler::default();
    for event in stream.feed(bytes) {
        if let StreamEvent::Packet(packet) = event
            && let Some(raw) = reassembler.add(&packet, Duration::ZERO).unwrap()
        {
            return raw;
        }
    }
    panic!("incomplete fixture");
}
fn actions(actor: &mut DeviceActor) -> Vec<TransportAction> {
    std::iter::from_fn(|| actor.next_action()).collect()
}
fn events(actions: Vec<TransportAction>) -> Vec<DeviceEvent> {
    actions
        .into_iter()
        .filter_map(|a| {
            if let TransportAction::Event(e) = a {
                Some(e)
            } else {
                None
            }
        })
        .collect()
}
fn stamp(actor: &DeviceActor, id: u64) -> RequestStamp {
    RequestStamp {
        epoch: actor.epoch(),
        intent: IntentId(id),
    }
}
fn receive(actor: &mut DeviceActor, transfer: &RawTransfer, ms: u64) {
    for packet in encode_transfer(transfer).unwrap() {
        actor.receive(actor.epoch(), &packet, Duration::from_millis(ms));
    }
}
fn ready(mode: BackendMode) -> DeviceActor {
    let mut actor = DeviceActor::with_mode(
        TransportConfig {
            minimum_request_spacing: Duration::ZERO,
            ..TransportConfig::default()
        },
        mode,
    );
    actor.command(
        DeviceCommand::Connect {
            intent: IntentId(1),
            selection: DeviceSelection {
                input_id: "fixture-in".into(),
                output_id: "fixture-out".into(),
                fingerprint_hint: None,
            },
        },
        Duration::ZERO,
    );
    actions(&mut actor);
    actor.opened(Ok(()), Duration::ZERO);
    actions(&mut actor);
    actor.sent(Ok(()), Duration::ZERO);
    let identity = RawTransfer::from_records(
        0x12,
        &[gp200_protocol::RawRecord {
            command: 0x2001,
            body: vec![1, 0, 2, 0],
        }],
    )
    .unwrap();
    receive(&mut actor, &identity, 1);
    actions(&mut actor);
    actor.sent(Ok(()), Duration::from_millis(2));
    receive(&mut actor, &raw(CURRENT), 3);
    actions(&mut actor);
    assert_eq!(actor.state(), ConnectionState::Ready);
    actor
}
fn wire(actions: Vec<TransportAction>) -> RawTransfer {
    let bytes: Vec<_> = actions
        .into_iter()
        .find_map(|a| {
            if let TransportAction::Send { packets } = a {
                Some(packets.concat())
            } else {
                None
            }
        })
        .expect("wire send");
    raw(&bytes)
}
fn submit(actor: &mut DeviceActor, patch: EditPatch) {
    actor.command(
        DeviceCommand::SubmitEdit {
            stamp: stamp(actor, 2),
            target: PresetTarget::EditBuffer,
            base_revision: actor.snapshot(PresetTarget::EditBuffer).unwrap().revision,
            patch,
        },
        Duration::from_millis(4),
    );
}
fn begin_readback(actor: &mut DeviceActor) {
    actor.sent(Ok(()), Duration::from_millis(5));
    actions(actor);
    actor.tick(Duration::from_millis(50));
    let request = wire(actions(actor));
    assert!(request.records().iter().any(|r| r.command == 1));
    actor.sent(Ok(()), Duration::from_millis(51));
}
fn retarget(source: &RawTransfer, group: u16, slot: u16) -> RawTransfer {
    let request = full_stored_write_request(slot, source).unwrap();
    let mut records = vec![request.records()[0].clone()];
    records[0].body[..2].copy_from_slice(&group.to_le_bytes());
    records.extend_from_slice(&request.records()[2..]);
    RawTransfer::from_records(source.kind(), &records).unwrap()
}
fn cache_stored(actor: &mut DeviceActor, slot: SlotId, source: &RawTransfer) {
    actor.command(
        DeviceCommand::ReadStored {
            stamp: stamp(actor, 9),
            slot,
        },
        Duration::from_millis(4),
    );
    actions(actor);
    actor.sent(Ok(()), Duration::from_millis(5));
    receive(actor, &retarget(source, 1, slot.get()), 6);
    actions(actor);
}
fn changed(
    source: &RawTransfer,
    change: impl FnOnce(&mut Vec<gp200_protocol::RawRecord>),
) -> RawTransfer {
    let mut records = source.records().to_vec();
    change(&mut records);
    RawTransfer::from_records(source.kind(), &records).unwrap()
}

#[test]
fn all_eleven_catalog_numeric_paths_send_xml_id_and_index_and_reject_wrong_index() {
    let catalog = Catalog::embedded().unwrap();
    for module_id in 0..11 {
        let mut actor = ready(BackendMode::Hardware);
        let module = actor
            .snapshot(PresetTarget::EditBuffer)
            .unwrap()
            .view
            .module(ModuleId::new(module_id).unwrap())
            .unwrap();
        let definition = catalog.model_by_id(module_id, module.model.0).unwrap();
        let parameter = definition
            .parameters
            .iter()
            .find(|p| {
                p.control_kind() == ControlKind::Numeric
                    && p.bound_parameter_id.is_none()
                    && p.minimum_is_off_raw.is_none()
                    && !p.raw_attributes.contains_key("MinISMute")
            })
            .unwrap();
        let (minimum, maximum, step) = parameter.numeric_bounds().unwrap();
        let value = (minimum + step).min(maximum);
        let patch = EditPatch::SetParameter {
            module: ModuleId::new(module_id).unwrap(),
            model: module.model,
            model_index: ModelIndex(definition.xml_index.into()),
            parameter: ParamId::new(parameter.parameter_id).unwrap(),
            value: F32Bits(value.to_bits()),
        };
        submit(&mut actor, patch.clone());
        let request = wire(actions(&mut actor));
        assert_eq!(
            request.records()[1].body[0..2],
            [module_id, parameter.parameter_id]
        );
        assert_eq!(
            u32::from_le_bytes(request.records()[1].body[4..8].try_into().unwrap()),
            u32::from(definition.xml_index)
        );
        let mut actor = ready(BackendMode::Hardware);
        let EditPatch::SetParameter {
            module,
            model,
            parameter,
            value,
            ..
        } = patch
        else {
            unreachable!()
        };
        submit(
            &mut actor,
            EditPatch::SetParameter {
                module,
                model,
                model_index: ModelIndex(u32::from(definition.xml_index) + 1),
                parameter,
                value,
            },
        );
        let effects = actions(&mut actor);
        assert!(
            !effects
                .iter()
                .any(|e| matches!(e, TransportAction::Send { .. }))
        );
        assert!(
            events(effects)
                .iter()
                .any(|e| matches!(e, DeviceEvent::OperationFailed { .. }))
        );
    }
}

#[test]
fn physical_fractional_position_remains_exact_and_numeric_rejects_nonfinite_or_outside_range() {
    for value in [f32::from_bits(0x42c6_b5c9), f32::NAN, -0.1, 100.1] {
        let mut actor = ready(BackendMode::Hardware);
        submit(
            &mut actor,
            EditPatch::SetParameter {
                module: ModuleId::new(1).unwrap(),
                model: ModelCode(0x0500000a),
                model_index: ModelIndex(2),
                parameter: ParamId::new(3).unwrap(),
                value: F32Bits(value.to_bits()),
            },
        );
        let effects = actions(&mut actor);
        if value.is_finite() && (0.0..=100.0).contains(&value) {
            assert_eq!(
                &wire(effects).records()[1].body[8..12],
                &value.to_le_bytes()
            );
        } else {
            assert!(
                !effects
                    .iter()
                    .any(|e| matches!(e, TransportAction::Send { .. }))
            );
        }
    }
}

#[test]
fn bypass_readback_preserves_every_unrelated_meaningful_byte() {
    let mut actor = ready(BackendMode::Hardware);
    let source = actor
        .snapshot(PresetTarget::EditBuffer)
        .unwrap()
        .raw
        .clone();
    submit(
        &mut actor,
        EditPatch::SetEnabled {
            module: ModuleId::new(4).unwrap(),
            enabled: true,
        },
    );
    let request = wire(actions(&mut actor));
    assert_eq!(request.records()[1].body, [4, 1, 0, 0]);
    begin_readback(&mut actor);
    let wrong = changed(&source, |records| {
        records
            .iter_mut()
            .find(|r| r.command == 0x14 && r.body[0] == 4)
            .unwrap()
            .body[1] = 1;
        records.iter_mut().find(|r| r.command == 2).unwrap().body[32] ^= 1;
    });
    receive(&mut actor, &wrong, 52);
    let result = events(actions(&mut actor));
    assert!(result.iter().any(|e| matches!(
        e,
        DeviceEvent::EditProgress {
            stage: EditStage::Failed(_),
            ..
        }
    )));
    assert!(
        result
            .iter()
            .any(|e| matches!(e, DeviceEvent::SnapshotReceived { .. }))
    );
    assert!(!result.iter().any(|e| matches!(
        e,
        DeviceEvent::EditProgress {
            stage: EditStage::Verified,
            ..
        }
    )));
}

#[test]
fn chain_reorder_keeps_mode_and_final_opaque_byte() {
    let mut actor = ready(BackendMode::Hardware);
    let source = actor
        .snapshot(PresetTarget::EditBuffer)
        .unwrap()
        .raw
        .clone();
    let original = source.records().iter().find(|r| r.command == 8).unwrap();
    submit(
        &mut actor,
        EditPatch::MoveModule {
            module: ModuleId::new(10).unwrap(),
            before: Some(ModuleId::new(0).unwrap()),
        },
    );
    let request = wire(actions(&mut actor));
    let chain = &request.records()[1];
    assert_eq!(&chain.body[..4], &original.body[..4]);
    assert_eq!(chain.body[15], original.body[15]);
    assert_eq!(chain.body[4], 10);
    begin_readback(&mut actor);
    let expected = changed(&source, |records| {
        *records.iter_mut().find(|r| r.command == 8).unwrap() = chain.clone()
    });
    receive(&mut actor, &expected, 52);
    assert!(events(actions(&mut actor)).iter().any(|e| matches!(
        e,
        DeviceEvent::EditProgress {
            stage: EditStage::Verified,
            ..
        }
    )));
}

#[test]
fn metadata_readback_preserves_strings_and_accepts_only_explicit_volatile_offsets() {
    let mut actor = ready(BackendMode::Hardware);
    let source = actor
        .snapshot(PresetTarget::EditBuffer)
        .unwrap()
        .raw
        .clone();
    submit(
        &mut actor,
        EditPatch::SetMetadata(MetadataPatch {
            name: Some("Test name".into()),
            tempo: Some(121),
            level: Some(81),
            ..Default::default()
        }),
    );
    let request = wire(actions(&mut actor));
    assert_eq!(
        request
            .records()
            .iter()
            .map(|r| r.command)
            .collect::<Vec<_>>(),
        [0, 7, 6, 6]
    );
    begin_readback(&mut actor);
    let expected = changed(&source, |records| {
        let meta = records.iter_mut().find(|r| r.command == 2).unwrap();
        meta.body[16..32].copy_from_slice(&gp200_protocol::ascii_name("Test name").unwrap());
        meta.body[2..4].copy_from_slice(&121_u16.to_le_bytes());
        meta.body[4..6].copy_from_slice(&81_u16.to_le_bytes());
        meta.body[15] ^= 1;
        records.iter_mut().find(|r| r.command == 8).unwrap().body[15] ^= 1;
        for r in records.iter_mut().filter(|r| r.command == 0xf) {
            r.body[2] ^= 1;
            r.body[3] ^= 1;
        }
    });
    receive(&mut actor, &expected, 52);
    assert!(events(actions(&mut actor)).iter().any(|e| matches!(
        e,
        DeviceEvent::EditProgress {
            stage: EditStage::Verified,
            ..
        }
    )));
    assert_eq!(
        &actor
            .snapshot(PresetTarget::EditBuffer)
            .unwrap()
            .raw
            .records()
            .iter()
            .find(|r| r.command == 2)
            .unwrap()
            .body[32..],
        &source
            .records()
            .iter()
            .find(|r| r.command == 2)
            .unwrap()
            .body[32..]
    );
}

#[test]
fn full_stored_write_requires_reviewed_target_revision_and_preserves_current_cache() {
    let slot = SlotId::new(255).unwrap();
    let mut actor = ready(BackendMode::Hardware);
    let source = actor.snapshot(PresetTarget::EditBuffer).unwrap().clone();
    actor.command(
        DeviceCommand::WriteStoredPreset {
            stamp: stamp(&actor, 2),
            slot,
            expected_target_revision: Revision(1),
            snapshot: source.clone(),
        },
        Duration::from_millis(4),
    );
    assert!(
        !actions(&mut actor)
            .iter()
            .any(|e| matches!(e, TransportAction::Send { .. }))
    );
    cache_stored(&mut actor, slot, &source.raw);
    actor.command(
        DeviceCommand::WriteStoredPreset {
            stamp: stamp(&actor, 3),
            slot,
            expected_target_revision: Revision(0),
            snapshot: source.clone(),
        },
        Duration::from_millis(7),
    );
    assert!(
        !actions(&mut actor)
            .iter()
            .any(|e| matches!(e, TransportAction::Send { .. }))
    );
    actor.command(
        DeviceCommand::WriteStoredPreset {
            stamp: stamp(&actor, 4),
            slot,
            expected_target_revision: Revision(1),
            snapshot: source.clone(),
        },
        Duration::from_millis(8),
    );
    let request = wire(actions(&mut actor));
    assert_eq!(request.payload().len(), 1184);
    begin_readback(&mut actor);
    receive(&mut actor, &retarget(&source.raw, 1, 255), 52);
    assert!(events(actions(&mut actor)).iter().any(|e| matches!(
        e,
        DeviceEvent::EditProgress {
            stage: EditStage::Verified,
            ..
        }
    )));
    assert_eq!(
        actor.snapshot(PresetTarget::EditBuffer).unwrap().raw,
        source.raw
    );
    let mut actor = ready(BackendMode::Hardware);
    let active = SlotId::new(23).unwrap();
    cache_stored(&mut actor, active, &source.raw);
    actor.command(
        DeviceCommand::WriteStoredPreset {
            stamp: stamp(&actor, 5),
            slot: active,
            expected_target_revision: Revision(1),
            snapshot: source,
        },
        Duration::from_millis(7),
    );
    assert!(
        !actions(&mut actor)
            .iter()
            .any(|e| matches!(e, TransportAction::Send { .. }))
    );
}

#[test]
fn audition_verifies_target_and_only_the_sampled_expression_overlay() {
    let slot = SlotId::new(255).unwrap();
    let mut actor = ready(BackendMode::Hardware);
    let source = actor
        .snapshot(PresetTarget::EditBuffer)
        .unwrap()
        .raw
        .clone();
    cache_stored(&mut actor, slot, &source);
    actor.command(
        DeviceCommand::SelectStored {
            stamp: stamp(&actor, 2),
            slot,
        },
        Duration::from_millis(7),
    );
    let request = wire(actions(&mut actor));
    assert_eq!(request.payload(), [8, 16, 4, 0, 0, 0, 255, 0]);
    begin_readback(&mut actor);
    let expected = retarget(&source, 0, 255);
    let actual = changed(&expected, |records| {
        records
            .iter_mut()
            .find(|r| r.command == 0x14 && r.body[0] == 10)
            .unwrap()
            .body[8..12]
            .copy_from_slice(&99.35505_f32.to_le_bytes())
    });
    receive(&mut actor, &actual, 52);
    let result = events(actions(&mut actor));
    assert!(result.iter().any(|e| matches!(
        e,
        DeviceEvent::EditProgress {
            stage: EditStage::Verified,
            ..
        }
    )));
    assert!(result.iter().any(
        |e| matches!(e,DeviceEvent::Diagnostic(d) if d.category=="runtime-expression-overlay")
    ));
    assert_eq!(
        actor.snapshot(PresetTarget::EditBuffer).unwrap().context,
        Some(WireContext {
            group: 0,
            selector: 255
        })
    );
}

#[test]
fn save_as_verifies_stored_then_reads_new_current_context_before_verified() {
    let slot = SlotId::new(255).unwrap();
    let mut actor = ready(BackendMode::Hardware);
    let source = actor.snapshot(PresetTarget::EditBuffer).unwrap().clone();
    cache_stored(&mut actor, slot, &source.raw);
    actor.command(
        DeviceCommand::SaveBufferTo {
            stamp: stamp(&actor, 2),
            slot,
            expected_buffer_revision: source.revision,
        },
        Duration::from_millis(7),
    );
    let request = wire(actions(&mut actor));
    assert_eq!(request.records().len(), 1);
    assert_eq!(request.records()[0].command, 0x2003);
    begin_readback(&mut actor);
    receive(&mut actor, &retarget(&source.raw, 1, 255), 52);
    let effects = actions(&mut actor);
    let result = events(
        effects
            .iter()
            .filter_map(|a| {
                if let TransportAction::Event(e) = a {
                    Some(TransportAction::Event(e.clone()))
                } else {
                    None
                }
            })
            .collect(),
    );
    assert!(result.iter().any(|e| matches!(
        e,
        DeviceEvent::SnapshotReceived {
            target: PresetTarget::Stored(_),
            ..
        }
    )));
    assert!(!result.iter().any(|e| matches!(
        e,
        DeviceEvent::EditProgress {
            stage: EditStage::Verified,
            ..
        }
    )));
    let current_request = wire(effects);
    assert_eq!(
        current_request.payload(),
        gp200_protocol::read_current_request().payload()
    );
    actor.sent(Ok(()), Duration::from_millis(53));
    receive(&mut actor, &retarget(&source.raw, 0, 255), 54);
    let result = events(actions(&mut actor));
    assert!(matches!(
        result[0],
        DeviceEvent::SnapshotReceived {
            target: PresetTarget::EditBuffer,
            ..
        }
    ));
    assert!(matches!(
        result[1],
        DeviceEvent::EditProgress {
            stage: EditStage::Verified,
            ..
        }
    ));
    assert_eq!(
        actor.snapshot(PresetTarget::EditBuffer).unwrap().context,
        Some(WireContext {
            group: 0,
            selector: 255
        })
    );
}

#[test]
fn simulator_and_replay_provenance_cannot_gain_hardware_permission() {
    for mode in [BackendMode::Simulator, BackendMode::CaptureReplay] {
        let mut actor = ready(mode);
        let snapshot = actor.snapshot(PresetTarget::EditBuffer).unwrap();
        assert!(!matches!(snapshot.source, SnapshotSource::Hardware { .. }));
        assert!(
            snapshot
                .evidence
                .iter()
                .all(|e| e.level != EvidenceLevel::LiveVerified)
        );
        let identity = DeviceIdentity {
            model: DeviceModel::Gp200,
            mode,
            firmware: None,
            display_name: "fixture".into(),
        };
        assert!(!verified_capabilities().allows(OperationKind::SetParameter, &identity));
        assert_eq!(
            capabilities_for_mode(mode).allows(OperationKind::SavePreset, &identity),
            mode == BackendMode::Simulator
        );
        if mode == BackendMode::CaptureReplay {
            submit(
                &mut actor,
                EditPatch::SetEnabled {
                    module: ModuleId::new(4).unwrap(),
                    enabled: true,
                },
            );
            assert!(
                !actions(&mut actor)
                    .iter()
                    .any(|e| matches!(e, TransportAction::Send { .. }))
            );
        }
    }
}

#[test]
fn nr_model_switch_resets_declared_defaults_and_other_models_require_separate_hardware_proof() {
    let mut actor = ready(BackendMode::Hardware);
    let source = actor
        .snapshot(PresetTarget::EditBuffer)
        .unwrap()
        .raw
        .clone();
    submit(
        &mut actor,
        EditPatch::SetModel {
            module: ModuleId::new(4).unwrap(),
            model: ModelCode(0x1d),
        },
    );
    assert_eq!(
        wire(actions(&mut actor)).records()[1].body,
        [4, 0, 0, 0, 0x1d, 0, 0, 0]
    );
    begin_readback(&mut actor);
    let expected = changed(&source, |records| {
        let nr = records
            .iter_mut()
            .find(|r| r.command == 0x14 && r.body[0] == 4)
            .unwrap();
        nr.body[4..8].copy_from_slice(&0x1d_u32.to_le_bytes());
        for (i, value) in [20_f32, 25., 60.].into_iter().enumerate() {
            nr.body[8 + i * 4..12 + i * 4].copy_from_slice(&value.to_le_bytes());
        }
    });
    receive(&mut actor, &expected, 52);
    assert!(events(actions(&mut actor)).iter().any(|e| matches!(
        e,
        DeviceEvent::EditProgress {
            stage: EditStage::Verified,
            ..
        }
    )));
    let mut actor = ready(BackendMode::Hardware);
    submit(
        &mut actor,
        EditPatch::SetModel {
            module: ModuleId::new(3).unwrap(),
            model: ModelCode(0x07000001),
        },
    );
    assert!(
        !actions(&mut actor)
            .iter()
            .any(|e| matches!(e, TransportAction::Send { .. }))
    );
    let mut actor = ready(BackendMode::Simulator);
    submit(
        &mut actor,
        EditPatch::SetModel {
            module: ModuleId::new(3).unwrap(),
            model: ModelCode(0x07000001),
        },
    );
    assert_eq!(wire(actions(&mut actor)).records()[1].command, 0x16);
}

#[test]
fn clear_verifies_observed_device_default_and_refuses_active_stored_target() {
    let mut actor = ready(BackendMode::Hardware);
    let before = actor
        .snapshot(PresetTarget::EditBuffer)
        .unwrap()
        .raw
        .clone();
    let slot = SlotId::new(255).unwrap();
    cache_stored(&mut actor, slot, &before);
    actor.command(
        DeviceCommand::ClearStoredPreset {
            stamp: stamp(&actor, 2),
            slot,
            expected_target_revision: Revision(1),
        },
        Duration::from_millis(7),
    );
    let request = wire(actions(&mut actor));
    assert_eq!(
        request
            .records()
            .iter()
            .map(|r| r.command)
            .collect::<Vec<_>>(),
        [0, 1, 0x2009]
    );
    begin_readback(&mut actor);
    let default = RawTransfer::from_payload(
        0x12,
        include_bytes!("../src/fixtures/device-cleared.payload.bin").to_vec(),
    )
    .unwrap();
    receive(&mut actor, &default, 52);
    assert!(events(actions(&mut actor)).iter().any(|e| matches!(
        e,
        DeviceEvent::EditProgress {
            stage: EditStage::Verified,
            ..
        }
    )));
    assert_eq!(
        actor.snapshot(PresetTarget::EditBuffer).unwrap().raw,
        before
    );
    let view = &actor.snapshot(PresetTarget::Stored(slot)).unwrap().view;
    assert_eq!(view.name.value().map(String::as_str), Some("It's GP-200"));
    assert_eq!(view.level.value(), Some(&50));
    let mut actor = ready(BackendMode::Hardware);
    let slot = SlotId::new(23).unwrap();
    cache_stored(&mut actor, slot, &before);
    actor.command(
        DeviceCommand::ClearStoredPreset {
            stamp: stamp(&actor, 2),
            slot,
            expected_target_revision: Revision(1),
        },
        Duration::from_millis(7),
    );
    assert!(
        !actions(&mut actor)
            .iter()
            .any(|e| matches!(e, TransportAction::Send { .. }))
    );
}

#[test]
fn conflicting_duplicate_reply_retires_session_and_cannot_verify_trailing_chunks() {
    let mut actor = ready(BackendMode::Hardware);
    let request_stamp = stamp(&actor, 2);
    actor.command(
        DeviceCommand::ReadCurrent(request_stamp),
        Duration::from_millis(4),
    );
    actions(&mut actor);
    actor.sent(Ok(()), Duration::from_millis(5));
    let packets = encode_transfer(&raw(CURRENT)).unwrap();
    actor.receive(actor.epoch(), &packets[0], Duration::from_millis(6));
    let old_epoch = actor.epoch();
    let mut conflicting = packets[0].clone();
    conflicting[15] ^= 1;
    actor.receive(old_epoch, &conflicting, Duration::from_millis(7));
    for packet in &packets[1..] {
        actor.receive(old_epoch, packet, Duration::from_millis(8));
    }
    let result = events(actions(&mut actor));
    assert!(result.iter().any(
        |e| matches!(e,DeviceEvent::OperationFailed{error,..}if error.kind==ErrorKind::Protocol)
    ));
    assert!(
        !result
            .iter()
            .any(|e| matches!(e, DeviceEvent::SnapshotReceived { .. }))
    );
    assert_eq!(actor.state(), ConnectionState::Recovering);
    assert!(actor.snapshot(PresetTarget::EditBuffer).is_none());
}

#[test]
fn save_followup_loss_is_indeterminate_and_never_replays_persistent_write() {
    let mut actor = ready(BackendMode::Hardware);
    let source = actor.snapshot(PresetTarget::EditBuffer).unwrap().clone();
    let slot = SlotId::new(255).unwrap();
    cache_stored(&mut actor, slot, &source.raw);
    actor.command(
        DeviceCommand::SaveBufferTo {
            stamp: stamp(&actor, 2),
            slot,
            expected_buffer_revision: source.revision,
        },
        Duration::from_millis(7),
    );
    actions(&mut actor);
    begin_readback(&mut actor);
    receive(&mut actor, &retarget(&source.raw, 1, 255), 52);
    actions(&mut actor);
    actor.lost(ServiceError::new(
        ErrorKind::Disconnected,
        "Lost before confirming selected context",
    ));
    let result = events(actions(&mut actor));
    assert!(result.iter().any(|e| matches!(
        e,
        DeviceEvent::EditProgress {
            stage: EditStage::Indeterminate(_),
            ..
        }
    )));
    actor.tick(Duration::from_secs(10));
    assert!(
        !actions(&mut actor)
            .iter()
            .any(|e| matches!(e, TransportAction::Send { .. }))
    );
}
