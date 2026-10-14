//! Fresh passive observations and dynamic ALSA aliases under retained ownership.
use super::firmware_linux_completion_admission::bad;
use crate::{
    device_session_guard::{DeviceSessionGuard, EndpointIdentity},
    firmware_entry_enumeration::{self, EnumerationReceipt, OwnedEnumerationReceipt},
};
use gp200_domain::*;
use serde::Serialize;
use std::path::Path;
/// The actual physical guard has two distinct sealed startup paths. Recovery
/// never manufactures a captured-normal handoff or a normal retirement epoch.
pub(crate) struct CompletionOwnedAnchor {
    pub(crate) binding: FirmwareUsbObservation,
    pub(crate) guard: Option<DeviceSessionGuard>,
}
impl CompletionOwnedAnchor {
    pub(crate) fn from_captured_normal(
        mut handoff: crate::firmware_backend::FirmwareNormalHandoff,
    ) -> Result<Self, ServiceError> {
        crate::firmware_backend::NormalEntryBinding::from_handoff(&handoff)?;
        let guard = handoff
            .guard
            .take()
            .ok_or_else(|| bad("Captured normal physical guard absent"))?;
        if !guard.owns_normal_binding(&handoff.binding) {
            return Err(bad("Captured normal physical ownership differs"));
        }
        Ok(Self {
            binding: handoff.binding,
            guard: Some(guard),
        })
    }
    pub(crate) fn fresh_recovery(
        lineage: &crate::firmware_linux_recovery_admission::CheckedRecoveryLineage,
        registry: &crate::firmware_quarantine::QuarantineRegistry,
    ) -> Result<Self, ServiceError> {
        lineage.revalidate()?;
        let observation = observe_at(
            lineage.original_binding(),
            1,
            Path::new("/sys/bus/usb/devices"),
            Path::new("/sys/class/sound"),
            crate::raw_backend::completion_endpoints()?,
        )?;
        let guard = DeviceSessionGuard::recovery(
            &[alias(&observation.binding)?],
            &observation.binding,
            registry,
            lineage,
        )?;
        let after = observe_at(
            lineage.original_binding(),
            1,
            Path::new("/sys/bus/usb/devices"),
            Path::new("/sys/class/sound"),
            crate::raw_backend::completion_endpoints()?,
        )?;
        require_same_target(
            &observation,
            &after,
            "Fresh recovery candidate changed while locking actual aliases",
        )?;
        lineage.revalidate()?;
        Ok(Self {
            binding: observation.binding,
            guard: Some(guard),
        })
    }
}
#[derive(Debug, Clone, Serialize)]
pub(crate) struct CompletionEndpointObservation {
    pub(crate) binding: FirmwareUsbObservation,
    pub(crate) selection: DeviceSelection,
}
/// A current description. It is not deserializable or an opener.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct CompletionBindingObservation<R = EnumerationReceipt> {
    pub(crate) binding: FirmwareUsbObservation,
    pub(crate) selection: DeviceSelection,
    pub(crate) usb: R,
    pub(crate) observed_generation: u64,
    pub(crate) relation: String,
}
/// Private consuming witness minted only under the actual retained guard.
pub(crate) struct CompletionBindingWitness {
    observation: CompletionBindingObservation,
    normal_sha256: String,
    association: FirmwareUsbObservation,
    normal_return_only: bool,
    #[cfg(test)]
    fixture_source: Option<(
        std::path::PathBuf,
        std::path::PathBuf,
        Vec<CompletionEndpointObservation>,
    )>,
}
impl CompletionBindingWitness {
    pub(crate) fn observation(&self) -> &CompletionBindingObservation {
        &self.observation
    }
    pub(crate) fn endpoint(&self) -> &str {
        &self.observation.binding.endpoint
    }
    pub(crate) fn revalidate(
        &self,
        normal: &FirmwareUsbObservation,
        guard: &DeviceSessionGuard,
    ) -> Result<(), ServiceError> {
        #[cfg(test)]
        let profile = self.fixture_source.as_ref().and_then(|(usb, _, _)| {
            crate::firmware_linux_completion_journal::test_profile::for_fixture_usb(usb)
        });
        #[cfg(test)]
        let _revalidation_timer =
            crate::firmware_linux_completion_journal::test_profile::Timer::new(
                profile.clone(),
                "binding_revalidate_total",
            );
        if self.normal_sha256 != normal.digest() || !guard.owns_normal_binding(normal) {
            return Err(bad("Retained normal physical/alias guard differs"));
        }
        #[cfg(test)]
        if let Some((usb, sound, endpoints)) = &self.fixture_source {
            let enumeration_timer =
                crate::firmware_linux_completion_journal::test_profile::Timer::new(
                    profile,
                    "binding_fixture_enumeration",
                );
            let current = refresh_association_with(
                &self.association,
                self.observation.observed_generation,
                usb,
                sound,
                endpoints.clone(),
                self.normal_return_only,
                || {},
            )?;
            enumeration_timer.finish();
            return require_same_target(
                &self.observation,
                &current,
                "Fixture completion binding changed",
            );
        }
        let current = refresh_association_with(
            &self.association,
            self.observation.observed_generation,
            Path::new("/sys/bus/usb/devices"),
            Path::new("/sys/class/sound"),
            crate::raw_backend::completion_endpoints()?,
            self.normal_return_only,
            || {},
        )?;
        require_same_target(
            &self.observation,
            &current,
            "Completion USB descriptors/interface/endpoint or dynamic card binding changed",
        )
    }
}
/// Bind only the retained pedal and its actual ALSA mapping. The full host USB
/// inventory is useful review context, but an unrelated peripheral is not part
/// of the authority for an already-open pedal writer.
fn require_same_target<B: AsRef<OwnedEnumerationReceipt>, A: AsRef<OwnedEnumerationReceipt>>(
    before: &CompletionBindingObservation<B>,
    after: &CompletionBindingObservation<A>,
    context: &str,
) -> Result<(), ServiceError> {
    let before_usb = before.usb.as_ref();
    let after_usb = after.usb.as_ref();
    let mut changed = Vec::new();
    macro_rules! compare {
        ($field:expr, $current:expr, $label:literal) => {
            if $field != $current {
                changed.push($label.to_owned());
            }
        };
    }
    compare!(
        before.binding.topology,
        after.binding.topology,
        "binding topology"
    );
    compare!(
        before.binding.interface,
        after.binding.interface,
        "binding interface"
    );
    compare!(
        before.binding.endpoint,
        after.binding.endpoint,
        "ALSA endpoint"
    );
    compare!(before.binding.vendor, after.binding.vendor, "USB vendor");
    compare!(before.binding.product, after.binding.product, "USB product");
    compare!(before.binding.serial, after.binding.serial, "USB serial");
    compare!(
        before.selection.input_id,
        after.selection.input_id,
        "raw input selection"
    );
    compare!(
        before.selection.output_id,
        after.selection.output_id,
        "raw output selection"
    );
    compare!(
        before.selection.fingerprint_hint,
        after.selection.fingerprint_hint,
        "raw selection fingerprint"
    );
    compare!(
        before.observed_generation,
        after.observed_generation,
        "observed generation"
    );
    compare!(
        before_usb.owned.location,
        after_usb.owned.location,
        "owned USB location"
    );
    compare!(
        before_usb.owned.inode,
        after_usb.owned.inode,
        "owned USB inode"
    );
    if before_usb.owned.fields != after_usb.owned.fields {
        let keys = before_usb
            .owned
            .fields
            .keys()
            .chain(after_usb.owned.fields.keys())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .filter(|key| before_usb.owned.fields.get(*key) != after_usb.owned.fields.get(*key))
            .map(String::as_str)
            .collect::<Vec<_>>();
        // Keys come from the bounded fixed sysfs field set; never report values.
        changed.push(format!("owned USB fields [{}]", keys.join(", ")));
    }
    compare!(
        before_usb.owned.descriptors,
        after_usb.owned.descriptors,
        "owned USB descriptors"
    );
    compare!(
        before_usb.owned.interfaces,
        after_usb.owned.interfaces,
        "owned USB interfaces/endpoints"
    );
    compare!(
        before_usb.normal_interface,
        after_usb.normal_interface,
        "canonical interface"
    );
    compare!(
        before_usb.interface_inode,
        after_usb.interface_inode,
        "interface inode"
    );
    compare!(
        before_usb.sound_device,
        after_usb.sound_device,
        "sound mapping"
    );
    if changed.is_empty() {
        Ok(())
    } else {
        // Components and fixed field keys only: no inventory or identity values.
        Err(bad(format!("{context}: {}", changed.join(", "))))
    }
}

