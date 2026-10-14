use super::*;
use crate::firmware_attempt_ledger::tests::{Temp, job, send};
use std::time::Instant;
fn event(w: &FirmwareLedgerWorker) -> FirmwareLedgerEvent {
    let end = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(e) = w.try_next() {
            return e;
        }
        assert!(Instant::now() < end, "worker event deadline");
        thread::sleep(Duration::from_millis(5));
    }
}
#[test]
fn moved_store_remains_exclusive_through_worker_lifetime() {
    let t = Temp::new();
    let store = LibraryStore::open(&t.0).unwrap();
    let w = FirmwareLedgerWorker::spawn(store).unwrap();
    assert!(matches!(LibraryStore::open(&t.0),Err(e) if e.kind==ErrorKind::Busy));
    let j = job();
    w.try_submit(FirmwareLedgerRequest {
        intent: 1,
        expected_sequence: 0,
        review_sha256: j.digest().hex(),
        action: FirmwareLedgerAction::Begin {
            job: j.clone(),
            origin: FirmwareObservationOrigin::MockSimulation,
        },
    })
    .unwrap();
    let e = event(&w);
    assert_eq!(e.run, w.run_id());
    assert!(matches!(e.result, Ok(FirmwareLedgerReply::Prepared { .. })));
    assert!(matches!(LibraryStore::open(&t.0),Err(e) if e.kind==ErrorKind::Busy));
    let store = w.join().unwrap();
    assert!(matches!(LibraryStore::open(&t.0),Err(e) if e.kind==ErrorKind::Busy));
    drop(store);
    LibraryStore::open(&t.0).unwrap();
}
#[test]
fn stale_review_or_sequence_stops_private_owner_and_cannot_be_replayed() {
    let t = Temp::new();
    let w = FirmwareLedgerWorker::spawn(LibraryStore::open(&t.0).unwrap()).unwrap();
    let j = job();
    w.try_submit(FirmwareLedgerRequest {
        intent: 1,
        expected_sequence: 0,
        review_sha256: j.digest().hex(),
        action: FirmwareLedgerAction::Begin {
            job: j.clone(),
            origin: FirmwareObservationOrigin::MockSimulation,
        },
    })
    .unwrap();
    event(&w);
    w.try_submit(FirmwareLedgerRequest {
        intent: 2,
        expected_sequence: 99,
        review_sha256: j.digest().hex(),
        action: FirmwareLedgerAction::Checkpoint { ordinal: 0 },
    })
    .unwrap();
    assert!(event(&w).result.is_err());
    assert!(w.is_stopped());
    assert!(
        w.try_submit(FirmwareLedgerRequest {
            intent: 3,
            expected_sequence: 0,
            review_sha256: j.digest().hex(),
            action: FirmwareLedgerAction::Checkpoint { ordinal: 0 }
        })
        .is_err()
    );
    assert!(w.join().unwrap().firmware_publication_uncertain);
}
#[test]
fn raw_queue_overflow_sets_sticky_stop_before_any_next_checkpoint() {
    let (commands, _command_rx) = bounded(CAPACITY);
    let (raw, _raw_rx) = bounded(RAW_CAPACITY);
    let (_tx, events) = bounded(CAPACITY);
    let w = FirmwareLedgerWorker {
        run: "test rail".into(),
        commands,
        raw,
        events,
        stopped: Arc::new(StopGate::default()),
        pending: Arc::new(AtomicU64::new(0)),
        offer: Arc::new(AtomicU64::new(0)),
        join: None,
    };
    for sequence in 0..RAW_CAPACITY {
        w.try_record_callback(FirmwareCallbackDto {
            sequence: sequence as u64,
            at_ms: sequence as u64,
            clock_origin: "fixture".into(),
            epoch: SessionEpoch(1),
            binding_sha256: crate::store::digest(b"claim"),
            raw: vec![0xf0, 0x7e, 0x48, 4, 0x7f, 0xf7],
        })
        .unwrap();
    }
    assert!(
        w.try_record_callback(FirmwareCallbackDto {
            sequence: 64,
            at_ms: 64,
            clock_origin: "fixture".into(),
            epoch: SessionEpoch(1),
            binding_sha256: crate::store::digest(b"claim"),
            raw: vec![0xf0, 0x7e, 0x48, 4, 0x7f, 0xf7]
        })
        .is_err()
    );
    assert!(w.is_stopped());
    assert_eq!(_raw_rx.len(), 64);
}
#[test]
fn command_overflow_and_disconnected_rails_stop_before_dispatch() {
    for mode in 0..3 {
        let (commands, command_rx) = bounded(CAPACITY);
        let (raw, raw_rx) = bounded(RAW_CAPACITY);
        let (_tx, events) = bounded(CAPACITY);
        let w = FirmwareLedgerWorker {
            run: "static command rail test".into(),
            commands,
            raw,
            events,
            stopped: Arc::new(StopGate::default()),
            pending: Arc::new(AtomicU64::new(0)),
            offer: Arc::new(AtomicU64::new(0)),
            join: None,
        };
        let inspect = |intent| FirmwareLedgerRequest {
            intent,
            expected_sequence: 0,
            review_sha256: String::new(),
            action: FirmwareLedgerAction::Inspect(
                FirmwareAttemptId::parse("00000000000000000000000000000001").unwrap(),
            ),
        };
        match mode {
            0 => {
                for i in 0..CAPACITY {
                    w.try_submit(inspect(i as u64 + 1)).unwrap();
                }
                assert_eq!(command_rx.len(), CAPACITY);
                assert!(w.try_submit(inspect(9)).is_err());
            }
            1 => {
                drop(command_rx);
                assert!(w.try_submit(inspect(1)).is_err());
            }
            _ => {
                drop(raw_rx);
                assert!(
                    w.try_record_callback(FirmwareCallbackDto {
                        sequence: 1,
                        at_ms: 1,
                        clock_origin: "fixture".into(),
                        epoch: SessionEpoch(1),
                        binding_sha256: crate::store::digest(b"claim"),
                        raw: vec![0xf0, 0x7e, 0x48, 4, 0x7f, 0xf7],
                    })
                    .is_err()
                );
            }
        }
        assert!(w.is_stopped());
        assert!(w.try_submit(inspect(10)).is_err());
    }
}
#[test]
fn full_critical_reply_rail_stops_real_worker_and_retains_fault() {
    let t = Temp::new();
    let w = FirmwareLedgerWorker::spawn(LibraryStore::open(&t.0).unwrap()).unwrap();
    let j = job();
    w.try_submit(FirmwareLedgerRequest {
        intent: 1,
        expected_sequence: 0,
        review_sha256: j.digest().hex(),
        action: FirmwareLedgerAction::Begin {
            job: j,
            origin: FirmwareObservationOrigin::LocalObservation,
        },
    })
    .unwrap();
    let FirmwareLedgerReply::Prepared { attempt, .. } = event(&w).result.unwrap() else {
        panic!("Prepared")
    };
    for i in 0..8 {
        w.try_submit(FirmwareLedgerRequest {
            intent: i + 2,
            expected_sequence: 0,
            review_sha256: String::new(),
            action: FirmwareLedgerAction::Inspect(attempt.clone()),
        })
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while w.events.len() != i as usize + 1 {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(5));
        }
    }
    w.try_submit(FirmwareLedgerRequest {
        intent: 11,
        expected_sequence: 0,
        review_sha256: String::new(),
        action: FirmwareLedgerAction::Inspect(attempt.clone()),
    })
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !w.is_stopped() {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(5));
    }
    let mut store = w.join().unwrap();
    assert!(store.firmware_publication_uncertain);
    let l = FirmwareAttemptLedger::open(&mut store).unwrap();
    assert!(
        l.load(&attempt)
            .unwrap()
            .recent
            .iter()
            .any(|r| r.label == "Evidence fault")
    );
}
#[test]
fn cancellation_after_durable_start_preserves_observation_and_returns_no_resume() {
    let t = Temp::new();
    let w = FirmwareLedgerWorker::spawn(LibraryStore::open(&t.0).unwrap()).unwrap();
    let j = job();
    w.try_submit(FirmwareLedgerRequest {
        intent: 1,
        expected_sequence: 0,
        review_sha256: j.digest().hex(),
        action: FirmwareLedgerAction::Begin {
            job: j,
            origin: FirmwareObservationOrigin::LocalObservation,
        },
    })
    .unwrap();
    let FirmwareLedgerReply::Prepared { attempt, .. } = event(&w).result.unwrap() else {
        panic!("expected Prepared")
    };
    let mut store = w.join().unwrap();
    assert!(store.firmware_publication_uncertain);
    let l = FirmwareAttemptLedger::open(&mut store).unwrap();
    let found = l.load(&attempt).unwrap();
    assert!(found.recent.iter().any(|r| r.label == "Evidence fault"));
    assert!(!found.can_resume());
    assert!(!found.permits_flash());
}

