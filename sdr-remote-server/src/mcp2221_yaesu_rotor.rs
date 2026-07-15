// SPDX-License-Identifier: GPL-2.0-or-later

//! MCP2221A-based driver voor de Yaesu G-1000DXC rotor (direct
//! aansturen via een Adafruit MCP2221A breakout op 5 V, zonder
//! tussenkomst van een EA7HG-PCB).
//!
//! Hardware-mapping (zie memory `project_yaesu_rotor_mcp2221_plan`):
//!
//! | MCP2221A pin | Functie | DIN-7 pin Yaesu | Toelichting |
//! |--------------|---------|-----------------|-------------|
//! | GP0 (GPIO out) | gate BST82 (CW) | drain -> pin 1 (R/CW) | low = idle, high = CW actief |
//! | GP1 (GPIO out) | gate BST82 (CCW) | drain -> pin 2 (L/CCW) | low = idle, high = CCW actief |
//! | GP2 (DAC) | snelheid | pin 3 (Speed) | DAC-Vref = Vdd -> 0-5 V, 5-bit (32 stappen) |
//! | GP3 (ADC) | positie-feedback | pin 4 (Position) via 1,8k+2,2k deler | ADC-Vref = intern 4,096 V, 10-bit (max meet ≈ 7,45 V) |
//! | GND | gemeenschappelijke massa | pin 5 (Signal GND) | |
//!
//! Belangrijk: de breakout moet eerst op 5 V gejumperd worden (3V-pad
//! doorgeknipt, 5V-pad gebrugd op de onderkant van het Adafruit-bord);
//! anders schakelt de BST82 niet hard genoeg en is de DAC-range maar
//! 0-3,3 V.
//!
//! Deze module bevat alleen low-level primitives + een initialisatie-
//! routine die de SRAM-settings van het bord op de juiste mode zet.
//! Server-integratie (RotorBackend, EquipmentCommand-handlers, polling-
//! thread) volgt in fase 3 van PATCH-yaesu-rotor-mcp2221.

use anyhow::{anyhow, Context, Result};
use log::{debug, info, warn};
use std::collections::VecDeque;
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use crate::rotor::{Rotor, RotorCmd, RotorStatus};

use mcp2221_hal::analog::{VoltageReference, VrmVoltage};
use mcp2221_hal::gpio::{GpioChanges, GpioDirection, LogicLevel};
use mcp2221_hal::settings::{Gp0Mode, Gp1Mode, Gp2Mode, Gp3Mode};
use mcp2221_hal::MCP2221;

/// DAC-uitgang op 5 bits (32 stappen). Met DAC-Vref = Vdd (5 V) levert
/// dat ~156 mV per stap. Voor de Yaesu G-1000DXC speed-ingang is dat
/// grof maar voldoende - de ingang is hoog-ohmig (~108 k) en bedoeld
/// voor continu-variabele DC.
pub const DAC_MAX: u8 = 31;

/// 10-bit ADC full-scale.
pub const ADC_FULL_SCALE: u16 = 1023;

/// ADC-referentie spanning (intern Vrm 4,096 V). Niet Vdd, want de
/// Adafruit USB-rail varieert en de Yaesu positie-uitgang is
/// ratiometrisch t.o.v. zijn eigen interne ~4,54 V referentie. De
/// stabielere interne 4,096 V geeft reproduceerbare kalibratie.
pub const ADC_VREF_V: f32 = 4.096;

/// Positie-spanningsdeler op het printje. Operator heeft hem na een eerste
/// build (1,8 k + 10 k -> ratio 1,18, max meting 4,83 V) verkrapt naar
/// 1,8 k + 2,2 k -> ratio (1800+2200)/2200 = 1,818, max meting
/// 4,096 × 1,818 ≈ 7,45 V. Reden: de Yaesu G-1000DXC kan boven 4,8 V
/// uitkomen op pin 4 voor de hoogste graden, waardoor met de eerste
/// ratio de ADC clipte boven ~365°. Met 2,2 k onder valt 0-5 V pin 4
/// netjes binnen ADC-range met ruime marge. Operator moet na deze
/// hardware-aanpassing opnieuw "Park CCW" + "Park CW" drukken want de
/// opgeslagen v_at_0deg / v_at_max_deg waardes hangen aan de oude
/// ratio en kloppen anders niet meer met de fysieke positie.
pub const POSITION_DIVIDER_RATIO: f32 = (1_800.0 + 2_200.0) / 2_200.0;

/// Driver-status - gespiegeld aan het `Mcp2221Debug` tuner-pad.
#[derive(Debug, Clone)]
pub enum Status {
    /// Nog geen poging gedaan om te verbinden.
    NotInitialized,
    /// Bord open + pin-modes geconfigureerd.
    Connected,
    /// Open faalde of een latere call gaf een error. String bevat de reden.
    Error(String),
}

/// Aantal samples in de moving-average buffer voor ADC-filtering.
/// Bij 5 Hz poll = 2 sec window. Lang genoeg om 50/60 Hz ripple en
/// USB-jitter uit te middelen, kort genoeg om rotor-beweging tijdens
/// run vlot te volgen (~0,2°/sample × 10 = 2° na full settle).
pub const ADC_AVG_WINDOW: usize = 10;

/// Driver-snapshot voor UI- en log-doeleinden.
#[derive(Debug, Clone)]
pub struct RotorSnapshot {
    pub status: Status,
    /// Laatst commanded GP0 niveau (CW): true = actief.
    pub gp0_cw_high: bool,
    /// Laatst commanded GP1 niveau (CCW): true = actief.
    pub gp1_ccw_high: bool,
    /// Laatst commanded DAC waarde (0..=DAC_MAX).
    pub dac_value: u8,
    /// Laatst gelezen ADC raw counts (0..=ADC_FULL_SCALE), `None` tot
    /// de eerste succesvolle poll.
    pub last_adc_raw: Option<u16>,
    /// Aantal samples nu in de moving-average buffer (0..=ADC_AVG_WINDOW).
    pub samples_in_window: usize,
    /// Gemiddelde ADC-counts over de moving-average buffer.
    /// Wordt nog gebruikt voor de log-diagnose; UI toont `median_adc_raw`
    /// als primaire (stabielere) waarde.
    pub avg_adc_raw: Option<f32>,
    /// Mediaan ADC-counts over de buffer. Robuust tegen ruis-spikes;
    /// UI toont deze als primaire positie-waarde.
    pub median_adc_raw: Option<u16>,
    /// Min/max ADC-counts in de buffer (voor ruis-diagnose). Beide `None`
    /// tot de eerste sample. Gebruik (max - min) als ruwe peak-to-peak
    /// spread.
    pub min_adc_raw: Option<u16>,
    pub max_adc_raw: Option<u16>,
}

