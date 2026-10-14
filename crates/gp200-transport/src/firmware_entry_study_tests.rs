use super::*;
use std::os::unix::fs::PermissionsExt;
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};
static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[derive(Clone, Copy, PartialEq, Eq)]
enum ExpiryBoundary {
    Authorization,
    Native,
}
struct RereadExpiryHook {
    boundary: ExpiryBoundary,
    approved: Instant,
    deadline: Duration,
    reread_reached: Rc<Cell<bool>>,
    pause_consumed: Rc<Cell<bool>>,
}
thread_local! {
    static REREAD_EXPIRY: RefCell<Option<RereadExpiryHook>> = const { RefCell::new(None) };
}
struct ScopedRereadExpiry {
    previous: Option<RereadExpiryHook>,
    reread_reached: Rc<Cell<bool>>,
    pause_consumed: Rc<Cell<bool>>,
}
impl ScopedRereadExpiry {
    fn new(approved: Instant, deadline: Duration, boundary: ExpiryBoundary) -> Self {
        assert!(deadline <= Duration::from_millis(100));
        let reread_reached = Rc::new(Cell::new(false));
        let pause_consumed = Rc::new(Cell::new(false));
        let previous = REREAD_EXPIRY.with(|slot| {
            slot.replace(Some(RereadExpiryHook {
                boundary,
                approved,
                deadline,
                reread_reached: reread_reached.clone(),
                pause_consumed: pause_consumed.clone(),
            }))
        });
        Self {
            previous,
            reread_reached,
            pause_consumed,
        }
    }
    fn assert_consumed_after_reread(&self) {
        assert!(self.reread_reached.get(), "genuine rereads completed");
        assert!(self.pause_consumed.get(), "one-shot expiry pause consumed");
    }
}
impl Drop for ScopedRereadExpiry {
    fn drop(&mut self) {
        REREAD_EXPIRY.with(|slot| slot.replace(self.previous.take()));
    }
}
fn consume_expiry_pause(boundary: ExpiryBoundary) {
    // Release the thread-local borrow before waiting. The hook cannot affect
    // another test thread, and Drop restores it even if the test unwinds.
    let hook = REREAD_EXPIRY.with(|slot| slot.borrow_mut().take());
    let Some(hook) = hook else {
        return;
    };
    if hook.boundary != boundary {
        REREAD_EXPIRY.with(|slot| slot.replace(Some(hook)));
        return;
    }
    assert!(hook.reread_reached.get(), "pause follows genuine rereads");
    assert!(!hook.pause_consumed.replace(true), "one-shot expiry pause");
    while hook.approved.elapsed() < hook.deadline {
        thread::sleep(hook.deadline.saturating_sub(hook.approved.elapsed()));
    }
}
pub(super) fn after_authorization_reread() {
    REREAD_EXPIRY.with(|slot| {
        if let Some(hook) = slot.borrow().as_ref()
            && hook.boundary == ExpiryBoundary::Authorization
        {
            hook.reread_reached.set(true);
        }
    });
    consume_expiry_pause(ExpiryBoundary::Authorization);
}
pub(super) fn after_native_reread() {
    REREAD_EXPIRY.with(|slot| {
        if let Some(hook) = slot.borrow().as_ref()
            && hook.boundary == ExpiryBoundary::Native
        {
            hook.reread_reached.set(true);
        }
    });
}
pub(super) fn before_native_immediate() {
    consume_expiry_pause(ExpiryBoundary::Native);
}

#[test]
fn scoped_reread_expiry_hook_is_removed_when_unused_or_unwinding() {
    assert!(REREAD_EXPIRY.with(|slot| slot.borrow().is_none()));
    {
        let _scope = ScopedRereadExpiry::new(
            Instant::now(),
            Duration::from_millis(100),
            ExpiryBoundary::Authorization,
        );
        thread::spawn(|| assert!(REREAD_EXPIRY.with(|slot| slot.borrow().is_none())))
            .join()
            .unwrap();
    }
    assert!(REREAD_EXPIRY.with(|slot| slot.borrow().is_none()));
    let unwind = std::panic::catch_unwind(|| {
        let _scope = ScopedRereadExpiry::new(
            Instant::now(),
            Duration::from_millis(100),
            ExpiryBoundary::Native,
        );
        panic!("fixture hook unwind");
    });
    assert!(unwind.is_err());
    assert!(REREAD_EXPIRY.with(|slot| slot.borrow().is_none()));
    after_authorization_reread();
    after_native_reread();
    before_native_immediate();
}

