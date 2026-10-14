use gp200_protocol::*;
use std::time::Duration;

const IDENTITY: &[u8] = include_bytes!("fixtures/identity-original.bin");
const CURRENT: &[u8] = include_bytes!("fixtures/current-original.bin");
const BASELINE_CURRENT: &[u8] = include_bytes!("fixtures/current-baseline.bin");
const STORED: &[u8] = include_bytes!("fixtures/stored-23-original.bin");
const NOTIFIED_IDENTITY: &[u8] = include_bytes!("fixtures/identity-with-notification.bin");

fn decode_capture(bytes: &[u8], callback_size: usize) -> Vec<RawTransfer> {
    let mut stream = StreamDecoder::default();
    let mut reassembly = Reassembler::default();
    let mut transfers = Vec::new();
    for (index, callback) in bytes.chunks(callback_size).enumerate() {
        for event in stream.feed(callback) {
            match event {
                StreamEvent::Packet(packet) => {
                    if let Some(transfer) = reassembly
                        .add(&packet, Duration::from_micros(index as u64))
                        .unwrap()
                    {
                        transfers.push(transfer);
                    }
                }
                StreamEvent::Realtime(_) | StreamEvent::OtherMidi(_) => (),
                unexpected => panic!("unexpected event {unexpected:?}"),
            }
        }
    }
    stream.finish().unwrap();
    assert!(reassembly.progress().is_none());
    transfers
}

fn packets(bytes: &[u8]) -> Vec<Packet> {
    StreamDecoder::default()
        .feed(bytes)
        .into_iter()
        .map(|event| {
            if let StreamEvent::Packet(packet) = event {
                packet
            } else {
                panic!("expected packet")
            }
        })
        .collect()
}

fn record_transfer(body: Vec<u8>) -> RawTransfer {
    RawTransfer::from_records(
        0x12,
        &[RawRecord {
            command: 0x9abc,
            body,
        }],
    )
    .unwrap()
}

#[test]
fn exact_identify_vector_and_actual_reply() {
    let exact_query = [
        0xf0, 0x21, 0x25, 0x7e, 0x47, 0x50, 0x2d, 0x32, 0x11, 0x04, 0x00, 0x00, 0x00, 0x00, 0x01,
        0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0xf7,
    ];
    let request = identify_request();
    assert_eq!(request.payload(), &[0x01, 0x20, 0, 0]);
    assert_eq!(
        encode_transfer(&request).unwrap(),
        vec![exact_query.to_vec()]
    );
    let reply = decode_capture(IDENTITY, 3).remove(0);
    assert_eq!(reply.payload(), &[1, 0x20, 4, 0, 1, 0, 2, 0]);
    assert_eq!(
        Identity::from_transfer(&reply),
        Some(Identity {
            state_raw: 1,
            model_raw: 2
        })
    );
    assert_eq!(
        Identity::from_transfer(&reply).unwrap().model(),
        WireDeviceModel::Gp200
    );
    assert_eq!(encode_transfer(&reply).unwrap(), vec![IDENTITY.to_vec()]);
}

#[test]
fn current_and_stored_requests_are_distinct_and_bounded() {
    assert_eq!(
        read_current_request().payload(),
        &[0, 0, 4, 0, 0, 0, 0, 0, 1, 0, 4, 0, 255, 255, 255, 255]
    );
    assert_eq!(
        read_stored_request(23).unwrap().payload(),
        &[0, 0, 4, 0, 1, 0, 23, 0, 1, 0, 4, 0, 23, 0, 23, 0]
    );
    assert_eq!(
        read_stored_request(255).unwrap().payload(),
        &[0, 0, 4, 0, 1, 0, 255, 0, 1, 0, 4, 0, 255, 0, 255, 0]
    );
    assert!(read_stored_request(0).is_ok());
    assert_eq!(
        read_stored_request(256),
        Err(ProtocolError::InvalidSlot(256))
    );
    assert_eq!(
        read_stored_request(u16::MAX),
        Err(ProtocolError::InvalidSlot(u16::MAX))
    );
}

