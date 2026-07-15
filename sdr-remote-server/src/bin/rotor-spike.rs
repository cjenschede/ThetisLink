// SPDX-License-Identifier: GPL-2.0-or-later

//! Standalone hardware-test voor het MCP2221A-rotor-printje.
//!
//! Doel (PATCH-yaesu-rotor-mcp2221 fase 2): operator kan met multimeter de
//! BST82-gates / DAC / ADC verifiëren zonder een hele server-restart-
//! cycle. De driver-module (`mcp2221_yaesu_rotor.rs`) wordt via
//! `#[path]` gedeeld met de hoofd-server-binary; geen library-refactor.
//!
//! Gebruik (vanuit workspace-root):
//!
//! ```text
//! cargo run -p sdr-remote-server --bin rotor-spike -- --serial rot_rotor1 --test all
//! ```
//!
//! Sub-tests (`--test`):
//!
//! | Naam | Wat de operator ziet/meet |
//! |------|------------------------|
//! | `gpio` | GP0 (BST82-CW) hi/low + GP1 (BST82-CCW) hi/low met 1 s pauze; DMM op DIN pin 1 resp. pin 2 t.o.v. GND moet ~0 V tonen tijdens HIGH (BST82 trekt naar GND), open tijdens LOW |
//! | `dac` | DAC sweep 0->31->0 in stapjes; DMM op DIN pin 3 t.o.v. GND moet ~0 V -> ~5 V -> ~0 V tonen |
//! | `adc` | continu poll (1 Hz) van GP3 ADC; operator draait rotor handmatig (of legt vaste spanning op pin 4) en ziet raw + omgerekende Yaesu-pin spanning meelopen |
//! | `all` | alle drie achter elkaar |
//!
//! Bij elke transactie wordt een `log::info!` regel geschreven met de
//! commande/uitkomst zodat de server-log + multimeter waarneming
//! gekoppeld kunnen worden.

// Stubs voor de twee server-runtime dependencies van
// `mcp2221_yaesu_rotor.rs` (Rotor-facade + config-persistence). Het
// spike-binary gebruikt alleen `YaesuRotorDriver`, niet
// `RotorInstance`, maar de path-include compileert wél de hele file.
mod rotor {
    use std::sync::{mpsc, Arc, Mutex};

    #[derive(Debug, Clone, Copy)]
    pub enum RotorCmd {
        Stop,
        Cw,
        Ccw,
        GoTo(u16),
    }

    #[derive(Debug, Clone, Default)]
    pub struct RotorStatus {
        pub connected: bool,
        pub rotating: bool,
        pub angle_x10: u16,
        pub target_x10: u16,
    }

    pub struct Rotor;
    impl Rotor {
        pub fn from_handles(
            _tx: mpsc::Sender<RotorCmd>,
            _status: Arc<Mutex<RotorStatus>>,
        ) -> Self {
            Self
        }
        pub fn from_handles_with_max(
            _tx: mpsc::Sender<RotorCmd>,
            _status: Arc<Mutex<RotorStatus>>,
            _max_deg_x10: u16,
        ) -> Self {
            Self
        }
    }
}

mod config {
    pub struct RotorConfigStub {
        pub v_at_0deg: f32,
        pub v_at_max_deg: f32,
        pub max_deg: u16,
        pub ramp_pct_per_sec: f32,
        pub shortest_route_in_overlap: bool,
    }
    pub struct ConfigStub {
        pub rotors: Vec<RotorConfigStub>,
    }
    pub fn modify_config<F: FnOnce(&mut ConfigStub)>(_f: F) {
        // No-op in spike binary; calibration-persistence is alleen voor server.
    }
}

#[path = "../mcp2221_yaesu_rotor.rs"]
mod mcp2221_yaesu_rotor;

use std::env;
use std::process::ExitCode;
use std::thread::sleep;
use std::time::Duration;

use log::{error, info, warn};

use mcp2221_yaesu_rotor::{YaesuRotorDriver, ADC_FULL_SCALE, ADC_VREF_V, DAC_MAX};

fn print_usage() {
    eprintln!(
        "rotor-spike - hardware-test voor MCP2221A Yaesu rotor printje

USAGE:
    rotor-spike --serial <ROT_SERIAL> [--test <gpio|dac|adc|all>] [--no-cleanup]

OPTIES:
    --serial <S>       USB serial van het MCP2221A bord (typisch `rot_<naam>`).
                       Leeg = eerste bord op de bus (alleen handig bij één
                       MCP2221A aangesloten).
    --test <NAAM>      Welke sub-test (`gpio` / `dac` / `adc` / `all`). Default `all`.
    --no-cleanup       Laat GP0/GP1 hoog en DAC ongelijk aan 0 staan na afloop.
                       Default: alles weer netjes idle.

VOORBEELD:
    rotor-spike --serial rot_rotor1 --test gpio
"
    );
}

fn parse_args() -> Option<(Option<String>, String, bool)> {
    let mut serial: Option<String> = None;
    let mut test = "all".to_string();
    let mut cleanup = true;
    let mut args = env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--serial" => serial = args.next(),
            "--test" => {
                if let Some(v) = args.next() {
                    test = v;
                } else {
                    return None;
                }
            }
            "--no-cleanup" => cleanup = false,
            "-h" | "--help" => return None,
            _ => {
                eprintln!("Onbekend argument: {}", a);
                return None;
            }
        }
    }
    Some((serial, test, cleanup))
}

