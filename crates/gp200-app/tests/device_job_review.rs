use gp200_app::*;
use gp200_catalog::Catalog;
use gp200_domain::*;
use gp200_protocol::{RawRecord, WRITE_KIND};
use std::{collections::BTreeMap, sync::Arc};

fn identity() -> DeviceIdentity {
    DeviceIdentity {
        model: DeviceModel::Gp200,
        mode: BackendMode::Hardware,
        firmware: None,
        display_name: "GP-200".into(),
    }
}
fn capabilities() -> Capabilities {
    let mut result = Capabilities::gp200_read_only();
    for operation in [OperationKind::IrRead, OperationKind::IrWrite] {
        result.operations.insert(
            operation,
            FeatureSupport::Verified {
                profile: ValidationProfileId("untrusted test claim".into()),
                mode: BackendMode::Hardware,
                firmware: None,
                evidence: vec![],
            },
        );
    }
    result
}
fn session(epoch: u64, revision: u64) -> DeviceJobSession {
    DeviceJobSession::observe_normal(
        SessionEpoch(epoch),
        DeviceSelection {
            input_id: "test-in".into(),
            output_id: "test-out".into(),
            fingerprint_hint: None,
        },
        Revision(revision),
        &RawTransfer::from_records(
            WRITE_KIND,
            &[RawRecord {
                command: 0x2001,
                body: vec![1, 0, 2, 0],
            }],
        )
        .unwrap(),
        &identity(),
        &capabilities(),
    )
    .unwrap()
}
fn app() -> AppController {
    let mut app = AppController::new(Catalog::embedded().unwrap(), AppPreferences::default());
    app.dispatch(AppInput::Device(DeviceEvent::SessionChanged {
        epoch: SessionEpoch(3),
        identity: identity(),
        capabilities: capabilities(),
    }));
    app.dispatch(AppInput::Device(DeviceEvent::DeviceJobSessionObserved(
        session(3, 1),
    )));
    app.dispatch(AppInput::Device(DeviceEvent::ConnectionChanged {
        epoch: SessionEpoch(3),
        state: ConnectionState::Ready,
    }));
    app
}
fn blob(bytes: &[u8]) -> RetainedJobBlob {
    RetainedJobBlob::checked(
        AssetId(JobSha256::of(bytes).hex()),
        bytes.into(),
        JobSha256::of(bytes),
    )
    .unwrap()
}
fn install_preparation(app: &mut AppController) -> JobSha256 {
    let slot = AssetSlot::new(AssetBank::Ir, 19).unwrap();
    let effects = app.dispatch(AppInput::Ui(UiIntent::RefreshAssetMetadata(slot)));
    let Effect::Device(DeviceCommand::ReadAssetMetadata { stamp, .. }) = &effects[0] else {
        panic!("Expected metadata request");
    };
    let mut body = vec![0; 24];
    body[..2].copy_from_slice(&19_u16.to_le_bytes());
    let baseline = Arc::new(
        DeviceAssetMetadata::from_transfer(
            slot,
            *stamp,
            Revision(4),
            SnapshotSource::Hardware { epoch: stamp.epoch },
            1000,
            RawTransfer::from_records(
                WRITE_KIND,
                &[RawRecord {
                    command: 0x1009,
                    body,
                }],
            )
            .unwrap(),
        )
        .unwrap(),
    );
    app.dispatch(AppInput::Device(DeviceEvent::AssetMetadataReceived {
        stamp: *stamp,
        metadata: Arc::clone(&baseline),
    }));
    let wav = include_bytes!("../../../tests/fixtures/device-assets/synthetic-ir-pcm24.wav");
    let coefficients =
        include_bytes!("../../../tests/fixtures/device-assets/synthetic-ir-coefficients.bin");
    let job = PreparedDeviceJob::prepare(
        JobId(7),
        session(3, 1),
        blob(wav),
        NormalAssetRecipe::ir(
            slot,
            "Reviewed IR".into(),
            blob(wav),
            IrJobLengthPolicy::RejectLong,
            blob(coefficients),
        )
        .unwrap(),
        JobConversion::checked(
            "gp200-ir-44100-pcm24-v1".into(),
            Arc::from(b"{}".as_slice()),
            JobSha256::of(b"{}"),
            BTreeMap::new(),
        )
        .unwrap(),
        baseline,
        AssetBackupReview::Unavailable {
            reason: "Metadata only".into(),
        },
    )
    .unwrap();
    let digest = job.review_sha256();
    app.dispatch(AppInput::Assets(AssetEvent::DeviceJobPrepared(job)));
    digest
}

#[test]
fn stale_session_observations_and_changed_capabilities_cannot_replace_actor_context() {
    let mut app = app();
    let current = app.state().device_job_session.clone().unwrap();
    app.dispatch(AppInput::Device(DeviceEvent::DeviceJobSessionObserved(
        session(2, 8),
    )));
    assert_eq!(app.state().device_job_session.as_ref(), Some(&current));
    let mut changed = capabilities();
    changed.operations.remove(&OperationKind::IrRead);
    app.dispatch(AppInput::Device(DeviceEvent::SessionChanged {
        epoch: SessionEpoch(3),
        identity: identity(),
        capabilities: changed,
    }));
    assert!(app.state().device_job_session.is_none());
    app.dispatch(AppInput::Device(DeviceEvent::DeviceJobSessionObserved(
        session(3, 1),
    )));
    assert!(app.state().device_job_session.is_none());
}
#[test]
fn reviewed_preparation_never_submits_on_forged_capability_or_old_review() {
    let mut app = app();
    let digest = install_preparation(&mut app);
    assert!(
        app.execute_prepared_job(JobId(7), JobSha256::of(b"old review"))
            .is_empty()
    );
    assert!(app.execute_prepared_job(JobId(7), digest).is_empty());
    assert!(!app.device_operation_pending());
    assert!(app.state().prepared_jobs.contains_key(&JobId(7)));
    app.dispatch(AppInput::Device(DeviceEvent::ConnectionChanged {
        epoch: SessionEpoch(4),
        state: ConnectionState::Disconnected,
    }));
    assert!(app.state().device_job_session.is_none());
    assert!(app.execute_prepared_job(JobId(7), digest).is_empty());
}
