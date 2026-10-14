use gp200_catalog::{
    Catalog, ControlKind, EMBEDDED_CATALOG_JSON, Favorites, ModelKey, PresentationState, SlotKind,
    format_value, module_id, module_name,
};

fn catalog() -> Catalog {
    Catalog::embedded().expect("the generated real catalog validates")
}

#[test]
fn every_gp200_control_is_available_and_ordered_separately_from_ids() {
    let catalog = catalog();
    assert_eq!(catalog.variant(), "GP-200");
    assert_eq!(catalog.model_count(), 322);
    assert_eq!(catalog.parameter_count(), 1400);
    let expected = [31, 6, 43, 76, 4, 90, 6, 28, 22, 15, 1];
    let mut differing_ids = 0;
    for (id, count) in expected.into_iter().enumerate() {
        let module = catalog.module_by_id(id as u8).unwrap();
        assert_eq!(module.models.len(), count);
        assert_eq!(module.module_id(), Some(id as u8));
        for model in &module.models {
            assert!(model.description().is_none());
            assert!(!model.additional.contains_key("raw_description_xml"));
            assert_eq!(
                catalog.model_by_id(id as u8, model.code).unwrap().name,
                model.name
            );
            for parameter in model.parameters_in_display_order() {
                assert_ne!(parameter.control_kind(), ControlKind::Unknown);
                assert_eq!(
                    model.parameter(parameter.parameter_id).unwrap().name,
                    parameter.name
                );
                if parameter.parameter_id != parameter.display_index {
                    differing_ids += 1;
                }
            }
        }
    }
    assert_eq!(differing_ids, 293);
}

#[test]
fn cab_sparse_xml_ids_read_correct_live_float_slots() {
    let catalog = catalog();
    let model = catalog.model("CAB", 0x0a00_0010).unwrap();
    assert_eq!(model.name, "Match");
    assert_eq!(
        model
            .parameters_in_display_order()
            .iter()
            .map(|parameter| parameter.parameter_id)
            .collect::<Vec<_>>(),
        vec![1, 5, 6]
    );
    let mut values = [0.0; 15];
    values[1] = 60.0;
    values[5] = 100.0;
    values[6] = 20001.0;
    assert_eq!(
        format_value(model.parameter(1).unwrap(), values[1], &values).text,
        "60"
    );
    assert_eq!(
        format_value(model.parameter(5).unwrap(), values[5], &values).text,
        "100 Hz"
    );
    let high_cut = format_value(model.parameter(6).unwrap(), values[6], &values);
    assert_eq!(high_cut.text, "OFF");
    assert_eq!(high_cut.state, PresentationState::Sentinel);
    assert!(high_cut.editable);
    assert_eq!(high_cut.raw_bits, 20001.0f32.to_bits());
    assert!(!high_cut.text.contains("Hz"));
}

#[test]
fn model_xml_index_is_not_code_and_optional_descriptions_use_independent_indices() {
    let catalog = catalog();
    let model = catalog.model("AMP", 117440533).unwrap();
    assert_eq!(model.name, "Match CL");
    assert_eq!(model.xml_index, 13);
    assert_eq!(model.display_index, 13);
    assert_eq!(model.raw_attributes["Index"], "13");
    assert_ne!(u32::from(model.xml_index), model.code);
    let cab = catalog.model("CAB", 167772176).unwrap();
    assert_eq!(cab.display_index, 25);
    assert!(cab.description().is_none());
    // The parser still supports caller-supplied descriptions. This invented
    // fixture checks the independent index without republishing vendor prose.
    let mut source: serde_json::Value = serde_json::from_str(EMBEDDED_CATALOG_JSON).unwrap();
    let cab = source["modules"][5]["models"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|model| model["code"] == 167772176)
        .unwrap();
    cab["english_descriptions"] = serde_json::json!([{
        "text": "Synthetic optional description", "name": "Match", "index": 10,
        "raw_attributes": {"ID": "167772176"},
        "source": {"file": "synthetic-test", "line": 1}
    }]);
    let supplied = Catalog::from_json(&source.to_string()).unwrap();
    let cab = supplied.model("CAB", 167772176).unwrap();
    assert_eq!(cab.display_index, 25);
    assert_eq!(cab.english_descriptions[0].index, 10);
    assert_eq!(cab.description(), Some("Synthetic optional description"));
}

