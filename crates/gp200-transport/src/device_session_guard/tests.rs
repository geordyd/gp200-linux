use super::*;
use crate::{MidiDeviceService, ReceiveSink, TransportBackend, TransportConfig};
use gp200_domain::{
    ConnectionState, DeviceCommand, DeviceEvent, DeviceSelection, DeviceService, IntentId,
    PortPairCandidate, no_wake,
};
use std::io::Write;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::process::{Child, Command, Stdio};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::thread;
use std::time::{Duration, Instant};

static NEXT: AtomicUsize = AtomicUsize::new(0);
const DEADLINE: Duration = Duration::from_secs(5);

struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "gp200-guard-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        Self(path)
    }
    fn root(&self) -> GuardRoot {
        GuardRoot::open_runtime(&self.0, rustix::process::geteuid().as_raw(), false).unwrap()
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn endpoint(name: &str) -> EndpointIdentity {
    EndpointIdentity {
        location: format!("fixture-location-{name}"),
        card_id: name.into(),
        device: 0,
        subdevice: 0,
    }
}
fn assert_busy(root: &GuardRoot, name: &str) {
    let error = DeviceSessionGuard::acquire(root, &[endpoint(name)])
        .err()
        .expect("contended lock");
    assert_eq!(error.kind, ErrorKind::Busy);
    assert!(!error.message.contains(name));
    assert!(!error.message.contains("fixture-location"));
}

struct State {
    opens: AtomicUsize,
    sends: AtomicUsize,
    drops: AtomicUsize,
    fault: AtomicBool,
}
impl State {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            opens: AtomicUsize::new(0),
            sends: AtomicUsize::new(0),
            drops: AtomicUsize::new(0),
            fault: AtomicBool::new(false),
        })
    }
}
struct FakeSession(Arc<State>);
impl MidiSession for FakeSession {
    fn send(&mut self, _: &[u8]) -> Result<(), ServiceError> {
        self.0.sends.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }
    fn health(&mut self) -> Result<(), ServiceError> {
        if self.0.fault.load(Ordering::Acquire) {
            Err(ServiceError::new(ErrorKind::Disconnected, "test unplug"))
        } else {
            Ok(())
        }
    }
}
impl Drop for FakeSession {
    fn drop(&mut self) {
        self.0.drops.fetch_add(1, Ordering::AcqRel);
    }
}
fn open(
    root: &GuardRoot,
    names: &[&str],
    state: Arc<State>,
) -> Result<Box<dyn MidiSession>, ServiceError> {
    open_guarded_at(
        root,
        &names.iter().map(|name| endpoint(name)).collect::<Vec<_>>(),
        || {
            state.opens.fetch_add(1, Ordering::AcqRel);
            Ok(Box::new(FakeSession(state)))
        },
    )
}

#[test]
fn contention_refuses_before_endpoint_construction_and_sends() {
    let temp = Temp::new();
    let root = temp.root();
    let state = State::new();
    let first = open(&root, &["private-usb-serial-123"], state.clone()).unwrap();
    let rejected = State::new();
    let error = open(&root, &["private-usb-serial-123"], rejected.clone())
        .err()
        .unwrap();
    assert_eq!(error.kind, ErrorKind::Busy);
    assert_eq!(rejected.opens.load(Ordering::Acquire), 0);
    assert_eq!(rejected.sends.load(Ordering::Acquire), 0);
    assert!(!error.message.contains("private-usb-serial-123"));
    drop(first);
    let second = open(&root, &["private-usb-serial-123"], rejected.clone()).unwrap();
    assert_eq!(rejected.opens.load(Ordering::Acquire), 1);
    drop(second);
}

#[test]
fn overlapping_and_reversed_routes_contend_without_deadlock_or_partial_locks() {
    let temp = Temp::new();
    let root = temp.root();
    let held =
        DeviceSessionGuard::acquire(&root, &[endpoint("route-a"), endpoint("route-b")]).unwrap();
    for names in [["route-b", "route-a"], ["route-c", "route-b"]] {
        let state = State::new();
        assert_eq!(
            open(&root, &names, state.clone()).err().unwrap().kind,
            ErrorKind::Busy
        );
        assert_eq!(state.opens.load(Ordering::Acquire), 0);
        drop(DeviceSessionGuard::acquire(&root, &[endpoint("route-c")]).unwrap());
    }
    // A different physical endpoint may be held concurrently.
    let other = DeviceSessionGuard::acquire(&root, &[endpoint("route-other")]).unwrap();
    drop(other);
    drop(held);
    drop(DeviceSessionGuard::acquire(&root, &[endpoint("route-b"), endpoint("route-a")]).unwrap());
}

