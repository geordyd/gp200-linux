use gp200_domain::*;
use std::sync::Arc;
fn image(lengths: [usize; 4]) -> Vec<u8> {
    let mut bytes = vec![0; 96 + lengths.iter().sum::<usize>()];
    bytes[..4].copy_from_slice(b"FRMW");
    let declared = (bytes.len() - 48) as u32;
    bytes[4..8].copy_from_slice(&declared.to_le_bytes());
    bytes[16..22].copy_from_slice(b"GP-200");
    bytes[88..92].copy_from_slice(b"V182");
    bytes[33] = 2;
    let mut offset = 96;
    for (i, len) in lengths.iter().enumerate() {
        bytes[48 + i * 8..52 + i * 8].copy_from_slice(&((offset - 48) as u32).to_le_bytes());
        bytes[52 + i * 8..56 + i * 8].copy_from_slice(&(*len as u32).to_le_bytes());
        for (n, b) in bytes[offset..offset + len].iter_mut().enumerate() {
            *b = (n % 251) as u8;
        }
        offset += len;
    }
    bytes[14] = bytes[48..].iter().fold(0u8, |a, b| a.wrapping_add(*b));
    bytes
}
fn context() -> FirmwareUpdateContextDto {
    FirmwareUpdateContextDto {
        profile_id: "offline fixture; no live admission".into(),
        hardware_policy: "unknown hardware applicability".into(),
        update_mode: "descriptive only".into(),
        normal: None,
        versions: vec![FirmwareVersionClaim {
            origin: FirmwareVersionOrigin::HumanReported,
            value: "1.6.3".into(),
            raw: vec![],
        }],
        backup_objects: vec![],
        loss_items: vec!["Unavailable prior IR audio".into()],
        power_statement: "recorded power statement".into(),
        connection_statement: "recorded connection statement".into(),
    }
}
fn documents() -> Vec<FirmwareMaterialInput> {
    [
        FirmwareMaterialKind::Profile,
        FirmwareMaterialKind::Evidence,
        FirmwareMaterialKind::BackupArchive,
        FirmwareMaterialKind::BackupInventory,
        FirmwareMaterialKind::RestorePlan,
        FirmwareMaterialKind::Recovery,
        FirmwareMaterialKind::ApprovalStatement,
    ]
    .into_iter()
    .map(|kind| FirmwareMaterialInput {
        kind,
        bytes: Arc::from(&b"descriptive fixture; never authorization"[..]),
    })
    .collect()
}
fn prepare(
    raw: Vec<u8>,
    c: FirmwareUpdateContextDto,
    chunk: u16,
) -> Result<ReviewedFirmwareUpdate, ServiceError> {
    ReviewedFirmwareUpdate::prepare(
        RetainedJobBlob::checked(
            AssetId("fixture".into()),
            raw.clone().into(),
            JobSha256::of(&raw),
        )?,
        c,
        documents(),
        chunk,
    )
}
fn job() -> ReviewedFirmwareUpdate {
    prepare(image([260, 3, 4, 5]), context(), 1).unwrap()
}
fn send(d: &FirmwareBlockDescription) -> FirmwareSendObservation {
    FirmwareSendObservation {
        action: d.ordinal,
        wire_sha256: d.wire_sha256.clone(),
        attempted: u64::from(d.wire_bytes),
        extent: FirmwareOsExtent::Known(u64::from(d.wire_bytes)),
        drain: FirmwareDrainObservation::Returned,
        started_sequence: 2,
        finished_sequence: 4,
        started_at_ms: 10,
        finished_at_ms: 20,
    }
}
#[test]
fn immutable_preimage_reconstructs_but_never_issues_permission() {
    let j = job();
    let supplied = j
        .materials()
        .map(|(k, b)| FirmwareMaterialInput {
            kind: k.clone(),
            bytes: Arc::from(b),
        })
        .collect();
    let rebuilt = ReviewedFirmwareUpdate::reconstruct(j.preimage(), supplied).unwrap();
    assert_eq!(rebuilt.digest(), j.digest());
    assert!(!rebuilt.permits_flash());
    assert!(!rebuilt.complete_backup_verified());
    assert_eq!(
        rebuilt.context().versions[0].origin,
        FirmwareVersionOrigin::HumanReported
    );
}
#[test]
fn imported_pin_allocation_budget_is_checked_before_blob_loading() {
    let j = job();
    let mut value: serde_json::Value = serde_json::from_slice(j.preimage()).unwrap();
    let pins = value["pins"].as_array_mut().unwrap();
    for i in 0..3 {
        pins.push(
            serde_json::to_value(FirmwareMaterialPin {
                kind: FirmwareMaterialKind::BackupObject(format!("large-{i}")),
                sha256: JobSha256::of(&[i]).hex(),
                bytes: FIRMWARE_SOURCE_LIMIT,
            })
            .unwrap(),
        );
    }
    let error =
        ReviewedFirmwareUpdate::retained_pins(&serde_json::to_vec(&value).unwrap()).unwrap_err();
    assert!(error.to_string().contains("128 MiB"));
    let original: serde_json::Value = serde_json::from_slice(j.preimage()).unwrap();
    for fault in 0..4 {
        let mut altered = original.clone();
        let pins = altered["pins"].as_array_mut().unwrap();
        match fault {
            0 => {
                pins.push(pins[0].clone());
            }
            1 => {
                pins[1]["bytes"] = serde_json::json!(2 * 1024 * 1024 + 1);
            }
            2 => {
                let mut alias = pins[0].clone();
                alias["kind"] = serde_json::json!({"BackupObject":"aliased"});
                alias["bytes"] = serde_json::json!(pins[0]["bytes"].as_u64().unwrap() + 1);
                pins.push(alias);
            }
            _ => {
                pins[0]["sha256"] = serde_json::json!("AA".repeat(32));
            }
        }
        assert!(
            ReviewedFirmwareUpdate::retained_pins(&serde_json::to_vec(&altered).unwrap()).is_err()
        );
    }
}
#[test]
fn source_hash_checksum_and_sections_are_independently_checked() {
    let mut b = image([8, 3, 4, 5]);
    b[100] ^= 1;
    assert!(prepare(b, context(), 8).is_err());
    let mut b = image([8, 3, 4, 5]);
    b[48..52].copy_from_slice(&u32::MAX.to_le_bytes());
    b[14] = b[48..].iter().fold(0u8, |a, b| a.wrapping_add(*b));
    assert!(prepare(b, context(), 8).is_err());
    assert!(prepare(image([8, 3, 4, 5]), context(), 0).is_err());
    assert!(prepare(image([8, 3, 4, 5]), context(), 256).is_err());
}
#[test]
fn changed_review_material_cannot_reconstruct_an_old_digest() {
    let j = job();
    let mut inputs: Vec<_> = j
        .materials()
        .map(|(k, b)| FirmwareMaterialInput {
            kind: k.clone(),
            bytes: Arc::from(b),
        })
        .collect();
    inputs
        .iter_mut()
        .find(|m| m.kind == FirmwareMaterialKind::ApprovalStatement)
        .unwrap()
        .bytes = Arc::from(&b"changed statement"[..]);
    assert!(ReviewedFirmwareUpdate::reconstruct(j.preimage(), inputs).is_err());
    let mut c = context();
    c.loss_items.push("another unavailable slot".into());
    assert_ne!(
        prepare(image([260, 3, 4, 5]), c, 1).unwrap().digest(),
        j.digest()
    );
}
#[test]
fn absolute_wrap_keys_and_independent_wire_bytes_remain_distinct() {
    let j = job();
    for ordinal in [0, 127, 128, 255, 256] {
        let d = j.block(ordinal).unwrap();
        assert_eq!(d.file_offset, 96 + ordinal);
        assert_eq!(d.block_in_section, ordinal as u32);
        assert_eq!(d.index_mod128, (ordinal % 128) as u8);
        let wire = j.block_wire(&d).unwrap();
        assert_eq!(&wire[..5], &[0xf0, 0x7e, 0x48, 4, (ordinal % 128) as u8]);
        assert_eq!(wire[5], ((ordinal % 251) as u8) >> 4);
        assert_eq!(wire[6], ((ordinal % 251) as u8) & 15);
        assert_eq!(
            &wire[wire.len() - 3..],
            &[(wire[5] + wire[6]) >> 4, (wire[5] + wire[6]) & 15, 0xf7]
        );
    }
    assert_ne!(j.block(0).unwrap(), j.block(128).unwrap());
    let mut forged = j.block(128).unwrap();
    forged.file_offset = 96;
    assert!(j.block_wire(&forged).is_err());
}
#[test]
fn tail_padding_uses_exact_ff_and_section_selectors() {
    let j = prepare(image([10, 3, 4, 5]), context(), 8).unwrap();
    let d = j.block(1).unwrap();
    assert_eq!((d.content_bytes, d.padded_bytes), (2, 6));
    let w = j.block_wire(&d).unwrap();
    assert_eq!(&w[9..21], &[15; 12]);
    assert_eq!(
        j.blocks().map(|d| d.selector).collect::<Vec<_>>(),
        vec![0, 0, 1, 2, 16]
    );
}
#[test]
fn iterator_and_random_lookup_use_native_slot_order_and_exact_payloads() {
    use gp200_protocol::FirmwareBlock;
    let lengths = [259, 5, 7, 3];
    let mut raw = image(lengths);
    let mut offset = 96;
    for (slot, length) in lengths.into_iter().enumerate() {
        raw[offset..offset + length].fill(0x30 + slot as u8);
        offset += length;
    }
    raw[14] = raw[48..].iter().fold(0u8, |a, b| a.wrapping_add(*b));
    for chunk in [1, 2, 7, 180, 255] {
        let job = prepare(raw.clone(), context(), chunk).unwrap();
        let mut blocks = job.blocks();
        let mut ordinal = 0;
        for (section, (slot, selector)) in
            [(0, 0), (2, 1), (3, 2), (1, 0x10)].into_iter().enumerate()
        {
            let source = &job.review().summary().sections[slot];
            let mut content = Vec::new();
            for index in 0..source.bytes.div_ceil(usize::from(chunk)) {
                let block = blocks.next().unwrap();
                assert_eq!(job.block(ordinal).unwrap(), block);
                assert_eq!((block.section, block.selector), (section as u8, selector));
                assert_eq!(block.block_in_section, index as u32);
                assert_eq!(
                    block.file_offset as usize,
                    source.file_offset + index * usize::from(chunk)
                );
                let wire = job.block_wire(&block).unwrap();
                let decoded = FirmwareBlock::decode(&wire, chunk).unwrap();
                let bytes = usize::from(block.content_bytes);
                assert!(
                    decoded.data_padded[..bytes]
                        .iter()
                        .all(|b| *b == 0x30 + slot as u8)
                );
                assert!(decoded.data_padded[bytes..].iter().all(|b| *b == 0xff));
                content.extend_from_slice(&decoded.data_padded[..bytes]);
                ordinal += 1;
            }
            assert_eq!(content, job.review().section(slot).unwrap());
            assert_eq!(JobSha256::of(&content), source.sha256);
        }
        assert!(blocks.next().is_none());
        assert!(job.block(ordinal).is_err());
    }
}
#[test]
fn cheap_iterator_does_not_build_a_large_packet_vector() {
    let j = prepare(image([262144, 16, 16, 16]), context(), 8).unwrap();
    assert_eq!(j.budget().blocks, 32774);
    assert_eq!(j.blocks().take(3).count(), 3);
    assert_eq!(j.block(32773).unwrap().section, 3);
    assert!(std::mem::size_of_val(&j.blocks()) < 128);
}
#[test]
fn independent_real_image_budget_calculation_admits_chunk128_and_reserves_faults() {
    let sections = [4192632u64, 1834032, 294912, 131072];
    let blocks = sections.iter().map(|b| b.div_ceil(128)).sum();
    let b = FirmwareJournalBudget::for_blocks(blocks).unwrap();
    assert_eq!(b.blocks, 50412);
    assert_eq!(b.actions, 50444);
    assert_eq!(b.frames, 252413);
    assert_eq!(b.bytes, 787253116);
    assert_eq!(b.callbacks, 100952);
    assert!(b.frames - FIRMWARE_TERMINAL_RESERVE > 0);
    assert!(FirmwareJournalBudget::for_blocks(1_000_000).is_err());
    assert!(FirmwareJournalBudget::for_blocks(u64::MAX).is_err());
    assert!(FirmwareJournalBudget::for_blocks(0).is_err());
}
#[test]
fn count_discrepancy_never_becomes_a_live_profile() {
    let c = firmware_block_count_comparison(16777217, 1).unwrap();
    assert_eq!(c.safe_integer_blocks, 16777217);
    assert_eq!(c.native_float32_blocks, 16777216);
    assert!(c.discrepancy());
    assert!(!job().permits_flash());
}
#[test]
fn local_upload_metadata_and_not_applicable_claims_do_not_complete_backup() {
    let mut c = context();
    c.backup_objects = vec![
        FirmwareBackupObjectDto {
            object: "ir20".into(),
            coverage: FirmwareBackupCoverage::LocalSource,
            content_sha256: None,
            restore_assessment: "unavailable device correlation".into(),
        },
        FirmwareBackupObjectDto {
            object: "snaptone".into(),
            coverage: FirmwareBackupCoverage::ClaimedNotApplicable,
            content_sha256: None,
            restore_assessment: "manual version only".into(),
        },
    ];
    assert!(
        !prepare(image([10, 3, 4, 5]), c, 8)
            .unwrap()
            .complete_backup_verified()
    );
}
#[test]
fn missing_and_duplicate_review_documents_are_rejected() {
    let raw = image([10, 3, 4, 5]);
    let source = RetainedJobBlob::checked(
        AssetId("source".into()),
        raw.clone().into(),
        JobSha256::of(&raw),
    )
    .unwrap();
    let mut docs = documents();
    docs.pop();
    assert!(ReviewedFirmwareUpdate::prepare(source.clone(), context(), docs, 8).is_err());
    let mut docs = documents();
    docs.push(docs[0].clone());
    assert!(ReviewedFirmwareUpdate::prepare(source, context(), docs, 8).is_err());
}
#[test]
fn source_and_automatic_version_claims_stay_separate() {
    let mut c = context();
    c.versions.push(FirmwareVersionClaim {
        origin: FirmwareVersionOrigin::ClaimedAutomatic,
        value: "9.9.9".into(),
        raw: vec![],
    });
    let j = prepare(image([10, 3, 4, 5]), c, 8).unwrap();
    assert_eq!(j.review().summary().image_version.as_deref(), Some("1.8.2"));
    assert_eq!(j.review().summary().installed_version, None);
    assert!(!j.permits_flash());
}
#[test]
fn raw_statuses_cannot_acknowledge_even_a_complete_os_write() {
    let j = job();
    let d = j.block(0).unwrap();
    let mut p = FirmwareLocalProjection::for_job(&j, FirmwareObservationOrigin::LocalObservation);
    p.checkpoint(&j, d.clone()).unwrap();
    p.observe_send(&send(&d)).unwrap();
    for (seq, raw) in [
        vec![0xf0, 0x7e, 0x48, 4, 0x7f, 0xf7],
        vec![0xf0, 0x7e, 0x48, 3, 1, 2, 0xf7],
        vec![0xf0, 0x7e, 0x48, 7, 1, 2, 0xf7],
    ]
    .into_iter()
    .enumerate()
    {
        p.observe_callback(&FirmwareCallbackDto {
            sequence: seq as u64 + 10,
            at_ms: seq as u64,
            clock_origin: "fixture-clock".into(),
            epoch: SessionEpoch(9),
            binding_sha256: JobSha256::of(b"claim").hex(),
            raw,
        })
        .unwrap();
    }
    assert_eq!(p.next_block(), 0);
    assert_eq!(p.device_verified_bytes(), 0);
    assert!(!p.completion_verified());
    assert!(!p.resume_supported());
    assert!(p.acknowledge_mock(&d).is_err());
    assert!(p.checkpoint(&j, j.block(1).unwrap()).is_err());
}
#[test]
fn explicit_mock_progress_is_separate_from_callback_observations() {
    let j = job();
    let d = j.block(0).unwrap();
    let mut p = FirmwareLocalProjection::for_job(&j, FirmwareObservationOrigin::MockSimulation);
    p.checkpoint(&j, d.clone()).unwrap();
    assert!(p.acknowledge_mock(&d).is_err());
    p.observe_send(&send(&d)).unwrap();
    p.acknowledge_mock(&d).unwrap();
    assert_eq!(p.simulated_source_bytes(), 1);
    assert_eq!(p.device_verified_bytes(), 0);
    p.derive_reopen();
    assert!(p.checkpoint(&j, j.block(1).unwrap()).is_err());
}
#[test]
fn partial_write_and_duplicate_os_observation_cannot_advance_or_retry() {
    let j = job();
    let d = j.block(0).unwrap();
    let mut p = FirmwareLocalProjection::for_job(&j, FirmwareObservationOrigin::MockSimulation);
    p.checkpoint(&j, d.clone()).unwrap();
    let mut s = send(&d);
    s.extent = FirmwareOsExtent::Unknown;
    p.observe_send(&s).unwrap();
    assert_eq!(p.state(), &FirmwareDerivedLocalState::Indeterminate);
    assert!(p.acknowledge_mock(&d).is_err());
    assert!(p.observe_send(&send(&d)).is_err());
    assert_eq!(p.offered_source_bytes(), 0);
}
#[test]
fn repeated_callback_order_and_native_error_stop_without_changing_raw_input() {
    let j = job();
    let mut p = FirmwareLocalProjection::for_job(&j, FirmwareObservationOrigin::LocalObservation);
    let raw = vec![0xf0, 0x7e, 0x48, 4, 0x7f, 0xf7];
    let c = FirmwareCallbackDto {
        sequence: 1,
        at_ms: 100,
        clock_origin: "opaque-clock".into(),
        epoch: SessionEpoch(1),
        binding_sha256: JobSha256::of(b"claimed").hex(),
        raw: raw.clone(),
    };
    p.observe_callback(&c).unwrap();
    p.observe_callback(&c).unwrap();
    assert_eq!(p.state(), &FirmwareDerivedLocalState::Indeterminate);
    assert_eq!(c.raw, raw);
    assert!(p.close(FirmwareHostClosure::CancelledBeforeEntry).is_err());
}
#[test]
fn closure_and_reopen_cannot_claim_device_completion() {
    let mut p = FirmwareLocalProjection::new(FirmwareObservationOrigin::LocalObservation);
    p.close(FirmwareHostClosure::ObservationFinished).unwrap();
    p.derive_reopen();
    assert_eq!(
        p.recorded_closure(),
        Some(&FirmwareHostClosure::ObservationFinished)
    );
    assert!(!p.completion_verified());
    assert!(p.close(FirmwareHostClosure::ObservationFinished).is_err());
}

