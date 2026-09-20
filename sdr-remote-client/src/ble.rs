// SPDX-License-Identifier: GPL-2.0-or-later
//! The Bluetooth transport for a YPC21 / PTT-Z01 transmit button on the desktop.
//!
//! Only the carrying of bytes lives here. The rule - what a press, a release and
//! a lost link mean for the transmitter - is `sdr_remote_logic::ble_ptt`, shared
//! with the phone, and the button is fed into the PTT decision as one more
//! latching control next to MIDI (`ui/ble_screen.rs`).
//!
//! Windows: WinRT through the `windows` crate the client already depends on, so
//! no new crate enters the workspace or the SBOM. Measured on the owner's button
//! on 2026-09-17 before a line of this was written: Windows hands out the FFE1
//! notifications **without** the button being paired in the OS (`IsPaired`
//! stayed false throughout), the same as on Android. Other platforms get a
//! transport that says so and does nothing.
//!
//! Everything WinRT blocks on (`.get()`) runs on a thread of its own; the UI
//! thread only ever reads a channel.

use std::sync::mpsc;
use std::time::{Duration, Instant};

use sdr_remote_logic::ble_ptt::BleButtonEvent;

/// How long a scan runs before it stops on its own. Found by asking which exit
/// nobody tries (working rule 10): leave the settings screen while it is
/// looking for buttons, and the watcher ran for as long as the client did.
pub const SCAN_TIMEOUT: Duration = Duration::from_secs(60);

/// Has a scan that started at `since` run its course?
pub fn scan_expired(since: Option<Instant>, now: Instant) -> bool {
    since.map(|s| now.duration_since(s) >= SCAN_TIMEOUT).unwrap_or(false)
}

/// FFE0 / FFE1: the Telink serial profile these buttons speak. Documented in the
/// Android patch brief and measured on the owner's exemplar (see the memory on
/// the YPC21): press = `0x01`, release = `0x00`, one byte per notification.
pub const SERVICE_UUID: u128 = 0x0000ffe0_0000_1000_8000_00805f9b34fb;
pub const CHARACTERISTIC_UUID: u128 = 0x0000ffe1_0000_1000_8000_00805f9b34fb;

/// What the transport reports to the UI thread.
#[derive(Debug)]
pub enum BleEvent {
    /// A device seen while scanning. `likely` when its advertisement carries the
    /// PTT service, so it can be listed first.
    Found { addr: u64, name: String, rssi: i16, likely: bool },
    /// A link event for the rule, stamped with the connection attempt it belongs
    /// to - the rule ignores anything from an attempt that has been replaced.
    Link(u64, BleButtonEvent),
    /// The subscription is in place: the button will be heard from now on.
    Attached(u64),
    /// A connection attempt ended without a subscription.
    Failed(u64, String),
    /// Something worth a line in the log and on the status line.
    Note(String),
}

/// Where the transport stands, for the settings screen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BleStatus {
    Off,
    Scanning,
    Connecting,
    Connected,
    /// The link is gone and the platform is watching for the button to come back.
    Waiting,
}

/// A Bluetooth address as it is written in the config and shown on screen.
pub fn format_addr(addr: u64) -> String {
    let b = addr.to_be_bytes();
    format!("{:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}", b[2], b[3], b[4], b[5], b[6], b[7])
}

/// The other way round; `None` for anything that is not six hex bytes.
pub fn parse_addr(s: &str) -> Option<u64> {
    let parts: Vec<&str> = s.trim().split(':').collect();
    if parts.len() != 6 {
        return None;
    }
    let mut v: u64 = 0;
    for p in parts {
        v = (v << 8) | u8::from_str_radix(p, 16).ok()? as u64;
    }
    if v == 0 { None } else { Some(v) }
}

pub struct BleManager {
    pub event_rx: mpsc::Receiver<BleEvent>,
    event_tx: mpsc::Sender<BleEvent>,
    /// One per connection attempt. The rule (`BlePttState`) is told the number
    /// and drops events from any earlier attempt.
    generation: u64,
    status: BleStatus,
    /// When the running scan started, so it can be stopped after `SCAN_TIMEOUT`.
    scan_since: Option<Instant>,
    #[cfg(windows)]
    scan: Option<win::Scan>,
    #[cfg(windows)]
    link: win::LinkSlot,
}

