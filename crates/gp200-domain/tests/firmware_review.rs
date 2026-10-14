use gp200_domain::*;
use gp200_protocol::BootloaderIdentity;

const IMAGE: &[u8] = include_bytes!("../../../tests/fixtures/device-assets/synthetic-firmware.bin");
fn blob(bytes: &[u8]) -> RetainedJobBlob {
    RetainedJobBlob::checked(
        AssetId("retained firmware".into()),
        bytes.into(),
        JobSha256::of(bytes),
    )
    .unwrap()
}
fn review() -> FirmwareReview {
    FirmwareReview::inspect(blob(IMAGE), None, &FirmwareBackupAssessment::default()).unwrap()
}
fn bootloader(chunk: u8) -> BootloaderIdentity {
    let mut raw = vec![0; 39];
    raw[..4].copy_from_slice(&[0xf0, 0x7e, 0x48, 1]);
    raw[4..10].copy_from_slice(b"GP-200");
    raw[36] = chunk >> 4;
    raw[37] = chunk & 15;
    raw[38] = 0xf7;
    BootloaderIdentity::decode(&raw).unwrap()
}
fn checksum(bytes: &mut [u8]) {
    bytes[14] = bytes[48..]
        .iter()
        .fold(0_u8, |sum, byte| sum.wrapping_add(*byte));
}