#[test]
fn actual_185_byte_replies_preserve_raw_records_and_context() {
    for bytes in [CURRENT, STORED, BASELINE_CURRENT] {
        let chunks = packets(bytes);
        assert_eq!(
            chunks.iter().map(|p| p.payload.len()).collect::<Vec<_>>(),
            [185, 185, 185, 185, 185, 185, 66]
        );
        assert_eq!(
            chunks.iter().map(|p| p.offset).collect::<Vec<_>>(),
            [0, 185, 370, 555, 740, 925, 1110]
        );
        let raw = decode_capture(bytes, 11).remove(0);
        assert_eq!(raw.payload().len(), 1176);
        assert_eq!(
            raw.records()
                .iter()
                .filter(|record| record.command == 0x14)
                .count(),
            11
        );
        assert_eq!(
            RawTransfer::from_records(raw.kind(), raw.records()).unwrap(),
            raw
        );
        let encoded = encode_transfer(&raw).unwrap().concat();
        assert_eq!(decode_capture(&encoded, 7), [raw]);
    }
    assert_eq!(
        WireContext::from_transfer(&decode_capture(CURRENT, 1)[0]),
        Some(WireContext {
            group: 0,
            selector: 23
        })
    );
    assert_eq!(
        WireContext::from_transfer(&decode_capture(STORED, 1)[0]),
        Some(WireContext {
            group: 1,
            selector: 23
        })
    );
}

#[test]
fn unsolicited_parameter_does_not_satisfy_identity() {
    let transfers = decode_capture(NOTIFIED_IDENTITY, 2);
    assert_eq!(transfers.len(), 2);
    assert_eq!(transfers[0].records()[0].command, 5);
    assert_eq!(Identity::from_transfer(&transfers[0]), None);
    assert_eq!(
        Identity::from_transfer(&transfers[1]),
        Some(Identity {
            state_raw: 1,
            model_raw: 2
        })
    );
    // Even records preceding identity in the same payload cannot hide it.
    let joined = RawTransfer::from_records(
        0x12,
        &[
            transfers[0].records()[0].clone(),
            transfers[1].records()[0].clone(),
        ],
    )
    .unwrap();
    assert_eq!(
        Identity::from_transfer(&joined).unwrap().model(),
        WireDeviceModel::Gp200
    );
}

#[test]
fn independent_live_parameter_write_vectors_use_index_not_code() {
    let context = WireContext {
        group: 0,
        selector: 23,
    };
    for (gain, expected) in [
        (
            51.0,
            include_bytes!("fixtures/parameter-edit-request.bin").as_slice(),
        ),
        (
            50.0,
            include_bytes!("fixtures/parameter-restore-request.bin").as_slice(),
        ),
    ] {
        let request = parameter_write_request(
            context,
            ParameterWrite {
                module_id: 3,
                xml_param_id: 0,
                model_index: WireModelIndex(13),
                value: F32Bits::from_f32(gain),
            },
        )
        .unwrap();
        assert_eq!(encode_transfer(&request).unwrap().concat(), expected);
        assert_eq!(&request.records()[1].body[2..8], &[0, 0, 13, 0, 0, 0]);
    }
    let mut parameter = ParameterWrite {
        module_id: 11,
        xml_param_id: 0,
        model_index: WireModelIndex(13),
        value: F32Bits(0),
    };
    assert_eq!(parameter.to_record(), Err(ProtocolError::InvalidModule(11)));
    parameter.module_id = 3;
    parameter.xml_param_id = 15;
    assert_eq!(
        parameter.to_record(),
        Err(ProtocolError::InvalidParameter(15))
    );
}

#[test]
fn all_callback_boundaries_and_realtime_interleaving() {
    let expected = decode_capture(NOTIFIED_IDENTITY, NOTIFIED_IDENTITY.len());
    for split in 0..=NOTIFIED_IDENTITY.len() {
        let mut stream = StreamDecoder::default();
        let mut events = stream.feed(&NOTIFIED_IDENTITY[..split]);
        events.extend(stream.feed(&NOTIFIED_IDENTITY[split..]));
        stream.finish().unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, StreamEvent::Packet(_)))
                .count(),
            2
        );
    }
    let mut interleaved = vec![0x90, 60, 100];
    for byte in NOTIFIED_IDENTITY {
        interleaved.extend_from_slice(&[*byte, 0xf8, 0xfe, 0xff]);
    }
    assert_eq!(decode_capture(&interleaved, 1), expected);
}