#[test]
fn synced_rate_and_time_never_render_as_ordinary_units() {
    let catalog = catalog();
    for (module, code, id, sync_id) in [("MOD", 67108885, 1, 2), ("DLY", 184549387, 1, 3)] {
        let model = catalog.model(module, code).unwrap();
        let parameter = model.parameter(id).unwrap();
        let mut values = [0.0; 15];
        values[id as usize] = 4.0;
        values[sync_id as usize] = 1.0;
        let presentation = parameter.presentation(4.0, &values);
        assert_eq!(presentation.state, PresentationState::SyncSubdivision);
        assert_eq!(presentation.text, "1/4");
        assert!(presentation.editable);
        assert_eq!(
            catalog.format_value(parameter, 4.0).state,
            PresentationState::MissingBinding
        );
        values[sync_id as usize] = 2.0;
        assert_eq!(
            parameter.presentation(4.0, &values).state,
            PresentationState::UnknownBinding
        );
    }
}

#[test]
fn nonsynced_value_uses_declared_range_without_clamping() {
    let catalog = catalog();
    let model = catalog.model("DLY", 184549387).unwrap();
    let parameter = model.parameter(1).unwrap();
    let values = [0.0; 15];
    assert_eq!(parameter.presentation(500.0, &values).text, "500 ms");
    let out_of_range = parameter.presentation(4.0, &values);
    assert_eq!(out_of_range.text, "Raw 4");
    assert_eq!(out_of_range.state, PresentationState::OutOfRange);
    assert!(!out_of_range.editable);
}

#[test]
fn unknown_choice_nan_payload_signed_zero_are_retained() {
    let catalog = catalog();
    let model = catalog.model("DLY", 184549387).unwrap();
    let switch = model.parameter(3).unwrap();
    assert_eq!(switch.presentation(1.0, &[]).text, "ON");
    assert_eq!(
        switch.presentation(7.0, &[]).state,
        PresentationState::UnknownChoice
    );
    let number = model.parameter(0).unwrap();
    let nan = f32::from_bits(0x7fc1_1234);
    assert_eq!(number.presentation(nan, &[]).raw_bits, 0x7fc1_1234);
    assert_eq!(
        number.presentation(nan, &[]).state,
        PresentationState::NonFinite
    );
    assert_eq!(number.presentation(-0.0, &[]).raw_bits, 0x8000_0000);
}

#[test]
fn declared_minimum_off_formats_without_changing_raw_bits() {
    let catalog = catalog();
    let model = catalog.model("CAB", 167772176).unwrap();
    let low_cut = model.parameter(5).unwrap();
    let sentinel = low_cut.presentation(19.0, &[]);
    assert_eq!(sentinel.state, PresentationState::Sentinel);
    assert_eq!(sentinel.text, "OFF");
    assert!(sentinel.editable);
    assert_eq!(low_cut.raw_attributes["MinISOff"], "1");
}

#[test]
fn defaults_are_sparse_and_unparsed_text_is_not_silently_guessed() {
    let catalog = catalog();
    let cab = catalog.model("CAB", 167772176).unwrap();
    let original = [0x7fc1_2345u32; 15];
    let mut values = original;
    cab.apply_declared_defaults(&mut values);
    assert_eq!(values[1], 50.0f32.to_bits());
    assert_eq!(values[5], 19.0f32.to_bits());
    assert_eq!(values[6], 20001.0f32.to_bits());
    for id in [0, 2, 3, 4, 7, 8, 9, 10, 11, 12, 13, 14] {
        assert_eq!(values[id], original[id]);
    }
    let detune = catalog.model("MOD", 16777257).unwrap();
    let parameter = detune.parameter(0).unwrap();
    assert_eq!(parameter.default, None);
    assert_eq!(parameter.raw_default(), Some("-25 Cents"));
    assert!(!detune.default_values().contains_key(&0));
}