#[test]
fn review_reparses_immutable_image_and_section_hashes() {
    let review = review();
    let summary = review.summary();
    assert_eq!(summary.model, DeviceModel::Gp200);
    assert_eq!(summary.image_version.as_deref(), Some("1.8.0"));
    assert_eq!(summary.source_sha256, JobSha256::of(IMAGE));
    assert!(!summary.checksum_is_signature);
    for index in 0..4 {
        assert_eq!(
            summary.sections[index].sha256,
            JobSha256::of(review.section(index).unwrap())
        );
    }
    assert!(
        summary
            .blockers
            .contains(&FirmwareReviewBlocker::InstalledFirmwareUnknown)
    );
    assert!(
        summary
            .blockers
            .contains(&FirmwareReviewBlocker::BootloaderTransitionAndReconnectUnverified)
    );
    assert!(
        summary
            .blockers
            .contains(&FirmwareReviewBlocker::FinalizationAndRecoveryUnverified)
    );
}
#[test]
fn forged_inspection_data_cannot_replace_source_bytes() {
    assert!(
        FirmwareReview::inspect(
            blob(&IMAGE[..IMAGE.len() - 1]),
            None,
            &FirmwareBackupAssessment::default()
        )
        .is_err()
    );
    let mut corrupted = IMAGE.to_vec();
    corrupted[100] ^= 1;
    assert!(
        FirmwareReview::inspect(blob(&corrupted), None, &FirmwareBackupAssessment::default())
            .is_err()
    );
    let mut unknown = IMAGE.to_vec();
    unknown[16..32].fill(b'x');
    assert!(
        FirmwareReview::inspect(blob(&unknown), None, &FirmwareBackupAssessment::default())
            .is_err()
    );
    let mut overlapping = IMAGE.to_vec();
    overlapping[56..60].copy_from_slice(&32_u32.to_le_bytes());
    checksum(&mut overlapping);
    assert!(
        FirmwareReview::inspect(
            blob(&overlapping),
            None,
            &FirmwareBackupAssessment::default()
        )
        .is_err()
    );
}
#[test]
fn backup_manifest_counters_never_grant_full_content_backup_proof() {
    let mut assessment = FirmwareBackupAssessment::default();
    assessment.stored_presets = 256;
    assessment.current_buffer = true;
    assessment.globals = true;
    assessment.verified_ir_contents = 20;
    assessment.verified_snaptone_contents = 10;
    assert!(!assessment.complete());
    let review = FirmwareReview::inspect(blob(IMAGE), None, &assessment).unwrap();
    assert!(
        review
            .summary()
            .blockers
            .contains(&FirmwareReviewBlocker::CompleteContentBackupMissing)
    );
}
#[test]
fn chunk_preview_uses_exact_sections_and_negotiated_raw_identity() {
    let mut identity = bootloader(7);
    // Public display field mutations cannot introduce zero division or a new
    // product/chunk negotiation in place of the retained authoritative packet.
    identity.chunk_bytes = 0;
    identity.product_raw.fill(b'x');
    let mut rehearsal = review().rehearse(&identity).unwrap();
    assert_eq!(rehearsal.total_blocks(), 16);
    let chunk = rehearsal.next_chunk().unwrap();
    assert_eq!(chunk.content_bytes, 7);
    assert_eq!(chunk.file_offset, 80);
    assert_eq!(chunk.wire_bytes, 22);
    assert_eq!(chunk.index_mod128, 0);
    assert_eq!(rehearsal.progress().completed, 0);
    assert!(!rehearsal.progress().cancellable);
}
#[test]
fn missing_or_cross_file_ack_does_not_advance_or_resend() {
    let mut rehearsal = review().rehearse(&bootloader(7)).unwrap();
    let chunk = rehearsal.next_chunk().unwrap();
    assert!(rehearsal.next_chunk().is_err());
    let mut wrong = chunk.clone();
    wrong.source_sha256 = JobSha256::of(b"other firmware");
    assert!(rehearsal.acknowledge_simulated_chunk(&wrong).is_err());
    assert_eq!(rehearsal.progress().completed, 0);
    rehearsal.acknowledgement_missing();
    assert_eq!(*rehearsal.phase(), FirmwareRehearsalPhase::Indeterminate);
    assert!(rehearsal.acknowledge_simulated_chunk(&chunk).is_err());
    assert!(rehearsal.next_chunk().is_err());
    assert_eq!(&*rehearsal.retained_source(), IMAGE);
}
#[test]
fn preparation_cancel_is_safe_and_no_later_boundary_is_invented() {
    let mut before = review().rehearse(&bootloader(7)).unwrap();
    before.cancel().unwrap();
    assert_eq!(*before.phase(), FirmwareRehearsalPhase::Cancelled);
    assert!(before.next_chunk().is_err());
    let mut after = review().rehearse(&bootloader(7)).unwrap();
    let chunk = after.next_chunk().unwrap();
    assert_eq!(after.cancel().unwrap_err().kind, ErrorKind::Unverified);
    after.acknowledge_simulated_chunk(&chunk).unwrap();
    assert!(after.cancel().is_err());
}
#[test]
fn all_simulated_bytes_acknowledged_still_do_not_claim_successful_flash() {
    let mut rehearsal = review().rehearse(&bootloader(7)).unwrap();
    for (section, slot) in [0, 2, 3, 1].into_iter().enumerate() {
        let reviewed = review();
        let raw_section = &reviewed.summary().sections[slot];
        let mut content_offset = 0;
        while matches!(
            rehearsal.phase(),
            FirmwareRehearsalPhase::Prepared | FirmwareRehearsalPhase::Transferring
        ) {
            let chunk = rehearsal.next_chunk().unwrap();
            assert_eq!(chunk.section, section as u8);
            assert_eq!(chunk.file_offset, raw_section.file_offset + content_offset);
            let expected = gp200_protocol::firmware_block_candidate(
                chunk.block_in_section,
                7,
                &reviewed.source()[chunk.file_offset..chunk.file_offset + chunk.content_bytes],
            )
            .unwrap();
            assert_eq!(chunk.wire_sha256, JobSha256::of(expected.raw()));
            content_offset += chunk.content_bytes;
            rehearsal.acknowledge_simulated_chunk(&chunk).unwrap();
        }
        assert_eq!(content_offset, raw_section.bytes);
        assert_eq!(
            *rehearsal.phase(),
            FirmwareRehearsalPhase::AwaitingSectionAcknowledgement
        );
        assert!(
            rehearsal
                .acknowledge_simulated_section(JobSha256::of(IMAGE), (section as u8 + 1) % 4)
                .is_err()
        );
        rehearsal
            .acknowledge_simulated_section(JobSha256::of(IMAGE), section as u8)
            .unwrap();
    }
    assert_eq!(
        *rehearsal.phase(),
        FirmwareRehearsalPhase::AwaitingFinalizeEvidence
    );
    assert_eq!(
        rehearsal.progress().completed,
        rehearsal.progress().total.unwrap()
    );
    assert!(rehearsal.next_chunk().is_err());
    rehearsal.acknowledgement_missing();
    assert_eq!(*rehearsal.phase(), FirmwareRehearsalPhase::Indeterminate);
}
#[test]
fn count_metadata_preserves_raw_slots_and_maps_native_selectors() {
    let review = review();
    let counts = review.count_review(7).unwrap();
    for (slot, selector) in [0, 0x10, 1, 2].into_iter().enumerate() {
        let entry = &counts.sections[slot];
        let source = &review.summary().sections[slot];
        assert_eq!(entry.image_table_slot, slot as u8);
        assert_eq!(entry.selector_raw, selector);
        assert_eq!(entry.source_sha256, source.sha256);
        assert_eq!(entry.count.section_bytes as usize, source.bytes);
        assert_eq!(
            entry.count.safe_integer_blocks as usize,
            source.bytes.div_ceil(7)
        );
    }
}

