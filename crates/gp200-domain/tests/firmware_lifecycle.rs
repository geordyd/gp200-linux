use gp200_domain::*;
use std::sync::Arc;
fn job() -> ReviewedFirmwareUpdate {
    let b = include_bytes!("../../../tests/fixtures/device-assets/synthetic-firmware.bin");
    let ctx = FirmwareUpdateContextDto {
        profile_id: "local".into(),
        hardware_policy: "unknown".into(),
        update_mode: "mock".into(),
        normal: None,
        versions: vec![],
        backup_objects: vec![],
        loss_items: vec![],
        power_statement: "fixture".into(),
        connection_statement: "fixture".into(),
    };
    let docs = [
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
        bytes: Arc::from(&b"description"[..]),
    })
    .collect();
    ReviewedFirmwareUpdate::prepare(
        RetainedJobBlob::checked(
            AssetId("image".into()),
            b.as_slice().into(),
            JobSha256::of(b),
        )
        .unwrap(),
        ctx,
        docs,
        8,
    )
    .unwrap()
}
fn plan(j: &ReviewedFirmwareUpdate) -> FirmwareLifecyclePlanDto {
    FirmwareLifecyclePlanDto {
        review_sha256: j.digest().hex(),
        profile_sha256: JobSha256::of(b"rule").hex(),
        origin: FirmwareObservationOrigin::MockSimulation,
        nonblock_actions: 1,
        epoch: SessionEpoch(3),
        binding: FirmwareUsbObservation {
            topology: "test topology".into(),
            vendor: 1,
            product: 2,
            serial: "test serial".into(),
            interface: "test interface".into(),
            endpoint: "test endpoint".into(),
        },
        generation: 4,
        clock_origin: "test clock".into(),
    }
}
#[test]
fn lifecycle_budget_accounts_for_raw_chunks_and_rejects_large_schedules() {
    let b = FirmwareLifecycleBudget::checked(50412, 16).unwrap();
    assert_eq!(b.actions, 50428);
    assert_eq!(b.raw_records, 100920);
    assert!(b.bytes > 1_000_000_000);
    assert!(FirmwareLifecycleBudget::checked(u64::MAX, 32).is_err());
    assert!(FirmwareLifecycleBudget::checked(1, 33).is_err());
    assert!(FirmwareLifecycleBudget::checked(1_000_000, 0).is_err());
}
#[test]
fn descriptions_and_imported_physical_claims_never_advance_device_progress() {
    let j = job();
    let mut p = plan(&j);
    p.origin = FirmwareObservationOrigin::LocalObservation;
    let mut projection = FirmwareLifecycleProjection::new(&j, p.clone()).unwrap();
    let a = FirmwareLifecycleActionDescription {
        ordinal: 0,
        kind: FirmwareLifecycleKind::IdentityQuery,
        wire: vec![0xf0, 1, 0xf7],
        wire_sha256: JobSha256::of(&[0xf0, 1, 0xf7]).hex(),
        block: None,
        epoch: p.epoch,
        binding_sha256: p.binding.digest(),
        generation: p.generation,
        rule_sha256: p.profile_sha256.clone(),
    };
    projection.checkpoint(&j, a.clone()).unwrap();
    projection
        .observe_send(&FirmwareLifecycleSendObservation {
            action: 0,
            wire_sha256: a.wire_sha256,
            attempted: 3,
            extent: FirmwareOsExtent::Known(3),
            drain: FirmwareDrainObservation::Returned,
            started_sequence: 0,
            finished_sequence: Some(1),
            started_at_ns: 0,
            finished_at_ns: Some(1),
        })
        .unwrap();
    projection
        .raw(&FirmwareLifecycleRawObservation {
            sequence: 1,
            at_ns: 1,
            epoch: p.epoch,
            generation: p.generation,
            binding_sha256: p.binding.digest(),
            clock_origin: p.clock_origin,
            raw: vec![1],
        })
        .unwrap();
    assert!(
        projection
            .decide(&FirmwareLifecycleDecision {
                action: 0,
                rule_sha256: p.profile_sha256,
                first_raw: 1,
                last_raw: 1,
                observed_frontier: 1,
                mock_matched: true
            })
            .is_err()
    );
    assert_eq!(projection.device_verified_bytes(), 0);
    assert!(!projection.completion_verified());
    assert!(!projection.resume_supported());
}
#[test]
fn raw_sequence_generation_and_frame_source_limits_are_independent() {
    let j = job();
    let p = plan(&j);
    let mut projection = FirmwareLifecycleProjection::new(&j, p.clone()).unwrap();
    let r = FirmwareLifecycleRawObservation {
        sequence: 1,
        at_ns: 1,
        epoch: p.epoch,
        generation: p.generation,
        binding_sha256: p.binding.digest(),
        clock_origin: p.clock_origin,
        raw: vec![0xff; 1024],
    };
    projection.raw(&r).unwrap();
    assert!(projection.raw(&r).is_err());
    let mut r = r;
    r.sequence = 2;
    r.raw.push(0);
    assert!(projection.raw(&r).is_err());
    r.raw.pop();
    r.generation += 1;
    assert!(projection.raw(&r).is_err());
}