struct Fixture {
    root: PathBuf,
    inputs: EntryStudyInputs,
    reservation: FirmwareCaptureReservation,
}
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "gp200-entry-review-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        let path = root.join("source-and-context.txt");
        fs::write(&path, b"Source/recovery fixture only: no physical approval").unwrap();
        let bytes = fs::read(&path).unwrap();
        let pin = EntryStudyFilePin {
            path,
            bytes: bytes.len() as u64,
            sha256: JobSha256::of(&bytes).hex(),
        };
        Self {
            inputs: EntryStudyInputs {
                sources: vec![pin.clone()],
                loss: pin.clone(),
                recovery: pin.clone(),
                vendor_guidance: pin,
                audit_directory: root.clone(),
                observation_seconds: 1,
                approval_wait_seconds: 30,
            },
            reservation: FirmwareCaptureReservation {
                actor_run: "fixture-actual-private-tests".into(),
                reservation: "fixture".into(),
                normal_owner: "fixture".into(),
                epoch: SessionEpoch(1),
                usb: FirmwareUsbObservation {
                    topology: "fixture".into(),
                    interface: "fixture".into(),
                    endpoint: "fixture".into(),
                    vendor: 1,
                    product: 2,
                    serial: "fixture".into(),
                },
                clock_origin: "fixture".into(),
                prepared_monotonic_ns: 1,
                prepared_wall_millis: 1,
                initial_fence: FirmwareCaptureFence {
                    allocated: 0,
                    processed: 0,
                },
            },
            root,
        }
    }
    fn draft(&self) -> EntryStudyDraft {
        EntryStudyDraft::prepare(
            crate::firmware_tests::job(),
            self.reservation.clone(),
            "genuine-test-acquisition".into(),
            JobSha256::of(b"index").hex(),
            serde_json::json!({"test":"no device"}),
            vec![],
            self.inputs.clone(),
        )
        .unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}
#[test]
fn exact_signal_is_consuming_same_run_and_never_production_transfer_authority() {
    let f = Fixture::new();
    let draft = f.draft();
    let sha = draft.digest();
    let authorization = draft
        .approve_exact(&format!("EXECUTE ENTRY ONCE {sha}"))
        .unwrap();
    assert!(authorization.matches_job(&crate::firmware_tests::job()));
    assert!(crate::firmware_profile::issue_entry().is_err());
    assert!(crate::firmware_profile::issue_physical().is_err());
    assert!(crate::firmware_profile::issue_boot_binding().is_err());
    assert_eq!(authorization.review.candidates, entry_candidates());
    assert!(
        authorization
            .review
            .unknowns
            .contains("Linux recovery are unaccepted")
    );
}
#[test]
fn stale_signal_source_drift_and_elapsed_time_never_authorize_entry() {
    let f = Fixture::new();
    let draft = f.draft();
    assert!(
        draft
            .approve_exact(&format!(
                "EXECUTE ENTRY ONCE {}",
                JobSha256::of(b"old review").hex()
            ))
            .is_err()
    );
    let draft = f.draft();
    let signal = format!("EXECUTE ENTRY ONCE {}", draft.digest());
    fs::write(&f.inputs.sources[0].path, b"drift").unwrap();
    assert!(draft.approve_exact(&signal).is_err());
    let f = Fixture::new();
    let mut draft = f.draft();
    let signal = format!("EXECUTE ENTRY ONCE {}", draft.digest());
    draft.created = Instant::now() - Duration::from_secs(31);
    assert!(draft.expired());
    assert!(draft.approve_exact(&signal).is_err());
}

#[test]
fn captured_archive_drift_after_exact_signal_prevents_revalidation() {
    let f = Fixture::new();
    let path = f.root.join("captured-archive.zip");
    fs::write(&path, b"genuine retained archive fixture").unwrap();
    let bytes = fs::read(&path).unwrap();
    let pin = EntryStudyFilePin {
        path: path.clone(),
        bytes: bytes.len() as u64,
        sha256: JobSha256::of(&bytes).hex(),
    };
    let draft = EntryStudyDraft::prepare(
        crate::firmware_tests::job(),
        f.reservation.clone(),
        "current acquisition".into(),
        JobSha256::of(b"index").hex(),
        serde_json::json!({"test":"no hardware"}),
        vec![pin],
        f.inputs.clone(),
    )
    .unwrap();
    let signal = format!("EXECUTE ENTRY ONCE {}", draft.digest());
    let authorization = draft.approve_exact(&signal).unwrap();
    fs::write(path, b"changed archive").unwrap();
    assert!(authorization.revalidate().is_err());
}

