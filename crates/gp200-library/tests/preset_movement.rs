use gp200_domain::{DeviceModel, SlotId};
use gp200_library::{
    PresetInsertionBoundary, PresetMoveAction, PresetMovementError, PresetMovementOperation,
    PresetMovementPlan,
};

fn slot(value: u16) -> SlotId {
    SlotId::new(value).unwrap()
}

fn boundary(model: DeviceModel, value: u16) -> PresetInsertionBoundary {
    PresetInsertionBoundary::new(model, value).unwrap()
}

// Independent reference: remove the selected identities together, then insert
// one stable block into the remaining list. It never composes insertion steps.
fn block_reference(count: u16, selected: &[u16], action: PresetMoveAction) -> Vec<u16> {
    let remaining: Vec<_> = (0..count)
        .filter(|value| !selected.contains(value))
        .collect();
    let destination = match action {
        PresetMoveAction::Top => 0,
        PresetMoveAction::Bottom => usize::from(count) - selected.len(),
        PresetMoveAction::Previous => usize::from(selected[0].saturating_sub(1)),
        PresetMoveAction::Next => {
            usize::from((selected[selected.len() - 1] + 2).min(count)) - selected.len()
        }
    };
    remaining[..destination]
        .iter()
        .chain(selected)
        .chain(&remaining[destination..])
        .copied()
        .collect()
}

// Independent single-source oracle: splice the source between the original
// identities before/after its final destination; no element is shifted in place.
fn single_reference(count: u16, source: u16, insertion_boundary: u16) -> Vec<u16> {
    let destination = if insertion_boundary > source {
        insertion_boundary - 1
    } else {
        insertion_boundary
    };
    let remaining: Vec<_> = (0..count).filter(|value| *value != source).collect();
    remaining[..usize::from(destination)]
        .iter()
        .copied()
        .chain(std::iter::once(source))
        .chain(remaining[usize::from(destination)..].iter().copied())
        .collect()
}

fn check_complete_plan(plan: &PresetMovementPlan, expected: &[u16]) {
    let actual: Vec<_> = plan
        .destination_to_source()
        .iter()
        .map(|slot| slot.get())
        .collect();
    assert_eq!(actual, expected);
    assert_eq!(usize::from(plan.bank_size()), expected.len());
    let mut seen = vec![false; expected.len()];
    for (destination, &source) in expected.iter().enumerate() {
        assert!(!seen[usize::from(source)]);
        seen[usize::from(source)] = true;
        assert_eq!(
            plan.destination_for(slot(source)),
            Some(slot(destination as u16))
        );
        assert_eq!(
            plan.original_source_for(slot(destination as u16)),
            Some(slot(source))
        );
        assert_eq!(plan.mappings()[destination].source, slot(source));
        assert_eq!(
            plan.mappings()[destination].destination,
            slot(destination as u16)
        );
        assert_eq!(
            plan.mappings()[destination].shift(),
            destination as i16 - source as i16
        );
    }
    assert!(seen.into_iter().all(|value| value));

    let changed: Vec<_> = expected
        .iter()
        .enumerate()
        .filter_map(|(destination, &source)| {
            (destination != usize::from(source)).then_some(destination)
        })
        .collect();
    let actual_changed: Vec<_> = plan
        .changed_mappings()
        .map(|row| usize::from(row.destination.get()))
        .collect();
    assert_eq!(actual_changed, changed);
    assert_eq!(plan.is_noop(), changed.is_empty());
    match (changed.first(), changed.last(), plan.affected_interval()) {
        (Some(&first), Some(&last), Some(interval)) => {
            assert_eq!(interval.first().get(), first as u16);
            assert_eq!(interval.last().get(), last as u16);
            let interval_slots: Vec<_> = interval
                .slots()
                .map(|slot| usize::from(slot.get()))
                .collect();
            assert_eq!(interval_slots, (first..=last).collect::<Vec<_>>());
            let affected: Vec<_> = plan
                .affected_mappings()
                .map(|row| usize::from(row.destination.get()))
                .collect();
            assert_eq!(affected, interval_slots);
            let mut expected_sentinels = Vec::new();
            if first > 0 {
                expected_sentinels.push((first - 1) as u16);
            }
            if last + 1 < expected.len() {
                expected_sentinels.push((last + 1) as u16);
            }
            assert_eq!(
                plan.sentinels()
                    .iter()
                    .map(|slot| slot.get())
                    .collect::<Vec<_>>(),
                expected_sentinels
            );
            for sentinel in plan.sentinels() {
                assert_eq!(plan.original_source_for(*sentinel), Some(*sentinel));
                assert!(!interval.contains(*sentinel));
            }
        }
        (None, None, None) => {
            assert!(plan.sentinels().is_empty());
            assert_eq!(plan.affected_mappings().count(), 0);
        }
        _ => panic!("interval and independently changed slots disagree"),
    }

    // Apply the mathematical inverse to the final synthetic bank. This does
    // not claim that live slots or immutable archive blobs have been read.
    let mut restored = vec![u16::MAX; expected.len()];
    for inverse in plan.inverse_mappings() {
        restored[usize::from(inverse.destination.get())] =
            expected[usize::from(inverse.source.get())];
    }
    assert_eq!(restored, (0..expected.len() as u16).collect::<Vec<_>>());

    assert_eq!(
        plan.original_selection().len(),
        plan.final_selection().len()
    );
    for (&original, &final_slot) in plan.original_selection().iter().zip(plan.final_selection()) {
        assert_eq!(expected[usize::from(final_slot.get())], original.get());
    }
    if let Some(current) = plan.current_before() {
        let after = plan.current_after_original_reselection().unwrap();
        assert_eq!(expected[usize::from(after.get())], current.get());
    } else {
        assert!(plan.current_after_original_reselection().is_none());
    }
}