#[test]
fn transmit_boundaries_all_byte_values_and_maximum_14bit_size() {
    for total in [4, 182, 183, 184, 256, 366, 367, MAX_TRANSFER_LENGTH] {
        let raw = record_transfer((0..total - 4).map(|i| i as u8).collect());
        assert_eq!(raw.payload().len(), total);
        let encoded = encode_transfer(&raw).unwrap();
        assert!(encoded.iter().all(|packet| packet.len() <= 380));
        assert!(
            encoded
                .iter()
                .all(|packet| packet[1..packet.len() - 1].iter().all(|byte| *byte <= 0x7f))
        );
        let mut assembler = Reassembler::default();
        let mut complete = None;
        for (index, bytes) in encoded.iter().rev().enumerate() {
            complete = assembler
                .add(
                    &decode_packet(bytes).unwrap(),
                    Duration::from_micros(index as u64),
                )
                .unwrap();
        }
        assert_eq!(complete, Some(raw));
    }
    assert_eq!(
        encode_packets(&[], 0x12),
        Err(ProtocolError::InvalidTotal(0))
    );
    assert_eq!(
        encode_packets(&vec![0; MAX_TRANSFER_LENGTH + 1], 0x12),
        Err(ProtocolError::InvalidTotal(MAX_TRANSFER_LENGTH + 1))
    );
    assert_eq!(
        encode_packets(&[0], 0x80),
        Err(ProtocolError::InvalidKind(0x80))
    );
}

#[test]
fn duplicate_overlap_is_idempotent_and_conflicts_are_atomic() {
    let raw = record_transfer(vec![42, 43, 44, 45]);
    let mut assembler = Reassembler::default();
    let packet = Packet {
        kind: 0x12,
        total: 8,
        offset: 2,
        payload: raw.payload()[2..6].to_vec(),
    };
    assert_eq!(assembler.add(&packet, Duration::ZERO).unwrap(), None);
    assert_eq!(
        assembler.add(&packet, Duration::from_millis(1)).unwrap(),
        None
    );
    assert_eq!(assembler.progress().unwrap().received, 4);
    // The first byte is new; a later overlapping byte conflicts. No mutation.
    let conflict = Packet {
        kind: 0x12,
        total: 8,
        offset: 1,
        payload: vec![0, 255],
    };
    assert_eq!(
        assembler.add(&conflict, Duration::from_millis(2)),
        Err(ProtocolError::ConflictingOverlap(2))
    );
    assert_eq!(assembler.progress().unwrap().received, 4);
    let first = Packet {
        kind: 0x12,
        total: 8,
        offset: 0,
        payload: raw.payload()[..4].to_vec(),
    };
    let last = Packet {
        kind: 0x12,
        total: 8,
        offset: 4,
        payload: raw.payload()[4..].to_vec(),
    };
    assert!(
        assembler
            .add(&first, Duration::from_millis(3))
            .unwrap()
            .is_none()
    );
    assert_eq!(
        assembler.add(&last, Duration::from_millis(4)).unwrap(),
        Some(raw)
    );
    assert!(assembler.progress().is_none());
}

#[test]
fn mismatch_never_implicitly_restarts_and_expiry_is_explicit() {
    let mut assembler = Reassembler::new(Duration::from_secs(1));
    let packet = Packet {
        kind: 0x12,
        total: 8,
        offset: 0,
        payload: vec![1, 2],
    };
    assembler.add(&packet, Duration::from_secs(10)).unwrap();
    let different = Packet {
        kind: 0x11,
        total: 4,
        offset: 0,
        payload: vec![1],
    };
    assert!(matches!(
        assembler.add(&different, Duration::from_millis(10001)),
        Err(ProtocolError::InterleavedTransfer { .. })
    ));
    assert_eq!(assembler.progress().unwrap().total, 8);
    assembler
        .add(&packet, Duration::from_millis(10999))
        .unwrap();
    assert_eq!(
        assembler.add(&packet, Duration::from_secs(11)),
        Err(ProtocolError::TransferExpired)
    );
    assert!(assembler.expire(Duration::from_millis(10999)).is_none());
    let expired = assembler.expire(Duration::from_secs(11)).unwrap();
    assert_eq!(expired.age, Duration::from_secs(1));
    assert_eq!(
        expired.incomplete,
        IncompleteTransfer {
            kind: 0x12,
            total: 8,
            received: 2
        }
    );
    assert!(assembler.progress().is_none());
}