#[test]
fn authority_expiring_during_actual_executable_reread_does_not_pass() {
    let f = Fixture::new();
    let draft = f.draft();
    let signal = format!("EXECUTE ENTRY ONCE {}", draft.digest());
    let mut authorization = draft.approve_exact(&signal).unwrap();
    // A shorter private deadline and scoped pause exercise genuine reads even
    // when the optimized executable hashes in less than 100 ms.
    authorization.approved = Instant::now();
    let deadline = Duration::from_millis(100);
    let expiry = ScopedRereadExpiry::new(
        authorization.approved,
        deadline,
        ExpiryBoundary::Authorization,
    );
    assert!(authorization.approved.elapsed() < deadline);
    let error = authorization
        .revalidate_with_deadline(deadline)
        .unwrap_err();
    assert!(
        error
            .message
            .contains("expired while rereading exact inputs")
    );
    expiry.assert_consumed_after_reread();
    assert!(authorization.approved.elapsed() >= deadline);
    println!(
        "FW06_B_REAL_REREAD_EXPIRY {}",
        serde_json::json!({"actual_executable_reread":true,"bounded_private_test_deadline_ms":100,"expired_after_reads":true,"authority_rejected":true,"hardware_operations":0})
    );
}

#[test]
fn actual_passive_fixture_monitor_blocks_second_offer_on_devnum_change_and_keeps_transition_sticky()
{
    let f = firmware_entry_enumeration::tests::Fixture::new();
    let audit_dir = f.root.join("audit");
    fs::create_dir(&audit_dir).unwrap();
    let mut audit = EntryAudit::start_at(
        f.binding.clone(),
        audit_dir.clone(),
        1,
        PassivePaths {
            usb: f.root.join("usb"),
            sound: f.root.join("sound"),
        },
    )
    .unwrap();
    audit.fresh_before_offer(0).unwrap();
    audit.mark_attempt();
    fs::write(Path::new(&f.binding.topology).join("devnum"), "5").unwrap();
    assert!(audit.fresh_before_offer(1).is_err());
    fs::write(Path::new(&f.binding.topology).join("devnum"), "4").unwrap();
    assert!(audit.fresh_before_offer(1).is_err());
    assert!(audit.healthy().is_err());
    audit.close().unwrap();
    assert!(audit.thread.is_none());
    let rows = fs::read_to_string(audit_dir.join("entry-host-observations.jsonl"))
        .unwrap()
        .lines()
        .map(|s| serde_json::from_str::<serde_json::Value>(s).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        rows.first().unwrap()["receipt"]["owned"]["fields"]["devnum"],
        "4"
    );
    assert!(rows.iter().any(|row| row["ordinal"] == 1
        && row["receipt"]["owned"]["fields"]["devnum"] == "5"
        && row["sticky_transition"] == true));
    assert_eq!(rows.last().unwrap()["kind"], "passive-monitor-joined");
    println!(
        "FW06_B_PASSIVE_TRANSITION {}",
        serde_json::json!({"actual_mock_sysfs_files":true,"retained_card_id":"GP200","second_offer_prohibited":true,"transition_sticky":true,"actual_monitor_joined":audit.thread.is_none(),"audit_rows":rows.len(),"hardware_operations":0})
    );
}

