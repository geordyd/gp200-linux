use super::*;
use crossbeam_channel::{Receiver, Sender, TryRecvError};
use gp200_domain::SessionEpoch;
use std::collections::VecDeque;
use std::sync::atomic::AtomicUsize;

const TEST_DEADLINE: Duration = Duration::from_secs(2);
type ReceivedBytes = Receiver<(SessionEpoch, Vec<u8>)>;

enum ReadStep {
    Bytes(Vec<u8>),
    WouldBlock,
    Interrupted,
    Fault(ServiceError),
    // This controlled gate lets tests observe an unfinished read without
    // depending on thread scheduling or using a guessed startup delay.
    Gate {
        entered: Sender<()>,
        release: Receiver<()>,
        next: Box<ReadStep>,
    },
}

struct TestInput {
    steps: VecDeque<ReadStep>,
    statuses: VecDeque<Result<(), ServiceError>>,
    idle_interrupted: bool,
    reads: Arc<AtomicUsize>,
    dropped: Arc<AtomicBool>,
}

impl TestInput {
    fn new(steps: impl IntoIterator<Item = ReadStep>) -> Self {
        Self {
            steps: steps.into_iter().collect(),
            statuses: VecDeque::new(),
            idle_interrupted: false,
            reads: Arc::new(AtomicUsize::new(0)),
            dropped: Arc::new(AtomicBool::new(false)),
        }
    }

    fn perform(step: ReadStep, buffer: &mut [u8]) -> Result<usize, InputReadError> {
        match step {
            ReadStep::Bytes(bytes) => {
                buffer[..bytes.len()].copy_from_slice(&bytes);
                Ok(bytes.len())
            }
            ReadStep::WouldBlock => Err(InputReadError::WouldBlock),
            ReadStep::Interrupted => Err(InputReadError::Interrupted),
            ReadStep::Fault(error) => Err(InputReadError::Failed(error)),
            ReadStep::Gate {
                entered,
                release,
                next,
            } => {
                entered.send(()).expect("observe gated read");
                release
                    .recv_timeout(TEST_DEADLINE)
                    .expect("release gated read");
                Self::perform(*next, buffer)
            }
        }
    }
}

impl RawMidiInput for TestInput {
    fn read(&mut self, buffer: &mut [u8]) -> Result<usize, InputReadError> {
        self.reads.fetch_add(1, Ordering::AcqRel);
        let step = self.steps.pop_front().unwrap_or(if self.idle_interrupted {
            ReadStep::Interrupted
        } else {
            ReadStep::WouldBlock
        });
        Self::perform(step, buffer)
    }

    fn check_status(&mut self) -> Result<(), ServiceError> {
        self.statuses.pop_front().unwrap_or(Ok(()))
    }
}

impl Drop for TestInput {
    fn drop(&mut self) {
        self.dropped.store(true, Ordering::Release);
    }
}

fn sink(capacity: usize) -> (ReceiveSink, ReceivedBytes, Arc<AtomicBool>) {
    let (sender, receiver) = bounded(capacity);
    let overflow = Arc::new(AtomicBool::new(false));
    (
        ReceiveSink::new(sender, overflow.clone(), SessionEpoch(51)),
        receiver,
        overflow,
    )
}

fn error(kind: ErrorKind, message: &str) -> ServiceError {
    ServiceError::new(kind, message)
}

#[test]
fn connection_cannot_expose_a_sendable_session_until_read_finishes() {
    let (entered_sender, entered_receiver) = bounded(1);
    let (release_sender, release_receiver) = bounded(1);
    let input = TestInput::new([ReadStep::Gate {
        entered: entered_sender,
        release: release_receiver,
        next: Box::new(ReadStep::WouldBlock),
    }]);
    let dropped = input.dropped.clone();
    let (receive, _bytes, _) = sink(8);
    let (completed_sender, completed_receiver) = bounded(1);
    let connector = thread::spawn(move || {
        completed_sender
            .send(RawReader::start(input, receive, TEST_DEADLINE))
            .expect("return startup result");
    });
    entered_receiver
        .recv_timeout(TEST_DEADLINE)
        .expect("first read started");
    assert!(matches!(
        completed_receiver.try_recv(),
        Err(TryRecvError::Empty)
    ));
    release_sender.send(()).unwrap();
    let reader = completed_receiver
        .recv_timeout(TEST_DEADLINE)
        .expect("startup completed")
        .unwrap();
    connector.join().unwrap();
    reader.health().unwrap();
    drop(reader);
    assert!(dropped.load(Ordering::Acquire));
}

