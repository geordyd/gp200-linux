use gp200_protocol::*;

#[test]
fn connected_all20_ir_metadata_records_are_exact_and_do_not_imply_content_backup() {
    let payload =
        include_bytes!("../../../tests/fixtures/device-assets/connected-ir-metadata.payload.bin");
    let transfer = RawTransfer::from_payload(WRITE_KIND, payload.to_vec()).unwrap();
    assert_eq!(transfer.records().len(), 20);
    for (index, record) in transfer.records().iter().enumerate() {
        let metadata = AssetSlotMetadata::decode(AssetBank::Ir, record).unwrap();
        assert_eq!(metadata.slot.index() as usize, index);
        assert_eq!(metadata.raw(), record);
        assert_eq!(metadata.raw().body.len(), 24);
        assert_eq!(metadata.flags_raw, 0);
        assert_eq!(metadata.checksum_raw, 0);
        // Native NUL name tail includes non-ASCII opaque bytes. Keep all16.
        assert_eq!(metadata.display_name(), "User IR");
        assert_eq!(metadata.name_raw[7], 0);
        assert_eq!(metadata.name_raw[12..], [0xff; 4]);
    }
    assert_eq!(transfer.payload(), payload);
}

fn metadata(bank: AssetBank, slot: u16) -> RawRecord {
    let mut body = vec![0_u8; 24];
    body[..2].copy_from_slice(&slot.to_le_bytes());
    body[2..4].copy_from_slice(&0x9a7b_u16.to_le_bytes());
    body[4..8].copy_from_slice(&0x12345678_u32.to_le_bytes());
    body[8..24].copy_from_slice(b"RAW\0\xffabcdefghijk");
    RawRecord {
        command: bank.metadata_command(),
        body,
    }
}
#[test]
fn exact_metadata_reply_preserves_unknown_flags_name_tail_and_checksum() {
    for bank in [AssetBank::Ir, AssetBank::SnapTone] {
        let record = metadata(bank, bank.slots() - 1);
        let decoded = AssetSlotMetadata::decode(bank, &record).unwrap();
        assert_eq!(decoded.raw(), &record);
        assert_eq!(decoded.flags_raw, 0x9a7b);
        assert_eq!(decoded.checksum_raw, 0x12345678);
        assert_eq!(decoded.display_name(), "RAW");
        assert_eq!(decoded.name_raw[4], 0xff);
        assert!(
            AssetSlotMetadata::decode(
                if bank == AssetBank::Ir {
                    AssetBank::SnapTone
                } else {
                    AssetBank::Ir
                },
                &record
            )
            .is_err()
        );
        for length in [0, 4, 20, 23, 25] {
            let mut broken = record.clone();
            broken.body.resize(length, 0);
            assert!(AssetSlotMetadata::decode(bank, &broken).is_err());
        }
    }
}
#[test]
fn slot_constructor_and_reply_decoder_reject_invalid_targets() {
    for bank in [AssetBank::Ir, AssetBank::SnapTone] {
        assert!(AssetSlot::new(bank, 0).is_ok());
        assert!(AssetSlot::new(bank, bank.slots() - 1).is_ok());
        assert!(AssetSlot::new(bank, bank.slots()).is_err());
        let mut record = metadata(bank, bank.slots());
        assert!(AssetSlotMetadata::decode(bank, &record).is_err());
        record.body[..2].copy_from_slice(&u16::MAX.to_le_bytes());
        assert!(AssetSlotMetadata::decode(bank, &record).is_err());
    }
}
#[test]
fn metadata_candidate_query_has_exact_traced_shape_and_no_preset_context() {
    let slot = AssetSlot::new(AssetBank::Ir, 19).unwrap();
    let transfer = asset_metadata_read_candidate(slot);
    assert_eq!(transfer.kind(), READ_KIND);
    assert_eq!(transfer.payload().len(), 28);
    assert_eq!(transfer.records().len(), 1);
    assert_eq!(transfer.records()[0].command, 0x1009);
    assert_eq!(&transfer.records()[0].body[..4], &[19, 0, 1, 0]);
    assert_eq!(&transfer.records()[0].body[4..], &[0; 20]);
    assert!(WireContext::from_transfer(&transfer).is_none());
}
#[test]
fn rename_candidate_uses_exact_bank_slot_and_retained_alignment_word() {
    let record = metadata(AssetBank::SnapTone, 9);
    let baseline = AssetSlotMetadata::decode(AssetBank::SnapTone, &record).unwrap();
    let transfer = asset_rename_candidate(&baseline, "Linux IR").unwrap();
    assert_eq!(transfer.kind(), WRITE_KIND);
    assert_eq!(transfer.records()[0].command, 0x1015);
    assert_eq!(transfer.records()[0].body.len(), 20);
    assert_eq!(&transfer.records()[0].body[..4], &record.body[..4]);
    assert_eq!(&transfer.records()[0].body[4..12], b"Linux IR");
    assert_eq!(&transfer.records()[0].body[12..], &[0; 8]);
    assert_eq!(baseline.raw(), &record);
    let reply = AssetRenameReply::decode(AssetBank::SnapTone, &transfer.records()[0]).unwrap();
    assert_eq!(reply.raw(), &transfer.records()[0]);
    for name in ["", "   ", "01234567890123456", "é", "tab\t", "~"] {
        assert!(asset_rename_candidate(&baseline, name).is_err());
    }
}
#[test]
fn bulk_candidate_matches_independent_ir_fixture_and_checksum() {
    let packed =
        include_bytes!("../../../tests/fixtures/device-assets/synthetic-ir-coefficients.bin");
    let transfer = asset_upload_candidate(
        AssetSlot::new(AssetBank::Ir, 19).unwrap(),
        "Impulse",
        packed,
    )
    .unwrap();
    let record = &transfer.records()[0];
    assert_eq!(record.command, 0x100a);
    assert_eq!(record.body.len(), 0x1018);
    assert_eq!(&record.body[..4], &[19, 0, 0, 0]);
    assert_eq!(&record.body[4..8], &2041_u32.to_le_bytes());
    assert_eq!(&record.body[24..], packed);
    let frames = encode_transfer(&transfer).unwrap();
    let mut assembler = Reassembler::default();
    let mut rebuilt = None;
    for frame in frames {
        if let Some(value) = assembler
            .add(&decode_packet(&frame).unwrap(), std::time::Duration::ZERO)
            .unwrap()
        {
            rebuilt = Some(value);
        }
    }
    assert_eq!(rebuilt.unwrap(), transfer);
}
#[test]
fn snaptone_bulk_requires_explicit_native_prefix_and_preserves_it_exactly() {
    let raw = include_bytes!("../../../tests/fixtures/device-assets/synthetic-calibration.clo");
    let slot = AssetSlot::new(AssetBank::SnapTone, 4).unwrap();
    assert!(asset_upload_candidate(slot, "Tone", raw).is_err());
    let transfer = asset_upload_candidate(slot, "Tone", &raw[..8192]).unwrap();
    let record = &transfer.records()[0];
    assert_eq!(record.command, 0x1013);
    assert_eq!(record.body.len(), 0x2018);
    assert_eq!(&record.body[24..], &raw[..8192]);
    let expected: u32 = raw[..8192].iter().map(|byte| u32::from(*byte)).sum();
    assert_eq!(&record.body[4..8], &expected.to_le_bytes());
    assert!(asset_upload_candidate(slot, "Tone", &raw[..8191]).is_err());
}
#[test]
fn asset_status_remains_opaque_until_semantics_are_observed() {
    let record = RawRecord {
        command: 0x100b,
        body: vec![19, 0, 0xcd, 0xab, 1, 2, 3, 4],
    };
    let reply = AssetStatusReply::decode(AssetBank::Ir, &record).unwrap();
    assert_eq!(reply.flags_raw, 0xabcd);
    assert_eq!(reply.value_raw, 0x04030201);
    assert_eq!(reply.raw(), &record);
    assert!(AssetStatusReply::decode(AssetBank::SnapTone, &record).is_err());
}