/// Count references to this exact canonical topology without reading unrelated
/// descriptor/field blobs. Duplicate aliases to the owned node remain invalid,
/// including when other device inventory entries are unreadable or disappear.
fn require_unique_location(usb: &Path, topology: &str) -> Result<(), ServiceError> {
    let entries = std::fs::read_dir(usb)
        .map_err(bad)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(bad)?;
    if entries.len() > 512 {
        return Err(bad("Passive USB inventory exceeded bound"));
    }
    let matches = entries
        .iter()
        .filter(|entry| {
            entry.path().join("idVendor").exists()
                && std::fs::canonicalize(entry.path())
                    .is_ok_and(|location| location == Path::new(topology))
        })
        .count();
    if matches != 1 {
        return Err(bad(
            "Owned physical location has no unique passive candidate",
        ));
    }
    Ok(())
}

pub(crate) fn baseline(
    normal: &FirmwareUsbObservation,
) -> Result<EnumerationReceipt, ServiceError> {
    firmware_entry_enumeration::capture_at(
        Path::new("/sys/bus/usb/devices"),
        Path::new("/sys/class/sound"),
        normal,
    )
}
pub(crate) fn accept(
    normal: &FirmwareUsbObservation,
    generation: u64,
    guard: &mut DeviceSessionGuard,
) -> Result<CompletionBindingWitness, ServiceError> {
    accept_association(normal, normal, generation, guard, false)
}
/// A recovery guard remains bound to its fresh boot anchor. Return association
/// instead uses the original normal serial retained in the checked local history.
pub(crate) fn accept_normal_return(
    anchor: &FirmwareUsbObservation,
    original_normal: &FirmwareUsbObservation,
    generation: u64,
    guard: &mut DeviceSessionGuard,
) -> Result<CompletionBindingWitness, ServiceError> {
    accept_association(anchor, original_normal, generation, guard, true)
}
fn accept_association(
    anchor: &FirmwareUsbObservation,
    association: &FirmwareUsbObservation,
    generation: u64,
    guard: &mut DeviceSessionGuard,
    normal_return_only: bool,
) -> Result<CompletionBindingWitness, ServiceError> {
    if generation == 0
        || !guard.owns_normal_binding(anchor)
        || !associated_identity(association, anchor)
    {
        return Err(bad(
            "Fresh binding requires this live original physical owner",
        ));
    }
    let observation = observe_association_at(
        association,
        generation,
        Path::new("/sys/bus/usb/devices"),
        Path::new("/sys/class/sound"),
        crate::raw_backend::completion_endpoints()?,
        normal_return_only,
    )?;
    guard.retain_aliases(&[alias(&observation.binding)?])?;
    // Observe again after acquiring the dynamic alias; stale numeric cards are never reused.
    let after = observe_association_at(
        association,
        generation,
        Path::new("/sys/bus/usb/devices"),
        Path::new("/sys/class/sound"),
        crate::raw_backend::completion_endpoints()?,
        normal_return_only,
    )?;
    require_same_target(
        &observation,
        &after,
        "Fresh completion alias changed while locking",
    )?;
    Ok(CompletionBindingWitness {
        observation,
        normal_sha256: anchor.digest(),
        association: association.clone(),
        normal_return_only,
        #[cfg(test)]
        fixture_source: None,
    })
}

