use gp200_domain::*;
use gp200_library::*;
use std::{fs, path::PathBuf, sync::Arc};
struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        let p = std::env::temp_dir().join(format!(
            "gp200-firmware-public-{}-{}",
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
fn job() -> Arc<ReviewedFirmwareUpdate> {
    let b: &[u8] = include_bytes!("../../../tests/fixtures/device-assets/synthetic-firmware.bin");
    let c = FirmwareUpdateContextDto {
        profile_id: "imported profile claim".into(),
        hardware_policy: "unaccepted".into(),
        update_mode: "observation".into(),
        normal: None,
        versions: vec![],
        backup_objects: vec![],
        loss_items: vec![],
        power_statement: "claim".into(),
        connection_statement: "claim".into(),
    };
    let d = [
        FirmwareMaterialKind::Profile,
        FirmwareMaterialKind::Evidence,
        FirmwareMaterialKind::BackupArchive,
        FirmwareMaterialKind::BackupInventory,
        FirmwareMaterialKind::RestorePlan,
        FirmwareMaterialKind::Recovery,
        FirmwareMaterialKind::ApprovalStatement,
    ]
    .into_iter()
    .map(|kind| FirmwareMaterialInput {
        kind,
        bytes: Arc::from(&b"untrusted imported claim"[..]),
    })
    .collect();
    Arc::new(
        ReviewedFirmwareUpdate::prepare(
            RetainedJobBlob::checked(AssetId("source".into()), b.into(), JobSha256::of(b)).unwrap(),
            c,
            d,
            128,
        )
        .unwrap(),
    )
}
#[test]
fn public_reopen_retains_closure_separately_from_live_permission() {
    let t = Temp::new();
    let id;
    {
        let mut s = LibraryStore::open(&t.0).unwrap();
        let mut l = FirmwareAttemptLedger::open(&mut s).unwrap();
        id = l.reserve_id().unwrap();
        let (c, r) = l
            .begin_reserved(&id, job(), FirmwareObservationOrigin::LocalObservation)
            .unwrap();
        assert!(!r.permits_flash());
        l.close(c, FirmwareHostClosure::CancelledBeforeEntry)
            .unwrap();
    }
    let mut s = LibraryStore::open(&t.0).unwrap();
    let mut l = FirmwareAttemptLedger::open(&mut s).unwrap();
    let r = l.load(&id).unwrap();
    assert!(r.issues.is_empty());
    assert_eq!(
        r.projection.unwrap().recorded_closure(),
        Some(&FirmwareHostClosure::CancelledBeforeEntry)
    );
    assert!(
        l.begin_reserved(&id, job(), FirmwareObservationOrigin::LocalObservation)
            .is_err()
    );
}
#[test]
fn imported_exact_raw_evidence_never_issues_a_cursor_or_checkpoint() {
    let t = Temp::new();
    let id;
    let raw;
    {
        let mut s = LibraryStore::open(&t.0).unwrap();
        let mut l = FirmwareAttemptLedger::open(&mut s).unwrap();
        id = l.reserve_id().unwrap();
        let (c, _) = l
            .begin_reserved(&id, job(), FirmwareObservationOrigin::LocalObservation)
            .unwrap();
        l.close(c, FirmwareHostClosure::ObservationFinished)
            .unwrap();
        raw = fs::read(
            t.0.join("firmware-attempt-ledger")
                .join(format!("{}.journal", id.as_str())),
        )
        .unwrap();
    }
    let other = Temp::new();
    let mut s = LibraryStore::open(&other.0).unwrap();
    let mut l = FirmwareAttemptLedger::open(&mut s).unwrap();
    let retained = l.import_observation(id.clone(), job(), &raw).unwrap();
    assert!(!retained.can_resume());
    assert!(!retained.permits_flash());
    assert!(!retained.local_checkpoint_confirmed);
    assert_eq!(
        fs::read(
            other
                .0
                .join("firmware-attempt-ledger")
                .join(format!("{}.journal", id.as_str()))
        )
        .unwrap(),
        raw
    );
    assert!(
        l.begin_reserved(&id, job(), FirmwareObservationOrigin::LocalObservation)
            .is_err()
    );
}

#[test]
fn imported_epoch_dto_can_describe_a_role_but_cannot_reopen_a_live_writer() {
    let t = Temp::new();
    let j = job();
    let id;
    {
        let mut store = LibraryStore::open(&t.0).unwrap();
        let mut ledger = FirmwareAttemptLedger::open(&mut store).unwrap();
        id = ledger.reserve_id().unwrap();
        let (c, _) = ledger
            .begin_reserved(&id, j.clone(), FirmwareObservationOrigin::LocalObservation)
            .unwrap();
        let plan = FirmwareLifecyclePlanDto {
            review_sha256: j.digest().hex(),
            profile_sha256: JobSha256::of(b"untrusted descriptive rule").hex(),
            origin: FirmwareObservationOrigin::LocalObservation,
            nonblock_actions: 1,
            epoch: SessionEpoch(1),
            binding: FirmwareUsbObservation {
                topology: "description".into(),
                vendor: 1,
                product: 2,
                serial: "description".into(),
                interface: "description".into(),
                endpoint: "description".into(),
            },
            generation: 1,
            clock_origin: "description".into(),
        };
        let c = ledger.prepare_lifecycle(c, plan.clone()).unwrap();
        let epoch = FirmwareLifecycleEpochObservation {
            role: FirmwareLifecycleEpochRole::NormalEntry,
            epoch: plan.epoch,
            generation: 1,
            binding: plan.binding,
            previous_binding_sha256: None,
            previous_epoch: None,
            source_instance: "imported source claim".into(),
            clock_origin: plan.clock_origin,
            retired_source: None,
            raw_identity: vec![1],
            negotiation: vec![],
            transition: None,
            post_normal_refresh: None,
        };
        let imported: FirmwareLifecycleEpochObservation =
            serde_json::from_slice(&epoch.retained_json().unwrap()).unwrap();
        assert_eq!(imported, epoch);
        let (c, checkpoint) = ledger.lifecycle_epoch(c, imported).unwrap();
        assert!(!checkpoint.permits_flash());
        ledger
            .lifecycle_close(c, FirmwareLifecycleHostClosure::ClosedBeforeAnyOffer)
            .unwrap();
    }
    let mut store = LibraryStore::open(&t.0).unwrap();
    let mut ledger = FirmwareAttemptLedger::open(&mut store).unwrap();
    let r = ledger.load(&id).unwrap();
    assert_eq!(r.lifecycle.as_ref().unwrap().epochs().len(), 1);
    assert!(!r.can_resume());
    assert!(!r.permits_flash());
    assert!(
        ledger
            .begin_reserved(&id, j, FirmwareObservationOrigin::LocalObservation)
            .is_err()
    );
}
