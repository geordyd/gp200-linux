use gp200_domain::{BackendMode, F32Bits};
use gp200_protocol::*;
use gp200_sim::*;
use sha2::{Digest, Sha256};
use std::time::Duration;

fn transact(pedal: &mut SimulatedPedal, request: &RawTransfer, time: u64) -> Vec<Delivery> {
    encode_transfer(request)
        .unwrap()
        .into_iter()
        .flat_map(|packet| pedal.receive(&packet, Duration::from_millis(time)).unwrap())
        .collect()
}
#[test]
fn asset_metadata_bank_matches_captured_ir_and_explicit_synthetic_snaptone_records() {
    let mut pedal = SimulatedPedal::captured();
    let captured = RawTransfer::from_payload(
        WRITE_KIND,
        include_bytes!("../../../tests/fixtures/device-assets/connected-ir-metadata.payload.bin")
            .to_vec(),
    )
    .unwrap();
    let before = pedal.current().clone();
    for index in 0..20 {
        let slot = AssetSlot::new(AssetBank::Ir, index).unwrap();
        let decoded = decode(&transact(
            &mut pedal,
            &asset_metadata_read_candidate(slot),
            u64::from(index),
        ));
        assert_eq!(decoded.len(), 1);
        assert_eq!(
            decoded[0].records(),
            &captured.records()[usize::from(index)..usize::from(index) + 1]
        );
    }
    for index in 0..10 {
        let slot = AssetSlot::new(AssetBank::SnapTone, index).unwrap();
        let decoded = decode(&transact(
            &mut pedal,
            &asset_metadata_read_candidate(slot),
            21 + u64::from(index),
        ));
        assert_eq!(
            AssetSlotMetadata::decode(AssetBank::SnapTone, &decoded[0].records()[0])
                .unwrap()
                .display_name(),
            format!("Demo Snap {}", index + 1)
        );
    }
    assert_eq!(pedal.current(), &before);
}
#[test]
fn asset_query_rejects_malformed_and_noncanonical_tail_without_mutation() {
    let mut pedal = SimulatedPedal::captured();
    let before = pedal.current().clone();
    let baseline = asset_metadata_read_candidate(AssetSlot::new(AssetBank::Ir, 0).unwrap());
    for body in [
        vec![0; 23],
        {
            let mut body = baseline.records()[0].body.clone();
            body[10] = 1;
            body
        },
        {
            let mut body = baseline.records()[0].body.clone();
            body[..2].copy_from_slice(&20_u16.to_le_bytes());
            body
        },
    ] {
        let request = RawTransfer::from_records(
            READ_KIND,
            &[RawRecord {
                command: 0x1009,
                body,
            }],
        )
        .unwrap();
        let result = encode_transfer(&request)
            .unwrap()
            .into_iter()
            .try_for_each(|packet| pedal.receive(&packet, Duration::ZERO).map(|_| ()));
        assert!(result.is_err());
        assert_eq!(pedal.current(), &before);
        pedal.reset_stream();
    }
}
fn decode(deliveries: &[Delivery]) -> Vec<RawTransfer> {
    let mut decoder = StreamDecoder::new();
    let mut assembler = Reassembler::default();
    let mut result = Vec::new();
    for delivery in deliveries {
        if let Delivery::Bytes { at, bytes } = delivery {
            for event in decoder.feed(bytes) {
                match event {
                    StreamEvent::Packet(packet)
                        if packet.offset == 0 && packet.payload.len() == packet.total =>
                    {
                        result.push(RawTransfer::from_payload(packet.kind, packet.payload).unwrap())
                    }
                    StreamEvent::Packet(packet) => {
                        if let Some(raw) = assembler.add(&packet, *at).unwrap() {
                            result.push(raw);
                        }
                    }
                    StreamEvent::Error(error) => panic!("{error}"),
                    _ => {}
                }
            }
        }
    }
    result
}
fn parameter(pedal: &SimulatedPedal, module: u8, id: usize) -> F32Bits {
    let record = pedal
        .current()
        .records()
        .iter()
        .find(|record| record.command == 0x14 && record.body[0] == module)
        .unwrap();
    F32Bits::from_le_bytes(record.body[8 + id * 4..12 + id * 4].try_into().unwrap())
}
fn gain(bits: F32Bits) -> RawTransfer {
    parameter_write_request(
        WireContext {
            group: 0,
            selector: 23,
        },
        ParameterWrite {
            module_id: 3,
            xml_param_id: 0,
            model_index: WireModelIndex(13),
            value: bits,
        },
    )
    .unwrap()
}
#[test]
fn owned_fixture_hashes_and_all_256_raw_slots_are_exact() {
    let manifest: serde_json::Value =
        serde_json::from_str(include_str!("../../../tests/fixtures/sim/manifest.json")).unwrap();
    let bank = include_bytes!("../../../tests/fixtures/sim/gp200-bank.bin");
    assert_eq!(
        format!("{:x}", Sha256::digest(bank)),
        manifest["bank_sha256"].as_str().unwrap()
    );
    let mut pedal = SimulatedPedal::captured();
    let current = pedal.current().clone();
    assert_eq!(
        format!("{:x}", Sha256::digest(current.payload())),
        manifest["current_sha256"].as_str().unwrap()
    );
    assert_eq!(
        format!(
            "{:x}",
            Sha256::digest(include_bytes!(
                "../../../tests/fixtures/sim/device-cleared.payload.bin"
            ))
        ),
        manifest["device_clear"]["sha256"].as_str().unwrap()
    );
    for (slot, payload) in bank.as_chunks::<1176>().0.iter().enumerate() {
        assert_eq!(
            format!("{:x}", Sha256::digest(payload)),
            manifest["slots"][slot]["sha256"].as_str().unwrap()
        );
        let reply = decode(&transact(
            &mut pedal,
            &read_stored_request(slot as u16).unwrap(),
            slot as u64,
        ));
        assert_eq!(reply.len(), 1);
        assert_eq!(reply[0].payload(), payload);
        assert_eq!(
            WireContext::from_transfer(&reply[0]),
            Some(WireContext {
                group: 1,
                selector: slot as u16
            })
        );
    }
    assert_eq!(pedal.current(), &current);
    assert_eq!(pedal.selected_slot(), 23);
}
#[test]
fn real_encoder_decoder_and_observed_185_byte_reply_chunking_work() {
    let mut pedal = SimulatedPedal::captured();
    let identity = decode(&transact(&mut pedal, &identify_request(), 0));
    assert_eq!(
        Identity::from_transfer(&identity[0]),
        Some(Identity {
            state_raw: 1,
            model_raw: 2
        })
    );
    let deliveries = transact(&mut pedal, &read_current_request(), 1);
    assert_eq!(deliveries.len(), 7);
    if let Delivery::Bytes { bytes, .. } = &deliveries[0] {
        assert_eq!(decode_packet(bytes).unwrap().payload.len(), 185);
    }
    assert_eq!(decode(&deliveries), vec![pedal.current().clone()]);
}
#[test]
fn stored_preview_between_edits_cannot_redirect_buffer_context_or_bits() {
    let mut pedal = SimulatedPedal::captured();
    let stored = pedal.stored(23).unwrap().clone();
    transact(&mut pedal, &gain(F32Bits::from_f32(51.0)), 0);
    transact(&mut pedal, &read_stored_request(255).unwrap(), 1);
    transact(&mut pedal, &gain(F32Bits::from_f32(52.0)), 2);
    assert_eq!(parameter(&pedal, 3, 0), F32Bits::from_f32(52.0));
    assert_eq!(pedal.stored(23), Some(&stored));
    assert_eq!(
        WireContext::from_transfer(pedal.current()),
        Some(WireContext {
            group: 0,
            selector: 23
        })
    );
}
#[test]
fn parameter_xml_index_is_distinct_from_model_code_and_writes_are_atomic() {
    let mut pedal = SimulatedPedal::captured();
    let original = pedal.current().clone();
    let wrong = parameter_write_request(
        WireContext {
            group: 0,
            selector: 23,
        },
        ParameterWrite {
            module_id: 3,
            xml_param_id: 0,
            model_index: WireModelIndex(0x0700_0015),
            value: F32Bits::from_f32(51.0),
        },
    )
    .unwrap();
    let packet = encode_transfer(&wrong).unwrap().remove(0);
    assert!(pedal.receive(&packet, Duration::ZERO).is_err());
    assert_eq!(pedal.current(), &original);
    let mut records = gain(F32Bits::from_f32(51.0)).records().to_vec();
    records.push(RawRecord {
        command: 0xbeef,
        body: vec![1],
    });
    let request = RawTransfer::from_records(WRITE_KIND, &records).unwrap();
    assert!(
        pedal
            .receive(&encode_transfer(&request).unwrap()[0], Duration::ZERO)
            .is_err()
    );
    assert_eq!(pedal.current(), &original);
}
#[test]
fn raw_notification_can_change_and_restore_off_grid_expression_value() {
    let mut pedal = SimulatedPedal::captured();
    let original = parameter(&pedal, 1, 3);
    let off_grid = F32Bits::from_f32(99.355);
    let delivery = pedal
        .external_parameter_edit(1, 3, off_grid, Duration::ZERO)
        .unwrap();
    assert_eq!(parameter(&pedal, 1, 3), off_grid);
    assert_eq!(decode(&delivery)[0].records()[0].command, 5);
    pedal
        .external_parameter_edit(1, 3, original, Duration::ZERO)
        .unwrap();
    assert_eq!(parameter(&pedal, 1, 3), original);
}
#[test]
fn physical_expression_overlay_changes_selected_buffer_without_changing_stored_data() {
    let mut pedal = SimulatedPedal::captured();
    let original = pedal.stored(255).unwrap().clone();
    pedal
        .set_expression_overlays(vec![ExpressionOverlay {
            selected_slot: Some(255),
            module: 10,
            parameter: 0,
            value: F32Bits::from_f32(99.355),
        }])
        .unwrap();
    transact(&mut pedal, &select_stored_request(255).unwrap(), 0);
    assert_eq!(pedal.selected_slot(), 255);
    assert_eq!(parameter(&pedal, 10, 0), F32Bits::from_f32(99.355));
    assert_eq!(pedal.stored(255), Some(&original));
    assert_ne!(pedal.current().payload(), original.payload());
}
#[test]
fn simulated_save_and_full_write_affect_only_the_explicit_stored_target() {
    let mut pedal = SimulatedPedal::captured();
    let before = pedal.stored(23).unwrap().clone();
    transact(&mut pedal, &gain(F32Bits::from_f32(51.0)), 0);
    transact(
        &mut pedal,
        &save_buffer_request(255, ascii_name("Simulated Save").unwrap()).unwrap(),
        1,
    );
    assert_eq!(pedal.stored(23), Some(&before));
    assert_eq!(pedal.selected_slot(), 255);
    assert_eq!(
        WireContext::from_transfer(pedal.current()),
        Some(WireContext {
            group: 0,
            selector: 255
        })
    );
    let saved = pedal.stored(255).unwrap().clone();
    let metadata = saved
        .records()
        .iter()
        .find(|record| record.command == 2)
        .unwrap();
    assert_eq!(
        &metadata.body[16..32],
        &ascii_name("Simulated Save").unwrap()
    );
    let request = full_stored_write_request(254, &saved).unwrap();
    transact(&mut pedal, &request, 2);
    assert_eq!(
        WireContext::from_transfer(pedal.stored(254).unwrap()),
        Some(WireContext {
            group: 1,
            selector: 254
        })
    );
    assert_eq!(pedal.stored(255), Some(&saved));
}
#[test]
fn arbitrary_callbacks_realtime_reverse_and_duplicate_packets_reassemble_identically() {
    let mut pedal = SimulatedPedal::captured();
    pedal.profile.callback_chunk_size = 7;
    pedal.profile.realtime_byte = Some(0xf8);
    pedal
        .queue_fault(ReplyFault {
            reverse_packets: true,
            duplicate_packet: Some(3),
            ..Default::default()
        })
        .unwrap();
    let deliveries = transact(&mut pedal, &read_current_request(), 0);
    assert_eq!(decode(&deliveries), vec![pedal.current().clone()]);
}
#[test]
fn interleaved_unsolicited_transfer_does_not_replace_a_multipart_reply() {
    let mut pedal = SimulatedPedal::captured();
    pedal
        .queue_fault(ReplyFault {
            unsolicited_after_packet: Some(2),
            ..Default::default()
        })
        .unwrap();
    let decoded = decode(&transact(&mut pedal, &read_current_request(), 0));
    assert_eq!(decoded.len(), 2);
    assert_eq!(decoded[0].records()[0].command, 5);
    assert_eq!(decoded[1], *pedal.current());
}
#[test]
fn dropped_packet_expires_and_cancel_discards_incomplete_request() {
    let mut pedal = SimulatedPedal::captured();
    pedal
        .queue_fault(ReplyFault {
            drop_packet: Some(2),
            ..Default::default()
        })
        .unwrap();
    let mut assembler = Reassembler::new(Duration::from_secs(1));
    for delivery in transact(&mut pedal, &read_current_request(), 0) {
        if let Delivery::Bytes { bytes, .. } = delivery {
            assert!(
                assembler
                    .add(&decode_packet(&bytes).unwrap(), Duration::ZERO)
                    .unwrap()
                    .is_none()
            );
        }
    }
    assert!(assembler.expire(Duration::from_secs(1)).is_some());
    let large = full_stored_write_request(255, pedal.current()).unwrap();
    let packets = encode_transfer(&large).unwrap();
    pedal.receive(&packets[0], Duration::ZERO).unwrap();
    pedal.reset_stream();
    let identity = decode(&transact(&mut pedal, &identify_request(), 1));
    assert!(Identity::from_transfer(&identity[0]).is_some());
}
#[test]
fn scenario_logs_and_fault_outputs_are_deterministic_and_declared_simulation() {
    let run = || {
        let mut pedal = SimulatedPedal::captured();
        pedal.profile.latency_ms = 25;
        pedal.profile.inter_packet_ms = 2;
        pedal
            .queue_fault(ReplyFault {
                duplicate_packet: Some(2),
                ..Default::default()
            })
            .unwrap();
        let delivery = transact(&mut pedal, &read_current_request(), 100);
        (delivery, pedal.log().clone())
    };
    let first = run();
    assert_eq!(first, run());
    assert_eq!(first.1.backend_mode, BackendMode::Simulator);
    assert!(first.1.timing.contains("Synthetic"));
    assert_eq!(first.0[0].at(), Duration::from_millis(125));
}