struct Inner {
    device: Option<MCP2221>,
    status: Status,
    /// USB serial van het te openen bord (`rot_<naam>`). `None` valt
    /// terug op "eerste board op de bus" - alleen handig voor het
    /// standalone test-spike binary.
    target_serial: Option<String>,
    gp0_cw_high: bool,
    gp1_ccw_high: bool,
    dac_value: u8,
    last_adc_raw: Option<u16>,
    /// Ringbuffer van laatste ADC-samples voor moving-average filtering.
    /// Eerder kreeg de UI elke 200 ms een ruwe sample en operator zag tot
    /// ~500 mV schommeling op een DC-feedback signaal; deze buffer
    /// middelt dat uit naar een stabielere display-waarde en geeft
    /// tegelijk min/max-spread voor ruis-diagnose.
    adc_samples: VecDeque<u16>,
    /// Huidige capaciteit van de moving-average buffer. Wordt door de
    /// poll-thread aangepast op basis van motion/idle state: tijdens
    /// beweging krimpt het venster naar ~1 sec (30 samples), bij
    /// stilstand groeit het naar ~60 sec (60 samples).
    adc_buffer_cap: usize,
}

impl Inner {
    fn new(target_serial: Option<String>) -> Self {
        Self {
            device: None,
            status: Status::NotInitialized,
            target_serial,
            gp0_cw_high: false,
            gp1_ccw_high: false,
            dac_value: 0,
            last_adc_raw: None,
            adc_samples: VecDeque::with_capacity(ADC_AVG_WINDOW),
            adc_buffer_cap: ADC_AVG_WINDOW,
        }
    }

    /// (Re)open het bord en configureer pin-modes + analog-references.
    fn try_connect(&mut self) -> Result<()> {
        let dev = match &self.target_serial {
            Some(sn) if !sn.is_empty() => MCP2221::connect_with_serial(sn)
                .map_err(|e| anyhow!("connect_with_serial({}): {:?}", sn, e))?,
            _ => MCP2221::connect().map_err(|e| anyhow!("connect: {:?}", e))?,
        };

        // GP0/GP1 = GPIO out (low = idle), GP2 = DAC, GP3 = ADC.
        let (_chip, mut gp) = dev
            .sram_read_settings()
            .map_err(|e| anyhow!("sram_read_settings: {:?}", e))?;
        gp.gp0_mode = Gp0Mode::Gpio;
        gp.gp0_direction = GpioDirection::Output;
        gp.gp0_value = LogicLevel::Low;
        gp.gp1_mode = Gp1Mode::Gpio;
        gp.gp1_direction = GpioDirection::Output;
        gp.gp1_value = LogicLevel::Low;
        gp.gp2_mode = Gp2Mode::AnalogOutput; // DAC1
        gp.gp3_mode = Gp3Mode::AnalogInput; // ADC3
        dev.sram_write_gp_settings(gp)
            .map_err(|e| anyhow!("sram_write_gp_settings: {:?}", e))?;

        // DAC-Vref = Vdd (5 V), ADC-Vref = intern 4,096 V.
        dev.analog_set_output_reference(VoltageReference::Vdd)
            .map_err(|e| anyhow!("analog_set_output_reference(Vdd): {:?}", e))?;
        dev.analog_set_input_reference(VoltageReference::Vrm(VrmVoltage::V4_096))
            .map_err(|e| anyhow!("analog_set_input_reference(Vrm 4.096): {:?}", e))?;

        // DAC start op 0 (geen speed).
        dev.analog_write(0)
            .map_err(|e| anyhow!("analog_write(0): {:?}", e))?;

        self.device = Some(dev);
        self.status = Status::Connected;
        self.gp0_cw_high = false;
        self.gp1_ccw_high = false;
        self.dac_value = 0;
        info!(
            "YaesuRotor: bord verbonden + geconfigureerd (target={:?})",
            self.target_serial
        );
        Ok(())
    }
}

/// Driver-instance. Houdt de USB-handle vast en serialiseert alle calls.
pub struct YaesuRotorDriver {
    inner: Mutex<Inner>,
}

impl YaesuRotorDriver {
    /// Maak een nieuwe driver-instance gebonden aan een specifiek MCP2221A
    /// USB serial (typisch `rot_<naam>` per ThetisLink-conventie). `None`
    /// valt terug op "eerste bord op de bus" - alleen sinnig voor losse
    /// hardware-tests; productie moet altijd via serial.
    pub fn with_target_serial(target_serial: Option<String>) -> Self {
        Self {
            inner: Mutex::new(Inner::new(target_serial)),
        }
    }

    /// Forceer een (re)connect-poging. Standaardgebruik: bij init en
    /// na een USB-fout.
    pub fn reconnect(&self) -> Result<()> {
        let mut g = self.inner.lock().expect("YaesuRotorDriver mutex poisoned");
        g.device = None;
        g.try_connect().context("reconnect")
    }

    /// Snapshot van de huidige driver-state (zonder USB-call).
    /// Berekent zowel mean (voor log-diagnose) als median (voor UI display)
    /// over de moving-average buffer. Median is robuust tegen ruis-spikes
    /// die de mean wegtrekken - operator-bevinding 2026-06-04: EMI-pickup
    /// veroorzaakt spreads van ~150 raw counts ongeacht rotor-positie,
    /// een mediaan over 10 samples filtert de uitschieters effectief uit
    /// terwijl rotor-bewegingen wel direct meeloeren.
    pub fn snapshot(&self) -> RotorSnapshot {
        let g = self.inner.lock().expect("YaesuRotorDriver mutex poisoned");
        let samples_in_window = g.adc_samples.len();
        let (avg_adc_raw, median_adc_raw, min_adc_raw, max_adc_raw) =
            if g.adc_samples.is_empty() {
                (None, None, None, None)
            } else {
                let sum: u32 = g.adc_samples.iter().map(|&v| v as u32).sum();
                let avg = sum as f32 / samples_in_window as f32;
                let min = g.adc_samples.iter().copied().min();
                let max = g.adc_samples.iter().copied().max();
                let mut sorted: Vec<u16> = g.adc_samples.iter().copied().collect();
                sorted.sort_unstable();
                let median = sorted[sorted.len() / 2];
                (Some(avg), Some(median), min, max)
            };
        RotorSnapshot {
            status: g.status.clone(),
            gp0_cw_high: g.gp0_cw_high,
            gp1_ccw_high: g.gp1_ccw_high,
            dac_value: g.dac_value,
            last_adc_raw: g.last_adc_raw,
            samples_in_window,
            avg_adc_raw,
            median_adc_raw,
            min_adc_raw,
            max_adc_raw,
        }
    }