#[test]
fn every_gp200_source_and_boundary_matches_independent_partition_oracle() {
    let model = DeviceModel::Gp200;
    let mut count = 0;
    for source in 0..256 {
        for insertion_boundary in 0..=256 {
            let plan = PresetMovementPlan::insert(
                model,
                slot(source),
                boundary(model, insertion_boundary),
                Some(slot(source)),
            )
            .unwrap();
            let expected = single_reference(256, source, insertion_boundary);
            check_complete_plan(&plan, &expected);
            assert_eq!(plan.operation(), PresetMovementOperation::Insertion);
            assert_eq!(plan.insertion_steps().len(), 1);
            assert_eq!(plan.insertion_steps()[0].source, slot(source));
            assert_eq!(plan.insertion_steps()[0].boundary.get(), insertion_boundary);
            assert_eq!(
                plan.insertion_steps()[0].destination,
                plan.destination_for(slot(source)).unwrap()
            );
            assert_eq!(
                plan.is_noop(),
                insertion_boundary == source || insertion_boundary == source + 1
            );
            count += 1;
        }
    }
    assert_eq!(count, 65_792);
}

#[test]
fn all_multiselections_of_two_small_prefixes_match_stable_block_oracle() {
    let actions = [
        PresetMoveAction::Top,
        PresetMoveAction::Bottom,
        PresetMoveAction::Previous,
        PresetMoveAction::Next,
    ];
    let model = DeviceModel::Gp200;
    let mut cases = 0;
    for prefix_size in [6, 8] {
        for mask in 1_u16..(1_u16 << prefix_size) {
            let selected: Vec<_> = (0..prefix_size)
                .filter(|index| mask & (1 << index) != 0)
                .collect();
            let slots: Vec<_> = selected.iter().rev().copied().map(slot).collect();
            for action in actions {
                let expected = block_reference(256, &selected, action);
                // Check representative current policies and then every sound
                // through the complete inverse map, including distant slots.
                for current in [selected[0], selected[selected.len() - 1], 128, 255] {
                    let plan =
                        PresetMovementPlan::selected(model, &slots, action, Some(slot(current)))
                            .unwrap();
                    check_complete_plan(&plan, &expected);
                    assert_eq!(plan.operation(), PresetMovementOperation::Selection(action));
                    assert_eq!(
                        plan.original_selection()
                            .iter()
                            .map(|slot| slot.get())
                            .collect::<Vec<_>>(),
                        selected
                    );
                }
                let plan = PresetMovementPlan::selected(model, &slots, action, None).unwrap();
                for current in 0..256 {
                    let after = plan.destination_for(slot(current)).unwrap();
                    assert_eq!(expected[usize::from(after.get())], current);
                }
                cases += 1;
            }
        }
    }
    assert_eq!(cases, (63 + 255) * 4);
}

#[test]
fn final_destination_conversion_checks_every_gp200_pair() {
    let model = DeviceModel::Gp200;
    for source in 0..256 {
        for destination in 0..256 {
            let plan =
                PresetMovementPlan::to_destination(model, slot(source), slot(destination), None)
                    .unwrap();
            let insertion_boundary = destination + u16::from(destination > source);
            assert_eq!(
                plan.destination_to_source()
                    .iter()
                    .map(|slot| slot.get())
                    .collect::<Vec<_>>(),
                single_reference(256, source, insertion_boundary)
            );
            assert_eq!(plan.destination_for(slot(source)), Some(slot(destination)));
        }
    }
}