#[test]
fn unknown_source_version_tag_blocks_preparation_without_rewriting_source() {
    let mut b = image([8, 3, 4, 5]);
    b[88..92].copy_from_slice(b"X999");
    b[14] = b[48..].iter().fold(0u8, |a, b| a.wrapping_add(*b));
    assert!(prepare(b, context(), 8).is_err());
}

#[test]
fn host_indeterminate_closure_and_final_mock_boundary_remain_honest() {
    let mut p = FirmwareLocalProjection::new(FirmwareObservationOrigin::LocalObservation);
    p.close(FirmwareHostClosure::HostStoppedIndeterminate)
        .unwrap();
    assert_eq!(p.state(), &FirmwareDerivedLocalState::Indeterminate);
    let j = prepare(image([2, 3, 4, 5]), context(), 8).unwrap();
    let mut p = FirmwareLocalProjection::for_job(&j, FirmwareObservationOrigin::MockSimulation);
    for d in j.blocks() {
        p.checkpoint(&j, d.clone()).unwrap();
        p.observe_send(&send(&d)).unwrap();
        p.acknowledge_mock(&d).unwrap();
    }
    assert_eq!(
        p.state(),
        &FirmwareDerivedLocalState::AwaitingFinalizeEvidence
    );
    assert!(p.simulated_awaiting_finalize());
    assert!(!p.completion_verified());
    p.derive_reopen();
    assert!(p.simulated_awaiting_finalize());
    assert!(!p.resume_supported());
}
