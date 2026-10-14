use gp200_domain::*;
use gp200_library::*;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};
struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        let p = std::env::temp_dir().join(format!(
            "fw05a0-history-{}-{}",
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
fn wait(c: &mut LibraryFirmwareRuntimeClient) -> LibraryFirmwareEvent {
    let until = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(v) = c.try_next_event() {
            return v;
        }
        assert!(Instant::now() < until, "{:?}", c.terminal_failure());
        std::thread::sleep(Duration::from_millis(2));
    }
}
fn request(
    c: &mut LibraryFirmwareRuntimeClient,
    a: LibraryFirmwareAction,
) -> Result<LibraryFirmwareResult, ServiceError> {
    let address = c.try_request(a).unwrap();
    let e = wait(c);
    assert_eq!(address, e.address);
    e.result
}
fn detail(c: &mut LibraryFirmwareRuntimeClient, id: &FirmwareAttemptId) -> FirmwareHistoryDetail {
    match request(
        c,
        LibraryFirmwareAction::InspectAttempt {
            attempt: id.clone(),
        },
    )
    .unwrap()
    {
        LibraryFirmwareResult::HistoryDetail(d) => *d,
        v => panic!("{v:?}"),
    }
}
fn finish(c: &mut LibraryFirmwareRuntimeClient) {
    let until = Instant::now() + Duration::from_secs(10);
    let address = loop {
        match c.try_request(LibraryFirmwareAction::RelinquishStore) {
            Ok(v) => break v,
            Err(SubmitError::Busy(_)) => {
                assert!(Instant::now() < until);
                std::thread::sleep(Duration::from_millis(2));
            }
            Err(e) => panic!("{e:?}"),
        }
    };
    let e = wait(c);
    assert_eq!(e.address, address);
    let mut h = match e.result.unwrap() {
        LibraryFirmwareResult::Retirement(h) => h,
        v => panic!("{v:?}"),
    };
    loop {
        if let Some(s) = h.try_take_result() {
            drop(s.unwrap());
            break;
        }
        assert!(Instant::now() < until);
        std::thread::sleep(Duration::from_millis(2));
    }
}
fn job() -> Arc<ReviewedFirmwareUpdate> {
    let b: &[u8] = include_bytes!("../../../tests/fixtures/device-assets/synthetic-firmware.bin");
    let context = FirmwareUpdateContextDto {
        profile_id: "imported history claim".into(),
        hardware_policy: "unaccepted".into(),
        update_mode: "observation".into(),
        normal: None,
        versions: vec![],
        backup_objects: vec![],
        loss_items: vec![],
        power_statement: "claim".into(),
        connection_statement: "claim".into(),
    };
    let materials = [
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
        bytes: Arc::from(&b"independent untrusted history claim"[..]),
    })
    .collect();
    Arc::new(
        ReviewedFirmwareUpdate::prepare(
            RetainedJobBlob::checked(AssetId("source".into()), b.into(), JobSha256::of(b)).unwrap(),
            context,
            materials,
            128,
        )
        .unwrap(),
    )
}
fn frame(
    id: &FirmwareAttemptId,
    sequence: u64,
    previous: Option<&[u8]>,
    entry: serde_json::Value,
) -> Vec<u8> {
    let body=serde_json::to_vec(&json!({"schema":1,"attempt":id.as_str(),"sequence":sequence,"previous":previous.map(|p|format!("{:x}",Sha256::digest(p))),"entry":entry})).unwrap();
    let mut raw = b"GP2FWL01".to_vec();
    raw.extend_from_slice(&(body.len() as u32).to_le_bytes());
    raw.extend_from_slice(&Sha256::digest(&body));
    raw.extend_from_slice(&body);
    raw
}
fn fixture(store: &LibraryStore, lifecycle: bool, tail: bool) -> (FirmwareAttemptId, Vec<u8>) {
    let id = FirmwareAttemptId::parse(if lifecycle {
        "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
    } else {
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
    })
    .unwrap();
    let ledger = store.root().join("firmware-attempt-ledger");
    fs::create_dir_all(&ledger).unwrap();
    let job = job();
    for (_, raw) in job.materials() {
        fs::write(
            store
                .root()
                .join("blobs")
                .join(format!("{:x}", Sha256::digest(raw))),
            raw,
        )
        .unwrap();
    }
    fs::write(
        store.root().join("blobs").join(job.digest().hex()),
        job.preimage(),
    )
    .unwrap();
    fs::write(
        ledger.join(format!("{}.reserved", id.as_str())),
        format!("gp200-firmware-local-attempt-v1:{}\n", id.as_str()),
    )
    .unwrap();
    let first = frame(
        &id,
        0,
        None,
        json!({"Prepared":{"review":job.digest().hex(),"origin":"MockSimulation"}}),
    );
    let mut raw = first.clone();
    if lifecycle {
        let plan = FirmwareLifecyclePlanDto {
            review_sha256: job.digest().hex(),
            profile_sha256: JobSha256::of(b"rule").hex(),
            origin: FirmwareObservationOrigin::MockSimulation,
            nonblock_actions: 1,
            epoch: SessionEpoch(1),
            binding: FirmwareUsbObservation {
                topology: "history".into(),
                vendor: 1,
                product: 2,
                serial: "history".into(),
                interface: "history".into(),
                endpoint: "history".into(),
            },
            generation: 1,
            clock_origin: "history".into(),
        };
        let second = frame(&id, 1, Some(&first), json!({"LifecyclePrepared":plan}));
        raw.extend_from_slice(&second);
        raw.extend_from_slice(&frame(
            &id,
            2,
            Some(&second),
            json!({"LifecycleClosed":"ClosedBeforeAnyOffer"}),
        ));
    } else {
        raw.extend_from_slice(&frame(
            &id,
            1,
            Some(&first),
            json!({"Closed":"ObservationFinished"}),
        ));
    }
    if tail {
        raw.extend_from_slice(&[0x7f, 0, 0xff, 0x80, 0x7d]);
    }
    fs::write(ledger.join(format!("{}.journal", id.as_str())), &raw).unwrap();
    (id, raw)
}
fn open(
    t: &Temp,
    lifecycle: bool,
    tail: bool,
) -> (
    LibraryWorker,
    LibraryFirmwareRuntimeClient,
    FirmwareAttemptId,
    Vec<u8>,
) {
    let store = LibraryStore::open(t.0.join("library")).unwrap();
    let (id, raw) = fixture(&store, lifecycle, tail);
    let mut worker = LibraryWorker::from_store(store, Arc::new(|| {})).unwrap();
    let c = worker.take_firmware_runtime_client().unwrap();
    (worker, c, id, raw)
}
fn export(
    c: &mut LibraryFirmwareRuntimeClient,
    d: &FirmwareHistoryDetail,
    path: &Path,
) -> Result<FirmwareHistoryExportReceipt, ServiceError> {
    match request(
        c,
        LibraryFirmwareAction::ExportAttempt {
            attempt: d.attempt.clone(),
            expected_prefix: d.prefix.clone(),
            destination: path.into(),
        },
    )? {
        LibraryFirmwareResult::Exported(r) => Ok(r),
        v => panic!("{v:?}"),
    }
}
#[test]
fn independent_legacy_and_lifecycle_frames_export_whole_rejected_tail_and_no_authority() {
    for lifecycle in [false, true] {
        let t = Temp::new();
        let (_worker, mut c, id, raw) = open(&t, lifecycle, true);
        let d = detail(&mut c, &id);
        assert!(!d.can_resume() && !d.permits_flash());
        let checked = d.checked.as_ref().unwrap();
        assert_eq!(checked.accepted_records, if lifecycle { 3 } else { 2 });
        assert_eq!(checked.raw_tail.as_ref().unwrap().bytes, 5);
        let destination = t.0.join("originals.zip");
        let r = export(&mut c, &d, &destination).unwrap();
        assert!(!r.can_resume() && !r.permits_flash());
        assert_eq!(
            format!("{:x}", Sha256::digest(fs::read(&destination).unwrap())),
            r.archive_sha256.as_str()
        );
        let mut zip = zip::ZipArchive::new(fs::File::open(&destination).unwrap()).unwrap();
        let mut recovered = vec![];
        zip.by_name(&format!(
            "original/firmware-attempt-ledger/{}.journal",
            id.as_str()
        ))
        .unwrap()
        .read_to_end(&mut recovered)
        .unwrap();
        assert_eq!(recovered, raw);
        let mut manifest = String::new();
        zip.by_name("observation.json")
            .unwrap()
            .read_to_string(&mut manifest)
            .unwrap();
        let m: serde_json::Value = serde_json::from_str(&manifest).unwrap();
        assert_eq!(m["can_resume"], false);
        assert_eq!(m["permits_flash"], false);
        assert_eq!(m["completion_verified"], false);
        assert_eq!(m["device_verified_bytes"], 0);
        assert_eq!(m["accepted_records"], if lifecycle { 3 } else { 2 });
        if lifecycle {
            assert_eq!(m["lifecycle_host_closure"], "ClosedBeforeAnyOffer");
        } else {
            assert_eq!(m["recorded_legacy_closure"], "ObservationFinished");
            assert_eq!(m["derived_reopen_state"], "Indeterminate");
        }
        finish(&mut c);
    }
}
#[test]
fn original_bad_tail_and_sidecar_changes_invalidate_snapshot_even_when_prefix_is_identical() {
    let t = Temp::new();
    let (_worker, mut c, id, _) = open(&t, false, true);
    let d = detail(&mut c, &id);
    let journal =
        t.0.join("library/firmware-attempt-ledger")
            .join(format!("{}.journal", id.as_str()));
    let mut raw = fs::read(&journal).unwrap();
    *raw.last_mut().unwrap() ^= 1;
    fs::write(&journal, &raw).unwrap();
    let newer = detail(&mut c, &id);
    assert_eq!(newer.prefix.prefix_sha256, d.prefix.prefix_sha256);
    assert_ne!(
        newer.prefix.source_snapshot_sha256,
        d.prefix.source_snapshot_sha256
    );
    assert_eq!(
        export(&mut c, &d, &t.0.join("stale.zip")).unwrap_err().kind,
        ErrorKind::Conflict
    );
    let sidecar = journal.with_extension("previous");
    fs::write(sidecar, b"original corrupt sidecar").unwrap();
    assert_eq!(
        export(&mut c, &newer, &t.0.join("sidecar-stale.zip"))
            .unwrap_err()
            .kind,
        ErrorKind::Conflict
    );
    finish(&mut c);
}
#[test]
fn orphan_corrupt_reservation_missing_material_and_raw_bytes_remain_observable_without_relaxing_ledger()
 {
    for mode in ["orphan", "corrupt-reservation", "missing-material"] {
        let t = Temp::new();
        let (mut worker, mut c, id, raw) = open(&t, false, true);
        let root = t.0.join("library");
        let reservation = root
            .join("firmware-attempt-ledger")
            .join(format!("{}.reserved", id.as_str()));
        match mode {
            "orphan" => fs::remove_file(&reservation).unwrap(),
            "corrupt-reservation" => {
                fs::write(&reservation, b"corrupt original reservation").unwrap()
            }
            _ => {
                let job = job();
                let pin = ReviewedFirmwareUpdate::retained_pins(job.preimage())
                    .unwrap()
                    .into_iter()
                    .next()
                    .unwrap();
                fs::remove_file(root.join("blobs").join(pin.sha256)).unwrap();
            }
        }
        let d = detail(&mut c, &id);
        if mode != "missing-material" {
            assert!(d.load_error.is_some());
            assert_eq!(d.kind, FirmwareHistoryExportKind::RawDiagnostics);
        }
        assert!(!d.permits_flash());
        let output = t.0.join("raw.zip");
        export(&mut c, &d, &output).unwrap();
        let mut zip = zip::ZipArchive::new(fs::File::open(output).unwrap()).unwrap();
        let mut bytes = vec![];
        zip.by_name(&format!(
            "original/firmware-attempt-ledger/{}.journal",
            id.as_str()
        ))
        .unwrap()
        .read_to_end(&mut bytes)
        .unwrap();
        assert_eq!(bytes, raw);
        finish(&mut c);
        assert!(worker.take_firmware_runtime_client().is_none());
    }
}
#[test]
fn pages_bind_actual_sorted_roster_and_reject_wrong_bounds_or_changed_roster() {
    let t = Temp::new();
    let store = LibraryStore::open(t.0.join("library")).unwrap();
    let dir = store.root().join("firmware-attempt-ledger");
    fs::create_dir(&dir).unwrap();
    for i in 1..=35 {
        let id = format!("{i:032x}");
        fs::write(
            dir.join(format!("{id}.reserved")),
            format!("gp200-firmware-local-attempt-v1:{id}\n"),
        )
        .unwrap();
    }
    let mut worker = LibraryWorker::from_store(store, Arc::new(|| {})).unwrap();
    let mut c = worker.take_firmware_runtime_client().unwrap();
    let page = match request(
        &mut c,
        LibraryFirmwareAction::ListHistory {
            cursor: None,
            limit: 32,
        },
    )
    .unwrap()
    {
        LibraryFirmwareResult::HistoryPage(p) => p,
        v => panic!("{v:?}"),
    };
    assert_eq!(page.total, 35);
    assert_eq!(page.attempts.len(), 32);
    let cursor = page.next.unwrap();
    let last = match request(
        &mut c,
        LibraryFirmwareAction::ListHistory {
            cursor: Some(cursor.clone()),
            limit: 32,
        },
    )
    .unwrap()
    {
        LibraryFirmwareResult::HistoryPage(p) => p,
        v => panic!("{v:?}"),
    };
    assert_eq!(last.attempts.len(), 3);
    assert!(last.next.is_none());
    let id = "ffffffffffffffffffffffffffffffff";
    fs::write(dir.join(format!("{id}.reserved")), b"new original").unwrap();
    assert_eq!(
        request(
            &mut c,
            LibraryFirmwareAction::ListHistory {
                cursor: Some(cursor),
                limit: 32
            }
        )
        .unwrap_err()
        .kind,
        ErrorKind::Conflict
    );
    assert!(
        request(
            &mut c,
            LibraryFirmwareAction::ListHistory {
                cursor: None,
                limit: 33
            }
        )
        .is_err()
    );
    finish(&mut c);
}
#[test]
fn no_clobber_authority_alias_traversal_and_symlink_paths_reject_without_original_changes() {
    let t = Temp::new();
    let (_worker, mut c, id, _) = open(&t, false, true);
    let d = detail(&mut c, &id);
    let journal =
        t.0.join("library/firmware-attempt-ledger")
            .join(format!("{}.journal", id.as_str()));
    let before = fs::read(&journal).unwrap();
    let existing = t.0.join("exists.zip");
    fs::write(&existing, b"retain original destination").unwrap();
    assert!(export(&mut c, &d, &existing).is_err());
    assert_eq!(fs::read(&existing).unwrap(), b"retain original destination");
    let hardlink = t.0.join("authority-alias.zip");
    fs::hard_link(&journal, &hardlink).unwrap();
    assert!(export(&mut c, &d, &hardlink).is_err());
    assert!(export(&mut c, &d, &t.0.join("library/new.zip")).is_err());
    assert!(export(&mut c, &d, &t.0.join("library/../traversal.zip")).is_err());
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(t.0.join("library"), t.0.join("alias")).unwrap();
        assert!(export(&mut c, &d, &t.0.join("alias/new.zip")).is_err());
        std::os::unix::fs::symlink(&journal, t.0.join("destination-link.zip")).unwrap();
        assert!(export(&mut c, &d, &t.0.join("destination-link.zip")).is_err());
    }
    assert_eq!(fs::read(&journal).unwrap(), before);
    finish(&mut c);
}
#[test]
fn clean_recorded_legacy_closure_reopens_as_descriptive_paused_history() {
    let t = Temp::new();
    let (_worker, mut c, id, _) = open(&t, false, false);
    let d = detail(&mut c, &id);
    let checked = d.checked.as_ref().unwrap();
    assert_eq!(checked.accepted_records, 2);
    assert!(checked.raw_tail.is_none());
    assert_eq!(
        checked.projection.as_ref().unwrap().recorded_closure(),
        Some(&FirmwareHostClosure::ObservationFinished)
    );
    assert!(!checked.can_resume() && !d.permits_flash());
    let receipt = export(&mut c, &d, &t.0.join("clean.zip")).unwrap();
    assert!(!receipt.can_resume());
    finish(&mut c);
}