    /// Schrijf GP0 (CW) + GP1 (CCW) in één HID-transactie.
    ///
    /// Belangrijk: drukt nooit beide tegelijk hoog (de Yaesu-ingangen
    /// zijn elektrisch niet beschermd tegen gelijktijdige R+L). Bij
    /// `cw=true, ccw=true` wordt CCW automatisch op false gezet en
    /// een waarschuwing gelogd.
    pub fn set_direction(&self, cw: bool, ccw: bool) -> Result<()> {
        let (cw, ccw) = if cw && ccw {
            warn!("YaesuRotor: CW+CCW tegelijk gevraagd; CCW geforceerd uit");
            (true, false)
        } else {
            (cw, ccw)
        };

        let mut g = self.inner.lock().expect("YaesuRotorDriver mutex poisoned");
        if g.device.is_none() {
            g.try_connect().context("auto-reconnect in set_direction")?;
        }
        let dev = g
            .device
            .as_ref()
            .ok_or_else(|| anyhow!("device niet beschikbaar"))?;

        let mut changes = GpioChanges::new();
        changes.with_gp0_level(if cw { LogicLevel::High } else { LogicLevel::Low });
        changes.with_gp1_level(if ccw { LogicLevel::High } else { LogicLevel::Low });
        dev.gpio_write(&changes).map_err(|e| {
            let msg = format!("gpio_write CW={} CCW={}: {:?}", cw, ccw, e);
            g.device = None;
            g.status = Status::Error(msg.clone());
            anyhow!(msg)
        })?;
        g.gp0_cw_high = cw;
        g.gp1_ccw_high = ccw;
        g.status = Status::Connected;
        // Op debug-niveau: tijdens GoTo wordt set_direction elke tick
        // (~30 Hz) opnieuw aangeroepen om CW/CCW vast te houden; een
        // info-log per tick gaf onleesbare spam in de server-log.
        debug!("YaesuRotor: GP0(CW)={} GP1(CCW)={}", cw, ccw);
        Ok(())
    }

    /// Stel de DAC-waarde in (0..=`DAC_MAX`). Hogere waarde = snellere
    /// rotor (Yaesu-ingang is hoog-ohmig en accepteert 0-5 V continu).
    pub fn set_dac(&self, value: u8) -> Result<()> {
        let value = value.min(DAC_MAX);
        let mut g = self.inner.lock().expect("YaesuRotorDriver mutex poisoned");
        if g.device.is_none() {
            g.try_connect().context("auto-reconnect in set_dac")?;
        }
        let dev = g
            .device
            .as_ref()
            .ok_or_else(|| anyhow!("device niet beschikbaar"))?;
        dev.analog_write(value).map_err(|e| {
            let msg = format!("analog_write({}): {:?}", value, e);
            g.device = None;
            g.status = Status::Error(msg.clone());
            anyhow!(msg)
        })?;
        g.dac_value = value;
        debug!(
            "YaesuRotor: DAC={} ({}/{} = {:.2} V @ Vdd≈5V)",
            value,
            value,
            DAC_MAX,
            (value as f32 / DAC_MAX as f32) * 5.0
        );
        Ok(())
    }

    /// Lees de ADC op GP3 (positie-feedback) en geef raw counts terug.
    /// Geen rate-limit hier - de caller (poll-thread of spike-binary)
    /// bepaalt zelf de cadans.
    pub fn read_position_raw(&self) -> Result<u16> {
        let mut g = self.inner.lock().expect("YaesuRotorDriver mutex poisoned");
        if g.device.is_none() {
            g.try_connect().context("auto-reconnect in read_position_raw")?;
        }
        let dev = g
            .device
            .as_ref()
            .ok_or_else(|| anyhow!("device niet beschikbaar"))?;
        let reading = dev.analog_read().map_err(|e| {
            let msg = format!("analog_read: {:?}", e);
            g.device = None;
            g.status = Status::Error(msg.clone());
            anyhow!(msg)
        })?;
        let raw = reading
            .gp3
            .ok_or_else(|| anyhow!("GP3 niet als ADC geconfigureerd"))?;
        g.last_adc_raw = Some(raw);
        // Push naar moving-average ringbuffer; drop oudste sample(s)
        // tot we onder de huidige `adc_buffer_cap` zitten. Cap kan
        // dynamisch krimpen (motion->idle transitie zet cap kleiner)
        // dus eventueel meerdere oude samples in één call droppen.
        let cap = g.adc_buffer_cap.max(1);
        while g.adc_samples.len() >= cap {
            g.adc_samples.pop_front();
        }
        g.adc_samples.push_back(raw);
        Ok(raw)
    }

    /// Pas de moving-average buffer-grootte aan (motion vs idle).
    /// Bij krimpen worden oudste samples gedropt; bij groeien blijven
    /// bestaande samples staan en wordt de buffer langzaam gevuld.
    pub fn set_buffer_cap(&self, cap: usize) {
        let cap = cap.max(1);
        let mut g = self.inner.lock().expect("YaesuRotorDriver mutex poisoned");
        g.adc_buffer_cap = cap;
        while g.adc_samples.len() > cap {
            g.adc_samples.pop_front();
        }
    }

    /// Wis alle samples uit de moving-average buffer. Gebruikt bij
    /// idle->motion transitie zodat oude 1-Hz samples niet de display
    /// vervuilen tijdens een actieve rotatie.
    pub fn clear_samples(&self) {
        let mut g = self.inner.lock().expect("YaesuRotorDriver mutex poisoned");
        g.adc_samples.clear();
    }

    /// Reken raw ADC-counts om naar de Yaesu position-pin spanning,
    /// d.w.z. ongedaan-maken van de printje-spanningsdeler.
    pub fn raw_to_yaesu_volts(raw: u16) -> f32 {
        let adc_v = (raw as f32) * ADC_VREF_V / (ADC_FULL_SCALE as f32);
        adc_v * POSITION_DIVIDER_RATIO
    }
}

// ── Server-side runtime: RotorInstance + poll-thread ─────────────────────

/// Poll-interval tijdens actieve beweging (gate aan of GoTo target).
/// 33 ms ≈ 30 Hz; bewust **geen** veelvoud van 50/60 Hz zodat samples
/// niet syncen met de 50/100 Hz netvoedingsripple die op de Yaesu
/// position-uitgang lekt. Window = 30 samples = 1 sec gemiddelde.
const ADC_POLL_INTERVAL_MOTION_MS: u64 = 33;

/// Poll-interval bij stilstand (gate uit, geen target actief). 1 Hz +
/// 60-sample buffer = 60-sec moving average, voor zeer rustige UI-
/// display. Bij overgang naar beweging wordt de buffer in-flight
/// teruggebracht naar de motion-cap.
const ADC_POLL_INTERVAL_IDLE_MS: u64 = 1000;

/// Buffer-grootte tijdens beweging. Bewust klein gehouden (10 samples
/// × 33 ms ≈ 333 ms venster, mediaan-lag ~165 ms) zodat de control-
/// loop de werkelijke rotor-positie snel genoeg ziet om op tijd de
/// soft-stop in te zetten. Bij 30 samples liep de gemeten positie
/// ~1,5° achter (bij 3°/s) en kwam de decel-trigger te laat -
/// rotor stopte abrupt op target i.p.v. lineair.
const ADC_AVG_WINDOW_MOTION: usize = 10;

