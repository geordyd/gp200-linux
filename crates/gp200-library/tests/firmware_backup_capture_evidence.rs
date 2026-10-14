use gp200_domain::*;
use gp200_library::*;
use std::{
    fs,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};
struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        let p = std::env::temp_dir().join(format!(
            "fw04e-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&p).unwrap();
        Self(p)
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn reservation() -> FirmwareCaptureReservation {
    FirmwareCaptureReservation {
        actor_run: "actor-original".into(),
        reservation: "reservation-original".into(),
        normal_owner: "guard-original".into(),
        epoch: SessionEpoch(7),
        usb: FirmwareUsbObservation {
            topology: "1-2".into(),
            vendor: 0x84ef,
            product: 0x002a,
            serial: "fixed-serial-not-identity-proof".into(),
            interface: "midi".into(),
            endpoint: "raw0".into(),
        },
        clock_origin: "actual-test-clock".into(),
        prepared_monotonic_ns: 0,
        prepared_wall_millis: 1,
        initial_fence: FirmwareCaptureFence {
            allocated: 0,
            processed: 0,
        },
    }
}
fn identity() -> Vec<u8> {
    vec![0x12, 0x01, 0x20, 0x04, 0x00, 0x01, 0x00, 0x02, 0x00]
}
fn evidence(kind: FirmwareCaptureBoundaryKind, sequence: u64) -> FirmwareCaptureEvidence {
    FirmwareCaptureEvidence {
        read: FirmwareCaptureRead::Identity(kind),
        sequence,
        before: FirmwareCaptureFence {
            allocated: sequence - 1,
            processed: sequence - 1,
        },
        after: FirmwareCaptureFence {
            allocated: sequence,
            processed: sequence,
        },
        offer: Some(FirmwareCaptureOfferInterval {
            started_ns: sequence * 10,
            returned_ns: sequence * 10 + 1,
            accepted_bytes: 22,
            requested_bytes: 22,
        }),
        offered_read_bytes: FirmwareCaptureRead::Identity(FirmwareCaptureBoundaryKind::Opening)
            .request_bytes()
            .unwrap(),
        completed_ns: Some(sequence * 10 + 3),
        callbacks: vec![FirmwareCaptureCallback {
            ordinal: sequence,
            epoch: SessionEpoch(7),
            monotonic_ns: Some(sequence * 10 + 2),
            original_len: 4,
            retained: vec![0xf0, 0x11, sequence as u8, 0xf7],
        }],
        missing: vec![],
        raw: identity(),
        complete: true,
        diagnostics: vec![],
    }
}
fn origin() -> BackupOrigin {
    BackupOrigin {
        identity: DeviceIdentity {
            model: DeviceModel::Gp200,
            mode: BackendMode::Hardware,
            firmware: None,
            display_name: "GP-200".into(),
        },
        epoch: SessionEpoch(7),
        device_key: "descriptive-device-key".into(),
        evidence: vec![],
    }
}
fn boundary(sequence: u64) -> FirmwareArchiveBoundary {
    FirmwareArchiveBoundary {
        epoch: SessionEpoch(7),
        identity: origin().identity,
        device_key: origin().device_key,
        sequence,
        monotonic_millis: sequence,
        wall_clock_millis: sequence,
        raw_identity: identity(),
    }
}
fn begin() -> FirmwareBackupBegin {
    FirmwareBackupBegin {
        origin: origin(),
        opening: boundary(1),
        context: FirmwareBackupReviewContext {
            device: FirmwareDeviceAssociation::named_local_record(
                "GP-200",
                "caller reported, no physical proof",
            )
            .unwrap(),
            manual_version: None,
            destination: FirmwareBackupDestination {
                model: "GP-200".into(),
                version: FirmwareVersion::new(1, 8, 2).unwrap(),
                image_sha256: "a".repeat(64),
                restoration_profile: None,
            },
            local_material: vec![],
            preservation_uncertainty: "Unknown".into(),
            recovery_limitations: "Unknown".into(),
        },
    }
}
fn wait(c: &mut FirmwareBackupClient) -> FirmwareBackupEvent {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(e) = c.try_next_event() {
            return e;
        }
        assert!(
            Instant::now() < deadline,
            "Missing result {:?}",
            c.terminal_failure()
        );
        std::thread::sleep(Duration::from_millis(1));
    }
}
fn submit(
    c: &mut FirmwareBackupClient,
    a: FirmwareBackupAction,
) -> Box<FirmwareCaptureDurableResult> {
    c.try_submit(a).ok().unwrap();
    match wait(c).result {
        Ok(FirmwareBackupResult::CaptureDurable(r)) => r,
        _ => panic!("No genuine capture durability result"),
    }
}
fn consume(r: FirmwareBackupDurableReceipt) {
    let d = r.description().clone();
    r.try_with_first_offer(&d, || ()).unwrap();
}
fn review(r: FirmwareBackupDurableReceipt) {
    let d = r.description().clone();
    r.try_consume_for_review(&d).unwrap();
}
#[test]
fn actual_single_writer_preserves_raw_boundaries_closes_publishes_and_revalidates() {
    let t = Temp::new();
    let mut w = LibraryWorker::open(&t.0, Arc::new(|| {})).unwrap();
    let mut c = w.take_firmware_backup_client().unwrap();
    assert!(w.take_firmware_backup_client().is_none());
    assert!(LibraryStore::open(&t.0).is_err());
    let r = submit(&mut c, FirmwareBackupAction::PrepareCapture(reservation()));
    assert_eq!(
        r.receipt.description().kind,
        FirmwareCaptureReceiptKind::Prepared
    );
    let id = r.receipt.description().capture_id.clone();
    let dir = t.0.join("firmware-capture-evidence").join(&id);
    let raw = fs::read(dir.join("record-0000.json")).unwrap();
    let json: serde_json::Value = serde_json::from_slice(&raw).unwrap();
    assert_eq!(json["preparation"]["normal_owner"], "guard-original");
    assert_eq!(json["evidence"], serde_json::Value::Null);
    let independently_prepared = FirmwareCaptureRecordInspection {
        schema: FIRMWARE_CAPTURE_EVIDENCE_SCHEMA.into(),
        capture_id: id.clone(),
        worker_run: c.run_id().into(),
        worker_sequence: 1,
        acquisition: None,
        kind: FirmwareCaptureReceiptKind::Prepared,
        previous_sha256: ContentHash::of(&[]).as_str().into(),
        preparation: Some(reservation()),
        evidence: None,
        cancelled: false,
    };
    assert_eq!(
        canonical_firmware_capture_record_bytes(&independently_prepared).unwrap(),
        raw
    );
    assert_eq!(
        firmware_capture_record_hash(&independently_prepared)
            .unwrap()
            .as_str(),
        r.receipt.description().evidence_sha256
    );
    let mut invalid_preparation = independently_prepared.clone();
    invalid_preparation.kind = FirmwareCaptureReceiptKind::Begun;
    assert!(firmware_capture_record_hash(&invalid_preparation).is_err());

    assert!(matches!(
        c.try_submit(FirmwareBackupAction::PublishCaptured),
        Err(SubmitError::Busy(_))
    ));
    consume(r.receipt);
    let r = submit(
        &mut c,
        FirmwareBackupAction::BeginCaptured(Box::new(FirmwareCapturedBegin {
            begin: begin(),
            evidence: evidence(FirmwareCaptureBoundaryKind::Opening, 1),
        })),
    );
    consume(r.receipt);
    let middle = evidence(FirmwareCaptureBoundaryKind::Middle, 2);
    let r = submit(
        &mut c,
        FirmwareBackupAction::StoreCaptureBoundary(Box::new(middle.clone())),
    );
    let independently_middle = FirmwareCaptureRecordInspection {
        schema: FIRMWARE_CAPTURE_EVIDENCE_SCHEMA.into(),
        capture_id: id.clone(),
        worker_run: c.run_id().into(),
        worker_sequence: 3,
        acquisition: r.receipt.description().acquisition.clone(),
        kind: FirmwareCaptureReceiptKind::Boundary,
        previous_sha256: ContentHash::of(&fs::read(dir.join("record-0001.json")).unwrap())
            .as_str()
            .into(),
        preparation: None,
        evidence: Some(middle.clone()),
        cancelled: false,
    };
    let canonical = canonical_firmware_capture_record_bytes(&independently_middle).unwrap();
    assert_eq!(canonical, fs::read(dir.join("record-0002.json")).unwrap());
    assert_eq!(
        firmware_capture_record_hash(&independently_middle)
            .unwrap()
            .as_str(),
        r.receipt.description().evidence_sha256
    );
    let mut altered = independently_middle;
    altered.evidence.as_mut().unwrap().callbacks[0].retained[1] ^= 1;
    assert_ne!(
        firmware_capture_record_hash(&altered).unwrap().as_str(),
        r.receipt.description().evidence_sha256
    );
    consume(r.receipt);
    let final_e = evidence(FirmwareCaptureBoundaryKind::Final, 3);
    let r = submit(
        &mut c,
        FirmwareBackupAction::StoreCaptureBoundary(Box::new(final_e.clone())),
    );
    review(r.receipt);
    let r = submit(
        &mut c,
        FirmwareBackupAction::CloseCaptured {
            boundary: boundary(3),
            outcome: BackupAcquisition::Cancelled,
        },
    );
    review(r.receipt);
    let mut r = submit(&mut c, FirmwareBackupAction::PublishCaptured);
    review(r.receipt);
    let publication = r.publication.take().unwrap();
    assert!(publication.summary().cancelled);
    assert!(!publication.summary().complete_restorable_backup());
    assert!(!publication.summary().permits_flash());
    let index = publication.index_path().to_owned();
    let summary = reopen_firmware_capture_evidence(&index).unwrap();
    assert_eq!(&summary, publication.summary());
    assert_eq!(summary.records, 5);
    let middle_record: serde_json::Value =
        serde_json::from_slice(&fs::read(dir.join("record-0002.json")).unwrap()).unwrap();
    assert_eq!(
        middle_record["evidence"],
        serde_json::to_value(middle).unwrap()
    );
    let record: serde_json::Value =
        serde_json::from_slice(&fs::read(dir.join("record-0003.json")).unwrap()).unwrap();
    assert_eq!(record["evidence"], serde_json::to_value(final_e).unwrap());
    assert!(
        c.capture_retirement_blockers()
            .iter()
            .any(|s| s.contains("revalidated"))
    );
    let mut r = submit(
        &mut c,
        FirmwareBackupAction::RevalidateCaptured(publication),
    );
    assert!(
        c.capture_retirement_blockers()
            .iter()
            .any(|s| s.contains("receipt"))
    );
    review(r.receipt);
    assert!(c.capture_retirement_blockers().is_empty());
    let publication = r.publication.take().unwrap();
    publication.revalidate().unwrap();
    // Independently reconstruct all attacker-controlled pins: correlation must still fail.
    let original_index = fs::read(&index).unwrap();
    let mut mutable_index: serde_json::Value = serde_json::from_slice(&original_index).unwrap();
    let mut originals = Vec::new();
    let mut previous = ContentHash::of(&[]).as_str().to_owned();
    let mut total = 0u64;
    for (i, pin) in mutable_index["records"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .enumerate()
    {
        let path = dir.join(pin["name"].as_str().unwrap());
        let original = fs::read(&path).unwrap();
        let mut record: serde_json::Value = serde_json::from_slice(&original).unwrap();
        record["previous_sha256"] = previous.into();
        if i == 2 {
            record["evidence"]["callbacks"][0]["epoch"] = 6.into();
        }
        // Preserve struct field order, independently deserialize and serialize exact record bytes.
        let record: FirmwareCaptureRecordInspection = serde_json::from_value(record).unwrap();
        let bytes = serde_json::to_vec(&record).unwrap();
        previous = ContentHash::of(&bytes).as_str().into();
        pin["sha256"] = previous.clone().into();
        pin["bytes"] = (bytes.len() as u64).into();
        total += bytes.len() as u64;
        fs::write(&path, &bytes).unwrap();
        originals.push((path, original));
    }
    mutable_index["record_bytes"] = total.into();
    // The private index type has a fixed canonical order, so retain the original order manually.
    let original_text = String::from_utf8(original_index.clone()).unwrap();
    let mut rewritten = original_text;
    let original_value: serde_json::Value = serde_json::from_slice(&original_index).unwrap();
    for (before, after) in original_value["records"]
        .as_array()
        .unwrap()
        .iter()
        .zip(mutable_index["records"].as_array().unwrap())
    {
        rewritten = rewritten.replace(
            before["sha256"].as_str().unwrap(),
            after["sha256"].as_str().unwrap(),
        );
    }
    // All epoch values are single digit, preserving exact original record lengths and index totals.
    assert_eq!(
        mutable_index["record_bytes"],
        original_value["record_bytes"]
    );
    fs::write(&index, rewritten).unwrap();
    let error = reopen_firmware_capture_evidence(&index).unwrap_err();
    assert!(error.message.contains("callback session differs"));
    for (path, original) in originals {
        fs::write(path, original).unwrap();
    }
    fs::write(&index, original_index).unwrap();
    publication.revalidate().unwrap();
    let mut changed = fs::read(dir.join("record-0002.json")).unwrap();
    changed[0] = b' ';
    fs::write(dir.join("record-0002.json"), changed).unwrap();
    assert!(reopen_firmware_capture_evidence(&index).is_err());
    assert!(publication.revalidate().is_err());
}
#[test]
fn cancelled_after_dequeue_receipt_cannot_invoke_first_offer() {
    let t = Temp::new();
    let mut w = LibraryWorker::open(&t.0, Arc::new(|| {})).unwrap();
    let mut c = w.take_firmware_backup_client().unwrap();
    let r = submit(&mut c, FirmwareBackupAction::PrepareCapture(reservation()));
    let d = r.receipt.description().clone();
    c.cancel();
    let mut offered = false;
    assert!(
        r.receipt
            .try_with_first_offer(&d, || offered = true)
            .is_err()
    );
    assert!(!offered);
    assert!(!c.capture_retirement_blockers().is_empty());
}
#[test]
fn matching_serializable_description_does_not_replace_genuine_receipt_and_wrong_binding_fails() {
    let t = Temp::new();
    let mut w = LibraryWorker::open(&t.0, Arc::new(|| {})).unwrap();
    let mut c = w.take_firmware_backup_client().unwrap();
    let r = submit(&mut c, FirmwareBackupAction::PrepareCapture(reservation()));
    let mut d: FirmwareCaptureReceiptDescription =
        serde_json::from_slice(&serde_json::to_vec(r.receipt.description()).unwrap()).unwrap();
    d.reservation.normal_owner = "other guard".into();
    let mut offered = false;
    assert!(
        r.receipt
            .try_with_first_offer(&d, || offered = true)
            .is_err()
    );
    assert!(!offered);
    assert!(c.terminal_failure().is_some());
}
#[test]
fn lost_receipt_or_lost_consumer_revokes_dequeued_success() {
    for lost_client in [false, true] {
        let t = Temp::new();
        let mut w = LibraryWorker::open(&t.0, Arc::new(|| {})).unwrap();
        let mut c = w.take_firmware_backup_client().unwrap();
        let r = submit(&mut c, FirmwareBackupAction::PrepareCapture(reservation()));
        let d = r.receipt.description().clone();
        if lost_client {
            drop(c);
            let mut offered = false;
            assert!(
                r.receipt
                    .try_with_first_offer(&d, || offered = true)
                    .is_err()
            );
            assert!(!offered);
        } else {
            drop(r);
            assert!(c.terminal_failure().is_some());
            assert!(matches!(
                c.try_submit(FirmwareBackupAction::PrepareCapture(reservation())),
                Err(SubmitError::Disconnected(_))
            ));
        }
    }
}
#[test]
fn failure_original_prefix_and_missing_extents_are_independently_reread() {
    let t = Temp::new();
    let mut w = LibraryWorker::open(&t.0, Arc::new(|| {})).unwrap();
    let mut c = w.take_firmware_backup_client().unwrap();
    let r = submit(&mut c, FirmwareBackupAction::PrepareCapture(reservation()));
    let id = r.receipt.description().capture_id.clone();
    consume(r.receipt);
    let mut e = evidence(FirmwareCaptureBoundaryKind::Opening, 1);
    e.complete = false;
    e.callbacks[0].original_len = 100_000;
    e.callbacks[0].monotonic_ns = None;
    e.after = FirmwareCaptureFence {
        allocated: 5,
        processed: 1,
    };
    e.missing.push(FirmwareCaptureMissingExtent {
        first_ordinal: 2,
        last_ordinal: 5,
        reason: "Overflow after source ordinal allocation".into(),
    });
    e.offer = None;
    e.offered_read_bytes.clear();
    e.completed_ns = None;
    let r = submit(
        &mut c,
        FirmwareBackupAction::StoreCaptureFailure(Box::new(e.clone())),
    );
    review(r.receipt);
    let raw = fs::read(
        t.0.join("firmware-capture-evidence")
            .join(id)
            .join("record-0001.json"),
    )
    .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&raw).unwrap();
    assert_eq!(v["evidence"], serde_json::to_value(e).unwrap());
    assert!(
        c.capture_retirement_blockers()
            .iter()
            .any(|s| s.contains("not closed"))
    );
}

fn preset_raw() -> Vec<u8> {
    let mut records = RawTransfer::from_payload(
        0x12,
        include_bytes!("../../../assets/demo/read-current-payload.bin").to_vec(),
    )
    .unwrap()
    .records()
    .to_vec();
    let module = records
        .iter_mut()
        .find(|r| r.command == 0x14 && r.body[0] == 3)
        .unwrap();
    module.body[60..64].copy_from_slice(&0x7fc1_2345u32.to_le_bytes());
    module.body[64..68].copy_from_slice(&0x8000_0000u32.to_le_bytes());
    records.push(RawRecord {
        command: 0x6666,
        body: vec![0xff, 0x80, 0, 99],
    });
    let raw = RawTransfer::from_records(0x12, &records).unwrap();
    [vec![0x12], raw.payload().to_vec()].concat()
}
fn globals_raw() -> Vec<u8> {
    [
        vec![0x12],
        include_bytes!("../../../tests/fixtures/globals/baseline.payload.bin").to_vec(),
    ]
    .concat()
}
fn object_evidence(
    object: FirmwareBackupObject,
    pass: FirmwareObservationPass,
    seq: u64,
    raw: Vec<u8>,
) -> FirmwareCaptureEvidence {
    let mut e = evidence(FirmwareCaptureBoundaryKind::Middle, seq);
    e.read = FirmwareCaptureRead::Object { object, pass };
    e.offered_read_bytes = e.read.request_bytes().unwrap();
    e.offer.as_mut().unwrap().requested_bytes = e.offered_read_bytes.len() as u64;
    e.offer.as_mut().unwrap().accepted_bytes = e.offered_read_bytes.len() as u64;
    e.raw = raw;
    e
}
fn store_object(c: &mut FirmwareBackupClient, e: FirmwareCaptureEvidence) {
    let (object, pass) = match e.read {
        FirmwareCaptureRead::Object { object, pass } => (object, pass),
        _ => panic!("Wrong fixture"),
    };
    let observation = FirmwareArchiveObservation {
        object,
        pass,
        epoch: SessionEpoch(7),
        revision: Revision(e.sequence),
        sequence: e.sequence,
        monotonic_millis: e.sequence,
        wall_clock_millis: e.sequence,
        bytes: e.raw.clone(),
    };
    let r = submit(
        c,
        FirmwareBackupAction::StoreCaptured(Box::new(FirmwareCapturedObservation {
            observation,
            evidence: e,
        })),
    );
    consume(r.receipt);
}
#[test]
fn real_archive_store_and_middle_current_globals_preserve_hidden_bits_and_strict_comparison() {
    let t = Temp::new();
    let mut w = LibraryWorker::open(&t.0, Arc::new(|| {})).unwrap();
    let mut c = w.take_firmware_backup_client().unwrap();
    let r = submit(&mut c, FirmwareBackupAction::PrepareCapture(reservation()));
    let id = r.receipt.description().capture_id.clone();
    consume(r.receipt);
    let r = submit(
        &mut c,
        FirmwareBackupAction::BeginCaptured(Box::new(FirmwareCapturedBegin {
            begin: begin(),
            evidence: evidence(FirmwareCaptureBoundaryKind::Opening, 1),
        })),
    );
    consume(r.receipt);
    let current = preset_raw();
    let globals = globals_raw();
    store_object(
        &mut c,
        object_evidence(
            FirmwareBackupObject::Current,
            FirmwareObservationPass::Baseline,
            2,
            current.clone(),
        ),
    );
    store_object(
        &mut c,
        object_evidence(
            FirmwareBackupObject::Globals,
            FirmwareObservationPass::Baseline,
            3,
            globals.clone(),
        ),
    );
    for (seq, object, raw) in [
        (4, FirmwareBackupObject::Current, current.clone()),
        (5, FirmwareBackupObject::Globals, globals.clone()),
    ] {
        let mut e = evidence(FirmwareCaptureBoundaryKind::Middle, seq);
        e.read = FirmwareCaptureRead::Boundary {
            boundary: FirmwareCaptureBoundaryKind::Middle,
            object,
        };
        e.offered_read_bytes = e.read.request_bytes().unwrap();
        e.offer.as_mut().unwrap().requested_bytes = e.offered_read_bytes.len() as u64;
        e.offer.as_mut().unwrap().accepted_bytes = e.offered_read_bytes.len() as u64;
        e.raw = raw;
        let r = submit(
            &mut c,
            FirmwareBackupAction::StoreCaptureBoundary(Box::new(e)),
        );
        consume(r.receipt);
    }
    let r = submit(
        &mut c,
        FirmwareBackupAction::StoreCaptureBoundary(Box::new(evidence(
            FirmwareCaptureBoundaryKind::Middle,
            6,
        ))),
    );
    consume(r.receipt);
    let mut records = RawTransfer::from_payload(0x12, current[1..].to_vec())
        .unwrap()
        .records()
        .to_vec();
    records.iter_mut().find(|r| r.command == 2).unwrap().body[15] ^= 1;
    let changed = RawTransfer::from_records(0x12, &records).unwrap();
    let changed = [vec![0x12], changed.payload().to_vec()].concat();
    store_object(
        &mut c,
        object_evidence(
            FirmwareBackupObject::Current,
            FirmwareObservationPass::Comparison,
            7,
            changed.clone(),
        ),
    );
    store_object(
        &mut c,
        object_evidence(
            FirmwareBackupObject::Globals,
            FirmwareObservationPass::Comparison,
            8,
            globals.clone(),
        ),
    );
    let r = submit(
        &mut c,
        FirmwareBackupAction::StoreCaptureBoundary(Box::new(evidence(
            FirmwareCaptureBoundaryKind::Final,
            9,
        ))),
    );
    review(r.receipt);
    let r = submit(
        &mut c,
        FirmwareBackupAction::CloseCaptured {
            boundary: boundary(9),
            outcome: BackupAcquisition::Cancelled,
        },
    );
    review(r.receipt);
    let mut r = submit(&mut c, FirmwareBackupAction::PublishCaptured);
    review(r.receipt);
    let publication = r.publication.take().unwrap();
    assert!(!publication.archive().summary().comparison_complete);
    let directory = t.0.join("firmware-capture-evidence").join(id);
    let baseline = reopen_firmware_capture_record(&directory.join("record-0002.json")).unwrap();
    assert_eq!(baseline.evidence.unwrap().raw, current);
    let middle = reopen_firmware_capture_record(&directory.join("record-0004.json")).unwrap();
    assert_eq!(middle.evidence.unwrap().raw, preset_raw());
    let middle = reopen_firmware_capture_record(&directory.join("record-0005.json")).unwrap();
    assert_eq!(middle.evidence.unwrap().raw, globals);
    let comparison = reopen_firmware_capture_record(&directory.join("record-0007.json")).unwrap();
    assert_eq!(comparison.evidence.unwrap().raw, changed);
    let sidecar: serde_json::Value =
        serde_json::from_slice(&fs::read(publication.archive().sidecar_path()).unwrap()).unwrap();
    assert_eq!(
        sidecar["equality_policy"],
        "gp200-fw04-a-archival-exact-bytes-v1"
    );
    publication.revalidate().unwrap();
}

#[test]
fn finished_closure_requires_final_identity_and_final_blocks_new_completed_reads() {
    for use_final in [false, true] {
        let t = Temp::new();
        let mut w = LibraryWorker::open(&t.0, Arc::new(|| {})).unwrap();
        let mut c = w.take_firmware_backup_client().unwrap();
        let r = submit(&mut c, FirmwareBackupAction::PrepareCapture(reservation()));
        let id = r.receipt.description().capture_id.clone();
        consume(r.receipt);
        let r = submit(
            &mut c,
            FirmwareBackupAction::BeginCaptured(Box::new(FirmwareCapturedBegin {
                begin: begin(),
                evidence: evidence(FirmwareCaptureBoundaryKind::Opening, 1),
            })),
        );
        consume(r.receipt);
        let kind = if use_final {
            FirmwareCaptureBoundaryKind::Final
        } else {
            FirmwareCaptureBoundaryKind::Middle
        };
        let r = submit(
            &mut c,
            FirmwareBackupAction::StoreCaptureBoundary(Box::new(evidence(kind, 2))),
        );
        let action = if use_final {
            review(r.receipt);
            FirmwareBackupAction::StoreCaptureBoundary(Box::new(evidence(
                FirmwareCaptureBoundaryKind::Middle,
                3,
            )))
        } else {
            consume(r.receipt);
            FirmwareBackupAction::CloseCaptured {
                boundary: boundary(2),
                outcome: BackupAcquisition::Finished,
            }
        };
        c.try_submit(action).ok().unwrap();
        let error = wait(&mut c)
            .result
            .err()
            .expect("Invalid capture progression accepted");
        assert!(error.message.contains(if use_final {
            "No completed capture progression after Final"
        } else {
            "exact durable Final identity"
        }));
        let closed = reopen_firmware_capture_record(
            &t.0.join("firmware-capture-evidence")
                .join(id)
                .join("record-0003.json"),
        )
        .unwrap();
        assert_eq!(closed.kind, FirmwareCaptureReceiptKind::Closed);
        assert!(closed.cancelled);
        assert!(closed.evidence.is_none());
    }
}

#[test]
fn actual_reservation_rejects_complete_old_epoch_and_wrong_offered_read() {
    for old_epoch in [false, true] {
        let t = Temp::new();
        let mut w = LibraryWorker::open(&t.0, Arc::new(|| {})).unwrap();
        let mut c = w.take_firmware_backup_client().unwrap();
        let r = submit(&mut c, FirmwareBackupAction::PrepareCapture(reservation()));
        consume(r.receipt);
        let mut e = evidence(FirmwareCaptureBoundaryKind::Opening, 1);
        if old_epoch {
            e.callbacks[0].epoch = SessionEpoch(6);
        } else {
            e.offered_read_bytes = FirmwareCaptureRead::Object {
                object: FirmwareBackupObject::Current,
                pass: FirmwareObservationPass::Baseline,
            }
            .request_bytes()
            .unwrap();
            e.offer.as_mut().unwrap().requested_bytes = e.offered_read_bytes.len() as u64;
            e.offer.as_mut().unwrap().accepted_bytes = e.offered_read_bytes.len() as u64;
        }
        c.try_submit(FirmwareBackupAction::BeginCaptured(Box::new(
            FirmwareCapturedBegin {
                begin: begin(),
                evidence: e,
            },
        )))
        .ok()
        .unwrap();
        let error = wait(&mut c)
            .result
            .err()
            .expect("Invalid original evidence received a receipt");
        assert!(error.message.contains(if old_epoch {
            "callback session differs"
        } else {
            "offered read/object/slot differs"
        }));
        assert!(!c.capture_retirement_blockers().is_empty());
    }
}

#[test]
fn original_stale_callback_and_partial_first_offer_survive_failure_publication() {
    let t = Temp::new();
    let mut w = LibraryWorker::open(&t.0, Arc::new(|| {})).unwrap();
    let mut c = w.take_firmware_backup_client().unwrap();
    let r = submit(&mut c, FirmwareBackupAction::PrepareCapture(reservation()));
    let id = r.receipt.description().capture_id.clone();
    consume(r.receipt);
    let r = submit(
        &mut c,
        FirmwareBackupAction::BeginCaptured(Box::new(FirmwareCapturedBegin {
            begin: begin(),
            evidence: evidence(FirmwareCaptureBoundaryKind::Opening, 1),
        })),
    );
    consume(r.receipt);
    let r = submit(
        &mut c,
        FirmwareBackupAction::StoreCaptureBoundary(Box::new(evidence(
            FirmwareCaptureBoundaryKind::Middle,
            2,
        ))),
    );
    consume(r.receipt);
    let mut e = evidence(FirmwareCaptureBoundaryKind::Middle, 3);
    e.complete = false;
    e.callbacks[0].epoch = SessionEpoch(6);
    e.offer.as_mut().unwrap().accepted_bytes = 3;
    e.completed_ns = None;
    e.diagnostics = vec![
        "Old epoch callback retained; first nonblocking write accepted only three bytes; no retry"
            .into(),
    ];
    let r = submit(
        &mut c,
        FirmwareBackupAction::StoreCaptureFailure(Box::new(e.clone())),
    );
    review(r.receipt);
    let r = submit(
        &mut c,
        FirmwareBackupAction::CloseCaptured {
            boundary: boundary(2),
            outcome: BackupAcquisition::Cancelled,
        },
    );
    review(r.receipt);
    let mut r = submit(&mut c, FirmwareBackupAction::PublishCaptured);
    review(r.receipt);
    let publication = r.publication.take().unwrap();
    let record = reopen_firmware_capture_record(
        &t.0.join("firmware-capture-evidence")
            .join(id)
            .join("record-0003.json"),
    )
    .unwrap();
    assert_eq!(record.evidence, Some(e));
    assert_eq!(record.kind, FirmwareCaptureReceiptKind::Failure);
    publication.revalidate().unwrap();
    assert!(!publication.summary().permits_flash());
}