#[test]
fn invalid_packets_never_allocate_or_overflow() {
    let mut assembler = Reassembler::default();
    for packet in [
        Packet {
            kind: 0x12,
            total: usize::MAX,
            offset: 0,
            payload: vec![1],
        },
        Packet {
            kind: 0x12,
            total: 2,
            offset: usize::MAX,
            payload: vec![1],
        },
        Packet {
            kind: 0x12,
            total: 2,
            offset: 1,
            payload: vec![1, 2],
        },
        Packet {
            kind: 0x12,
            total: 0,
            offset: 0,
            payload: vec![1],
        },
        Packet {
            kind: 0x12,
            total: 2,
            offset: 0,
            payload: vec![],
        },
        Packet {
            kind: 0x80,
            total: 2,
            offset: 0,
            payload: vec![1],
        },
    ] {
        assert!(assembler.add(&packet, Duration::ZERO).is_err());
        assert!(assembler.progress().is_none());
    }
    let packet = Packet {
        kind: 0x12,
        total: 8,
        offset: 0,
        payload: vec![1],
    };
    assembler.add(&packet, Duration::from_millis(100)).unwrap();
    assert_eq!(
        assembler.add(&packet, Duration::from_millis(99)),
        Err(ProtocolError::TimestampWentBackwards)
    );
}

#[test]
fn malformed_framing_nibbles_and_headers_are_rejected() {
    assert_eq!(
        decode_packet(&IDENTITY[..IDENTITY.len() - 1]),
        Err(ProtocolError::NotNormalPacket)
    );
    for index in 8..13 {
        let mut invalid = IDENTITY.to_vec();
        invalid[index] = 0x80;
        assert_eq!(
            decode_packet(&invalid),
            Err(ProtocolError::StatusInPacket(index))
        );
    }
    let mut odd = IDENTITY.to_vec();
    odd.remove(13);
    assert_eq!(decode_packet(&odd), Err(ProtocolError::OddNibbleCount));
    let mut invalid = IDENTITY.to_vec();
    invalid[13] = 16;
    assert_eq!(
        decode_packet(&invalid),
        Err(ProtocolError::InvalidNibble(13))
    );
    let mut zero = IDENTITY.to_vec();
    zero[9] = 0;
    zero[10] = 0;
    assert_eq!(decode_packet(&zero), Err(ProtocolError::InvalidTotal(0)));
    let mut out_of_bounds = IDENTITY.to_vec();
    out_of_bounds[11] = 1;
    assert!(matches!(
        decode_packet(&out_of_bounds),
        Err(ProtocolError::ChunkOutOfBounds { .. })
    ));
}

#[test]
fn special_and_foreign_sysex_are_separate_from_normal_records() {
    let short = [
        0xf0, 0x21, 0x25, 0x7e, 0x47, 0x50, 0x2d, 0x32, 0x11, 0x12, 0, 0, 0, 0xf7,
    ];
    let mut firmware = IDENTITY.to_vec();
    firmware[4] = 0x48;
    let mut stream = StreamDecoder::default();
    assert_eq!(
        stream.feed(&short),
        [StreamEvent::UnrecognizedSysex(short.to_vec())]
    );
    assert_eq!(
        stream.feed(&firmware),
        [StreamEvent::UnrecognizedSysex(firmware)]
    );
    assert!(matches!(
        decode_packet(&short),
        Err(ProtocolError::NotNormalPacket)
    ));
}