/// Buffer-grootte bij stilstand (60 samples × 1000 ms = 60 sec).
const ADC_AVG_WINDOW_IDLE: usize = 60;

/// Hoeveel graden tussen huidige positie en target moet er nog
/// over zijn voordat we de beweging stoppen (anti-overshoot deadband).
/// Conservatief op 1°; Yaesu's eigen mechanische slop is ~1° dus
/// software kan niet preciezer.
const GOTO_DEADBAND_DEG: f32 = 1.0;

/// Geschatte rotorsnelheid bij max DAC (deg/sec). Gebruikt om de
/// decel-distance voor soft-stop te schatten zodat de rotor op
/// min-DAC exact op target uitkomt. G-1000DXC met DAC ≈ 5 V haalt
/// circa 6°/s; eerder ingestelde 3°/s gaf te korte decel-zone.
const MAX_ROTOR_DEG_PER_SEC: f32 = 6.0;

/// Veiligheidsfactor op de berekende decel-distance. Werkelijke
/// rotorsnelheid varieert (mast-belasting, voeding-droop) en de
/// mediaan-filter geeft sowieso een kleine meet-lag - beter ruim op
/// tijd beginnen met ramp-down en de laatste graden op DAC=0 uit
/// laten lopen dan een abrupte stop op target.
const DECEL_SAFETY_FACTOR: f32 = 2.2;

/// Live-snapshot voor UI: bevat zowel commande-state als laatst-
/// gemeten ADC-positie.
#[derive(Debug, Clone)]
pub struct RotorInstanceStatus {
    pub status: Status,
    /// Display-label (`name`-veld uit RotorConfig, of mcp_serial als
    /// name leeg is). UI gebruikt dit voor de paneel-titel.
    pub label: String,
    pub gp0_cw_high: bool,
    pub gp1_ccw_high: bool,
    pub dac_value: u8,
    /// Laatst gelezen raw ADC counts (0..=1023), `None` tot eerste
    /// succesvolle poll.
    pub last_adc_raw: Option<u16>,
    /// Idem, omgerekend naar de Yaesu position-pin spanning (post
    /// printje-spanningsdeler).
    pub last_yaesu_volts: Option<f32>,
    /// Aantal samples nu in de moving-average buffer.
    pub samples_in_window: usize,
    /// Gemiddelde ADC-counts over de buffer (alleen voor log-diagnose).
    pub avg_adc_raw: Option<f32>,
    /// Mediaan ADC-counts (primair voor UI; robuust tegen ruis-spikes).
    pub median_adc_raw: Option<u16>,
    /// Yaesu-pin spanning afgeleid van `median_adc_raw`. UI toont dit
    /// als primaire spanningswaarde - geen drift door uitschieters.
    pub median_yaesu_volts: Option<f32>,
    /// Peak-to-peak spread van de samples in het venster (max − min,
    /// in raw counts). 0 = perfect stabiel; hoog = ruis op de bron.
    pub adc_p2p_raw: Option<u16>,
    /// Huidige kalibratie-state (eindpunten + max_deg). UI rendert
    /// de Park-knoppen + max_deg-spinner op basis hiervan.
    pub calibration: RotorCalibration,
    /// Berekende positie in graden via lineaire mapping van
    /// `median_yaesu_volts` over de kalibratie-eindpunten. `None`
    /// als kalibratie ongeldig (eindpunten gelijk) of nog geen sample.
    pub position_deg: Option<f32>,
}

/// Kalibratie-state per rotor: lineaire mapping van Yaesu pin-4
/// spanning naar graden. Aangepast door UI-knoppen "Park CCW"/"Park CW"
/// + max_deg-spinner; gepersisteerd via `config::modify_config`.
#[derive(Debug, Clone, Copy)]
pub struct RotorCalibration {
    pub v_at_0deg: f32,
    pub v_at_max_deg: f32,
    pub max_deg: u16,
    /// Ramp-rate voor soft-start/stop (% per seconde).
    pub ramp_pct_per_sec: f32,
    /// Bij rotors met overlap (max_deg > 360): kies bij GoTo de
    /// kortste route via de overlap-zone. Zie `RotorConfig`-doc.
    pub shortest_route_in_overlap: bool,
}

impl Default for RotorCalibration {
    fn default() -> Self {
        Self {
            v_at_0deg: 0.0,
            v_at_max_deg: 4.5,
            max_deg: 450,
            ramp_pct_per_sec: 50.0,
            shortest_route_in_overlap: false,
        }
    }
}

impl RotorCalibration {
    /// Reken pin-4 spanning om naar rotor-positie in graden via
    /// lineaire mapping. Returns `None` als kalibratie nog ongeldig
    /// is (eindpunten te dicht bij elkaar / inverted).
    pub fn volts_to_degrees(&self, v: f32) -> Option<f32> {
        let span = self.v_at_max_deg - self.v_at_0deg;
        if span.abs() < 0.01 {
            // Eindpunten gelijk -> niet kalibreerd, geen mapping mogelijk.
            return None;
        }
        let frac = (v - self.v_at_0deg) / span;
        Some(frac * self.max_deg as f32)
    }
}

/// Server-side wrapper rond `YaesuRotorDriver` met achtergrond-poll-
/// thread die de ADC continu uitleest, zodat de UI een live positie-
/// display kan tonen zonder zelf USB-calls te doen. Tevens een
/// `Rotor`-facade producent zodat het bestaande client-rotor-window
/// (positie-display, CW/CCW/Stop/GoTo) automatisch werkt zonder
/// client-side wijzigingen.
pub struct RotorInstance {
    driver: Arc<YaesuRotorDriver>,
    label: String,
    /// Slot-index in `config.rotors` zodat `set_calibration_*` weet
    /// welke entry te muteren. Voor MAX_ROTORS=1 nu altijd 0.
    slot_index: usize,
    /// Kalibratie-state in geheugen - gespiegeld met `config.rotors[slot]`
    /// kalibratie-velden. UI muteert hier (Park-knoppen), poll-thread
    /// leest hier (voor GoTo-control). `Arc<Mutex<>>` zodat beide
    /// dezelfde gedeelde state delen.
    calibration: Arc<Mutex<RotorCalibration>>,
    /// Cmd-channel voor de `Rotor`-facade (client -> server commands).
    /// Poll-thread handelt elke tick één commando af.
    cmd_tx: mpsc::Sender<RotorCmd>,
    /// Live `RotorStatus` - gedeeld met de `Rotor`-facade. Poll-thread
    /// schrijft hier elke ADC-poll de actuele angle/connected/rotating.
    rotor_status: Arc<Mutex<RotorStatus>>,
    /// Shutdown-flag voor de poll-thread. Geen explicit JoinHandle -
    /// rotor-instance leeft normaal de hele server-runtime.
    shutdown: Arc<std::sync::atomic::AtomicBool>,
}