#[test]
fn failed_open_releases_ownership_before_reconnect() {
    let temp = Temp::new();
    let root = temp.root();
    let error = open_guarded_at(&root, &[endpoint("failed")], || {
        Err(ServiceError::new(
            ErrorKind::Disconnected,
            "startup failure",
        ))
    })
    .err()
    .unwrap();
    assert_eq!(error.kind, ErrorKind::Disconnected);
    drop(open(&root, &["failed"], State::new()).unwrap());
}

#[test]
fn session_guard_outlives_blocked_reader_and_handle_destructors() {
    struct ClosingSession {
        entered: crossbeam_channel::Sender<()>,
        finish: crossbeam_channel::Receiver<()>,
        stopped: Arc<AtomicBool>,
    }
    impl MidiSession for ClosingSession {
        fn send(&mut self, _: &[u8]) -> Result<(), ServiceError> {
            Ok(())
        }
    }
    impl Drop for ClosingSession {
        fn drop(&mut self) {
            self.entered.send(()).unwrap();
            self.finish.recv_timeout(DEADLINE).unwrap();
            self.stopped.store(true, Ordering::Release);
        }
    }
    let temp = Temp::new();
    let root = temp.root();
    let (entered_tx, entered_rx) = crossbeam_channel::bounded(1);
    let (finish_tx, finish_rx) = crossbeam_channel::bounded(1);
    let stopped = Arc::new(AtomicBool::new(false));
    let session = open_guarded_at(&root, &[endpoint("closing")], || {
        Ok(Box::new(ClosingSession {
            entered: entered_tx,
            finish: finish_rx,
            stopped: stopped.clone(),
        }))
    })
    .unwrap();
    let closer = thread::spawn(move || drop(session));
    entered_rx.recv_timeout(DEADLINE).unwrap();
    assert!(!stopped.load(Ordering::Acquire));
    assert_busy(&root, "closing");
    finish_tx.send(()).unwrap();
    closer.join().unwrap();
    assert!(stopped.load(Ordering::Acquire));
    drop(DeviceSessionGuard::acquire(&root, &[endpoint("closing")]).unwrap());
}

#[test]
fn keys_are_backend_observations_with_distinct_location_and_subdevice() {
    let base = endpoint("same-name");
    let mut other_location = endpoint("same-name");
    other_location.location = "other-physical-location".into();
    let mut other_subdevice = endpoint("same-name");
    other_subdevice.subdevice = 1;
    assert_ne!(base.key(), other_location.key());
    assert_ne!(base.key(), other_subdevice.key());
    assert_eq!(base.key().len(), 69);
    assert!(!base.key().contains("same-name"));
}

#[test]
fn duplex_input_and_output_are_deduplicated_to_one_private_lock() {
    let temp = Temp::new();
    let root = temp.root();
    let held =
        DeviceSessionGuard::acquire(&root, &[endpoint("duplex"), endpoint("duplex")]).unwrap();
    assert_eq!(held._locks.len(), 1);
    assert_eq!(std::fs::read_dir(&temp.0).unwrap().count(), 1);
    let metadata = std::fs::metadata(temp.0.join(endpoint("duplex").key())).unwrap();
    assert_eq!(metadata.permissions().mode() & 0o7777, 0o600);
    assert_busy(&root, "duplex");
    drop(held);
}

#[test]
fn runtime_rejects_symlink_leaf_ancestor_relative_and_unprotected_permissions() {
    let temp = Temp::new();
    let uid = rustix::process::geteuid().as_raw();
    let real = temp.0.join("real");
    std::fs::create_dir(&real).unwrap();
    std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o700)).unwrap();
    symlink(&real, temp.0.join("alias")).unwrap();
    assert!(GuardRoot::open_runtime(&temp.0.join("alias"), uid, false).is_err());
    assert!(GuardRoot::open_runtime(&temp.0.join("alias/child"), uid, true).is_err());
    assert!(GuardRoot::open_runtime(Path::new("relative"), uid, true).is_err());
    assert!(GuardRoot::open_runtime(&temp.0, uid + 1, false).is_err());
    std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(GuardRoot::open_runtime(&real, uid, false).is_err());
    std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o777)).unwrap();
    assert!(GuardRoot::open_runtime(&real.join("child"), uid, true).is_err());
    assert!(!real.join("child").exists());
}

