//! Exact bidirectional factory model pairs supported by native captures.
//!
//! These are descriptive profiles, not transferable write authorization. The
//! actor still checks the current context, complete raw module, assignments,
//! catalog defaults, epoch and revision, then verifies the entire readback.
//! In particular, sharing a baseline does not establish a cross-pair.

/// One captured model pair and its fixed name in the root-operated probe.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CapturedModelProfile {
    pub name: &'static str,
    pub module: u8,
    pub baseline: u32,
    pub target: u32,
}

const PROFILES: [CapturedModelProfile; 17] = [
    CapturedModelProfile {
        name: "nr",
        module: 4,
        baseline: 0x1b,
        target: 0x1d,
    },
    CapturedModelProfile {
        name: "cab",
        module: 5,
        baseline: 0x0a000010,
        target: 0x0a000000,
    },
    CapturedModelProfile {
        name: "eq",
        module: 6,
        baseline: 0x0100003c,
        target: 0x01000035,
    },
    CapturedModelProfile {
        name: "mod",
        module: 7,
        baseline: 0x04000015,
        target: 0x04000001,
    },
    CapturedModelProfile {
        name: "dly",
        module: 8,
        baseline: 0x0b00000b,
        target: 0x0b00001d,
    },
    CapturedModelProfile {
        name: "rvb",
        module: 9,
        baseline: 0x0c000002,
        target: 0x0c000001,
    },
    // Original captures and hashes: model-breadth-native-acceptance.json and
    // tests/fixtures/model-breadth/manifest.json. The Gate3 negative is absent.
    CapturedModelProfile {
        name: "nr-autoswell",
        module: 4,
        baseline: 0x1b,
        target: 0x0400002d,
    },
    CapturedModelProfile {
        name: "cab-twdcp",
        module: 5,
        baseline: 0x0a000010,
        target: 0x0a000001,
    },
    CapturedModelProfile {
        name: "cab-j120",
        module: 5,
        baseline: 0x0a000010,
        target: 0x0a000011,
    },
    CapturedModelProfile {
        name: "eq-guitareq2",
        module: 6,
        baseline: 0x0100003c,
        target: 0x01000036,
    },
    CapturedModelProfile {
        name: "eq-hyper",
        module: 6,
        baseline: 0x0100003c,
        target: 0x01000043,
    },
    CapturedModelProfile {
        name: "mod-jet",
        module: 7,
        baseline: 0x04000015,
        target: 0x04000011,
    },
    CapturedModelProfile {
        name: "mod-vibrato",
        module: 7,
        baseline: 0x04000015,
        target: 0x04000017,
    },
    CapturedModelProfile {
        name: "dly-digital-s",
        module: 8,
        baseline: 0x0b00000b,
        target: 0x0b00001f,
    },
    CapturedModelProfile {
        name: "dly-analog",
        module: 8,
        baseline: 0x0b00000b,
        target: 0x0b000001,
    },
    CapturedModelProfile {
        name: "rvb-plate",
        module: 9,
        baseline: 0x0c000002,
        target: 0x0c000003,
    },
    CapturedModelProfile {
        name: "rvb-spring",
        module: 9,
        baseline: 0x0c000002,
        target: 0x0c000004,
    },
];

/// Captured pairs only; no transitive closure or category inference is applied.
pub fn captured_model_profiles() -> &'static [CapturedModelProfile] {
    &PROFILES
}

pub fn captured_model_profile(name: &str) -> Option<&'static CapturedModelProfile> {
    PROFILES.iter().find(|profile| profile.name == name)
}

pub(crate) fn captured_transition(module: u8, previous: u32, target: u32) -> bool {
    PROFILES.iter().any(|profile| {
        profile.module == module
            && ((profile.baseline == previous && profile.target == target)
                || (profile.target == previous && profile.baseline == target))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_pairs_work_in_both_directions_without_promoting_cross_pairs() {
        for profile in PROFILES {
            assert!(captured_transition(
                profile.module,
                profile.baseline,
                profile.target
            ));
            assert!(captured_transition(
                profile.module,
                profile.target,
                profile.baseline
            ));
            assert!(!captured_transition(
                profile.module,
                profile.target,
                profile.target
            ));
            assert!(!captured_transition(3, profile.baseline, profile.target));
            assert_eq!(captured_model_profile(profile.name), Some(&profile));
            for other in PROFILES {
                if other.module == profile.module && other.target != profile.target {
                    assert!(!captured_transition(
                        profile.module,
                        profile.target,
                        other.target
                    ));
                }
            }
        }
        assert!(!captured_transition(4, 0x1b, 0x21)); // Ignored Gate3.
        assert!(!captured_transition(7, 0x04000015, 0x04000000)); // Ignored A Chorus.
        assert!(!captured_transition(5, 0x0a000010, 0x0a100000)); // User IR.
        assert!(captured_model_profile("nr-gate3").is_none());
    }
}