#[test]
fn clear_uses_actual_device_defaults_and_does_not_modify_current_buffer() {
    let mut pedal = SimulatedPedal::captured();
    let current = pedal.current().clone();
    transact(&mut pedal, &clear_stored_request(255).unwrap(), 0);
    let cleared = pedal.stored(255).unwrap();
    let body = &cleared
        .records()
        .iter()
        .find(|record| record.command == 2)
        .unwrap()
        .body;
    assert_eq!(&body[16..32], &ascii_name("It's GP-200").unwrap());
    assert_eq!(u16::from_le_bytes(body[4..6].try_into().unwrap()), 50);
    assert_eq!(u16::from_le_bytes(body[2..4].try_into().unwrap()), 120);
    assert_eq!(pedal.current(), &current);
}

#[test]
fn recovered_nr_model_change_overlays_defaults_and_preserves_assignment_records() {
    let mut pedal = SimulatedPedal::captured();
    let original = pedal.current().clone();
    let request = contextual_write_request(
        WireContext {
            group: 0,
            selector: 23,
        },
        &[model_write_record(4, 0x1d).unwrap()],
    )
    .unwrap();
    transact(&mut pedal, &request, 0);
    let nr = pedal
        .current()
        .records()
        .iter()
        .find(|record| record.command == 0x14 && record.body[0] == 4)
        .unwrap();
    assert_eq!(&nr.body[4..8], &0x1du32.to_le_bytes());
    for (id, value) in [20f32, 25f32, 60f32].into_iter().enumerate() {
        assert_eq!(&nr.body[8 + 4 * id..12 + 4 * id], &value.to_le_bytes());
    }
    let before_nr = original
        .records()
        .iter()
        .find(|record| record.command == 0x14 && record.body[0] == 4)
        .unwrap();
    assert_eq!(&nr.body[20..68], &before_nr.body[20..68]);
    let assignments = |raw: &RawTransfer| {
        raw.records()
            .iter()
            .filter(|record| matches!(record.command, 0xc | 0x10 | 0xf))
            .cloned()
            .collect::<Vec<_>>()
    };
    assert_eq!(assignments(pedal.current()), assignments(&original));
}
