use gp200_protocol::*;
#[test]
fn firmware_identity_preserves_raw_product_version_and_negotiated_length() {
    let mut raw = vec![0xf0, 0x7e, 0x48, 1];
    raw.extend_from_slice(b"GP-200\0\x11\x12abcdefg");
    raw.extend_from_slice(b"V180\0\x13\x14abcdefghi");
    raw.extend_from_slice(&[0xf, 0xf, 0xf7]);
    let decoded = BootloaderIdentity::decode(&raw).unwrap();
    assert_eq!(decoded.raw(), raw);
    assert_eq!(decoded.chunk_bytes, 255);
    assert_eq!(&decoded.product_raw[..6], b"GP-200");
    assert_eq!(&decoded.version_raw[..4], b"V180");
    for offset in [0, 1, 2, 3, 38] {
        let mut broken = raw.clone();
        broken[offset] ^= 1;
        assert!(BootloaderIdentity::decode(&broken).is_err());
    }
    let mut broken = raw.clone();
    broken[36] = 16;
    assert!(BootloaderIdentity::decode(&broken).is_err());
    broken[36] = 0;
    broken[37] = 0;
    assert!(BootloaderIdentity::decode(&broken).is_err());
    for size in 0..39 {
        assert!(BootloaderIdentity::decode(&raw[..size]).is_err());
    }
}
#[test]
fn firmware_candidate_matches_hand_calculated_nibbles_sum_and_ff_padding() {
    let block = firmware_block_candidate(129, 3, &[0xab, 0x01]).unwrap();
    assert_eq!(
        block.raw(),
        [0xf0, 0x7e, 0x48, 4, 1, 10, 11, 0, 1, 15, 15, 3, 4, 0xf7]
    );
    assert_eq!(block.data_padded, [0xab, 1, 0xff]);
    assert_eq!(block.checksum, 52);
    assert_eq!(block.block_index_mod128, 1);
    assert_eq!(FirmwareBlock::decode(block.raw(), 3).unwrap(), block);
}
#[test]
fn firmware_candidate_all_byte_values_cross_wrap_and_checksum_overflow() {
    let source: Vec<u8> = (0..=254).collect();
    let block = firmware_block_candidate(128, 255, &source).unwrap();
    assert_eq!(block.raw().len(), 518);
    assert_eq!(block.data_padded, source);
    assert_eq!(block.block_index_mod128, 0);
    let sum = source.iter().fold(0_u8, |sum, byte| {
        sum.wrapping_add((byte >> 4) + (byte & 15))
    });
    assert_eq!(block.checksum, sum);
}
#[test]
fn firmware_bounds_bad_nibbles_checksum_and_wrong_chunk_are_rejected() {
    for chunk in [0, 256, u16::MAX] {
        assert!(firmware_block_candidate(0, chunk, &[1]).is_err());
    }
    assert!(firmware_block_candidate(0, 2, &[]).is_err());
    assert!(firmware_block_candidate(0, 2, &[1, 2, 3]).is_err());
    let raw = firmware_block_candidate(0, 2, &[1, 2])
        .unwrap()
        .raw()
        .to_vec();
    assert!(FirmwareBlock::decode(&raw, 1).is_err());
    let mut broken = raw.clone();
    broken[5] = 16;
    assert!(FirmwareBlock::decode(&broken, 2).is_err());
    let mut broken = raw.clone();
    broken[5] ^= 1;
    assert!(FirmwareBlock::decode(&broken, 2).is_err());
    for length in 0..raw.len() {
        assert!(FirmwareBlock::decode(&raw[..length], 2).is_err());
    }
}

