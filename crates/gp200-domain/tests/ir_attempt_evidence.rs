// Private capture acceptance suite; excluded from public source builds.
// Run with RUSTFLAGS="--cfg gp200_private_evidence" only in the retained lab checkout.
#![cfg(gp200_private_evidence)]

use gp200_domain::*;
use gp200_protocol::{READ_KIND, WRITE_KIND};
use std::{collections::BTreeMap, sync::Arc};

const WAV: &[u8] = include_bytes!("../../../tests/fixtures/device-assets/synthetic-ir-pcm24.wav");
const COEFFICIENTS: &[u8] =
    include_bytes!("../../../tests/fixtures/device-assets/synthetic-ir-coefficients.bin");
// Frozen physical observations from the consumed one-attempt receipt. Reading
// these bytes is offline; they do not authorize another operation.
const IR20_STATUS_FRAME: &[u8] = include_bytes!(
    "../../../analysis/implementation/known-source-ir-upload-preparation/root-approved-20-upload/upload-response.bin"
);
const IR20_METADATA: &[u8] = include_bytes!(
    "../../../analysis/implementation/known-source-ir-upload-preparation/root-approved-20-upload/after-ir19.payload.bin"
);

fn retained(bytes: &[u8]) -> RetainedJobBlob {
    RetainedJobBlob::checked(
        AssetId(JobSha256::of(bytes).hex()),
        Arc::from(bytes),
        JobSha256::of(bytes),
    )
    .unwrap()
}
fn prepared(name: &str) -> Result<PreparedDeviceJob, ServiceError> {
    let caps = Capabilities::gp200_read_only();
    let identity = DeviceIdentity {
        model: DeviceModel::Gp200,
        mode: BackendMode::Hardware,
        firmware: None,
        display_name: "GP-200".into(),
    };
    let selection = DeviceSelection {
        input_id: "hw:GP200,0,0".into(),
        output_id: "hw:GP200,0,0".into(),
        fingerprint_hint: None,
    };
    let raw_identity = RawTransfer::from_records(
        WRITE_KIND,
        &[RawRecord {
            command: 0x2001,
            body: vec![1, 0, 2, 0],
        }],
    )
    .unwrap();
    let session = DeviceJobSession::observe_normal(
        SessionEpoch(7),
        selection,
        Revision(1),
        &raw_identity,
        &identity,
        &caps,
    )?;
    let target = AssetSlot::new(AssetBank::Ir, 19).unwrap();
    let recipe = NormalAssetRecipe::ir(
        target,
        name.into(),
        retained(WAV),
        IrJobLengthPolicy::RejectLong,
        retained(COEFFICIENTS),
    )?;
    let options: Arc<[u8]> = Arc::from(b"{\"truncate\":false}".as_slice());
    let conversion = JobConversion::checked(
        recipe.codec_revision().into(),
        options.clone(),
        JobSha256::of(&options),
        BTreeMap::new(),
    )?;
    let mut before_name = [0; 16];
    before_name[..7].copy_from_slice(b"User IR");
    PreparedDeviceJob::prepare(
        JobId(4),
        session,
        retained(WAV),
        recipe,
        conversion,
        Arc::new(metadata(
            19,
            0,
            0,
            before_name,
            2,
            99,
            SnapshotSource::Hardware {
                epoch: SessionEpoch(7),
            },
        )),
        AssetBackupReview::Unavailable {
            reason: "Only metadata is retained".into(),
        },
    )
}