#[test]
fn interrupted_read_does_not_mark_capture_ready() {
    let (entered_sender, entered_receiver) = bounded(1);
    let (release_sender, release_receiver) = bounded(1);
    let input = TestInput::new([
        ReadStep::Interrupted,
        ReadStep::Gate {
            entered: entered_sender,
            release: release_receiver,
            next: Box::new(ReadStep::WouldBlock),
        },
    ]);
    let reads = input.reads.clone();
    let (receive, _bytes, _) = sink(8);
    let (completed_sender, completed_receiver) = bounded(1);
    let connector = thread::spawn(move || {
        completed_sender
            .send(RawReader::start(input, receive, TEST_DEADLINE))
            .unwrap();
    });
    entered_receiver.recv_timeout(TEST_DEADLINE).unwrap();
    assert_eq!(reads.load(Ordering::Acquire), 2);
    assert!(matches!(
        completed_receiver.try_recv(),
        Err(TryRecvError::Empty)
    ));
    release_sender.send(()).unwrap();
    let reader = completed_receiver
        .recv_timeout(TEST_DEADLINE)
        .unwrap()
        .unwrap();
    drop(reader);
    connector.join().unwrap();
}

#[test]
fn bytes_received_by_the_priming_read_keep_their_epoch_and_order() {
    let first = vec![0xf0, 0x21, 0x25, 0x7e];
    let second = vec![0x47, 0x50, 0x2d, 0x32, 0xf7];
    let input = TestInput::new([
        ReadStep::Bytes(first.clone()),
        ReadStep::Bytes(second.clone()),
    ]);
    let dropped = input.dropped.clone();
    let (receive, bytes, _) = sink(8);
    let reader = RawReader::start(input, receive, TEST_DEADLINE).unwrap();
    assert_eq!(
        bytes.recv_timeout(TEST_DEADLINE).unwrap(),
        (SessionEpoch(51), first)
    );
    assert_eq!(
        bytes.recv_timeout(TEST_DEADLINE).unwrap(),
        (SessionEpoch(51), second)
    );
    drop(reader);
    assert!(dropped.load(Ordering::Acquire));
}

#[test]
fn startup_read_and_status_failures_close_input_before_returning() {
    for status_failure in [false, true] {
        let expected = error(ErrorKind::Disconnected, "unplug during first input read");
        let mut input = TestInput::new(if status_failure {
            vec![ReadStep::WouldBlock]
        } else {
            vec![ReadStep::Fault(expected.clone())]
        });
        if status_failure {
            input.statuses.push_back(Err(expected.clone()));
        }
        let dropped = input.dropped.clone();
        let (receive, _bytes, _) = sink(8);
        let result = RawReader::start(input, receive, TEST_DEADLINE);
        assert!(matches!(result, Err(ref actual) if actual == &expected));
        assert!(dropped.load(Ordering::Acquire));
    }
}

#[test]
fn initial_overrun_preserves_bytes_but_refuses_session_exposure() {
    let first = vec![0xf0, 0xf7];
    let expected = error(ErrorKind::Protocol, "input overrun during first read");
    let mut input = TestInput::new([ReadStep::Bytes(first.clone())]);
    input.statuses.push_back(Err(expected.clone()));
    let dropped = input.dropped.clone();
    let (receive, bytes, _) = sink(8);
    let result = RawReader::start(input, receive, TEST_DEADLINE);
    assert!(matches!(result, Err(ref actual) if actual == &expected));
    assert!(dropped.load(Ordering::Acquire));
    assert_eq!(bytes.try_recv().unwrap(), (SessionEpoch(51), first));
}

