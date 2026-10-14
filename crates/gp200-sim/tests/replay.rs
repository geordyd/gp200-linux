use gp200_domain::{BackendMode, ErrorKind};
use gp200_protocol::*;
use gp200_sim::*;
use gp200_transport::TransportBackend;
use std::time::Duration;

fn manifest() -> ReplayManifest {
    ReplayManifest::parse(include_bytes!(
        "../../../tests/fixtures/sim/replay-identity-current.json"
    ))
    .unwrap()
}
#[test]
fn manifest_hashes_mode_and_bounds_are_checked_before_replay() {
    let fixture = manifest();
    assert_eq!(fixture.backend_mode, BackendMode::CaptureReplay);
    let mut bad = fixture.clone();
    bad.backend_mode = BackendMode::Hardware;
    assert!(bad.exchanges().is_err());
    let mut bad = fixture.clone();
    bad.exchanges[0].response_sha256 = "0".repeat(64);
    assert!(bad.exchanges().unwrap_err().message.contains("SHA-256"));
    let mut bad = fixture.clone();
    bad.exchanges[0].request_payload_hex = "not hex".into();
    assert!(bad.exchanges().is_err());
    let mut bad = fixture;
    bad.exchanges[0].response[0].at_ms = 3_600_001;
    assert!(bad.exchanges().is_err());
    assert!(ReplayManifest::parse(&vec![b' '; 8 * 1024 * 1024 + 1]).is_err());
}
#[test]
fn expectation_replay_preserves_capture_bytes_and_rejects_unexpected_edits() {
    let exchanges = manifest().exchanges().unwrap();
    let expected = exchanges.clone();
    let mut replay = ExpectationReplay::new(exchanges).unwrap();
    assert_eq!(replay.mode(), BackendMode::CaptureReplay);
    let mutation = parameter_write_request(
        WireContext {
            group: 0,
            selector: 23,
        },
        ParameterWrite {
            module_id: 3,
            xml_param_id: 0,
            model_index: WireModelIndex(13),
            value: F32Bits::from_f32(51.0),
        },
    )
    .unwrap();
    assert_eq!(
        replay
            .receive(&encode_transfer(&mutation).unwrap()[0], Duration::ZERO)
            .unwrap_err()
            .kind,
        ErrorKind::Conflict
    );
    assert_eq!(replay.remaining(), 2);
    for (index, exchange) in expected.into_iter().enumerate() {
        let outgoing = encode_transfer(&exchange.expected_request)
            .unwrap()
            .concat();
        let mut result = Vec::new();
        for bytes in outgoing.chunks(3) {
            result.extend(
                replay
                    .receive(bytes, Duration::from_millis(index as u64))
                    .unwrap(),
            );
        }
        assert_eq!(
            result
                .into_iter()
                .map(|delivery| match delivery {
                    Delivery::Bytes { bytes, .. } => bytes,
                    _ => panic!("Unexpected disconnect"),
                })
                .collect::<Vec<_>>(),
            exchange
                .response
                .into_iter()
                .map(|chunk| chunk.bytes)
                .collect::<Vec<_>>()
        );
    }
    assert_eq!(replay.remaining(), 0);
    assert!(
        replay
            .receive(
                &encode_transfer(&identify_request()).unwrap()[0],
                Duration::from_millis(3)
            )
            .is_err()
    );
}
#[test]
fn passive_replay_releases_timestamps_and_never_consumes_capture_on_send() {
    let mut passive = PassiveReplay::new(vec![
        CaptureChunk {
            offset: Duration::from_millis(20),
            bytes: vec![0xf8],
        },
        CaptureChunk {
            offset: Duration::from_millis(50),
            bytes: vec![0xf0, 0xf7],
        },
    ])
    .unwrap();
    assert_eq!(passive.mode(), BackendMode::CaptureReplay);
    assert!(passive.advance(Duration::from_millis(19)).is_empty());
    assert_eq!(
        passive.send(&[1, 2, 3]).unwrap_err().kind,
        ErrorKind::Unverified
    );
    assert_eq!(
        passive.advance(Duration::from_millis(20)),
        vec![Delivery::Bytes {
            at: Duration::from_millis(20),
            bytes: vec![0xf8]
        }]
    );
    assert_eq!(passive.advance(Duration::from_millis(100)).len(), 1);
    assert!(passive.advance(Duration::from_millis(200)).is_empty());
}
#[test]
fn replay_backend_declares_its_mode_and_only_offers_explicit_replay_ports() {
    let mut backend = ExpectationReplayBackend::new(manifest().exchanges().unwrap()).unwrap();
    assert_eq!(backend.mode(), BackendMode::CaptureReplay);
    assert_eq!(backend.enumerate().unwrap(), vec![replay_port()]);
}