fn begin_worker(w: &FirmwareLedgerWorker, j: &Arc<ReviewedFirmwareUpdate>) -> FirmwareAttemptId {
    w.try_submit(FirmwareLedgerRequest {
        intent: 1,
        expected_sequence: 0,
        review_sha256: j.digest().hex(),
        action: FirmwareLedgerAction::Begin {
            job: j.clone(),
            origin: FirmwareObservationOrigin::LocalObservation,
        },
    })
    .unwrap();
    let FirmwareLedgerReply::Prepared { attempt, .. } = event(w).result.unwrap() else {
        panic!("Prepared")
    };
    attempt
}
fn checkpoint_worker(
    w: &FirmwareLedgerWorker,
    j: &ReviewedFirmwareUpdate,
) -> FirmwareBlockDescription {
    w.try_submit(FirmwareLedgerRequest {
        intent: 2,
        expected_sequence: 0,
        review_sha256: j.digest().hex(),
        action: FirmwareLedgerAction::Checkpoint { ordinal: 0 },
    })
    .unwrap();
    let FirmwareLedgerReply::Checkpoint(checkpoint) = event(w).result.unwrap() else {
        panic!("Checkpoint")
    };
    checkpoint.action().unwrap().clone()
}
fn assert_sticky_history(w: FirmwareLedgerWorker, t: &Temp, id: &FirmwareAttemptId, prior: &[u8]) {
    assert!(w.is_stopped());
    assert!(
        w.try_submit(FirmwareLedgerRequest {
            intent: 99,
            expected_sequence: 0,
            review_sha256: String::new(),
            action: FirmwareLedgerAction::Inspect(id.clone()),
        })
        .is_err()
    );
    assert!(w.try_next().is_none());
    let mut store = w.join().unwrap();
    assert!(store.firmware_publication_uncertain);
    assert!(store.commit(store.manifest.clone()).is_err());
    assert!(
        store
            .publish_blob(b"ordinary publication after stopped owner")
            .is_err()
    );
    let path =
        t.0.join("firmware-attempt-ledger")
            .join(format!("{}.journal", id.as_str()));
    let retained = std::fs::read(path).unwrap();
    assert_eq!(&retained[..prior.len()], prior);
    let ledger = FirmwareAttemptLedger::open(&mut store).unwrap();
    let found = ledger.load(id).unwrap();
    assert!(found.recent.iter().any(|r| r.label == "Evidence fault"));
    assert!(!found.can_resume());
    assert!(!found.permits_flash());
    assert_eq!(
        found.projection.unwrap().state(),
        &FirmwareDerivedLocalState::Indeterminate
    );
}
fn revocation_barrier(after_enqueue: bool, raw_fault: bool) {
    let t = Temp::new();
    let (reached, reached_rx) = bounded(1);
    let (release, release_rx) = bounded(1);
    let w = FirmwareLedgerWorker::spawn_inner(
        LibraryStore::open(&t.0).unwrap(),
        Some(ReplyBoundaryHook {
            intent: 2,
            after_enqueue,
            reached,
            release: release_rx,
        }),
    )
    .unwrap();
    let j = job();
    let id = begin_worker(&w, &j);
    w.try_submit(FirmwareLedgerRequest {
        intent: 2,
        expected_sequence: 0,
        review_sha256: j.digest().hex(),
        action: FirmwareLedgerAction::Checkpoint { ordinal: 0 },
    })
    .unwrap();
    reached_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    // This is an actual durable checkpoint in the real worker, with the thread
    // paused either before gated enqueue or after enqueue and before delivery.
    let path =
        t.0.join("firmware-attempt-ledger")
            .join(format!("{}.journal", id.as_str()));
    let prior = std::fs::read(&path).unwrap();
    assert_eq!(w.events.len(), usize::from(after_enqueue));
    if raw_fault {
        assert!(
            w.try_record_callback(FirmwareCallbackDto {
                sequence: 1,
                at_ms: 1,
                clock_origin: "fixture".into(),
                epoch: SessionEpoch(1),
                binding_sha256: crate::store::digest(b"claim"),
                raw: vec![0; 519],
            })
            .is_err()
        );
    } else {
        w.stop();
    }
    if after_enqueue {
        assert!(matches!(event(&w).result, Err(e) if e.kind == ErrorKind::Indeterminate));
        release.send(()).unwrap();
    } else {
        release.send(()).unwrap();
        assert!(matches!(event(&w).result, Err(e) if e.kind == ErrorKind::Indeterminate));
    }
    assert_sticky_history(w, &t, &id, &prior);
}
#[test]
fn stop_after_checkpoint_reread_before_enqueue_revokes_reply() {
    revocation_barrier(false, false);
}
#[test]
fn stop_after_enqueue_before_delivery_revokes_reply() {
    revocation_barrier(true, false);
}
#[test]
fn raw_fault_after_checkpoint_reread_before_enqueue_revokes_reply() {
    revocation_barrier(false, true);
}
#[test]
fn raw_fault_after_enqueue_before_delivery_revokes_reply() {
    revocation_barrier(true, true);
}
#[test]
fn invalid_send_after_checkpoint_faults_owner_even_without_cursor() {
    let t = Temp::new();
    let w = FirmwareLedgerWorker::spawn(LibraryStore::open(&t.0).unwrap()).unwrap();
    let j = job();
    let id = begin_worker(&w, &j);
    let description = checkpoint_worker(&w, &j);
    let path =
        t.0.join("firmware-attempt-ledger")
            .join(format!("{}.journal", id.as_str()));
    let prior = std::fs::read(path).unwrap();
    let mut invalid = send(&description);
    invalid.wire_sha256 = crate::store::digest(b"wrong wire");
    w.try_submit(FirmwareLedgerRequest {
        intent: 3,
        expected_sequence: 1,
        review_sha256: j.digest().hex(),
        action: FirmwareLedgerAction::Send(invalid),
    })
    .unwrap();
    assert!(event(&w).result.is_err());
    assert_sticky_history(w, &t, &id, &prior);
}
#[test]
fn invalid_cancel_after_mode_boundary_faults_owner_even_without_cursor() {
    let t = Temp::new();
    let w = FirmwareLedgerWorker::spawn(LibraryStore::open(&t.0).unwrap()).unwrap();
    let j = job();
    let id = begin_worker(&w, &j);
    w.try_submit(FirmwareLedgerRequest {
        intent: 2,
        expected_sequence: 0,
        review_sha256: j.digest().hex(),
        action: FirmwareLedgerAction::ModeBoundary {
            raw: vec![0xf0, 0x7e, 0x48, 9, 0xf7],
            note: "foreign observation; device effects unknown".into(),
        },
    })
    .unwrap();
    assert!(matches!(
        event(&w).result,
        Ok(FirmwareLedgerReply::Recorded { sequence: 1 })
    ));
    let path =
        t.0.join("firmware-attempt-ledger")
            .join(format!("{}.journal", id.as_str()));
    let prior = std::fs::read(path).unwrap();
    w.try_submit(FirmwareLedgerRequest {
        intent: 3,
        expected_sequence: 1,
        review_sha256: j.digest().hex(),
        action: FirmwareLedgerAction::Close(FirmwareHostClosure::CancelledBeforeEntry),
    })
    .unwrap();
    assert!(event(&w).result.is_err());
    assert_sticky_history(w, &t, &id, &prior);
}
fn lifecycle_plan(j: &ReviewedFirmwareUpdate) -> FirmwareLifecyclePlanDto {
    FirmwareLifecyclePlanDto {
        review_sha256: j.digest().hex(),
        profile_sha256: JobSha256::of(b"rule").hex(),
        origin: FirmwareObservationOrigin::MockSimulation,
        nonblock_actions: 1,
        epoch: SessionEpoch(7),
        binding: FirmwareUsbObservation {
            topology: "test".into(),
            vendor: 1,
            product: 2,
            serial: "test".into(),
            interface: "test".into(),
            endpoint: "test".into(),
        },
        generation: 1,
        clock_origin: "test".into(),
    }
}
fn local_action(
    w: &FirmwareLedgerWorker,
    j: &Arc<ReviewedFirmwareUpdate>,
) -> FirmwareLocalActionCheckpoint {
    w.try_submit(FirmwareLedgerRequest {
        intent: 1,
        expected_sequence: 0,
        review_sha256: j.digest().hex(),
        action: FirmwareLedgerAction::Begin {
            job: j.clone(),
            origin: FirmwareObservationOrigin::MockSimulation,
        },
    })
    .unwrap();
    event(w).result.unwrap();
    let plan = lifecycle_plan(j);
    w.try_submit(FirmwareLedgerRequest {
        intent: 2,
        expected_sequence: 0,
        review_sha256: j.digest().hex(),
        action: FirmwareLedgerAction::LifecyclePrepared(plan.clone()),
    })
    .unwrap();
    assert!(matches!(
        event(w).result,
        Ok(FirmwareLedgerReply::Recorded { sequence: 1 })
    ));
    let wire = vec![0xf0, 1, 0xf7];
    let a = FirmwareLifecycleActionDescription {
        ordinal: 0,
        kind: FirmwareLifecycleKind::IdentityQuery,
        wire_sha256: JobSha256::of(&wire).hex(),
        wire,
        block: None,
        epoch: plan.epoch,
        binding_sha256: plan.binding.digest(),
        generation: 1,
        rule_sha256: plan.profile_sha256,
    };
    w.try_submit(FirmwareLedgerRequest {
        intent: 3,
        expected_sequence: 1,
        review_sha256: j.digest().hex(),
        action: FirmwareLedgerAction::LifecycleCheckpoint(a),
    })
    .unwrap();
    let FirmwareLedgerReply::ActionCheckpoint(c) = event(w).result.unwrap() else {
        panic!("action checkpoint")
    };
    c
}
#[test]
fn lifecycle_receipt_is_bound_to_actual_worker_and_stop_revokes_it() {
    let t = Temp::new();
    let w = FirmwareLedgerWorker::spawn(LibraryStore::open(&t.0).unwrap()).unwrap();
    let c = local_action(&w, &job());
    assert_eq!(c.sequence(), 2);
    assert_eq!(c.run_id(), w.run_id());
    w.request_stop();
    assert!(w.try_local_offer_fence(c).is_err());
    assert!(w.join().unwrap().firmware_publication_uncertain);
}
#[test]
fn foreign_worker_receipt_cannot_acquire_local_offer_fence() {
    let a = Temp::new();
    let b = Temp::new();
    let wa = FirmwareLedgerWorker::spawn(LibraryStore::open(&a.0).unwrap()).unwrap();
    let wb = FirmwareLedgerWorker::spawn(LibraryStore::open(&b.0).unwrap()).unwrap();
    let c = local_action(&wa, &job());
    assert!(wb.try_local_offer_fence(c).is_err());
    wa.join().unwrap();
    wb.join().unwrap();
}
#[test]
fn consumed_local_fence_is_one_use_and_no_flash_permission() {
    let t = Temp::new();
    let w = FirmwareLedgerWorker::spawn(LibraryStore::open(&t.0).unwrap()).unwrap();
    let c = local_action(&w, &job());
    assert!(!c.permits_flash());
    let fence = w.try_local_offer_fence(c).unwrap();
    drop(fence);
    assert_eq!(w.offer.load(Ordering::SeqCst), 0);
    w.join().unwrap();
}

