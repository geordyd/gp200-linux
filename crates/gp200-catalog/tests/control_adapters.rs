use gp200_catalog::{
    Catalog, ControlError, ControlRequest, ControlSemantics, OrdinaryRestore, PresentationState,
    SYNC_SUBDIVISIONS, SentinelKind, control_semantics, plan_control_request, recognized_sentinel,
    sentinel_value, subdivision_index,
};

fn catalog() -> Catalog {
    Catalog::embedded().unwrap()
}

#[test]
fn all_1400_descriptors_have_explicit_recovered_semantics_and_plan_coverage() {
    let catalog = catalog();
    let mut counts = [0usize; 5];
    let mut sentinels = 0;
    for model in catalog.modules().iter().flat_map(|m| &m.models) {
        for parameter in &model.parameters {
            let mut bits = [0x7fc12345; 15];
            let ordinary = parameter
                .default
                .or(parameter.minimum)
                .unwrap_or(0.0)
                .to_bits();
            bits[usize::from(parameter.parameter_id)] = ordinary;
            let request = match control_semantics(parameter) {
                ControlSemantics::Numeric => {
                    counts[0] += 1;
                    ControlRequest::RawBits(ordinary)
                }
                ControlSemantics::Choice => {
                    counts[1] += 1;
                    ControlRequest::Choice(parameter.choices[0].id)
                }
                ControlSemantics::Switch => {
                    counts[2] += 1;
                    ControlRequest::Choice(parameter.choices[0].id)
                }
                ControlSemantics::SyncNumeric { power_parameter } => {
                    counts[3] += 1;
                    bits[usize::from(power_parameter)] = 1.0f32.to_bits();
                    ControlRequest::Subdivision(4)
                }
                ControlSemantics::SyncPower { value_parameter } => {
                    counts[4] += 1;
                    bits[usize::from(parameter.parameter_id)] = 0.0f32.to_bits();
                    bits[usize::from(value_parameter)] = model
                        .parameter(value_parameter)
                        .unwrap()
                        .default
                        .unwrap()
                        .to_bits();
                    ControlRequest::Sync {
                        enabled: true,
                        ordinary_restore: None,
                    }
                }
                ControlSemantics::Unsupported => {
                    panic!("{} {} unsupported", model.name, parameter.name)
                }
            };
            let plan = plan_control_request(model, parameter.parameter_id, &bits, request).unwrap();
            assert_eq!(plan.model, model.key());
            assert_eq!(plan.model_xml_index, model.xml_index);
            assert_eq!(plan.before_values, bits);
            for after in &plan.expected_after_steps {
                for id in 0..15 {
                    if !plan
                        .steps
                        .iter()
                        .any(|step| usize::from(step.parameter_id) == id)
                    {
                        assert_eq!(after[id], bits[id], "undefined/unrelated float was changed");
                    }
                }
            }
            if let Some(value) = sentinel_value(parameter, SentinelKind::Off) {
                sentinels += 1;
                let plan = plan_control_request(
                    model,
                    parameter.parameter_id,
                    &bits,
                    ControlRequest::Sentinel(SentinelKind::Off),
                )
                .unwrap();
                assert_eq!(plan.steps[0].raw_bits, value.to_bits());
            }
        }
    }
    assert_eq!(counts, [1213, 32, 57, 49, 49]);
    assert_eq!(counts.iter().sum::<usize>(), 1400);
    assert_eq!(sentinels, 180);
}

#[test]
fn every_declared_choice_uses_its_id_and_unknown_values_fail() {
    let catalog = catalog();
    for model in catalog.modules().iter().flat_map(|m| &m.models) {
        for parameter in &model.parameters {
            if !matches!(
                control_semantics(parameter),
                ControlSemantics::Choice | ControlSemantics::Switch
            ) {
                continue;
            }
            for choice in &parameter.choices {
                let plan = plan_control_request(
                    model,
                    parameter.parameter_id,
                    &[0; 15],
                    ControlRequest::Choice(choice.id),
                )
                .unwrap();
                assert_eq!(plan.steps[0].raw_bits, (choice.id as f32).to_bits());
                assert_eq!(
                    parameter
                        .presentation(f32::from_bits(plan.steps[0].raw_bits), &[])
                        .text,
                    choice.name
                );
            }
            assert_eq!(
                plan_control_request(
                    model,
                    parameter.parameter_id,
                    &[0; 15],
                    ControlRequest::RawBits(9999.0f32.to_bits())
                )
                .unwrap_err(),
                ControlError::UnknownChoice
            );
        }
    }
}

