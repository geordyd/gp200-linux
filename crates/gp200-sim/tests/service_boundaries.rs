use gp200_domain::*;
use gp200_sim::*;
use gp200_transport::{MidiDeviceService, TransportConfig};
use std::{
    sync::{Arc, mpsc},
    time::{Duration, Instant},
};

fn until(
    service: &MidiDeviceService,
    wake: &mpsc::Receiver<()>,
    predicate: impl Fn(&DeviceEvent) -> bool,
) -> Vec<DeviceEvent> {
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut events = Vec::new();
    loop {
        while let Some(event) = service.try_next_event() {
            let done = predicate(&event);
            events.push(event);
            if done {
                return events;
            }
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(
            !remaining.is_zero(),
            "Bounded worker event deadline expired"
        );
        wake.recv_timeout(remaining)
            .expect("Worker event wake before deadline");
    }
}
#[test]
fn real_worker_callback_saturation_is_a_protocol_fault_and_never_a_partial_snapshot() {
    let backend = SimulatorBackend::new();
    let handle = backend.handle();
    let (wake_tx, wake_rx) = mpsc::channel();
    let config = TransportConfig {
        callback_capacity: 8,
        minimum_request_spacing: Duration::ZERO,
        packet_spacing: Duration::ZERO,
        ..Default::default()
    };
    let service = MidiDeviceService::with_backend(
        Box::new(backend),
        config,
        Arc::new(move || {
            let _ = wake_tx.send(());
        }),
    );
    service
        .try_submit(DeviceCommand::Connect {
            intent: IntentId(1),
            selection: simulator_port().selection,
        })
        .unwrap();
    let connected = until(&service, &wake_rx, |event| {
        matches!(
            event,
            DeviceEvent::ConnectionChanged {
                state: ConnectionState::Ready,
                ..
            }
        )
    });
    let epoch = connected
        .iter()
        .find_map(|event| match event {
            DeviceEvent::SessionChanged {
                epoch, identity, ..
            } => {
                assert_eq!(identity.mode, BackendMode::Simulator);
                Some(*epoch)
            }
            _ => None,
        })
        .unwrap();
    handle.with_pedal(|pedal| pedal.profile.callback_chunk_size = 1);
    service
        .try_submit(DeviceCommand::ReadCurrent(RequestStamp {
            epoch,
            intent: IntentId(2),
        }))
        .unwrap();
    let events = until(
        &service,
        &wake_rx,
        |event| matches!(event, DeviceEvent::OperationFailed { stamp, error } if stamp.intent == IntentId(2) && error.kind == ErrorKind::Protocol),
    );
    assert!(!events.iter().any(|event| matches!(event, DeviceEvent::SnapshotReceived { stamp, .. } if stamp.intent == IntentId(2))));
    service.shutdown();
    assert!(service.wait_stopped(Duration::from_secs(1)));
}
#[test]
fn expectation_replay_connects_through_the_real_worker_and_stays_replay() {
    let manifest = ReplayManifest::parse(include_bytes!(
        "../../../tests/fixtures/sim/replay-identity-current.json"
    ))
    .unwrap();
    let backend = ExpectationReplayBackend::new(manifest.exchanges().unwrap()).unwrap();
    let (wake_tx, wake_rx) = mpsc::channel();
    let config = TransportConfig {
        minimum_request_spacing: Duration::ZERO,
        packet_spacing: Duration::ZERO,
        ..Default::default()
    };
    let service = MidiDeviceService::with_backend(
        Box::new(backend),
        config,
        Arc::new(move || {
            let _ = wake_tx.send(());
        }),
    );
    service
        .try_submit(DeviceCommand::Connect {
            intent: IntentId(1),
            selection: replay_port().selection,
        })
        .unwrap();
    let events = until(&service, &wake_rx, |event| {
        matches!(
            event,
            DeviceEvent::ConnectionChanged {
                state: ConnectionState::Ready,
                ..
            }
        )
    });
    assert!(events.iter().any(|event| matches!(event, DeviceEvent::SessionChanged { identity, .. } if identity.mode == BackendMode::CaptureReplay)));
    assert!(events.iter().any(|event| matches!(event, DeviceEvent::SnapshotReceived { snapshot, .. } if matches!(snapshot.source, SnapshotSource::Capture { .. }))));
    assert!(!events.iter().any(|event| matches!(event, DeviceEvent::SnapshotReceived { snapshot, .. } if matches!(snapshot.source, SnapshotSource::Hardware { .. }))));
    service.shutdown();
    assert!(service.wait_stopped(Duration::from_secs(1)));
}