fn epoch_begin_worker(
    w: &FirmwareLedgerWorker,
    j: &Arc<ReviewedFirmwareUpdate>,
) -> FirmwareAttemptId {
    w.try_submit(FirmwareLedgerRequest {
        intent: 1,
        expected_sequence: 0,
        review_sha256: j.digest().hex(),
        action: FirmwareLedgerAction::Begin {
            job: j.clone(),
            origin: FirmwareObservationOrigin::MockSimulation,
        },
    })
    .unwrap();
    let FirmwareLedgerReply::Prepared { attempt: id, .. } = event(w).result.unwrap() else {
        panic!("Epoch test actual Begin reply");
    };
    w.try_submit(FirmwareLedgerRequest {
        intent: 2,
        expected_sequence: 0,
        review_sha256: j.digest().hex(),
        action: FirmwareLedgerAction::LifecyclePrepared(
            crate::firmware_attempt_ledger::tests::epoch_plan(j),
        ),
    })
    .unwrap();
    let e = event(w);
    assert_eq!(e.run, w.run_id());
    assert_eq!(e.intent, 2);
    assert!(matches!(
        e.result,
        Ok(FirmwareLedgerReply::Recorded { sequence: 1 })
    ));
    id
}
#[test]
fn epoch_reply_names_actual_same_writer_run_intent_and_durable_sequence() {
    use crate::firmware_attempt_ledger::tests::{epoch_entry, epoch_next, epoch_plan};
    let t = Temp::new();
    let j = job();
    let w = FirmwareLedgerWorker::spawn(LibraryStore::open(&t.0).unwrap()).unwrap();
    let id = epoch_begin_worker(&w, &j);
    let entry = epoch_entry(&epoch_plan(&j));
    w.try_submit(FirmwareLedgerRequest {
        intent: 3,
        expected_sequence: 1,
        review_sha256: j.digest().hex(),
        action: FirmwareLedgerAction::LifecycleEpoch(Box::new(entry.clone())),
    })
    .unwrap();
    let e = event(&w);
    assert_eq!(e.run, w.run_id());
    assert_eq!(e.intent, 3);
    assert!(matches!(
        e.result,
        Ok(FirmwareLedgerReply::Recorded { sequence: 2 })
    ));
    let boot = epoch_next(&entry, FirmwareLifecycleEpochRole::Bootloader, 0, 0);
    w.try_submit(FirmwareLedgerRequest {
        intent: 4,
        expected_sequence: 2,
        review_sha256: j.digest().hex(),
        action: FirmwareLedgerAction::LifecycleEpoch(Box::new(boot.clone())),
    })
    .unwrap();
    let e = event(&w);
    assert_eq!(e.run, w.run_id());
    assert_eq!(e.intent, 4);
    assert!(matches!(
        e.result,
        Ok(FirmwareLedgerReply::Recorded { sequence: 3 })
    ));
    w.try_submit(FirmwareLedgerRequest {
        intent: 5,
        expected_sequence: 3,
        review_sha256: j.digest().hex(),
        action: FirmwareLedgerAction::LifecycleClosed(
            FirmwareLifecycleHostClosure::ClosedBeforeAnyOffer,
        ),
    })
    .unwrap();
    assert!(matches!(
        event(&w).result,
        Ok(FirmwareLedgerReply::Observation(_))
    ));
    let mut store = w.join().unwrap();
    let ledger = FirmwareAttemptLedger::open(&mut store).unwrap();
    let retained = ledger.load(&id).unwrap();
    assert_eq!(retained.lifecycle.as_ref().unwrap().epochs(), [entry, boot]);
    assert!(retained.local_checkpoint_confirmed);
    assert!(!retained.permits_flash());
    assert!(!retained.can_resume());
}
#[test]
fn epoch_stale_expected_sequence_stops_real_writer_without_recording_new_role() {
    let t = Temp::new();
    let j = job();
    let w = FirmwareLedgerWorker::spawn(LibraryStore::open(&t.0).unwrap()).unwrap();
    let id = epoch_begin_worker(&w, &j);
    w.try_submit(FirmwareLedgerRequest {
        intent: 3,
        expected_sequence: 0,
        review_sha256: j.digest().hex(),
        action: FirmwareLedgerAction::LifecycleEpoch(Box::new(
            crate::firmware_attempt_ledger::tests::epoch_entry(
                &crate::firmware_attempt_ledger::tests::epoch_plan(&j),
            ),
        )),
    })
    .unwrap();
    assert!(event(&w).result.is_err());
    assert!(w.is_stopped());
    let mut store = w.join().unwrap();
    let ledger = FirmwareAttemptLedger::open(&mut store).unwrap();
    let retained = ledger.load(&id).unwrap();
    assert!(retained.lifecycle.unwrap().epochs().is_empty());
    assert!(store.firmware_publication_uncertain);
}
#[test]
fn epoch_stop_before_enqueue_and_before_consumption_revoke_actual_durable_reply() {
    for after_enqueue in [false, true] {
        let t = Temp::new();
        let (reached, rx) = bounded(1);
        let (release, release_rx) = bounded(1);
        let w = FirmwareLedgerWorker::spawn_inner(
            LibraryStore::open(&t.0).unwrap(),
            Some(ReplyBoundaryHook {
                intent: 3,
                after_enqueue,
                reached,
                release: release_rx,
            }),
        )
        .unwrap();
        let j = job();
        let id = epoch_begin_worker(&w, &j);
        w.try_submit(FirmwareLedgerRequest {
            intent: 3,
            expected_sequence: 1,
            review_sha256: j.digest().hex(),
            action: FirmwareLedgerAction::LifecycleEpoch(Box::new(
                crate::firmware_attempt_ledger::tests::epoch_entry(
                    &crate::firmware_attempt_ledger::tests::epoch_plan(&j),
                ),
            )),
        })
        .unwrap();
        rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let path =
            t.0.join("firmware-attempt-ledger")
                .join(format!("{}.journal", id.as_str()));
        let bytes = std::fs::read(&path).unwrap();
        w.request_stop();
        if after_enqueue {
            assert!(matches!(event(&w).result,Err(e)if e.kind==ErrorKind::Indeterminate));
            release.send(()).unwrap();
        } else {
            release.send(()).unwrap();
            assert!(matches!(event(&w).result,Err(e)if e.kind==ErrorKind::Indeterminate));
        }
        let mut store = w.join().unwrap();
        assert!(store.firmware_publication_uncertain);
        let ledger = FirmwareAttemptLedger::open(&mut store).unwrap();
        let retained = ledger.load(&id).unwrap();
        assert_eq!(retained.lifecycle.unwrap().epochs().len(), 1);
        assert_eq!(
            std::fs::read(path).unwrap().get(..bytes.len()),
            Some(bytes.as_slice())
        );
    }
}