fn id(number: u128) -> IrAttemptId {
    IrAttemptId::parse(&format!("{number:032x}")).unwrap()
}
fn binding() -> IrAttemptBinding {
    let mut name = [0; 16];
    name[..13].copy_from_slice(b"Linux IR test");
    IrAttemptBinding::from_dto(IrAttemptBindingDto {
        attempt: id(1),
        job: JobId(4),
        session: IrSessionBindingDto {
            epoch: SessionEpoch(7),
            selection: DeviceSelection {
                input_id: "hw:GP200,0,0".into(),
                output_id: "hw:GP200,0,0".into(),
                fingerprint_hint: None,
            },
            identity_revision: Revision(1),
            identity_sha256: JobSha256::of(b"identity").hex(),
            session_sha256: JobSha256::of(b"session").hex(),
        },
        target: AssetSlot::new(AssetBank::Ir, 19).unwrap(),
        review_sha256: JobSha256::of(b"review").hex(),
        baseline_sha256: JobSha256::of(b"baseline").hex(),
        baseline_revision: Revision(2),
        source_sha256: JobSha256::of(b"source").hex(),
        coefficients_sha256: JobSha256::of(b"coefficients").hex(),
        payload_sha256: JobSha256::of(b"payload").hex(),
        wire_sha256: JobSha256::of(b"wire").hex(),
        codec_revision: "gp200-ir-44100-pcm24-v1".into(),
        length_policy: IrAttemptLengthPolicy::RejectLong,
        name_raw: name,
        additive_checksum: 1379,
        payload_bytes: 4124,
        wire_bytes: 8570,
        packet_count: 23,
    })
    .unwrap()
}
fn status(slot: u16, flags: u16, value: u32) -> RawTransfer {
    let mut body = slot.to_le_bytes().to_vec();
    body.extend(flags.to_le_bytes());
    body.extend(value.to_le_bytes());
    RawTransfer::from_records(
        WRITE_KIND,
        &[RawRecord {
            command: 0x100b,
            body,
        }],
    )
    .unwrap()
}
fn metadata(
    target: u16,
    flags: u16,
    sum: u32,
    name: [u8; 16],
    revision: u64,
    captured: u64,
    source: SnapshotSource,
) -> DeviceAssetMetadata {
    let mut body = target.to_le_bytes().to_vec();
    body.extend(flags.to_le_bytes());
    body.extend(sum.to_le_bytes());
    body.extend(name);
    DeviceAssetMetadata::from_transfer(
        AssetSlot::new(AssetBank::Ir, target).unwrap(),
        RequestStamp {
            epoch: SessionEpoch(7),
            intent: IntentId(12),
        },
        Revision(revision),
        source,
        captured,
        RawTransfer::from_records(
            WRITE_KIND,
            &[RawRecord {
                command: 0x1009,
                body,
            }],
        )
        .unwrap(),
    )
    .unwrap()
}
fn full_send(evidence: &mut IrAttemptEvidence) {
    evidence
        .observe_send(
            evidence.binding().context(),
            101,
            IrTransportExtent::OfferedToProcess,
            8570,
            IrSendCompletion::Process(IrProcessObservation {
                exit_code: Some(0),
                timed_out: false,
                reaped: true,
            }),
        )
        .unwrap();
}
fn good_metadata(evidence: &mut IrAttemptEvidence) {
    let pins = evidence.binding().pins();
    evidence
        .observe_metadata(
            evidence.binding().context(),
            120,
            metadata(
                19,
                0,
                pins.additive_checksum,
                pins.name_raw,
                3,
                119,
                SnapshotSource::Hardware {
                    epoch: SessionEpoch(7),
                },
            ),
        )
        .unwrap();
}

#[test]
fn checked_identifiers_and_imported_pins_reject_invalid_shapes_without_claiming_existence() {
    for bad in [
        "",
        "0",
        "00000000000000000000000000000000",
        "gggggggggggggggggggggggggggggggg",
        "../../../../../../../../../../..",
    ] {
        assert!(IrAttemptId::parse(bad).is_err());
    }
    assert_eq!(
        IrAttemptId::parse("ABCDEF0123456789ABCDEF0123456789")
            .unwrap()
            .as_str(),
        "abcdef0123456789abcdef0123456789"
    );
    let original = binding().to_dto();
    for variant in 0..7 {
        let mut dto = original.clone();
        match variant {
            0 => dto.target = AssetSlot::new(AssetBank::SnapTone, 0).unwrap(),
            1 => dto.review_sha256 = "bad".into(),
            2 => dto.wire_bytes -= 1,
            3 => dto.codec_revision = "unknown".into(),
            4 => dto.name_raw[15] = 0xff,
            5 => dto.session.selection.input_id.clear(),
            _ => dto.baseline_revision = Revision(0),
        }
        assert!(
            IrAttemptBinding::from_dto(dto).is_err(),
            "variant {variant}"
        );
    }
    // Valid descriptive pins are not checked against files or an actual endpoint.
    assert_eq!(
        IrAttemptBinding::from_dto(original.clone())
            .unwrap()
            .to_dto(),
        original
    );
}

