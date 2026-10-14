use gp200_domain::*;
use gp200_protocol::{RawRecord, WRITE_KIND};
use std::{collections::BTreeMap, sync::Arc};

const WAV: &[u8] = include_bytes!("../../../tests/fixtures/device-assets/synthetic-ir-pcm24.wav");
const COEFFICIENTS: &[u8] =
    include_bytes!("../../../tests/fixtures/device-assets/synthetic-ir-coefficients.bin");
const CLO: &[u8] = include_bytes!("../../../tests/fixtures/assets/clo/upstream-source2048.clo");
const PADDED_CLO: &[u8] =
    include_bytes!("../../../tests/fixtures/assets/clo/upstream-final1024.clo");
fn blob(bytes: &[u8]) -> RetainedJobBlob {
    RetainedJobBlob::checked(
        AssetId(JobSha256::of(bytes).hex()),
        bytes.into(),
        JobSha256::of(bytes),
    )
    .unwrap()
}
fn identity() -> DeviceIdentity {
    DeviceIdentity {
        model: DeviceModel::Gp200,
        mode: BackendMode::Hardware,
        firmware: None,
        display_name: "GP-200".into(),
    }
}
fn raw_identity() -> RawTransfer {
    RawTransfer::from_records(
        WRITE_KIND,
        &[RawRecord {
            command: 0x2001,
            body: vec![1, 0, 2, 0],
        }],
    )
    .unwrap()
}
fn selection() -> DeviceSelection {
    DeviceSelection {
        input_id: "hw:GP200,0,0".into(),
        output_id: "hw:GP200,0,0".into(),
        fingerprint_hint: None,
    }
}
fn session(capabilities: &Capabilities) -> DeviceJobSession {
    DeviceJobSession::observe_normal(
        SessionEpoch(7),
        selection(),
        Revision(1),
        &raw_identity(),
        &identity(),
        capabilities,
    )
    .unwrap()
}
fn slot() -> AssetSlot {
    AssetSlot::new(AssetBank::Ir, 19).unwrap()
}
fn baseline(slot: AssetSlot, epoch: SessionEpoch, revision: Revision) -> Arc<DeviceAssetMetadata> {
    let mut body = vec![0; 24];
    body[..2].copy_from_slice(&slot.index().to_le_bytes());
    body[8..15].copy_from_slice(b"User IR");
    Arc::new(
        DeviceAssetMetadata::from_transfer(
            slot,
            RequestStamp {
                epoch,
                intent: IntentId(1),
            },
            revision,
            SnapshotSource::Hardware { epoch },
            1000,
            RawTransfer::from_records(
                WRITE_KIND,
                &[RawRecord {
                    command: slot.bank().metadata_command(),
                    body,
                }],
            )
            .unwrap(),
        )
        .unwrap(),
    )
}
fn recipe() -> NormalAssetRecipe {
    NormalAssetRecipe::ir(
        slot(),
        "Reviewed IR".into(),
        blob(WAV),
        IrJobLengthPolicy::RejectLong,
        blob(COEFFICIENTS),
    )
    .unwrap()
}
fn conversion(revision: &str, options: &[u8]) -> JobConversion {
    JobConversion::checked(
        revision.into(),
        options.into(),
        JobSha256::of(options),
        BTreeMap::new(),
    )
    .unwrap()
}
fn prepared(capabilities: &Capabilities, backup: AssetBackupReview) -> PreparedDeviceJob {
    PreparedDeviceJob::prepare(
        JobId(5),
        session(capabilities),
        blob(WAV),
        recipe(),
        conversion("gp200-ir-44100-pcm24-v1", b"{\"truncate\":false}"),
        baseline(slot(), SessionEpoch(7), Revision(4)),
        backup,
    )
    .unwrap()
}
fn no_backup() -> AssetBackupReview {
    AssetBackupReview::Unavailable {
        reason: "Only metadata is readable".into(),
    }
}

