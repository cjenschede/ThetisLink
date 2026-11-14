// SPDX-License-Identifier: GPL-2.0-or-later

//! The serial ports of this machine, under the names that stay.
//!
//! On Windows a port is `COM13`, and that name follows the device. On Linux the
//! kernel hands out `/dev/ttyUSB4` or `/dev/ttyACM2` in the order the devices
//! show up, so after a reboot or a cable in another socket the same number can
//! be another device: an Amplitec moved from `ttyACM1` to `ttyACM2`, and a CAT
//! port to `ttyUSB4` while `ttyUSB3` became the SPE (2026-11-10/11). udev also
//! gives each port a name under `/dev/serial/by-id/` built from the device
//! itself, and that one stays. So the list offers that name, and two names
//! for one device count as the same port.
//!
//! One place for this: the settings window, the key line of CW and anything
//! else that lists or compares ports asks here.

use std::path::{Path, PathBuf};

/// Where udev puts the names that stay (Linux).
#[cfg(not(windows))]
const BY_ID_DIR: &str = "/dev/serial/by-id";

/// Every serial port of this machine as `serialport` lists it, each under the
/// name to keep in a conf: on Linux its `/dev/serial/by-id/` name when it has
/// one, else the kernel's; on Windows the COM name.
pub fn list() -> Vec<serialport::SerialPortInfo> {
    let ports = serialport::available_ports().unwrap_or_default();
    #[cfg(not(windows))]
    {
        under_names_that_stay(ports, Path::new(BY_ID_DIR))
    }
    #[cfg(windows)]
    ports
}

/// `list`, the names only.
pub fn names() -> Vec<String> {
    list().into_iter().map(|p| p.port_name).collect()
}

/// `ports` with each name replaced by the link in `by_id_dir` that points to
/// it, when there is one. Apart from `list` so a test can hand it a directory.
#[cfg(not(windows))]
fn under_names_that_stay(ports: Vec<serialport::SerialPortInfo>, by_id_dir: &Path) -> Vec<serialport::SerialPortInfo> {
    let links = by_id_links(by_id_dir);
    ports
        .into_iter()
        .map(|mut p| {
            p.port_name = stable_name(&p.port_name, &links);
            p
        })
        .collect()
}

/// The entries of a `/dev/serial/by-id/` directory and the devices they point to.
#[cfg(not(windows))]
fn by_id_links(by_id_dir: &Path) -> Vec<(PathBuf, PathBuf)> {
    let Ok(dir) = std::fs::read_dir(by_id_dir) else { return Vec::new() };
    dir.filter_map(|e| e.ok())
        .filter_map(|e| {
            let link = e.path();
            let target = std::fs::canonicalize(&link).ok()?;
            Some((link, target))
        })
        .collect()
}

/// The name to keep for `port`: the link that points to it, when there is one.
#[cfg_attr(windows, allow(dead_code))]
fn stable_name(port: &str, links: &[(PathBuf, PathBuf)]) -> String {
    links
        .iter()
        .find(|(_, target)| target == Path::new(port))
        .map(|(link, _)| link.to_string_lossy().into_owned())
        .unwrap_or_else(|| port.to_string())
}

/// Whether two names are the same port. Windows: COM names regardless of
/// case. Elsewhere: the same text, or the same device behind them, so a
/// `/dev/serial/by-id/` name and the `/dev/ttyUSB4` it points to are one port.
pub fn same(a: &str, b: &str) -> bool {
    #[cfg(windows)]
    {
        a.eq_ignore_ascii_case(b)
    }
    #[cfg(not(windows))]
    {
        a == b
            || match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
                (Ok(x), Ok(y)) => x == y,
                _ => false,
            }
    }
}

/// How a port reads in a list: on Linux a `/dev/serial/by-id/usb-` name
/// without that prefix (the device's own description is the part that tells
/// ports apart), anything else as it is.
pub fn label(name: &str) -> &str {
    #[cfg(not(windows))]
    {
        let rest = name.strip_prefix(BY_ID_DIR).and_then(|r| r.strip_prefix('/'));
        if let Some(rest) = rest {
            return rest.strip_prefix("usb-").unwrap_or(rest);
        }
    }
    name
}

/// The width of a port list in the settings window: as given on Windows; on
/// Linux wide enough for a `/dev/serial/by-id/` name, whose distinguishing
/// part (serial number, interface) sits at the end.
pub fn combo_width(windows_width: f32) -> f32 {
    if cfg!(windows) { windows_width } else { windows_width.max(380.0) }
}

/// One log line at start: every port in `names` (from `names()`, the same
/// list the settings window offers), and on Linux the device each is now.
pub fn log_ports(names: &[String]) {
    #[cfg(not(windows))]
    let shown: Vec<String> = names
        .iter()
        .map(|n| match std::fs::canonicalize(n) {
            Ok(dev) if dev != Path::new(n) => format!("{} = {}", label(n), dev.display()),
            _ => n.clone(),
        })
        .collect();
    #[cfg(windows)]
    let shown = names;
    log::info!("Serial ports ({}): {}", shown.len(), shown.join(", "));
}

/// A directory of its own for a test, removed when the test ends, also when it
/// fails. The name carries the process and a counter, so two tests in one run
/// and two runs side by side never share one. The path is resolved once, so a
/// temp directory that is itself a link (macOS: `/var`) reads the same as what
/// `canonicalize` gives back. Only the tests outside Windows use it.
#[cfg(all(test, not(windows)))]
pub(crate) struct TestDir(pub(crate) PathBuf);