#[test]
fn actual_ir20_shaped_observations_preserve_unknown_value_and_post_nul_drift() {
    let mut evidence = IrAttemptEvidence::start(binding(), 100);
    full_send(&mut evidence);
    let packet = gp200_protocol::decode_packet(IR20_STATUS_FRAME).unwrap();
    assert_eq!(
        (packet.total, packet.offset, packet.kind),
        (12, 0, WRITE_KIND)
    );
    let raw_status = RawTransfer::from_payload(packet.kind, packet.payload).unwrap();
    assert_eq!(raw_status, status(19, 1, 0));
    evidence
        .observe_status(evidence.binding().context(), 110, raw_status)
        .unwrap();
    let mut name = evidence.binding().pins().name_raw;
    name[14..].copy_from_slice(&[0xff, 0xff]);
    let raw_metadata = RawTransfer::from_payload(WRITE_KIND, IR20_METADATA.to_vec()).unwrap();
    let captured = DeviceAssetMetadata::from_transfer(
        AssetSlot::new(AssetBank::Ir, 19).unwrap(),
        RequestStamp {
            epoch: SessionEpoch(7),
            intent: IntentId(12),
        },
        Revision(3),
        SnapshotSource::Hardware {
            epoch: SessionEpoch(7),
        },
        119,
        raw_metadata,
    )
    .unwrap();
    evidence
        .observe_metadata(evidence.binding().context(), 120, captured)
        .unwrap();
    assert_eq!(
        evidence.statuses()[0].fields(),
        Some(IrStatusFields {
            slot_raw: 19,
            flags_raw: 1,
            value_raw: 0
        })
    );
    assert!(
        evidence.statuses()[0]
            .fields()
            .unwrap()
            .raw_flags_equal_one()
    );
    assert_eq!(
        evidence.metadata()[0].comparison(),
        IrMetadataComparison {
            full_name_matches: false,
            display_name_bytes_match: true,
            additive_checksum_matches: false
        }
    );
    assert_eq!(evidence.metadata()[0].snapshot().metadata().name_raw, name);
    let result = evidence
        .finish(IrAttemptFinishReason::ObservationComplete)
        .unwrap();
    assert_eq!(result.final_state, IrAttemptFinalState::Indeterminate);
    assert_eq!(
        result.reasons,
        vec![IrIndeterminateReason::MetadataMismatch]
    );
}

#[test]
fn unknown_full_width_status_words_do_not_change_content_assessment() {
    for (flags, value) in [(0, 0), (1, 0), (0x8001, 0xdeadbeef), (0xffff, u32::MAX)] {
        let mut evidence = IrAttemptEvidence::start(binding(), 100);
        full_send(&mut evidence);
        good_metadata(&mut evidence);
        evidence
            .observe_status(evidence.binding().context(), 110, status(19, flags, value))
            .unwrap();
        assert_eq!(evidence.statuses()[0].fields().unwrap().flags_raw, flags);
        assert_eq!(evidence.statuses()[0].fields().unwrap().value_raw, value);
        assert_eq!(
            evidence
                .finish(IrAttemptFinishReason::ObservationComplete)
                .unwrap()
                .final_state,
            IrAttemptFinalState::ContentUnverified
        );
    }
}

#[test]
fn mismatched_and_stale_statuses_remain_recorded_and_cannot_settle_the_target() {
    for variant in 0..5 {
        let mut evidence = IrAttemptEvidence::start(binding(), 100);
        full_send(&mut evidence);
        good_metadata(&mut evidence);
        let mut context = evidence.binding().context().to_dto();
        match variant {
            0 => context.attempt = id(2),
            1 => context.session.epoch = SessionEpoch(8),
            2 => context.session.selection.input_id = "other-port".into(),
            _ => {}
        }
        let slot = if variant == 3 { 18 } else { 19 };
        let at = if variant == 4 { 99 } else { 110 };
        evidence
            .observe_status(
                IrObservationContext::from_dto(context).unwrap(),
                at,
                status(slot, 1, 0),
            )
            .unwrap();
        assert!(!evidence.statuses()[0].relation().matches());
        assert!(
            evidence
                .finish(IrAttemptFinishReason::ObservationComplete)
                .unwrap()
                .reasons
                .contains(&IrIndeterminateReason::MatchingStatusMissing)
        );
    }
}