impl BleManager {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            event_rx: rx,
            event_tx: tx,
            generation: 0,
            status: BleStatus::Off,
            scan_since: None,
            #[cfg(windows)]
            scan: None,
            #[cfg(windows)]
            link: win::LinkSlot::new(),
        }
    }

    /// Is there a transport on this platform at all?
    pub fn available() -> bool {
        cfg!(windows)
    }

    pub fn status(&self) -> BleStatus {
        self.status
    }

    pub fn set_status(&mut self, s: BleStatus) {
        self.status = s;
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn scan_since(&self) -> Option<Instant> {
        self.scan_since
    }

    /// Start looking for buttons. A connected button does not advertise, so an
    /// existing link is let go first - the same rule as on the phone.
    pub fn scan_start(&mut self) {
        self.disconnect();
        #[cfg(windows)]
        {
            match win::Scan::start(self.event_tx.clone()) {
                Ok(s) => {
                    self.scan = Some(s);
                    self.status = BleStatus::Scanning;
                    self.scan_since = Some(Instant::now());
                }
                Err(e) => {
                    let _ = self.event_tx.send(BleEvent::Note(format!("scan could not start: {}", e)));
                }
            }
        }
        #[cfg(not(windows))]
        {
            let _ = self.event_tx.send(BleEvent::Note("Bluetooth buttons are not available on this platform".into()));
        }
    }

    pub fn scan_stop(&mut self) {
        self.scan_since = None;
        #[cfg(windows)]
        {
            self.scan = None;
        }
        if self.status == BleStatus::Scanning {
            self.status = BleStatus::Off;
        }
    }

    /// Connect to a button by address. Returns the generation of this attempt;
    /// the caller hands it to `BlePttState::begin`.
    pub fn connect(&mut self, addr: u64, name: &str) -> u64 {
        self.scan_stop();
        self.disconnect();
        self.generation += 1;
        let generation = self.generation;
        self.status = BleStatus::Connecting;
        let _ = self.event_tx.send(BleEvent::Note(format!("connecting to {} ({})", name, format_addr(addr))));
        #[cfg(windows)]
        {
            self.link.begin(generation);
            win::connect(addr, generation, self.event_tx.clone(), self.link.clone());
        }
        #[cfg(not(windows))]
        {
            let _ = addr;
            let _ = self.event_tx.send(BleEvent::Failed(generation, "not available on this platform".into()));
        }
        generation
    }

    /// Let the button go. Whatever it held is the rule's business: the caller
    /// feeds `Disconnected` for this generation, and that releases it.
    pub fn disconnect(&mut self) {
        #[cfg(windows)]
        {
            self.link.close();
        }
        if matches!(self.status, BleStatus::Connecting | BleStatus::Connected | BleStatus::Waiting) {
            self.status = BleStatus::Off;
        }
    }
}

#[cfg(windows)]
mod win {
    //! WinRT: BluetoothLEAdvertisementWatcher for the scan, BluetoothLEDevice +
    //! GattSession with MaintainConnection for the link, and the FFE1
    //! characteristic's ValueChanged for the bytes.

    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{mpsc, Arc, Mutex};

    use log::{info, warn};
    use windows::core::GUID;
    use windows::Devices::Bluetooth::Advertisement::{
        BluetoothLEAdvertisementReceivedEventArgs, BluetoothLEAdvertisementWatcher, BluetoothLEScanningMode,
    };
    use windows::Devices::Bluetooth::GenericAttributeProfile::{
        GattCharacteristic, GattClientCharacteristicConfigurationDescriptorValue, GattCommunicationStatus,
        GattSession, GattValueChangedEventArgs,
    };
    use windows::Devices::Bluetooth::{BluetoothCacheMode, BluetoothConnectionStatus, BluetoothLEDevice};
    use windows::Foundation::{EventRegistrationToken, TypedEventHandler};
    use windows::Storage::Streams::DataReader;

    use super::{BleEvent, BleButtonEvent, CHARACTERISTIC_UUID, SERVICE_UUID};