#[test]
fn priming_queue_overflow_is_a_failed_startup_not_silent_loss() {
    let input = TestInput::new([ReadStep::Bytes(vec![0xf0, 0xf7])]);
    let dropped = input.dropped.clone();
    let (receive, _bytes, overflow) = sink(0);
    let result = RawReader::start(input, receive, TEST_DEADLINE);
    assert!(
        matches!(result, Err(ref actual) if actual.kind == ErrorKind::Protocol && actual.message.contains("queue overflow"))
    );
    assert!(overflow.load(Ordering::Acquire));
    assert!(dropped.load(Ordering::Acquire));
}

#[test]
fn readiness_deadline_joins_an_interrupted_reader_and_closes_input() {
    let mut input = TestInput::new([]);
    input.idle_interrupted = true;
    let reads = input.reads.clone();
    let dropped = input.dropped.clone();
    let (receive, _bytes, _) = sink(8);
    let result = RawReader::start(input, receive, Duration::from_millis(30));
    assert!(matches!(result, Err(ref actual) if actual.kind == ErrorKind::Timeout));
    assert!(reads.load(Ordering::Acquire) > 0);
    assert!(dropped.load(Ordering::Acquire));
}

#[test]
fn a_reader_fault_after_readiness_stays_visible_on_every_health_check() {
    let (entered_sender, entered_receiver) = bounded(1);
    let (release_sender, release_receiver) = bounded(1);
    let expected = error(ErrorKind::Disconnected, "unplug after capture readiness");
    let input = TestInput::new([
        ReadStep::WouldBlock,
        ReadStep::Gate {
            entered: entered_sender,
            release: release_receiver,
            next: Box::new(ReadStep::Fault(expected.clone())),
        },
    ]);
    let dropped = input.dropped.clone();
    let (receive, _bytes, _) = sink(8);
    let mut reader = RawReader::start(input, receive, TEST_DEADLINE).unwrap();
    entered_receiver.recv_timeout(TEST_DEADLINE).unwrap();
    reader.health().unwrap();
    release_sender.send(()).unwrap();
    // Joining establishes that the fault has been published without polling.
    reader.thread.take().unwrap().join().unwrap();
    assert_eq!(reader.health(), Err(expected.clone()));
    assert_eq!(reader.health(), Err(expected));
    assert!(dropped.load(Ordering::Acquire));
}