#[test]
fn app_directory_symlink_and_lock_symlink_fifo_hardlink_are_rejected() {
    let temp = Temp::new();
    let root = temp.root();
    let uid = rustix::process::geteuid().as_raw();
    let real = temp.0.join("other");
    std::fs::create_dir(&real).unwrap();
    symlink(&real, temp.0.join("gp200-studio")).unwrap();
    assert!(private_child(&root.directory, "gp200-studio", uid).is_err());
    let lock_path = temp.0.join(endpoint("unsafe-lock").key());
    let target = temp.0.join("target");
    std::fs::write(&target, b"preserved").unwrap();
    symlink(&target, &lock_path).unwrap();
    assert!(DeviceSessionGuard::acquire(&root, &[endpoint("unsafe-lock")]).is_err());
    assert_eq!(std::fs::read(&target).unwrap(), b"preserved");
    std::fs::remove_file(&lock_path).unwrap();
    fs::mknodat(
        &root.directory,
        endpoint("unsafe-lock").key().as_str(),
        FileType::Fifo,
        Mode::RUSR | Mode::WUSR,
        0,
    )
    .unwrap();
    assert!(DeviceSessionGuard::acquire(&root, &[endpoint("unsafe-lock")]).is_err());
    std::fs::remove_file(&lock_path).unwrap();
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).unwrap();
    std::fs::hard_link(&target, &lock_path).unwrap();
    assert!(DeviceSessionGuard::acquire(&root, &[endpoint("unsafe-lock")]).is_err());
    std::fs::remove_file(&lock_path).unwrap();
    std::fs::write(&lock_path, b"").unwrap();
    std::fs::set_permissions(&lock_path, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(DeviceSessionGuard::acquire(&root, &[endpoint("unsafe-lock")]).is_err());
}

struct FakeBackend {
    root: Arc<GuardRoot>,
    state: Arc<State>,
}
impl TransportBackend for FakeBackend {
    fn enumerate(&mut self) -> Result<Vec<PortPairCandidate>, ServiceError> {
        Ok(vec![])
    }
    fn selection_present(&mut self, _: &DeviceSelection) -> Result<bool, ServiceError> {
        Ok(true)
    }
    fn connect(
        &mut self,
        _: &DeviceSelection,
        _: ReceiveSink,
    ) -> Result<Box<dyn MidiSession>, ServiceError> {
        open(&self.root, &["service-endpoint"], self.state.clone())
    }
}
fn service(root: Arc<GuardRoot>, state: Arc<State>) -> MidiDeviceService {
    MidiDeviceService::with_backend(
        Box::new(FakeBackend { root, state }),
        TransportConfig::alsa_raw(),
        no_wake(),
    )
}
fn connect(service: &MidiDeviceService, intent: u64) {
    service
        .try_submit(DeviceCommand::Connect {
            intent: IntentId(intent),
            selection: DeviceSelection {
                input_id: "test-input".into(),
                output_id: "test-output".into(),
                fingerprint_hint: None,
            },
        })
        .unwrap();
}
fn wait_until(mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + DEADLINE;
    while !condition() {
        assert!(Instant::now() < deadline, "test lifecycle deadline");
        thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn production_service_disconnect_unplug_and_shutdown_release_for_reconnect() {
    let temp = Temp::new();
    let root = Arc::new(temp.root());
    for termination in ["disconnect", "unplug", "shutdown"] {
        let state = State::new();
        let service = service(root.clone(), state.clone());
        connect(&service, 1);
        wait_until(|| state.opens.load(Ordering::Acquire) == 1);
        assert_busy(&root, "service-endpoint");
        match termination {
            "disconnect" => service
                .try_submit(DeviceCommand::Disconnect {
                    intent: IntentId(2),
                })
                .unwrap(),
            "unplug" => state.fault.store(true, Ordering::Release),
            "shutdown" => service.shutdown(),
            _ => unreachable!(),
        }
        wait_until(|| state.drops.load(Ordering::Acquire) == 1);
        if termination != "shutdown" {
            // Terminal state follows TransportAction::Close. The worker stays
            // alive: release is due to disconnect/loss, not process shutdown.
            let expected = if termination == "disconnect" {
                ConnectionState::Disconnected
            } else {
                ConnectionState::Recovering
            };
            let mut closed = false;
            wait_until(|| {
                while let Some(event) = service.try_next_event() {
                    if matches!(event, DeviceEvent::ConnectionChanged { state, .. } if state == expected)
                    {
                        closed = true;
                    }
                }
                closed
            });
            assert!(!service.stopped());
            drop(DeviceSessionGuard::acquire(&root, &[endpoint("service-endpoint")]).unwrap());
            state.fault.store(false, Ordering::Release);
            connect(&service, 3);
            wait_until(|| state.opens.load(Ordering::Acquire) == 2);
            assert_busy(&root, "service-endpoint");
        }
        service.shutdown();
        assert!(service.wait_stopped(DEADLINE));
        drop(DeviceSessionGuard::acquire(&root, &[endpoint("service-endpoint")]).unwrap());
    }
}

#[test]
fn production_service_contention_is_busy_before_any_endpoint_or_byte() {
    let temp = Temp::new();
    let root = Arc::new(temp.root());
    let owner = DeviceSessionGuard::acquire(&root, &[endpoint("service-endpoint")]).unwrap();
    let state = State::new();
    let service = service(root, state.clone());
    connect(&service, 1);
    let mut error = None;
    wait_until(|| {
        while let Some(event) = service.try_next_event() {
            if let DeviceEvent::OperationFailed { error: e, .. } = event {
                error = Some(e);
            }
        }
        error.is_some()
    });
    assert_eq!(error.unwrap().kind, ErrorKind::Busy);
    assert_eq!(state.opens.load(Ordering::Acquire), 0);
    assert_eq!(state.sends.load(Ordering::Acquire), 0);
    service.shutdown();
    assert!(service.wait_stopped(DEADLINE));
    drop(owner);
}

struct TestChild(Child);
impl Drop for TestChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn child(temp: &Temp, cwd: &Path, data: &Path, prefix: &str, hold: bool, names: &str) -> TestChild {
    let child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "device_session_guard::tests::process_guard_fixture",
            "--ignored",
            "--nocapture",
        ])
        .current_dir(cwd)
        .env("XDG_DATA_HOME", data)
        .env("GP200_GUARD_TEST_ROOT", &temp.0)
        .env("GP200_GUARD_TEST_PREFIX", prefix)
        .env("GP200_GUARD_TEST_NAMES", names)
        .env("GP200_GUARD_TEST_HOLD", if hold { "1" } else { "0" })
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    TestChild(child)
}

