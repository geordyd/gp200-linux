use gp200_domain::{DeviceGlobalState, GlobalControlEdit, Revision, SessionEpoch};
use gp200_protocol::*;
use gp200_sim::{Delivery, SimulatedPedal};
use std::time::Duration;

fn transact(pedal: &mut SimulatedPedal, request: &RawTransfer, ms: u64) -> Vec<Delivery> {
    encode_transfer(request)
        .unwrap()
        .into_iter()
        .flat_map(|p| pedal.receive(&p, Duration::from_millis(ms)).unwrap())
        .collect()
}
fn decode(deliveries: Vec<Delivery>) -> RawTransfer {
    let mut d = StreamDecoder::new();
    let mut a = Reassembler::default();
    for delivery in deliveries {
        if let Delivery::Bytes { at, bytes } = delivery {
            for e in d.feed(&bytes) {
                if let StreamEvent::Packet(p) = e
                    && let Some(raw) = a.add(&p, at).unwrap()
                {
                    return raw;
                }
            }
        }
    }
    panic!("complete reply missing")
}
fn state(raw: RawTransfer) -> DeviceGlobalState {
    DeviceGlobalState::from_transfer(Revision(1), SessionEpoch(0), raw, vec![]).unwrap()
}

#[test]
fn select_and_save_as_keep_complete_global_readback_and_current_context_consistent() {
    let mut pedal = SimulatedPedal::captured();
    let initial_globals = pedal.globals().clone();
    let original_stored = pedal.stored(23).unwrap().clone();
    let source_stored = pedal.stored(8).unwrap().clone();
    for (index, (slot, request)) in [
        (8, select_stored_request(8).unwrap()),
        (
            221,
            save_buffer_request(221, *b"Selected target\0").unwrap(),
        ),
    ]
    .into_iter()
    .enumerate()
    {
        assert!(transact(&mut pedal, &request, 1 + index as u64 * 2).is_empty());
        let actual = decode(transact(
            &mut pedal,
            &read_globals_request().unwrap(),
            2 + index as u64 * 2,
        ));
        assert_eq!(pedal.selected_slot(), slot);
        assert_eq!(
            WireContext::from_transfer(pedal.current()),
            Some(WireContext {
                group: 0,
                selector: slot
            })
        );
        let general = actual
            .records()
            .iter()
            .find(|record| record.command == GENERAL_SETTINGS_COMMAND)
            .unwrap();
        assert_eq!(
            u16::from_le_bytes(general.body[4..6].try_into().unwrap()),
            slot
        );
        // Selection/save may change only the selected-slot word of globals;
        // no EQ/drum/FS/opaque bytes may follow the new preset by accident.
        let mut expected_records = initial_globals.records().to_vec();
        expected_records
            .iter_mut()
            .find(|record| record.command == GENERAL_SETTINGS_COMMAND)
            .unwrap()
            .body[4..6]
            .copy_from_slice(&slot.to_le_bytes());
        assert_eq!(
            actual,
            RawTransfer::from_records(WRITE_KIND, &expected_records).unwrap()
        );
        assert_eq!(pedal.stored(23), Some(&original_stored));
        assert_eq!(pedal.stored(8), Some(&source_stored));
    }
    let saved = decode(transact(&mut pedal, &read_stored_request(221).unwrap(), 5));
    assert_eq!(
        WireContext::from_transfer(&saved),
        Some(WireContext {
            group: 1,
            selector: 221
        })
    );
    assert_eq!(
        &saved
            .records()
            .iter()
            .find(|record| record.command == 2)
            .unwrap()
            .body[16..32],
        b"Selected target\0"
    );
}