fn firmware_sink() -> crate::firmware_backend::FirmwareReceiver {
    use gp200_domain::*;
    crate::firmware_backend::FirmwareReceiver::new(&FirmwareLifecyclePlanDto {
        review_sha256: "00".repeat(32),
        profile_sha256: "11".repeat(32),
        origin: FirmwareObservationOrigin::MockSimulation,
        nonblock_actions: 0,
        epoch: SessionEpoch(61),
        generation: 7,
        clock_origin: "readiness fixture".into(),
        binding: FirmwareUsbObservation {
            topology: "test USB".into(),
            vendor: 1,
            product: 2,
            serial: "test serial".into(),
            interface: "test interface".into(),
            endpoint: "test endpoint".into(),
        },
    })
}
#[test]
fn firmware_capture_readiness_waits_for_actual_noninterrupted_read_and_joins() {
    let (entered_tx, entered_rx) = bounded(1);
    let (release_tx, release_rx) = bounded(1);
    let input = TestInput::new([
        ReadStep::Interrupted,
        ReadStep::Gate {
            entered: entered_tx,
            release: release_rx,
            next: Box::new(ReadStep::WouldBlock),
        },
    ]);
    let dropped = input.dropped.clone();
    let reads = input.reads.clone();
    let receiver = firmware_sink();
    let (tx, rx) = bounded(1);
    let worker = thread::spawn(move || {
        tx.send(crate::firmware_raw::FirmwareRawReader::start(
            input,
            receiver.sink,
            TEST_DEADLINE,
        ))
        .unwrap();
    });
    entered_rx.recv_timeout(TEST_DEADLINE).unwrap();
    assert_eq!(reads.load(Ordering::Acquire), 2);
    assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
    release_tx.send(()).unwrap();
    let reader = rx.recv_timeout(TEST_DEADLINE).unwrap().unwrap();
    worker.join().unwrap();
    drop(reader);
    assert!(dropped.load(Ordering::Acquire));
}
#[test]
fn firmware_capture_fault_before_ready_has_allocated_frontier_and_real_join() {
    let input = TestInput::new([ReadStep::Fault(error(
        ErrorKind::Io,
        "synthetic firmware capture error",
    ))]);
    let dropped = input.dropped.clone();
    let receiver = firmware_sink();
    assert!(
        crate::firmware_raw::FirmwareRawReader::start(input, receiver.sink.clone(), TEST_DEADLINE)
            .is_err()
    );
    assert!(dropped.load(Ordering::Acquire));
    assert!(receiver.sink.fault.load(Ordering::Acquire));
    assert_eq!(receiver.sink.allocated.load(Ordering::Acquire), 1);
}
#[test]
fn firmware_capture_status_fault_and_readiness_timeout_never_return_a_reader() {
    let mut input = TestInput::new([ReadStep::WouldBlock]);
    input
        .statuses
        .push_back(Err(error(ErrorKind::Io, "synthetic xrun")));
    let dropped = input.dropped.clone();
    let receiver = firmware_sink();
    assert!(
        crate::firmware_raw::FirmwareRawReader::start(input, receiver.sink, TEST_DEADLINE).is_err()
    );
    assert!(dropped.load(Ordering::Acquire));
    let mut input = TestInput::new([]);
    input.idle_interrupted = true;
    let dropped = input.dropped.clone();
    let receiver = firmware_sink();
    let Err(e) = crate::firmware_raw::FirmwareRawReader::start(
        input,
        receiver.sink,
        Duration::from_millis(15),
    ) else {
        panic!("interrupted-only capture incorrectly became ready")
    };
    assert_eq!(e.kind, ErrorKind::Timeout);
    assert!(dropped.load(Ordering::Acquire));
}
#[test]
fn firmware_capture_priming_raw_is_verbatim_and_bounded_overflow_faults_readiness() {
    let bytes = vec![0xf0, 1, 0xf7];
    let input = TestInput::new([ReadStep::Bytes(bytes.clone())]);
    let receiver = firmware_sink();
    let reader =
        crate::firmware_raw::FirmwareRawReader::start(input, receiver.sink.clone(), TEST_DEADLINE)
            .unwrap();
    let raw = receiver.received.recv_timeout(TEST_DEADLINE).unwrap();
    assert_eq!(raw.raw, bytes);
    assert_eq!(raw.sequence, 1);
    assert_eq!(raw.epoch, SessionEpoch(61));
    assert_eq!(raw.generation, 7);
    drop(reader);
    let receiver = firmware_sink();
    for _ in 0..64 {
        receiver.sink.receive(&[1]);
    }
    let input = TestInput::new([ReadStep::Bytes(vec![2])]);
    let dropped = input.dropped.clone();
    assert!(
        crate::firmware_raw::FirmwareRawReader::start(input, receiver.sink.clone(), TEST_DEADLINE)
            .is_err()
    );
    assert!(dropped.load(Ordering::Acquire));
    assert!(receiver.sink.allocated.load(Ordering::Acquire) > 64);
}

