//! Synthetic sysfs reproductions of the 2026-10-12 physical first-entry
//! observation. Synthetic tests never enumerate or open a physical MIDI device.
//! The ignored, explicitly opted-in timing probe reads native control metadata
//! and sysfs only; it never opens a MIDI stream or establishes a physical owner.
use super::*;
use std::{fs, os::unix::fs::PermissionsExt, path::PathBuf};

struct Fixture {
    passive: crate::firmware_entry_enumeration::tests::Fixture,
    boot: FirmwareUsbObservation,
}
impl Fixture {
    fn new() -> Self {
        let passive = crate::firmware_entry_enumeration::tests::Fixture::new();
        let mut boot = passive.binding.clone();
        boot.product = 0x002b;
        boot.serial = "V1.0.0".into();
        boot.endpoint = "hw:GP200Boot,0,0".into();
        let result = Self { passive, boot };
        result.boot_sysfs();
        result
    }
    fn boot_sysfs(&self) {
        let node = Path::new(&self.boot.topology);
        for (field, value) in [
            ("idProduct", "002b"),
            ("serial", "V1.0.0"),
            ("manufacturer", "Valeton"),
            ("product", "GP-200"),
            ("bcdDevice", "0100"),
            ("bNumInterfaces", "1"),
            ("devnum", "5"),
        ] {
            fs::write(node.join(field), value).unwrap();
        }
        // This literal is the independently retained physical descriptor blob,
        // not a generated value derived from the acceptance implementation.
        let hex = "1201000200000040ef842b0000010102030109025300010104802009040000020103000507240100014100062402010100062402020200092403010901020100092403020a0101010009058302000200000005250101090905030200020000000525010101";
        let bytes = hex
            .as_bytes()
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
            .collect::<Vec<_>>();
        fs::write(node.join("descriptors"), bytes).unwrap();
        let interface = Path::new(&self.boot.interface);
        let _ = fs::remove_dir_all(interface.join("ep_81"));
        for (field, value) in [("bInterfaceNumber", "00"), ("bNumEndpoints", "02")] {
            fs::write(interface.join(field), value).unwrap();
        }
        for endpoint in ["83", "03"] {
            let path = interface.join(format!("ep_{endpoint}"));
            fs::create_dir_all(&path).unwrap();
            for (field, value) in [
                ("bEndpointAddress", endpoint),
                ("bmAttributes", "02"),
                ("wMaxPacketSize", "0200"),
                ("bInterval", "0"),
            ] {
                fs::write(path.join(field), value).unwrap();
            }
        }
        fs::write(self.passive.root.join("sound/card3/id"), "GP200Boot").unwrap();
    }
    fn normal_sysfs(&self) {
        let node = Path::new(&self.boot.topology);
        for (field, value) in [
            ("idProduct", "002a"),
            ("serial", "20220118"),
            ("bcdDevice", "0101"),
            ("bNumInterfaces", "4"),
            ("devnum", "6"),
        ] {
            fs::write(node.join(field), value).unwrap();
        }
        fs::write(node.join("descriptors"), vec![0; 18]).unwrap();
        fs::write(self.passive.root.join("sound/card3/id"), "GP200").unwrap();
    }
    fn observe(
        &self,
        endpoints: Vec<CompletionEndpointObservation>,
    ) -> Result<CompletionBindingObservation, ServiceError> {
        observe_at(
            &self.passive.binding,
            2,
            &self.passive.root.join("usb"),
            &self.passive.root.join("sound"),
            endpoints,
        )
    }
    fn root(&self, name: &str) -> PathBuf {
        let path = self.passive.root.join(name);
        fs::create_dir(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        path
    }
    fn guarded(&self, anchor: &FirmwareUsbObservation) -> (PathBuf, DeviceSessionGuard) {
        let runtime = self.root("runtime");
        let registry =
            crate::firmware_quarantine::QuarantineRegistry::mock(&self.root("registry")).unwrap();
        let guard = DeviceSessionGuard::fixture_physical(
            &runtime,
            &[alias(anchor).unwrap()],
            anchor,
            &registry,
        )
        .unwrap();
        (runtime, guard)
    }
}
fn endpoint(binding: FirmwareUsbObservation) -> CompletionEndpointObservation {
    let card = binding
        .endpoint
        .trim_start_matches("hw:")
        .split(',')
        .next()
        .unwrap();
    CompletionEndpointObservation {
        selection: DeviceSelection {
            input_id: format!("alsa-raw:in:{card}:0:0"),
            output_id: format!("alsa-raw:out:{card}:0:0"),
            fingerprint_hint: None,
        },
        binding,
    }
}

#[test]
fn observed_gp200_normal_to_boot_serial_transition_is_passive_only() {
    let f = Fixture::new();
    let observed = f.observe(vec![endpoint(f.boot.clone())]).unwrap();
    assert_eq!(observed.binding.serial, "V1.0.0");
    assert_eq!(observed.binding.product, 0x002b);
    assert_eq!(observed.usb.owned.fields["devnum"], "5");
    assert!(
        observed
            .relation
            .contains("unobserved until exact reviewed identity query")
    );
    assert_eq!(observed.usb.owned.descriptors.len(), 101);
}
#[test]
fn observed_boot_transition_rejects_wrong_location_vendor_model_and_serial() {
    let f = Fixture::new();
    for changed in [
        FirmwareUsbObservation {
            topology: f.passive.root.display().to_string(),
            ..f.boot.clone()
        },
        FirmwareUsbObservation {
            vendor: 0x9999,
            ..f.boot.clone()
        },
        FirmwareUsbObservation {
            product: 0x0029,
            ..f.boot.clone()
        },
        FirmwareUsbObservation {
            product: 0x002a,
            ..f.boot.clone()
        },
        FirmwareUsbObservation {
            serial: "V2.0.0".into(),
            ..f.boot.clone()
        },
        FirmwareUsbObservation {
            serial: f.passive.binding.serial.clone(),
            ..f.boot.clone()
        },
    ] {
        assert!(f.observe(vec![endpoint(changed)]).is_err());
    }
}
#[test]
fn boot_profile_rejects_changed_descriptor_product_and_midi_layout() {
    let f = Fixture::new();
    let node = Path::new(&f.boot.topology);
    let descriptors = fs::read(node.join("descriptors")).unwrap();
    let mut changed = descriptors.clone();
    changed[9] ^= 1;
    fs::write(node.join("descriptors"), changed).unwrap();
    assert!(f.observe(vec![endpoint(f.boot.clone())]).is_err());
    fs::write(node.join("descriptors"), descriptors).unwrap();
    fs::write(node.join("product"), "GP-200LT").unwrap();
    assert!(f.observe(vec![endpoint(f.boot.clone())]).is_err());
    fs::write(node.join("product"), "GP-200").unwrap();
    fs::write(
        Path::new(&f.boot.interface).join("ep_83/bmAttributes"),
        "03",
    )
    .unwrap();
    assert!(f.observe(vec![endpoint(f.boot.clone())]).is_err());
}
#[test]
fn boot_alias_sound_mapping_and_location_inventory_must_remain_unique() {
    let f = Fixture::new();
    assert!(
        f.observe(vec![endpoint(f.boot.clone()), endpoint(f.boot.clone())])
            .is_err()
    );
    fs::create_dir(f.passive.root.join("sound/card4")).unwrap();
    fs::write(f.passive.root.join("sound/card4/id"), "GP200Boot").unwrap();
    std::os::unix::fs::symlink(&f.boot.interface, f.passive.root.join("sound/card4/device"))
        .unwrap();
    assert!(f.observe(vec![endpoint(f.boot.clone())]).is_err());
    fs::remove_dir_all(f.passive.root.join("sound/card4")).unwrap();
    std::os::unix::fs::symlink(
        &f.boot.topology,
        f.passive.root.join("usb/duplicate-location"),
    )
    .unwrap();
    assert!(f.observe(vec![endpoint(f.boot.clone())]).is_err());
}
#[test]
fn boot_witness_retains_dynamic_alias_and_rereads_changed_descriptors() {
    let f = Fixture::new();
    let (runtime, mut guard) = f.guarded(&f.passive.binding);
    let witness = accept_fixture(
        &f.passive.binding,
        2,
        &mut guard,
        &f.passive.root.join("usb"),
        &f.passive.root.join("sound"),
        vec![endpoint(f.boot.clone())],
    )
    .unwrap();
    witness.revalidate(&f.passive.binding, &guard).unwrap();
    assert!(DeviceSessionGuard::fixture(&runtime, &[alias(&f.boot).unwrap()]).is_err());
    fs::write(Path::new(&f.boot.topology).join("devnum"), "9").unwrap();
    assert!(witness.revalidate(&f.passive.binding, &guard).is_err());
    drop(guard);
    assert!(DeviceSessionGuard::fixture(&runtime, &[alias(&f.boot).unwrap()]).is_ok());
}
#[test]
fn recovery_boot_anchor_requires_original_normal_serial_on_return() {
    let f = Fixture::new();
    let (_, mut guard) = f.guarded(&f.boot);
    // A held boot endpoint cannot be accepted as application return.
    assert!(
        accept_association_fixture(
            &f.boot,
            &f.passive.binding,
            3,
            &mut guard,
            &f.passive.root.join("usb"),
            &f.passive.root.join("sound"),
            vec![endpoint(f.boot.clone())],
            true
        )
        .is_err()
    );
    f.normal_sysfs();
    let witness = accept_association_fixture(
        &f.boot,
        &f.passive.binding,
        4,
        &mut guard,
        &f.passive.root.join("usb"),
        &f.passive.root.join("sound"),
        vec![endpoint(f.passive.binding.clone())],
        true,
    )
    .unwrap();
    assert!(guard.owns_normal_binding(&f.boot));
    assert!(!guard.owns_normal_binding(&f.passive.binding));
    assert_eq!(witness.observation().binding.serial, "20220118");
    witness.revalidate(&f.boot, &guard).unwrap();
    let mut wrong = f.passive.binding.clone();
    wrong.serial = "replacement".into();
    fs::write(Path::new(&wrong.topology).join("serial"), &wrong.serial).unwrap();
    assert!(
        accept_association_fixture(
            &f.boot,
            &f.passive.binding,
            5,
            &mut guard,
            &f.passive.root.join("usb"),
            &f.passive.root.join("sound"),
            vec![endpoint(wrong)],
            true
        )
        .is_err()
    );
    assert!(witness.revalidate(&f.boot, &guard).is_err());
}

/// A different USB device shares the host inventory but has no relation to the
/// retained pedal. Its fields and interface are independently readable fixtures.
fn unrelated_usb(f: &Fixture) -> PathBuf {
    let node = f.passive.root.join("usb/2-7");
    let interface = node.join("2-7:1.0");
    fs::create_dir_all(&interface).unwrap();
    for (field, value) in [
        ("idVendor", "1234"),
        ("idProduct", "5678"),
        ("busnum", "2"),
        ("devnum", "7"),
        ("bcdDevice", "0100"),
        ("bDeviceClass", "00"),
        ("bDeviceSubClass", "00"),
        ("bDeviceProtocol", "00"),
        ("bNumConfigurations", "1"),
        ("bNumInterfaces", "1"),
        ("bConfigurationValue", "1"),
        ("speed", "12"),
    ] {
        fs::write(node.join(field), value).unwrap();
    }
    fs::write(node.join("descriptors"), vec![0; 18]).unwrap();
    for (field, value) in [
        ("bInterfaceNumber", "00"),
        ("bAlternateSetting", "0"),
        ("bNumEndpoints", "0"),
        ("bInterfaceClass", "03"),
        ("bInterfaceSubClass", "00"),
        ("bInterfaceProtocol", "00"),
    ] {
        fs::write(interface.join(field), value).unwrap();
    }
    node
}

#[test]
fn retained_boot_witness_allows_unrelated_usb_add_remove_fields_and_interfaces() {
    let f = Fixture::new();
    let (_, mut guard) = f.guarded(&f.passive.binding);
    let witness = accept_fixture(
        &f.passive.binding,
        2,
        &mut guard,
        &f.passive.root.join("usb"),
        &f.passive.root.join("sound"),
        vec![endpoint(f.boot.clone())],
    )
    .unwrap();
    assert_eq!(witness.observation().usb.devices.len(), 1);
    let unrelated = unrelated_usb(&f);
    let added = f.observe(vec![endpoint(f.boot.clone())]).unwrap();
    assert_eq!(added.usb.devices.len(), 2);
    assert_ne!(witness.observation().usb, added.usb);
    witness.revalidate(&f.passive.binding, &guard).unwrap();

    fs::write(unrelated.join("devnum"), "8").unwrap();
    fs::write(unrelated.join("descriptors"), vec![1; 18]).unwrap();
    fs::write(unrelated.join("2-7:1.0/bAlternateSetting"), "1").unwrap();
    witness.revalidate(&f.passive.binding, &guard).unwrap();

    // A partial disappearing peripheral produces descriptive inventory errors.
    // Exact owned receipt reads and physical/ALSA uniqueness still succeed.
    fs::remove_file(unrelated.join("descriptors")).unwrap();
    let incomplete = f.observe(vec![endpoint(f.boot.clone())]).unwrap();
    assert_eq!(incomplete.usb.errors.len(), 1);
    witness.revalidate(&f.passive.binding, &guard).unwrap();
    fs::remove_dir_all(unrelated).unwrap();
    witness.revalidate(&f.passive.binding, &guard).unwrap();
}

fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let destination = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &destination);
        } else {
            fs::copy(entry.path(), destination).unwrap();
        }
    }
}

