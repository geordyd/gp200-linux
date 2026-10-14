//! Passive Linux enumeration only. No USB handle/control request or MIDI open.
use gp200_domain::{FirmwareUsbObservation, ServiceError};
use serde::Serialize;
use std::{collections::BTreeMap, fs, os::unix::fs::MetadataExt, path::Path};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct UsbReceipt {
    pub location: String,
    pub inode: u64,
    pub fields: BTreeMap<String, String>,
    pub descriptors: Vec<u8>,
    pub interfaces: BTreeMap<String, BTreeMap<String, String>>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct OwnedEnumerationReceipt {
    pub owned: UsbReceipt,
    pub normal_interface: String,
    pub interface_inode: u64,
    pub sound_device: String,
}
impl AsRef<OwnedEnumerationReceipt> for OwnedEnumerationReceipt {
    fn as_ref(&self) -> &OwnedEnumerationReceipt {
        self
    }
}
/// Full review context retains the same flattened serialized fields. A fresh
/// owned receipt has its own type and never represents an empty host inventory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct EnumerationReceipt {
    pub devices: Vec<UsbReceipt>,
    pub errors: Vec<String>,
    #[serde(flatten)]
    pub target: OwnedEnumerationReceipt,
}
impl std::ops::Deref for EnumerationReceipt {
    type Target = OwnedEnumerationReceipt;
    fn deref(&self) -> &Self::Target {
        &self.target
    }
}
impl AsRef<OwnedEnumerationReceipt> for EnumerationReceipt {
    fn as_ref(&self) -> &OwnedEnumerationReceipt {
        &self.target
    }
}
fn read(path: &Path, bound: usize) -> Result<Vec<u8>, ServiceError> {
    use std::io::Read;
    #[cfg(test)]
    tests::record_read(path);
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .and_then(|f| f.take(bound as u64 + 1).read_to_end(&mut bytes))
        .map_err(|e| ServiceError::invalid(format!("Passive read {}: {e}", path.display())))?;
    if bytes.len() > bound {
        return Err(ServiceError::invalid("Passive sysfs field exceeded bound"));
    }
    Ok(bytes)
}
fn field(path: &Path, name: &str) -> Result<String, ServiceError> {
    String::from_utf8(read(&path.join(name), 4096)?)
        .map(|s| s.trim().to_owned())
        .map_err(|_| ServiceError::invalid("Non-UTF8 passive sysfs field"))
}
fn device(path: &Path) -> Result<UsbReceipt, ServiceError> {
    let path = fs::canonicalize(path).map_err(|e| ServiceError::invalid(e.to_string()))?;
    let metadata = fs::metadata(&path).map_err(|e| ServiceError::invalid(e.to_string()))?;
    let mut fields = BTreeMap::new();
    for name in [
        "idVendor",
        "idProduct",
        "busnum",
        "devnum",
        "bcdDevice",
        "bDeviceClass",
        "bDeviceSubClass",
        "bDeviceProtocol",
        "bNumConfigurations",
        "bNumInterfaces",
        "bConfigurationValue",
        "speed",
    ] {
        fields.insert(name.into(), field(&path, name)?);
    }
    for name in ["serial", "manufacturer", "product", "authorized"] {
        if path.join(name).exists() {
            fields.insert(name.into(), field(&path, name)?);
        }
    }
    let descriptors = read(&path.join("descriptors"), 65536)?;
    if descriptors.len() < 18 {
        return Err(ServiceError::invalid(
            "Passive descriptor blob is incomplete",
        ));
    }
    let mut interfaces = BTreeMap::new();
    let mut children: Vec<_> = fs::read_dir(&path)
        .map_err(|e| ServiceError::invalid(e.to_string()))?
        .collect::<Result<_, _>>()
        .map_err(|e| ServiceError::invalid(e.to_string()))?;
    children.sort_by_key(|e| e.file_name());
    for entry in children {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.contains(':') {
            continue;
        }
        let interface = entry.path();
        if !interface.join("bInterfaceNumber").exists() {
            continue;
        }
        let mut values = BTreeMap::new();
        for key in [
            "bInterfaceNumber",
            "bAlternateSetting",
            "bNumEndpoints",
            "bInterfaceClass",
            "bInterfaceSubClass",
            "bInterfaceProtocol",
        ] {
            values.insert(key.into(), field(&interface, key)?);
        }
        let endpoints =
            fs::read_dir(&interface).map_err(|e| ServiceError::invalid(e.to_string()))?;
        for endpoint in endpoints {
            let endpoint = endpoint.map_err(|e| ServiceError::invalid(e.to_string()))?;
            let ep = endpoint.file_name().to_string_lossy().into_owned();
            if !ep.starts_with("ep_") {
                continue;
            }
            for key in [
                "bEndpointAddress",
                "bmAttributes",
                "wMaxPacketSize",
                "bInterval",
                "type",
                "direction",
                "interval",
            ] {
                if endpoint.path().join(key).exists() {
                    values.insert(format!("{ep}/{key}"), field(&endpoint.path(), key)?);
                }
            }
        }
        interfaces.insert(name, values);
    }
    Ok(UsbReceipt {
        location: path.display().to_string(),
        inode: metadata.ino(),
        fields,
        descriptors,
        interfaces,
    })
}
pub(crate) fn capture_at(
    usb: &Path,
    sound: &Path,
    binding: &FirmwareUsbObservation,
) -> Result<EnumerationReceipt, ServiceError> {
    let target = capture_owned_at(sound, binding)?;
    let (devices, errors) = inventory_at(usb)?;
    Ok(EnumerationReceipt {
        devices,
        errors,
        target,
    })
}
/// Fresh bounded reads of every owned field, descriptor, interface and endpoint
/// plus the exact canonical interface/inode and unique ALSA sound mapping.
/// This does not scan or read unrelated USB descriptor/field blobs.
pub(crate) fn capture_owned_at(
    sound: &Path,
    binding: &FirmwareUsbObservation,
) -> Result<OwnedEnumerationReceipt, ServiceError> {
    binding.validate()?;
    let owned = device(Path::new(&binding.topology))?;
    let hex = |key: &str| {
        owned
            .fields
            .get(key)
            .and_then(|v| u16::from_str_radix(v, 16).ok())
    };
    if hex("idVendor") != Some(binding.vendor)
        || hex("idProduct") != Some(binding.product)
        || owned.fields.get("serial") != Some(&binding.serial)
    {
        return Err(ServiceError::invalid("Fresh owned USB identity changed"));
    }
    let interface = fs::canonicalize(&binding.interface)
        .map_err(|e| ServiceError::invalid(format!("Normal interface disappeared: {e}")))?;
    if !interface.starts_with(&owned.location) {
        return Err(ServiceError::invalid(
            "Normal interface moved outside the owned USB location",
        ));
    }
    let interface_inode = fs::metadata(&interface)
        .map_err(|e| ServiceError::invalid(e.to_string()))?
        .ino();
    let card = binding
        .endpoint
        .strip_prefix("hw:")
        .and_then(|s| s.split(',').next())
        .ok_or_else(|| ServiceError::invalid("Normal raw endpoint malformed"))?;
    if card.is_empty() || !card.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
        return Err(ServiceError::invalid(
            "Physical ALSA card identifier invalid",
        ));
    }
    let mut mappings = Vec::new();
    for entry in fs::read_dir(sound).map_err(|e| ServiceError::invalid(e.to_string()))? {
        let entry = entry.map_err(|e| ServiceError::invalid(e.to_string()))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name
            .strip_prefix("card")
            .is_some_and(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
        {
            continue;
        }
        let id = fs::read_to_string(entry.path().join("id")).ok();
        if id.as_deref().map(str::trim) == Some(card)
            || (card.bytes().all(|b| b.is_ascii_digit()) && name == format!("card{card}"))
        {
            mappings.push(
                fs::canonicalize(entry.path().join("device"))
                    .map_err(|e| ServiceError::invalid(e.to_string()))?,
            );
        }
    }
    if mappings.len() != 1 {
        return Err(ServiceError::invalid(
            "Physical sound card mapping missing or ambiguous",
        ));
    }
    let sound_device = mappings.pop().expect("one mapping");
    if sound_device != interface {
        return Err(ServiceError::invalid(
            "Fresh sound endpoint maps to a changed interface",
        ));
    }
    Ok(OwnedEnumerationReceipt {
        owned,
        normal_interface: interface.display().to_string(),
        interface_inode,
        sound_device: sound_device.display().to_string(),
    })
}
pub(crate) fn inventory_at(usb: &Path) -> Result<(Vec<UsbReceipt>, Vec<String>), ServiceError> {
    let mut nodes: Vec<_> = fs::read_dir(usb)
        .map_err(|e| ServiceError::invalid(e.to_string()))?
        .collect::<Result<_, _>>()
        .map_err(|e| ServiceError::invalid(e.to_string()))?;
    nodes.sort_by_key(|e| e.file_name());
    if nodes.len() > 512 {
        return Err(ServiceError::invalid(
            "Passive USB inventory exceeded bound",
        ));
    }
    let mut devices = Vec::new();
    let mut errors = Vec::new();
    for node in nodes {
        if node.path().join("idVendor").exists() {
            match device(&node.path()) {
                Ok(d) => devices.push(d),
                Err(e) => errors.push(format!("{}: {}", node.path().display(), e.message)),
            }
        }
    }
    Ok((devices, errors))
}
#[cfg(test)]
#[path = "firmware_entry_enumeration_tests.rs"]
pub(crate) mod tests;