impl RotorInstance {
    /// Open het bord (by USB serial) en spawn de ADC-poll-thread.
    /// Faalt zacht: als het bord initieel niet open kan, draait de poll-
    /// thread alsnog en zal periodiek pogen te reconnecten. UI ziet
    /// `Status::Error(..)` tot het bord beschikbaar is.
    pub fn new(
        slot_index: usize,
        serial: &str,
        label: &str,
        calibration: RotorCalibration,
    ) -> Arc<Self> {
        let target = if serial.is_empty() {
            None
        } else {
            Some(serial.to_string())
        };
        let driver = Arc::new(YaesuRotorDriver::with_target_serial(target));
        // Initial reconnect attempt: log result maar block niet de start.
        if let Err(e) = driver.reconnect() {
            warn!(
                "RotorInstance {}: init reconnect mislukt ({:?}); poll-thread retried periodically",
                label, e
            );
        }
        let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (cmd_tx, cmd_rx) = mpsc::channel::<RotorCmd>();
        let rotor_status = Arc::new(Mutex::new(RotorStatus::default()));
        let calibration_arc = Arc::new(Mutex::new(calibration));
        let inst = Arc::new(Self {
            driver: driver.clone(),
            label: label.to_string(),
            slot_index,
            calibration: calibration_arc.clone(),
            cmd_tx,
            rotor_status: rotor_status.clone(),
            shutdown: shutdown.clone(),
        });

        // Spawn poll-thread: leest elke ADC_POLL_INTERVAL_MS de ADC,
        // updatet driver-internal `last_adc_raw`, schrijft de actuele
        // angle naar `rotor_status`, en handelt incoming RotorCmd's af
        // (manual Cw/Ccw/Stop + bang-bang GoTo control-loop).
        let label_for_thread = label.to_string();
        std::thread::Builder::new()
            .name(format!("rotor-poll-{}", label))
            .spawn(move || {
                rotor_poll_thread(
                    driver,
                    shutdown,
                    label_for_thread,
                    cmd_rx,
                    rotor_status,
                    calibration_arc,
                );
            })
            .ok();
        info!("RotorInstance {}: started", label);
        inst
    }