#[test]
fn retained_boot_witness_rejects_target_enumeration_and_interface_inode_changes() {
    for (change, expected) in [
        ("devnum", "owned USB fields [devnum]"),
        ("authorized", "owned USB fields [authorized]"),
        ("node inode", "owned USB inode"),
        ("interface inode", "interface inode"),
        ("endpoint interval", "owned USB interfaces/endpoints"),
    ] {
        let f = Fixture::new();
        let (_, mut guard) = f.guarded(&f.passive.binding);
        let witness = accept_fixture(
            &f.passive.binding,
            2,
            &mut guard,
            &f.passive.root.join("usb"),
            &f.passive.root.join("sound"),
            vec![endpoint(f.boot.clone())],
        )
        .unwrap();
        match change {
            "devnum" => {
                fs::write(Path::new(&f.boot.topology).join("devnum"), "9").unwrap();
            }
            "authorized" => {
                fs::write(Path::new(&f.boot.topology).join("authorized"), "0").unwrap();
            }
            "node inode" | "interface inode" => {
                let target = Path::new(if change == "node inode" {
                    &f.boot.topology
                } else {
                    &f.boot.interface
                });
                let retained = f.passive.root.join("retained-original-inode");
                fs::rename(target, &retained).unwrap();
                // Retain the old inode while creating a new node at the same path.
                copy_tree(&retained, target);
            }
            "endpoint interval" => {
                fs::write(Path::new(&f.boot.interface).join("ep_83/bInterval"), "1").unwrap();
            }
            _ => unreachable!(),
        }
        let error = witness.revalidate(&f.passive.binding, &guard).unwrap_err();
        assert!(error.message.contains(expected), "{change}: {error:?}");
        assert!(!error.message.contains(&f.boot.serial));
        assert!(!error.message.contains(&f.boot.topology));
    }
}