#[test]
fn normal_consuming_close_joins_actual_inflight_read_and_retains_its_late_original() {
    let (entered_tx, entered_rx) = bounded(1);
    let (release_tx, release_rx) = bounded(1);
    let input = TestInput::new([
        ReadStep::WouldBlock,
        ReadStep::Gate {
            entered: entered_tx,
            release: release_rx,
            next: Box::new(ReadStep::Bytes(vec![0xf0, 0x7d, 0x65, 0xf7])),
        },
    ]);
    let dropped = input.dropped.clone();
    let (tx, received) = bounded(64);
    let overflow = Arc::new(AtomicBool::new(false));
    let source = crate::backend::NormalCallbackSource::new(Instant::now(), SessionEpoch(51));
    let sink = ReceiveSink::observed(tx, overflow.clone(), SessionEpoch(51), source.clone());
    let mut reader = RawReader::start(input, sink, TEST_DEADLINE).unwrap();
    entered_rx.recv_timeout(TEST_DEADLINE).unwrap();
    let before = source.fence(0);
    let closer = std::thread::spawn(move || reader.close_receipt());
    release_tx.send(()).unwrap();
    let mut receipt = closer.join().unwrap();
    receipt.output_destroyed = true;
    assert!(dropped.load(Ordering::Acquire));
    assert_eq!(
        receipt.join,
        gp200_domain::FirmwareNormalReaderJoinObservation::Returned
    );
    let binding = gp200_domain::FirmwareUsbObservation {
        topology: "fixture".into(),
        vendor: 1,
        product: 2,
        serial: "synthetic".into(),
        interface: "mode".into(),
        endpoint: "alias".into(),
    };
    let (observation, seal) = crate::firmware_transition::normal_retirement(
        receipt,
        &source,
        SessionEpoch(51),
        binding,
        before,
        &received,
        false,
    );
    assert!(seal.is_err());
    assert_eq!(observation.allocated_after, 1);
    assert_eq!(
        observation.retained_raw[0].raw,
        vec![0xf0, 0x7d, 0x65, 0xf7]
    );
    observation.retained_json().unwrap();
}
#[test]
fn updater_consuming_close_joins_actual_final_read_and_keeps_shared_original_frontier() {
    let (entered_tx, entered_rx) = bounded(1);
    let (release_tx, release_rx) = bounded(1);
    let input = TestInput::new([
        ReadStep::WouldBlock,
        ReadStep::Gate {
            entered: entered_tx,
            release: release_rx,
            next: Box::new(ReadStep::Bytes(vec![0xf0, 0x7d, 0x67, 0xf7])),
        },
    ]);
    let dropped = input.dropped.clone();
    let receiver = firmware_sink();
    let mut reader =
        crate::firmware_raw::FirmwareRawReader::start(input, receiver.sink.clone(), TEST_DEADLINE)
            .unwrap();
    entered_rx.recv_timeout(TEST_DEADLINE).unwrap();
    let closer = std::thread::spawn(move || reader.close());
    release_tx.send(()).unwrap();
    let mut receipt = closer.join().unwrap();
    assert!(dropped.load(Ordering::Acquire));
    assert_eq!(
        receipt.observation().reader_join,
        gp200_domain::FirmwareNormalReaderJoinObservation::Returned
    );
    assert_eq!(receipt.observation().allocated, 1);
    assert!(!receipt.accounts(&receiver.sink, 0));
    let raw = receiver.received.recv_timeout(TEST_DEADLINE).unwrap();
    assert_eq!(raw.sequence, 1);
    assert_eq!(raw.raw, vec![0xf0, 0x7d, 0x67, 0xf7]);
    assert!(receipt.accounts(&receiver.sink, 1));
    assert!(!receipt.repeat().is_fresh());
}
#[test]
fn updater_explicit_close_preserves_actual_reader_fault_and_repeated_close_cannot_issue_seal() {
    let (entered_tx, entered_rx) = bounded(1);
    let (release_tx, release_rx) = bounded(1);
    let receiver = firmware_sink();
    let input = TestInput::new([
        ReadStep::WouldBlock,
        ReadStep::Gate {
            entered: entered_tx,
            release: release_rx,
            next: Box::new(ReadStep::Fault(error(
                ErrorKind::Io,
                "fixture final read failed",
            ))),
        },
    ]);
    let mut reader =
        crate::firmware_raw::FirmwareRawReader::start(input, receiver.sink.clone(), TEST_DEADLINE)
            .unwrap();
    entered_rx.recv_timeout(TEST_DEADLINE).unwrap();
    release_tx.send(()).unwrap();
    let first = reader.close();
    assert!(first.observation().fault_or_missing);
    assert_eq!(first.fault.unwrap().message, "fixture final read failed");
    let repeated = reader.close();
    assert_eq!(
        repeated.observation().reader_join,
        gp200_domain::FirmwareNormalReaderJoinObservation::Returned
    );
    assert!(!repeated.is_fresh());
    assert!(receiver.sink.allocated.load(Ordering::Acquire) > 0);
}