#[cfg(all(test, not(windows)))]
impl TestDir {
    pub(crate) fn new(what: &str) -> Self {
        static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("tl-{what}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        TestDir(std::fs::canonicalize(&dir).unwrap())
    }
}

#[cfg(all(test, not(windows)))]
impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_port_with_a_link_gets_the_link_and_one_without_keeps_its_name() {
        let links = vec![
            (PathBuf::from("/dev/serial/by-id/usb-Silicon_Labs_CP2105_008DE7E0-if00-port0"), PathBuf::from("/dev/ttyUSB4")),
            (PathBuf::from("/dev/serial/by-id/usb-STMicroelectronics_STM32-if00"), PathBuf::from("/dev/ttyACM2")),
        ];
        assert_eq!(stable_name("/dev/ttyUSB4", &links), "/dev/serial/by-id/usb-Silicon_Labs_CP2105_008DE7E0-if00-port0");
        assert_eq!(stable_name("/dev/ttyACM2", &links), "/dev/serial/by-id/usb-STMicroelectronics_STM32-if00");
        assert_eq!(stable_name("/dev/ttyS0", &links), "/dev/ttyS0", "no link: the kernel's name");
        assert_eq!(stable_name("COM13", &[]), "COM13");
    }

    #[cfg(windows)]
    #[test]
    fn windows_port_names_are_the_same_regardless_of_case() {
        assert!(same("COM13", "com13"));
        assert!(!same("COM13", "COM12"));
        assert_eq!(label("COM13"), "COM13");
    }

    /// The case this module is for: a conf with the name that stays, a list
    /// or a CAT port with the kernel's name, and the other way round.
    #[cfg(not(windows))]
    #[test]
    fn a_link_and_the_device_it_points_to_are_the_same_port() {
        let tmp = TestDir::new("serial-ports");
        let dir = &tmp.0;
        let dev = dir.join("ttyUSB4");
        let other = dir.join("ttyUSB3");
        std::fs::write(&dev, b"").unwrap();
        std::fs::write(&other, b"").unwrap();
        let link = dir.join("usb-CP2105_008DE7E0-if00-port0");
        std::os::unix::fs::symlink(&dev, &link).unwrap();
        let by_path = dir.join("pci-0000_00_14.0-usb-0_2_1.0-port0");
        std::os::unix::fs::symlink(&dev, &by_path).unwrap();
        let (dev, other, link) = (dev.to_str().unwrap(), other.to_str().unwrap(), link.to_str().unwrap());
        assert!(same(link, dev), "the link and its device");
        assert!(same(dev, link), "either way round");
        assert!(same(link, by_path.to_str().unwrap()), "two links to one device, both resolved");
        assert!(!same(link, other), "not the device next to it");
        assert!(same("/dev/nothing-here", "/dev/nothing-here"), "the same text without a device");
        assert!(!same("COM13", "com13"), "case counts outside Windows");
    }

    /// What the settings window and the key line get: the list itself comes
    /// back under the names in the by-id directory, and a port without a link
    /// under its own. Each port keeps its type, the field the key line reads
    /// vendor, product and serial number from; the field-free kinds stand in for
    /// a USB port, whose struct changes shape with a feature another crate of the
    /// workspace switches on.
    #[cfg(not(windows))]
    #[test]
    fn the_list_comes_back_under_the_names_in_the_by_id_directory() {
        let tmp = TestDir::new("by-id-list");
        let (devs, by_id) = (tmp.0.join("dev"), tmp.0.join("by-id"));
        std::fs::create_dir_all(&devs).unwrap();
        std::fs::create_dir_all(&by_id).unwrap();
        for d in ["ttyUSB4", "ttyACM2", "ttyS0"] {
            std::fs::write(devs.join(d), b"").unwrap();
        }
        let cat = by_id.join("usb-Silicon_Labs_CP2105_008DE7E0-if00-port0");
        let amp = by_id.join("usb-STMicroelectronics_STM32-if00");
        std::os::unix::fs::symlink("../dev/ttyUSB4", &cat).unwrap();
        std::os::unix::fs::symlink("../dev/ttyACM2", &amp).unwrap();
        let text = |p: &Path| p.to_str().unwrap().to_string();
        use serialport::SerialPortType::{BluetoothPort, PciPort, Unknown};
        let port = |d: &str, kind: serialport::SerialPortType| serialport::SerialPortInfo {
            port_name: text(&devs.join(d)),
            port_type: kind,
        };
        let renamed = under_names_that_stay(
            vec![port("ttyUSB4", PciPort), port("ttyACM2", BluetoothPort), port("ttyS0", Unknown)],
            &by_id,
        );
        let names: Vec<String> = renamed.iter().map(|p| p.port_name.clone()).collect();
        assert_eq!(names, vec![text(&cat), text(&amp), text(&devs.join("ttyS0"))]);
        let kinds: Vec<_> = renamed.into_iter().map(|p| p.port_type).collect();
        assert_eq!(kinds, vec![PciPort, BluetoothPort, Unknown], "a renamed port keeps its type");
        let none = under_names_that_stay(vec![port("ttyUSB4", Unknown)], &tmp.0.join("not-there"));
        assert_eq!(none[0].port_name, text(&devs.join("ttyUSB4")), "no by-id directory: the kernel's names");
    }

    #[cfg(not(windows))]
    #[test]
    fn a_by_id_name_reads_without_its_directory_and_usb_prefix() {
        assert_eq!(label("/dev/serial/by-id/usb-FTDI_FT232R_USB_UART_AI040UUF-if00-port0"), "FTDI_FT232R_USB_UART_AI040UUF-if00-port0");
        assert_eq!(label("/dev/ttyUSB4"), "/dev/ttyUSB4");
    }
}