#[test]
fn static_sync_literal_oracle_covers_every_value_and_native_midpoint_ties() {
    let expected = [
        "1/1", "1/2", "1/2D", "1/2T", "1/4", "1/4D", "1/4T", "1/8", "1/8D", "1/8T", "1/16",
    ];
    assert_eq!(SYNC_SUBDIVISIONS, expected);
    let catalog = catalog();
    let model = catalog.model("DLY", 0x0b00000b).unwrap();
    let time = model.parameter(1).unwrap();
    let mut values = [0.0; 15];
    values[3] = 1.0;
    for (index, label) in expected.iter().enumerate() {
        let p = time.presentation(index as f32, &values);
        assert_eq!(p.text, *label);
        assert_eq!(p.raw_bits, (index as f32).to_bits());
        assert_eq!(p.state, PresentationState::SyncSubdivision);
    }
    for index in 0..10 {
        let midpoint = index as f32 + 0.5;
        assert_eq!(subdivision_index(midpoint), Some(index));
        assert_eq!(
            subdivision_index(f32::from_bits(midpoint.to_bits() + 1)),
            Some(index + 1)
        );
    }
    for invalid in [-1.0, 11.0, f32::NAN, f32::INFINITY] {
        assert_eq!(subdivision_index(invalid), None);
    }
}

#[test]
fn sync_requests_are_ordered_and_missing_cache_is_never_invented() {
    let catalog = catalog();
    let model = catalog.model("MOD", 0x04000015).unwrap();
    let mut values = [0x7fc11234; 15];
    values[1] = 0.7f32.to_bits();
    values[2] = 0.0f32.to_bits();
    let on = plan_control_request(
        model,
        2,
        &values,
        ControlRequest::Sync {
            enabled: true,
            ordinary_restore: None,
        },
    )
    .unwrap();
    assert_eq!(
        on.steps
            .iter()
            .map(|x| (x.parameter_id, x.raw_bits))
            .collect::<Vec<_>>(),
        [(2, 1.0f32.to_bits()), (1, 4.0f32.to_bits())]
    );
    assert_eq!(on.expected_after_steps[0][1], values[1]);
    values = on.expected_after_steps[1].clone().try_into().unwrap();
    assert_eq!(
        plan_control_request(
            model,
            2,
            &values,
            ControlRequest::Sync {
                enabled: false,
                ordinary_restore: None
            }
        )
        .unwrap_err(),
        ControlError::MissingOrdinaryRestore
    );
    let off = plan_control_request(
        model,
        2,
        &values,
        ControlRequest::Sync {
            enabled: false,
            ordinary_restore: Some(OrdinaryRestore::ObservedInSameContext(0.7f32.to_bits())),
        },
    )
    .unwrap();
    assert_eq!(
        off.steps
            .iter()
            .map(|x| (x.parameter_id, x.raw_bits))
            .collect::<Vec<_>>(),
        [(2, 0.0f32.to_bits()), (1, 0.7f32.to_bits())]
    );
    assert_eq!(
        plan_control_request(model, 2, &values, ControlRequest::Choice(0)).unwrap_err(),
        ControlError::ExplicitSyncActionRequired
    );
    assert_eq!(
        plan_control_request(
            model,
            2,
            &values,
            ControlRequest::Sync {
                enabled: true,
                ordinary_restore: None
            }
        )
        .unwrap()
        .steps
        .len(),
        2
    );
}