#[test]
fn duplicate_and_malformed_but_framed_replies_keep_the_exact_raw_payload() {
    let mut evidence = IrAttemptEvidence::start(binding(), 100);
    let original = status(19, 0x8001, 0x12345678);
    for at in [110, 111] {
        evidence
            .observe_status(evidence.binding().context(), at, original.clone())
            .unwrap();
    }
    assert_eq!(evidence.statuses()[1].duplicate_of(), Some(0));
    assert_eq!(evidence.statuses()[1].raw(), &original);
    let shapes = [
        RawTransfer::from_records(READ_KIND, original.records()).unwrap(),
        RawTransfer::from_records(
            WRITE_KIND,
            &[RawRecord {
                command: 0x1014,
                body: vec![0; 8],
            }],
        )
        .unwrap(),
        RawTransfer::from_records(
            WRITE_KIND,
            &[RawRecord {
                command: 0x100b,
                body: vec![0; 7],
            }],
        )
        .unwrap(),
        status(20, 1, 0),
        RawTransfer::from_records(WRITE_KIND, &[]).unwrap(),
    ];
    for raw in shapes {
        evidence
            .observe_status(evidence.binding().context(), 112, raw.clone())
            .unwrap();
        assert_eq!(evidence.statuses().last().unwrap().raw(), &raw);
        assert!(evidence.statuses().last().unwrap().shape_issue().is_some());
    }
    let json = serde_json::to_vec(&evidence.to_dto()).unwrap();
    let restored = IrAttemptEvidence::from_json(&json).unwrap();
    assert_eq!(restored.events(), evidence.events());
    assert_eq!(
        restored.finish_reason(),
        Some(IrAttemptFinishReason::InterruptedByRestart)
    );
    assert_eq!(restored.statuses()[1].duplicate_of(), Some(0));
}

#[test]
fn metadata_requires_fresh_hardware_provenance_not_name_or_checksum_alone() {
    for variant in 0..5 {
        let mut evidence = IrAttemptEvidence::start(binding(), 100);
        full_send(&mut evidence);
        evidence
            .observe_status(evidence.binding().context(), 110, status(19, 1, 0))
            .unwrap();
        let name = evidence.binding().pins().name_raw;
        let (slot, revision, captured, source) = match variant {
            0 => (
                19,
                2,
                119,
                SnapshotSource::Hardware {
                    epoch: SessionEpoch(7),
                },
            ),
            1 => (
                19,
                3,
                99,
                SnapshotSource::Hardware {
                    epoch: SessionEpoch(7),
                },
            ),
            2 => (
                19,
                3,
                121,
                SnapshotSource::Hardware {
                    epoch: SessionEpoch(7),
                },
            ),
            3 => (19, 3, 119, SnapshotSource::Simulator),
            _ => (
                18,
                3,
                119,
                SnapshotSource::Hardware {
                    epoch: SessionEpoch(7),
                },
            ),
        };
        evidence
            .observe_metadata(
                evidence.binding().context(),
                120,
                metadata(slot, 0xffff, 1379, name, revision, captured, source),
            )
            .unwrap();
        assert!(!evidence.metadata()[0].fresh());
        assert!(
            evidence
                .finish(IrAttemptFinishReason::ObservationComplete)
                .unwrap()
                .reasons
                .contains(&IrIndeterminateReason::FreshTargetMetadataMissing)
        );
    }
}