#[test]
fn retained_boot_witness_rejects_descriptor_and_sound_mapping_changes() {
    for change in ["descriptors", "sound mapping"] {
        let f = Fixture::new();
        let (_, mut guard) = f.guarded(&f.passive.binding);
        let witness = accept_fixture(
            &f.passive.binding,
            2,
            &mut guard,
            &f.passive.root.join("usb"),
            &f.passive.root.join("sound"),
            vec![endpoint(f.boot.clone())],
        )
        .unwrap();
        if change == "descriptors" {
            let path = Path::new(&f.boot.topology).join("descriptors");
            let mut bytes = fs::read(&path).unwrap();
            bytes[9] ^= 1;
            fs::write(path, bytes).unwrap();
        } else {
            let path = f.passive.root.join("sound/card3/device");
            fs::remove_file(&path).unwrap();
            std::os::unix::fs::symlink(&f.passive.root, path).unwrap();
        }
        let error = witness.revalidate(&f.passive.binding, &guard).unwrap_err();
        let expected = if change == "descriptors" {
            "Fresh boot descriptors differ"
        } else {
            "Fresh sound endpoint maps to a changed interface"
        };
        assert!(error.message.contains(expected), "{change}: {error:?}");
    }
}

#[test]
fn retained_boot_witness_rejects_raw_selection_changes() {
    for change in ["input", "output", "fingerprint", "endpoint"] {
        let f = Fixture::new();
        let (_, mut guard) = f.guarded(&f.passive.binding);
        let mut witness = accept_fixture(
            &f.passive.binding,
            2,
            &mut guard,
            &f.passive.root.join("usb"),
            &f.passive.root.join("sound"),
            vec![endpoint(f.boot.clone())],
        )
        .unwrap();
        let current = &mut witness.fixture_source.as_mut().unwrap().2[0];
        match change {
            "input" => current.selection.input_id = "alsa-raw:in:GP200Boot:0:1".into(),
            "output" => current.selection.output_id = "alsa-raw:out:GP200Boot:0:1".into(),
            "fingerprint" => {
                current.selection.fingerprint_hint = Some(DeviceFingerprint {
                    vendor: Some(0x84ef),
                    product: Some(0x002b),
                    serial: Some("V1.0.0".into()),
                    hint: "different".into(),
                });
            }
            "endpoint" => current.binding.endpoint = "hw:GP200Boot,0,1".into(),
            _ => unreachable!(),
        }
        let error = witness.revalidate(&f.passive.binding, &guard).unwrap_err();
        let expected = match change {
            "input" => "raw input selection",
            "output" => "raw output selection",
            "fingerprint" => "raw selection fingerprint",
            "endpoint" => "ALSA endpoint",
            _ => unreachable!(),
        };
        assert!(error.message.contains(expected), "{change}: {error:?}");
    }
}

