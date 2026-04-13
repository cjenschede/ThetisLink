# ThetisLink Refactoring Plan

## Samenvatting

Doel: de ~5.700 LOC in `ui.rs` terugbrengen naar ~5.000 LOC door geïdentificeerde duplicatie te elimineren, zonder impact op latency, functionaliteit of visueel gedrag.

5 fasen, geordend van laagste naar hoogste risico. Elke fase is zelfstandig deployable.

## Fase 1: App State Structurering (zeer laag risico)

**Doel:** 280+ losse velden in `SdrRemoteApp` groeperen in logische structs.

**LOC reductie:** ~0 netto (structurele verbetering, geen code eliminatie)

### Nieuwe structs:

**SpectrumState** (~22 velden per RX):
```rust
struct SpectrumState {
    bins: Vec<u8>, center_hz: u32, span_hz: u32,
    ref_level: i8, db_per_unit: u8, last_seq: u16,
    full_bins: Vec<u8>, full_center_hz: u32, full_span_hz: u32, full_sequence: u16,
    ref_db: f32, range_db: f32, zoom: f32, pan: f32,
    last_sent_zoom: f32, last_sent_pan: f32, zoom_pan_changed_at: Option<Instant>,
    waterfall: WaterfallRingBuffer, waterfall_contrast: f32,
    auto_ref_enabled: bool, auto_ref_value: f32, auto_ref_frames: u32, auto_ref_initialized: bool,
}
```
Vervangt 42 losse velden (22×RX1 + 20×RX2) door `spectrum_rx1: SpectrumState`, `spectrum_rx2: SpectrumState`.

**VfoState** (~10 velden per VFO):
```rust
struct VfoState {
    frequency_hz: u64, pending_freq: Option<u64>,
    mode: u8, smeter: u16,
    filter_low_hz: i32, filter_high_hz: i32, filter_changed_at: Option<Instant>,
    nr_level: u8, anf_on: bool, freq_step_index: usize,
}
```
Vervangt 20 losse velden door `vfo_a: VfoState`, `vfo_b: VfoState`.

**Per-apparaat structs:** AmplitecState (~5), TunerState (~4), SpeState (~16), Rf2kState (~45), UltraBeamState (~12), RotorState (~6).

**Risico:** Puur mechanisch. Compiler vangt alle missende referenties.

## Fase 2: Band Geheugen Deduplicatie (laag risico)

**Doel:** 4 functies → 2 geparametriseerde functies.

**LOC reductie:** ~50 LOC

### Vfo enum met command mapping:
```rust
enum Vfo { A, B }

impl Vfo {
    fn set_frequency_cmd(&self, hz: u64) -> Command;
    fn set_mode_cmd(&self, mode: u8) -> Command;
    fn filter_low_id(&self) -> ControlId;
    fn filter_high_id(&self) -> ControlId;
    fn nr_id(&self) -> ControlId;
    fn anf_id(&self) -> ControlId;
}
```

`save_current_band(vfo)` en `restore_band(vfo, label, default_freq)` vervangen de 4 losse functies.

## Fase 3: Spectrum Plot Unificatie (medium risico)

**Doel:** `spectrum_plot()` + `rx2_spectrum_plot()` → 1 functie.

**LOC reductie:** ~330 LOC

### Verschillen (slechts 5 configuratiepunten):

| Aspect | RX1 | RX2 |
|--------|-----|-----|
| Scroll/drag/click keys | `"spectrum_*"` | `"rx2_spectrum_*"` |
| Band markers | Ja | Nee |
| `is_popout` parameter | Ja | Altijd true |

```rust
struct SpectrumPlotConfig<'a> {
    scroll_key: &'a str,
    drag_key: &'a str,
    click_key: &'a str,
    show_band_markers: bool,
    is_popout: bool,
}
```

## Fase 4: Spectrum Content Rendering (medium risico)

**Doel:** `render_spectrum_content()` + `render_rx2_spectrum_only()` → 1 functie.

**LOC reductie:** ~120 LOC

Met `SpectrumState` struct uit Fase 1: pass `&mut SpectrumState` als parameter i.p.v. via `&mut self`.

## Fase 5: VFO Controls Unificatie (hoog risico)

**Doel:** `render_rx1_controls()` + `render_rx2_controls_with_split()` → 1 functie.

**LOC reductie:** ~200 LOC

### Subtiele verschillen:
- Freq inline editing: alleen RX1
- Split/VFO Sync knoppen: alleen RX2
- S-meter PTT flags: RX1 heeft ptt/other_tx, RX2 altijd false
- Volume: VFO A vs VFO B commands

```rust
struct VfoConfig {
    vfo: Vfo,
    label: &'static str,
    show_split_button: bool,
    show_vfo_sync: bool,
    is_popout: bool,
}
```

## Totaal Overzicht

| Fase | Beschrijving | LOC Reductie | Risico | Afhankelijkheid |
|------|-------------|-------------|--------|-----------------|
| 1 | App State structs | ~0 | Zeer laag | Geen |
| 2 | Band geheugen parametriseren | ~50 | Laag | Fase 1 |
| 3 | Spectrum plot unificatie | ~330 | Medium | Geen |
| 4 | Spectrum content rendering | ~120 | Medium | Fase 1 + 3 |
| 5 | VFO controls unificatie | ~200 | Hoog | Fase 1 + 2 |
| **Totaal** | | **~700 LOC** | | |

## Aanbevolen Volgorde

```
Fase 1 (state structs) → Fase 3 (spectrum plot) → Fase 2 (band mem) → Fase 4 (spectrum content) → Fase 5 (vfo controls)
```

Fase 3 is onafhankelijk en kan parallel met Fase 1.

## Buiten Scope

- **Latency-paden:** Geen wijziging aan audio, netwerk, jitter buffer of engine
- **Server refactoring:** CatConnection RX1/RX2 duplicatie is minder urgent
- **Protocol:** Geen wijzigingen aan ControlId of pakketformaten
- **Nieuwe features:** Dit plan voegt geen functionaliteit toe