#[test]
fn actual_os_thread_spawn_failure_returns_exact_store_and_held_lock() {
    let t = Temp::new();
    let mut store = LibraryStore::open(&t.0).unwrap();
    store.recovery_note = Some("retained original startup marker".into());
    let result = FirmwareLedgerWorker::spawn_retaining_store_with_builder(
        store,
        thread::Builder::new()
            .name("gp200-ledger-failed-spawn".into())
            .stack_size(usize::MAX / 2),
    );
    let (error, returned) = match result {
        Err(failed) => failed.into_parts(),
        Ok(worker) => {
            drop(worker.join().unwrap());
            panic!("unallocatable real OS thread stack must fail");
        }
    };
    assert_eq!(error.kind, ErrorKind::Io);
    assert!(!error.message.is_empty());
    assert_eq!(returned.root(), t.0.as_path());
    assert_eq!(
        returned.recovery_note.as_deref(),
        Some("retained original startup marker")
    );
    assert!(matches!(LibraryStore::open(&t.0), Err(e) if e.kind == ErrorKind::Busy));
    assert!(!t.0.join("firmware-attempt-ledger").exists());
    drop(returned);
    LibraryStore::open(&t.0).unwrap();
}
#[test]
fn owning_start_success_uses_actual_thread_and_returns_original_store_after_join() {
    let t = Temp::new();
    let mut store = LibraryStore::open(&t.0).unwrap();
    store.recovery_note = Some("owning successful start marker".into());
    let worker = match FirmwareLedgerWorker::spawn_retaining_store(store) {
        Ok(worker) => worker,
        Err(failed) => panic!("{failed:?}"),
    };
    assert!(matches!(LibraryStore::open(&t.0), Err(e) if e.kind == ErrorKind::Busy));
    let returned = worker.join().unwrap();
    assert_eq!(
        returned.recovery_note.as_deref(),
        Some("owning successful start marker")
    );
    assert!(matches!(LibraryStore::open(&t.0), Err(e) if e.kind == ErrorKind::Busy));
    drop(returned);
    LibraryStore::open(&t.0).unwrap();
}

