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
            "fw04-worker-{}-{}",
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
fn origin() -> BackupOrigin {
    BackupOrigin {
        identity: DeviceIdentity {
            model: DeviceModel::Gp200,
            mode: BackendMode::Hardware,
            firmware: None,
            display_name: "GP-200".into(),
        },
        epoch: SessionEpoch(1),
        device_key: "serial".into(),
        evidence: vec![],
    }
}
fn boundary(seq: u64) -> FirmwareArchiveBoundary {
    FirmwareArchiveBoundary {
        epoch: SessionEpoch(1),
        identity: origin().identity,
        device_key: origin().device_key,
        sequence: seq,
        monotonic_millis: seq,
        wall_clock_millis: seq,
        raw_identity: vec![1],
    }
}
fn begin() -> FirmwareBackupAction {
    let device = FirmwareDeviceAssociation::named_local_record("GP-200", "synthetic").unwrap();
    FirmwareBackupAction::Begin(Box::new(FirmwareBackupBegin {
        origin: origin(),
        context: FirmwareBackupReviewContext {
            device,
            manual_version: None,
            destination: FirmwareBackupDestination {
                model: "GP-200".into(),
                version: FirmwareVersion::new(1, 8, 2).unwrap(),
                image_sha256: "a".repeat(64),
                restoration_profile: None,
            },
            local_material: vec![],
            preservation_uncertainty: "unknown".into(),
            recovery_limitations: "unknown".into(),
        },
        opening: boundary(1),
    }))
}
fn wait(client: &mut FirmwareBackupClient) -> FirmwareBackupEvent {
    let until = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(e) = client.try_next_event() {
            return e;
        }
        assert!(
            Instant::now() < until,
            "No worker reply: {:?}",
            client.terminal_failure()
        );
        std::thread::sleep(Duration::from_millis(2));
    }
}
fn submit(client: &mut FirmwareBackupClient, action: FirmwareBackupAction) -> FirmwareBackupEvent {
    let address = client.try_submit(action).ok().unwrap();
    let e = wait(client);
    assert_eq!(e.address, address);
    e
}
#[test]
fn one_extraction_one_writer_and_exact_operation_acknowledgements() {
    let t = Temp::new();
    let mut worker = LibraryWorker::open(&t.0, Arc::new(|| {})).unwrap();
    let mut c = worker.take_firmware_backup_client().unwrap();
    assert!(worker.take_firmware_backup_client().is_none());
    assert!(LibraryStore::open(&t.0).is_err());
    let first = c.try_submit(begin()).ok().unwrap();
    assert!(matches!(
        c.try_submit(FirmwareBackupAction::Publish),
        Err(SubmitError::Busy(_))
    ));
    let event = wait(&mut c);
    assert_eq!(event.address, first);
    assert!(matches!(
        event.result,
        Ok(FirmwareBackupResult::Begun { .. })
    ));
    let event = submit(
        &mut c,
        FirmwareBackupAction::Close {
            boundary: boundary(2),
            outcome: BackupAcquisition::Cancelled,
        },
    );
    assert!(matches!(event.result, Ok(FirmwareBackupResult::Closed)));
    let event = submit(&mut c, FirmwareBackupAction::Publish);
    let receipt = match event.result {
        Ok(FirmwareBackupResult::PackagePublished(r)) => r,
        _ => panic!("No package"),
    };
    assert!(!receipt.summary().permits_flash());
    let event = submit(&mut c, FirmwareBackupAction::Revalidate(receipt));
    assert!(matches!(
        event.result,
        Ok(FirmwareBackupResult::Revalidated(_))
    ));
    assert!(c.is_idle());
    assert!(c.storage_settled());
    assert!(c.try_next_event().is_none());
}
#[test]
fn cancellation_revokes_already_queued_success() {
    let t = Temp::new();
    let mut worker = LibraryWorker::open(&t.0, Arc::new(|| {})).unwrap();
    let mut c = worker.take_firmware_backup_client().unwrap();
    c.try_submit(begin()).ok().unwrap();
    let until = Instant::now() + Duration::from_secs(10);
    while !c.storage_settled() {
        assert!(Instant::now() < until);
        std::thread::sleep(Duration::from_millis(2));
    }
    c.cancel();
    let e = wait(&mut c);
    assert_eq!(e.result.err().unwrap().kind, ErrorKind::Indeterminate);
    assert!(matches!(
        c.try_submit(FirmwareBackupAction::Publish),
        Err(SubmitError::Disconnected(_))
    ));
    assert!(c.terminal_failure().is_some());
}
#[test]
fn producer_failure_is_sticky_and_no_followup_is_accepted() {
    let t = Temp::new();
    let mut worker = LibraryWorker::open(&t.0, Arc::new(|| {})).unwrap();
    let mut c = worker.take_firmware_backup_client().unwrap();
    let e = submit(&mut c, FirmwareBackupAction::Publish);
    assert!(e.result.is_err());
    assert!(c.terminal_failure().is_some());
    assert!(matches!(
        c.try_submit(begin()),
        Err(SubmitError::Disconnected(_))
    ));
}
#[test]
fn dropping_consumer_preserves_cancelled_archival_descriptor() {
    let t = Temp::new();
    let mut worker = LibraryWorker::open(&t.0, Arc::new(|| {})).unwrap();
    let mut c = worker.take_firmware_backup_client().unwrap();
    let e = submit(&mut c, begin());
    let id = match e.result {
        Ok(FirmwareBackupResult::Begun { acquisition }) => acquisition,
        _ => panic!("No begin"),
    };
    drop(c);
    drop(worker);
    let until = Instant::now() + Duration::from_secs(10);
    let store = loop {
        match LibraryStore::open(&t.0) {
            Ok(s) => break s,
            Err(_) => {
                assert!(Instant::now() < until);
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    };
    assert_eq!(
        store.device_backup(&id).unwrap().acquisition,
        BackupAcquisition::Cancelled
    );
}
#[test]
fn genuine_local_receipt_from_foreign_acquisition_is_rejected() {
    let foreign = Temp::new();
    let mut store = LibraryStore::open(&foreign.0).unwrap();
    let begin = match begin() {
        FirmwareBackupAction::Begin(b) => b,
        _ => unreachable!(),
    };
    let mut draft = store
        .begin_firmware_backup_review(begin.origin, begin.context, begin.opening)
        .unwrap();
    store
        .close_firmware_backup_review(&mut draft, boundary(2), BackupAcquisition::Cancelled)
        .unwrap();
    let receipt = store.publish_firmware_backup_review(draft).unwrap();
    let t = Temp::new();
    let mut worker = LibraryWorker::open(&t.0, Arc::new(|| {})).unwrap();
    let mut client = worker.take_firmware_backup_client().unwrap();
    assert!(submit(&mut client, crate::begin()).result.is_ok());
    let e = submit(
        &mut client,
        FirmwareBackupAction::Revalidate(Box::new(receipt)),
    );
    assert!(
        e.result
            .err()
            .unwrap()
            .message
            .contains("another acquisition")
    );
    assert!(client.terminal_failure().is_some());
}

fn capture_reservation() -> FirmwareCaptureReservation {
    FirmwareCaptureReservation {
        actor_run: "actual worker test actor".into(),
        reservation: "actual worker reservation".into(),
        normal_owner: "guard".into(),
        epoch: SessionEpoch(1),
        usb: FirmwareUsbObservation {
            topology: "1-2".into(),
            vendor: 0x84ef,
            product: 0x2a,
            serial: "fixed serial".into(),
            interface: "midi".into(),
            endpoint: "normal raw".into(),
        },
        clock_origin: "test monotonic".into(),
        prepared_monotonic_ns: 1,
        prepared_wall_millis: 2,
        initial_fence: FirmwareCaptureFence {
            allocated: 0,
            processed: 0,
        },
    }
}
#[test]
fn capture_preparation_queued_success_is_revoked_without_receipt_drop_deadlock() {
    let t = Temp::new();
    let mut w = LibraryWorker::open(&t.0, Arc::new(|| {})).unwrap();
    let mut c = w.take_firmware_backup_client().unwrap();
    c.try_submit(FirmwareBackupAction::PrepareCapture(capture_reservation()))
        .ok()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !c.storage_settled() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(1));
    }
    c.cancel();
    assert!(wait(&mut c).result.is_err());
    assert!(!c.capture_retirement_blockers().is_empty());
}
#[test]
fn archival_begin_cannot_bypass_prepared_capture_or_unconsumed_receipt() {
    let t = Temp::new();
    let mut w = LibraryWorker::open(&t.0, Arc::new(|| {})).unwrap();
    let mut c = w.take_firmware_backup_client().unwrap();
    c.try_submit(FirmwareBackupAction::PrepareCapture(capture_reservation()))
        .ok()
        .unwrap();
    let receipt = match wait(&mut c).result {
        Ok(FirmwareBackupResult::CaptureDurable(r)) => r.receipt,
        _ => panic!("No preparation receipt"),
    };
    assert!(matches!(c.try_submit(begin()), Err(SubmitError::Busy(_))));
    let expected = receipt.description().clone();
    receipt.try_with_first_offer(&expected, || ()).unwrap();
    assert!(submit(&mut c, begin()).result.is_err());
    assert!(c.terminal_failure().is_some());
    assert!(!c.capture_retirement_blockers().is_empty());
}
#[cfg(unix)]
#[test]
fn capture_authority_directory_symlink_is_rejected_by_actual_worker() {
    let t = Temp::new();
    let other = Temp::new();
    let mut w = LibraryWorker::open(&t.0, Arc::new(|| {})).unwrap();
    let mut c = w.take_firmware_backup_client().unwrap();
    std::os::unix::fs::symlink(&other.0, t.0.join("firmware-capture-evidence")).unwrap();
    assert!(
        submit(
            &mut c,
            FirmwareBackupAction::PrepareCapture(capture_reservation())
        )
        .result
        .is_err()
    );
    assert!(fs::read_dir(&other.0).unwrap().next().is_none());
    assert!(
        c.capture_retirement_blockers()
            .iter()
            .any(|s| s.contains("publication failure"))
    );
}
