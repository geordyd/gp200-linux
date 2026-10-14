use gp200_protocol::*;
use std::time::Duration;

fn fixture() -> RawTransfer {
    let mut decoder = StreamDecoder::new();
    let mut assembler = Reassembler::default();
    for event in decoder.feed(include_bytes!("fixtures/current-baseline.bin")) {
        if let StreamEvent::Packet(packet) = event
            && let Some(raw) = assembler.add(&packet, Duration::ZERO).unwrap()
        {
            return raw;
        }
    }
    panic!("fixture incomplete")
}

#[test]
fn named_save_and_selection_have_no_context_prefix() {
    assert_eq!(
        clear_stored_request(255).unwrap().payload(),
        &[
            0, 0, 4, 0, 1, 0, 255, 0, 1, 0, 4, 0, 255, 0, 255, 0, 9, 32, 4, 0, 255, 0, 0, 0
        ]
    );
    assert!(clear_stored_request(256).is_err());
    let name = ascii_name("ABCDEFGHIJKLMNOP").unwrap();
    let save = save_buffer_request(255, name).unwrap();
    assert_eq!(save.kind(), WRITE_KIND);
    assert_eq!(save.records().len(), 1);
    assert_eq!(save.records()[0].command, 0x2003);
    assert_eq!(
        save.records()[0].body,
        [vec![255, 0, 0, 0], name.to_vec()].concat()
    );
    let select = select_stored_request(255).unwrap();
    assert_eq!(select.payload(), &[8, 16, 4, 0, 0, 0, 255, 0]);
    assert!(select_stored_request(256).is_err());
    assert!(ascii_name("a longer than sixteen char name").is_err());
    assert!(ascii_name("é").is_err());
    assert!(ascii_name("line\nfeed").is_err());
}

#[test]
fn bypass_and_model_use_exact_distinct_layouts() {
    assert_eq!(enabled_write_record(10, true).unwrap().body, [10, 1, 0, 0]);
    assert_eq!(
        model_write_record(3, 0x07000015).unwrap().body,
        [3, 0, 0, 0, 0x15, 0, 0, 7]
    );
    assert!(enabled_write_record(11, false).is_err());
    assert!(model_write_record(11, 0).is_err());
    assert!(
        contextual_write_request(
            WireContext {
                group: 2,
                selector: 23
            },
            &[]
        )
        .is_err()
    );
}

#[test]
fn chain_and_metadata_builders_preserve_opaque_bytes_and_sparse_fields() {
    let mut record = RawRecord {
        command: 8,
        body: vec![23, 0, 0xab, 0xcd],
    };
    record.body.extend(0..11);
    record.body.push(0xef);
    let mut order = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10];
    order.swap(1, 2);
    let result = chain_write_record(&record, &order).unwrap();
    assert_eq!(&result.body[..4], &record.body[..4]);
    assert_eq!(result.body[15], 0xef);
    order[2] = 2;
    assert!(chain_write_record(&record, &order).is_err());
    assert_eq!(metadata_write_record(1, 121).unwrap().body, [1, 0, 121, 0]);
    assert!(metadata_write_record(7, 0).is_err());
}

#[test]
fn full_paste_retargets_only_established_slot_fields_and_has_no_file_crc() {
    let source = fixture();
    let mut malformed = source.records().to_vec();
    malformed
        .iter_mut()
        .find(|r| r.command == 0x14)
        .unwrap()
        .body[2] = 14;
    assert!(
        full_stored_write_request(
            23,
            &RawTransfer::from_records(WRITE_KIND, &malformed).unwrap()
        )
        .is_err()
    );
    let mut malformed = source.records().to_vec();
    let chain = malformed.iter_mut().find(|r| r.command == 8).unwrap();
    chain.body[4] = chain.body[5];
    assert!(
        full_stored_write_request(
            23,
            &RawTransfer::from_records(WRITE_KIND, &malformed).unwrap()
        )
        .is_err()
    );
    let request = full_stored_write_request(255, &source).unwrap();
    assert_eq!(request.payload().len(), 1184);
    assert_eq!(request.records().len(), 35);
    assert_eq!(request.records()[0].body, [1, 0, 255, 0]);
    assert_eq!(request.records()[1].body, [255, 0, 255, 0]);
    let originals: Vec<_> = source.records().iter().filter(|r| r.command != 0).collect();
    for (index, (a, b)) in originals.iter().zip(&request.records()[2..]).enumerate() {
        assert_eq!(a.command, b.command);
        let mut expected = a.body.clone();
        if index < 2 {
            expected[..2].copy_from_slice(&255_u16.to_le_bytes());
        }
        assert_eq!(b.body, expected);
    }
    let mut incomplete = source.records().to_vec();
    incomplete.pop();
    assert!(
        full_stored_write_request(
            23,
            &RawTransfer::from_records(WRITE_KIND, &incomplete).unwrap()
        )
        .is_err()
    );
    let mut unknown = source.records().to_vec();
    unknown.push(RawRecord {
        command: 0xeeee,
        body: vec![1],
    });
    assert!(
        full_stored_write_request(
            23,
            &RawTransfer::from_records(WRITE_KIND, &unknown).unwrap()
        )
        .is_err()
    );
}

#[test]
fn explicit_reply_chunk_size_preserves_framing_and_bounded_offsets() {
    let source = fixture();
    let packets = encode_packets_with_chunk_size(source.payload(), source.kind(), 185).unwrap();
    let decoded: Vec<_> = packets.iter().map(|p| decode_packet(p).unwrap()).collect();
    assert_eq!(
        decoded.iter().map(|p| p.offset).collect::<Vec<_>>(),
        [0, 185, 370, 555, 740, 925, 1110]
    );
    assert_eq!(
        decoded
            .iter()
            .flat_map(|p| p.payload.clone())
            .collect::<Vec<_>>(),
        source.payload()
    );
    assert_eq!(
        encode_packets(source.payload(), source.kind()).unwrap(),
        encode_packets_with_chunk_size(source.payload(), source.kind(), 183).unwrap()
    );
    assert!(encode_packets_with_chunk_size(source.payload(), source.kind(), 0).is_err());
    assert!(
        encode_packets_with_chunk_size(source.payload(), source.kind(), MAX_TRANSFER_LENGTH + 1)
            .is_err()
    );
}
