use gp200_domain::{EvidenceLevel, Recovered, SnapshotSource};
use gp200_sim::{SimulatedPedal, captured_demo_snapshot, public_demo_snapshot};

#[test]
fn public_demo_changes_only_name_and_records_synthetic_provenance() {
    let captured = captured_demo_snapshot();
    let demo = public_demo_snapshot();
    assert_eq!(demo.source, SnapshotSource::Simulator);
    assert_eq!(
        demo.view.name.value().map(String::as_str),
        Some("Clean Shimmer")
    );
    assert!(matches!(
        &demo.view.name,
        Recovered::Candidate { evidence, .. }
            if evidence.iter().any(|item| item.level == EvidenceLevel::Proposed
                && item.locator.as_ref().is_some_and(|text| text.contains("Synthetic public demo name")))
    ));
    let mut expected = captured.raw.records().to_vec();
    let name = expected
        .iter_mut()
        .find(|record| record.command == 2)
        .unwrap();
    name.body[16..32].copy_from_slice(b"Clean Shimmer\0\0\0");
    assert_eq!(demo.raw.records(), expected);
    assert_eq!(SimulatedPedal::public_demo().current(), demo.raw.as_ref());
    assert_eq!(SimulatedPedal::captured().current(), captured.raw.as_ref());
    assert_eq!(captured_demo_snapshot().raw, captured.raw);
}