    const SERVICE: GUID = GUID::from_u128(SERVICE_UUID);
    const CHARACTERISTIC: GUID = GUID::from_u128(CHARACTERISTIC_UUID);

    /// A running advertisement watcher. Dropping it stops the scan and unhooks
    /// the handler, so a scan cannot outlive the screen that started it.
    pub struct Scan {
        watcher: BluetoothLEAdvertisementWatcher,
        token: EventRegistrationToken,
    }

    impl Scan {
        pub fn start(tx: mpsc::Sender<BleEvent>) -> windows::core::Result<Self> {
            let watcher = BluetoothLEAdvertisementWatcher::new()?;
            watcher.SetScanningMode(BluetoothLEScanningMode::Active)?;
            let token = watcher.Received(&TypedEventHandler::new(
                move |_w: &Option<BluetoothLEAdvertisementWatcher>,
                      args: &Option<BluetoothLEAdvertisementReceivedEventArgs>| {
                    if let Some(a) = args {
                        let adv = a.Advertisement()?;
                        let name = adv.LocalName()?.to_string();
                        if name.is_empty() {
                            return Ok(());
                        }
                        let mut likely = false;
                        if let Ok(uuids) = adv.ServiceUuids() {
                            for i in 0..uuids.Size().unwrap_or(0) {
                                if uuids.GetAt(i)? == SERVICE {
                                    likely = true;
                                }
                            }
                        }
                        let _ = tx.send(BleEvent::Found {
                            addr: a.BluetoothAddress()?,
                            name,
                            rssi: a.RawSignalStrengthInDBm()?,
                            likely,
                        });
                    }
                    Ok(())
                },
            ))?;
            watcher.Start()?;
            Ok(Self { watcher, token })
        }
    }

    impl Drop for Scan {
        fn drop(&mut self) {
            let _ = self.watcher.RemoveReceived(self.token);
            let _ = self.watcher.Stop();
        }
    }

    /// One live link: the device, the session that keeps Windows reconnecting,
    /// and the characteristic with its handlers. Dropping it unhooks everything
    /// and closes the device.
    ///
    /// The characteristic is replaced on every return of the link: Windows
    /// closes the old object when the link drops ("The object has been closed",
    /// 0x80000013, measured 2026-09-17 10:44), so writing the CCCD on it again
    /// is not enough - service and characteristic have to be looked up afresh.
    pub struct Link {
        device: BluetoothLEDevice,
        _session: GattSession,
        characteristic: GattCharacteristic,
        value_token: EventRegistrationToken,
        status_token: EventRegistrationToken,
    }

    impl Drop for Link {
        fn drop(&mut self) {
            let _ = self.characteristic.RemoveValueChanged(self.value_token);
            let _ = self.device.RemoveConnectionStatusChanged(self.status_token);
            let _ = self.device.Close();
        }
    }

    /// Where the worker thread puts the link once it stands, and the number of
    /// the attempt that is current. A worker that finishes after its attempt was
    /// replaced (the operator pressed "forget" while it was still connecting)
    /// finds a newer number here and drops what it built instead of storing it.
    #[derive(Clone)]
    pub struct LinkSlot {
        link: Arc<Mutex<Option<Link>>>,
        current: Arc<AtomicU64>,
    }

    impl LinkSlot {
        pub fn new() -> Self {
            Self { link: Arc::new(Mutex::new(None)), current: Arc::new(AtomicU64::new(0)) }
        }
        pub fn begin(&self, generation: u64) {
            self.current.store(generation, Ordering::SeqCst);
        }
        pub fn close(&self) {
            self.current.store(u64::MAX, Ordering::SeqCst);
            if let Ok(mut g) = self.link.lock() {
                *g = None;
            }
        }
        fn store(&self, generation: u64, link: Link) -> bool {
            if self.current.load(Ordering::SeqCst) != generation {
                return false;
            }
            if let Ok(mut g) = self.link.lock() {
                *g = Some(link);
                return true;
            }
            false
        }
        /// A fresh characteristic after the link came back. The old one is
        /// unhooked; if the attempt is no longer current, or the link is not
        /// there yet, the new one is unhooked instead and nothing changes.
        fn replace_characteristic(&self, generation: u64, ch: GattCharacteristic, token: EventRegistrationToken) -> bool {
            let mut swapped = false;
            if self.current.load(Ordering::SeqCst) == generation {
                if let Ok(mut g) = self.link.lock() {
                    if let Some(link) = g.as_mut() {
                        let _ = link.characteristic.RemoveValueChanged(link.value_token);
                        link.characteristic = ch.clone();
                        link.value_token = token;
                        swapped = true;
                    }
                }
            }
            if !swapped {
                let _ = ch.RemoveValueChanged(token);
            }
            swapped
        }
    }