fn test_gpio(driver: &YaesuRotorDriver) {
    info!("[GPIO test] start");
    info!("[GPIO test] GP0 (CW) HIGH - DMM op DIN-pin 1 t.o.v. GND moet ~0 V tonen (BST82 trekt naar GND)");
    if let Err(e) = driver.set_direction(true, false) {
        error!("[GPIO test] GP0 high faalde: {:?}", e);
        return;
    }
    sleep(Duration::from_millis(1500));

    info!("[GPIO test] GP0 LOW - DIN-pin 1 moet open zijn (>1 MΩ naar GND)");
    if let Err(e) = driver.set_direction(false, false) {
        error!("[GPIO test] GP0 low faalde: {:?}", e);
        return;
    }
    sleep(Duration::from_millis(1500));

    info!("[GPIO test] GP1 (CCW) HIGH - DIN-pin 2 moet ~0 V tonen");
    if let Err(e) = driver.set_direction(false, true) {
        error!("[GPIO test] GP1 high faalde: {:?}", e);
        return;
    }
    sleep(Duration::from_millis(1500));

    info!("[GPIO test] GP1 LOW - DIN-pin 2 moet open zijn");
    if let Err(e) = driver.set_direction(false, false) {
        error!("[GPIO test] GP1 low faalde: {:?}", e);
        return;
    }
    info!("[GPIO test] klaar");
}

fn test_dac(driver: &YaesuRotorDriver) {
    info!("[DAC test] start - DMM op DIN-pin 3 t.o.v. GND, verwacht 0..5 V sweep");
    let steps: Vec<u8> = (0..=DAC_MAX).step_by(4).chain(std::iter::once(DAC_MAX)).collect();
    for v in &steps {
        let pred_v = (*v as f32 / DAC_MAX as f32) * 5.0;
        info!("[DAC test] set DAC={:>2}/{} (verwacht ~{:.2} V)", v, DAC_MAX, pred_v);
        if let Err(e) = driver.set_dac(*v) {
            error!("[DAC test] set_dac({}): {:?}", v, e);
            return;
        }
        sleep(Duration::from_millis(800));
    }
    info!("[DAC test] sweep terug naar 0");
    for v in steps.iter().rev().skip(1) {
        info!("[DAC test] set DAC={:>2}/{}", v, DAC_MAX);
        if let Err(e) = driver.set_dac(*v) {
            error!("[DAC test] set_dac({}): {:?}", v, e);
            return;
        }
        sleep(Duration::from_millis(400));
    }
    info!("[DAC test] klaar");
}

fn test_adc(driver: &YaesuRotorDriver) {
    info!("[ADC test] start - 20 polls @ 1 Hz; draai rotor handmatig of leg vaste spanning op DIN-pin 4");
    for i in 1..=20 {
        match driver.read_position_raw() {
            Ok(raw) => {
                let adc_v = (raw as f32) * ADC_VREF_V / (ADC_FULL_SCALE as f32);
                let yaesu_v = YaesuRotorDriver::raw_to_yaesu_volts(raw);
                info!(
                    "[ADC test] #{:>2}: raw={:>4} ({:.3} V op ADC-pin) -> Yaesu pin 4 ≈ {:.3} V",
                    i, raw, adc_v, yaesu_v
                );
            }
            Err(e) => {
                error!("[ADC test] #{:>2}: read faalde: {:?}", i, e);
                sleep(Duration::from_secs(1));
                continue;
            }
        }
        sleep(Duration::from_secs(1));
    }
    info!("[ADC test] klaar");
}

fn cleanup(driver: &YaesuRotorDriver) {
    info!("[cleanup] reset: GP0/GP1 LOW + DAC 0");
    if let Err(e) = driver.set_direction(false, false) {
        warn!("[cleanup] set_direction faalde: {:?}", e);
    }
    if let Err(e) = driver.set_dac(0) {
        warn!("[cleanup] set_dac(0) faalde: {:?}", e);
    }
}

fn main() -> ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format_timestamp_millis()
        .init();

    let (serial, test, do_cleanup) = match parse_args() {
        Some(v) => v,
        None => {
            print_usage();
            return ExitCode::from(2);
        }
    };
    if serial.is_none() {
        warn!("Geen --serial opgegeven; spike opent het eerste MCP2221A bord op de bus.");
        warn!("Dit is alleen verstandig met één bord aangesloten.");
    }

    let driver = YaesuRotorDriver::with_target_serial(serial.clone());
    if let Err(e) = driver.reconnect() {
        error!("Bord openen / initialiseren mislukte: {:?}", e);
        return ExitCode::from(1);
    }

    match test.as_str() {
        "gpio" => test_gpio(&driver),
        "dac" => test_dac(&driver),
        "adc" => test_adc(&driver),
        "all" => {
            test_gpio(&driver);
            test_dac(&driver);
            test_adc(&driver);
        }
        other => {
            error!("Onbekende test \"{}\". Kies gpio / dac / adc / all.", other);
            return ExitCode::from(2);
        }
    }

    if do_cleanup {
        cleanup(&driver);
    } else {
        warn!("--no-cleanup: GP0/GP1 + DAC blijven staan in laatste state.");
    }

    let snap = driver.snapshot();
    info!(
        "Final snapshot: status={:?} CW={} CCW={} DAC={} last_adc={:?}",
        snap.status, snap.gp0_cw_high, snap.gp1_ccw_high, snap.dac_value, snap.last_adc_raw
    );
    ExitCode::SUCCESS
}
