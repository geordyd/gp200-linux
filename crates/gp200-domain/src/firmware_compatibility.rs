//! Descriptive local firmware reports. These types never represent a native
//! observation, transport mapping, validation profile, or permission to write.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::{fmt, str::FromStr};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("Invalid firmware description: {0}")]
pub struct FirmwareDescriptionError(&'static str);

/// Numeric ordering applies to an entered version, not to image-header bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FirmwareVersion([u16; 3]);

impl FirmwareVersion {
    pub fn new(major: u16, minor: u16, patch: u16) -> Result<Self, FirmwareDescriptionError> {
        let components = [major, minor, patch];
        if components.iter().any(|number| *number > 999) {
            return Err(FirmwareDescriptionError("component exceeds 999"));
        }
        Ok(Self(components))
    }
    pub const fn components(self) -> [u16; 3] {
        self.0
    }
}

impl FromStr for FirmwareVersion {
    type Err = FirmwareDescriptionError;
    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        if raw.len() > 16 || !raw.is_ascii() {
            return Err(FirmwareDescriptionError(
                "version must be at most 16 ASCII bytes",
            ));
        }
        let text = raw.strip_prefix(['V', 'v']).unwrap_or(raw);
        let mut parts = text.split('.');
        let mut components = [0; 3];
        for number in &mut components {
            let part = parts
                .next()
                .ok_or(FirmwareDescriptionError("three components required"))?;
            if part.is_empty()
                || part.len() > 3
                || !part.bytes().all(|byte| byte.is_ascii_digit())
                || (part.len() > 1 && part.starts_with('0'))
            {
                return Err(FirmwareDescriptionError("invalid decimal component"));
            }
            *number = part
                .parse()
                .map_err(|_| FirmwareDescriptionError("invalid decimal component"))?;
        }
        if parts.next().is_some() {
            return Err(FirmwareDescriptionError("three components required"));
        }
        Self::new(components[0], components[1], components[2])
    }
}
impl fmt::Display for FirmwareVersion {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}.{}.{}", self.0[0], self.0[1], self.0[2])
    }
}
impl Serialize for FirmwareVersion {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}
impl<'de> Deserialize<'de> for FirmwareVersion {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

/// Explicit metadata selection, separate from any wire mapping identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum EditorMetadataProfile {
    Gp200Editor160,
    Gp200Editor181,
}
impl EditorMetadataProfile {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Gp200Editor160 => "GP-200 editor 1.6.0 metadata",
            Self::Gp200Editor181 => "GP-200 editor 1.8.1 metadata",
        }
    }
}

