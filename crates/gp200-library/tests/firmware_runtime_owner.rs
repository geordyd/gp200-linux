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
            "fw05a0-owner-{}-{}",
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
        assert!(
            Instant::now() < until,
            "runtime reply missing {:?} {:?}",
            c.retirement_state(),
            c.terminal_failure()
        );
        std::thread::sleep(Duration::from_millis(2));
    }
}
fn take(c: &mut LibraryFirmwareRuntimeClient) -> LibraryStore {
    let address = c
        .try_request(LibraryFirmwareAction::RelinquishStore)
        .unwrap();
    let event = wait(c);
    assert_eq!(event.address, address);
    let mut handle = match event.result.unwrap() {
        LibraryFirmwareResult::Retirement(h) => h,
        v => panic!("Unexpected {v:?}"),
    };
    let until = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(result) = handle.try_take_result() {
            assert!(handle.try_take_result().is_none());
            return result.unwrap();
        }
        assert!(Instant::now() < until);
        std::thread::sleep(Duration::from_millis(2));
    }
}
#[test]
fn actual_join_moves_store_once_and_fresh_worker_revokes_old_clones() {
    let t = Temp::new();
    let mut store = LibraryStore::open(&t.0).unwrap();
    store.recovery_note = Some("Genuine owner marker never published to disk".into());
    let mut worker = LibraryWorker::from_store(store, Arc::new(|| {})).unwrap();
    let old = worker.workflow_client();
    let mut backup = worker.take_firmware_backup_client().unwrap();
    let mut client = worker.take_firmware_runtime_client().unwrap();
    assert!(worker.take_firmware_runtime_client().is_none());
    assert!(LibraryStore::open(&t.0).is_err());
    let store = take(&mut client);
    assert_eq!(
        store.recovery_note.as_deref(),
        Some("Genuine owner marker never published to disk")
    );
    assert!(LibraryStore::open(&t.0).is_err());
    assert!(matches!(
        old.try_submit(WorkflowCommand {
            intent: IntentId(1),
            action: WorkflowAction::ListBackups
        }),
        Err(SubmitError::Disconnected(_))
    ));
    assert!(old.try_next_event().is_none());
    assert!(matches!(
        backup.try_submit(FirmwareBackupAction::Publish),
        Err(SubmitError::Disconnected(_))
    ));
    let mut fresh = LibraryWorker::from_store(store, Arc::new(|| {})).unwrap();
    let mut fresh_client = fresh.take_firmware_runtime_client().unwrap();
    assert_ne!(fresh_client.run_id(), client.run_id());
    fresh
        .try_submit(LibraryCommand::Query {
            intent: IntentId(2),
            query: LibraryQuery::default(),
        })
        .unwrap();
    let until = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(LibraryEvent::QueryResult { intent, .. }) = fresh.try_next_event() {
            assert_eq!(intent, IntentId(2));
            break;
        }
        assert!(Instant::now() < until);
        std::thread::sleep(Duration::from_millis(2));
    }
    let returned = take(&mut fresh_client);
    assert_eq!(
        returned.recovery_note.as_deref(),
        Some("Genuine owner marker never published to disk")
    );
    drop(returned);
    assert!(LibraryStore::open(&t.0).is_ok());
}
#[test]
fn unread_actual_library_and_workflow_results_prevent_retirement() {
    let t = Temp::new();
    let mut worker = LibraryWorker::open(&t.0, Arc::new(|| {})).unwrap();
    let mut c = worker.take_firmware_runtime_client().unwrap();
    worker
        .try_submit(LibraryCommand::Query {
            intent: IntentId(10),
            query: LibraryQuery::default(),
        })
        .unwrap();
    std::thread::sleep(Duration::from_millis(100));
    assert!(matches!(
        c.try_request(LibraryFirmwareAction::RelinquishStore),
        Err(SubmitError::Busy(_))
    ));
    assert!(worker.try_next_event().is_some());
    let workflow = worker.workflow_client();
    workflow
        .try_submit(WorkflowCommand {
            intent: IntentId(11),
            action: WorkflowAction::ListBackups,
        })
        .unwrap();
    std::thread::sleep(Duration::from_millis(100));
    assert!(matches!(
        c.try_request(LibraryFirmwareAction::RelinquishStore),
        Err(SubmitError::Busy(_))
    ));
    assert!(workflow.try_next_event().is_some());
    let store = take(&mut c);
    drop(store);
}
#[test]
fn runtime_history_acknowledgement_and_pending_request_are_correlated() {
    let t = Temp::new();
    let mut worker = LibraryWorker::open(&t.0, Arc::new(|| {})).unwrap();
    let mut c = worker.take_firmware_runtime_client().unwrap();
    let address = c
        .try_request(LibraryFirmwareAction::ListHistory {
            cursor: None,
            limit: 32,
        })
        .unwrap();
    assert!(matches!(
        c.try_request(LibraryFirmwareAction::RelinquishStore),
        Err(SubmitError::Busy(_))
    ));
    let e = wait(&mut c);
    assert_eq!(e.address, address);
    match e.result.unwrap() {
        LibraryFirmwareResult::HistoryPage(p) => assert_eq!(p.total, 0),
        v => panic!("{v:?}"),
    };
    drop(take(&mut c));
}
#[test]
fn queued_history_success_is_revoked_after_cancel() {
    let t = Temp::new();
    let mut worker = LibraryWorker::open(&t.0, Arc::new(|| {})).unwrap();
    let mut c = worker.take_firmware_runtime_client().unwrap();
    c.try_request(LibraryFirmwareAction::ListHistory {
        cursor: None,
        limit: 32,
    })
    .unwrap();
    std::thread::sleep(Duration::from_millis(100));
    c.cancel();
    assert!(wait(&mut c).result.is_err());
    assert!(matches!(
        c.try_request(LibraryFirmwareAction::RelinquishStore),
        Err(SubmitError::Disconnected(_))
    ));
    worker
        .try_submit(LibraryCommand::Query {
            intent: IntentId(3),
            query: LibraryQuery::default(),
        })
        .unwrap();
}
fn backup_wait(c: &mut FirmwareBackupClient) -> FirmwareBackupEvent {
    let end = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(e) = c.try_next_event() {
            return e;
        }
        assert!(Instant::now() < end, "{:?}", c.terminal_failure());
        std::thread::sleep(Duration::from_millis(2));
    }
}
fn reservation() -> FirmwareCaptureReservation {
    FirmwareCaptureReservation {
        actor_run: "descriptive public runtime test".into(),
        reservation: "test reservation".into(),
        normal_owner: "test owner".into(),
        epoch: SessionEpoch(1),
        usb: FirmwareUsbObservation {
            topology: "test".into(),
            vendor: 1,
            product: 2,
            serial: "test".into(),
            interface: "test".into(),
            endpoint: "test".into(),
        },
        clock_origin: "test".into(),
        prepared_monotonic_ns: 1,
        prepared_wall_millis: 1,
        initial_fence: FirmwareCaptureFence {
            allocated: 0,
            processed: 0,
        },
    }
}
#[test]
fn genuine_early_aborted_capture_blocks_transfer_but_consumed_blocked_reply_restores_same_owner_work()
 {
    let t = Temp::new();
    let mut worker = LibraryWorker::open(&t.0, Arc::new(|| {})).unwrap();
    let mut backup = worker.take_firmware_backup_client().unwrap();
    let mut c = worker.take_firmware_runtime_client().unwrap();
    backup
        .try_submit(FirmwareBackupAction::PrepareCapture(reservation()))
        .ok()
        .unwrap();
    let receipt = match backup_wait(&mut backup).result.unwrap() {
        FirmwareBackupResult::CaptureDurable(v) => v.receipt,
        _ => panic!(),
    };
    let description = receipt.description().clone();
    receipt.try_consume_for_capture_abort(&description).unwrap();
    c.try_request(LibraryFirmwareAction::RelinquishStore)
        .unwrap();
    let end = Instant::now() + Duration::from_secs(5);
    while c.retirement_state() != LibraryStoreRetirementState::BlockedPendingAcknowledgement {
        assert!(Instant::now() < end);
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(matches!(
        worker.try_submit(LibraryCommand::Query {
            intent: IntentId(4),
            query: LibraryQuery::default()
        }),
        Err(SubmitError::Busy(_))
    ));
    let event = wait(&mut c);
    match event.result.unwrap() {
        LibraryFirmwareResult::Blocked { reasons } => {
            assert!(reasons.iter().any(|r| r.contains("aborted")))
        }
        v => panic!("{v:?}"),
    };
    assert_eq!(c.retirement_state(), LibraryStoreRetirementState::Running);
    assert!(LibraryStore::open(&t.0).is_err());
    worker
        .try_submit(LibraryCommand::Query {
            intent: IntentId(5),
            query: LibraryQuery::default(),
        })
        .unwrap();
    loop {
        if worker.try_next_event().is_some() {
            break;
        }
        assert!(Instant::now() < end);
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(matches!(
        backup.try_submit(FirmwareBackupAction::PrepareCapture(reservation())),
        Err(SubmitError::Disconnected(_))
    ));
}
#[test]
fn actual_producer_racing_retirement_is_either_admitted_before_barrier_or_rejected_with_original_command()
 {
    for _ in 0..8 {
        let t = Temp::new();
        let mut worker = LibraryWorker::open(&t.0, Arc::new(|| {})).unwrap();
        let mut c = worker.take_firmware_runtime_client().unwrap();
        let producer = worker.workflow_client();
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let b = barrier.clone();
        let thread = std::thread::spawn(move || {
            b.wait();
            producer.try_submit(WorkflowCommand {
                intent: IntentId(99),
                action: WorkflowAction::ListBackups,
            })
        });
        barrier.wait();
        let retirement = c.try_request(LibraryFirmwareAction::RelinquishStore);
        let offered = thread.join().unwrap();
        match retirement {
            Ok(address) => {
                assert!(offered.is_err());
                let e = wait(&mut c);
                assert_eq!(e.address, address);
                let mut h = match e.result.unwrap() {
                    LibraryFirmwareResult::Retirement(h) => h,
                    v => panic!("{v:?}"),
                };
                let end = Instant::now() + Duration::from_secs(5);
                loop {
                    if let Some(s) = h.try_take_result() {
                        drop(s.unwrap());
                        break;
                    }
                    assert!(Instant::now() < end);
                    std::thread::sleep(Duration::from_millis(2));
                }
            }
            Err(SubmitError::Busy(_)) => {
                assert!(offered.is_ok());
                let end = Instant::now() + Duration::from_secs(5);
                loop {
                    if worker.try_next_workflow_event().is_some() {
                        break;
                    }
                    assert!(Instant::now() < end);
                    std::thread::sleep(Duration::from_millis(2));
                }
                drop(take(&mut c));
            }
            Err(e) => panic!("{e:?}"),
        }
    }
}
fn legacy_origin() -> BackupOrigin {
    BackupOrigin {
        identity: DeviceIdentity {
            model: DeviceModel::Gp200,
            mode: BackendMode::Hardware,
            firmware: None,
            display_name: "GP-200".into(),
        },
        epoch: SessionEpoch(1),
        device_key: "legacy-test".into(),
        evidence: vec![],
    }
}
fn legacy_boundary(sequence: u64) -> FirmwareArchiveBoundary {
    FirmwareArchiveBoundary {
        epoch: SessionEpoch(1),
        identity: legacy_origin().identity,
        device_key: legacy_origin().device_key,
        sequence,
        monotonic_millis: sequence,
        wall_clock_millis: sequence,
        raw_identity: vec![1],
    }
}
#[test]
fn legacy_closed_archive_requires_actual_publication_and_consumed_revalidation_before_store_return()
{
    let t = Temp::new();
    let mut worker = LibraryWorker::open(&t.0, Arc::new(|| {})).unwrap();
    let mut backup = worker.take_firmware_backup_client().unwrap();
    let mut c = worker.take_firmware_runtime_client().unwrap();
    let context = FirmwareBackupReviewContext {
        device: FirmwareDeviceAssociation::named_local_record("GP-200", "legacy untrusted test")
            .unwrap(),
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
    };
    backup
        .try_submit(FirmwareBackupAction::Begin(Box::new(FirmwareBackupBegin {
            origin: legacy_origin(),
            context,
            opening: legacy_boundary(1),
        })))
        .ok()
        .unwrap();
    assert!(backup_wait(&mut backup).result.is_ok());
    backup
        .try_submit(FirmwareBackupAction::Close {
            boundary: legacy_boundary(2),
            outcome: BackupAcquisition::Cancelled,
        })
        .ok()
        .unwrap();
    assert!(backup_wait(&mut backup).result.is_ok());
    c.try_request(LibraryFirmwareAction::RelinquishStore)
        .unwrap();
    match wait(&mut c).result.unwrap() {
        LibraryFirmwareResult::Blocked { reasons } => {
            assert!(reasons.iter().any(|r| r.contains("not published")))
        }
        v => panic!("{v:?}"),
    };
    assert_eq!(c.retirement_state(), LibraryStoreRetirementState::Running);
    backup
        .try_submit(FirmwareBackupAction::Publish)
        .ok()
        .unwrap();
    let package = match backup_wait(&mut backup).result.unwrap() {
        FirmwareBackupResult::PackagePublished(p) => p,
        _ => panic!(),
    };
    backup
        .try_submit(FirmwareBackupAction::Revalidate(package))
        .ok()
        .unwrap();
    std::thread::sleep(Duration::from_millis(100));
    c.try_request(LibraryFirmwareAction::RelinquishStore)
        .unwrap();
    assert!(matches!(
        wait(&mut c).result,
        Ok(LibraryFirmwareResult::Blocked { .. })
    ));
    assert!(matches!(
        backup_wait(&mut backup).result,
        Ok(FirmwareBackupResult::Revalidated(_))
    ));
    drop(take(&mut c));
}
