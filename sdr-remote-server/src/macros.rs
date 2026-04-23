// SPDX-License-Identifier: GPL-2.0-or-later

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use log::{info, warn};

use crate::tuner::{self, Jc4sTuner};

pub const NUM_MACRO_SLOTS: usize = 24;

#[derive(Clone, Debug)]
pub enum MacroAction {
    Cat(String),
    Delay(u32),
    Tune,
}

#[derive(Clone, Debug)]
pub struct MacroDef {
    pub label: String,
    pub actions: Vec<MacroAction>,
}

pub type MacroSlots = [Option<MacroDef>; NUM_MACRO_SLOTS];

pub fn empty_slots() -> MacroSlots {
    std::array::from_fn(|_| None)
}

/// Slot display name (F1..F12, ^F1..^F12)
pub fn slot_name(index: usize) -> String {
    if index < 12 {
        format!("F{}", index + 1)
    } else {
        format!("^F{}", index - 11)
    }
}

/// Format actions as summary string for tooltips
pub fn actions_summary(actions: &[MacroAction]) -> String {
    actions
        .iter()
        .map(|a| match a {
            MacroAction::Cat(cmd) => cmd.clone(),
            MacroAction::Delay(ms) => format!("delay:{}ms", ms),
            MacroAction::Tune => "tune".to_string(),
        })
        .collect::<Vec<_>>()
        .join(" ")
}

// --- Load / Save ---

fn macros_path() -> PathBuf {
    let exe = std::env::current_exe().unwrap_or_default();
    exe.parent()
        .unwrap_or(std::path::Path::new("."))
        .join("thetislink-macros.conf")
}

pub fn load() -> MacroSlots {
    let path = macros_path();
    let mut slots = empty_slots();

    let contents = match fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return slots,
    };

    for line in contents.lines() {
        let line = line.trim();
        if let Some((key, value)) = line.split_once('=') {
            let key = key.trim();
            let value = value.trim();

            // macro_N_label=...
            if let Some(rest) = key.strip_prefix("macro_") {
                if let Some(idx_str) = rest.strip_suffix("_label") {
                    if let Ok(idx) = idx_str.parse::<usize>() {
                        if idx < NUM_MACRO_SLOTS {
                            let slot = slots[idx].get_or_insert_with(|| MacroDef {
                                label: String::new(),
                                actions: Vec::new(),
                            });
                            slot.label = value.to_string();
                        }
                    }
                } else if let Ok(idx) = rest.parse::<usize>() {
                    // macro_N=...
                    if idx < NUM_MACRO_SLOTS {
                        let actions = parse_actions(value);
                        let slot = slots[idx].get_or_insert_with(|| MacroDef {
                            label: String::new(),
                            actions: Vec::new(),
                        });
                        slot.actions = actions;
                    }
                }
            }
        }
    }

    // Remove slots that have no label and no actions
    for slot in slots.iter_mut() {
        if let Some(ref def) = slot {
            if def.label.is_empty() && def.actions.is_empty() {
                *slot = None;
            }
        }
    }

    slots
}

fn parse_actions(s: &str) -> Vec<MacroAction> {
    let mut actions = Vec::new();
    let mut remaining = s.trim();

    while !remaining.is_empty() {
        remaining = remaining.trim_start();
        if remaining.is_empty() {
            break;
        }

        if remaining.eq_ignore_ascii_case("tune") {
            actions.push(MacroAction::Tune);
            remaining = &remaining[4..];
        } else if let Some(rest) = remaining.strip_prefix("delay:") {
            // delay:200
            let end = rest.find(|c: char| c.is_whitespace()).unwrap_or(rest.len());
            if let Ok(ms) = rest[..end].parse::<u32>() {
                actions.push(MacroAction::Delay(ms));
            }
            remaining = &rest[end..];
        } else if let Some(semi_pos) = remaining.find(';') {
            // CAT command: everything up to and including ';'
            let cmd = &remaining[..=semi_pos];
            actions.push(MacroAction::Cat(cmd.to_string()));
            remaining = &remaining[semi_pos + 1..];
        } else {
            // Unknown token — skip to next whitespace
            let end = remaining
                .find(|c: char| c.is_whitespace())
                .unwrap_or(remaining.len());
            remaining = &remaining[end..];
        }
    }

    actions
}

pub fn save(slots: &MacroSlots) {
    let path = macros_path();
    let mut contents = String::new();

    for (i, slot) in slots.iter().enumerate() {
        if let Some(ref def) = slot {
            contents.push_str(&format!("macro_{}_label={}\n", i, def.label));
            let actions_str = def
                .actions
                .iter()
                .map(|a| match a {
                    MacroAction::Cat(cmd) => cmd.clone(),
                    MacroAction::Delay(ms) => format!("delay:{}", ms),
                    MacroAction::Tune => "tune".to_string(),
                })
                .collect::<Vec<_>>()
                .join(" ");
            contents.push_str(&format!("macro_{}={}\n", i, actions_str));
        }
    }

    let _ = fs::write(&path, contents);
}