#[test]
fn retained_boot_witness_still_rejects_owned_alias_ambiguity_with_unrelated_errors() {
    let f = Fixture::new();
    let unrelated = unrelated_usb(&f);
    fs::remove_file(unrelated.join("descriptors")).unwrap();
    let (_, mut guard) = f.guarded(&f.passive.binding);
    let witness = accept_fixture(
        &f.passive.binding,
        2,
        &mut guard,
        &f.passive.root.join("usb"),
        &f.passive.root.join("sound"),
        vec![endpoint(f.boot.clone())],
    )
    .unwrap();
    let duplicate = f.passive.root.join("usb/duplicate-owned-location");
    std::os::unix::fs::symlink(&f.boot.topology, &duplicate).unwrap();
    let error = witness.revalidate(&f.passive.binding, &guard).unwrap_err();
    assert!(error.message.contains("no unique passive candidate"));
    fs::remove_file(duplicate).unwrap();
    fs::create_dir(f.passive.root.join("sound/card4")).unwrap();
    fs::write(f.passive.root.join("sound/card4/id"), "GP200Boot").unwrap();
    std::os::unix::fs::symlink(&f.boot.interface, f.passive.root.join("sound/card4/device"))
        .unwrap();
    let error = witness.revalidate(&f.passive.binding, &guard).unwrap_err();
    assert!(
        error
            .message
            .contains("sound card mapping missing or ambiguous")
    );
}