fn literal_header(tag: [u8; 4]) -> Vec<u8> {
    let mut header = vec![0; 92];
    header[..4].copy_from_slice(b"FRMW");
    header[8..16].copy_from_slice(&[3, 0, 8, 0, 0, 0, 205, 0]);
    header[16..22].copy_from_slice(b"GP-200");
    header[32..36].copy_from_slice(&[1, 2, 0x45, 0x58]);
    header[88..92].copy_from_slice(&tag);
    header
}
#[test]
fn fixed_packets_match_literal_complete_frames_and_reject_every_truncation() {
    let literals: &[(FirmwareFixedPacket, &[u8])] = &[
        (FirmwareFixedPacket::IdentityQuery, &[240, 126, 72, 1, 247]),
        (FirmwareFixedPacket::TerminalQuery, &[240, 126, 72, 7, 247]),
        (
            FirmwareFixedPacket::TerminalControl,
            &[240, 126, 72, 9, 247],
        ),
        (
            FirmwareFixedPacket::NativeGlobalErrorCandidate,
            &[240, 126, 72, 6, 247],
        ),
        (
            FirmwareFixedPacket::SectionTerminator,
            &[240, 126, 72, 5, 123, 247],
        ),
    ];
    for (kind, packet) in literals {
        assert_eq!(kind.raw(), *packet);
        assert_eq!(FirmwareFixedPacket::decode(packet).unwrap(), *kind);
        for n in 0..packet.len() {
            assert!(FirmwareFixedPacket::decode(&packet[..n]).is_err());
        }
        let mut extended = packet.to_vec();
        extended.insert(4, 0);
        assert!(FirmwareFixedPacket::decode(&extended).is_err());
    }
}
#[test]
fn section_request_maps_four_image_slots_and_a_distinct_terminal_selector() {
    for (selector, slot, literal) in [
        (0, Some(0), [240, 126, 72, 127, 0, 247]),
        (1, Some(2), [240, 126, 72, 127, 1, 247]),
        (2, Some(3), [240, 126, 72, 127, 2, 247]),
        (16, Some(1), [240, 126, 72, 127, 16, 247]),
        (17, None, [240, 126, 72, 127, 17, 247]),
    ] {
        let value = FirmwareSectionSelector::from_raw(selector).unwrap();
        assert_eq!(value.request_candidate(), literal);
        assert_eq!(value.image_table_slot(), slot);
        if let Some(slot) = slot {
            assert_eq!(
                FirmwareSectionSelector::for_image_table_slot(slot),
                Some(value)
            );
        }
        assert_eq!(
            FirmwareSectionSelector::decode_request(&literal).unwrap(),
            value
        );
        for n in 0..6 {
            assert!(FirmwareSectionSelector::decode_request(&literal[..n]).is_err());
        }
    }
    for invalid in [3, 15, 18, 127, 128, 255] {
        assert!(FirmwareSectionSelector::from_raw(invalid).is_err());
    }
    assert_eq!(FirmwareSectionSelector::for_image_table_slot(4), None);
    assert_eq!(
        FirmwareSectionSelector::IMAGE_TRANSFER_ORDER.map(|s| s.image_table_slot().unwrap()),
        [0, 2, 3, 1]
    );
    assert!(FirmwareSectionSelector::decode_request(&[240, 126, 72, 127, 128, 247]).is_err());
}
#[test]
fn descriptor_literal_exercises_be_u32_order_without_section_or_checksum() {
    let literal = [
        240, 126, 72, 2, 1, 2, 3, 4, 5, 6, 7, 8, 9, 0, 10, 11, 12, 13, 14, 15, 247,
    ];
    let decoded = FirmwareDescriptor::decode(&literal).unwrap();
    assert_eq!(decoded.section_bytes(), 0x12345678);
    assert_eq!(decoded.block_count(), 0x90abcdef);
    assert_eq!(
        FirmwareDescriptor::candidate(0x12345678, 0x90abcdef).raw(),
        literal
    );
    let zero = FirmwareDescriptor::candidate(0, 0);
    assert_eq!(
        zero.raw(),
        [
            240, 126, 72, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 247
        ]
    );
    let maximal = FirmwareDescriptor::candidate(u32::MAX, u32::MAX);
    assert_eq!(&maximal.raw()[4..20], &[15; 16]);
    for n in 0..21 {
        assert!(FirmwareDescriptor::decode(&literal[..n]).is_err());
    }
    for offset in 4..20 {
        let mut broken = literal;
        broken[offset] = 16;
        assert_eq!(
            FirmwareDescriptor::decode(&broken),
            Err(FirmwareWireError::InvalidNibble)
        );
    }
    let mut wrong = literal;
    wrong[3] = 3;
    assert!(FirmwareDescriptor::decode(&wrong).is_err());
}
#[test]
fn real_image_first_wrap_and_final_packets_match_pinned_literals() {
    // Accepted FW01 section0/chunk1: source offsets80,207,208,4192711.
    for (index, source, literal) in [
        (0, 255, [240, 126, 72, 4, 0, 15, 15, 1, 14, 247]),
        (127, 0, [240, 126, 72, 4, 127, 0, 0, 0, 0, 247]),
        (128, 43, [240, 126, 72, 4, 0, 2, 11, 0, 13, 247]),
        (4_192_631, 0, [240, 126, 72, 4, 119, 0, 0, 0, 0, 247]),
    ] {
        let block = firmware_block_candidate(index, 1, &[source]).unwrap();
        assert_eq!(block.raw(), literal);
        assert_eq!(
            FirmwareBlock::decode(&literal, 1).unwrap().data_padded,
            [source]
        );
    }
}
#[test]
fn padded_checksum_literal_includes_ff_tail_and_excludes_mod128_index() {
    let literal = [240, 126, 72, 4, 0, 1, 2, 10, 11, 15, 15, 15, 15, 5, 4, 247];
    let block = firmware_block_candidate(128, 4, &[0x12, 0xab]).unwrap();
    assert_eq!(block.raw(), literal);
    assert_eq!(block.data_padded, [0x12, 0xab, 0xff, 0xff]);
    assert_eq!(block.checksum, 84);
    let tail = firmware_block_candidate(0, 3, &[0]).unwrap();
    assert_eq!(
        tail.raw(),
        [240, 126, 72, 4, 0, 0, 0, 15, 15, 15, 15, 3, 12, 247]
    );
    let mut modified = literal;
    modified[4] = 127;
    assert_eq!(FirmwareBlock::decode(&modified, 4).unwrap().checksum, 84);
    modified[12] = 14;
    assert_eq!(
        FirmwareBlock::decode(&modified, 4),
        Err(FirmwareWireError::ChecksumMismatch)
    );
}
#[test]
fn modulo128_duplicate_wire_bytes_do_not_identify_absolute_blocks() {
    let a = firmware_block_candidate(0, 1, &[42]).unwrap();
    let b = firmware_block_candidate(128, 1, &[42]).unwrap();
    assert_eq!(a.raw(), b.raw());
    assert_eq!(a.block_index_mod128, 0);
    assert!(FirmwareBlock::decode(&[240, 126, 72, 4, 127, 247], 1).is_err());
}
#[test]
fn native_unsafe_identity_guard_is_not_used_by_the_strict_decoder() {
    let mut wrong = vec![0; 39];
    wrong[..4].copy_from_slice(&[240, 126, 72, 2]);
    wrong[36] = 1;
    wrong[38] = 247;
    assert_eq!(
        BootloaderIdentity::decode(&wrong),
        Err(FirmwareWireError::InvalidShape)
    );
    assert_eq!(
        BootloaderIdentity::decode(&[240, 126, 72, 1, 247]),
        Err(FirmwareWireError::InvalidShape)
    );
    let mut valid = wrong.clone();
    valid[3] = 1;
    for n in 0..39 {
        let evidence = FirmwareRxEvidence::retain(&valid[..n]).unwrap();
        assert_eq!(evidence.raw(), &valid[..n]);
        assert!(evidence.classification().is_err());
    }
    valid[36] = 16;
    assert_eq!(
        BootloaderIdentity::decode(&valid),
        Err(FirmwareWireError::InvalidNibble)
    );
    valid[36] = 0;
    assert_eq!(
        BootloaderIdentity::decode(&valid),
        Err(FirmwareWireError::InvalidChunkLength)
    );
}
#[test]
fn image_normalization_keeps_raw_v182_and_announcement_source_separate() {
    let header = literal_header(*b"V182");
    let image = FirmwareImageProvenance::from_header(&header).unwrap();
    assert_eq!(image.raw_header(), header);
    assert_eq!(image.raw_words(), [3, 8, 0, 205]);
    assert_eq!(image.native_normalized_words(), [3, 264, 0, 205]);
    assert_eq!(image.version_tag_file_offset(), 88);
    assert_eq!(image.version_tag_raw(), b"V182");
    let announcement = image.announcement_candidate().unwrap();
    let literal = [240, 126, 72, 11, 1, 86, 49, 56, 50, 247];
    assert_eq!(announcement.raw(), literal);
    assert_eq!(announcement.image_provenance(), Some(&image));
    let decoded = FirmwareAnnouncement::decode(&literal).unwrap();
    assert!(decoded.image_provenance().is_none());
    for n in 0..10 {
        assert!(FirmwareAnnouncement::decode(&literal[..n]).is_err());
    }
    for n in 0..92 {
        assert!(FirmwareImageProvenance::from_header(&header[..n]).is_err());
    }
    let mut extra = header;
    extra.push(0);
    assert!(FirmwareImageProvenance::from_header(&extra).is_err());
}
#[test]
fn lowercase_b_uses_native_v_branch_and_strict_digits_are_required() {
    for (tag, family, normalized) in [
        (*b"B163", b'B', 1),
        (*b"b163", b'V', 3),
        (*b"V163", b'V', 3),
        (*b"v163", b'V', 3),
    ] {
        let header = literal_header(tag);
        let image = FirmwareImageProvenance::from_header(&header).unwrap();
        assert_eq!(image.raw_words(), [3, 8, 0, 205]);
        assert_eq!(image.native_normalized_words(), [normalized, 262, 0, 205]);
        assert_eq!(image.metadata_raw()[1], 2);
        assert_eq!(image.native_normalized_metadata()[1], 3);
        assert_eq!(
            image.announcement_candidate().unwrap().raw(),
            [240, 126, 72, 11, 1, family, 49, 54, 51, 247]
        );
    }
    let bad = FirmwareImageProvenance::from_header(&literal_header(*b"V/8:")).unwrap();
    assert_eq!(bad.native_normalized_words()[1], 0xff08);
    assert_eq!(bad.native_normalized_metadata()[1], 10);
    assert_eq!(
        bad.announcement_candidate(),
        Err(FirmwareWireError::InvalidImageVersion)
    );
    let unknown = FirmwareImageProvenance::from_header(&literal_header(*b"X999")).unwrap();
    assert!(!unknown.tag_recognized());
    assert_eq!(unknown.raw_words(), unknown.native_normalized_words());
    assert_eq!(unknown.metadata_raw(), unknown.native_normalized_metadata());
    assert_eq!(
        unknown.announcement_candidate(),
        Err(FirmwareWireError::UnrecognizedImageTag)
    );
}
#[test]
fn native_digit_subtraction_wraps_before_sign_extension_for_nonascii_input() {
    let image =
        FirmwareImageProvenance::from_header(&literal_header([b'V', 0x80, 0xaf, 0])).unwrap();
    // 0x80-0x30=0x50=80; 0xaf-0x30=0x7f=127, then 16-bit sum.
    assert_eq!(image.native_normalized_words()[1], 0x507f);
    assert_eq!(image.native_normalized_metadata()[1], 208);
    assert_eq!(
        image.announcement_candidate(),
        Err(FirmwareWireError::InvalidImageVersion)
    );
}
#[test]
fn terminal_candidate_rejects_odd_non_nibble_and_native_count_narrowing() {
    let packet = [240, 126, 72, 7, 1, 2, 10, 11, 247];
    let evidence = FirmwareRxEvidence::retain(&packet).unwrap();
    assert_eq!(
        evidence.classification(),
        &Ok(FirmwareRxCandidate::TerminalData {
            decoded: vec![0x12, 0xab]
        })
    );
    assert_eq!(evidence.payload_raw(), [1, 2, 10, 11]);
    for (raw, reason) in [
        (
            vec![240, 126, 72, 7, 0, 247],
            FirmwareWireError::OddNibbleCount,
        ),
        (
            vec![240, 126, 72, 7, 16, 0, 247],
            FirmwareWireError::InvalidNibble,
        ),
    ] {
        let rejected = FirmwareRxEvidence::retain(&raw).unwrap();
        assert_eq!(rejected.raw(), raw);
        assert_eq!(rejected.classification(), &Err(reason));
    }
    for (count, accepted) in [(255, true), (256, false)] {
        let mut raw = vec![240, 126, 72, 7];
        raw.resize(4 + 2 * count, 0);
        raw.push(247);
        let evidence = FirmwareRxEvidence::retain(&raw).unwrap();
        assert_eq!(evidence.raw(), raw);
        if accepted {
            assert!(evidence.classification().is_ok());
        } else {
            assert_eq!(
                evidence.classification(),
                &Err(FirmwareWireError::TerminalCountOverflow)
            );
        }
    }
}
#[test]
fn raw_status_retains_opcode_payload_and_offset4_without_becoming_block_or_ack() {
    let raw = [240, 126, 72, 4, 127, 247];
    let status = FirmwareRxEvidence::retain(&raw).unwrap();
    assert_eq!(
        status.classification(),
        &Ok(FirmwareRxCandidate::OpaqueStatus)
    );
    assert_eq!(status.raw(), raw);
    assert_eq!(status.opcode_raw(), Some(4));
    assert_eq!(status.offset4_raw(), Some(127));
    assert_eq!(status.payload_raw(), [127]);
    assert!(FirmwareBlock::decode(&raw, 1).is_err());
    let malformed = [241, 126, 72, 4, 127, 247];
    let evidence = FirmwareRxEvidence::retain(&malformed).unwrap();
    assert_eq!(evidence.raw(), malformed);
    assert_eq!(
        evidence.classification(),
        &Err(FirmwareWireError::InvalidShape)
    );
}
#[test]
fn exact_global_error_and_malformed_error_are_separate_retained_evidence() {
    let exact = [240, 126, 72, 6, 247];
    assert_eq!(
        FirmwareRxEvidence::retain(&exact).unwrap().classification(),
        &Ok(FirmwareRxCandidate::NativeGlobalError)
    );
    for malformed in [
        vec![240, 126, 72, 6, 0, 247],
        vec![240, 126, 72, 6],
        vec![240, 126, 72, 6, 127],
    ] {
        let evidence = FirmwareRxEvidence::retain(&malformed).unwrap();
        assert_eq!(evidence.raw(), malformed);
        assert_eq!(
            evidence.classification(),
            &Err(FirmwareWireError::InvalidShape)
        );
    }
}
#[test]
fn oversized_rx_is_explicitly_rejected_before_copying() {
    assert_eq!(
        FirmwareRxEvidence::retain(&vec![0; MAX_FIRMWARE_PACKET_BYTES + 1]),
        Err(FirmwareWireError::PacketTooLarge)
    );
    let exact_bound = vec![0; MAX_FIRMWARE_PACKET_BYTES];
    let evidence = FirmwareRxEvidence::retain(&exact_bound).unwrap();
    assert_eq!(evidence.raw(), exact_bound);
    assert!(evidence.classification().is_err());
}
#[test]
fn rejected_missing_or_wrong_trailer_does_not_drop_actual_payload_bytes() {
    for (raw, payload) in [
        (vec![240, 126, 72, 7, 16, 0], vec![16, 0]),
        (vec![240, 126, 72, 7, 16, 0, 99], vec![16, 0, 99]),
        (vec![240, 126, 72, 7, 16, 0, 247], vec![16, 0]),
        (vec![240, 126, 72, 7, 16], vec![16]),
    ] {
        let evidence = FirmwareRxEvidence::retain(&raw).unwrap();
        assert_eq!(evidence.raw(), raw);
        assert_eq!(evidence.payload_raw(), payload);
        assert!(evidence.classification().is_err());
    }
}