    /// The whole connect on a thread of its own: address -> device -> session ->
    /// service -> characteristic -> subscribe. Every step that can fail reports
    /// as `Failed`, never silently.
    pub fn connect(addr: u64, generation: u64, tx: mpsc::Sender<BleEvent>, slot: LinkSlot) {
        std::thread::Builder::new()
            .name("ble-ptt-connect".into())
            .spawn(move || {
                match build_link(addr, generation, tx.clone(), slot.clone()) {
                    Ok(link) => {
                        if slot.store(generation, link) {
                            let _ = tx.send(BleEvent::Attached(generation));
                            let _ = tx.send(BleEvent::Link(generation, BleButtonEvent::Connected));
                        } else {
                            info!("BLE PTT: attempt {} finished after it was replaced - let go", generation);
                        }
                    }
                    Err(e) => {
                        let _ = tx.send(BleEvent::Failed(generation, e));
                    }
                }
            })
            .ok();
    }

    fn build_link(addr: u64, generation: u64, tx: mpsc::Sender<BleEvent>, slot: LinkSlot) -> Result<Link, String> {
        let device = BluetoothLEDevice::FromBluetoothAddressAsync(addr)
            .and_then(|op| op.get())
            .map_err(|e| format!("device lookup: {}", e))?;
        // A device Windows has never seen comes back as a null object, which
        // the binding surfaces as an error on the next call. Say so plainly.
        let id = device.BluetoothDeviceId().map_err(|_| "Windows does not know this address (press the button and try again)".to_string())?;
        let session = GattSession::FromDeviceIdAsync(&id)
            .and_then(|op| op.get())
            .map_err(|e| format!("session: {}", e))?;
        // This is the reconnect: with it set, Windows keeps a link open while the
        // button is in range and re-establishes it when it comes back.
        session.SetMaintainConnection(true).map_err(|e| format!("maintain connection: {}", e))?;

        let (characteristic, value_token) = attach(&device, generation, tx.clone())?;

        // The link coming and going. On a return, service and characteristic are
        // looked up again (the old objects are closed) and the subscription is
        // written again (the button is not bonded and keeps nothing) - on a
        // thread, because the handler runs on a WinRT thread that must not block.
        let stx = tx.clone();
        let device_for_return = device.clone();
        let slot_for_return = slot.clone();
        let status_token = device
            .ConnectionStatusChanged(&TypedEventHandler::new(move |d: &Option<BluetoothLEDevice>, _| {
                if let Some(d) = d {
                    match d.ConnectionStatus()? {
                        BluetoothConnectionStatus::Disconnected => {
                            let _ = stx.send(BleEvent::Link(generation, BleButtonEvent::Disconnected));
                        }
                        _ => {
                            let dev = device_for_return.clone();
                            let rtx = stx.clone();
                            let rslot = slot_for_return.clone();
                            std::thread::spawn(move || match attach(&dev, generation, rtx.clone()) {
                                Ok((ch, token)) => {
                                    if rslot.replace_characteristic(generation, ch, token) {
                                        let _ = rtx.send(BleEvent::Note("button back in reach, subscribed again".into()));
                                        let _ = rtx.send(BleEvent::Attached(generation));
                                        let _ = rtx.send(BleEvent::Link(generation, BleButtonEvent::Connected));
                                    } else {
                                        info!("BLE PTT: back in reach for attempt {} that is no longer current - let go", generation);
                                    }
                                }
                                Err(e) => {
                                    warn!("BLE PTT: back in reach but not subscribed again: {}", e);
                                    let _ = rtx.send(BleEvent::Note(format!("back in reach, subscribing failed: {}", e)));
                                }
                            });
                        }
                    }
                }
                Ok(())
            }))
            .map_err(|e| format!("status handler: {}", e))?;

        Ok(Link { device, _session: session, characteristic, value_token, status_token })
    }