#[test]
fn incomplete_or_failed_send_and_restart_never_resume_or_become_verified() {
    for variant in 0..5 {
        let mut evidence = IrAttemptEvidence::start(binding(), 100);
        let completion = match variant {
            0 => IrSendCompletion::InProgress,
            1 => IrSendCompletion::TransportFailed,
            2 => IrSendCompletion::Process(IrProcessObservation {
                exit_code: Some(0),
                timed_out: true,
                reaped: true,
            }),
            3 => IrSendCompletion::Process(IrProcessObservation {
                exit_code: Some(0),
                timed_out: false,
                reaped: false,
            }),
            _ => IrSendCompletion::TransportReturned,
        };
        evidence
            .observe_send(
                evidence.binding().context(),
                101,
                IrTransportExtent::TransportSubmitted,
                if variant == 4 { 8569 } else { 8570 },
                completion,
            )
            .unwrap();
        evidence
            .observe_status(evidence.binding().context(), 110, status(19, 1, 0))
            .unwrap();
        good_metadata(&mut evidence);
        let summary = evidence
            .finish(IrAttemptFinishReason::ObservationComplete)
            .unwrap();
        assert_eq!(summary.final_state, IrAttemptFinalState::Indeterminate);
        assert!(
            summary
                .reasons
                .contains(&IrIndeterminateReason::SendPartialOrFailed),
            "variant {variant}"
        );
        let mut imported =
            IrAttemptEvidence::from_json(&serde_json::to_vec(&evidence.to_dto()).unwrap()).unwrap();
        assert!(
            imported
                .observe_status(imported.binding().context(), 130, status(19, 1, 0))
                .is_err()
        );
        assert!(
            imported
                .finish(IrAttemptFinishReason::ObservationComplete)
                .is_err()
        );
    }
}

#[test]
fn earlier_full_accounting_cannot_mask_later_failure_partial_or_in_progress_accounting() {
    for (count, completion) in [
        (8570, IrSendCompletion::TransportFailed),
        (8569, IrSendCompletion::TransportReturned),
        (8570, IrSendCompletion::InProgress),
    ] {
        let mut evidence = IrAttemptEvidence::start(binding(), 100);
        full_send(&mut evidence);
        evidence
            .observe_send(
                evidence.binding().context(),
                102,
                IrTransportExtent::TransportSubmitted,
                count,
                completion,
            )
            .unwrap();
        evidence
            .observe_status(evidence.binding().context(), 110, status(19, 1, 0))
            .unwrap();
        good_metadata(&mut evidence);
        assert!(evidence.statuses()[0].relation().matches());
        assert!(evidence.statuses()[0].after_accounted_send());
        assert!(evidence.metadata()[0].fresh());
        assert!(evidence.metadata()[0].comparison().all_match());
        let summary = evidence
            .finish(IrAttemptFinishReason::ObservationComplete)
            .unwrap();
        assert_eq!(summary.final_state, IrAttemptFinalState::Indeterminate);
        assert_eq!(
            summary.reasons,
            vec![IrIndeterminateReason::SendPartialOrFailed]
        );
        assert_eq!(
            IrAttemptEvidence::from_json(&serde_json::to_vec(&evidence.to_dto()).unwrap())
                .unwrap()
                .summary(),
            Some(summary)
        );
    }
}

#[test]
fn additive_collision_and_matching_metadata_still_cannot_prove_content() {
    let mut a = vec![0_u8; 4096];
    let mut b = a.clone();
    a[..4].copy_from_slice(&1_i32.to_le_bytes());
    a[4..8].copy_from_slice(&2_i32.to_le_bytes());
    b[..4].copy_from_slice(&2_i32.to_le_bytes());
    b[4..8].copy_from_slice(&1_i32.to_le_bytes());
    let sum = |bytes: &[u8]| bytes.iter().map(|value| u32::from(*value)).sum::<u32>();
    assert_eq!(sum(&a), sum(&b));
    assert_ne!(a, b);
    assert_ne!(JobSha256::of(&a), JobSha256::of(&b));
    let mut evidence = IrAttemptEvidence::start(binding(), 100);
    evidence
        .observe_send(
            evidence.binding().context(),
            101,
            IrTransportExtent::TransportSubmitted,
            8570,
            IrSendCompletion::TransportReturned,
        )
        .unwrap();
    evidence
        .observe_status(evidence.binding().context(), 110, status(19, 1, 1379))
        .unwrap();
    good_metadata(&mut evidence);
    assert_eq!(
        evidence
            .finish(IrAttemptFinishReason::ObservationComplete)
            .unwrap()
            .final_state,
        IrAttemptFinalState::ContentUnverified
    );
    assert_eq!(
        IrAttemptEvidence::from_json(&serde_json::to_vec(&evidence.to_dto()).unwrap())
            .unwrap()
            .summary(),
        evidence.summary()
    );
}

