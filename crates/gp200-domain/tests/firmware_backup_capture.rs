use gp200_domain::*;
fn evidence() -> FirmwareCaptureEvidence {
    FirmwareCaptureEvidence {
        read: FirmwareCaptureRead::Identity(FirmwareCaptureBoundaryKind::Opening),
        sequence: 1,
        before: FirmwareCaptureFence {
            allocated: 3,
            processed: 3,
        },
        after: FirmwareCaptureFence {
            allocated: 4,
            processed: 4,
        },
        offer: Some(FirmwareCaptureOfferInterval {
            started_ns: 10,
            returned_ns: 11,
            accepted_bytes: 22,
            requested_bytes: 22,
        }),
        offered_read_bytes: FirmwareCaptureRead::Identity(FirmwareCaptureBoundaryKind::Opening)
            .request_bytes()
            .unwrap(),
        completed_ns: Some(15),
        callbacks: vec![FirmwareCaptureCallback {
            ordinal: 4,
            epoch: SessionEpoch(7),
            monotonic_ns: Some(12),
            original_len: 3,
            retained: vec![0xf0, 0x42, 0xf7],
        }],
        missing: vec![],
        raw: vec![0x12, 1],
        complete: true,
        diagnostics: vec![],
    }
}
#[test]
fn complete_observation_requires_exact_frontier_times_and_extents() {
    assert!(evidence().validate().is_ok());
    for mutate in 0..11 {
        let mut e = evidence();
        match mutate {
            0 => e.callbacks[0].ordinal = 5,
            1 => e.callbacks[0].original_len = 9,
            2 => e.callbacks[0].monotonic_ns = None,
            3 => e.offer.as_mut().unwrap().accepted_bytes = 1,
            4 => e.before.processed = 2,
            5 => e.completed_ns = Some(10),
            6 => e.after.allocated = 7,
            7 => {
                e.offered_read_bytes.pop().unwrap();
            }
            8 => e.offered_read_bytes[8] = 0x12,
            9 => e.callbacks[0].epoch = SessionEpoch(0),
            _ => {
                e.offered_read_bytes = FirmwareCaptureRead::Object {
                    object: FirmwareBackupObject::Current,
                    pass: FirmwareObservationPass::Baseline,
                }
                .request_bytes()
                .unwrap();
                e.offer.as_mut().unwrap().requested_bytes = e.offered_read_bytes.len() as u64;
                e.offer.as_mut().unwrap().accepted_bytes = e.offered_read_bytes.len() as u64;
            }
        }
        assert!(e.validate().is_err(), "Mutation {mutate} accepted");
    }
}
#[test]
fn failure_retains_missing_raw_extents_and_unknown_time_without_completed_claim() {
    let mut e = evidence();
    e.complete = false;
    e.callbacks[0].original_len = 100_000;
    e.callbacks[0].monotonic_ns = None;
    e.offer = None;
    e.offered_read_bytes.clear();
    e.completed_ns = None;
    e.missing.push(FirmwareCaptureMissingExtent {
        first_ordinal: 5,
        last_ordinal: 6,
        reason: "Queue overflow; positions allocated before rejection".into(),
    });
    e.after.allocated = 6;
    e.after.processed = 4;
    assert!(e.validate().is_ok());
    assert_eq!(
        serde_json::from_slice::<FirmwareCaptureEvidence>(&serde_json::to_vec(&e).unwrap())
            .unwrap(),
        e
    );
    e.complete = true;
    assert!(e.validate().is_err());
}
#[test]
fn callbacks_and_record_material_are_bounded_and_slot_aliases_rejected() {
    let mut e = evidence();
    e.complete = false;
    e.callbacks[0].retained = vec![1; gp200_protocol::MAX_PACKET_LENGTH + 1];
    e.callbacks[0].original_len = e.callbacks[0].retained.len() as u64;
    assert!(e.validate().is_err());
    let mut e = evidence();
    e.read = FirmwareCaptureRead::Object {
        object: FirmwareBackupObject::Stored(256),
        pass: FirmwareObservationPass::Baseline,
    };
    assert!(e.validate().is_err());
    let mut e = evidence();
    e.callbacks = vec![e.callbacks[0].clone(); FIRMWARE_CAPTURE_CHUNK_LIMIT + 1];
    assert!(e.validate().is_err());
    let mut e = evidence();
    e.read = FirmwareCaptureRead::Boundary {
        boundary: FirmwareCaptureBoundaryKind::Opening,
        object: FirmwareBackupObject::Current,
    };
    assert!(e.validate().is_err());
}

#[test]
fn each_read_address_has_one_bounded_read_packet_and_partial_offer_is_preserved() {
    for object in [
        FirmwareBackupObject::Current,
        FirmwareBackupObject::Stored(255),
        FirmwareBackupObject::Globals,
        FirmwareBackupObject::IrMetadata(19),
        FirmwareBackupObject::SnapToneMetadata(9),
    ] {
        let read = FirmwareCaptureRead::Object {
            object,
            pass: FirmwareObservationPass::Comparison,
        };
        let bytes = read.request_bytes().unwrap();
        let packet = gp200_protocol::decode_packet(&bytes).unwrap();
        assert_eq!(packet.kind, 0x11);
        assert_eq!(packet.offset, 0);
        assert_eq!(packet.total, packet.payload.len());
    }
    let mut e = evidence();
    e.complete = false;
    e.offer.as_mut().unwrap().accepted_bytes = 3;
    e.callbacks[0].epoch = SessionEpoch(6);
    assert!(e.validate().is_ok());
    e.offer.as_mut().unwrap().accepted_bytes = 23;
    assert!(e.validate().is_err());
}

#[test]
fn canonical_metadata_representation_roundtrips_every_original_byte_without_authority() {
    for command in [0x1009, 0x1012] {
        let record = gp200_protocol::RawRecord {
            command,
            body: (0..24).map(|i| (i * 11) as u8).collect(),
        };
        let bytes = firmware_capture_metadata_record_bytes(&record).unwrap();
        let reopened: gp200_protocol::RawRecord = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(reopened, record);
        assert_eq!(serde_json::to_vec(&reopened).unwrap(), bytes);
    }
    assert!(
        firmware_capture_metadata_record_bytes(&gp200_protocol::RawRecord {
            command: 0x2001,
            body: vec![0; 24]
        })
        .is_err()
    );
    assert!(
        firmware_capture_metadata_record_bytes(&gp200_protocol::RawRecord {
            command: 0x1009,
            body: vec![0; 25]
        })
        .is_err()
    );
}
