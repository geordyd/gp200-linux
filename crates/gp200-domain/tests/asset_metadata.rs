use gp200_domain::*;
use gp200_protocol::{RawRecord, WRITE_KIND};
use std::sync::Arc;

fn slot(index: u16) -> AssetSlot {
    AssetSlot::new(AssetBank::Ir, index).unwrap()
}
fn stamp(intent: u64) -> RequestStamp {
    RequestStamp {
        epoch: SessionEpoch(4),
        intent: IntentId(intent),
    }
}
fn fixture(index: usize) -> RawTransfer {
    let bank = RawTransfer::from_payload(
        WRITE_KIND,
        include_bytes!("../../../tests/fixtures/device-assets/connected-ir-metadata.payload.bin")
            .to_vec(),
    )
    .unwrap();
    RawTransfer::from_records(WRITE_KIND, &bank.records()[index..index + 1]).unwrap()
}
fn metadata(index: u16, intent: u64, revision: u64) -> Arc<DeviceAssetMetadata> {
    Arc::new(
        DeviceAssetMetadata::from_transfer(
            slot(index),
            stamp(intent),
            Revision(revision),
            SnapshotSource::Hardware {
                epoch: SessionEpoch(4),
            },
            123,
            fixture(usize::from(index)),
        )
        .unwrap(),
    )
}
#[test]
fn all20_captured_slots_keep_every_opaque_byte_without_occupancy_inference() {
    for index in 0..20 {
        let value = metadata(index, 1, 1);
        assert_eq!(value.slot(), slot(index));
        assert_eq!(value.metadata().display_name(), "User IR");
        assert_eq!(value.metadata().flags_raw, 0);
        assert_eq!(value.metadata().checksum_raw, 0);
        assert_eq!(
            value.metadata().name_raw[8..],
            [0x3c, 0x54, 0x3b, 0x80, 0xff, 0xff, 0xff, 0xff]
        );
        assert_eq!(**value.raw(), fixture(usize::from(index)));
    }
}
#[test]
fn snapshot_rejects_wrong_bank_slot_kind_duplicate_status_and_truncated_record() {
    let make = |target, raw| {
        DeviceAssetMetadata::from_transfer(
            target,
            stamp(1),
            Revision(1),
            SnapshotSource::Simulator,
            0,
            raw,
        )
    };
    assert!(make(slot(0), fixture(1)).is_err());
    assert!(make(AssetSlot::new(AssetBank::SnapTone, 0).unwrap(), fixture(0)).is_err());
    let record = fixture(0).records()[0].clone();
    assert!(
        make(
            slot(0),
            RawTransfer::from_records(gp200_protocol::READ_KIND, std::slice::from_ref(&record))
                .unwrap()
        )
        .is_err()
    );
    assert!(
        make(
            slot(0),
            RawTransfer::from_records(WRITE_KIND, &[record.clone(), record.clone()]).unwrap()
        )
        .is_err()
    );
    assert!(
        make(
            slot(0),
            RawTransfer::from_records(
                WRITE_KIND,
                &[RawRecord {
                    command: 0x100b,
                    body: vec![0; 8]
                }]
            )
            .unwrap()
        )
        .is_err()
    );
    assert!(
        make(
            slot(0),
            RawTransfer::from_records(
                WRITE_KIND,
                &[RawRecord {
                    command: record.command,
                    body: record.body[..23].to_vec()
                }]
            )
            .unwrap()
        )
        .is_err()
    );
    assert!(
        DeviceAssetMetadata::from_transfer(
            slot(0),
            stamp(1),
            Revision(1),
            SnapshotSource::Hardware {
                epoch: SessionEpoch(3)
            },
            0,
            fixture(0)
        )
        .is_err()
    );
}
#[test]
fn cache_requires_exact_pending_stamp_revision_and_session() {
    let mut cache = DeviceAssetInventory::new(SessionEpoch(4));
    assert!(!cache.complete(metadata(0, 1, 1)));
    assert!(cache.begin(slot(0), stamp(1)));
    assert!(!cache.complete(metadata(1, 1, 1)));
    assert!(!cache.complete(metadata(0, 2, 1)));
    assert!(cache.complete(metadata(0, 1, 1)));
    assert_eq!(cache.complete_count(AssetBank::Ir), 1);
    assert!(cache.begin(slot(0), stamp(2)));
    assert!(!cache.complete(metadata(0, 2, 1)));
    assert!(cache.complete(metadata(0, 2, 2)));
    cache.invalidate(SessionEpoch(5));
    assert_eq!(cache.entry(slot(0)).status, AssetMetadataReadStatus::Stale);
    assert_eq!(
        cache
            .entry(slot(0))
            .snapshot
            .as_ref()
            .unwrap()
            .stamp()
            .epoch,
        SessionEpoch(4)
    );
    assert_eq!(cache.complete_count(AssetBank::Ir), 0);
    assert!(!cache.begin(slot(1), stamp(3)));
    assert!(!cache.complete(metadata(0, 2, 3)));
}
#[test]
fn cache_preserves_timeout_reason_and_stale_data_is_not_complete() {
    let mut cache = DeviceAssetInventory::new(SessionEpoch(4));
    cache.begin(slot(3), stamp(1));
    assert!(!cache.fail(
        slot(3),
        stamp(2),
        ServiceError::new(ErrorKind::Timeout, "no reply")
    ));
    assert!(cache.fail(
        slot(3),
        stamp(1),
        ServiceError::new(ErrorKind::Timeout, "no reply")
    ));
    cache.invalidate(SessionEpoch(5));
    assert!(
        matches!(&cache.entry(slot(3)).status, AssetMetadataReadStatus::Failed { error, .. } if error.kind == ErrorKind::Timeout)
    );
}