#[test]
fn stream_recovers_after_nested_status_and_oversized_messages() {
    let mut stream = StreamDecoder::default();
    assert!(stream.feed(&[0xf0, 0, 1]).is_empty());
    assert!(stream.is_incomplete());
    assert_eq!(
        stream.feed(&[0x90]),
        [
            StreamEvent::Error(ProtocolError::UnexpectedStatus(0x90)),
            StreamEvent::OtherMidi(0x90)
        ]
    );
    stream.feed(&[0xf0, 0, 1]);
    let nested = stream.feed(IDENTITY);
    assert!(matches!(
        nested[0],
        StreamEvent::Error(ProtocolError::NestedSysex)
    ));
    assert!(matches!(nested[1], StreamEvent::Packet(_)));
    stream.feed(&[0xf0]);
    let overflow = stream.feed(&vec![0; MAX_PACKET_LENGTH + 500]);
    assert_eq!(
        overflow,
        [StreamEvent::Error(ProtocolError::PacketTooLarge)]
    );
    assert_eq!(stream.buffered_len(), 0);
    assert!(stream.is_incomplete());
    assert!(stream.feed(&[0xf7]).is_empty());
    assert!(!stream.is_incomplete());
    assert!(matches!(stream.feed(IDENTITY)[0], StreamEvent::Packet(_)));
    stream.feed(&[0xf0]);
    assert_eq!(stream.finish(), Err(ProtocolError::IncompleteSysex));
    assert!(!stream.is_incomplete());
}

#[test]
fn unknown_records_nan_payloads_and_signed_zero_are_lossless() {
    let floats = [
        F32Bits(0x7fc0_1234),
        F32Bits(0x7f80_0000),
        F32Bits(0xff80_0000),
        F32Bits(0x8000_0000),
        F32Bits(0),
    ];
    let raw = RawTransfer::from_records(
        0x12,
        &[
            RawRecord {
                command: 0xf123,
                body: floats
                    .iter()
                    .flat_map(|float| float.to_le_bytes())
                    .collect(),
            },
            RawRecord {
                command: 0xf123,
                body: vec![],
            },
            RawRecord {
                command: 0xfffe,
                body: vec![0, 255, 17],
            },
        ],
    )
    .unwrap();
    let encoded = encode_transfer(&raw).unwrap().concat();
    assert_eq!(
        decode_capture(&encoded, 1).as_slice(),
        std::slice::from_ref(&raw)
    );
    assert_eq!(
        RawTransfer::from_payload(raw.kind(), raw.payload().to_vec()).unwrap(),
        raw
    );
    assert_eq!(floats[0].as_f32().to_bits(), floats[0].0);
}

#[test]
fn completed_bad_record_framing_resets_assembly() {
    for (payload, expected) in [
        (vec![1, 2, 3], RawError::TruncatedHeader(0)),
        (vec![1, 0, 2, 0, 255], RawError::TruncatedBody(0)),
    ] {
        let packet = Packet {
            kind: 0x12,
            total: payload.len(),
            offset: 0,
            payload,
        };
        let mut assembler = Reassembler::default();
        assert_eq!(
            assembler.add(&packet, Duration::ZERO),
            Err(ProtocolError::Records(expected))
        );
        assert!(assembler.progress().is_none());
    }
}

#[test]
fn malformed_identity_remains_raw_and_unknown_model_is_explicit() {
    let malformed = RawTransfer::from_records(
        0x12,
        &[RawRecord {
            command: 0x2001,
            body: vec![1, 0, 2],
        }],
    )
    .unwrap();
    assert!(Identity::from_transfer(&malformed).is_none());
    assert_eq!(malformed.records()[0].body, [1, 0, 2]);
    assert_eq!(
        Identity {
            state_raw: 99,
            model_raw: 500
        }
        .model(),
        WireDeviceModel::Unknown(500)
    );
    assert_eq!(
        Identity {
            state_raw: 99,
            model_raw: 3
        }
        .model(),
        WireDeviceModel::Gp200Lt
    );
    assert_eq!(
        Identity {
            state_raw: 99,
            model_raw: 4
        }
        .model(),
        WireDeviceModel::Gp200Jr
    );
}

#[test]
fn deterministic_malformed_stream_corpus_never_exceeds_retained_bounds() {
    // Dependency-free deterministic fuzz regression. Full fuzzing can reuse
    // these public pure entry points without MIDI, UI or filesystem setup.
    let mut seed = 0x8b12_54e7u32;
    for sample in 0..2000usize {
        let mut bytes = Vec::with_capacity(sample % 400);
        for _ in 0..sample % 400 {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            bytes.push(seed as u8);
        }
        let _ = decode_packet(&bytes);
        let mut stream = StreamDecoder::default();
        for callback in bytes.chunks(1 + sample % 31) {
            let _ = stream.feed(callback);
            assert!(stream.buffered_len() <= MAX_PACKET_LENGTH);
        }
        let _ = stream.finish();
        assert_eq!(stream.buffered_len(), 0);
    }
}