#[test]
fn actual_checked_preparation_binds_exact_recipe_and_still_cannot_enable_asset_executor() {
    let job = prepared("Prepared IR").unwrap();
    let checked = IrAttemptBinding::from_prepared(id(8), &job).unwrap();
    let pins = checked.pins();
    assert_eq!(pins.source_sha256, JobSha256::of(WAV).hex());
    assert_eq!(pins.coefficients_sha256, JobSha256::of(COEFFICIENTS).hex());
    assert_eq!(pins.additive_checksum, 2041);
    assert_eq!(pins.review_sha256, job.review_sha256().hex());
    assert_eq!(
        pins.baseline_sha256,
        JobSha256::of(job.baseline().raw().payload()).hex()
    );
    let candidate =
        gp200_protocol::asset_upload_candidate(job.target(), job.recipe().name(), COEFFICIENTS)
            .unwrap();
    let packets = gp200_protocol::encode_transfer(&candidate).unwrap();
    let wire: Vec<_> = packets.iter().flatten().copied().collect();
    assert_eq!(
        pins.payload_sha256,
        JobSha256::of(candidate.payload()).hex()
    );
    assert_eq!(pins.wire_sha256, JobSha256::of(&wire).hex());
    assert_eq!(
        (pins.payload_bytes, pins.wire_bytes, pins.packet_count),
        (4124, 8570, 23)
    );
    let caps = Capabilities::gp200_read_only();
    assert_eq!(
        ValidatedDeviceJob::validate(
            job.clone(),
            job.review_sha256(),
            job.session(),
            job.session().identity(),
            &caps,
            job.baseline()
        )
        .unwrap_err()
        .kind,
        ErrorKind::Unverified
    );
}

#[test]
fn imported_intended_names_have_the_exact_checked_recipe_constraints() {
    for byte in 0_u8..=127 {
        let name = String::from_utf8(vec![byte]).unwrap();
        let expected = prepared(&name).is_ok();
        let mut dto = binding().to_dto();
        dto.name_raw = [0; 16];
        dto.name_raw[0] = byte;
        assert_eq!(
            IrAttemptBinding::from_dto(dto).is_ok(),
            expected,
            "ASCII {byte}"
        );
    }
    for name in [
        "1234567890123456",
        " }",
        " ",
        "12345678901234567",
        "é",
        "~",
        "",
        "a\0b",
    ] {
        let expected = prepared(name).is_ok();
        if name.len() <= 16 {
            let mut dto = binding().to_dto();
            dto.name_raw = [0; 16];
            dto.name_raw[..name.len()].copy_from_slice(name.as_bytes());
            assert_eq!(
                IrAttemptBinding::from_dto(dto).is_ok(),
                expected,
                "{name:?}"
            );
        } else {
            assert!(!expected);
        }
    }
}

#[test]
fn pre_send_status_cannot_be_promoted_by_a_later_completed_send_or_duplicate() {
    let mut evidence = IrAttemptEvidence::start(binding(), 100);
    let raw = status(19, 1, 0);
    evidence
        .observe_status(evidence.binding().context(), 105, raw.clone())
        .unwrap();
    evidence
        .observe_send(
            evidence.binding().context(),
            110,
            IrTransportExtent::TransportSubmitted,
            8570,
            IrSendCompletion::TransportReturned,
        )
        .unwrap();
    assert!(evidence.statuses()[0].relation().matches());
    assert!(!evidence.statuses()[0].after_accounted_send());
    evidence
        .observe_status(evidence.binding().context(), 115, raw)
        .unwrap();
    assert!(evidence.statuses()[1].after_accounted_send());
    assert_eq!(evidence.statuses()[1].duplicate_of(), Some(0));
    good_metadata(&mut evidence);
    let summary = evidence
        .finish(IrAttemptFinishReason::ObservationComplete)
        .unwrap();
    assert_eq!(
        summary.reasons,
        vec![IrIndeterminateReason::MatchingStatusMissing]
    );
    let imported =
        IrAttemptEvidence::from_json(&serde_json::to_vec(&evidence.to_dto()).unwrap()).unwrap();
    assert!(!imported.statuses()[0].after_accounted_send());
    assert_eq!(imported.summary(), Some(summary));
    // Even when recorded after a send, an earlier timestamp stays pre-send.
    let mut earlier = IrAttemptEvidence::start(binding(), 100);
    full_send(&mut earlier);
    earlier
        .observe_status(earlier.binding().context(), 100, status(19, 1, 0))
        .unwrap();
    assert!(!earlier.statuses()[0].after_accounted_send());
}