#[test]
fn noncontiguous_examples_compact_in_physical_order() {
    let selected = [slot(3), slot(1)];
    let previous = PresetMovementPlan::selected(
        DeviceModel::Gp200,
        &selected,
        PresetMoveAction::Previous,
        Some(slot(2)),
    )
    .unwrap();
    assert_eq!(
        &previous.destination_to_source()[..6],
        &[slot(1), slot(3), slot(0), slot(2), slot(4), slot(5)]
    );
    assert_eq!(previous.final_selection(), &[slot(0), slot(1)]);
    assert_eq!(previous.current_after_original_reselection(), Some(slot(3)));
    let next = PresetMovementPlan::selected(
        DeviceModel::Gp200,
        &selected,
        PresetMoveAction::Next,
        Some(slot(2)),
    )
    .unwrap();
    assert_eq!(
        &next.destination_to_source()[..6],
        &[slot(0), slot(2), slot(4), slot(1), slot(3), slot(5)]
    );
    assert_eq!(next.final_selection(), &[slot(3), slot(4)]);
    assert_eq!(next.current_after_original_reselection(), Some(slot(1)));
    assert_eq!(next.sentinels(), &[slot(0), slot(5)]);
}

#[test]
fn gp200_lt_has_same_arithmetic_bank_and_jr_has_a_distinct_end() {
    for model in [
        DeviceModel::Gp200,
        DeviceModel::Gp200Lt,
        DeviceModel::Gp200Jr,
    ] {
        let count = if model == DeviceModel::Gp200Jr {
            255
        } else {
            256
        };
        let plan = PresetMovementPlan::insert(
            model,
            slot(0),
            boundary(model, count),
            Some(slot(count - 1)),
        )
        .unwrap();
        check_complete_plan(&plan, &single_reference(count, 0, count));
        assert_eq!(plan.model(), model);
        assert_eq!(plan.bank_size(), count);
        assert_eq!(plan.destination_for(slot(0)), Some(slot(count - 1)));
        assert!(plan.sentinels().is_empty());
        for action in [
            PresetMoveAction::Top,
            PresetMoveAction::Bottom,
            PresetMoveAction::Previous,
            PresetMoveAction::Next,
        ] {
            let selected = [1, 4, count - 2];
            let slots: Vec<_> = selected.iter().copied().map(slot).collect();
            let plan = PresetMovementPlan::selected(model, &slots, action, None).unwrap();
            check_complete_plan(&plan, &block_reference(count, &selected, action));
        }
    }
}

#[test]
fn noops_preserve_full_sound_selection_and_current_mapping() {
    let model = DeviceModel::Gp200;
    for source in [0, 17, 254, 255] {
        for insertion_boundary in [source, source + 1] {
            let plan = PresetMovementPlan::insert(
                model,
                slot(source),
                boundary(model, insertion_boundary),
                Some(slot(23)),
            )
            .unwrap();
            check_complete_plan(&plan, &(0..256).collect::<Vec<_>>());
            assert!(plan.is_noop());
            assert_eq!(plan.final_selection(), &[slot(source)]);
            assert_eq!(plan.current_after_original_reselection(), Some(slot(23)));
        }
    }
    let every_slot: Vec<_> = (0..256).map(slot).collect();
    for action in [
        PresetMoveAction::Top,
        PresetMoveAction::Bottom,
        PresetMoveAction::Previous,
        PresetMoveAction::Next,
    ] {
        let plan =
            PresetMovementPlan::selected(model, &every_slot, action, Some(slot(23))).unwrap();
        check_complete_plan(&plan, &(0..256).collect::<Vec<_>>());
        assert!(plan.is_noop());
    }
}

#[test]
fn arbitrary_multiselection_at_edge_still_uses_recovered_compaction() {
    let model = DeviceModel::Gp200;
    let plan =
        PresetMovementPlan::selected(model, &[slot(0), slot(2)], PresetMoveAction::Previous, None)
            .unwrap();
    assert_eq!(
        &plan.destination_to_source()[..4],
        &[slot(0), slot(2), slot(1), slot(3)]
    );
    assert!(!plan.is_noop());
    let plan =
        PresetMovementPlan::selected(model, &[slot(253), slot(255)], PresetMoveAction::Next, None)
            .unwrap();
    assert_eq!(
        &plan.destination_to_source()[252..],
        &[slot(252), slot(254), slot(253), slot(255)]
    );
    assert!(!plan.is_noop());
}

#[test]
fn current_sound_tracks_selected_shifted_and_unchanged_positions() {
    let model = DeviceModel::Gp200;
    for (current, expected) in [(7, 2), (2, 3), (6, 7), (8, 8), (0, 0)] {
        let plan = PresetMovementPlan::to_destination(model, slot(7), slot(2), Some(slot(current)))
            .unwrap();
        assert_eq!(plan.current_before(), Some(slot(current)));
        assert_eq!(
            plan.current_after_original_reselection(),
            Some(slot(expected))
        );
    }
}