#[test]
fn hashes_are_fixed_size_and_checked_against_bytes() {
    assert!(JobSha256::parse("anything").is_err());
    assert!(JobSha256::parse(&"é".repeat(32)).is_err());
    assert_eq!(
        JobSha256::parse(&JobSha256::of(WAV).hex()).unwrap(),
        JobSha256::of(WAV)
    );
    assert!(
        RetainedJobBlob::checked(
            AssetId("wav".into()),
            WAV.into(),
            JobSha256::of(COEFFICIENTS)
        )
        .is_err()
    );
    assert!(RetainedJobBlob::checked(AssetId("".into()), WAV.into(), JobSha256::of(WAV)).is_err());
    assert!(
        RetainedJobBlob::checked(AssetId("empty".into()), Arc::from([]), JobSha256::of(&[]))
            .is_err()
    );
}
#[test]
fn independent_ir_repacking_rejects_wrong_bank_and_forged_coefficients() {
    let wrong = AssetSlot::new(AssetBank::SnapTone, 9).unwrap();
    assert!(
        NormalAssetRecipe::ir(
            wrong,
            "IR".into(),
            blob(WAV),
            IrJobLengthPolicy::RejectLong,
            blob(COEFFICIENTS)
        )
        .is_err()
    );
    let mut corrupted = COEFFICIENTS.to_vec();
    corrupted[3] ^= 1;
    assert!(
        NormalAssetRecipe::ir(
            slot(),
            "IR".into(),
            blob(WAV),
            IrJobLengthPolicy::RejectLong,
            blob(&corrupted)
        )
        .is_err()
    );
    assert!(
        NormalAssetRecipe::ir(
            slot(),
            "ø".into(),
            blob(WAV),
            IrJobLengthPolicy::RejectLong,
            blob(COEFFICIENTS)
        )
        .is_err()
    );
    assert_eq!(recipe().derived(), COEFFICIENTS);
}
#[test]
fn wav_constraints_and_truncation_require_the_explicit_recipe() {
    let mut wrong_rate = WAV.to_vec();
    wrong_rate[24..28].copy_from_slice(&48000_u32.to_le_bytes());
    assert!(
        NormalAssetRecipe::ir(
            slot(),
            "IR".into(),
            blob(&wrong_rate),
            IrJobLengthPolicy::RejectLong,
            blob(COEFFICIENTS)
        )
        .is_err()
    );
    let mut long = WAV[..44].to_vec();
    long.extend_from_slice(&vec![0; 3075]);
    long.push(0);
    long[4..8].copy_from_slice(&3112_u32.to_le_bytes());
    long[40..44].copy_from_slice(&3075_u32.to_le_bytes());
    assert!(
        NormalAssetRecipe::ir(
            slot(),
            "IR".into(),
            blob(&long),
            IrJobLengthPolicy::RejectLong,
            blob(&vec![0; 4096])
        )
        .is_err()
    );
    assert!(
        NormalAssetRecipe::ir(
            slot(),
            "IR".into(),
            blob(&long),
            IrJobLengthPolicy::KeepFirst1024,
            blob(&vec![0; 4096])
        )
        .is_ok()
    );
}
#[test]
fn complete_clo_and_exact_native_prefix_are_required() {
    let slot = AssetSlot::new(AssetBank::SnapTone, 9).unwrap();
    for (bytes, profile) in [
        (CLO, NativeCloProfile::NativeA128B2048Layout),
        (PADDED_CLO, NativeCloProfile::IndependentA128B1024Padded),
    ] {
        let recipe = NormalAssetRecipe::native_clo(
            slot,
            "Reviewed SC".into(),
            blob(bytes),
            blob(&bytes[..8192]),
        )
        .unwrap();
        assert_eq!(recipe.clo_profile(), Some(profile));
        assert_eq!(recipe.derived(), &bytes[..8192]);
    }
    assert!(
        NormalAssetRecipe::native_clo(slot, "SC".into(), blob(&CLO[..8192]), blob(&CLO[..8192]))
            .is_err()
    );
    let mut corrupted = CLO.to_vec();
    corrupted[104] ^= 1;
    assert!(
        NormalAssetRecipe::native_clo(
            slot,
            "SC".into(),
            blob(&corrupted),
            blob(&corrupted[..8192])
        )
        .is_err()
    );
    assert!(
        NormalAssetRecipe::native_clo(slot, "SC".into(), blob(CLO), blob(&PADDED_CLO[..8192]))
            .is_err()
    );
}
#[test]
fn normal_identity_cannot_invent_firmware_or_accept_unknown_mode() {
    let caps = Capabilities::gp200_read_only();
    let mut raw = raw_identity().records().to_vec();
    raw[0].body[0] = 2;
    assert!(
        DeviceJobSession::observe_normal(
            SessionEpoch(7),
            selection(),
            Revision(1),
            &RawTransfer::from_records(WRITE_KIND, &raw).unwrap(),
            &identity(),
            &caps
        )
        .is_err()
    );
    assert!(
        DeviceJobSession::observe_normal(
            SessionEpoch(7),
            selection(),
            Revision(0),
            &raw_identity(),
            &identity(),
            &caps
        )
        .is_err()
    );
    let mut claimed = identity();
    claimed.firmware = Some(FirmwareIdentity {
        version: "1.8.0".into(),
        raw: vec![],
        evidence: vec![],
    });
    assert!(
        DeviceJobSession::observe_normal(
            SessionEpoch(7),
            selection(),
            Revision(1),
            &raw_identity(),
            &claimed,
            &caps
        )
        .is_err()
    );
}
#[test]
fn wrong_source_codec_options_and_metadata_session_are_rejected() {
    let caps = Capabilities::gp200_read_only();
    for (source, codec, baseline) in [
        (
            blob(COEFFICIENTS),
            "gp200-ir-44100-pcm24-v1",
            baseline(slot(), SessionEpoch(7), Revision(4)),
        ),
        (
            blob(WAV),
            "unreviewed-codec",
            baseline(slot(), SessionEpoch(7), Revision(4)),
        ),
        (
            blob(WAV),
            "gp200-ir-44100-pcm24-v1",
            baseline(slot(), SessionEpoch(8), Revision(4)),
        ),
    ] {
        assert!(
            PreparedDeviceJob::prepare(
                JobId(5),
                session(&caps),
                source,
                recipe(),
                conversion(codec, b"{}"),
                baseline,
                no_backup()
            )
            .is_err()
        );
    }
    assert!(
        JobConversion::checked(
            "codec".into(),
            Arc::from(b"{}".as_slice()),
            JobSha256::of(b"[]"),
            BTreeMap::new()
        )
        .is_err()
    );
    let other_bank = AssetSlot::new(AssetBank::SnapTone, 0).unwrap();
    assert!(
        PreparedDeviceJob::prepare(
            JobId(5),
            session(&caps),
            blob(WAV),
            recipe(),
            conversion("gp200-ir-44100-pcm24-v1", b"{}"),
            baseline(other_bank, SessionEpoch(7), Revision(4)),
            no_backup()
        )
        .is_err()
    );
}
#[test]
fn reconnect_endpoint_identity_revision_and_capability_changes_invalidate_review() {
    let caps = Capabilities::gp200_read_only();
    let job = prepared(&caps, no_backup());
    for (epoch, endpoint, revision) in [
        (8, "hw:GP200,0,0", 1),
        (7, "another-port", 1),
        (7, "hw:GP200,0,0", 2),
    ] {
        let mut selection = selection();
        selection.input_id = endpoint.into();
        let active = DeviceJobSession::observe_normal(
            SessionEpoch(epoch),
            selection,
            Revision(revision),
            &raw_identity(),
            &identity(),
            &caps,
        )
        .unwrap();
        assert_eq!(
            job.recheck_active(&active, &identity(), &caps, job.baseline())
                .unwrap_err()
                .kind,
            ErrorKind::Conflict
        );
    }
    let mut changed = caps.clone();
    changed.operations.remove(&OperationKind::ReadEditBuffer);
    assert_eq!(
        job.recheck_active(job.session(), &identity(), &changed, job.baseline())
            .unwrap_err()
            .kind,
        ErrorKind::Conflict
    );
}
#[test]
fn target_metadata_revision_is_rechecked_and_never_just_a_name() {
    let caps = Capabilities::gp200_read_only();
    let job = prepared(&caps, no_backup());
    assert!(
        job.recheck_active(job.session(), &identity(), &caps, job.baseline())
            .is_ok()
    );
    assert_eq!(
        job.recheck_active(
            job.session(),
            &identity(),
            &caps,
            &baseline(slot(), SessionEpoch(7), Revision(5))
        )
        .unwrap_err()
        .kind,
        ErrorKind::Conflict
    );
    let mut records = job.baseline().raw().records().to_vec();
    records[0].body[3] = 1;
    let altered = DeviceAssetMetadata::from_transfer(
        slot(),
        job.baseline().stamp(),
        Revision(4),
        SnapshotSource::Hardware {
            epoch: SessionEpoch(7),
        },
        1000,
        RawTransfer::from_records(WRITE_KIND, &records).unwrap(),
    )
    .unwrap();
    assert!(
        job.recheck_active(job.session(), &identity(), &caps, &altered)
            .is_err()
    );
}
#[test]
fn metadata_zero_checksum_and_user_ir_name_cannot_establish_backup_or_authority() {
    let mut caps = Capabilities::gp200_read_only();
    caps.operations.insert(
        OperationKind::IrWrite,
        FeatureSupport::Verified {
            profile: ValidationProfileId("forged-write-profile".into()),
            mode: BackendMode::Hardware,
            firmware: None,
            evidence: vec![EvidenceRef::new("a claim", EvidenceLevel::LiveVerified)],
        },
    );
    let baseline = baseline(slot(), SessionEpoch(7), Revision(4));
    let job = prepared(
        &caps,
        AssetBackupReview::RetainedOriginal {
            content: blob(COEFFICIENTS),
            baseline_sha256: JobSha256::of(baseline.raw().payload()),
        },
    );
    let review = job.review(&caps);
    assert!(
        review
            .blockers
            .contains(&DeviceJobBlocker::FullContentBackupUnverified)
    );
    assert!(
        review
            .blockers
            .contains(&DeviceJobBlocker::TransferReadbackRecoveryProfileMissing)
    );
    assert_eq!(
        ValidatedDeviceJob::validate(
            job.clone(),
            job.review_sha256(),
            job.session(),
            &identity(),
            &caps,
            &baseline
        )
        .unwrap_err()
        .kind,
        ErrorKind::Unverified
    );
}
#[test]
fn retained_original_must_match_exact_baseline_and_bank_length() {
    let caps = Capabilities::gp200_read_only();
    for content in [blob(WAV), blob(COEFFICIENTS)] {
        let backup = AssetBackupReview::RetainedOriginal {
            content,
            baseline_sha256: JobSha256::of(b"different metadata"),
        };
        assert!(
            PreparedDeviceJob::prepare(
                JobId(5),
                session(&caps),
                blob(WAV),
                recipe(),
                conversion("gp200-ir-44100-pcm24-v1", b"{}"),
                baseline(slot(), SessionEpoch(7), Revision(4)),
                backup
            )
            .is_err()
        );
    }
}
#[test]
fn canonical_review_binds_options_and_source_reproducibly() {
    let caps = Capabilities::gp200_read_only();
    let first = prepared(&caps, no_backup()).review(&caps);
    assert_eq!(
        first.review_sha256,
        prepared(&caps, no_backup()).review(&caps).review_sha256
    );
    let changed = PreparedDeviceJob::prepare(
        JobId(5),
        session(&caps),
        blob(WAV),
        recipe(),
        conversion("gp200-ir-44100-pcm24-v1", b"different options"),
        baseline(slot(), SessionEpoch(7), Revision(4)),
        no_backup(),
    )
    .unwrap();
    assert_ne!(first.review_sha256, changed.review(&caps).review_sha256);
}

#[test]
fn an_older_review_digest_cannot_authorize_a_replaced_preparation() {
    let caps = Capabilities::gp200_read_only();
    let job = prepared(&caps, no_backup());
    assert_eq!(
        ValidatedDeviceJob::validate(
            job.clone(),
            JobSha256::of(b"older reviewed job"),
            job.session(),
            &identity(),
            &caps,
            job.baseline()
        )
        .unwrap_err()
        .kind,
        ErrorKind::Conflict
    );
    assert!(std::mem::size_of::<PreparedDeviceJob>() <= 16);
}