    /// Service -> characteristic -> ValueChanged -> CCCD, uncached, so a return
    /// of the link gets live objects and not the closed ones from before.
    fn attach(
        device: &BluetoothLEDevice,
        generation: u64,
        tx: mpsc::Sender<BleEvent>,
    ) -> Result<(GattCharacteristic, EventRegistrationToken), String> {
        let services = device
            .GetGattServicesForUuidWithCacheModeAsync(SERVICE, BluetoothCacheMode::Uncached)
            .and_then(|op| op.get())
            .map_err(|e| format!("services: {}", e))?;
        if services.Status().map_err(|e| e.to_string())? != GattCommunicationStatus::Success {
            return Err("no PTT service (FFE0) on this device".into());
        }
        let service = services
            .Services()
            .and_then(|v| v.GetAt(0))
            .map_err(|_| "no PTT service (FFE0) on this device".to_string())?;
        let chars = service
            .GetCharacteristicsForUuidWithCacheModeAsync(CHARACTERISTIC, BluetoothCacheMode::Uncached)
            .and_then(|op| op.get())
            .map_err(|e| format!("characteristics: {}", e))?;
        let characteristic = chars
            .Characteristics()
            .and_then(|v| v.GetAt(0))
            .map_err(|_| "PTT service has nothing to listen to (no FFE1)".to_string())?;

        // The bytes.
        let btx = tx;
        let value_token = characteristic
            .ValueChanged(&TypedEventHandler::new(
                move |_c: &Option<GattCharacteristic>, args: &Option<GattValueChangedEventArgs>| {
                    if let Some(a) = args {
                        let reader = DataReader::FromBuffer(&a.CharacteristicValue()?)?;
                        let n = reader.UnconsumedBufferLength()?;
                        for _ in 0..n {
                            let _ = btx.send(BleEvent::Link(generation, BleButtonEvent::Notification(reader.ReadByte()?)));
                        }
                    }
                    Ok(())
                },
            ))
            .map_err(|e| format!("subscribe: {}", e))?;

        if let Err(e) = subscribe(&characteristic) {
            let _ = characteristic.RemoveValueChanged(value_token);
            return Err(e);
        }
        Ok((characteristic, value_token))
    }

    /// Ask the button to notify. This is the write that, on the measurement of
    /// 2026-09-17, went through without any pairing.
    fn subscribe(ch: &GattCharacteristic) -> Result<(), String> {
        let status = ch
            .WriteClientCharacteristicConfigurationDescriptorAsync(GattClientCharacteristicConfigurationDescriptorValue::Notify)
            .and_then(|op| op.get())
            .map_err(|e| format!("CCCD write: {}", e))?;
        if status != GattCommunicationStatus::Success {
            return Err(format!("CCCD write refused ({:?})", status));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_scan_stops_on_its_own_after_a_minute() {
        let t0 = Instant::now();
        assert!(!scan_expired(None, t0), "no scan, nothing to expire");
        assert!(!scan_expired(Some(t0), t0 + Duration::from_secs(59)));
        assert!(scan_expired(Some(t0), t0 + SCAN_TIMEOUT));
        assert_eq!(SCAN_TIMEOUT, Duration::from_secs(60), "written out, not expressed in itself");
    }

    #[test]
    fn an_address_survives_the_round_trip() {
        let a = 0xA4C1_38F1_3C82u64;
        assert_eq!(format_addr(a), "A4:C1:38:F1:3C:82");
        assert_eq!(parse_addr("A4:C1:38:F1:3C:82"), Some(a));
        assert_eq!(parse_addr("a4:c1:38:f1:3c:82"), Some(a), "case does not matter");
        assert_eq!(parse_addr(""), None);
        assert_eq!(parse_addr("00:00:00:00:00:00"), None, "zero is 'no button'");
        assert_eq!(parse_addr("A4:C1:38"), None);
        assert_eq!(parse_addr("A4:C1:38:F1:3C:ZZ"), None);
    }
}