#[test]
fn consuming_refresh_reads_both_owned_snapshots_without_unrelated_usb_blobs() {
    let f = Fixture::new();
    let unrelated = unrelated_usb(&f);
    let (_, mut guard) = f.guarded(&f.passive.binding);
    let witness = accept_fixture(
        &f.passive.binding,
        2,
        &mut guard,
        &f.passive.root.join("usb"),
        &f.passive.root.join("sound"),
        vec![endpoint(f.boot.clone())],
    )
    .unwrap();
    assert_eq!(witness.observation().usb.devices.len(), 2);

    let log = firmware_entry_enumeration::tests::ReadLog::start();
    witness.revalidate(&f.passive.binding, &guard).unwrap();
    let consuming_reads = log.finish();
    assert!(
        consuming_reads
            .iter()
            .all(|path| !path.starts_with(&unrelated))
    );
    for relative in [
        "idVendor",
        "idProduct",
        "serial",
        "devnum",
        "descriptors",
        "1-2:1.0/bAlternateSetting",
        "1-2:1.0/ep_83/bInterval",
        "1-2:1.0/ep_03/bEndpointAddress",
    ] {
        let path = Path::new(&f.boot.topology).join(relative);
        assert_eq!(
            consuming_reads.iter().filter(|p| **p == path).count(),
            2,
            "{relative}"
        );
    }
    let log = firmware_entry_enumeration::tests::ReadLog::start();
    let full = f.observe(vec![endpoint(f.boot.clone())]).unwrap();
    let review_reads = log.finish();
    assert_eq!(full.usb.devices.len(), 2);
    assert!(review_reads.contains(&unrelated.join("descriptors")));
    assert!(review_reads.contains(&unrelated.join("2-7:1.0/bInterfaceClass")));
    assert_eq!(
        review_reads
            .iter()
            .filter(|p| **p == Path::new(&f.boot.topology).join("descriptors"))
            .count(),
        2
    );
}