// --- Macro Runner ---

#[derive(Clone, Debug)]
pub struct MacroRunnerStatus {
    pub running: bool,
    pub current_label: String,
    pub step: usize,
    pub total_steps: usize,
    pub active_slot: usize,
}

impl Default for MacroRunnerStatus {
    fn default() -> Self {
        Self {
            running: false,
            current_label: String::new(),
            step: 0,
            total_steps: 0,
            active_slot: 0,
        }
    }
}

pub struct MacroRunner {
    status: Arc<Mutex<MacroRunnerStatus>>,
    abort: Arc<AtomicBool>,
}

impl MacroRunner {
    pub fn new() -> Self {
        Self {
            status: Arc::new(Mutex::new(MacroRunnerStatus::default())),
            abort: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn status(&self) -> MacroRunnerStatus {
        self.status.lock().unwrap().clone()
    }

    pub fn is_running(&self) -> bool {
        self.status.lock().unwrap().running
    }

    pub fn abort(&self) {
        self.abort.store(true, Ordering::SeqCst);
    }

    pub fn run(
        &self,
        slot_index: usize,
        def: MacroDef,
        cat_tx: tokio::sync::mpsc::Sender<String>,
        tuner: Option<Arc<Jc4sTuner>>,
    ) {
        if self.is_running() {
            return;
        }

        let status = self.status.clone();
        let abort = self.abort.clone();

        // Reset abort flag
        abort.store(false, Ordering::SeqCst);

        // Set running
        {
            let mut s = status.lock().unwrap();
            s.running = true;
            s.current_label = def.label.clone();
            s.step = 0;
            s.total_steps = def.actions.len();
            s.active_slot = slot_index;
        }

        let label = def.label.clone();
        info!("Macro [{}] gestart ({} acties)", label, def.actions.len());

        std::thread::Builder::new()
            .name("macro-runner".to_string())
            .spawn(move || {
                for (i, action) in def.actions.iter().enumerate() {
                    if abort.load(Ordering::SeqCst) {
                        info!("Macro [{}] afgebroken bij stap {}/{}", label, i + 1, def.actions.len());
                        // If aborting during a tune action, also abort the tuner
                        if matches!(action, MacroAction::Tune) {
                            if let Some(ref t) = tuner {
                                t.send_command(tuner::TunerCmd::AbortTune);
                            }
                        }
                        break;
                    }

                    // Update step
                    {
                        let mut s = status.lock().unwrap();
                        s.step = i + 1;
                    }

                    match action {
                        MacroAction::Cat(cmd) => {
                            info!("Macro [{}] stap {}: CAT {}", label, i + 1, cmd);
                            if let Err(e) = cat_tx.blocking_send(cmd.clone()) {
                                warn!("Macro CAT send failed: {}", e);
                                break;
                            }
                        }
                        MacroAction::Delay(ms) => {
                            info!("Macro [{}] stap {}: delay {}ms", label, i + 1, ms);
                            // Sleep in small increments to check abort
                            let total = std::time::Duration::from_millis(*ms as u64);
                            let start = std::time::Instant::now();
                            while start.elapsed() < total {
                                if abort.load(Ordering::SeqCst) {
                                    break;
                                }
                                std::thread::sleep(std::time::Duration::from_millis(50));
                            }
                        }
                        MacroAction::Tune => {
                            if let Some(ref t) = tuner {
                                info!("Macro [{}] stap {}: tune", label, i + 1);
                                t.send_command(tuner::TunerCmd::StartTune);

                                // Wait for tune to complete (max 35s)
                                let timeout = std::time::Duration::from_secs(35);
                                let start = std::time::Instant::now();

                                // Wait for TUNING state first
                                std::thread::sleep(std::time::Duration::from_millis(200));

                                loop {
                                    if abort.load(Ordering::SeqCst) {
                                        t.send_command(tuner::TunerCmd::AbortTune);
                                        break;
                                    }
                                    if start.elapsed() > timeout {
                                        warn!("Macro [{}]: tune timeout", label);
                                        break;
                                    }
                                    let ts = t.status();
                                    if ts.state != tuner::TUNER_TUNING {
                                        // Done (ok, timeout, aborted, or idle)
                                        break;
                                    }
                                    std::thread::sleep(std::time::Duration::from_millis(100));
                                }
                            } else {
                                warn!("Macro [{}]: tune actie maar geen tuner geconfigureerd", label);
                            }
                        }
                    }
                }

                // Done
                info!("Macro [{}] afgerond", label);
                let mut s = status.lock().unwrap();
                s.running = false;
            })
            .ok();
    }
}