#[test]
fn opaque_labels_follow_sound_identity_and_inverse_without_retargeting_them() {
    let model = DeviceModel::Gp200;
    let original: Vec<_> = (0..256)
        .map(|index| format!("sound-{index}-音色-\\0-raw-label"))
        .collect();
    let plan = PresetMovementPlan::selected(
        model,
        &[slot(1), slot(4), slot(7)],
        PresetMoveAction::Next,
        None,
    )
    .unwrap();
    let final_labels: Vec<_> = plan
        .destination_to_source()
        .iter()
        .map(|source| original[usize::from(source.get())].clone())
        .collect();
    for (&source, &destination) in plan.original_selection().iter().zip(plan.final_selection()) {
        assert_eq!(
            final_labels[usize::from(destination.get())],
            original[usize::from(source.get())]
        );
    }
    let mut recovered = vec![String::new(); original.len()];
    for inverse in plan.inverse_mappings() {
        recovered[usize::from(inverse.destination.get())] =
            final_labels[usize::from(inverse.source.get())].clone();
    }
    assert_eq!(recovered, original);
}

#[test]
fn invalid_variant_boundaries_and_slots_cannot_enter_a_plan() {
    let gp = DeviceModel::Gp200;
    let jr = DeviceModel::Gp200Jr;
    assert!(matches!(
        PresetInsertionBoundary::new(gp, 257),
        Err(PresetMovementError::BoundaryOutOfRange { maximum: 256, .. })
    ));
    assert!(matches!(
        PresetInsertionBoundary::new(jr, 256),
        Err(PresetMovementError::BoundaryOutOfRange { maximum: 255, .. })
    ));
    assert!(matches!(
        PresetMovementPlan::insert(jr, slot(0), boundary(gp, 256), None),
        Err(PresetMovementError::BoundaryOutOfRange { maximum: 255, .. })
    ));
    for source in [Some(slot(255)), None] {
        let result = PresetMovementPlan::insert(
            jr,
            source.unwrap_or(slot(0)),
            boundary(jr, 255),
            if source.is_none() {
                Some(slot(255))
            } else {
                None
            },
        );
        assert!(
            matches!(result, Err(PresetMovementError::SlotOutOfRange { slot: invalid, maximum: 254, .. }) if invalid == slot(255))
        );
    }
    assert!(matches!(
        PresetMovementPlan::to_destination(jr, slot(0), slot(255), None),
        Err(PresetMovementError::SlotOutOfRange { .. })
    ));
    assert!(matches!(
        PresetMovementPlan::selected(jr, &[slot(255)], PresetMoveAction::Top, None),
        Err(PresetMovementError::SlotOutOfRange { .. })
    ));
    let valid = PresetMovementPlan::insert(jr, slot(0), boundary(jr, 255), None).unwrap();
    assert_eq!(valid.destination_for(slot(255)), None);
    assert_eq!(valid.original_source_for(slot(255)), None);
    assert!(matches!(
        PresetInsertionBoundary::new(DeviceModel::Unknown(99), 0),
        Err(PresetMovementError::UnsupportedModel(DeviceModel::Unknown(
            99
        )))
    ));
    assert!(matches!(
        PresetMovementPlan::insert(DeviceModel::Unknown(99), slot(0), boundary(gp, 0), None),
        Err(PresetMovementError::UnsupportedModel(DeviceModel::Unknown(
            99
        )))
    ));
    assert!(matches!(
        PresetMovementPlan::selected(
            DeviceModel::Unknown(99),
            &[slot(0)],
            PresetMoveAction::Top,
            None
        ),
        Err(PresetMovementError::UnsupportedModel(DeviceModel::Unknown(
            99
        )))
    ));
}

#[test]
fn empty_duplicate_and_oversized_selections_are_rejected() {
    let model = DeviceModel::Gp200;
    assert_eq!(
        PresetMovementPlan::selected(model, &[], PresetMoveAction::Top, None),
        Err(PresetMovementError::EmptySelection)
    );
    assert_eq!(
        PresetMovementPlan::selected(
            model,
            &[slot(2), slot(0), slot(2)],
            PresetMoveAction::Top,
            None
        ),
        Err(PresetMovementError::DuplicateSelection(slot(2)))
    );
    assert_eq!(
        PresetMovementPlan::selected(model, &vec![slot(0); 257], PresetMoveAction::Top, None),
        Err(PresetMovementError::SelectionTooLarge {
            selected: 257,
            maximum: 256
        })
    );
}