#[test]
fn consuming_refresh_rejects_changes_between_original_owned_snapshots() {
    for change in [
        "descriptor",
        "devnum",
        "endpoint interval",
        "node inode",
        "interface inode",
        "serial",
        "sound mapping",
        "sound alias",
        "topology alias",
    ] {
        let f = Fixture::new();
        let error = refresh_association_with(
            &f.passive.binding,
            2,
            &f.passive.root.join("usb"),
            &f.passive.root.join("sound"),
            vec![endpoint(f.boot.clone())],
            false,
            || match change {
                "descriptor" => {
                    let path = Path::new(&f.boot.topology).join("descriptors");
                    let mut bytes = fs::read(&path).unwrap();
                    bytes[9] ^= 1;
                    fs::write(path, bytes).unwrap();
                }
                "devnum" => fs::write(Path::new(&f.boot.topology).join("devnum"), "9").unwrap(),
                "endpoint interval" => {
                    fs::write(Path::new(&f.boot.interface).join("ep_83/bInterval"), "1").unwrap()
                }
                "node inode" | "interface inode" => {
                    let target = Path::new(if change == "node inode" {
                        &f.boot.topology
                    } else {
                        &f.boot.interface
                    });
                    let retained = f.passive.root.join("retained-original-inode");
                    fs::rename(target, &retained).unwrap();
                    copy_tree(&retained, target);
                }
                "serial" => {
                    fs::write(Path::new(&f.boot.topology).join("serial"), "V2.0.0").unwrap()
                }
                "sound mapping" => {
                    let path = f.passive.root.join("sound/card3/device");
                    fs::remove_file(&path).unwrap();
                    std::os::unix::fs::symlink(&f.passive.root, path).unwrap();
                }
                "sound alias" => {
                    fs::create_dir(f.passive.root.join("sound/card4")).unwrap();
                    fs::write(f.passive.root.join("sound/card4/id"), "GP200Boot").unwrap();
                    std::os::unix::fs::symlink(
                        &f.boot.interface,
                        f.passive.root.join("sound/card4/device"),
                    )
                    .unwrap();
                }
                "topology alias" => std::os::unix::fs::symlink(
                    &f.boot.topology,
                    f.passive.root.join("usb/duplicate-owned-location"),
                )
                .unwrap(),
                _ => unreachable!(),
            },
        )
        .unwrap_err();
        let expected = match change {
            "serial" => "Fresh owned USB identity changed",
            "sound mapping" => "Fresh sound endpoint maps to a changed interface",
            "sound alias" => "sound card mapping missing or ambiguous",
            "topology alias" => "no unique passive candidate",
            _ => "changed during passive refresh",
        };
        assert!(error.message.contains(expected), "{change}: {error:?}");
    }
}