fn entry_epoch(p: &FirmwareLifecyclePlanDto) -> FirmwareLifecycleEpochObservation {
    FirmwareLifecycleEpochObservation {
        role: FirmwareLifecycleEpochRole::NormalEntry,
        epoch: p.epoch,
        generation: p.generation,
        binding: p.binding.clone(),
        previous_binding_sha256: None,
        previous_epoch: None,
        source_instance: "updater-entry-source".into(),
        clock_origin: p.clock_origin.clone(),
        retired_source: None,
        raw_identity: vec![0xf0, 0x7d, 1, 0xf7],
        negotiation: vec![],
        transition: None,
        post_normal_refresh: None,
    }
}
fn next_epoch(
    previous: &FirmwareLifecycleEpochObservation,
    role: FirmwareLifecycleEpochRole,
    frontier: u64,
    at_ns: u64,
) -> FirmwareLifecycleEpochObservation {
    let mut binding = previous.binding.clone();
    binding.endpoint = format!("synthetic-{role:?}");
    let clock = if role == FirmwareLifecycleEpochRole::PostUpdateNormal {
        "fresh-normal-clock".into()
    } else {
        previous.clock_origin.clone()
    };
    let source = format!("synthetic-{role:?}-source");
    FirmwareLifecycleEpochObservation {
        role,
        epoch: SessionEpoch(previous.epoch.0 + 1),
        generation: previous.generation + 1,
        binding,
        previous_binding_sha256: Some(previous.binding.digest()),
        previous_epoch: Some(previous.epoch),
        source_instance: source.clone(),
        clock_origin: clock.clone(),
        retired_source: Some(FirmwareLifecycleSourceRetirement {
            source_instance: previous.source_instance.clone(),
            clock_origin: previous.clock_origin.clone(),
            epoch: previous.epoch,
            generation: previous.generation,
            allocated: frontier,
            processed: frontier,
            close_started_at_ns: at_ns,
            close_finished_at_ns: at_ns + 1,
            reader_join: FirmwareNormalReaderJoinObservation::Returned,
            fault_or_missing: false,
        }),
        raw_identity: vec![0xf0, 0x7d, 2, 0xf7],
        negotiation: vec![0xf0, 0x7d, 3, 0xf7],
        transition: Some(FirmwareLifecycleTransitionObservation {
            kind: FirmwareLifecycleTransitionKind::Reenumerated,
            previous_enumeration_generation: previous
                .transition
                .as_ref()
                .map_or(1, |t| t.enumeration_generation),
            enumeration_generation: previous
                .transition
                .as_ref()
                .map_or(2, |t| t.enumeration_generation + 1),
            old_source_disappeared: true,
            old_and_new_coexisted: false,
            candidate_count: 1,
        }),
        post_normal_refresh: (role == FirmwareLifecycleEpochRole::PostUpdateNormal).then(|| {
            FirmwarePostNormalRefreshObservation {
                epoch: SessionEpoch(previous.epoch.0 + 1),
                source_instance: source,
                clock_origin: clock,
                allocated: 4,
                processed: 4,
                identity_sha256: JobSha256::of(b"parsed synthetic identity").hex(),
                current_sha256: JobSha256::of(b"parsed synthetic current").hex(),
                globals_sha256: JobSha256::of(b"parsed synthetic globals").hex(),
                stored_sha256: JobSha256::of(b"parsed synthetic stored slot 0").hex(),
                current_revision: 1,
                globals_revision: 1,
                stored_revision: 1,
                stored_slot: 0,
                application_version: Some("synthetic application version".into()),
                assessment: "Synthetic fresh reads only; no physical completion evidence".into(),
            }
        }),
    }
}
fn accept_action(
    j: &ReviewedFirmwareUpdate,
    p: &mut FirmwareLifecycleProjection,
    ordinal: u64,
    at_ns: u64,
) {
    let block = ordinal.checked_sub(1).map(|i| j.block(i).unwrap());
    let wire = block
        .as_ref()
        .map_or_else(|| vec![0xf0, 0x7d, 1, 0xf7], |b| j.block_wire(b).unwrap());
    let a = FirmwareLifecycleActionDescription {
        ordinal,
        kind: if block.is_some() {
            FirmwareLifecycleKind::ImageBlock
        } else {
            FirmwareLifecycleKind::IdentityQuery
        },
        wire_sha256: JobSha256::of(&wire).hex(),
        wire: wire.clone(),
        block,
        epoch: p.active_epoch(),
        binding_sha256: p.active_binding().digest(),
        generation: p.active_generation(),
        rule_sha256: p.plan().profile_sha256.clone(),
    };
    p.checkpoint(j, a).unwrap();
    p.observe_send(&FirmwareLifecycleSendObservation {
        action: ordinal,
        wire_sha256: JobSha256::of(&wire).hex(),
        attempted: wire.len() as u64,
        extent: FirmwareOsExtent::Known(wire.len() as u64),
        drain: FirmwareDrainObservation::Returned,
        started_sequence: ordinal,
        finished_sequence: Some(ordinal + 1),
        started_at_ns: at_ns - 1,
        finished_at_ns: Some(at_ns),
    })
    .unwrap();
    p.raw(&FirmwareLifecycleRawObservation {
        sequence: ordinal + 1,
        at_ns,
        epoch: p.active_epoch(),
        generation: p.active_generation(),
        binding_sha256: p.active_binding().digest(),
        clock_origin: p.active_clock_origin().into(),
        raw: vec![0xf0, 0x7d, 0xf7],
    })
    .unwrap();
    p.decide(&FirmwareLifecycleDecision {
        action: ordinal,
        rule_sha256: p.plan().profile_sha256.clone(),
        first_raw: ordinal + 1,
        last_raw: ordinal + 1,
        observed_frontier: ordinal + 1,
        mock_matched: true,
    })
    .unwrap();
}
#[test]
fn three_roles_keep_global_action_and_raw_ordinals_and_require_fresh_normal_before_mock_finish() {
    let j = job();
    let plan = plan(&j);
    let mut p = FirmwareLifecycleProjection::new(&j, plan.clone()).unwrap();
    let entry = entry_epoch(&plan);
    p.observe_epoch(entry.clone()).unwrap();
    accept_action(&j, &mut p, 0, 1);
    let boot = next_epoch(&entry, FirmwareLifecycleEpochRole::Bootloader, 1, 1);
    p.observe_epoch(boot.clone()).unwrap();
    assert_eq!(p.plan(), &plan); // Initial description never rewritten.
    assert_eq!(p.next_action(), 1);
    for ordinal in 1..p.budget().actions {
        accept_action(&j, &mut p, ordinal, ordinal + 2);
    }
    assert!(p.close(FirmwareLifecycleHostClosure::MockFinished).is_err());
    let post = next_epoch(
        &boot,
        FirmwareLifecycleEpochRole::PostUpdateNormal,
        p.budget().actions,
        p.budget().actions + 2,
    );
    let mut unknown_version = p.clone();
    let mut incomplete = post.clone();
    incomplete
        .post_normal_refresh
        .as_mut()
        .unwrap()
        .application_version = None;
    unknown_version.observe_epoch(incomplete).unwrap();
    assert!(
        unknown_version
            .close(FirmwareLifecycleHostClosure::MockFinished)
            .is_err()
    );
    let mut stale_generation = p.clone();
    let mut stale_post = post.clone();
    stale_post
        .transition
        .as_mut()
        .unwrap()
        .previous_enumeration_generation = 1;
    assert!(stale_generation.observe_epoch(stale_post).is_err());
    p.observe_epoch(post.clone()).unwrap();
    assert_eq!(p.epochs().len(), 3);
    assert!(p.observe_epoch(post).is_err());
    p.close(FirmwareLifecycleHostClosure::MockFinished).unwrap();
    assert_eq!(p.device_verified_bytes(), 0);
    assert!(!p.completion_verified());
    assert!(!p.resume_supported());
}
#[test]
fn legacy_single_epoch_mock_closure_retains_its_original_meaning() {
    let j = job();
    let mut p = FirmwareLifecycleProjection::new(&j, plan(&j)).unwrap();
    for ordinal in 0..p.budget().actions {
        accept_action(&j, &mut p, ordinal, ordinal + 1);
    }
    p.close(FirmwareLifecycleHostClosure::MockFinished).unwrap();
    assert!(p.epochs().is_empty());
    assert!(!p.completion_verified());
}
#[test]
fn stale_duplicate_ambiguous_unjoined_and_unaccounted_epoch_descriptions_are_rejected() {
    let j = job();
    let plan = plan(&j);
    let entry = entry_epoch(&plan);
    let mut base = FirmwareLifecycleProjection::new(&j, plan.clone()).unwrap();
    base.observe_epoch(entry.clone()).unwrap();
    let valid = next_epoch(&entry, FirmwareLifecycleEpochRole::Bootloader, 0, 0);
    for case in 0..12 {
        let mut e = valid.clone();
        match case {
            0 => e.role = FirmwareLifecycleEpochRole::NormalEntry,
            1 => e.previous_epoch = None,
            2 => e.previous_binding_sha256 = Some(JobSha256::of(b"wrong binding").hex()),
            3 => e.source_instance = entry.source_instance.clone(),
            4 => e.generation = entry.generation,
            5 => e.epoch = entry.epoch,
            6 => e.retired_source.as_mut().unwrap().allocated = 1,
            7 => {
                e.retired_source.as_mut().unwrap().reader_join =
                    FirmwareNormalReaderJoinObservation::Panicked
            }
            8 => e.retired_source.as_mut().unwrap().fault_or_missing = true,
            9 => e.transition.as_mut().unwrap().candidate_count = 2,
            10 => e.transition.as_mut().unwrap().old_and_new_coexisted = true,
            11 => e.clock_origin = "reset updater clock".into(),
            _ => unreachable!(),
        }
        let mut p = base.clone();
        assert!(p.observe_epoch(e).is_err(), "case {case}");
        assert_eq!(p.epochs().len(), 1);
    }
    let mut unresolved = base.clone();
    let wire = vec![0xf0, 0x7d, 1, 0xf7];
    unresolved
        .checkpoint(
            &j,
            FirmwareLifecycleActionDescription {
                ordinal: 0,
                kind: FirmwareLifecycleKind::IdentityQuery,
                wire_sha256: JobSha256::of(&wire).hex(),
                wire,
                block: None,
                epoch: entry.epoch,
                binding_sha256: entry.binding.digest(),
                generation: entry.generation,
                rule_sha256: base.plan().profile_sha256.clone(),
            },
        )
        .unwrap();
    assert!(unresolved.observe_epoch(valid.clone()).is_err());
    let mut stopped = base.clone();
    stopped.stop();
    assert!(stopped.observe_epoch(valid.clone()).is_err());
    let mut same = valid.clone();
    same.transition.as_mut().unwrap().kind =
        FirmwareLifecycleTransitionKind::SameEnumerationProtocolChange;
    same.transition.as_mut().unwrap().enumeration_generation = 1;
    same.transition.as_mut().unwrap().old_source_disappeared = false;
    base.observe_epoch(same).unwrap();
    let mut late_first = FirmwareLifecycleProjection::new(&j, plan).unwrap();
    accept_action(&j, &mut late_first, 0, 1);
    assert!(late_first.observe_epoch(entry).is_err());
}
#[test]
fn epoch_checked_arithmetic_and_actual_encoded_caps_reject_without_truncation() {
    let j = job();
    let mut plan = plan(&j);
    plan.epoch = SessionEpoch(u64::MAX);
    plan.generation = u64::MAX;
    let entry = entry_epoch(&plan);
    let mut p = FirmwareLifecycleProjection::new(&j, plan).unwrap();
    p.observe_epoch(entry.clone()).unwrap();
    let mut e = entry.clone();
    e.role = FirmwareLifecycleEpochRole::Bootloader;
    e.previous_epoch = Some(entry.epoch);
    e.previous_binding_sha256 = Some(entry.binding.digest());
    e.epoch = SessionEpoch(0);
    e.generation = 0;
    assert!(p.observe_epoch(e).is_err());
    let mut e = entry;
    e.raw_identity = vec![255; 512];
    e.negotiation = vec![255; 512];
    assert!(e.retained_json().unwrap().len() <= FIRMWARE_LIFECYCLE_EPOCH_PAYLOAD_LIMIT - 512);
    e.raw_identity.push(255);
    assert!(e.retained_json().is_err());
    e.raw_identity.pop();
    e.binding.topology = "\"".repeat(256);
    e.binding.serial = "\"".repeat(256);
    e.binding.interface = "\"".repeat(256);
    e.binding.endpoint = "\"".repeat(256);
    e.source_instance = "\"".repeat(128);
    e.clock_origin = "\"".repeat(128);
    // Each field can be individually within its bound while the complete
    // post-normal description still exceeds the 8 KiB record allowance.
    let normal_plan = crate::plan(&j);
    let original = entry_epoch(&normal_plan);
    let mut oversized = next_epoch(
        &original,
        FirmwareLifecycleEpochRole::PostUpdateNormal,
        0,
        0,
    );
    oversized.raw_identity = vec![255; 512];
    oversized.negotiation = vec![255; 512];
    oversized.binding = e.binding;
    oversized.source_instance = e.source_instance;
    oversized.clock_origin = e.clock_origin;
    let refresh = oversized.post_normal_refresh.as_mut().unwrap();
    refresh.source_instance = oversized.source_instance.clone();
    refresh.clock_origin = oversized.clock_origin.clone();
    refresh.assessment = "\"".repeat(512);
    refresh.application_version = Some("\"".repeat(128));
    oversized.retired_source.as_mut().unwrap().source_instance = "\"".repeat(128);
    oversized.retired_source.as_mut().unwrap().clock_origin = "\"".repeat(128);
    assert!(
        serde_json::to_vec(&oversized).unwrap().len()
            > FIRMWARE_LIFECYCLE_EPOCH_PAYLOAD_LIMIT - 512
    );
    assert!(oversized.retained_json().is_err());
}
#[test]
fn retirement_dto_retains_partial_originals_and_rejects_oversize_error_or_prefix() {
    let j = job();
    let plan = plan(&j);
    let mut r = FirmwareNormalRetirementObservation {
        normal_epoch: plan.epoch,
        owner_generation: 1,
        source_instance: "normal-source".into(),
        clock_origin: "normal-clock".into(),
        binding: plan.binding,
        close_started_at_ns: Some(0),
        close_finished_at_ns: Some(1),
        allocated_before: 0,
        processed_before: 0,
        allocated_after: 1,
        processed_after: 0,
        reader_join: FirmwareNormalReaderJoinObservation::Panicked,
        output_handle_destroyed: true,
        reader_fault: Some(ServiceError::new(
            ErrorKind::Protocol,
            "Original reader fault",
        )),
        overflow: true,
        retained_raw: vec![FirmwareNormalRetirementRawObservation {
            sequence: 1,
            epoch: SessionEpoch(3),
            at_ns: None,
            original_length: 8192,
            raw: vec![255; 4096],
        }],
        missing: vec![FirmwareNormalRetirementGap {
            source_instance: "normal-source".into(),
            first_sequence: 1,
            last_sequence: 1,
            missing_length: Some(4096),
            reason: "Original suffix unavailable".into(),
        }],
        first_failure: None,
    };
    let encoded = r.retained_json().unwrap();
    let decoded: FirmwareNormalRetirementObservation = serde_json::from_slice(&encoded).unwrap();
    assert_eq!(decoded, r);
    r.retained_raw[0].raw.push(0);
    assert!(r.retained_json().is_err());
    r.retained_raw[0].raw.pop();
    r.retained_raw = vec![r.retained_raw[0].clone(); 64];
    assert!(r.retained_json().unwrap().len() < FIRMWARE_NORMAL_RETIREMENT_ENCODED_LIMIT);
    r.reader_fault.as_mut().unwrap().message = "x".repeat(513);
    assert!(r.retained_json().is_err());
}