#[test]
fn final_native_callback_after_durable_work_rechecks_deadline_stop_reader_and_exact_frontier() {
    use crate::{
        firmware_backend::FirmwareReceiver,
        firmware_raw::FirmwareRawReader,
        firmware_service::OwnerStop,
        raw_backend::{InputReadError, RawMidiInput},
    };
    struct ActualFixtureInput;
    impl RawMidiInput for ActualFixtureInput {
        fn read(&mut self, _buffer: &mut [u8]) -> Result<usize, InputReadError> {
            Err(InputReadError::WouldBlock)
        }
        fn check_status(&mut self) -> Result<(), ServiceError> {
            Ok(())
        }
    }
    let f = Fixture::new();
    let draft = f.draft();
    let signal = format!("EXECUTE ENTRY ONCE {}", draft.digest());
    let authorization = draft.approve_exact(&signal).unwrap();
    for case in [
        "expired-during-reread",
        "stop",
        "reader-fault",
        "late-original",
        "joined-reader",
        "passive-lock",
        "healthy",
        "partial-return",
        "unknown-return",
    ] {
        let sysfs = firmware_entry_enumeration::tests::Fixture::new();
        let audit_dir = sysfs.root.join("audit");
        fs::create_dir(&audit_dir).unwrap();
        let mut audit = EntryAudit::start_at(
            sysfs.binding.clone(),
            audit_dir.clone(),
            1,
            PassivePaths {
                usb: sysfs.root.join("usb"),
                sound: sysfs.root.join("sound"),
            },
        )
        .unwrap();
        let stop = Arc::new(OwnerStop::default());
        let native = Arc::new(EntryNativeFence {
            review: authorization.review.clone(),
            approved: Instant::now(),
            deadline: Duration::from_secs(120),
            audit_inode: authorization.audit_inode,
            owner_stop: OnceLock::new(),
            processed: AtomicU64::new(0),
        });
        native
            .owner_stop
            .set(stop.clone())
            .unwrap_or_else(|_| panic!("sole mock owner"));
        audit.native = Some(native);
        let plan = FirmwareLifecyclePlanDto {
            review_sha256: authorization.job.digest().hex(),
            profile_sha256: JobSha256::of(b"fixture").hex(),
            origin: FirmwareObservationOrigin::MockSimulation,
            nonblock_actions: 2,
            epoch: SessionEpoch(1),
            binding: sysfs.binding.clone(),
            generation: 1,
            clock_origin: "actual fixture source".into(),
        };
        let receiver = FirmwareReceiver::new(&plan);
        let mut reader = FirmwareRawReader::start(
            ActualFixtureInput,
            receiver.sink.clone(),
            Duration::from_secs(1),
        )
        .unwrap();
        audit.fresh_before_offer(0).unwrap();
        audit
            .write_record(
                "entry-write-intent-before-syscall",
                22,
                serde_json::json!({"fixture":true,"actual_native_invocations":0}),
                audit.now(),
                audit.now(),
            )
            .unwrap();
        match case {
            "stop" => stop.requested.store(true, Ordering::SeqCst),
            "reader-fault" => receiver.sink.reader_fault(),
            "late-original" => receiver.sink.receive(&[0xf0, 0x01, 0xf7]),
            "joined-reader" => {
                reader.close();
            }
            _ => {}
        }
        let state = audit.state.clone();
        let held = (case == "passive-lock").then(|| state.lock().unwrap());
        let calls = std::cell::Cell::new(0usize);
        // Deliberately age the audit clock: the post-write observation must
        // still wait its full window from the actual callback, not audit open.
        audit.origin = Instant::now() - Duration::from_secs(10);
        let expiry = (case == "expired-during-reread").then(|| {
            let native = Arc::get_mut(audit.native.as_mut().unwrap()).unwrap();
            native.approved = Instant::now();
            native.deadline = Duration::from_millis(100);
            let expiry =
                ScopedRereadExpiry::new(native.approved, native.deadline, ExpiryBoundary::Native);
            assert!(native.approved.elapsed() < native.deadline);
            expiry
        });
        let observed = audit.invoke_native(
            0,
            &receiver.sink,
            || reader.live_for_entry_fixture(),
            || {
                calls.set(calls.get() + 1);
                match case {
                    "partial-return" => Ok(3usize),
                    "unknown-return" => Err("actual mock unknown return"),
                    _ => Ok(22usize),
                }
            },
        );
        if let Some(expiry) = expiry.as_ref() {
            expiry.assert_consumed_after_reread();
            assert!(observed.as_ref().unwrap_err().message.contains(
                "Final native entry deadline, stop, reader fault or original frontier revoked invocation"
            ));
            let native = audit.native.as_ref().unwrap();
            assert!(native.approved.elapsed() >= native.deadline);
        }
        let invoked = matches!(case, "healthy" | "partial-return" | "unknown-return");
        if invoked {
            let expected = match case {
                "partial-return" => Ok(3usize),
                "unknown-return" => Err("actual mock unknown return"),
                _ => Ok(22usize),
            };
            assert_eq!(observed.unwrap().0, expected);
            assert_eq!(calls.get(), 1);
        } else {
            assert!(observed.is_err(), "{case}");
            assert_eq!(
                calls.get(),
                0,
                "{case}: no actual callback after durable work"
            );
        }
        drop(held);
        let retired = reader.close();
        assert_eq!(
            retired.observation().reader_join,
            FirmwareNormalReaderJoinObservation::Returned
        );
        let close_started = Instant::now();
        audit.close().unwrap();
        if invoked {
            assert!(close_started.elapsed() >= Duration::from_millis(900));
        }
        let rows = fs::read_to_string(audit_dir.join("entry-host-observations.jsonl")).unwrap();
        if !invoked {
            assert!(rows.contains("entry-native-invocation-denied"));
        }
        println!(
            "FW06_B_FINAL_NATIVE_BOUNDARY {}",
            serde_json::json!({"case":case,"actual_callback_invocations":calls.get(),"same_helper_as_alsa":true,"actual_raw_reader_joined":true,"durable_intent_present":rows.contains("entry-write-intent-before-syscall"),"hardware_operations":0})
        );
    }
}
