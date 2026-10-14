use super::*;
use std::sync::atomic::AtomicU64;
const WAV: &[u8] = include_bytes!("../../../tests/fixtures/device-assets/synthetic-ir-pcm24.wav");
const COEFFICIENTS: &[u8] =
    include_bytes!("../../../tests/fixtures/device-assets/synthetic-ir-coefficients.bin");
const CLO: &[u8] = include_bytes!("../../../tests/fixtures/assets/clo/upstream-source2048.clo");
const PADDED: &[u8] = include_bytes!("../../../tests/fixtures/assets/clo/upstream-final1024.clo");
static NEXT: AtomicU64 = AtomicU64::new(0);
struct Directory(PathBuf);
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn directory() -> Directory {
    let path = std::env::temp_dir().join(format!(
        "gp200-asset-job-worker-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir(&path).unwrap();
    Directory(path)
}
fn selection(path: PathBuf, bank: AssetBank) -> SourceSelection {
    SourceSelection {
        path,
        expected_sha256: None,
        bank,
        slot: bank.slots() - 1,
        name: "Reviewed".into(),
        truncate: false,
        provenance: BTreeMap::new(),
    }
}
fn binding(slot: AssetSlot) -> LiveBinding {
    let identity = DeviceIdentity {
        model: DeviceModel::Gp200,
        mode: BackendMode::Simulator,
        firmware: None,
        display_name: "Synthetic job review fixture".into(),
    };
    let caps = Capabilities::simulator();
    let session = DeviceJobSession::observe_normal(
        SessionEpoch(5),
        DeviceSelection {
            input_id: "fixture-input".into(),
            output_id: "fixture-output".into(),
            fingerprint_hint: None,
        },
        Revision(1),
        &RawTransfer::from_records(
            gp200_protocol::WRITE_KIND,
            &[gp200_protocol::RawRecord {
                command: 0x2001,
                body: vec![1, 0, 2, 0],
            }],
        )
        .unwrap(),
        &identity,
        &caps,
    )
    .unwrap();
    let mut body = vec![0; 24];
    body[..2].copy_from_slice(&slot.index().to_le_bytes());
    body[8..15].copy_from_slice(b"User IR");
    let baseline = Arc::new(
        DeviceAssetMetadata::from_transfer(
            slot,
            RequestStamp {
                epoch: SessionEpoch(5),
                intent: IntentId(2),
            },
            Revision(3),
            SnapshotSource::Simulator,
            10,
            RawTransfer::from_records(
                gp200_protocol::WRITE_KIND,
                &[gp200_protocol::RawRecord {
                    command: slot.bank().metadata_command(),
                    body,
                }],
            )
            .unwrap(),
        )
        .unwrap(),
    );
    LiveBinding { session, baseline }
}
#[test]
fn exact_ir_and_two_clo_profiles_are_prepared_with_private_bindings_and_no_transfer_authority() {
    for (bank, bytes, derived) in [
        (AssetBank::Ir, WAV, COEFFICIENTS),
        (AssetBank::SnapTone, CLO, &CLO[..8192]),
        (AssetBank::SnapTone, PADDED, &PADDED[..8192]),
    ] {
        let dir = directory();
        let path = dir.0.join("source");
        fs::write(&path, bytes).unwrap();
        let selection = selection(path, bank);
        let local = inspect(selection, &AtomicBool::new(false)).unwrap();
        assert_eq!(local.recipe.derived(), derived);
        let job = prepare(
            &local,
            binding(local.recipe.target()),
            JobId(9),
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(
            job.review(&Capabilities::simulator()).source_sha256,
            JobSha256::of(bytes)
        );
        assert!(
            job.review(&Capabilities::simulator())
                .blockers
                .contains(&DeviceJobBlocker::FullContentBackupUnverified)
        );
        assert!(
            ValidatedDeviceJob::validate(
                job.clone(),
                job.review_sha256(),
                job.session(),
                job.session().identity(),
                &Capabilities::simulator(),
                job.baseline()
            )
            .is_err()
        );
    }
}
#[test]
fn replaced_file_and_claimed_inventory_hash_cannot_inherit_inspected_review() {
    let dir = directory();
    let path = dir.0.join("source");
    fs::write(&path, WAV).unwrap();
    let mut selected = selection(path.clone(), AssetBank::Ir);
    selected.expected_sha256 = Some(JobSha256::of(CLO));
    assert!(
        inspect(selected.clone(), &AtomicBool::new(false))
            .unwrap_err()
            .contains("inventory identity")
    );
    selected.expected_sha256 = Some(JobSha256::of(WAV));
    let local = inspect(selected, &AtomicBool::new(false)).unwrap();
    let mut replacement = WAV.to_vec();
    let last = replacement.len() - 2;
    replacement[last] ^= 1;
    fs::write(path, replacement).unwrap();
    assert!(
        prepare(
            &local,
            binding(local.recipe.target()),
            JobId(9),
            &AtomicBool::new(false)
        )
        .unwrap_err()
        .contains("replaced after inspection")
    );
}
#[test]
fn unknown_slots_bad_names_and_unsupported_pcm_do_not_prepare() {
    let dir = directory();
    let path = dir.0.join("source");
    fs::write(&path, WAV).unwrap();
    let mut selected = selection(path.clone(), AssetBank::Ir);
    selected.slot = 20;
    assert!(inspect(selected.clone(), &AtomicBool::new(false)).is_err());
    selected.slot = 0;
    selected.name = "音色".into();
    assert!(inspect(selected.clone(), &AtomicBool::new(false)).is_err());
    selected.name = "IR".into();
    let mut wrong = WAV.to_vec();
    wrong[24..28].copy_from_slice(&48000_u32.to_le_bytes());
    fs::write(path, wrong).unwrap();
    assert!(inspect(selected, &AtomicBool::new(false)).is_err());
}
#[test]
fn cancellation_and_symlinks_are_rejected_before_retaining_material() {
    let dir = directory();
    let path = dir.0.join("source");
    fs::write(&path, WAV).unwrap();
    assert!(
        inspect(
            selection(path.clone(), AssetBank::Ir),
            &AtomicBool::new(true)
        )
        .unwrap_err()
        .contains("cancelled")
    );
    #[cfg(unix)]
    {
        let link = dir.0.join("link");
        std::os::unix::fs::symlink(path, &link).unwrap();
        assert!(
            inspect(selection(link, AssetBank::Ir), &AtomicBool::new(false))
                .unwrap_err()
                .contains("symbolic links")
        );
    }
}
#[test]
fn stale_epoch_or_target_baseline_cannot_seal_a_job() {
    let dir = directory();
    let path = dir.0.join("source");
    fs::write(&path, WAV).unwrap();
    let local = inspect(selection(path, AssetBank::Ir), &AtomicBool::new(false)).unwrap();
    let mut wrong = binding(local.recipe.target());
    wrong.baseline = binding(AssetSlot::new(AssetBank::Ir, 0).unwrap()).baseline;
    assert!(prepare(&local, wrong, JobId(8), &AtomicBool::new(false)).is_err());
}
#[test]
fn long_ir_requires_explicit_truncation_and_crc_corruption_blocks_clo() {
    let dir = directory();
    let path = dir.0.join("long.wav");
    let data = vec![0; 1025 * 3];
    let mut wav = b"RIFF".to_vec();
    wav.extend((36 + data.len() as u32).to_le_bytes());
    wav.extend(b"WAVEfmt ");
    wav.extend(16_u32.to_le_bytes());
    wav.extend(1_u16.to_le_bytes());
    wav.extend(1_u16.to_le_bytes());
    wav.extend(44100_u32.to_le_bytes());
    wav.extend(132300_u32.to_le_bytes());
    wav.extend(3_u16.to_le_bytes());
    wav.extend(24_u16.to_le_bytes());
    wav.extend(b"data");
    wav.extend((data.len() as u32).to_le_bytes());
    wav.extend(data);
    // Odd data has a RIFF pad outside the declared audio data.
    wav.push(0);
    let total = wav.len() as u32 - 8;
    wav[4..8].copy_from_slice(&total.to_le_bytes());
    fs::write(&path, wav).unwrap();
    let mut selected = selection(path, AssetBank::Ir);
    assert!(
        inspect(selected.clone(), &AtomicBool::new(false))
            .unwrap_err()
            .contains("explicitly choose")
    );
    selected.truncate = true;
    let local = inspect(selected, &AtomicBool::new(false)).unwrap();
    assert!(local.technical.contains("1 truncated"));
    let path = dir.0.join("corrupt.clo");
    let mut corrupt = CLO.to_vec();
    corrupt[8] ^= 1;
    fs::write(&path, corrupt).unwrap();
    assert!(
        inspect(
            selection(path, AssetBank::SnapTone),
            &AtomicBool::new(false)
        )
        .unwrap_err()
        .contains("CRC16")
    );
}
