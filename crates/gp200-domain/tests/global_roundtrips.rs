//! Regression against complete root-operated reversible device captures.
use gp200_domain::*;
use gp200_protocol::*;
use std::path::Path;

fn captured(profile: &str, stage: &str) -> DeviceGlobalState {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/globals")
        .join(format!("{profile}-{stage}.payload.bin"));
    DeviceGlobalState::from_transfer(
        Revision(1),
        SessionEpoch(1),
        RawTransfer::from_payload(WRITE_KIND, std::fs::read(path).unwrap()).unwrap(),
        vec![EvidenceRef::new(profile, EvidenceLevel::LiveVerified)],
    )
    .unwrap()
}

#[test]
fn connected_single_field_roundtrips_match_exact_expected_control_state() {
    for profile in [
        "display-mode",
        "input-level",
        "eq-band-gain",
        "stopped-drum-tempo",
        "stopped-drum-volume",
        "stopped-drum-pattern",
        "inactive-footswitch",
    ] {
        let before = captured(profile, "before");
        let after = captured(profile, "after");
        let restored = captured(profile, "restored");
        let edit = match profile {
            "display-mode" => GlobalControlEdit::General {
                field: GeneralSetting::DisplayMode,
                value: 1,
            },
            "input-level" => GlobalControlEdit::General {
                field: GeneralSetting::InputLevel,
                value: 1,
            },
            "eq-band-gain" => GlobalControlEdit::Eq {
                parameter: EqParameter::Band3Gain,
                value: F32Bits::from_f32(0.1),
            },
            "stopped-drum-tempo" => GlobalControlEdit::Drum {
                field: DrumSetting::Tempo,
                value: 121,
            },
            "stopped-drum-volume" => GlobalControlEdit::Drum {
                field: DrumSetting::Volume,
                value: 81,
            },
            "stopped-drum-pattern" => GlobalControlEdit::Drum {
                field: DrumSetting::Pattern,
                value: 1,
            },
            "inactive-footswitch" => GlobalControlEdit::Footswitch {
                table: GlobalFootswitchTableId::new(2).unwrap(),
                event: GlobalFootswitchEvent::new(19).unwrap(),
                function: GlobalFootswitchFunction::new(1).unwrap(),
            },
            _ => unreachable!(),
        };
        assert!(
            before.verifies_expected(&edit, &after).unwrap(),
            "{profile}"
        );
        assert!(
            !before.records().control_state_equal(after.records()),
            "{profile}"
        );
        assert!(
            before.records().control_state_equal(restored.records()),
            "{profile}"
        );
        // Whole bundles demonstrably vary even after exact selector restoration.
        assert_ne!(
            before.records().raw(),
            restored.records().raw(),
            "{profile}"
        );
    }
}

#[test]
fn quiet_play_capture_proves_mute_play_stop_and_original_volume_restoration() {
    let before = captured("quiet-drum-play", "before");
    let muted = captured("quiet-drum-play", "after");
    let playing = captured("quiet-drum-play", "playing");
    let stopped = captured("quiet-drum-play", "stopped");
    let restored = captured("quiet-drum-play", "restored");
    assert_eq!(
        before
            .records()
            .drum
            .as_ref()
            .unwrap()
            .value(DrumSetting::Volume),
        80
    );
    assert!(
        before
            .verifies_expected(
                &GlobalControlEdit::Drum {
                    field: DrumSetting::Volume,
                    value: 0
                },
                &muted
            )
            .unwrap()
    );
    assert_eq!(
        muted.records().runtime.as_ref().unwrap().drum_playing(),
        Some(false)
    );
    assert!(
        muted
            .verifies_expected(&GlobalControlEdit::DrumPlay(true), &playing)
            .unwrap()
    );
    assert!(
        playing
            .verifies_expected(&GlobalControlEdit::DrumPlay(false), &stopped)
            .unwrap()
    );
    assert!(
        stopped
            .verifies_expected(
                &GlobalControlEdit::Drum {
                    field: DrumSetting::Volume,
                    value: 80
                },
                &restored
            )
            .unwrap()
    );
    assert!(before.records().control_state_equal(restored.records()));
}