#[test]
fn processes_with_different_cwd_and_data_roots_contend_then_release_on_exit() {
    let temp = Temp::new();
    let cwd_a = Temp::new();
    let cwd_b = Temp::new();
    let data_a = Temp::new();
    let data_b = Temp::new();
    let mut owner = child(&temp, &cwd_a.0, &data_a.0, "owner", true, "a,b");
    wait_until(|| temp.0.join("owner-result").exists());
    assert_eq!(
        std::fs::read_to_string(temp.0.join("owner-result")).unwrap(),
        "Ready\n1"
    );
    let mut rejected = child(&temp, &cwd_b.0, &data_b.0, "rejected", false, "c,b");
    wait_until(|| rejected.0.try_wait().unwrap().is_some());
    let report = std::fs::read_to_string(temp.0.join("rejected-result")).unwrap();
    assert!(report.starts_with("Busy\n0\n"), "{report}");
    assert!(!report.contains("fixture-location"));
    owner.0.stdin.take().unwrap().write_all(b"x").unwrap();
    wait_until(|| owner.0.try_wait().unwrap().is_some());
    let mut reopened = child(&temp, &cwd_b.0, &data_b.0, "reopened", false, "b,c");
    wait_until(|| reopened.0.try_wait().unwrap().is_some());
    assert_eq!(
        std::fs::read_to_string(temp.0.join("reopened-result")).unwrap(),
        "Ready\n1"
    );
}

#[test]
fn process_termination_releases_guard_without_deleting_stable_lock_files() {
    let temp = Temp::new();
    let cwd = Temp::new();
    let data = Temp::new();
    let mut owner = child(
        &temp,
        &cwd.0,
        &data.0,
        "crash-owner",
        true,
        "crash-endpoint",
    );
    wait_until(|| temp.0.join("crash-owner-result").exists());
    assert_busy(&temp.root(), "crash-endpoint");
    owner.0.kill().unwrap();
    owner.0.wait().unwrap();
    assert!(temp.0.join(endpoint("crash-endpoint").key()).is_file());
    let mut reopened = child(
        &temp,
        &cwd.0,
        &data.0,
        "after-crash",
        false,
        "crash-endpoint",
    );
    wait_until(|| reopened.0.try_wait().unwrap().is_some());
    assert_eq!(
        std::fs::read_to_string(temp.0.join("after-crash-result")).unwrap(),
        "Ready\n1"
    );
}