fn label_valid(value: &str, maximum: usize) -> bool {
    !value.is_empty()
        && value.len() <= maximum
        && !value.trim().is_empty()
        && !value.chars().any(char::is_control)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", deny_unknown_fields)]
enum AssociationData {
    UsbSerial {
        model: String,
        vendor_id: u16,
        product_id: u16,
        serial: String,
    },
    NamedLocalRecord {
        model: String,
        record: String,
    },
}

/// A remembered descriptive key. A local record requires later explicit session
/// association; an endpoint, card number or fingerprint hint is not a key here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct FirmwareDeviceAssociation(AssociationData);
impl FirmwareDeviceAssociation {
    pub fn usb_serial(
        model: impl Into<String>,
        vendor_id: u16,
        product_id: u16,
        serial: impl Into<String>,
    ) -> Result<Self, FirmwareDescriptionError> {
        Self::checked(AssociationData::UsbSerial {
            model: model.into(),
            vendor_id,
            product_id,
            serial: serial.into(),
        })
    }
    pub fn named_local_record(
        model: impl Into<String>,
        record: impl Into<String>,
    ) -> Result<Self, FirmwareDescriptionError> {
        Self::checked(AssociationData::NamedLocalRecord {
            model: model.into(),
            record: record.into(),
        })
    }
    fn checked(data: AssociationData) -> Result<Self, FirmwareDescriptionError> {
        let valid = match &data {
            AssociationData::UsbSerial {
                model,
                vendor_id,
                product_id,
                serial,
            } => {
                label_valid(model, 64)
                    && *vendor_id != 0
                    && *product_id != 0
                    && label_valid(serial, 128)
            }
            AssociationData::NamedLocalRecord { model, record } => {
                label_valid(model, 64) && label_valid(record, 128)
            }
        };
        if !valid {
            return Err(FirmwareDescriptionError(
                "empty, oversized or invalid device association",
            ));
        }
        Ok(Self(data))
    }
    pub fn model(&self) -> &str {
        match &self.0 {
            AssociationData::UsbSerial { model, .. }
            | AssociationData::NamedLocalRecord { model, .. } => model,
        }
    }
    pub fn usb_identity(&self) -> Option<(u16, u16, &str)> {
        match &self.0 {
            AssociationData::UsbSerial {
                vendor_id,
                product_id,
                serial,
                ..
            } => Some((*vendor_id, *product_id, serial)),
            _ => None,
        }
    }
    pub fn local_record(&self) -> Option<&str> {
        match &self.0 {
            AssociationData::NamedLocalRecord { record, .. } => Some(record),
            _ => None,
        }
    }
}
impl<'de> Deserialize<'de> for FirmwareDeviceAssociation {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::checked(AssociationData::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DateData {
    year: u16,
    month: u8,
    day: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct FirmwareReportDate(DateData);
impl FirmwareReportDate {
    pub fn new(year: u16, month: u8, day: u8) -> Result<Self, FirmwareDescriptionError> {
        let leap =
            year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400));
        let maximum = match month {
            1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
            4 | 6 | 9 | 11 => 30,
            2 if leap => 29,
            2 => 28,
            _ => 0,
        };
        if year == 0 || year > 9999 || day == 0 || day > maximum {
            return Err(FirmwareDescriptionError("invalid Gregorian date"));
        }
        Ok(Self(DateData { year, month, day }))
    }
    pub const fn components(self) -> (u16, u8, u8) {
        (self.0.year, self.0.month, self.0.day)
    }
}
impl<'de> Deserialize<'de> for FirmwareReportDate {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let data = DateData::deserialize(deserializer)?;
        Self::new(data.year, data.month, data.day).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FirmwareReportProvenance {
    UserEnteredPedalDisplay,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReportData {
    raw_input: String,
    version: FirmwareVersion,
    device: FirmwareDeviceAssociation,
    recorded_date: FirmwareReportDate,
    provenance: FirmwareReportProvenance,
}

/// Validated structure still represents an assertion, including after import.
/// It cannot fill `DeviceIdentity.firmware` or produce capability evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct UserReportedFirmware(ReportData);
impl UserReportedFirmware {
    pub fn new(
        raw_input: impl Into<String>,
        device: FirmwareDeviceAssociation,
        recorded_date: FirmwareReportDate,
    ) -> Result<Self, FirmwareDescriptionError> {
        let raw_input = raw_input.into();
        let version = raw_input.parse()?;
        Ok(Self(ReportData {
            raw_input,
            version,
            device,
            recorded_date,
            provenance: FirmwareReportProvenance::UserEnteredPedalDisplay,
        }))
    }
    pub fn raw_input(&self) -> &str {
        &self.0.raw_input
    }
    pub const fn version(&self) -> FirmwareVersion {
        self.0.version
    }
    pub fn device(&self) -> &FirmwareDeviceAssociation {
        &self.0.device
    }
    pub const fn recorded_date(&self) -> FirmwareReportDate {
        self.0.recorded_date
    }
    pub const fn provenance(&self) -> FirmwareReportProvenance {
        self.0.provenance
    }
    /// This suggestion is local metadata only; it does not select a live context.
    pub fn suggested_metadata_profile(
        &self,
        associated_device: &FirmwareDeviceAssociation,
    ) -> Option<EditorMetadataProfile> {
        (self.device() == associated_device
            && associated_device.model() == "GP-200"
            && self.version().components() == [1, 6, 3])
        .then_some(EditorMetadataProfile::Gp200Editor160)
    }
}
impl<'de> Deserialize<'de> for UserReportedFirmware {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let data = ReportData::deserialize(deserializer)?;
        let parsed: FirmwareVersion = data.raw_input.parse().map_err(serde::de::Error::custom)?;
        if parsed != data.version {
            return Err(serde::de::Error::custom(
                "raw and canonical firmware versions disagree",
            ));
        }
        Ok(Self(data))
    }
}