#[test]
fn pre_send_metadata_and_unknown_fields_remain_observations_without_occupancy() {
    let mut evidence = IrAttemptEvidence::start(binding(), 100);
    good_metadata(&mut evidence);
    assert!(!evidence.metadata()[0].fresh());
    full_send(&mut evidence);
    assert!(!evidence.metadata()[0].fresh());
    let name = evidence.binding().pins().name_raw;
    evidence
        .observe_metadata(
            evidence.binding().context(),
            122,
            metadata(
                19,
                0xffff,
                1379,
                name,
                4,
                121,
                SnapshotSource::Hardware {
                    epoch: SessionEpoch(7),
                },
            ),
        )
        .unwrap();
    assert!(evidence.metadata()[1].fresh());
    assert_eq!(
        evidence.metadata()[1].snapshot().metadata().flags_raw,
        0xffff
    );
    assert!(evidence.metadata()[1].comparison().all_match());
}

#[test]
fn bounded_reconstruction_rejects_corruption_and_keeps_unfinished_import_closed() {
    let mut evidence = IrAttemptEvidence::start(binding(), 100);
    assert!(
        evidence
            .observe_send(
                evidence.binding().context(),
                101,
                IrTransportExtent::OfferedToProcess,
                8571,
                IrSendCompletion::InProgress
            )
            .is_err()
    );
    let oversized = RawTransfer::from_records(
        WRITE_KIND,
        &[RawRecord {
            command: 0x100b,
            body: vec![0; 4093],
        }],
    )
    .unwrap();
    assert!(
        evidence
            .observe_status(evidence.binding().context(), 101, oversized)
            .is_err()
    );
    for at in 0..256 {
        evidence
            .observe_status(evidence.binding().context(), at, status(19, 1, 0))
            .unwrap();
    }
    assert!(
        evidence
            .observe_status(evidence.binding().context(), 257, status(19, 1, 0))
            .is_err()
    );
    let mut dto = evidence.to_dto();
    dto.events.push(dto.events[0].clone());
    assert!(IrAttemptEvidence::reconstruct(dto).is_err());
    let clean = IrAttemptEvidence::start(binding(), 100).to_dto();
    for variant in 0..5 {
        let mut corrupt = clean.clone();
        match variant {
            0 => corrupt.schema = 2,
            1 => corrupt.binding.payload_sha256 = "claim".into(),
            2 => corrupt.binding.packet_count = 24,
            3 => corrupt.binding.session.identity_revision = Revision(0),
            _ => {
                corrupt.binding.session.selection.fingerprint_hint = Some(DeviceFingerprint {
                    vendor: None,
                    product: None,
                    serial: None,
                    hint: "x".repeat(17000),
                })
            }
        }
        assert!(IrAttemptEvidence::reconstruct(corrupt).is_err());
    }
    assert!(IrAttemptEvidence::from_json(&vec![b' '; 2 * 1024 * 1024 + 1]).is_err());
    assert!(IrAttemptEvidence::from_json(b"{bad").is_err());
    let mut invalid_slot = serde_json::to_value(&clean).unwrap();
    invalid_slot["binding"]["target"]["index"] = serde_json::json!(20);
    assert!(IrAttemptEvidence::from_json(&serde_json::to_vec(&invalid_slot).unwrap()).is_err());
    let mut imported = IrAttemptEvidence::from_json(&serde_json::to_vec(&clean).unwrap()).unwrap();
    assert_eq!(
        imported.finish_reason(),
        Some(IrAttemptFinishReason::InterruptedByRestart)
    );
    assert_eq!(
        imported.summary().unwrap().final_state,
        IrAttemptFinalState::Indeterminate
    );
    assert!(
        imported
            .observe_send(
                imported.binding().context(),
                101,
                IrTransportExtent::TransportSubmitted,
                8570,
                IrSendCompletion::TransportReturned
            )
            .is_err()
    );
}