fn alias(binding: &FirmwareUsbObservation) -> Result<EndpointIdentity, ServiceError> {
    let address = binding
        .endpoint
        .strip_prefix("hw:")
        .ok_or_else(|| bad("Completion endpoint is not an actual ALSA duplex address"))?;
    let fields = address.split(',').collect::<Vec<_>>();
    if fields.len() != 3 || fields[0].is_empty() {
        return Err(bad("Completion ALSA alias fields invalid"));
    }
    Ok(EndpointIdentity {
        location: binding.interface.clone(),
        card_id: fields[0].into(),
        device: fields[1].parse().map_err(bad)?,
        subdevice: fields[2].parse().map_err(bad)?,
    })
}
pub(crate) fn observe_at(
    normal: &FirmwareUsbObservation,
    generation: u64,
    usb: &Path,
    sound: &Path,
    endpoints: Vec<CompletionEndpointObservation>,
) -> Result<CompletionBindingObservation, ServiceError> {
    observe_association_at(normal, generation, usb, sound, endpoints, false)
}
/// This association identifies one held location; it never establishes protocol
/// mode or receiver version. The GP-200 boot USB serial is a version label, not
/// the normal family serial. Only this observed model-specific transition is admitted.
pub(crate) fn associated_identity(
    original: &FirmwareUsbObservation,
    candidate: &FirmwareUsbObservation,
) -> bool {
    if original.validate().is_err()
        || candidate.validate().is_err()
        || original.vendor != 0x84ef
        || candidate.vendor != 0x84ef
        || original.topology != candidate.topology
    {
        return false;
    }
    match (original.product, candidate.product) {
        (0x002a, 0x002a) => candidate.serial == original.serial,
        (0x002a, 0x002b) | (0x002b, 0x002b) => {
            candidate.serial == "V1.0.0"
                && (original.product == 0x002a || original.serial == "V1.0.0")
        }
        _ => false,
    }
}
fn observe_association_at(
    normal: &FirmwareUsbObservation,
    generation: u64,
    usb: &Path,
    sound: &Path,
    endpoints: Vec<CompletionEndpointObservation>,
    normal_return_only: bool,
) -> Result<CompletionBindingObservation, ServiceError> {
    observe_association_with(
        normal,
        generation,
        usb,
        endpoints,
        normal_return_only,
        |binding| {
            let receipt = firmware_entry_enumeration::capture_at(usb, sound, binding)?;
            let owned_inventory = receipt
                .devices
                .iter()
                .filter(|device| device.location == normal.topology)
                .collect::<Vec<_>>();
            if owned_inventory.as_slice() != [&receipt.owned] {
                return Err(bad(
                    "Owned USB inventory is incomplete or changed during passive capture",
                ));
            }
            Ok(receipt)
        },
    )
}