#[test]
fn entry_actual_worker_two_distinct_one_use_receipts_and_second_stop_revocation() {
    let t = Temp::new();
    let w = FirmwareLedgerWorker::spawn(LibraryStore::open(&t.0).unwrap()).unwrap();
    let j = job();
    let mut e = crate::firmware_attempt_ledger::tests::entry_study(&j);
    e.plan.origin = FirmwareObservationOrigin::LocalObservation;
    e.study_sha256 = FirmwareEntryObservationPlanDto::digest_for(&e.plan, &e.candidates);
    begin_worker(&w, &j);
    w.try_submit(FirmwareLedgerRequest {
        intent: 2,
        expected_sequence: 0,
        review_sha256: j.digest().hex(),
        action: FirmwareLedgerAction::LifecycleEntryPrepared(e.clone()),
    })
    .unwrap();
    assert!(matches!(
        event(&w).result,
        Ok(FirmwareLedgerReply::Recorded { sequence: 1 })
    ));
    let mut sequence = 1;
    for ordinal in 0..2 {
        let a = crate::firmware_attempt_ledger::tests::entry_host_action(&e, ordinal);
        let base = 3 + ordinal * 3;
        w.try_submit(FirmwareLedgerRequest {
            intent: base,
            expected_sequence: sequence,
            review_sha256: j.digest().hex(),
            action: FirmwareLedgerAction::LifecycleCheckpoint(a.clone()),
        })
        .unwrap();
        let FirmwareLedgerReply::ActionCheckpoint(c) = event(&w).result.unwrap() else {
            panic!("entry checkpoint");
        };
        sequence += 1;
        assert_eq!(c.action().ordinal, ordinal);
        assert_eq!(c.sequence(), sequence);
        assert_eq!(c.run_id(), w.run_id());
        assert!(!c.permits_flash());
        if ordinal == 1 {
            w.request_stop();
            assert!(w.try_local_offer_fence(c).is_err());
            break;
        }
        drop(w.try_local_offer_fence(c).unwrap());
        assert_eq!(w.offer.load(Ordering::SeqCst), 0);
        w.try_submit(FirmwareLedgerRequest {
            intent: base + 1,
            expected_sequence: sequence,
            review_sha256: j.digest().hex(),
            action: FirmwareLedgerAction::LifecycleSend(FirmwareLifecycleSendObservation {
                action: ordinal,
                wire_sha256: a.wire_sha256.clone(),
                attempted: a.wire.len() as u64,
                extent: FirmwareOsExtent::Known(a.wire.len() as u64),
                drain: FirmwareDrainObservation::Returned,
                started_sequence: 0,
                finished_sequence: Some(0),
                started_at_ns: 1,
                finished_at_ns: Some(2),
            }),
        })
        .unwrap();
        event(&w).result.unwrap();
        sequence += 1;
        w.try_submit(FirmwareLedgerRequest {
            intent: base + 2,
            expected_sequence: sequence,
            review_sha256: j.digest().hex(),
            action: FirmwareLedgerAction::LifecycleEntryHostSettled(FirmwareEntryHostSettlement {
                action: ordinal,
                wire_sha256: a.wire_sha256,
                study_sha256: e.study_sha256.clone(),
            }),
        })
        .unwrap();
        event(&w).result.unwrap();
        sequence += 1;
    }
    let mut store = w.join().unwrap();
    let ledger = FirmwareAttemptLedger::open(&mut store).unwrap();
    let id = ledger.attempt_ids().unwrap().pop().unwrap();
    let retained = ledger.load(&id).unwrap();
    assert!(!retained.can_resume());
    assert_eq!(retained.lifecycle.unwrap().next_action(), 1);
}
