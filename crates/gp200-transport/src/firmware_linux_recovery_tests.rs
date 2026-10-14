//! Local parser/admission fixtures. No native MIDI endpoint or firmware call exists here.
use super::*;
use crate::firmware_linux_recovery_admission::{RecoveryFilePrefix, load_local_lineage};
use gp200_domain::{FirmwareCaptureFence, FirmwareCaptureReservation, JobSha256, SessionEpoch};
use sha2::{Digest, Sha256};
use std::{
    fs::{File, OpenOptions},
    io::Write,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

static NEXT: AtomicU64 = AtomicU64::new(1);
struct Fixture {
    root: PathBuf,
    registry: PathBuf,
    library: PathBuf,
    directory: File,
}
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "gp200-fresh-recovery-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir(&root).unwrap();
        private_dir(&root);
        let registry = root.join("registry");
        std::fs::create_dir(&registry).unwrap();
        private_dir(&registry);
        let library = root.join("library");
        std::fs::create_dir(&library).unwrap();
        private_dir(&library);
        std::fs::create_dir(library.join("linux-firmware-completion")).unwrap();
        private_dir(&library.join("linux-firmware-completion"));
        let directory = File::open(&registry).unwrap();
        Self {
            root,
            registry,
            library,
            directory,
        }
    }
    fn attempt(&self, character: char) -> FirmwareAttemptId {
        FirmwareAttemptId::parse(&character.to_string().repeat(32)).unwrap()
    }
    fn initial(&self, character: char) -> FirmwareAttemptId {
        self.create(character, vec![])
    }
    fn create(
        &self,
        character: char,
        ancestors: Vec<RecoveryPredecessorReference>,
    ) -> FirmwareAttemptId {
        self.create_with_profile(
            character,
            ancestors,
            crate::firmware_vendor_sequence::VENDOR_CONTRACT_SHA256,
        )
    }
    fn create_with_profile(
        &self,
        character: char,
        ancestors: Vec<RecoveryPredecessorReference>,
        profile_sha256: &str,
    ) -> FirmwareAttemptId {
        let attempt = self.attempt(character);
        let journal = self
            .library
            .join("linux-firmware-completion")
            .join(format!("{}.journal", attempt.as_str()));
        let capture = FirmwareCaptureReservation {
            actor_run: "MockSimulation parser fixture".into(),
            reservation: "fixture actual descriptive acquisition".into(),
            normal_owner: "fixture retired normal owner".into(),
            epoch: SessionEpoch(10),
            usb: binding(),
            clock_origin: "fixture original clock".into(),
            prepared_monotonic_ns: 0,
            prepared_wall_millis: 1000,
            initial_fence: FirmwareCaptureFence {
                allocated: 0,
                processed: 0,
            },
        };
        let pins=(0..3).map(|i|serde_json::json!({"path":self.root.join(format!("historical-{i}.fixture")),"bytes":1,"sha256":"33".repeat(32)})).collect::<Vec<_>>();
        let review = serde_json::json!({"schema":1,"job_sha256":"11".repeat(32),"reservation":capture,"acquisition":"original descriptive acquisition","evidence_index_sha256":"44".repeat(32),"captured_materials":pins});
        let metadata = if ancestors.is_empty() {
            serde_json::json!({"review":review,"review_digest":"22".repeat(32),"original_normal_epoch":SessionEpoch(10)})
        } else {
            serde_json::json!({"purpose":FRESH_RECOVERY_PURPOSE,"review":review,"review_digest":"22".repeat(32),"original_binding":binding(),"anchor_binding":binding(),"predecessors":ancestors})
        };
        let begin = serde_json::to_vec(&serde_json::json!({"kind":"begin","schema":1,"run":attempt,"metadata":metadata,"resume_supported":false,"physical_authority":false})).unwrap();
        write_private(&journal, &frame(b"GP2LCJ01", &begin));
        let reservation = format!(
            "schema=1\norigin=LocalObservation\nattempt={}\nreview={}\nprofile={}\nbinding={}\ngeneration=1\nclock=local-fixture\nimage={}\njournal={}\nupdater_epoch=11\nno_physical_authority=true",
            attempt.as_str(),
            "11".repeat(32),
            profile_sha256,
            binding().digest(),
            crate::firmware_vendor_sequence::VENDOR_IMAGE_SHA256,
            journal.display()
        );
        let mut body = vec![0];
        body.extend_from_slice(reservation.as_bytes());
        write_private(
            &self
                .registry
                .join(format!("{}.quarantine", attempt.as_str())),
            &frame(b"GP2QUAR1", &body),
        );
        attempt
    }
    fn checked(&self, selected: &FirmwareAttemptId) -> Result<RecoveryPreparation, ServiceError> {
        load_local_lineage(&self.directory, &self.registry, &self.library, selected)
            .map(RecoveryPreparation::from_checked_lineage)
    }
    fn path(&self, attempt: &FirmwareAttemptId, journal: bool) -> PathBuf {
        if journal {
            self.library
                .join("linux-firmware-completion")
                .join(format!("{}.journal", attempt.as_str()))
        } else {
            self.registry
                .join(format!("{}.quarantine", attempt.as_str()))
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}
fn private_dir(path: &Path) {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
}
fn write_private(path: &Path, bytes: &[u8]) {
    std::fs::write(path, bytes).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
}
fn frame(magic: &[u8; 8], body: &[u8]) -> Vec<u8> {
    let mut bytes = magic.to_vec();
    bytes.extend_from_slice(&(body.len() as u32).to_le_bytes());
    bytes.extend_from_slice(&Sha256::digest(body));
    bytes.extend_from_slice(body);
    bytes
}
fn binding() -> FirmwareUsbObservation {
    FirmwareUsbObservation {
        topology: "fixture private topology".into(),
        vendor: 0x84ef,
        product: 0x2a,
        serial: "fixture serial".into(),
        interface: "fixture interface".into(),
        endpoint: "hw:GP2000,0,0".into(),
    }
}
fn append(path: &Path, bytes: &[u8]) {
    OpenOptions::new()
        .append(true)
        .open(path)
        .unwrap()
        .write_all(bytes)
        .unwrap();
}

#[test]
fn a_checked_local_initial_predecessor_is_descriptive_and_not_resumable() {
    let fixture = Fixture::new();
    let attempt = fixture.initial('a');
    let checked = fixture.checked(&attempt).unwrap();
    assert_eq!(checked.review().selected, attempt);
    assert_eq!(checked.review().predecessors.len(), 1);
    assert_eq!(checked.review().starts_at_section, 0);
    assert_eq!(checked.review().starts_at_block, 0);
    assert!(!checked.review().can_resume());
    assert!(!checked.review().permits_flash());
    assert!(!checked.review().fresh_normal_backup_available);
    assert!(!checked.review().installed_ir_audio_available);
    assert_eq!(checked.review().historical_captures.len(), 1);
    assert_eq!(
        checked.review().historical_captures[0].prepared_wall_millis,
        1000
    );
    checked.revalidate().unwrap();
}

#[test]
fn legacy_and_current_contract_predecessors_keep_original_prefixes_and_unknown_contract_rejects() {
    use crate::firmware_vendor_sequence::{LEGACY_VENDOR_CONTRACT_SHA256, VENDOR_CONTRACT_SHA256};
    let fixture = Fixture::new();
    let first = fixture.create_with_profile('a', vec![], LEGACY_VENDOR_CONTRACT_SHA256);
    let registry = std::fs::read(fixture.path(&first, false)).unwrap();
    let journal = std::fs::read(fixture.path(&first, true)).unwrap();
    let checked = fixture.checked(&first).unwrap();
    assert_eq!(
        checked.review().predecessors[0].registry.sha256,
        JobSha256::of(&registry).hex()
    );
    assert_eq!(
        checked.review().predecessors[0].journal.sha256,
        JobSha256::of(&journal).hex()
    );
    checked.revalidate().unwrap();
    let second = fixture.create_with_profile(
        'b',
        checked.review().predecessors.clone(),
        VENDOR_CONTRACT_SHA256,
    );
    let current = fixture.checked(&second).unwrap();
    assert_eq!(current.review().predecessors.len(), 2);
    assert!(!current.review().permits_flash());
    current.revalidate().unwrap();
    assert_eq!(
        std::fs::read(fixture.path(&first, false)).unwrap(),
        registry
    );
    assert_eq!(std::fs::read(fixture.path(&first, true)).unwrap(), journal);
    assert!(
        std::str::from_utf8(&registry[45..])
            .unwrap()
            .contains(&format!("profile={LEGACY_VENDOR_CONTRACT_SHA256}\n"))
    );
    let fixture = Fixture::new();
    let unknown = fixture.create_with_profile('a', vec![], &"dd".repeat(32));
    let error = fixture
        .checked(&unknown)
        .err()
        .expect("unknown contract rejected");
    assert!(
        error
            .to_string()
            .contains("Predecessor is not this supported local Linux vendor-image route")
    );
}

#[test]
fn historical_corrected_contract_remains_readable_without_changing_original_bytes() {
    let fixture = Fixture::new();
    let attempt = fixture.create_with_profile(
        'a',
        vec![],
        crate::firmware_vendor_sequence::HISTORICAL_CORRECTED_VENDOR_CONTRACT_SHA256,
    );
    let registry = std::fs::read(fixture.path(&attempt, false)).unwrap();
    let journal = std::fs::read(fixture.path(&attempt, true)).unwrap();
    let checked = fixture.checked(&attempt).unwrap();
    checked.revalidate().unwrap();
    assert!(!checked.review().permits_flash());
    assert!(!checked.review().can_resume());
    assert_eq!(
        std::fs::read(fixture.path(&attempt, false)).unwrap(),
        registry
    );
    assert_eq!(
        std::fs::read(fixture.path(&attempt, true)).unwrap(),
        journal
    );
}

#[test]
#[ignore = "requires the separately retained pinned firmware image; admission only, no owner or MIDI"]
fn fresh_completion_and_recovery_drafts_reject_the_legacy_contract_before_live_admission() {
    use crate::firmware_linux_completion_admission::LinuxCompletionDraft;
    use crate::firmware_vendor_sequence::LEGACY_VENDOR_CONTRACT_SHA256;
    use gp200_domain::{
        AssetId, FirmwareMaterialInput, FirmwareMaterialKind, FirmwareUpdateContextDto,
        RetainedJobBlob, ReviewedFirmwareUpdate,
    };
    let image = std::fs::read(
        std::env::var_os("GP200_VENDOR_SEQUENCE_IMAGE").expect("pinned exact offline image"),
    )
    .unwrap();
    let source = RetainedJobBlob::checked(
        AssetId("MockSimulation contract admission only".into()),
        Arc::from(image),
        JobSha256::parse(VENDOR_IMAGE_SHA256).unwrap(),
    )
    .unwrap();
    let context = FirmwareUpdateContextDto {
        profile_id: "offline contract admission".into(),
        hardware_policy: "No owner or physical opener".into(),
        update_mode: "MockSimulation".into(),
        normal: None,
        versions: vec![],
        backup_objects: vec![],
        loss_items: vec!["Installed IR audio remains unavailable".into()],
        power_statement: "Mock only".into(),
        connection_statement: "No USB opened".into(),
    };
    let materials = [
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
        bytes: Arc::from(&b"MockSimulation descriptive admission material"[..]),
    })
    .collect();
    let job = Arc::new(ReviewedFirmwareUpdate::prepare(source, context, materials, 128).unwrap());
    let legacy_path = Path::new(env!("CARGO_MANIFEST_DIR")).join(
        "../../analysis/implementation/firmware-transfer-static-closure/sequential-contract.json",
    );
    let legacy_bytes = std::fs::read(&legacy_path).unwrap();
    assert_eq!(
        JobSha256::of(&legacy_bytes).hex(),
        LEGACY_VENDOR_CONTRACT_SHA256
    );
    let legacy = EntryStudyFilePin {
        path: std::fs::canonicalize(legacy_path).unwrap(),
        bytes: legacy_bytes.len() as u64,
        sha256: LEGACY_VENDOR_CONTRACT_SHA256.into(),
    };
    let fixture = Fixture::new();
    let attempt = fixture.create_with_profile('a', vec![], LEGACY_VENDOR_CONTRACT_SHA256);
    let journal = std::fs::read(fixture.path(&attempt, true)).unwrap();
    let begin: serde_json::Value = serde_json::from_slice(&journal[44..]).unwrap();
    let reservation =
        serde_json::from_value(begin["metadata"]["review"]["reservation"].clone()).unwrap();
    let inputs = LinuxCompletionInputs {
        sources: vec![legacy.clone()],
        loss: legacy.clone(),
        recovery: legacy.clone(),
        vendor_guidance: legacy.clone(),
        contract: legacy.clone(),
        run_directory: fixture.root.clone(),
        approval_wait_seconds: 30,
        transition_seconds: 5,
        transfer_seconds: 300,
    };
    let error = LinuxCompletionDraft::prepare(
        job.clone(),
        reservation,
        "mock descriptive acquisition".into(),
        "44".repeat(32),
        serde_json::json!({"origin":"MockSimulation","physical_hardware":false}),
        vec![legacy; 3],
        inputs.clone(),
    )
    .err()
    .expect("legacy contract cannot create a fresh completion draft");
    assert!(
        error
            .to_string()
            .contains("Completion scope, exact image/contract or finite bounds differ")
    );
    let lineage = load_local_lineage(
        &fixture.directory,
        &fixture.registry,
        &fixture.library,
        &attempt,
    )
    .unwrap();
    let anchor = crate::firmware_linux_completion_binding::CompletionOwnedAnchor {
        binding: binding(),
        guard: None,
    };
    let error = LinuxRecoveryAuthorization::from_fresh_owner(
        job.review().clone(),
        inputs,
        lineage,
        &anchor,
    )
    .err()
    .expect("legacy contract cannot create a fresh recovery draft");
    assert!(
        error
            .to_string()
            .contains("Recovery exact image/contract/source or finite policy differs")
    );
}

