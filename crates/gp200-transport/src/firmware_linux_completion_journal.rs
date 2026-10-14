//! Durable local pairing under the actual moved store. Reopening never resumes I/O.
use super::firmware_linux_completion_admission::{bad, private_directory};
use crossbeam_channel::{Receiver, Sender, bounded};
use gp200_domain::*;
use gp200_library::LibraryStore;
use sha2::{Digest, Sha256};
use std::{
    fs::{File, OpenOptions},
    io::Write,
    os::unix::fs::{FileExt, MetadataExt, OpenOptionsExt},
    path::PathBuf,
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};
const NOFOLLOW: i32 = 0x20_000;
const MAX_RECORD: usize = 512 * 1024;
const MAX_BEGIN_RECORD: usize = 32 * 1024 * 1024;
// One write, one first drain, the existing 64-chunk raw queue, and one pair.
pub(crate) const MAX_BATCH_RECORDS: usize = 67;
pub(crate) const MAX_BATCH_BYTES: usize = 1024 * 1024;

/// In-memory measurements for explicitly selected disk-backed mock owners.
/// Exact fixture roots isolate concurrent tests; this module is absent from
/// every non-test build and cannot select a physical USB or ALSA opener.
#[cfg(test)]
pub(crate) mod test_profile {
    use std::{
        collections::BTreeMap,
        path::{Path, PathBuf},
        sync::{Arc, Mutex, Weak},
        time::Instant,
    };

    #[derive(Default)]
    struct Statistic {
        count: u64,
        total_ns: u64,
        min_ns: u64,
        max_ns: u64,
    }
    impl Statistic {
        fn record(&mut self, ns: u64) {
            self.min_ns = if self.count == 0 {
                ns
            } else {
                self.min_ns.min(ns)
            };
            self.max_ns = self.max_ns.max(ns);
            self.count += 1;
            self.total_ns = self
                .total_ns
                .checked_add(ns)
                .expect("bounded profile duration");
        }
        fn json(&self) -> serde_json::Value {
            serde_json::json!({
                "count": self.count,
                "total_ns": self.total_ns,
                "min_ns": self.min_ns,
                "max_ns": self.max_ns,
                "mean_ns": self.total_ns as f64 / self.count as f64,
            })
        }
    }
    #[derive(Default)]
    pub(crate) struct Measurements {
        phases: BTreeMap<&'static str, Statistic>,
        successful_append_kinds: BTreeMap<String, u64>,
        successful_commits: u64,
    }
    struct Registration {
        store: PathBuf,
        usb: PathBuf,
        measurements: Weak<Mutex<Measurements>>,
    }
    static REGISTERED: Mutex<Vec<Registration>> = Mutex::new(Vec::new());

    pub(crate) struct Profile {
        measurements: Arc<Mutex<Measurements>>,
    }
    impl Profile {
        pub(crate) fn new() -> Self {
            Self {
                measurements: Arc::new(Mutex::new(Measurements::default())),
            }
        }
        pub(crate) fn register_roots(&self, store: &Path, usb: &Path) {
            let mut registrations = REGISTERED.lock().unwrap();
            registrations.retain(|entry| entry.measurements.strong_count() != 0);
            assert!(
                registrations
                    .iter()
                    .all(|entry| entry.store != store && entry.usb != usb)
            );
            assert!(!registrations.iter().any(|entry| {
                entry
                    .measurements
                    .ptr_eq(&Arc::downgrade(&self.measurements))
            }));
            registrations.push(Registration {
                store: store.into(),
                usb: usb.into(),
                measurements: Arc::downgrade(&self.measurements),
            });
        }
        /// Read only after the original owner and journal worker have joined.
        pub(crate) fn snapshot(&self) -> serde_json::Value {
            let measurements = self.measurements.lock().unwrap();
            let phases = measurements
                .phases
                .iter()
                .map(|(name, value)| ((*name).to_owned(), value.json()))
                .collect::<BTreeMap<_, _>>();
            serde_json::json!({
                "schema": 1,
                "physical_hardware": false,
                "collection": "in-memory, exact private fixture Store and USB roots; read after original owner join",
                "binding_scope": "private fake sysfs filesystem and supplied FixturePlatform endpoints; no ALSA discovery",
                "durations_overlap": true,
                "duration_interpretation": "append_total contains append stages; wait_event_receive overlaps worker append; binding_revalidate_total contains binding_fixture_enumeration",
                "phases": phases,
                "successful_append_kinds": measurements.successful_append_kinds,
                "successful_commits": measurements.successful_commits,
            })
        }
    }
    impl Drop for Profile {
        fn drop(&mut self) {
            REGISTERED.lock().unwrap().retain(|entry| {
                !entry
                    .measurements
                    .ptr_eq(&Arc::downgrade(&self.measurements))
            });
        }
    }
    fn lookup(path: &Path, store: bool) -> Option<Arc<Mutex<Measurements>>> {
        REGISTERED.lock().unwrap().iter().find_map(|entry| {
            ((if store { &entry.store } else { &entry.usb }) == path)
                .then(|| entry.measurements.upgrade())
                .flatten()
        })
    }
    pub(crate) fn for_store(path: &Path) -> Option<Arc<Mutex<Measurements>>> {
        lookup(path, true)
    }
    pub(crate) fn for_fixture_usb(path: &Path) -> Option<Arc<Mutex<Measurements>>> {
        lookup(path, false)
    }
    pub(crate) fn appended(measurements: &Option<Arc<Mutex<Measurements>>>, kind: &str) {
        if let Some(measurements) = measurements {
            *measurements
                .lock()
                .unwrap()
                .successful_append_kinds
                .entry(kind.into())
                .or_default() += 1;
        }
    }
    pub(crate) fn committed(measurements: &Option<Arc<Mutex<Measurements>>>) {
        if let Some(measurements) = measurements {
            measurements.lock().unwrap().successful_commits += 1;
        }
    }
    pub(crate) struct Timer {
        started: Option<(Instant, Arc<Mutex<Measurements>>)>,
        phase: &'static str,
    }
    impl Timer {
        pub(crate) fn new(
            measurements: Option<Arc<Mutex<Measurements>>>,
            phase: &'static str,
        ) -> Self {
            Self {
                started: measurements.map(|measurements| (Instant::now(), measurements)),
                phase,
            }
        }
        pub(crate) fn finish(self) {}
    }
    impl Drop for Timer {
        fn drop(&mut self) {
            if let Some((started, measurements)) = self.started.take() {
                let ns =
                    u64::try_from(started.elapsed().as_nanos()).expect("bounded profile duration");
                measurements
                    .lock()
                    .unwrap()
                    .phases
                    .entry(self.phase)
                    .or_default()
                    .record(ns);
            }
        }
    }