#[test]
fn invalid_select_or_save_does_not_advance_global_selected_slot() {
    let mut pedal = SimulatedPedal::captured();
    let globals = pedal.globals().clone();
    let current = pedal.current().clone();
    let stored = pedal.stored(23).unwrap().clone();
    for (index, command) in [0x1008, 0x2003].into_iter().enumerate() {
        let mut body = if command == 0x1008 {
            vec![0, 0, 0, 1]
        } else {
            vec![0; 20]
        };
        if command == 0x2003 {
            body[..2].copy_from_slice(&256u16.to_le_bytes());
        }
        let request =
            RawTransfer::from_records(WRITE_KIND, &[RawRecord { command, body }]).unwrap();
        let packet = encode_transfer(&request).unwrap().remove(0);
        assert!(
            pedal
                .receive(&packet, Duration::from_millis(1 + index as u64))
                .is_err()
        );
        assert_eq!(pedal.selected_slot(), 23);
        assert_eq!(pedal.globals(), &globals);
        assert_eq!(pedal.current(), &current);
        assert_eq!(pedal.stored(23), Some(&stored));
        pedal.reset_stream();
    }
}

#[test]
fn global_commands_use_the_production_codec_and_preserve_all_preset_bytes() {
    let mut pedal = SimulatedPedal::captured();
    let current = pedal.current().clone();
    let stored = pedal.stored(23).unwrap().clone();
    let initial = decode(transact(&mut pedal, &read_globals_request().unwrap(), 1));
    assert_eq!(
        initial.payload(),
        include_bytes!("../../../tests/fixtures/globals/baseline.payload.bin")
    );
    let edits = [
        GlobalControlEdit::General {
            field: GeneralSetting::InputLevel,
            value: -1,
        },
        GlobalControlEdit::General {
            field: GeneralSetting::MidiInputSource,
            value: 1,
        },
        GlobalControlEdit::General {
            field: GeneralSetting::ExternalMode,
            value: 2,
        },
        GlobalControlEdit::General {
            field: GeneralSetting::ExternalFootswitch1,
            value: 22,
        },
        GlobalControlEdit::Eq {
            parameter: EqParameter::Band3Gain,
            value: F32Bits::from_f32(0.1),
        },
        GlobalControlEdit::Drum {
            field: DrumSetting::Tempo,
            value: 121,
        },
        GlobalControlEdit::Drum {
            field: DrumSetting::Volume,
            value: 79,
        },
        GlobalControlEdit::Drum {
            field: DrumSetting::Pattern,
            value: 1,
        },
        GlobalControlEdit::DrumPlay(true),
        GlobalControlEdit::Footswitch {
            table: GlobalFootswitchTableId::new(2).unwrap(),
            event: GlobalFootswitchEvent::new(19).unwrap(),
            function: GlobalFootswitchFunction::new(22).unwrap(),
        },
    ];
    for (i, edit) in edits.iter().enumerate() {
        let before = state(pedal.globals().clone());
        let expected = before.expected_after(edit).unwrap();
        assert!(transact(&mut pedal, &edit.request().unwrap(), 2 + i as u64 * 2).is_empty());
        let actual = decode(transact(
            &mut pedal,
            &read_globals_request().unwrap(),
            3 + i as u64 * 2,
        ));
        assert_eq!(actual, expected);
        assert!(before.verifies_expected(edit, &state(actual)).unwrap());
        assert_eq!(pedal.current(), &current);
        assert_eq!(pedal.stored(23).unwrap(), &stored);
        assert_eq!(pedal.selected_slot(), 23);
    }
}

#[test]
fn invalid_unknown_selector_and_bounds_cannot_modify_simulated_globals() {
    let mut pedal = SimulatedPedal::captured();
    let original = pedal.globals().clone();
    for (i, record) in [
        RawRecord {
            command: 0x1008,
            body: vec![37, 0, 0, 0],
        }, // reset path is deliberately absent
        RawRecord {
            command: 0x1008,
            body: vec![40, 0, 3, 0],
        },
        RawRecord {
            command: 0x1001,
            body: vec![0, 0, 100, 0],
        },
        RawRecord {
            command: 0x100f,
            body: vec![0, 0, 20, 0],
        },
        RawRecord {
            command: 0x1011,
            body: vec![0, 0, 2, 0],
        },
    ]
    .into_iter()
    .enumerate()
    {
        let request = RawTransfer::from_records(WRITE_KIND, &[record]).unwrap();
        let packet = encode_transfer(&request).unwrap().remove(0);
        assert!(
            pedal
                .receive(&packet, Duration::from_millis(1 + i as u64))
                .is_err()
        );
        assert_eq!(pedal.globals(), &original);
        pedal.reset_stream();
    }
}