    /// Geef een `Rotor`-facade terug die het bestaande client-rotor-
    /// protocol (CW/CCW/Stop/GoTo + RotorStatus poll) ondersteunt.
    /// De caller kan dezelfde Rotor in `ServerState.rotor` plaatsen
    /// als anders een EA7HG/PstRotator-instance.
    pub fn make_rotor_facade(&self) -> Rotor {
        // Geef de werkelijke max-rotatie door via de facade zodat externe
        // input-bronnen (PstRotator-listener) bij overlap-rotors de
        // mech-target+360° als alternatief kunnen overwegen.
        let max_deg = self
            .calibration
            .lock()
            .map(|c| c.max_deg)
            .unwrap_or(450);
        let max_deg_x10 = (max_deg as u16).saturating_mul(10);
        Rotor::from_handles_with_max(
            self.cmd_tx.clone(),
            self.rotor_status.clone(),
            max_deg_x10,
        )
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    /// Live snapshot van driver + label voor UI-rendering.
    pub fn status(&self) -> RotorInstanceStatus {
        let snap = self.driver.snapshot();
        let last_yaesu_volts = snap
            .last_adc_raw
            .map(|raw| YaesuRotorDriver::raw_to_yaesu_volts(raw));
        let median_yaesu_volts = snap
            .median_adc_raw
            .map(|raw| YaesuRotorDriver::raw_to_yaesu_volts(raw));
        let adc_p2p_raw = match (snap.min_adc_raw, snap.max_adc_raw) {
            (Some(lo), Some(hi)) => Some(hi.saturating_sub(lo)),
            _ => None,
        };
        let calibration = *self.calibration.lock().expect("rotor cal mutex poisoned");
        let position_deg =
            median_yaesu_volts.and_then(|v| calibration.volts_to_degrees(v));
        RotorInstanceStatus {
            status: snap.status,
            label: self.label.clone(),
            gp0_cw_high: snap.gp0_cw_high,
            gp1_ccw_high: snap.gp1_ccw_high,
            dac_value: snap.dac_value,
            last_adc_raw: snap.last_adc_raw,
            last_yaesu_volts,
            samples_in_window: snap.samples_in_window,
            avg_adc_raw: snap.avg_adc_raw,
            median_adc_raw: snap.median_adc_raw,
            median_yaesu_volts,
            adc_p2p_raw,
            calibration,
            position_deg,
        }
    }

    /// Bewaar de huidige mediaan-spanning als het 0°-eindpunt
    /// (CCW-park). Persisteert via `config::modify_config`.
    pub fn park_ccw(&self) {
        let snap = self.driver.snapshot();
        let v = match snap.median_adc_raw {
            Some(raw) => YaesuRotorDriver::raw_to_yaesu_volts(raw),
            None => {
                warn!("RotorInstance {}: park_ccw zonder ADC-sample", self.label);
                return;
            }
        };
        let mut cal = self.calibration.lock().expect("rotor cal mutex poisoned");
        cal.v_at_0deg = v;
        let cal_copy = *cal;
        drop(cal);
        self.persist_calibration(cal_copy);
        info!(
            "RotorInstance {}: park CCW gezet op {:.3} V (raw median {})",
            self.label,
            v,
            snap.median_adc_raw.unwrap()
        );
    }

    /// Bewaar de huidige mediaan-spanning als het max°-eindpunt
    /// (CW-park).
    pub fn park_cw(&self) {
        let snap = self.driver.snapshot();
        let v = match snap.median_adc_raw {
            Some(raw) => YaesuRotorDriver::raw_to_yaesu_volts(raw),
            None => {
                warn!("RotorInstance {}: park_cw zonder ADC-sample", self.label);
                return;
            }
        };
        let mut cal = self.calibration.lock().expect("rotor cal mutex poisoned");
        cal.v_at_max_deg = v;
        let cal_copy = *cal;
        drop(cal);
        self.persist_calibration(cal_copy);
        info!(
            "RotorInstance {}: park CW gezet op {:.3} V (raw median {})",
            self.label,
            v,
            snap.median_adc_raw.unwrap()
        );
    }

    /// Update `max_deg` (UI-spinner). Standaard 450 voor G-1000DXC.
    pub fn set_max_deg(&self, max_deg: u16) {
        let max_deg = max_deg.clamp(90, 720);
        let mut cal = self.calibration.lock().expect("rotor cal mutex poisoned");
        cal.max_deg = max_deg;
        let cal_copy = *cal;
        drop(cal);
        self.persist_calibration(cal_copy);
        info!("RotorInstance {}: max_deg gezet op {}°", self.label, max_deg);
    }

    /// Toggle de "kortste route via overlap-zone" optie. Persisted.
    pub fn set_shortest_route_in_overlap(&self, on: bool) {
        let mut cal = self.calibration.lock().expect("rotor cal mutex poisoned");
        cal.shortest_route_in_overlap = on;
        let cal_copy = *cal;
        drop(cal);
        self.persist_calibration(cal_copy);
        info!(
            "RotorInstance {}: shortest_route_in_overlap = {}",
            self.label, on
        );
    }

    /// Update soft-start/stop ramp-rate (% per seconde). Persisted.
    pub fn set_ramp_pct_per_sec(&self, pct: f32) {
        let pct = pct.clamp(1.0, 200.0);
        let mut cal = self.calibration.lock().expect("rotor cal mutex poisoned");
        cal.ramp_pct_per_sec = pct;
        let cal_copy = *cal;
        drop(cal);
        self.persist_calibration(cal_copy);
        info!("RotorInstance {}: ramp_pct_per_sec gezet op {:.1}", self.label, pct);
    }

    fn persist_calibration(&self, cal: RotorCalibration) {
        let slot = self.slot_index;
        crate::config::modify_config(|c| {
            if let Some(r) = c.rotors.get_mut(slot) {
                r.v_at_0deg = cal.v_at_0deg;
                r.v_at_max_deg = cal.v_at_max_deg;
                r.max_deg = cal.max_deg;
                r.ramp_pct_per_sec = cal.ramp_pct_per_sec;
                r.shortest_route_in_overlap = cal.shortest_route_in_overlap;
            }
        });
    }

    /// CW/CCW commande - delegeert direct naar de driver (USB-call op
    /// de aanroepende thread, zelfde patroon als Mcp2221Debug). Best
    /// vanuit een actie-handler (button click) - niet vanuit een
    /// hoge-frequentie render-loop.
    pub fn set_direction(&self, cw: bool, ccw: bool) {
        if let Err(e) = self.driver.set_direction(cw, ccw) {
            warn!("RotorInstance {}: set_direction faalde: {:?}", self.label, e);
        }
    }

    /// DAC-snelheid commande.
    pub fn set_dac(&self, value: u8) {
        if let Err(e) = self.driver.set_dac(value) {
            warn!("RotorInstance {}: set_dac({}) faalde: {:?}", self.label, value, e);
        }
    }
}

impl Drop for RotorInstance {
    fn drop(&mut self) {
        self.shutdown
            .store(true, std::sync::atomic::Ordering::Relaxed);
        // Best-effort cleanup: GP0/1 LOW + DAC 0 zodat een hot-swap
        // niet de rotor in een actieve state achterlaat.
        let _ = self.driver.set_direction(false, false);
        let _ = self.driver.set_dac(0);
        info!("RotorInstance {}: dropped (shutdown signaled)", self.label);
    }
}

fn rotor_poll_thread(
    driver: Arc<YaesuRotorDriver>,
    shutdown: Arc<std::sync::atomic::AtomicBool>,
    label: String,
    cmd_rx: mpsc::Receiver<RotorCmd>,
    rotor_status: Arc<Mutex<RotorStatus>>,
    calibration: Arc<Mutex<RotorCalibration>>,
) {
    let mut consecutive_errors = 0usize;
    let mut last_stat_log = Instant::now();
    let stat_log_interval = Duration::from_secs(5);
    // Actieve GoTo target, in graden. `None` betekent geen automatische
    // beweging - manual cmd's (Cw/Ccw) of stilstand.
    let mut target_deg: Option<f32> = None;
    // Ramp-state: huidige DAC-output (float voor smoothness) + waar we
    // naartoe ramp-en. Bij gate-on of GoTo -> dac_target = DAC_MAX; bij
    // landing of stop -> dac_target = 0. Per tick interpoleert
    // `current_dac` met `ramp_pct_per_sec`.
    let mut current_dac: f32 = 0.0;
    let mut dac_target: f32 = 0.0;
    // Vorige tick-tijdstip voor de exacte ramp-step berekening (echte
    // verstreken tijd, niet de nominale tick - anders verliezen we
    // ramp-snelheid als het OS de slaap-tijd oprekt).
    let mut last_tick = Instant::now();
    // Vorige motion-state voor edge-detection: bij idle->motion wissen
    // we de buffer (vers beginnen op 30 Hz), bij motion->idle laten we
    // de samples staan en groeit de buffer langzaam naar 60.
    let mut was_in_motion = false;
    // Manual-mode: zodra een externe bron (server-UI test-knoppen of
    // speed-slider) de DAC verandert i.p.v. de ramp-loop, geeft de
    // poll-thread de DAC-controle af zodat de slider-waarde blijft
    // staan. Wordt opgeheven zodra een cmd-channel command binnenkomt
    // (Cw/Ccw/Stop/GoTo via client). Detectie via `last_written_dac`:
    // als de actuele driver-DAC niet meer matcht met wat we het laatst
    // schreven, heeft iemand anders ingegrepen.
    let mut manual_mode = false;
    let mut last_written_dac: Option<u8> = None;
    // Initieel staat de cap op ADC_AVG_WINDOW (10); we forceren bij de
    // eerste tick de juiste motion/idle cap.
    driver.set_buffer_cap(ADC_AVG_WINDOW_IDLE);
    info!(
        "rotor-poll {}: thread started (motion={} ms / idle={} ms tick)",
        label, ADC_POLL_INTERVAL_MOTION_MS, ADC_POLL_INTERVAL_IDLE_MS
    );
    loop {
        if shutdown.load(std::sync::atomic::Ordering::Relaxed) {
            break;
        }
        match driver.read_position_raw() {
            Ok(_raw) => {
                consecutive_errors = 0;
            }
            Err(e) => {
                consecutive_errors += 1;
                // Eerste error gedetailleerd loggen, daarna stil tot
                // het weer goed gaat (anders 5 Hz log-spam).
                if consecutive_errors == 1 {
                    warn!("rotor-poll {}: read_position_raw faalde: {:?}", label, e);
                }
                // Iedere 50 polls (10 sec) een retry-reconnect-poging.
                if consecutive_errors % 50 == 0 {
                    if let Err(e2) = driver.reconnect() {
                        debug!("rotor-poll {}: reconnect retry faalde: {:?}", label, e2);
                    } else {
                        info!("rotor-poll {}: reconnected na {} fouten", label, consecutive_errors);
                        consecutive_errors = 0;
                    }
                }
            }
        }
        // Live RotorStatus update: bereken huidige graden uit de
        // mediaan-spanning + actieve kalibratie en publiceer in de
        // gedeelde `rotor_status` Arc zodat de client-rotor-window
        // het ziet zonder eigen USB-call.
        let cal_snap = *calibration.lock().expect("rotor cal mutex poisoned");
        let drv_snap = driver.snapshot();
        let median_v = drv_snap
            .median_adc_raw
            .map(|raw| YaesuRotorDriver::raw_to_yaesu_volts(raw));
        let current_deg = median_v.and_then(|v| cal_snap.volts_to_degrees(v));
        {
            let mut st = rotor_status.lock().expect("rotor status mutex poisoned");
            st.connected = matches!(drv_snap.status, Status::Connected);
            // angle_x10 verwacht 0..3600 voor 0..360°. Yaesu G-1000DXC
            // gaat tot 450°; we clampen naar 0..max_deg en multiplyceren.
            // Mocht de client-UI strict 0..360 zijn, ziet die nu een
            // bredere range - geen schade, en owners die de extra 90°
            // willen tonen hebben er voordeel van.
            if let Some(d) = current_deg {
                let clamped = d.clamp(0.0, cal_snap.max_deg as f32);
                st.angle_x10 = (clamped * 10.0).round() as u16;
            }
            st.rotating = drv_snap.gp0_cw_high || drv_snap.gp1_ccw_high;
            // target_x10 is alleen gevuld wanneer een GoTo actief is.
            st.target_x10 = target_deg
                .map(|t| (t.clamp(0.0, cal_snap.max_deg as f32) * 10.0).round() as u16)
                .unwrap_or(0);
        }

        // Pull pending rotor commands (manual + GoTo). Niet-blocking.
        // Manual cmd's zetten dac_target = max -> soft-start ramp omhoog.
        // Stop zet dac_target = 0 + gate uit (geen ramp-down nodig bij
        // expliciete stop). GoTo zet target_deg en laat de control-loop
        // erna ramp + landing afhandelen.
        match cmd_rx.try_recv() {
            Ok(RotorCmd::Stop) => {
                target_deg = None;
                dac_target = 0.0;
                current_dac = 0.0;
                manual_mode = false;
                if let Err(e) = driver.set_direction(false, false) {
                    warn!("rotor-poll {}: Stop set_direction faalde: {:?}", label, e);
                }
                let _ = driver.set_dac(0);
                last_written_dac = Some(0);
                info!("rotor-poll {}: Stop", label);
            }
            Ok(RotorCmd::Cw) => {
                target_deg = None;
                dac_target = DAC_MAX as f32;
                manual_mode = false;
                if let Err(e) = driver.set_direction(true, false) {
                    warn!("rotor-poll {}: Cw set_direction faalde: {:?}", label, e);
                }
                info!("rotor-poll {}: manual CW (ramp to DAC {})", label, DAC_MAX);
            }
            Ok(RotorCmd::Ccw) => {
                target_deg = None;
                dac_target = DAC_MAX as f32;
                manual_mode = false;
                if let Err(e) = driver.set_direction(false, true) {
                    warn!("rotor-poll {}: Ccw set_direction faalde: {:?}", label, e);
                }
                info!("rotor-poll {}: manual CCW (ramp to DAC {})", label, DAC_MAX);
            }
            Ok(RotorCmd::GoTo(x10)) => {
                manual_mode = false;
                let t = (x10 as f32) / 10.0;
                let primary = t.clamp(0.0, cal_snap.max_deg as f32);
                // Kortste-route optie: bij rotors met overlap-zone
                // (max_deg > 360) kan een alternatief target = primary
                // ± 360° fysiek dezelfde antenne-richting opleveren,
                // maar via een kortere mechanische route. Operator-keuze
                // 2026-06-04: per-rotor checkbox, default uit.
                let chosen = if cal_snap.shortest_route_in_overlap
                    && cal_snap.max_deg > 360
                {
                    let cur = current_deg.unwrap_or(primary);
                    let max_d = cal_snap.max_deg as f32;
                    let mut candidates: Vec<f32> = vec![primary];
                    // Alternatief +360 (geldig als <= max_deg)
                    if primary + 360.0 <= max_d {
                        candidates.push(primary + 360.0);
                    }
                    // Alternatief -360 (geldig als >= 0)
                    if primary - 360.0 >= 0.0 {
                        candidates.push(primary - 360.0);
                    }
                    let best = candidates
                        .into_iter()
                        .min_by(|a, b| {
                            (a - cur)
                                .abs()
                                .partial_cmp(&(b - cur).abs())
                                .unwrap_or(std::cmp::Ordering::Equal)
                        })
                        .unwrap_or(primary);
                    if (best - primary).abs() > 0.01 {
                        info!(
                            "rotor-poll {}: shortest-route gekozen {:.1}° (i.p.v. {:.1}°, huidig {:.1}°)",
                            label, best, primary, cur
                        );
                    }
                    best
                } else {
                    primary
                };
                target_deg = Some(chosen);
                dac_target = DAC_MAX as f32;
                info!("rotor-poll {}: GoTo target {:.1}°", label, chosen);
            }
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) => {
                info!("rotor-poll {}: cmd-channel closed, blijft draaien voor poll", label);
            }
        }

        // GoTo control-loop met soft-stop landing. Bereken hoeveel
        // graden de rotor nog nodig heeft om bij target uit te komen
        // én hoeveel graden hij tijdens een ramp-down zou aflegen op
        // basis van de huidige DAC + ramp_pct_per_sec. Als afstand-tot-
        // target ≤ decel-distance -> start ramp-down (dac_target=0). Bij
        // |Δ| ≤ deadband -> gate uit, target wissen.
        if let (Some(target), Some(current)) = (target_deg, current_deg) {
            let delta = target - current;
            if delta.abs() <= GOTO_DEADBAND_DEG {
                if let Err(e) = driver.set_direction(false, false) {
                    warn!("rotor-poll {}: target-reached stop faalde: {:?}", label, e);
                }
                dac_target = 0.0;
                current_dac = 0.0;
                let _ = driver.set_dac(0);
                info!(
                    "rotor-poll {}: GoTo target bereikt ({:.1}° vs {:.1}°, Δ={:.2})",
                    label, current, target, delta
                );
                target_deg = None;
            } else {
                // Richting-omkering bescherming: een lopende GoTo die
                // tijdens beweging een nieuwe target krijgt aan de
                // andere kant (delta-teken flipt) moest voorheen direct
                // de CW/CCW gates wisselen terwijl current_dac nog op
                // MAX stond - abrupte motor-reversal op vol vermogen.
                // Detecteer hier of de gewenste richting verschilt van
                // de huidige gate-stand; als ja én current_dac > 0,
                // houd huidige richting vast en ramp eerst DAC naar 0.
                // Pas wanneer DAC ~0 is wisselen we de gates en starten
                // de ramp-up in de nieuwe richting.
                let desired_cw = delta > 0.0;
                let direction_mismatch =
                    (desired_cw && drv_snap.gp1_ccw_high)
                        || (!desired_cw && drv_snap.gp0_cw_high);
                if direction_mismatch && current_dac > 1.0 {
                    // Soft-stop fase: gates blijven staan zoals ze
                    // waren, dac_target naar 0. Volgende tick (of paar
                    // ticks afhankelijk van ramp) komt current_dac
                    // onder de drempel en valt de else-tak in met de
                    // juiste gates.
                    dac_target = 0.0;
                } else {
                    // Veilig om richting te zetten: ofwel geen
                    // mismatch (rotor staat al de goede kant op),
                    // ofwel DAC is al ~0.
                    let _ = driver.set_direction(desired_cw, !desired_cw);
                    // Decel-distance: bij ramp_pct_per_sec gaat current_dac
                    // van huidige fractie naar 0 in (frac × 100 / pct) sec.
                    // Gemiddelde snelheid tijdens ramp = MAX × frac / 2.
                    // Distance = avg_speed × time = MAX × frac² × 50 / pct.
                    // Plus `DECEL_SAFETY_FACTOR` om meet-lag + speed-onzekerheid
                    // te dekken - beter te vroeg afremmen dan te laat.
                    let frac = current_dac / DAC_MAX as f32;
                    let decel_time_sec =
                        frac * 100.0 / cal_snap.ramp_pct_per_sec.max(1.0);
                    let decel_dist = MAX_ROTOR_DEG_PER_SEC
                        * frac
                        * 0.5
                        * decel_time_sec
                        * DECEL_SAFETY_FACTOR;
                    if delta.abs() <= decel_dist.max(GOTO_DEADBAND_DEG) {
                        // Begin ramp-down naar 0 zodat we op min-DAC
                        // binnen de deadband eindigen.
                        dac_target = 0.0;
                    } else {
                        dac_target = DAC_MAX as f32;
                    }
                }
            }
        }

        // Detecteer manual override: als de actuele driver-DAC niet
        // matcht met wat we het laatst hebben geschreven, heeft iemand
        // anders ingegrepen (server-UI speed-slider of test-knoppen).
        // We respecteren die waarde en geven de DAC-controle af tot
        // een nieuwe client-cmd binnenkomt.
        let actual_dac = drv_snap.dac_value;
        if let Some(last) = last_written_dac {
            if actual_dac != last && !manual_mode {
                debug!(
                    "rotor-poll {}: external DAC override ({} -> {}), entering manual mode",
                    label, last, actual_dac
                );
                manual_mode = true;
                current_dac = actual_dac as f32;
                dac_target = actual_dac as f32;
                // Lopende GoTo afbreken - gebruiker heeft de controle
                // overgenomen. Doelhoek wissen zodat de ramp-loop hem
                // niet later weer probeert te bereiken.
                target_deg = None;
            }
        }

        let now = Instant::now();
        let dt_sec = (now - last_tick).as_secs_f32();
        last_tick = now;

        if !manual_mode {
            // Normale ramp: interpoleer `current_dac` richting
            // `dac_target` met `ramp_pct_per_sec`. Echte verstreken tijd
            // sinds vorige tick (OS-jitter compensatie).
            let dac_step =
                cal_snap.ramp_pct_per_sec * 0.01 * DAC_MAX as f32 * dt_sec;
            if (dac_target - current_dac).abs() <= dac_step {
                current_dac = dac_target;
            } else if dac_target > current_dac {
                current_dac += dac_step;
            } else {
                current_dac -= dac_step;
            }
            current_dac = current_dac.clamp(0.0, DAC_MAX as f32);
            let new_dac = current_dac.round() as u8;
            // Alleen schrijven als de waarde feitelijk veranderd is -
            // anders blijven we tijdens stilstand elke tick een no-op
            // USB-call doen én lopen we het risico een handmatige DAC
            // tussen ticks per ongeluk te overschrijven.
            if last_written_dac.map(|p| p != new_dac).unwrap_or(true) {
                let _ = driver.set_dac(new_dac);
                last_written_dac = Some(new_dac);
            }
        } else {
            // Manual mode: houd `current_dac` synchroon met de echte
            // DAC zodat een volgende cmd-channel command vanaf de
            // huidige stand doorrampt i.p.v. vanaf 0.
            current_dac = actual_dac as f32;
            last_written_dac = Some(actual_dac);
        }

        // Motion-state bepalen *na* command-handling + ramp-step:
        // rotor is "in motion" als een gate aanstaat OF de DAC niet
        // op 0 is OF een GoTo target actief is. Bij transitie naar
        // motion: buffer cap krimpt naar 30 en buffer wordt gewist
        // zodat het 30-Hz venster vers begint. Bij transitie naar
        // idle: cap groeit naar 60; bestaande samples blijven staan.
        let in_motion = drv_snap.gp0_cw_high
            || drv_snap.gp1_ccw_high
            || target_deg.is_some()
            || current_dac > 0.5
            || dac_target > 0.5;
        if in_motion != was_in_motion {
            if in_motion {
                driver.set_buffer_cap(ADC_AVG_WINDOW_MOTION);
                driver.clear_samples();
                info!(
                    "rotor-poll {}: -> MOTION (cap={}, tick={} ms, buffer cleared)",
                    label, ADC_AVG_WINDOW_MOTION, ADC_POLL_INTERVAL_MOTION_MS
                );
            } else {
                driver.set_buffer_cap(ADC_AVG_WINDOW_IDLE);
                info!(
                    "rotor-poll {}: -> IDLE (cap={}, tick={} ms)",
                    label, ADC_AVG_WINDOW_IDLE, ADC_POLL_INTERVAL_IDLE_MS
                );
            }
            was_in_motion = in_motion;
        }

        // Periodieke ADC-statistiek (raw-counts min/max/mean/median +
        // omgerekende pin4-spanningen). Werd tijdens hardware-tuning
        // gebruikt om ruisniveau te diagnosticeren; nu de hardware
        // stabiel is staat dit op `debug!` zodat het niet langer de
        // server-log vervuilt maar wel beschikbaar blijft via
        // `RUST_LOG=sdr_remote_server::mcp2221_yaesu_rotor=debug`.
        if last_stat_log.elapsed() >= stat_log_interval {
            let snap = driver.snapshot();
            if let (Some(avg), Some(median), Some(min), Some(max)) = (
                snap.avg_adc_raw,
                snap.median_adc_raw,
                snap.min_adc_raw,
                snap.max_adc_raw,
            ) {
                let to_v = |raw_f: f32| {
                    raw_f * ADC_VREF_V / (ADC_FULL_SCALE as f32) * POSITION_DIVIDER_RATIO
                };
                debug!(
                    "rotor-poll {} ADC[{}]: raw min={} max={} mean={:.1} median={} (Δ={}) ; pin4 V min={:.3} max={:.3} median={:.3} (Δ={:.3} V)",
                    label,
                    snap.samples_in_window,
                    min,
                    max,
                    avg,
                    median,
                    max.saturating_sub(min),
                    to_v(min as f32),
                    to_v(max as f32),
                    to_v(median as f32),
                    to_v(max as f32) - to_v(min as f32),
                );
            }
            last_stat_log = Instant::now();
        }
        let sleep_ms = if was_in_motion {
            ADC_POLL_INTERVAL_MOTION_MS
        } else {
            ADC_POLL_INTERVAL_IDLE_MS
        };
        std::thread::sleep(Duration::from_millis(sleep_ms));
    }
    info!("rotor-poll {}: thread stopped", label);
}