#[test]
fn full_local_successor_lineage_is_required_and_old_prefixes_remain_bound() {
    let fixture = Fixture::new();
    let first = fixture.initial('a');
    let refs = fixture
        .checked(&first)
        .unwrap()
        .review()
        .predecessors
        .clone();
    let second = fixture.create('b', refs);
    let checked = fixture.checked(&second).unwrap();
    assert_eq!(checked.review().predecessors.len(), 2);
    // Appending a valid original negative record after its retained prefix preserves lineage,
    // but revokes the existing in-process exact-current snapshot.
    append(
        &fixture.path(&first, true),
        &frame(
            b"GP2LCJ01",
            br#"{"kind":"failed","calls":1,"no_retry":true}"#,
        ),
    );
    assert!(checked.revalidate().is_err());
    fixture.checked(&second).unwrap();
}

#[test]
fn unrelated_roots_branches_cycles_and_missing_ancestors_are_denied() {
    let fixture = Fixture::new();
    let first = fixture.initial('a');
    fixture.initial('b');
    assert!(fixture.checked(&first).is_err());
    let fixture = Fixture::new();
    let first = fixture.initial('a');
    let refs = fixture
        .checked(&first)
        .unwrap()
        .review()
        .predecessors
        .clone();
    let second = fixture.create('b', refs.clone());
    fixture.create('c', refs);
    assert!(fixture.checked(&second).is_err());
    let fixture = Fixture::new();
    let first = fixture.initial('a');
    let mut refs = fixture
        .checked(&first)
        .unwrap()
        .review()
        .predecessors
        .clone();
    refs[0].attempt = fixture.attempt('d');
    let second = fixture.create('b', refs);
    assert!(fixture.checked(&second).is_err());
}

