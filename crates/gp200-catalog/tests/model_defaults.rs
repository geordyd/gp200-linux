use gp200_catalog::{Catalog, ModelDefaultPlan};

fn body(payload: &[u8], module: u8) -> Vec<u8> {
    let mut offset = 0;
    while offset < payload.len() {
        let command = u16::from_le_bytes(payload[offset..offset + 2].try_into().unwrap());
        let length =
            u16::from_le_bytes(payload[offset + 2..offset + 4].try_into().unwrap()) as usize;
        let bytes = &payload[offset + 4..offset + 4 + length];
        if command == 0x14 && length == 68 && bytes[0] == module {
            return bytes.to_vec();
        }
        offset += 4 + length;
    }
    panic!("module absent");
}

#[test]
fn sparse_defaults_match_independent_complete_hardware_module_readbacks() {
    let catalog = Catalog::embedded().unwrap();
    let fixtures = [
        (
            4,
            include_bytes!("../../../tests/fixtures/model-transitions/nr-before.payload.bin")
                .as_slice(),
            include_bytes!("../../../tests/fixtures/model-transitions/nr-after.payload.bin")
                .as_slice(),
        ),
        (
            5,
            include_bytes!("../../../tests/fixtures/model-transitions/cab-before.payload.bin")
                .as_slice(),
            include_bytes!("../../../tests/fixtures/model-transitions/cab-after.payload.bin")
                .as_slice(),
        ),
        (
            6,
            include_bytes!("../../../tests/fixtures/model-transitions/eq-before.payload.bin")
                .as_slice(),
            include_bytes!("../../../tests/fixtures/model-transitions/eq-after.payload.bin")
                .as_slice(),
        ),
        (
            7,
            include_bytes!("../../../tests/fixtures/model-transitions/mod-before.payload.bin")
                .as_slice(),
            include_bytes!("../../../tests/fixtures/model-transitions/mod-after.payload.bin")
                .as_slice(),
        ),
        (
            8,
            include_bytes!("../../../tests/fixtures/model-transitions/dly-before.payload.bin")
                .as_slice(),
            include_bytes!("../../../tests/fixtures/model-transitions/dly-after.payload.bin")
                .as_slice(),
        ),
        (
            9,
            include_bytes!("../../../tests/fixtures/model-transitions/rvb-before.payload.bin")
                .as_slice(),
            include_bytes!("../../../tests/fixtures/model-transitions/rvb-after.payload.bin")
                .as_slice(),
        ),
        (
            6,
            include_bytes!("../../../tests/fixtures/model-transitions/eq-after.payload.bin")
                .as_slice(),
            include_bytes!("../../../tests/fixtures/model-transitions/eq-return.payload.bin")
                .as_slice(),
        ),
        (
            8,
            include_bytes!("../../../tests/fixtures/model-transitions/dly-after.payload.bin")
                .as_slice(),
            include_bytes!("../../../tests/fixtures/model-transitions/dly-return.payload.bin")
                .as_slice(),
        ),
        (
            5,
            include_bytes!(
                "../../../tests/fixtures/model-transitions/cab-return-before.payload.bin"
            )
            .as_slice(),
            include_bytes!("../../../tests/fixtures/model-transitions/cab-return.payload.bin")
                .as_slice(),
        ),
        (
            7,
            include_bytes!(
                "../../../tests/fixtures/model-transitions/mod-return-before.payload.bin"
            )
            .as_slice(),
            include_bytes!("../../../tests/fixtures/model-transitions/mod-return.payload.bin")
                .as_slice(),
        ),
        (
            9,
            include_bytes!(
                "../../../tests/fixtures/model-transitions/rvb-return-before.payload.bin"
            )
            .as_slice(),
            include_bytes!("../../../tests/fixtures/model-transitions/rvb-return.payload.bin")
                .as_slice(),
        ),
    ];
    for (module, before, after) in fixtures {
        let actual = body(after, module);
        let code = u32::from_le_bytes(actual[4..8].try_into().unwrap());
        let plan = ModelDefaultPlan::from_definition(catalog.model_by_id(module, code).unwrap());
        assert!(plan.unknown_defaults.is_empty());
        let mut expected = body(before, module);
        plan.apply_to_module_body(&mut expected).unwrap();
        assert_eq!(expected, actual, "module {module}, target {code:#010x}");
    }
}

#[test]
fn undeclared_nan_payload_signed_zero_and_unknown_defaults_are_not_invented() {
    let catalog = Catalog::embedded().unwrap();
    let mut model = catalog.model_by_id(5, 0x0a000000).unwrap().clone();
    model
        .parameters
        .iter_mut()
        .find(|p| p.parameter_id == 5)
        .unwrap()
        .default = None;
    let plan = ModelDefaultPlan::from_definition(&model);
    assert_eq!(plan.unknown_defaults, [5]);
    let mut bits = vec![0x7fc01234; 15];
    bits[5] = 0x80000000;
    plan.apply_to_bits(&mut bits).unwrap();
    assert_eq!(bits[0], 0x7fc01234);
    assert_eq!(bits[5], 0x80000000);
    assert_eq!(bits[1], 50_f32.to_bits());
    assert_eq!(bits[6], 20001_f32.to_bits());
}

#[test]
fn incomplete_arrays_and_record_shapes_fail_before_any_overlay() {
    let catalog = Catalog::embedded().unwrap();
    let plan = ModelDefaultPlan::from_definition(catalog.model_by_id(5, 0x0a000000).unwrap());
    let mut short = vec![0x7fc01234; 6];
    let original = short.clone();
    assert!(plan.apply_to_bits(&mut short).is_err());
    assert_eq!(short, original);
    let mut record = vec![0xaa; 68];
    let original = record.clone();
    assert!(plan.apply_to_module_body(&mut record).is_err());
    assert_eq!(record, original);
}
