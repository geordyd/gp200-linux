use gp200_protocol::*;
use gp200_sim::*;

use std::time::Duration;

fn transact(pedal: &mut SimulatedPedal, request: &RawTransfer, time: u64) -> Vec<Delivery> {
    encode_transfer(request)
        .unwrap()
        .into_iter()
        .flat_map(|packet| pedal.receive(&packet, Duration::from_millis(time)).unwrap())
        .collect()
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

#[test]
fn native_endpoints_and_quick_refs_mutate_current_only_and_preserve_other_records() {
    let mut pedal = SimulatedPedal::captured();
    let before = pedal.current().clone();
    let stored = pedal.stored(23).unwrap().clone();
    let context = WireContext {
        group: 0,
        selector: 23,
    };
    let request = expression_endpoint_write_request(
        context,
        ExpressionSlotId::new(1, 0).unwrap(),
        ExpressionEndpoint::Maximum,
        F32Bits::from_f32(99.0),
        [0; 2],
    )
    .unwrap();
    transact(&mut pedal, &request, 0);
    let mut expected = before.records().to_vec();
    let record = expected
        .iter_mut()
        .find(|record| record.command == 12 && record.body[0] == 16)
        .unwrap();
    record.body[4..8].copy_from_slice(&99.0f32.to_le_bytes());
    assert_eq!(pedal.current().records(), expected);
    let quick = RawRecord {
        command: 16,
        body: vec![0, 12, 0, 0],
    };
    transact(
        &mut pedal,
        &patch_assignment_write_request(context, &quick).unwrap(),
        1,
    );
    *expected
        .iter_mut()
        .find(|record| record.command == 16 && record.body[0] == 0)
        .unwrap() = quick;
    assert_eq!(pedal.current().records(), expected);
    assert_eq!(pedal.stored(23), Some(&stored));
    assert_eq!(
        decode(&transact(&mut pedal, &read_current_request(), 2)),
        [pedal.current().clone()]
    );
}

#[test]
fn stale_context_and_off_endpoint_are_rejected_without_partial_changes() {
    let mut pedal = SimulatedPedal::captured();
    let before = pedal.current().clone();
    let request = expression_endpoint_write_request(
        WireContext {
            group: 0,
            selector: 255,
        },
        ExpressionSlotId::new(1, 0).unwrap(),
        ExpressionEndpoint::Maximum,
        99.0.into(),
        [0; 2],
    )
    .unwrap();
    for packet in encode_transfer(&request).unwrap() {
        assert!(pedal.receive(&packet, Duration::ZERO).is_err());
    }
    assert_eq!(pedal.current(), &before);
    let request = expression_endpoint_write_request(
        WireContext {
            group: 0,
            selector: 23,
        },
        ExpressionSlotId::new(2, 2).unwrap(),
        ExpressionEndpoint::Maximum,
        99.0.into(),
        [0; 2],
    )
    .unwrap();
    for packet in encode_transfer(&request).unwrap() {
        assert!(pedal.receive(&packet, Duration::ZERO).is_err());
    }
    assert_eq!(pedal.current(), &before);
}

#[test]
fn only_the_exact_native_full_c_clear_normalizes_parameter_255_to_zero() {
    let context = WireContext {
        group: 0,
        selector: 23,
    };
    for (parameter, maximum, minimum, expected_parameter) in [
        (255, 0u32, 0u32, 0u16),
        (255, 99.0f32.to_bits(), 0, 255),
        (255, 0, (-0.0f32).to_bits(), 255),
        (0x1234, 0, 0, 0x1234),
        (0, 100.0f32.to_bits(), 3.0f32.to_bits(), 0),
    ] {
        let mut pedal = SimulatedPedal::captured();
        let before = pedal.current().clone();
        let stored = pedal.stored(23).unwrap().clone();
        let globals = pedal.globals().clone();
        let record = ExpressionAssignmentRecord {
            slot: ExpressionSlotId::new(1, 0).unwrap(),
            module_raw: 0xff,
            parameter_raw: parameter,
            maximum: F32Bits(maximum),
            minimum: F32Bits(minimum),
        }
        .to_record();
        assert!(
            transact(
                &mut pedal,
                &patch_assignment_write_request(context, &record).unwrap(),
                0
            )
            .is_empty()
        );
        let mut expected = before.records().to_vec();
        let replacement = expected
            .iter_mut()
            .find(|r| r.command == 12 && r.body[0] == 16)
            .unwrap();
        *replacement = record;
        replacement.body[2..4].copy_from_slice(&expected_parameter.to_le_bytes());
        assert_eq!(pedal.current().records(), expected);
        assert_eq!(
            decode(&transact(&mut pedal, &read_current_request(), 1)),
            [pedal.current().clone()]
        );
        assert_eq!(pedal.stored(23), Some(&stored));
        assert_eq!(pedal.globals(), &globals);
    }
}