/// Consuming refresh retains two fresh, equal owned snapshots and the shared
/// before/after unique-topology checks. Full inventory remains in preparation
/// and review observations. The empty callback is optimized out in production;
/// fixtures use this exact boundary to exercise changes between snapshots.
fn refresh_association_with(
    normal: &FirmwareUsbObservation,
    generation: u64,
    usb: &Path,
    sound: &Path,
    endpoints: Vec<CompletionEndpointObservation>,
    normal_return_only: bool,
    between_snapshots: impl FnOnce(),
) -> Result<CompletionBindingObservation<OwnedEnumerationReceipt>, ServiceError> {
    observe_association_with(
        normal,
        generation,
        usb,
        endpoints,
        normal_return_only,
        |binding| {
            let receipt = firmware_entry_enumeration::capture_owned_at(sound, binding)?;
            between_snapshots();
            let after = firmware_entry_enumeration::capture_owned_at(sound, binding)?;
            if receipt != after {
                return Err(bad(
                    "Owned USB/interface/sound mapping changed during passive refresh",
                ));
            }
            Ok(receipt)
        },
    )
}

fn observe_association_with<R: AsRef<OwnedEnumerationReceipt>>(
    normal: &FirmwareUsbObservation,
    generation: u64,
    usb: &Path,
    endpoints: Vec<CompletionEndpointObservation>,
    normal_return_only: bool,
    capture: impl FnOnce(&FirmwareUsbObservation) -> Result<R, ServiceError>,
) -> Result<CompletionBindingObservation<R>, ServiceError> {
    normal.validate()?;
    if generation == 0
        || !associated_identity(normal, normal)
        || (normal_return_only && normal.product != 0x002a)
    {
        return Err(bad(
            "Completion original GP-200 association or generation invalid",
        ));
    }
    require_unique_location(usb, &normal.topology)?;
    let mut candidates = endpoints
        .into_iter()
        .filter(|e| e.binding.topology == normal.topology)
        .collect::<Vec<_>>();
    if candidates.len() != 1 {
        return Err(bad(
            "Fresh physical pedal has ambiguous or absent dynamic ALSA duplex aliases",
        ));
    }
    let candidate = candidates.pop().expect("one fresh endpoint");
    candidate.binding.validate()?;
    if !associated_identity(normal, &candidate.binding)
        || (normal_return_only && candidate.binding.product != 0x002a)
    {
        return Err(bad(
            "Fresh GP-200 model/serial differs from the owned association",
        ));
    }
    let receipt = capture(&candidate.binding)?;
    require_unique_location(usb, &normal.topology)?;
    let owned = receipt.as_ref();
    if owned.owned.location != normal.topology
        || owned.owned.fields.get("serial") != Some(&candidate.binding.serial)
    {
        return Err(bad(
            "Fresh endpoint maps to a different or incomplete USB source",
        ));
    }
    if candidate.binding.product == 0x002b {
        validate_observed_boot_usb(owned)?;
    }
    let relation = if candidate.binding == *normal {
        "Fresh reader/generation at same enumeration; protocol mode remains unobserved until exact reviewed identity query"
    } else {
        "Fresh descriptor/interface/dynamic alias at held physical location; protocol mode remains unobserved until exact reviewed identity query"
    };
    Ok(CompletionBindingObservation {
        binding: candidate.binding,
        selection: candidate.selection,
        usb: receipt,
        observed_generation: generation,
        relation: relation.into(),
    })
}