#[test]
fn partial_checksum_unknown_pending_and_invalid_begin_deny_without_an_owner() {
    for mutation in 0..6 {
        let fixture = Fixture::new();
        let attempt = fixture.initial('a');
        match mutation {
            0 => append(&fixture.path(&attempt, true), b"partial"),
            1 => {
                let path = fixture.path(&attempt, false);
                let mut bytes = std::fs::read(&path).unwrap();
                bytes[12] ^= 1;
                write_private(&path, &bytes);
            }
            2 => write_private(
                &fixture.registry.join("unknown.pending"),
                b"private retained incomplete record",
            ),
            3 => append(
                &fixture.path(&attempt, true),
                &frame(b"GP2LCJ01", br#"{"kind":"begin","schema":1}"#),
            ),
            4 => append(
                &fixture.path(&attempt, true),
                &frame(b"GP2LCJ01", br#"{"kind":"unknown-authority"}"#),
            ),
            _ => {
                std::fs::remove_file(fixture.path(&attempt, true)).unwrap();
            }
        }
        assert!(fixture.checked(&attempt).is_err(), "mutation {mutation}");
    }
}

#[test]
fn symlink_hardlink_permissions_and_changed_named_identity_deny() {
    for mutation in 0..4 {
        let fixture = Fixture::new();
        let attempt = fixture.initial('a');
        let checked = fixture.checked(&attempt).unwrap();
        let path = fixture.path(&attempt, true);
        match mutation {
            0 => {
                let saved = fixture.root.join("saved");
                std::fs::rename(&path, &saved).unwrap();
                std::os::unix::fs::symlink(&saved, &path).unwrap();
            }
            1 => std::fs::hard_link(&path, fixture.root.join("second-link")).unwrap(),
            2 => std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap(),
            _ => {
                let bytes = std::fs::read(&path).unwrap();
                std::fs::remove_file(&path).unwrap();
                write_private(&path, &bytes);
            }
        }
        assert!(checked.revalidate().is_err(), "mutation {mutation}");
    }
}

#[test]
fn exact_registry_journal_image_binding_and_retained_prefix_must_agree() {
    let fixture = Fixture::new();
    let first = fixture.initial('a');
    let mut refs = fixture
        .checked(&first)
        .unwrap()
        .review()
        .predecessors
        .clone();
    refs[0].journal.sha256 = "ff".repeat(32);
    let second = fixture.create('b', refs);
    assert!(fixture.checked(&second).is_err());
    let fixture = Fixture::new();
    let first = fixture.initial('a');
    let mut refs = fixture
        .checked(&first)
        .unwrap()
        .review()
        .predecessors
        .clone();
    refs[0].registry.inode += 1;
    let second = fixture.create('b', refs);
    assert!(fixture.checked(&second).is_err());
    let fixture = Fixture::new();
    let attempt = fixture.initial('a');
    let path = fixture.path(&attempt, false);
    let bytes = std::fs::read(&path).unwrap();
    let body = std::str::from_utf8(&bytes[45..]).unwrap();
    let changed = body.replace(&binding().digest(), &"ee".repeat(32));
    let mut payload = vec![0];
    payload.extend_from_slice(changed.as_bytes());
    write_private(&path, &frame(b"GP2QUAR1", &payload));
    assert!(fixture.checked(&attempt).is_err());
}

#[test]
fn successful_zero_offer_closure_is_excluded_and_mock_is_not_physical() {
    let fixture = Fixture::new();
    let attempt = fixture.initial('a');
    append(
        &fixture.path(&attempt, false),
        &frame(
            b"GP2QUAR1",
            b"\x02creating owner observed zero driver calls",
        ),
    );
    assert!(fixture.checked(&attempt).is_err());
    let fixture = Fixture::new();
    let attempt = fixture.initial('a');
    let path = fixture.path(&attempt, false);
    let bytes = std::fs::read(&path).unwrap();
    let body = std::str::from_utf8(&bytes[45..])
        .unwrap()
        .replace("LocalObservation", "MockSimulation");
    let mut payload = vec![0];
    payload.extend_from_slice(body.as_bytes());
    write_private(&path, &frame(b"GP2QUAR1", &payload));
    assert!(fixture.checked(&attempt).is_err());
}

#[test]
fn exported_references_do_not_become_live_recovery_authority() {
    let fixture = Fixture::new();
    let attempt = fixture.initial('a');
    let checked = fixture.checked(&attempt).unwrap();
    let bytes = serde_json::to_vec(&checked.review().predecessors).unwrap();
    let imported: Vec<RecoveryPredecessorReference> = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(imported, checked.review().predecessors);
    let descriptor: &RecoveryFilePrefix = &imported[0].journal;
    let metadata = std::fs::metadata(&descriptor.path).unwrap();
    assert_eq!(metadata.ino(), descriptor.inode);
    assert_eq!(
        JobSha256::of(&std::fs::read(&descriptor.path).unwrap()).hex(),
        descriptor.sha256
    );
    // No constructor accepting these imported refs returns an owner, guard, store or permit.
    assert!(!checked.review().permits_flash());
}

#[test]
fn genuine_large_begin_uses_its_own_cap_and_does_not_enlarge_later_records() {
    let fixture = Fixture::new();
    let attempt = fixture.initial('a');
    let path = fixture.path(&attempt, true);
    let bytes = std::fs::read(&path).unwrap();
    let mut begin: serde_json::Value = serde_json::from_slice(&bytes[44..]).unwrap();
    begin["metadata"]["review"]["genuine_preparation"] =
        serde_json::json!({"large_fixture":"x".repeat(600*1024)});
    let large = serde_json::to_vec(&begin).unwrap();
    assert!(large.len() > 512 * 1024);
    write_private(&path, &frame(b"GP2LCJ01", &large));
    fixture.checked(&attempt).unwrap();
    let ordinary = serde_json::to_vec(
        &serde_json::json!({"kind":"raw-chunk","large_fixture":"x".repeat(600*1024)}),
    )
    .unwrap();
    append(&path, &frame(b"GP2LCJ01", &ordinary));
    assert!(fixture.checked(&attempt).is_err());
}

#[test]
fn historical_association_and_fresh_run_anchor_are_distinct_but_bound() {
    let fixture = Fixture::new();
    let first = fixture.initial('a');
    let refs = fixture
        .checked(&first)
        .unwrap()
        .review()
        .predecessors
        .clone();
    let second = fixture.create('b', refs);
    let mut fresh = binding();
    fresh.product = 0x002b;
    fresh.serial = "V1.0.0".into();
    fresh.endpoint = "hw:BootFixture,0,0".into();
    fresh.interface = "fresh boot fixture interface".into();
    replace_recovery_anchor(&fixture, &second, &fresh);
    let checked = fixture.checked(&second).unwrap();
    assert_eq!(checked.review().original_association, binding());
    let successor = checked
        .review()
        .predecessors
        .iter()
        .find(|p| p.attempt == second)
        .unwrap();
    assert_eq!(successor.original_binding_sha256, binding().digest());
    assert_eq!(successor.reservation_binding_sha256, fresh.digest());
}

fn replace_recovery_anchor(
    fixture: &Fixture,
    attempt: &FirmwareAttemptId,
    fresh: &FirmwareUsbObservation,
) {
    let path = fixture.path(attempt, true);
    let bytes = std::fs::read(&path).unwrap();
    let mut begin: serde_json::Value = serde_json::from_slice(&bytes[44..]).unwrap();
    begin["metadata"]["anchor_binding"] = serde_json::to_value(fresh).unwrap();
    write_private(
        &path,
        &frame(b"GP2LCJ01", &serde_json::to_vec(&begin).unwrap()),
    );
    let registry = fixture.path(attempt, false);
    let bytes = std::fs::read(&registry).unwrap();
    let text = std::str::from_utf8(&bytes[45..])
        .unwrap()
        .replace(&binding().digest(), &fresh.digest());
    let mut payload = vec![0];
    payload.extend_from_slice(text.as_bytes());
    write_private(&registry, &frame(b"GP2QUAR1", &payload));
}

#[test]
fn recovery_lineage_rejects_wrong_boot_model_serial_and_topology_even_with_exact_anchor_digest() {
    let original = binding();
    let boot = FirmwareUsbObservation {
        product: 0x002b,
        serial: "V1.0.0".into(),
        ..original.clone()
    };
    for unrelated in [
        FirmwareUsbObservation {
            product: 0x0029,
            ..boot.clone()
        },
        FirmwareUsbObservation {
            vendor: 0x8888,
            ..boot.clone()
        },
        FirmwareUsbObservation {
            serial: "V2.0.0".into(),
            ..boot.clone()
        },
        FirmwareUsbObservation {
            topology: "unrelated topology".into(),
            ..boot.clone()
        },
        FirmwareUsbObservation {
            serial: "different normal serial".into(),
            ..original.clone()
        },
    ] {
        let fixture = Fixture::new();
        let first = fixture.initial('a');
        let refs = fixture
            .checked(&first)
            .unwrap()
            .review()
            .predecessors
            .clone();
        let successor = fixture.create('b', refs);
        replace_recovery_anchor(&fixture, &successor, &unrelated);
        assert!(fixture.checked(&successor).is_err());
        assert!(
            fixture.checked(&first).is_err(),
            "entire unrelated unresolved lineage must remain held"
        );
        assert!(fixture.path(&first, true).exists());
        assert!(fixture.path(&successor, false).exists());
    }
}

#[test]
fn catalogue_retains_unresolved_and_zero_call_closed_history_without_authority() {
    let f = Fixture::new();
    let first = f.initial('a');
    let second = f.create(
        'b',
        f.checked(&first).unwrap().review().predecessors.clone(),
    );
    let mut body = vec![2];
    body.extend_from_slice(b"creating owner observed zero driver calls");
    append(&f.path(&second, false), &frame(b"GP2QUAR1", &body));
    let catalog = review_linux_firmware_history_at(&f.library, &f.registry).unwrap();
    assert_eq!(catalog.rows.len(), 2);
    assert!(catalog.diagnostics.is_empty());
    assert!(!catalog.rows[0].resolved && catalog.rows[1].resolved);
    assert!(
        catalog
            .rows
            .iter()
            .all(|r| !r.resume_supported && !r.physical_authority)
    );
    assert!(catalog.rows[1].historical_capture.is_none());
    assert_eq!(
        catalog.rows[0]
            .historical_capture
            .as_ref()
            .unwrap()
            .prepared_wall_millis,
        1000
    );
    assert!(!catalog.physical_authority && !catalog.resume_supported);
}
#[test]
fn catalogue_reports_partial_and_unknown_history_conservatively() {
    let f = Fixture::new();
    let attempt = f.initial('a');
    append(&f.path(&attempt, true), b"partial");
    write_private(&f.registry.join("ab.pending"), b"unsettled");
    let catalog = review_linux_firmware_history_at(&f.library, &f.registry).unwrap();
    assert!(catalog.rows.is_empty());
    assert_eq!(catalog.diagnostics.len(), 2);
    assert!(!catalog.physical_authority);
}

fn completed_linked_fixture() -> Fixture {
    completed_linked_fixture_with_device(None)
}

fn completed_linked_fixture_with_device(recorded_device: Option<u64>) -> Fixture {
    let f = Fixture::new();
    let a = f.initial('a');
    let b = f.create('b', f.checked(&a).unwrap().review().predecessors.clone());
    let mut refs = f.checked(&b).unwrap().review().predecessors.clone();
    let c = f.create('c', refs.clone());
    // Simulate a completed history retained across a Btrfs remount/reboot.
    // st_dev is allocated by the running kernel; inode numbers and bytes stay put.
    if let Some(device) = recorded_device {
        for r in &mut refs {
            r.registry.device = device;
        }
    }
    let description = refs
        .iter()
        .map(|r| {
            format!(
                "{},{},{},{},{}",
                r.attempt.as_str(),
                r.registry.bytes,
                r.registry.sha256,
                r.registry.device,
                r.registry.inode
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let set = JobSha256::of(description.as_bytes()).hex();
    let registry = crate::firmware_quarantine::QuarantineRegistry::mock(&f.registry).unwrap();
    assert!(registry.admit_normal().is_err());
    let mut members = Vec::new();
    for r in &refs {
        let body = format!(
            "schema=1\nkind=linked-recovery-predecessor\nsuccessor={}\nset={}\noriginal_bytes={}\noriginal_sha256={}\noriginal_device={}\noriginal_inode={}\nno_physical_authority=true",
            c.as_str(),
            set,
            r.registry.bytes,
            r.registry.sha256,
            r.registry.device,
            r.registry.inode
        );
        let mut payload = vec![6];
        payload.extend_from_slice(body.as_bytes());
        append(&r.registry.path, &frame(b"GP2QUAR1", &payload));
        assert!(
            registry.admit_normal().is_err(),
            "each partial predecessor append stays blocked"
        );
        let bytes = std::fs::read(&r.registry.path).unwrap();
        assert_eq!(
            JobSha256::of(&bytes[..r.registry.bytes as usize]).hex(),
            r.registry.sha256
        );
        members.push(format!(
            "predecessor={},{},{},{},{},{},{}",
            r.attempt.as_str(),
            r.registry.bytes,
            r.registry.sha256,
            r.registry.device,
            r.registry.inode,
            bytes.len(),
            JobSha256::of(&bytes).hex()
        ));
    }
    let journal = f.path(&c, true);
    append(&journal,&frame(b"GP2LCJ01",br#"{"kind":"live-recovery-completion-verified","physical_authority":false,"fixture":"descriptive linked disk parser only"}"#));
    let journal_bytes = std::fs::read(&journal).unwrap();
    let mut lines = vec![
        "schema=1".into(),
        "kind=linked-recovery-commit".into(),
        format!("successor={}", c.as_str()),
        format!("set={set}"),
        format!("source_review={}", "11".repeat(32)),
        format!(
            "image={}",
            crate::firmware_vendor_sequence::VENDOR_IMAGE_SHA256
        ),
        format!("journal={}", journal.display()),
        format!("journal_bytes={}", journal_bytes.len()),
        format!(
            "journal_prefix_sha256={}",
            JobSha256::of(&journal_bytes).hex()
        ),
        "journal_sequence=2".into(),
        "normal_epoch=20".into(),
        format!("normal_binding_sha256={}", binding().digest()),
        format!("identity_sha256={}", "33".repeat(32)),
        format!("current_sha256={}", "44".repeat(32)),
        format!("globals_sha256={}", "55".repeat(32)),
        "raw_allocated=3".into(),
        "raw_processed=3".into(),
        "reader_joined=true".into(),
        "output_destroyed=true".into(),
        format!("manual_phase_review={}", "66".repeat(32)),
        "manual_application_version=V1.8.2".into(),
        "contents_verified=false".into(),
        format!("count={}", members.len()),
    ];
    lines.extend(members);
    let mut payload = vec![7];
    payload.extend_from_slice(lines.join("\n").as_bytes());
    append(&f.path(&c, false), &frame(b"GP2QUAR1", &payload));
    registry.admit_normal().unwrap();
    f
}

#[test]
fn completed_linked_history_survives_device_renumbering_without_recovery_authority() {
    let f = completed_linked_fixture_with_device(Some(u64::MAX));
    let registry = crate::firmware_quarantine::QuarantineRegistry::mock(&f.registry).unwrap();
    registry.admit_normal().unwrap();
    let catalog = review_linux_firmware_history_at(&f.library, &f.registry).unwrap();
    assert_eq!(catalog.rows.len(), 3);
    assert!(catalog.diagnostics.is_empty());
    assert!(
        catalog
            .rows
            .iter()
            .all(|r| r.resolved && !r.physical_authority)
    );
    assert!(f.checked(&f.attempt('c')).is_err());
    f.initial('d');
    assert!(registry.admit_normal().is_err());
}

#[test]
fn renumbered_completed_history_still_rejects_changed_files_and_journal() {
    for mutation in 0..5 {
        let f = completed_linked_fixture_with_device(Some(u64::MAX));
        let member = f.path(&f.attempt('a'), false);
        let journal = f.path(&f.attempt('c'), true);
        match mutation {
            0 => append(&member, b"partial tail"),
            1 => {
                let mut bytes = std::fs::read(&member).unwrap();
                let n = u32::from_le_bytes(bytes[8..12].try_into().unwrap()) as usize;
                let body = String::from_utf8(bytes[45..44 + n].to_vec()).unwrap();
                let changed = body.replace("generation=1", "generation=2");
                assert_ne!(changed, body);
                bytes[45..44 + n].copy_from_slice(changed.as_bytes());
                // Keep the frame checksum valid: the committed content pin must reject it.
                let checksum = Sha256::digest(&bytes[44..44 + n]);
                bytes[12..44].copy_from_slice(&checksum);
                write_private(&member, &bytes);
            }
            2 => {
                // Identical content at the expected name must not hide an inode replacement.
                let replacement = f.registry.join("replacement");
                write_private(&replacement, &std::fs::read(&member).unwrap());
                std::fs::rename(replacement, &member).unwrap();
            }
            3 => append(&journal, b"partial tail"),
            _ => {
                let mut bytes = std::fs::read(&journal).unwrap();
                bytes[60] ^= 1;
                write_private(&journal, &bytes);
            }
        }
        let registry = crate::firmware_quarantine::QuarantineRegistry::mock(&f.registry).unwrap();
        assert!(
            registry.admit_normal().is_err(),
            "mutation {mutation} admitted"
        );
    }
}

#[test]
fn linked_complete_set_stays_held_until_commit_and_any_changed_member_revokes_admission() {
    let f = completed_linked_fixture();
    let a = f.attempt('a');
    let c = f.attempt('c');
    let registry = crate::firmware_quarantine::QuarantineRegistry::mock(&f.registry).unwrap();
    let catalog = review_linux_firmware_history_at(&f.library, &f.registry).unwrap();
    assert_eq!(catalog.rows.len(), 3);
    assert!(
        catalog
            .rows
            .iter()
            .all(|r| r.resolved && !r.physical_authority)
    );
    assert!(
        f.checked(&c).is_err(),
        "resolved disk history cannot reenter recovery admission"
    );
    append(&f.path(&a, false), b"crashed tail");
    assert!(registry.admit_normal().is_err());
}

#[test]
fn default_recovery_selection_skips_validated_completed_linked_history_before_new_unresolved_root()
{
    let f = completed_linked_fixture();
    let d = f.initial('d');
    let registry = crate::firmware_quarantine::QuarantineRegistry::mock(&f.registry).unwrap();
    let checked = registry
        .checked_recovery_fixture_default(&f.library)
        .unwrap();
    assert_eq!(checked.selected(), &d);
    assert_eq!(checked.references().len(), 1);
    assert!(
        registry.admit_normal().is_err(),
        "new unresolved root keeps ordinary admission held"
    );
    append(
        &f.path(&f.attempt('a'), false),
        b"changed linked member tail",
    );
    assert!(
        registry
            .checked_recovery_fixture_default(&f.library)
            .is_err(),
        "corrupt completed lineage cannot be skipped"
    );
}
