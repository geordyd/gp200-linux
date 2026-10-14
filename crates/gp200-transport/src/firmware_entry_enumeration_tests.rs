use super::*;
use std::os::unix::fs::symlink;
use std::path::PathBuf;
thread_local! {
    static READ_LOG: std::cell::RefCell<Option<Vec<PathBuf>>> = const { std::cell::RefCell::new(None) };
}
pub(crate) fn record_read(path: &Path) {
    READ_LOG.with(|log| {
        if let Some(paths) = log.borrow_mut().as_mut() {
            paths.push(path.into());
        }
    });
}
/// Thread-local auditing of the real bounded-read helper, scoped to one test.
pub(crate) struct ReadLog;
impl ReadLog {
    pub(crate) fn start() -> Self {
        READ_LOG.with(|log| assert!(log.replace(Some(Vec::new())).is_none()));
        Self
    }
    pub(crate) fn finish(self) -> Vec<PathBuf> {
        READ_LOG.with(|log| log.borrow_mut().take().unwrap())
    }
}
impl Drop for ReadLog {
    fn drop(&mut self) {
        READ_LOG.with(|log| {
            log.borrow_mut().take();
        });
    }
}
static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub(crate) struct Fixture {
    pub(crate) root: PathBuf,
    pub(crate) binding: FirmwareUsbObservation,
}
impl Fixture {
    pub(crate) fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "gp200-passive-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let usb = root.join("usb");
        let node = usb.join("1-2");
        let interface = node.join("1-2:1.0");
        let ep = interface.join("ep_81");
        let sound = root.join("sound/card3");
        fs::create_dir_all(&ep).unwrap();
        fs::create_dir_all(&sound).unwrap();
        for (key, value) in [
            ("idVendor", "84ef"),
            ("idProduct", "002a"),
            ("serial", "20220118"),
            ("busnum", "1"),
            ("devnum", "4"),
            ("bcdDevice", "0101"),
            ("bDeviceClass", "ef"),
            ("bDeviceSubClass", "02"),
            ("bDeviceProtocol", "01"),
            ("bNumConfigurations", "1"),
            ("bNumInterfaces", "4"),
            ("bConfigurationValue", "1"),
            ("speed", "480"),
        ] {
            fs::write(node.join(key), value).unwrap();
        }
        fs::write(node.join("descriptors"), vec![0; 18]).unwrap();
        for (key, value) in [
            ("bInterfaceNumber", "0"),
            ("bAlternateSetting", "0"),
            ("bNumEndpoints", "1"),
            ("bInterfaceClass", "01"),
            ("bInterfaceSubClass", "03"),
            ("bInterfaceProtocol", "00"),
        ] {
            fs::write(interface.join(key), value).unwrap();
        }
        for (key, value) in [
            ("bEndpointAddress", "81"),
            ("bmAttributes", "02"),
            ("wMaxPacketSize", "0200"),
            ("bInterval", "0"),
        ] {
            fs::write(ep.join(key), value).unwrap();
        }
        symlink(&interface, sound.join("device")).unwrap();
        fs::write(sound.join("id"), "GP200").unwrap();
        Self {
            binding: FirmwareUsbObservation {
                topology: node.display().to_string(),
                interface: interface.display().to_string(),
                endpoint: "hw:GP200,0,0".into(),
                vendor: 0x84ef,
                product: 0x002a,
                serial: "20220118".into(),
            },
            root,
        }
    }
    fn capture(&self) -> Result<EnumerationReceipt, ServiceError> {
        capture_at(
            &self.root.join("usb"),
            &self.root.join("sound"),
            &self.binding,
        )
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}
#[test]
fn exact_descriptor_interface_endpoint_and_fresh_os_instance_are_retained() {
    let f = Fixture::new();
    let before = f.capture().unwrap();
    assert_eq!(before.devices.len(), 1);
    assert_eq!(before.owned.descriptors.len(), 18);
    assert_eq!(
        before.owned.interfaces["1-2:1.0"]["ep_81/bEndpointAddress"],
        "81"
    );
    fs::write(Path::new(&f.binding.topology).join("devnum"), "5").unwrap();
    let after = f.capture().unwrap();
    assert_ne!(before.owned, after.owned);
    assert_eq!(before.owned.fields["devnum"], "4");
    assert_eq!(after.owned.fields["devnum"], "5");
}
#[test]
fn full_receipt_serialization_keeps_owned_fields_flat_and_inventory_complete() {
    let f = Fixture::new();
    let full = f.capture().unwrap();
    let owned = capture_owned_at(&f.root.join("sound"), &f.binding).unwrap();
    assert_eq!(full.as_ref(), &owned);
    let value = serde_json::to_value(&full).unwrap();
    let object = value.as_object().unwrap();
    let keys = object.keys().map(String::as_str).collect::<Vec<_>>();
    assert_eq!(
        keys,
        [
            "devices",
            "errors",
            "interface_inode",
            "normal_interface",
            "owned",
            "sound_device"
        ]
    );
    assert_eq!(value["devices"].as_array().unwrap().len(), 1);
    assert_eq!(value["owned"], value["devices"][0]);
}
#[test]
fn fixed_serial_and_same_vid_pid_do_not_hide_disappearance_or_sound_rebinding() {
    let f = Fixture::new();
    fs::remove_file(f.root.join("sound/card3/device")).unwrap();
    symlink(&f.root, f.root.join("sound/card3/device")).unwrap();
    assert!(f.capture().is_err());
    fs::remove_file(Path::new(&f.binding.topology).join("descriptors")).unwrap();
    assert!(f.capture().is_err());
}