#[test]
fn search_and_favorites_use_module_plus_code_identity() {
    let catalog = catalog();
    let matches = catalog.search_models(Some(3), "match");
    assert_eq!(matches.len(), 2);
    assert!(matches.iter().all(|model| model.module == "AMP"));
    assert!(!catalog.search_models(None, "match cl").is_empty());
    assert!(
        catalog
            .search_models(None, "impossible ZZZZquery")
            .is_empty()
    );
    assert_eq!(catalog.search_models(None, "").len(), 322);
    let mut favorites = Favorites::default();
    let model = catalog.model("AMP", 117440533).unwrap();
    assert!(favorites.toggle(model.key()));
    assert!(favorites.contains(model.key()));
    assert_eq!(catalog.favorite_models(&favorites)[0].name, "Match CL");
    favorites.set(
        ModelKey {
            module: 3,
            code: 0xffff_ffff,
        },
        true,
    );
    assert_eq!(catalog.favorite_models(&favorites).len(), 1); // Unknown keys survive persistence.
    let restored: Favorites =
        serde_json::from_str(&serde_json::to_string(&favorites).unwrap()).unwrap();
    assert!(restored.contains(ModelKey {
        module: 3,
        code: 0xffff_ffff
    }));
    assert!(!favorites.toggle(model.key()));
}

#[test]
fn user_slots_are_distinct_from_factory_algorithms() {
    let catalog = catalog();
    assert_eq!(
        catalog
            .module("CAB")
            .unwrap()
            .models
            .iter()
            .filter(|model| matches!(model.slot_kind(), SlotKind::UserIr { .. }))
            .count(),
        20
    );
    assert_eq!(
        catalog
            .modules()
            .iter()
            .flat_map(|module| &module.models)
            .filter(|model| matches!(model.slot_kind(), SlotKind::SnapTone { .. }))
            .count(),
        10
    );
    assert_eq!(
        catalog.model("CAB", 0x0a10_0013).unwrap().slot_kind(),
        SlotKind::UserIr { slot: 19 }
    );
    assert_eq!(
        catalog.model("DST", 0x0f00_0009).unwrap().slot_kind(),
        SlotKind::SnapTone { slot: 9 }
    );
    assert_eq!(
        catalog.model("AMP", 117440533).unwrap().slot_kind(),
        SlotKind::Factory
    );
}

#[test]
fn unknown_model_module_and_id_are_not_fabricated() {
    let catalog = catalog();
    assert!(catalog.model("AMP", 0xffff_ffff).is_none());
    assert!(catalog.model_by_id(255, 117440533).is_none());
    assert!(
        catalog
            .model("AMP", 117440533)
            .unwrap()
            .parameter(14)
            .is_none()
    );
    assert_eq!(module_name(3), Some("AMP"));
    assert_eq!(module_id("AMP"), Some(3));
    assert_eq!(module_name(11), None);
}

#[test]
fn parser_rejects_ambiguous_model_control_and_description_keys() {
    let original: serde_json::Value = serde_json::from_str(EMBEDDED_CATALOG_JSON).unwrap();
    let mut duplicate = original.clone();
    let model = duplicate["modules"][0]["models"][0].clone();
    duplicate["modules"][0]["models"]
        .as_array_mut()
        .unwrap()
        .push(model);
    assert!(Catalog::from_json(&duplicate.to_string()).is_err());
    let mut wrong_id = original.clone();
    wrong_id["modules"][0]["models"][0]["parameters"][0]["parameter_id"] = 1.into();
    assert!(Catalog::from_json(&wrong_id.to_string()).is_err());
    let mut wrong_description = original.clone();
    wrong_description["modules"][0]["models"][0]["english_descriptions"] = serde_json::json!([{
        "text": "Synthetic optional description", "name": "COMP", "index": 0,
        "raw_attributes": {"ID": "55"},
        "source": {"file": "synthetic-test", "line": 1}
    }]);
    assert!(Catalog::from_json(&wrong_description.to_string()).is_err());
    let mut wrong_index = original;
    wrong_index["modules"][0]["models"][0]["raw_attributes"]["Index"] = "3".into();
    assert!(Catalog::from_json(&wrong_index.to_string()).is_err());
}

#[test]
fn integral_decimal_ids_preserve_original_attribute_spelling() {
    let catalog = catalog();
    let parameter = catalog
        .modules()
        .iter()
        .flat_map(|module| &module.models)
        .flat_map(|model| &model.parameters)
        .find(|parameter| {
            parameter
                .raw_attributes
                .get("ID")
                .is_some_and(|id| id == "0.0")
        })
        .unwrap();
    assert_eq!(parameter.parameter_id, 0);
    assert_eq!(parameter.raw_attributes["ID"], "0.0");
}