    #[test]
    fn profiles_isolate_exact_fixture_roots_and_accumulated_samples() {
        let first = Profile::new();
        let second = Profile::new();
        first.register_roots(
            Path::new("profile-first-store"),
            Path::new("profile-first-usb"),
        );
        second.register_roots(
            Path::new("profile-second-store"),
            Path::new("profile-second-usb"),
        );
        let first_store = for_store(Path::new("profile-first-store")).unwrap();
        let first_usb = for_fixture_usb(Path::new("profile-first-usb")).unwrap();
        assert!(Arc::ptr_eq(&first_store, &first_usb));
        assert!(!Arc::ptr_eq(
            &first_store,
            &for_store(Path::new("profile-second-store")).unwrap()
        ));
        assert!(for_store(Path::new("unregistered-store")).is_none());
        assert!(for_fixture_usb(Path::new("profile-first-store")).is_none());
        Timer::new(Some(first_store), "selected-test").finish();
        assert_eq!(first.snapshot()["phases"]["selected-test"]["count"], 1);
        assert!(second.snapshot()["phases"].as_object().unwrap().is_empty());
        drop(first);
        assert!(for_fixture_usb(Path::new("profile-first-usb")).is_none());
        assert!(for_store(Path::new("profile-second-store")).is_some());
    }
}
#[derive(Default)]
struct Gate {
    stopped: AtomicBool,
    delivery: Mutex<()>,
    pending: AtomicU64,
    offer: AtomicU64,
}
impl Gate {
    fn lock(&self) -> MutexGuard<'_, ()> {
        self.delivery.lock().unwrap_or_else(|p| {
            self.stopped.store(true, Ordering::SeqCst);
            p.into_inner()
        })
    }
}
pub(crate) struct JournalReceipt {
    pub(crate) run: String,
    pub(crate) sequence: u64,
    pub(crate) prefix_sha256: String,
    pub(crate) path: PathBuf,
    pub(crate) bytes: u64,
}
impl JournalReceipt {
    pub(crate) fn revalidate(&self) -> Result<(), ServiceError> {
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(NOFOLLOW)
            .open(&self.path)
            .map_err(bad)?;
        let before = file.metadata().map_err(bad)?;
        let named = std::fs::symlink_metadata(&self.path).map_err(bad)?;
        if !before.is_file()
            || before.len() != self.bytes
            || before.nlink() != 1
            || before.mode() & 0o777 != 0o600
            || before.dev() != named.dev()
            || before.ino() != named.ino()
            || before.uid() != rustix::process::geteuid().as_raw()
        {
            return Err(bad("Live journal receipt file changed"));
        }
        let mut hash = Sha256::new();
        let mut offset = 0;
        let mut buffer = [0u8; 65536];
        while offset < self.bytes {
            let size = ((self.bytes - offset) as usize).min(buffer.len());
            let count = file.read_at(&mut buffer[..size], offset).map_err(bad)?;
            if count == 0 {
                return Err(bad("Live journal receipt truncated"));
            }
            hash.update(&buffer[..count]);
            offset += count as u64;
        }
        let after = file.metadata().map_err(bad)?;
        let named_after = std::fs::symlink_metadata(&self.path).map_err(bad)?;
        if after.len() != before.len()
            || after.mtime() != before.mtime()
            || after.mtime_nsec() != before.mtime_nsec()
            || after.ctime() != before.ctime()
            || after.ctime_nsec() != before.ctime_nsec()
            || named_after.dev() != before.dev()
            || named_after.ino() != before.ino()
            || format!("{:x}", hash.finalize()) != self.prefix_sha256
        {
            return Err(bad("Live journal receipt prefix changed"));
        }
        Ok(())
    }
}
pub(crate) struct CompletionCheckpoint {
    run: String,
    sequence: u64,
    prefix_sha256: String,
    intent: u64,
    ordinal: u64,
    wire: Vec<u8>,
    gate: Arc<Gate>,
    // Present only when this same transaction also durably acknowledged an outcome.
    pub(crate) outcome_receipt: Option<JournalReceipt>,
}
impl CompletionCheckpoint {
    pub(crate) fn matches_original(&self, ordinal: u64, wire: &[u8]) -> bool {
        self.ordinal == ordinal && self.wire == wire
    }
    #[cfg(test)]
    pub(crate) fn test_consume_offer(&mut self) {
        self.gate.offer.store(0, Ordering::SeqCst);
    }
    #[cfg(test)]
    pub(crate) fn sequence(&self) -> u64 {
        self.sequence
    }
    #[cfg(test)]
    pub(crate) fn prefix(&self) -> &str {
        &self.prefix_sha256
    }
    #[cfg(test)]
    pub(crate) fn ordinal(&self) -> u64 {
        self.ordinal
    }
    #[cfg(test)]
    pub(crate) fn wire(&self) -> &[u8] {
        &self.wire
    }
}
pub(crate) enum JournalReply {
    Checkpoint(CompletionCheckpoint),
    Recorded(JournalReceipt),
}
pub(crate) struct JournalEvent {
    pub(crate) intent: u64,
    pub(crate) result: Result<JournalReply, ServiceError>,
}
enum CommandKind {
    Checkpoint {
        ordinal: u64,
        purpose: String,
        wire: Vec<u8>,
    },
    OutcomeCheckpoint {
        values: Vec<serde_json::Value>,
        ordinal: u64,
        purpose: String,
        wire: Vec<u8>,
    },
    Record(serde_json::Value),
    Records(Vec<serde_json::Value>),
}
struct Command {
    intent: u64,
    kind: CommandKind,
}
#[cfg(test)]
#[derive(Debug, Clone, Copy)]
pub(crate) enum TestBatchFault {
    WriteThenError(usize),
    ShortWrite(usize),
    Sync,
    Reread,
    WrongIntent,
    Revoked,
    LostReply,
}
#[cfg(test)]
struct TestStopReply {
    checkpoint: bool,
    stop: Arc<crate::firmware_service::OwnerStop>,
    stall: Option<Duration>,
}
#[cfg(test)]
type TestBatchSync = Arc<Mutex<Option<Box<dyn FnOnce() + Send>>>>;
pub(crate) struct CompletionJournal {
    run: String,
    path: PathBuf,
    gate: Arc<Gate>,
    commands: Option<Sender<Command>>,
    events: Receiver<JournalEvent>,
    join: Option<JoinHandle<LibraryStore>>,
    next_intent: u64,
    #[cfg(test)]
    test_profile: Option<Arc<Mutex<test_profile::Measurements>>>,
    #[cfg(test)]
    test_stop_reply: Arc<Mutex<Option<TestStopReply>>>,
    #[cfg(test)]
    test_cancelled_reply_release: Arc<AtomicBool>,
    #[cfg(test)]
    test_batch_fault: Arc<Mutex<Option<TestBatchFault>>>,
    #[cfg(test)]
    test_batch_sync: TestBatchSync,
}
pub(crate) struct JournalStartFailure {
    pub(crate) error: ServiceError,
    pub(crate) store: LibraryStore,
}
pub(crate) struct OfferFence<'a> {
    _guard: MutexGuard<'a, ()>,
}
impl CompletionJournal {
    pub(crate) fn start(
        store: LibraryStore,
        run: String,
        metadata: serde_json::Value,
    ) -> Result<Self, Box<JournalStartFailure>> {
        let setup = (|| {
            if run.len() != 32
                || !run
                    .bytes()
                    .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
            {
                return Err(bad("Completion run identifier invalid"));
            }
            let directory = store.root().join("linux-firmware-completion");
            match std::fs::create_dir(&directory) {
                Ok(()) => {
                    use std::os::unix::fs::PermissionsExt;
                    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))
                        .map_err(bad)?;
                    File::open(store.root())
                        .and_then(|f| f.sync_all())
                        .map_err(bad)?;
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(bad(e)),
            }
            private_directory(&directory)?;
            let path = directory.join(format!("{run}.journal"));
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(NOFOLLOW)
                .open(&path)
                .map_err(bad)?;
            File::open(&directory)
                .and_then(|f| f.sync_all())
                .map_err(bad)?;
            let mut writer = Writer {
                file,
                path: path.clone(),
                run: run.clone(),
                sequence: 0,
                bytes: 0,
                hash: Sha256::new(),
                stamp: None,
                poisoned: false,
                #[cfg(test)]
                test_fault: None,
                #[cfg(test)]
                test_before_sync: None,
                #[cfg(test)]
                test_profile: test_profile::for_store(store.root()),
            };
            writer.append(serde_json::json!({"kind":"begin","schema":1,"run":run,"metadata":metadata,"resume_supported":false,"physical_authority":false}))?;
            Ok(writer)
        })();
        let writer = match setup {
            Ok(w) => w,
            Err(error) => return Err(Box::new(JournalStartFailure { error, store })),
        };
        let path = writer.path.clone();
        #[cfg(test)]
        let profile = writer.test_profile.clone();
        let gate = Arc::new(Gate::default());
        let thread_gate = gate.clone();
        let (tx, rx) = bounded::<Command>(4);
        let (etx, erx) = bounded(4);
        // The exact owner remains recoverable if OS thread creation fails.
        let retained = Arc::new(Mutex::new(Some(store)));
        let thread_store = retained.clone();
        let thread_run = run.clone();
        #[cfg(test)]
        let test_stop_reply = Arc::new(Mutex::new(None::<TestStopReply>));
        #[cfg(test)]
        let thread_test_stop_reply = test_stop_reply.clone();
        #[cfg(test)]
        let test_cancelled_reply_release = Arc::new(AtomicBool::new(false));
        #[cfg(test)]
        let thread_test_cancelled_reply_release = test_cancelled_reply_release.clone();
        #[cfg(test)]
        let test_batch_fault = Arc::new(Mutex::new(None));
        #[cfg(test)]
        let thread_test_batch_fault = test_batch_fault.clone();
        #[cfg(test)]
        let test_batch_sync = Arc::new(Mutex::new(None));
        #[cfg(test)]
        let thread_test_batch_sync = test_batch_sync.clone();
        let join=thread::Builder::new().name("gp200-linux-completion-journal".into()).spawn(move||{
            let store=thread_store.lock().expect("startup handoff lock").take().expect("one original store");
            let mut writer=writer;
            while let Ok(command)=rx.recv(){
                #[cfg(test)]
                let was_checkpoint=matches!(&command.kind,CommandKind::Checkpoint{..}|CommandKind::OutcomeCheckpoint{..});
                let result=(||{
                    let combined=matches!(&command.kind,CommandKind::OutcomeCheckpoint{..});
                    let checkpoint=match &command.kind{CommandKind::Checkpoint{ordinal,purpose,wire}|CommandKind::OutcomeCheckpoint{ordinal,purpose,wire,..}=>Some((*ordinal,purpose.clone(),wire.clone())),CommandKind::Record(_)|CommandKind::Records(_)=>None};
                    let receipt=match command.kind{
                        CommandKind::Checkpoint{ordinal,purpose,wire}=>writer.append(serde_json::json!({"kind":"checkpoint","ordinal":ordinal,"purpose":purpose,"wire_sha256":JobSha256::of(&wire).hex(),"wire":wire}))?,
                        CommandKind::OutcomeCheckpoint{values,ordinal,purpose,wire}=>{
                            #[cfg(test)]
                            {
                                writer.test_fault=thread_test_batch_fault.lock().unwrap().take();
                                writer.test_before_sync=thread_test_batch_sync.lock().unwrap().take();
                            }
                            writer.append_outcome_checkpoint(values,ordinal,purpose,wire)?
                        }
                        CommandKind::Record(value)=>writer.append(value)?,
                        CommandKind::Records(values)=>{
                            #[cfg(test)]
                            {
                                writer.test_fault=thread_test_batch_fault.lock().unwrap().take();
                                writer.test_before_sync=thread_test_batch_sync.lock().unwrap().take();
                            }
                            writer.append_many(values)?
                        }
                    };
                    if let Some((ordinal,_,wire))=checkpoint{
                        let _lock=thread_gate.lock();if thread_gate.stopped.load(Ordering::SeqCst){return Err(bad("Stopped local checkpoint cannot offer"));}
                        thread_gate.offer.store(command.intent,Ordering::SeqCst);
                        Ok(JournalReply::Checkpoint(CompletionCheckpoint{run:thread_run.clone(),sequence:receipt.sequence,prefix_sha256:receipt.prefix_sha256.clone(),intent:command.intent,ordinal,wire,gate:thread_gate.clone(),outcome_receipt:combined.then_some(receipt)}))
                    }else{Ok(JournalReply::Recorded(receipt))}
                })();
                if result.is_err(){thread_gate.stopped.store(true,Ordering::SeqCst);}
                #[cfg(test)]
                {
                    let mut hook=thread_test_stop_reply.lock().unwrap();
                    if hook.as_ref().is_some_and(|hook|hook.checkpoint==was_checkpoint){
                        let TestStopReply{stop,stall,..}=hook.take().unwrap();
                        stop.requested.store(true,Ordering::SeqCst);
                        drop(hook);
                        // Keep the exact original durable intent pending after Stop,
                        // until the Executor actually cancels that original wait.
                        if let Some(stall)=stall {
                            thread::sleep(stall);
                        } else {
                            let deadline=std::time::Instant::now()+std::time::Duration::from_secs(5);
                            while !thread_test_cancelled_reply_release.load(Ordering::SeqCst) && std::time::Instant::now()<deadline {
                                thread::sleep(std::time::Duration::from_millis(1));
                            }
                        }
                    }
                }
                #[cfg(test)]
                if matches!(writer.test_fault,Some(TestBatchFault::LostReply)) { continue; }
                #[cfg(test)]
                let _publication=test_profile::Timer::new(writer.test_profile.clone(),"worker_reply_publication");
                #[cfg(test)]
                if matches!(writer.test_fault,Some(TestBatchFault::Revoked)) {
                    thread_gate.stopped.store(true,Ordering::SeqCst);
                }
                #[cfg(test)]
                let intent=if matches!(writer.test_fault,Some(TestBatchFault::WrongIntent)) {command.intent+1}else{command.intent};
                #[cfg(not(test))]
                let intent=command.intent;
                if etx.send(JournalEvent{intent,result}).is_err(){break;}
            }
            store
        });
        match join {
            Ok(join) => Ok(Self {
                run,
                path,
                gate,
                commands: Some(tx),
                events: erx,
                join: Some(join),
                next_intent: 1,
                #[cfg(test)]
                test_profile: profile,
                #[cfg(test)]
                test_stop_reply,
                #[cfg(test)]
                test_cancelled_reply_release,
                #[cfg(test)]
                test_batch_fault,
                #[cfg(test)]
                test_batch_sync,
            }),
            Err(error) => Err(Box::new(JournalStartFailure {
                error: bad(error),
                store: retained
                    .lock()
                    .expect("startup handoff lock")
                    .take()
                    .expect("failed thread leaves exact store"),
            })),
        }
    }
    fn submit(&mut self, kind: CommandKind) -> Result<u64, ServiceError> {
        let _lock = self.gate.lock();
        if self.gate.stopped.load(Ordering::SeqCst) || self.gate.pending.load(Ordering::SeqCst) != 0
        {
            return Err(ServiceError::new(
                ErrorKind::Busy,
                "Completion journal has an unresolved original request",
            ));
        }
        let intent = self.next_intent;
        self.next_intent = self
            .next_intent
            .checked_add(1)
            .ok_or_else(|| bad("Journal intent exhausted"))?;
        self.gate.offer.store(0, Ordering::SeqCst);
        self.gate.pending.store(intent, Ordering::SeqCst);
        if self
            .commands
            .as_ref()
            .ok_or_else(|| bad("Journal owner closed"))?
            .try_send(Command { intent, kind })
            .is_err()
        {
            self.gate.stopped.store(true, Ordering::SeqCst);
            return Err(bad("Completion journal command rail lost evidence"));
        }
        Ok(intent)
    }
    pub(crate) fn checkpoint(
        &mut self,
        ordinal: u64,
        purpose: String,
        wire: Vec<u8>,
    ) -> Result<u64, ServiceError> {
        if wire.is_empty() || wire.len() > 518 {
            return Err(bad("Completion checkpoint wire extent invalid"));
        }
        self.submit(CommandKind::Checkpoint {
            ordinal,
            purpose,
            wire,
        })
    }
    /// The creating owner supplies the preceding outcome, while this API alone
    /// creates the final checkpoint. Ordinary batches cannot mint offer tokens.
    pub(crate) fn outcome_checkpoint(
        &mut self,
        values: Vec<serde_json::Value>,
        ordinal: u64,
        purpose: String,
        wire: Vec<u8>,
    ) -> Result<u64, ServiceError> {
        if wire.is_empty() || wire.len() > 518 || values.is_empty() {
            return Err(bad("Combined outcome/checkpoint extent invalid"));
        }
        self.submit(CommandKind::OutcomeCheckpoint {
            values,
            ordinal,
            purpose,
            wire,
        })
    }
    pub(crate) fn record(&mut self, value: serde_json::Value) -> Result<u64, ServiceError> {
        self.submit(CommandKind::Record(value))
    }
    pub(crate) fn records(&mut self, values: Vec<serde_json::Value>) -> Result<u64, ServiceError> {
        self.submit(CommandKind::Records(values))
    }
    pub(crate) fn healthy(&self) -> bool {
        !self.gate.stopped.load(Ordering::SeqCst)
    }
    #[cfg(test)]
    pub(crate) fn test_fail_next_batch(&self, fault: TestBatchFault) {
        *self.test_batch_fault.lock().unwrap() = Some(fault);
    }
    #[cfg(test)]
    pub(crate) fn test_before_next_batch_sync(&self, callback: Box<dyn FnOnce() + Send>) {
        *self.test_batch_sync.lock().unwrap() = Some(callback);
    }
    #[cfg(test)]
    pub(crate) fn test_stall_before_reply(
        &self,
        stop: Arc<crate::firmware_service::OwnerStop>,
        stall: Duration,
    ) {
        *self.test_stop_reply.lock().unwrap() = Some(TestStopReply {
            checkpoint: true,
            stop,
            stall: Some(stall),
        });
    }
    pub(crate) fn poll(&self) -> Option<JournalEvent> {
        self.events
            .try_recv()
            .ok()
            .map(|event| self.acknowledge(event))
    }
    pub(crate) fn wait_event(&self, timeout: Duration) -> Option<JournalEvent> {
        // The worker takes this gate when publishing a durable checkpoint.
        // Receiving must not hold it while waiting for that worker to finish.
        #[cfg(test)]
        let receive_timer =
            test_profile::Timer::new(self.test_profile.clone(), "wait_event_receive");
        let received = self.events.recv_timeout(timeout);
        #[cfg(test)]
        receive_timer.finish();
        match received {
            Ok(event) => Some(self.acknowledge(event)),
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => None,
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                // A lost rail retains the original host budget and pending intent.
                // Keep bounded raw/Stop checks without spinning while it is closed.
                thread::sleep(timeout);
                None
            }
        }
    }
    fn acknowledge(&self, mut event: JournalEvent) -> JournalEvent {
        #[cfg(test)]
        let _acknowledge_timer =
            test_profile::Timer::new(self.test_profile.clone(), "owner_reply_acknowledge");
        let _lock = self.gate.lock();
        if self
            .gate
            .pending
            .compare_exchange(event.intent, 0, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            self.gate.stopped.store(true, Ordering::SeqCst);
            event.result = Err(bad("Journal original intent differs"));
        }
        if self.gate.stopped.load(Ordering::SeqCst) && event.result.is_ok() {
            event.result = Err(bad("Journal acknowledgement revoked by stopped owner"));
        }
        event
    }
    pub(crate) fn pending_intent(&self) -> Option<u64> {
        let _lock = self.gate.lock();
        let intent = self.gate.pending.load(Ordering::SeqCst);
        (intent != 0).then_some(intent)
    }
    // Only the creating Executor's terminal zero-call rail uses this. It first
    // settles the original intent, then revokes any unused checkpoint before
    // recording cleanup; this never creates a replacement offer.
    pub(crate) fn retire_zero_call_offer(&self) -> Result<Option<u64>, ServiceError> {
        let _lock = self.gate.lock();
        if self.gate.stopped.load(Ordering::SeqCst) || self.gate.pending.load(Ordering::SeqCst) != 0
        {
            return Err(bad(
                "Zero-call journal retirement has an unresolved original intent",
            ));
        }
        let intent = self.gate.offer.swap(0, Ordering::SeqCst);
        Ok((intent != 0).then_some(intent))
    }
    #[cfg(test)]
    pub(crate) fn test_stop_before_reply(
        &self,
        checkpoint: bool,
        stop: Arc<crate::firmware_service::OwnerStop>,
    ) {
        self.test_cancelled_reply_release
            .store(false, Ordering::SeqCst);
        *self.test_stop_reply.lock().unwrap() = Some(TestStopReply {
            checkpoint,
            stop,
            stall: None,
        });
    }
    #[cfg(test)]
    pub(crate) fn test_release_cancelled_reply(&self) {
        self.test_cancelled_reply_release
            .store(true, Ordering::SeqCst);
    }
    pub(crate) fn offer_fence(
        &self,
        checkpoint: CompletionCheckpoint,
        ordinal: u64,
        wire: &[u8],
    ) -> Result<OfferFence<'_>, ServiceError> {
        let guard = self.gate.lock();
        if checkpoint.sequence == 0
            || JobSha256::parse(&checkpoint.prefix_sha256).is_err()
            || self.gate.stopped.load(Ordering::SeqCst)
            || self.gate.pending.load(Ordering::SeqCst) != 0
            || !Arc::ptr_eq(&self.gate, &checkpoint.gate)
            || self.run != checkpoint.run
            || checkpoint.ordinal != ordinal
            || checkpoint.wire != wire
            || self
                .gate
                .offer
                .compare_exchange(checkpoint.intent, 0, Ordering::SeqCst, Ordering::SeqCst)
                .is_err()
        {
            return Err(bad("Completion checkpoint stale, consumed or revoked"));
        }
        Ok(OfferFence { _guard: guard })
    }
    pub(crate) fn stop(&self) {
        self.gate.stopped.store(true, Ordering::SeqCst);
    }
    pub(crate) fn run(&self) -> &str {
        &self.run
    }
    pub(crate) fn path(&self) -> &std::path::Path {
        &self.path
    }
    pub(crate) fn join(mut self) -> Result<LibraryStore, ServiceError> {
        self.stop();
        self.commands.take();
        self.join
            .take()
            .expect("one journal thread")
            .join()
            .map_err(|_| bad("Completion journal owner panicked; original store unavailable"))
    }
}
impl Drop for CompletionJournal {
    fn drop(&mut self) {
        self.stop();
        self.commands.take();
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}
struct Writer {
    file: File,
    path: PathBuf,
    run: String,
    sequence: u64,
    bytes: u64,
    hash: Sha256,
    stamp: Option<(i64, i64, i64, i64)>,
    poisoned: bool,
    #[cfg(test)]
    test_fault: Option<TestBatchFault>,
    #[cfg(test)]
    test_before_sync: Option<Box<dyn FnOnce() + Send>>,
    #[cfg(test)]
    test_profile: Option<Arc<Mutex<test_profile::Measurements>>>,
}
#[cfg(test)]
struct TestWrite<'a> {
    file: &'a mut File,
    maximum: usize,
    fail_after_first: bool,
    wrote: bool,
}
#[cfg(test)]
impl Write for TestWrite<'_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self.wrote && self.fail_after_first {
            return Err(std::io::Error::other(
                "Injected next filesystem write error",
            ));
        }
        self.wrote = true;
        self.file
            .write(&bytes[..bytes.len().min(self.maximum.max(1))])
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}
fn stamp(metadata: &std::fs::Metadata) -> (i64, i64, i64, i64) {
    (
        metadata.mtime(),
        metadata.mtime_nsec(),
        metadata.ctime(),
        metadata.ctime_nsec(),
    )
}
impl Writer {
    fn append(&mut self, value: serde_json::Value) -> Result<JournalReceipt, ServiceError> {
        self.append_group(vec![value], false, false)
    }
    fn append_many(
        &mut self,
        values: Vec<serde_json::Value>,
    ) -> Result<JournalReceipt, ServiceError> {
        self.append_group(values, true, false)
    }
    fn append_outcome_checkpoint(
        &mut self,
        mut values: Vec<serde_json::Value>,
        ordinal: u64,
        purpose: String,
        wire: Vec<u8>,
    ) -> Result<JournalReceipt, ServiceError> {
        // An empty prefix is not an outcome, and caller-supplied checkpoint or
        // begin frames are never allowed even through this dedicated command.
        if values.is_empty()
            || values
                .iter()
                .any(|value| matches!(value["kind"].as_str(), Some("begin" | "checkpoint")))
        {
            self.poisoned = true;
            return Err(bad("Combined checkpoint requires ordinary outcome frames"));
        }
        values.push(serde_json::json!({"kind":"checkpoint","ordinal":ordinal,
            "purpose":purpose,"wire_sha256":JobSha256::of(&wire).hex(),"wire":wire}));
        self.append_group(values, true, true)
    }
    fn append_group(
        &mut self,
        values: Vec<serde_json::Value>,
        grouped: bool,
        final_checkpoint: bool,
    ) -> Result<JournalReceipt, ServiceError> {
        if self.poisoned {
            return Err(bad("Completion journal writer is poisoned"));
        }
        let result = self.append_checked(&values, grouped, final_checkpoint);
        if result.is_err() {
            self.poisoned = true;
        }
        result
    }
    fn append_checked(
        &mut self,
        values: &[serde_json::Value],
        grouped: bool,
        final_checkpoint: bool,
    ) -> Result<JournalReceipt, ServiceError> {
        #[cfg(test)]
        let _append_total = test_profile::Timer::new(self.test_profile.clone(), "append_total");
        if values.is_empty() || values.len() > MAX_BATCH_RECORDS {
            return Err(bad("Completion journal batch record budget exceeded"));
        }
        let sequence = self
            .sequence
            .checked_add(values.len() as u64)
            .ok_or_else(|| bad("Completion journal sequence overflow"))?;
        if sequence > FIRMWARE_FRAME_LIMIT {
            return Err(bad("Completion journal frame budget exceeded"));
        }
        let mut bytes = Vec::new();
        for (index, value) in values.iter().enumerate() {
            let kind = value.get("kind").and_then(|kind| kind.as_str());
            if grouped
                && (kind == Some("begin")
                    || (kind == Some("checkpoint")
                        && !(final_checkpoint && index + 1 == values.len())))
            {
                return Err(bad(
                    "Completion journal batches cannot contain begin or checkpoint",
                ));
            }
            if kind == Some("begin") && self.sequence != 0 {
                return Err(bad("Completion journal begin cannot be repeated"));
            }
            #[cfg(test)]
            let serialize_timer =
                test_profile::Timer::new(self.test_profile.clone(), "append_serialize_payload");
            let body = serde_json::to_vec(value).map_err(bad)?;
            let limit = if !grouped && self.sequence == 0 && kind == Some("begin") {
                MAX_BEGIN_RECORD
            } else {
                MAX_RECORD
            };
            if body.is_empty() || body.len() > limit {
                return Err(bad("Completion journal record budget exceeded"));
            }
            let frame_len = 44usize
                .checked_add(body.len())
                .ok_or_else(|| bad("Completion journal frame length overflow"))?;
            let group_len = bytes
                .len()
                .checked_add(frame_len)
                .ok_or_else(|| bad("Completion journal batch length overflow"))?;
            if grouped && group_len > MAX_BATCH_BYTES {
                return Err(bad("Completion journal aggregate batch budget exceeded"));
            }
            #[cfg(test)]
            serialize_timer.finish();
            #[cfg(test)]
            let frame_timer =
                test_profile::Timer::new(self.test_profile.clone(), "append_frame_hash_encode");
            bytes.extend_from_slice(b"GP2LCJ01");
            bytes.extend_from_slice(&u32::try_from(body.len()).map_err(bad)?.to_le_bytes());
            bytes.extend_from_slice(&Sha256::digest(&body));
            bytes.extend_from_slice(&body);
            #[cfg(test)]
            frame_timer.finish();
        }
        let end = self
            .bytes
            .checked_add(bytes.len() as u64)
            .ok_or_else(|| bad("Completion journal length overflow"))?;
        if end > FIRMWARE_JOURNAL_LIMIT {
            return Err(bad("Completion journal total budget exceeded"));
        }
        #[cfg(test)]
        let metadata_timer =
            test_profile::Timer::new(self.test_profile.clone(), "append_metadata_before");
        let before = self.file.metadata().map_err(bad)?;
        let named = std::fs::symlink_metadata(&self.path).map_err(bad)?;
        if !named.is_file()
            || named.file_type().is_symlink()
            || named.ino() != before.ino()
            || named.dev() != before.dev()
            || before.len() != self.bytes
            || before.nlink() != 1
            || before.mode() & 0o777 != 0o600
            || before.uid() != rustix::process::geteuid().as_raw()
            || self.stamp.is_some_and(|prior| prior != stamp(&before))
        {
            return Err(bad("Completion journal prefix, name or ownership changed"));
        }
        #[cfg(test)]
        metadata_timer.finish();
        #[cfg(test)]
        let write_timer = test_profile::Timer::new(self.test_profile.clone(), "append_write_all");
        #[cfg(test)]
        match self.test_fault {
            Some(TestBatchFault::WriteThenError(maximum) | TestBatchFault::ShortWrite(maximum)) => {
                // Exercise write_all itself: its first full-suffix request sees a
                // short filesystem write; the next request either succeeds or fails.
                let fail_after_first =
                    matches!(self.test_fault, Some(TestBatchFault::WriteThenError(_)));
                TestWrite {
                    file: &mut self.file,
                    maximum,
                    fail_after_first,
                    wrote: false,
                }
                .write_all(&bytes)
                .map_err(bad)?;
            }
            _ => self.file.write_all(&bytes).map_err(bad)?,
        }
        #[cfg(not(test))]
        self.file.write_all(&bytes).map_err(bad)?;
        #[cfg(test)]
        write_timer.finish();
        #[cfg(test)]
        if let Some(callback) = self.test_before_sync.take() {
            callback();
        }
        #[cfg(test)]
        if matches!(self.test_fault, Some(TestBatchFault::Sync)) {
            return Err(bad("Injected journal filesystem sync error"));
        }
        #[cfg(test)]
        let sync_timer = test_profile::Timer::new(self.test_profile.clone(), "append_sync_all");
        self.file.sync_all().map_err(bad)?;
        #[cfg(test)]
        sync_timer.finish();
        #[cfg(test)]
        let reread_timer =
            test_profile::Timer::new(self.test_profile.clone(), "append_reread_metadata");
        let after = self.file.metadata().map_err(bad)?;
        let mut reread = vec![0; bytes.len()];
        self.file
            .read_exact_at(&mut reread, self.bytes)
            .map_err(bad)?;
        #[cfg(test)]
        if matches!(self.test_fault, Some(TestBatchFault::Reread)) {
            reread[0] ^= 1;
        }
        let settled = self.file.metadata().map_err(bad)?;
        let settled_name = std::fs::symlink_metadata(&self.path).map_err(bad)?;
        if reread != bytes
            || after.len() != end
            || after.ino() != before.ino()
            || after.dev() != before.dev()
            || settled.len() != end
            || settled.ino() != before.ino()
            || settled.dev() != before.dev()
            || stamp(&settled) != stamp(&after)
            || settled.nlink() != 1
            || settled.mode() & 0o777 != 0o600
            || settled.uid() != rustix::process::geteuid().as_raw()
            || !settled_name.is_file()
            || settled_name.ino() != before.ino()
            || settled_name.dev() != before.dev()
        {
            return Err(bad("Completion journal durable reread differs"));
        }
        #[cfg(test)]
        reread_timer.finish();
        #[cfg(test)]
        let receipt_timer =
            test_profile::Timer::new(self.test_profile.clone(), "append_publication_receipt");
        self.hash.update(&bytes);
        self.bytes = end;
        self.sequence = sequence;
        self.stamp = Some(stamp(&settled));
        let receipt = JournalReceipt {
            run: self.run.clone(),
            sequence: self.sequence,
            prefix_sha256: format!("{:x}", self.hash.clone().finalize()),
            path: self.path.clone(),
            bytes: self.bytes,
        };
        #[cfg(test)]
        receipt_timer.finish();
        #[cfg(test)]
        for value in values {
            test_profile::appended(
                &self.test_profile,
                value
                    .get("kind")
                    .and_then(|kind| kind.as_str())
                    .unwrap_or("missing-kind"),
            );
        }
        #[cfg(test)]
        test_profile::committed(&self.test_profile);
        Ok(receipt)
    }
}