#[test]
#[ignore = "subprocess fixture; parent process test supplies private fixture directory"]
fn process_guard_fixture() {
    use std::io::Read;
    let root_path =
        PathBuf::from(std::env::var_os("GP200_GUARD_TEST_ROOT").expect("test-only root"));
    let prefix = std::env::var("GP200_GUARD_TEST_PREFIX").unwrap();
    assert!(matches!(
        prefix.as_str(),
        "owner" | "rejected" | "reopened" | "crash-owner" | "after-crash"
    ));
    let root =
        GuardRoot::open_runtime(&root_path, rustix::process::geteuid().as_raw(), false).unwrap();
    let names = std::env::var("GP200_GUARD_TEST_NAMES").unwrap();
    let names = names.split(',').collect::<Vec<_>>();
    let state = State::new();
    let opened = open(&root, &names, state.clone());
    let report = match &opened {
        Ok(_) => format!("Ready\n{}", state.opens.load(Ordering::Acquire)),
        Err(error) => format!(
            "{:?}\n{}\n{}",
            error.kind,
            state.opens.load(Ordering::Acquire),
            error.message
        ),
    };
    let staged = root_path.join(format!("{prefix}-stage"));
    std::fs::write(&staged, report).unwrap();
    std::fs::rename(staged, root_path.join(format!("{prefix}-result"))).unwrap();
    if std::env::var("GP200_GUARD_TEST_HOLD").unwrap() == "1" {
        std::io::stdin().read_exact(&mut [0_u8; 1]).unwrap();
    }
    drop(opened);
}

#[test]
fn fresh_same_alias_consumes_existing_lock_and_new_alias_is_locked_without_releasing_old() {
    let temp = Temp::new();
    let root = temp.root();
    let mut held = DeviceSessionGuard::acquire(&root, &[endpoint("old")]).unwrap();
    assert!(held.retain_aliases(&[]).is_err());
    held.retain_aliases(&[endpoint("old")]).unwrap();
    assert_eq!(held._locks.len(), 1);
    assert_busy(&root, "old");
    held.retain_aliases(&[endpoint("new")]).unwrap();
    assert_eq!(held._locks.len(), 2);
    assert_busy(&root, "old");
    assert_busy(&root, "new");
    let competitor = DeviceSessionGuard::acquire(&root, &[endpoint("third")]).unwrap();
    assert!(held.retain_aliases(&[endpoint("third")]).is_err());
    assert_busy(&root, "old");
    assert_busy(&root, "new");
    drop(competitor);
    drop(held);
    assert!(DeviceSessionGuard::acquire(&root, &[endpoint("old"), endpoint("new")]).is_ok());
}

#[test]
fn normal_entry_witness_requires_actual_physical_and_exact_original_alias_locks() {
    let t = Temp::new();
    let physical = Temp::new();
    let binding = gp200_domain::FirmwareUsbObservation {
        topology: "fixture physical topology".into(),
        vendor: 1,
        product: 2,
        serial: "fixture serial".into(),
        interface: "fixture-location-entry".into(),
        endpoint: "hw:entry,0,0".into(),
    };
    let registry = crate::firmware_quarantine::QuarantineRegistry::mock(&physical.0).unwrap();
    let alias_only = DeviceSessionGuard::fixture(&t.0, &[endpoint("entry")]).unwrap();
    assert!(!alias_only.owns_normal_binding(&binding));
    drop(alias_only);
    let guard =
        DeviceSessionGuard::fixture_physical(&t.0, &[endpoint("entry")], &binding, &registry)
            .unwrap();
    assert!(guard.owns_normal_binding(&binding));
    let mut other_physical = binding.clone();
    other_physical.topology = "a different unit".into();
    assert!(!guard.owns_normal_binding(&other_physical));
    for endpoint in [
        "hw:other,0,0",
        "hw:entry,1,0",
        "hw:entry,0,1",
        "hw:entry,0,0,0",
        "saved-name",
    ] {
        let mut changed = binding.clone();
        changed.endpoint = endpoint.into();
        assert!(!guard.owns_normal_binding(&changed));
    }
    let mut changed = binding.clone();
    changed.interface = "another interface".into();
    assert!(!guard.owns_normal_binding(&changed));
    assert!(DeviceSessionGuard::fixture(&t.0, &[endpoint("entry")]).is_err());
    assert!(registry.physical_lease(&binding).is_err());
    drop(guard);
    assert!(DeviceSessionGuard::fixture(&t.0, &[endpoint("entry")]).is_ok());
    assert!(registry.physical_lease(&binding).is_ok());
}