/// Read-only native measurements, requiring both an ignored-test invocation and
/// explicit opt-in before the first ALSA control discovery or live sysfs read.
#[test]
#[ignore = "native ALSA control metadata/sysfs only; requires GP200_FIRMWARE_PASSIVE_BINDING_PROBE=1"]
fn passive_native_boot_binding_full_vs_owned_timing_probe() {
    assert_eq!(
        std::env::var("GP200_FIRMWARE_PASSIVE_BINDING_PROBE").as_deref(),
        Ok("1"),
        "Explicit passive native binding timing opt-in is required"
    );

    use std::time::Instant;

    const PAIRS: usize = 20;
    // This is a descriptive comparison value, not a receiver or reader epoch.
    const DESCRIPTIVE_GENERATION: u64 = 1;
    let usb = Path::new("/sys/bus/usb/devices");
    let sound = Path::new("/sys/class/sound");
    let mut anchor = None;
    let mut pairs = Vec::with_capacity(PAIRS);
    for pair in 0..PAIRS {
        let enumeration_started = Instant::now();
        // completion_endpoints uses ALSA Ctl discovery and passive sysfs only.
        // No session, MIDI input/output stream, USB handle, or query is opened.
        let full_endpoints = crate::raw_backend::completion_endpoints()
            .expect("Fresh passive ALSA endpoint metadata for full observation");
        let full_endpoint_enumeration_ns = enumeration_started.elapsed().as_nanos();
        let full_candidates = full_endpoints
            .iter()
            .filter(|endpoint| {
                endpoint.binding.vendor == 0x84ef && endpoint.binding.product == 0x002b
            })
            .collect::<Vec<_>>();
        assert_eq!(
            full_candidates.len(),
            1,
            "Exactly one current 84ef/002b GP-200 duplex metadata binding is required"
        );
        let current_binding = full_candidates[0].binding.clone();
        let association = anchor.get_or_insert(current_binding);
        assert_eq!(
            &full_candidates[0].binding, association,
            "The initial descriptive boot binding must remain unchanged"
        );
        let capture_started = Instant::now();
        let full = observe_association_at(
            association,
            DESCRIPTIVE_GENERATION,
            usb,
            sound,
            full_endpoints,
            false,
        )
        .expect("Fresh full passive boot observation");
        let full_observation_ns = capture_started.elapsed().as_nanos();
        assert_eq!(full.usb.owned.fields["manufacturer"], "Valeton");
        assert_eq!(full.usb.owned.fields["product"], "GP-200");
        assert_eq!(full.binding.serial, "V1.0.0");

        let enumeration_started = Instant::now();
        let owned_endpoints = crate::raw_backend::completion_endpoints()
            .expect("Fresh passive ALSA endpoint metadata for owned refresh");
        let owned_endpoint_enumeration_ns = enumeration_started.elapsed().as_nanos();
        let owned_candidates = owned_endpoints
            .iter()
            .filter(|endpoint| {
                endpoint.binding.vendor == 0x84ef && endpoint.binding.product == 0x002b
            })
            .collect::<Vec<_>>();
        assert_eq!(
            owned_candidates.len(),
            1,
            "Exactly one fresh boot binding is required for each owned refresh"
        );
        assert_eq!(
            &owned_candidates[0].binding, association,
            "Fresh owned metadata must retain the same descriptive boot binding"
        );
        let capture_started = Instant::now();
        let owned = refresh_association_with(
            association,
            DESCRIPTIVE_GENERATION,
            usb,
            sound,
            owned_endpoints,
            false,
            || {},
        )
        .expect("Fresh double-snapshot owned boot refresh");
        let owned_refresh_ns = capture_started.elapsed().as_nanos();
        require_same_target(&full, &owned, "Passive timing pair changed").unwrap();
        assert_eq!(full.usb.as_ref(), owned.usb.as_ref());
        assert_eq!(full.binding, owned.binding);
        assert_eq!(full.selection, owned.selection);
        assert_eq!(full.observed_generation, owned.observed_generation);
        pairs.push(serde_json::json!({
            "pair": pair,
            "full_endpoint_enumeration_ns": full_endpoint_enumeration_ns,
            "full_observation_ns": full_observation_ns,
            "owned_endpoint_enumeration_ns": owned_endpoint_enumeration_ns,
            "owned_refresh_ns": owned_refresh_ns,
            "equal_owned_receipt_binding_selection_generation": true,
            "full_inventory_device_count": full.usb.devices.len(),
            "full_inventory_error_count": full.usb.errors.len(),
        }));
    }
    println!(
        "{}",
        serde_json::json!({
            "schema": 1,
            "probe": "passive_native_boot_binding_full_vs_owned_timing_probe",
            "pair_count": PAIRS,
            "descriptive_generation": DESCRIPTIVE_GENERATION,
            "normal_return_only": false,
            "binding": anchor.unwrap(),
            "order": "fresh full endpoint metadata, full observation, fresh owned endpoint metadata, owned refresh",
            "scope": "ALSA control discovery and passive sysfs; no MIDI streams, USB handle, protocol query, physical guard, or firmware transfer",
            "qualification": "Host metadata timings only; no receiver-state, native writer-speedup, firmware success, timeout, or speed-threshold claim",
            "pairs": pairs,
        })
    );
}