#[test]
fn sentinel_endpoint_oracles_keep_full_bits_and_do_not_apply_frequency_scale() {
    let catalog = catalog();
    let cab = catalog.model("CAB", 0x0a000010).unwrap();
    for (id, off, ordinary) in [(5, 19.0, 100.0), (6, 20001.0, 20000.0)] {
        let parameter = cab.parameter(id).unwrap();
        assert_eq!(parameter.presentation(off, &[]).text, "OFF");
        assert_eq!(parameter.presentation(off, &[]).raw_bits, off.to_bits());
        assert_eq!(
            parameter.presentation(ordinary, &[]).text,
            format!("{ordinary} Hz")
        );
    }
    let pitch = catalog
        .model("PRE", 0x01000023)
        .unwrap()
        .parameter(1)
        .unwrap();
    assert_eq!(pitch.presentation(-12.0, &[]).text, "-12");
    let mut mute = pitch.clone();
    mute.raw_attributes.insert("MinISMute".into(), "1".into());
    assert_eq!(recognized_sentinel(&mute, -24.0), Some(SentinelKind::Mute));
    assert_eq!(mute.presentation(-24.0, &[]).text, "Mute");
}

#[test]
fn ordinary_float_offgrid_values_truncate_only_the_label_and_preserve_nan_signed_zero() {
    let catalog = catalog();
    let model = catalog.model("WAH", 0x0500000a).unwrap();
    let position = model.parameter(3).unwrap();
    let raw = f32::from_bits(0x42c6b5c9);
    assert_eq!(position.presentation(raw, &[]).text, "99.35");
    let plan =
        plan_control_request(model, 3, &[0; 15], ControlRequest::RawBits(raw.to_bits())).unwrap();
    assert_eq!(plan.steps[0].raw_bits, 0x42c6b5c9);
    assert_eq!(position.presentation(-0.0, &[]).raw_bits, 0x80000000);
    assert_eq!(
        position
            .presentation(f32::from_bits(0x7fc01234), &[])
            .raw_bits,
        0x7fc01234
    );
    assert_eq!(
        plan_control_request(model, 3, &[0; 15], ControlRequest::RawBits(0x7fc01234)).unwrap_err(),
        ControlError::InvalidValue
    );
}

#[test]
fn incomplete_unknown_and_ambiguous_reverse_contexts_are_explicit_failures() {
    let catalog = catalog();
    let model = catalog.model("DLY", 0x0b00000b).unwrap();
    assert_eq!(
        plan_control_request(model, 14, &[0; 15], ControlRequest::RawBits(0)).unwrap_err(),
        ControlError::UnknownParameter(14)
    );
    assert_eq!(
        plan_control_request(model, 1, &[], ControlRequest::Subdivision(4)).unwrap_err(),
        ControlError::MissingValue(1)
    );
    let mut bits = [0; 15];
    bits[3] = 2.0f32.to_bits();
    assert_eq!(
        plan_control_request(model, 1, &bits, ControlRequest::Subdivision(4)).unwrap_err(),
        ControlError::UnknownSyncState
    );
    bits[3] = 1.0f32.to_bits();
    assert_eq!(
        plan_control_request(model, 1, &bits, ControlRequest::RawBits(500.0f32.to_bits()))
            .unwrap_err(),
        ControlError::InvalidValue
    );
    let mut broken = model.clone();
    let mut duplicate = broken.parameter(1).unwrap().clone();
    duplicate.parameter_id = 2;
    broken.parameters.retain(|p| p.parameter_id != 2);
    broken.parameters.push(duplicate);
    assert_eq!(
        plan_control_request(&broken, 1, &bits, ControlRequest::Subdivision(4)).unwrap_err(),
        ControlError::MissingBinding
    );
}

#[test]
fn three_shipped_power_binding_typoes_use_the_native_reverse_xml_id_lookup() {
    let catalog = catalog();
    for code in [0x0b000004, 0x0b000006, 0x0b000014] {
        let model = catalog.model("DLY", code).unwrap();
        let time = model.parameter(2).unwrap();
        let power_id = time.bound_parameter_id.unwrap();
        assert_eq!(
            model.parameter(power_id).unwrap().bound_parameter_id,
            Some(1)
        );
        assert_eq!(
            gp200_catalog::sync_value_parameter(model, power_id).unwrap(),
            2
        );
        let mut bits = [0; 15];
        bits[2] = 500.0f32.to_bits();
        let plan = plan_control_request(
            model,
            power_id,
            &bits,
            ControlRequest::Sync {
                enabled: true,
                ordinary_restore: None,
            },
        )
        .unwrap();
        assert_eq!(plan.steps[1].parameter_id, 2);
        assert_eq!(plan.expected_after_steps[1][1], 0);
        assert_eq!(plan.expected_after_steps[1][2], 4.0f32.to_bits());
    }
}