fn validate_observed_boot_usb(receipt: &OwnedEnumerationReceipt) -> Result<(), ServiceError> {
    let observed = &receipt.owned;
    for (field, expected) in [
        ("manufacturer", "Valeton"),
        ("product", "GP-200"),
        ("bcdDevice", "0100"),
        ("bNumInterfaces", "1"),
        ("bNumConfigurations", "1"),
        ("bConfigurationValue", "1"),
    ] {
        if observed.fields.get(field).map(String::as_str) != Some(expected) {
            return Err(bad(
                "Fresh boot USB metadata differs from observed GP-200 profile",
            ));
        }
    }
    // Actual first-entry observation, 2026-10-12. Descriptor equality binds the
    // boot MIDI layout (one interface, bulk83/03); it is not a receiver query.
    const DESCRIPTORS: &[u8] = &[
        0x12, 0x01, 0x00, 0x02, 0x00, 0x00, 0x00, 0x40, 0xef, 0x84, 0x2b, 0x00, 0x00, 0x01, 0x01,
        0x02, 0x03, 0x01, 0x09, 0x02, 0x53, 0x00, 0x01, 0x01, 0x04, 0x80, 0x20, 0x09, 0x04, 0x00,
        0x00, 0x02, 0x01, 0x03, 0x00, 0x05, 0x07, 0x24, 0x01, 0x00, 0x01, 0x41, 0x00, 0x06, 0x24,
        0x02, 0x01, 0x01, 0x00, 0x06, 0x24, 0x02, 0x02, 0x02, 0x00, 0x09, 0x24, 0x03, 0x01, 0x09,
        0x01, 0x02, 0x01, 0x00, 0x09, 0x24, 0x03, 0x02, 0x0a, 0x01, 0x01, 0x01, 0x00, 0x09, 0x05,
        0x83, 0x02, 0x00, 0x02, 0x00, 0x00, 0x00, 0x05, 0x25, 0x01, 0x01, 0x09, 0x09, 0x05, 0x03,
        0x02, 0x00, 0x02, 0x00, 0x00, 0x00, 0x05, 0x25, 0x01, 0x01, 0x01,
    ];
    let interface_name = Path::new(&receipt.normal_interface)
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| bad("Boot interface absent"))?;
    if Path::new(&receipt.normal_interface).parent() != Some(Path::new(&observed.location)) {
        return Err(bad(
            "Boot MIDI interface moved outside the exact held USB node",
        ));
    }
    let interface = observed
        .interfaces
        .get(interface_name)
        .ok_or_else(|| bad("Boot endpoint interface absent from fresh descriptors"))?;
    if observed.descriptors != DESCRIPTORS || observed.interfaces.len() != 1 {
        return Err(bad(
            "Fresh boot descriptors differ from observed GP-200 profile",
        ));
    }
    for (field, expected) in [
        ("bInterfaceNumber", "00"),
        ("bAlternateSetting", "0"),
        ("bNumEndpoints", "02"),
        ("bInterfaceClass", "01"),
        ("bInterfaceSubClass", "03"),
        ("bInterfaceProtocol", "00"),
        ("ep_83/bEndpointAddress", "83"),
        ("ep_03/bEndpointAddress", "03"),
        ("ep_83/bmAttributes", "02"),
        ("ep_03/bmAttributes", "02"),
        ("ep_83/wMaxPacketSize", "0200"),
        ("ep_03/wMaxPacketSize", "0200"),
    ] {
        if interface.get(field).map(String::as_str) != Some(expected) {
            return Err(bad(
                "Fresh boot MIDI interface/endpoints differ from observed profile",
            ));
        }
    }
    Ok(())
}