#[cfg(gp200_private_evidence)]
fn hex_bytes(hex: &str) -> Vec<u8> {
    assert!(hex.len().is_multiple_of(2));
    (0..hex.len())
        .step_by(2)
        .map(|n| u8::from_str_radix(&hex[n..n + 2], 16).unwrap())
        .collect()
}
#[test]
#[cfg(gp200_private_evidence)]
fn all_pinned_real_boundaries_match_independent_complete_packet_vectors() {
    use gp200_protocol::{FirmwareBlock, firmware_block_candidate};
    // This frozen codec evidence predates the corrected native slot mapping.
    // Its old selector labels identify the original vectors only; current
    // selector/source traversal is verified separately without rewriting it.
    let value:serde_json::Value=serde_json::from_str(include_str!("../../../analysis/implementation/firmware-wire-contract-research/real-image-boundaries.json")).unwrap();
    let records = value["records"].as_array().unwrap();
    assert_eq!(records.len(), 80);
    let mut covered = std::collections::BTreeSet::new();
    for record in records {
        let selector = record["section_selector"].as_u64().unwrap() as u8;
        let chunk = record["chunk_fixture"].as_u64().unwrap() as u16;
        assert_eq!(record["chunk_was_physically_negotiated"], false);
        let source = hex_bytes(record["source_hex"].as_str().unwrap());
        let expected = hex_bytes(record["complete_packet_hex"].as_str().unwrap());
        let index = record["absolute_block_index"].as_u64().unwrap() as u32;
        let actual = firmware_block_candidate(index, chunk, &source).unwrap();
        assert_eq!(actual.raw(), expected);
        assert_eq!(
            actual.raw().len(),
            record["packet_size"].as_u64().unwrap() as usize
        );
        assert_eq!(
            JobSha256::of(&source).hex(),
            record["source_sha256"].as_str().unwrap()
        );
        assert_eq!(
            JobSha256::of(actual.raw()).hex(),
            record["packet_sha256"].as_str().unwrap()
        );
        let decoded = FirmwareBlock::decode(&expected, chunk).unwrap();
        assert_eq!(
            decoded.block_index_mod128,
            record["index_mod128"].as_u64().unwrap() as u8
        );
        assert_eq!(&decoded.data_padded[..source.len()], source);
        assert!(
            decoded.data_padded[source.len()..]
                .iter()
                .all(|byte| *byte == 255)
        );
        assert_eq!(
            decoded.data_padded.len() - source.len(),
            record["tail_ff_padding_bytes"].as_u64().unwrap() as usize
        );
        let length = match selector {
            0 => 4_192_632,
            1 => 1_834_032,
            2 => 294_912,
            16 => 131_072,
            _ => panic!("unknown selector"),
        };
        let count = firmware_block_count_comparison(length, chunk).unwrap();
        assert_eq!(
            count.safe_integer_blocks,
            u32::try_from(record["block_count"].as_u64().unwrap()).unwrap()
        );
        assert!(!count.discrepancy());
        covered.insert((selector, chunk));
    }
    assert_eq!(covered.len(), 20);
}
#[test]
#[cfg(gp200_private_evidence)]
fn all_35_complete_golden_vectors_have_rust_codec_coverage() {
    use gp200_protocol::*;
    let vectors: serde_json::Value = serde_json::from_str(include_str!(
        "../../../analysis/implementation/firmware-wire-contract-research/golden-vectors.json"
    ))
    .unwrap();
    let vectors = vectors.as_array().unwrap();
    assert_eq!(vectors.len(), 35);
    let mut names = std::collections::BTreeSet::new();
    for record in vectors {
        let name = record["name"].as_str().unwrap();
        assert!(names.insert(name));
        let raw = hex_bytes(record["packet_hex"].as_str().unwrap());
        assert_eq!(raw.len(), record["size"].as_u64().unwrap() as usize);
        assert_eq!(
            JobSha256::of(&raw).hex(),
            record["sha256"].as_str().unwrap()
        );
        match name {
            "control_01"
            | "control_07"
            | "control_09"
            | "global_error_06"
            | "section_terminator_05_7b" => {
                assert_eq!(FirmwareFixedPacket::decode(&raw).unwrap().raw(), raw);
            }
            "image_announcement" => {
                let mut header = vec![0; 92];
                header[..4].copy_from_slice(b"FRMW");
                header[8..16].copy_from_slice(&[3, 0, 8, 0, 0, 0, 205, 0]);
                header[16..22].copy_from_slice(b"GP-200");
                header[32..36].copy_from_slice(&[1, 2, 69, 88]);
                header[88..92].copy_from_slice(b"V182");
                let image = FirmwareImageProvenance::from_header(&header).unwrap();
                assert_eq!(image.announcement_candidate().unwrap().raw(), raw);
                assert!(
                    FirmwareAnnouncement::decode(&raw)
                        .unwrap()
                        .image_provenance()
                        .is_none()
                );
            }
            "descriptor_synthetic" => {
                assert_eq!(
                    FirmwareDescriptor::candidate(0x12345678, 0x90abcdef).raw(),
                    raw
                );
                let decoded = FirmwareDescriptor::decode(&raw).unwrap();
                assert_eq!(decoded.section_bytes(), 0x12345678);
                assert_eq!(decoded.block_count(), 0x90abcdef);
            }
            "block_synthetic_ff_tail" => {
                assert_eq!(
                    firmware_block_candidate(128, 4, &[0x12, 0xab])
                        .unwrap()
                        .raw(),
                    raw
                );
            }
            "identity_synthetic" => {
                assert_eq!(BootloaderIdentity::decode(&raw).unwrap().chunk_bytes, 128);
            }
            "status_synthetic" => {
                assert_eq!(
                    FirmwareRxEvidence::retain(&raw).unwrap().classification(),
                    &Ok(FirmwareRxCandidate::OpaqueStatus)
                );
                assert!(FirmwareBlock::decode(&raw, 1).is_err());
            }
            _ if name.starts_with("section_request_") => {
                assert_eq!(
                    FirmwareSectionSelector::decode_request(&raw)
                        .unwrap()
                        .request_candidate(),
                    raw.as_slice()
                );
            }
            _ if name.starts_with("descriptor_section_") => {
                let section = record["section"].as_u64().unwrap();
                let chunk = record["chunk"].as_u64().unwrap() as u16;
                let length = match section {
                    0 => 4_192_632,
                    1 => 1_834_032,
                    2 => 294_912,
                    16 => 131_072,
                    _ => panic!("unknown section"),
                };
                let count = firmware_block_count_comparison(length, chunk).unwrap();
                assert_eq!(
                    FirmwareDescriptor::candidate(length, count.safe_integer_blocks).raw(),
                    raw
                );
                let decoded = FirmwareDescriptor::decode(&raw).unwrap();
                assert_eq!(decoded.section_bytes(), length);
                assert_eq!(decoded.block_count(), count.safe_integer_blocks);
            }
            _ => panic!("uncovered golden vector {name}"),
        }
    }
}
#[test]
fn count_comparison_retains_exact_integer_plan_and_exposes_float32_discrepancy() {
    let discrepancy = firmware_block_count_comparison(16_777_217, 1).unwrap();
    assert_eq!(discrepancy.safe_integer_blocks, 16_777_217);
    assert_eq!(discrepancy.native_float32_blocks, 16_777_216);
    assert!(discrepancy.discrepancy());
    for chunk in [1, 7, 127, 128, 255] {
        let zero = firmware_block_count_comparison(0, chunk).unwrap();
        assert_eq!(zero.safe_integer_blocks, 0);
        assert_eq!(zero.native_float32_blocks, 0);
        let maximum =
            firmware_block_count_comparison(MAX_FIRMWARE_COUNT_SECTION_BYTES, chunk).unwrap();
        assert_eq!(
            maximum.safe_integer_blocks,
            MAX_FIRMWARE_COUNT_SECTION_BYTES.div_ceil(u32::from(chunk))
        );
    }
    for chunk in [0, 256, u16::MAX] {
        assert!(firmware_block_count_comparison(1, chunk).is_err());
    }
    for length in [MAX_FIRMWARE_COUNT_SECTION_BYTES + 1, u32::MAX] {
        assert!(firmware_block_count_comparison(length, 1).is_err());
    }
}
#[test]
fn discrepancy_is_reviewable_and_blocks_rehearsal_without_replacing_raw_source() {
    let large = 16_777_217_u32;
    let lengths = [large, 16, 16, 32];
    let total = 80 + lengths.iter().map(|n| *n as usize).sum::<usize>();
    let mut raw = vec![0; total];
    raw[..92].copy_from_slice(&IMAGE[..92]);
    raw[4..8].copy_from_slice(&((total - 48) as u32).to_le_bytes());
    let mut offset = 80_u32;
    for (slot, length) in lengths.into_iter().enumerate() {
        raw[48 + slot * 8..52 + slot * 8].copy_from_slice(&(offset - 48).to_le_bytes());
        raw[52 + slot * 8..56 + slot * 8].copy_from_slice(&length.to_le_bytes());
        offset += length;
    }
    checksum(&mut raw);
    let review =
        FirmwareReview::inspect(blob(&raw), None, &FirmwareBackupAssessment::default()).unwrap();
    assert_eq!(review.source(), raw);
    assert_eq!(review.summary().image_provenance.raw_header(), &raw[..92]);
    let counts = review.count_review(1).unwrap();
    assert_eq!(counts.sections[0].count.safe_integer_blocks, 16_777_217);
    assert_eq!(counts.sections[0].count.native_float32_blocks, 16_777_216);
    assert!(
        counts
            .blockers
            .contains(&FirmwareCountReviewBlocker::IntegerNativeCountDiscrepancy)
    );
    assert!(
        counts
            .blockers
            .contains(&FirmwareCountReviewBlocker::OperationalTransferUnverified)
    );
    let mut rehearsal = review.rehearse(&bootloader(1)).unwrap();
    assert_eq!(rehearsal.count_review(), &counts);
    assert_eq!(rehearsal.total_blocks(), 16_777_281);
    assert_eq!(
        rehearsal.next_chunk().unwrap_err().kind,
        ErrorKind::Unverified
    );
    assert_eq!(rehearsal.progress().completed, 0);
    assert_eq!(*rehearsal.phase(), FirmwareRehearsalPhase::Prepared);
    rehearsal.cancel().unwrap();
}
#[test]
fn repeated_stale_reordered_statuses_and_terminal_candidates_never_grant_progress() {
    let mut transcript = FirmwareOfflineRxTranscript::default();
    let status = [240, 126, 72, 4, 127, 247];
    let terminal = [240, 126, 72, 7, 1, 2, 247];
    // Terminal before identity, followed by duplicate/late statuses, remains evidence.
    transcript.record_candidate(&terminal).unwrap();
    for _ in 0..129 {
        transcript.record_candidate(&status).unwrap();
    }
    transcript.record_candidate(bootloader(1).raw()).unwrap();
    transcript.record_candidate(&status).unwrap();
    assert_eq!(transcript.records().len(), 132);
    assert_eq!(*transcript.state(), FirmwareOfflineRxState::Recording);
    for (ordinal, record) in transcript.records().iter().enumerate() {
        assert_eq!(record.ordinal, ordinal);
    }
    assert!(!transcript.device_progress_verified());
    assert!(!transcript.completion_verified());
    assert!(!transcript.resume_supported());
    let mut rehearsal = review().rehearse(&bootloader(7)).unwrap();
    let pending = rehearsal.next_chunk().unwrap();
    assert_eq!(rehearsal.progress().completed, 0);
    assert_eq!(
        *rehearsal.phase(),
        FirmwareRehearsalPhase::AwaitingBlockAcknowledgement
    );
    // Only the separate explicit simulated API accepts this local preview.
    rehearsal.acknowledge_simulated_chunk(&pending).unwrap();
    assert_eq!(rehearsal.progress().completed, 7);
    assert!(!transcript.device_progress_verified());
}
#[test]
fn disconnect_exact_error_and_rejection_stop_without_resume_and_preserve_evidence() {
    for (packet, state) in [
        (
            vec![240, 126, 72, 6, 247],
            FirmwareOfflineRxState::StoppedNativeError,
        ),
        (
            vec![240, 126, 72, 6, 0, 247],
            FirmwareOfflineRxState::StoppedRejectedPacket,
        ),
        (
            vec![240, 126, 72, 7, 16, 0, 247],
            FirmwareOfflineRxState::StoppedRejectedPacket,
        ),
    ] {
        let mut transcript = FirmwareOfflineRxTranscript::default();
        transcript.record_candidate(&packet).unwrap();
        assert_eq!(transcript.records()[0].evidence.raw(), packet);
        assert_eq!(*transcript.state(), state);
        let retained = transcript.clone();
        transcript.disconnect();
        assert_eq!(transcript, retained);
        assert!(
            transcript
                .record_candidate(&[240, 126, 72, 4, 127, 247])
                .is_err()
        );
        assert!(!transcript.resume_supported());
        assert!(!transcript.completion_verified());
    }
    let mut disconnected = FirmwareOfflineRxTranscript::default();
    disconnected.disconnect();
    assert_eq!(
        *disconnected.state(),
        FirmwareOfflineRxState::StoppedDisconnect
    );
    assert!(disconnected.record_candidate(bootloader(7).raw()).is_err());
}
#[test]
fn offline_transcript_capacity_and_oversize_are_explicit_permanent_stops() {
    let status = [240, 126, 72, 4, 127, 247];
    let mut full = FirmwareOfflineRxTranscript::default();
    for _ in 0..MAX_FIRMWARE_OFFLINE_RX_RECORDS {
        full.record_candidate(&status).unwrap();
    }
    assert_eq!(
        full.record_candidate(&status).unwrap_err().kind,
        ErrorKind::Busy
    );
    assert_eq!(*full.state(), FirmwareOfflineRxState::StoppedCapacity);
    assert_eq!(full.records().len(), MAX_FIRMWARE_OFFLINE_RX_RECORDS);
    assert!(full.record_candidate(&status).is_err());
    let mut over = FirmwareOfflineRxTranscript::default();
    assert!(over.record_candidate(&vec![0; 519]).is_err());
    assert_eq!(
        *over.state(),
        FirmwareOfflineRxState::StoppedOversizedPacket
    );
    assert!(over.records().is_empty());
    assert!(over.record_candidate(&status).is_err());
}