#[cfg(test)]
mod wakeup_tests {
    use super::*;
    use std::time::Instant;

    fn journal(pending: u64) -> (CompletionJournal, Sender<JournalEvent>) {
        let (sender, events) = bounded(4);
        let gate = Arc::new(Gate::default());
        gate.pending.store(pending, Ordering::SeqCst);
        (
            CompletionJournal {
                run: "11111111111111111111111111111111".into(),
                path: PathBuf::new(),
                gate,
                commands: None,
                events,
                join: None,
                next_intent: 1,
                test_profile: None,
                test_stop_reply: Arc::new(Mutex::new(None)),
                test_cancelled_reply_release: Arc::new(AtomicBool::new(false)),
                test_batch_fault: Arc::new(Mutex::new(None)),
                test_batch_sync: Arc::new(Mutex::new(None)),
            },
            sender,
        )
    }

    fn recorded(intent: u64) -> JournalEvent {
        JournalEvent {
            intent,
            result: Ok(JournalReply::Recorded(JournalReceipt {
                run: "11111111111111111111111111111111".into(),
                sequence: 1,
                prefix_sha256: JobSha256::of(b"channel fixture only").hex(),
                path: PathBuf::new(),
                bytes: 1,
            })),
        }
    }

    #[test]
    fn grouped_frame_and_total_byte_checked_limits_poison_before_disk_write() {
        use std::os::unix::fs::OpenOptionsExt;
        for (sequence, bytes) in [
            (FIRMWARE_FRAME_LIMIT - 1, 0),
            (u64::MAX, 0),
            (0, FIRMWARE_JOURNAL_LIMIT - 1),
            (0, u64::MAX),
        ] {
            let path = std::env::temp_dir().join(format!(
                "gp200-batch-limits-{}-{}-{}",
                std::process::id(),
                sequence,
                bytes
            ));
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)
                .unwrap();
            let mut writer = Writer {
                file,
                path: path.clone(),
                run: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
                sequence,
                bytes,
                hash: Sha256::new(),
                stamp: None,
                poisoned: false,
                test_profile: None,
                test_fault: None,
                test_before_sync: None,
            };
            assert!(
                writer
                    .append_many(vec![serde_json::json!({"kind":"raw-chunk"}); 2])
                    .is_err()
            );
            assert!(writer.poisoned);
            assert_eq!(writer.sequence, sequence);
            assert_eq!(writer.bytes, bytes);
            assert_eq!(std::fs::metadata(&path).unwrap().len(), 0);
            assert!(
                writer
                    .append(serde_json::json!({"kind":"checkpoint"}))
                    .is_err()
            );
            drop(writer);
            std::fs::remove_file(path).unwrap();
        }
    }
    #[test]
    fn wait_receives_before_taking_the_checkpoint_delivery_gate() {
        let (journal, sender) = journal(17);
        let gate = journal.gate.clone();
        let guard = gate.lock();
        thread::scope(|scope| {
            let (entered, entering) = bounded(1);
            let waiting = &journal;
            let waiter = scope.spawn(move || {
                entered.send(()).unwrap();
                waiting.wait_event(Duration::from_secs(2))
            });
            entering.recv().unwrap();
            sender.send(recorded(17)).unwrap();
            let deadline = Instant::now() + Duration::from_secs(2);
            while !sender.is_empty() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(1));
            }
            let received_without_gate = sender.is_empty();
            drop(guard);
            let event = waiter.join().unwrap().expect("original reply returned");
            assert!(
                received_without_gate,
                "waiting for a reply must not hold or first acquire the worker's delivery gate"
            );
            assert_eq!(event.intent, 17);
            assert!(event.result.is_ok());
        });
        assert_eq!(journal.pending_intent(), None);
    }

    #[test]
    fn wait_timeout_keeps_the_original_pending_intent_and_leaves_gate_available() {
        let (journal, _sender) = journal(17);
        assert!(journal.wait_event(Duration::ZERO).is_none());
        assert_eq!(journal.pending_intent(), Some(17));
        assert!(!journal.gate.stopped.load(Ordering::SeqCst));
        assert!(journal.gate.delivery.try_lock().is_ok());
    }

    #[test]
    fn poll_and_wait_enforce_identical_original_intent_and_stop_validation() {
        for wait in [false, true] {
            for (pending, stopped, expected_error, remaining) in [
                (17, false, None, None),
                (18, false, Some("Journal original intent differs"), Some(18)),
                (
                    17,
                    true,
                    Some("Journal acknowledgement revoked by stopped owner"),
                    None,
                ),
            ] {
                let (journal, sender) = journal(pending);
                journal.gate.stopped.store(stopped, Ordering::SeqCst);
                sender.send(recorded(17)).unwrap();
                let event = if wait {
                    journal.wait_event(Duration::ZERO)
                } else {
                    journal.poll()
                }
                .expect("ready reply");
                assert_eq!(event.intent, 17);
                if let Some(expected_error) = expected_error {
                    let error = event.result.err().expect("acknowledgement rejected");
                    assert!(error.to_string().contains(expected_error));
                    assert!(journal.gate.stopped.load(Ordering::SeqCst));
                } else {
                    assert!(event.result.is_ok());
                }
                assert_eq!(journal.pending_intent(), remaining);
                assert!(journal.poll().is_none());
            }
        }
    }
}