pub(crate) fn observed_transition(previous: &EnumerationReceipt) -> Result<bool, ServiceError> {
    let (devices, errors) =
        firmware_entry_enumeration::inventory_at(Path::new("/sys/bus/usb/devices"))?;
    if !errors.is_empty() {
        return Err(bad("Passive transition inventory has unexplained errors"));
    }
    let candidates = devices
        .iter()
        .filter(|d| d.location == previous.owned.location)
        .collect::<Vec<_>>();
    match candidates.as_slice() {
        [] => Ok(true),
        [current] => Ok(**current != previous.owned),
        _ => Err(bad("Passive transition location is ambiguous")),
    }
}

#[cfg(test)]
pub(crate) fn accept_fixture(
    normal: &FirmwareUsbObservation,
    generation: u64,
    guard: &mut DeviceSessionGuard,
    usb: &Path,
    sound: &Path,
    endpoints: Vec<CompletionEndpointObservation>,
) -> Result<CompletionBindingWitness, ServiceError> {
    accept_association_fixture(
        normal, normal, generation, guard, usb, sound, endpoints, false,
    )
}
#[cfg(test)]
#[allow(clippy::too_many_arguments)]
pub(crate) fn accept_association_fixture(
    anchor: &FirmwareUsbObservation,
    association: &FirmwareUsbObservation,
    generation: u64,
    guard: &mut DeviceSessionGuard,
    usb: &Path,
    sound: &Path,
    endpoints: Vec<CompletionEndpointObservation>,
    normal_return_only: bool,
) -> Result<CompletionBindingWitness, ServiceError> {
    if generation == 0
        || !guard.owns_normal_binding(anchor)
        || !associated_identity(association, anchor)
    {
        return Err(bad("Fixture requires its actual local retained guard"));
    }
    let observation = observe_association_at(
        association,
        generation,
        usb,
        sound,
        endpoints.clone(),
        normal_return_only,
    )?;
    guard.retain_aliases(&[alias(&observation.binding)?])?;
    let after = observe_association_at(
        association,
        generation,
        usb,
        sound,
        endpoints.clone(),
        normal_return_only,
    )?;
    require_same_target(
        &observation,
        &after,
        "Fixture association changed while locking",
    )?;
    Ok(CompletionBindingWitness {
        observation,
        normal_sha256: anchor.digest(),
        association: association.clone(),
        normal_return_only,
        fixture_source: Some((usb.into(), sound.into(), endpoints)),
    })
}

#[cfg(test)]
#[path = "firmware_linux_completion_binding_tests.rs"]
mod tests;