fn entry_description(j: &ReviewedFirmwareUpdate) -> FirmwareEntryObservationPlanDto {
    let mut p = plan(j);
    p.nonblock_actions = 2;
    let candidates = [vec![0xf0, 0x21, 0xf7], vec![0xf0, 0x22, 0xf7]];
    let study_sha256 = FirmwareEntryObservationPlanDto::digest_for(&p, &candidates);
    FirmwareEntryObservationPlanDto {
        plan: p,
        candidates,
        study_sha256,
    }
}
fn entry_action(
    e: &FirmwareEntryObservationPlanDto,
    ordinal: u64,
) -> FirmwareLifecycleActionDescription {
    let wire = e.candidates[ordinal as usize].clone();
    FirmwareLifecycleActionDescription {
        ordinal,
        kind: FirmwareLifecycleKind::EntryTransition,
        wire_sha256: JobSha256::of(&wire).hex(),
        wire,
        block: None,
        epoch: e.plan.epoch,
        binding_sha256: e.plan.binding.digest(),
        generation: e.plan.generation,
        rule_sha256: e.study_sha256.clone(),
    }
}
fn entry_send(
    a: &FirmwareLifecycleActionDescription,
    extent: FirmwareOsExtent,
) -> FirmwareLifecycleSendObservation {
    FirmwareLifecycleSendObservation {
        action: a.ordinal,
        wire_sha256: a.wire_sha256.clone(),
        attempted: a.wire.len() as u64,
        extent,
        drain: FirmwareDrainObservation::Returned,
        started_sequence: 0,
        finished_sequence: Some(0),
        started_at_ns: 1,
        finished_at_ns: Some(2),
    }
}
#[test]
fn entry_two_action_host_projection_has_no_block_ack_transfer_or_clearance() {
    let j = job();
    let entry = entry_description(&j);
    let mut p = FirmwareLifecycleProjection::new_entry(&j, entry.clone()).unwrap();
    assert_eq!(p.budget().actions, 2);
    for ordinal in 0..2 {
        let a = entry_action(&entry, ordinal);
        p.checkpoint(&j, a.clone()).unwrap();
        assert!(
            p.decide(&FirmwareLifecycleDecision {
                action: ordinal,
                rule_sha256: a.rule_sha256.clone(),
                first_raw: 1,
                last_raw: 1,
                observed_frontier: 1,
                mock_matched: true
            })
            .is_err()
        );
        assert!(
            p.settle_entry_host(&FirmwareEntryHostSettlement {
                action: ordinal,
                wire_sha256: a.wire_sha256.clone(),
                study_sha256: entry.study_sha256.clone()
            })
            .is_err()
        );
        p.observe_send(&entry_send(
            &a,
            FirmwareOsExtent::Known(a.wire.len() as u64),
        ))
        .unwrap();
        p.settle_entry_host(&FirmwareEntryHostSettlement {
            action: ordinal,
            wire_sha256: a.wire_sha256.clone(),
            study_sha256: entry.study_sha256.clone(),
        })
        .unwrap();
        assert_eq!(p.device_verified_bytes(), 0);
        assert_eq!(p.simulated_source_bytes(), 0);
    }
    assert!(p.close(FirmwareLifecycleHostClosure::MockFinished).is_err());
    p.close(FirmwareLifecycleHostClosure::HostStoppedUnresolved)
        .unwrap();
    assert!(!p.completion_verified());
    assert!(!p.resume_supported());
}
#[test]
fn entry_projection_rejects_changed_order_scope_binding_ordinals_duplicate_and_partial() {
    let j = job();
    let entry = entry_description(&j);
    let mut changed = entry.clone();
    changed.candidates.swap(0, 1);
    assert!(FirmwareLifecycleProjection::new_entry(&j, changed).is_err());
    let first = entry_action(&entry, 0);
    for altered in 0..6 {
        let mut action = first.clone();
        match altered {
            0 => action.ordinal = u64::MAX,
            1 => action.ordinal = 1,
            2 => action.generation += 1,
            3 => action.binding_sha256 = JobSha256::of(b"stale").hex(),
            4 => action.kind = FirmwareLifecycleKind::IdentityQuery,
            _ => action.rule_sha256 = JobSha256::of(b"changed review").hex(),
        }
        let mut p = FirmwareLifecycleProjection::new_entry(&j, entry.clone()).unwrap();
        assert!(p.checkpoint(&j, action).is_err());
    }
    let mut p = FirmwareLifecycleProjection::new_entry(&j, entry.clone()).unwrap();
    p.checkpoint(&j, first.clone()).unwrap();
    assert!(p.checkpoint(&j, first.clone()).is_err());
    p.observe_send(&entry_send(&first, FirmwareOsExtent::Known(1)))
        .unwrap();
    assert!(
        p.settle_entry_host(&FirmwareEntryHostSettlement {
            action: 0,
            wire_sha256: first.wire_sha256,
            study_sha256: entry.study_sha256
        })
        .is_err()
    );
    assert!(p.stopped());
}
