// SPDX-License-Identifier: GPL-2.0-or-later

//! ThetisLink UI design-system: gedeelde kleur-, font- en spacing-constanten
//! plus widget-helpers zodat windows identiek zijn DOOR CONSTRUCTIE i.p.v. per
//! window handmatig nageschilderd. Eén bron van waarheid voor alle popout-
//! en tab-UI's (RX1/RX2/VRX/Yaesu/rotor).
//!
//! Waarden komen uit de UI-consistency audit (exacte parity-spec). Verander hier,
//! niet per call-site.

use egui::Color32;

// ── Thema-varianten (stap 1: egui-basisvisuals per thema) ───────────────────

/// Selectable UI theme. Step 1 switches only the egui base visuals (panel/window
/// fills, widget fills, default text); the many colours that already read from
/// `ui.visuals()` follow automatically. Hard-coded `Color32` call-sites are migrated
/// to a shared palette in later steps. Classic reproduces the original light look.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum ThemeVariant {
    Classic,
    Dark,
    Slate,
    Custom,
}

impl ThemeVariant {
    pub(crate) const ALL: [ThemeVariant; 4] = [
        ThemeVariant::Classic,
        ThemeVariant::Dark,
        ThemeVariant::Slate,
        ThemeVariant::Custom,
    ];

    pub(crate) fn label(self) -> &'static str {
        match self {
            ThemeVariant::Classic => "Classic (light)",
            ThemeVariant::Dark => "Dark",
            ThemeVariant::Slate => "Slate",
            ThemeVariant::Custom => "Custom",
        }
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            ThemeVariant::Classic => "classic",
            ThemeVariant::Dark => "dark",
            ThemeVariant::Slate => "slate",
            ThemeVariant::Custom => "custom",
        }
    }

    pub(crate) fn from_str(s: &str) -> ThemeVariant {
        match s.trim().to_ascii_lowercase().as_str() {
            "dark" => ThemeVariant::Dark,
            "slate" => ThemeVariant::Slate,
            "custom" => ThemeVariant::Custom,
            _ => ThemeVariant::Classic,
        }
    }
}

/// User-editable base colours. Step-1 scope: the three slots that fully drive the egui
/// base visuals (so a Custom theme applies everywhere that reads `ui.visuals()`). Accent
/// and the per-element hard-coded colours are added as more slots during the migration.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Palette {
    pub background: Color32,
    pub widget: Color32,
    pub text: Color32,
    /// Slider knob + rail colour. Drives `widgets.*.bg_fill`, which egui 0.29 uses for the
    /// slider handle/rail but NOT for buttons/combos (those use `weak_bg_fill`), so this
    /// tints sliders independently of the general widget colour.
    pub accent: Color32,
}

impl Palette {
    /// The "Slate" preset (blue-grey dark) - also the starting point for a fresh Custom.
    pub(crate) fn slate() -> Self {
        Self {
            background: Color32::from_rgb(28, 32, 42),
            widget: Color32::from_rgb(52, 60, 76),
            text: Color32::from_rgb(210, 216, 228),
            accent: Color32::from_rgb(86, 132, 204),
        }
    }

    /// Serialise to one conf value: `bg,widget,text,accent` (each `rrggbb`).
    pub(crate) fn to_config_string(self) -> String {
        let hex = |c: Color32| format!("{:02x}{:02x}{:02x}", c.r(), c.g(), c.b());
        format!(
            "{},{},{},{}",
            hex(self.background),
            hex(self.widget),
            hex(self.text),
            hex(self.accent)
        )
    }

    /// Parse `bg,widget,text[,accent]`; `None` on any malformed required field. A missing
    /// 4th field (older 3-colour configs) defaults accent to the widget colour.
    pub(crate) fn from_config_string(s: &str) -> Option<Self> {
        let mut it = s.split(',');
        let background = parse_hex_rgb(it.next()?)?;
        let widget = parse_hex_rgb(it.next()?)?;
        let text = parse_hex_rgb(it.next()?)?;
        let accent = it.next().and_then(parse_hex_rgb).unwrap_or(widget);
        Some(Self { background, widget, text, accent })
    }
}

fn parse_hex_rgb(s: &str) -> Option<Color32> {
    let s = s.trim();
    if s.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(&s[0..2], 16).ok()?;
    let g = u8::from_str_radix(&s[2..4], 16).ok()?;
    let b = u8::from_str_radix(&s[4..6], 16).ok()?;
    Some(Color32::from_rgb(r, g, b))
}

/// Perceived-luminance test to pick the egui base (dark() vs light()) for a palette.
fn is_dark(c: Color32) -> bool {
    let l = 0.299 * c.r() as f32 + 0.587 * c.g() as f32 + 0.114 * c.b() as f32;
    l < 128.0
}

/// Nudge every channel by `d` (clamped) - derives hovered/active widget states from the
/// single editable `widget` colour.
fn shade(c: Color32, d: i32) -> Color32 {
    let ch = |v: u8| (v as i32 + d).clamp(0, 255) as u8;
    Color32::from_rgb(ch(c.r()), ch(c.g()), ch(c.b()))
}

fn palette_visuals(p: &Palette) -> egui::Visuals {
    let mut v = if is_dark(p.background) {
        egui::Visuals::dark()
    } else {
        egui::Visuals::light()
    };
    // Widget states brighten on a dark base, darken on a light one.
    let (hov, act) = if is_dark(p.background) { (16, 32) } else { (-18, -36) };
    v.panel_fill = p.background;
    v.window_fill = p.background;
    // weak_bg_fill -> buttons/combos (the general widget colour).
    v.widgets.inactive.weak_bg_fill = p.widget;
    v.widgets.hovered.weak_bg_fill = shade(p.widget, hov);
    v.widgets.active.weak_bg_fill = shade(p.widget, act);
    // bg_fill -> slider knob + rail (and other value widgets), tinted with the accent so
    // sliders can be coloured independently of buttons.
    v.widgets.inactive.bg_fill = p.accent;
    v.widgets.hovered.bg_fill = shade(p.accent, hov);
    v.widgets.active.bg_fill = shade(p.accent, act);
    v.override_text_color = Some(p.text);
    v
}

fn classic_visuals() -> egui::Visuals {
    let mut v = egui::Visuals::light();
    let grey = Color32::from_rgb(230, 230, 230);
    v.panel_fill = grey;
    v.window_fill = grey;
    v.widgets.inactive.bg_fill = Color32::from_rgb(210, 210, 215);
    v.widgets.inactive.weak_bg_fill = Color32::from_rgb(210, 210, 215);
    v.widgets.hovered.bg_fill = Color32::from_rgb(195, 195, 200);
    v.widgets.hovered.weak_bg_fill = Color32::from_rgb(195, 195, 200);
    v.widgets.active.bg_fill = Color32::from_rgb(180, 180, 190);
    v.widgets.active.weak_bg_fill = Color32::from_rgb(180, 180, 190);
    v
}

fn dark_visuals() -> egui::Visuals {
    let mut v = egui::Visuals::dark();
    v.panel_fill = Color32::from_rgb(30, 32, 38);
    v.window_fill = Color32::from_rgb(26, 28, 34);
    v.widgets.inactive.bg_fill = Color32::from_rgb(48, 51, 59);
    v.widgets.inactive.weak_bg_fill = Color32::from_rgb(44, 47, 55);
    v.widgets.hovered.bg_fill = Color32::from_rgb(60, 64, 74);
    v.widgets.hovered.weak_bg_fill = Color32::from_rgb(56, 60, 70);
    v.widgets.active.bg_fill = Color32::from_rgb(74, 80, 92);
    v.widgets.active.weak_bg_fill = Color32::from_rgb(70, 76, 88);
    v
}

/// Apply the egui base visuals for `variant`. Called once per frame (cheap). Classic is
/// byte-for-byte the original light scheme; Dark + Slate are curated presets; Custom is
/// built from the user's editable `custom` palette.
pub(crate) fn apply_visuals(ctx: &egui::Context, variant: ThemeVariant, custom: &Palette) {
    let v = match variant {
        ThemeVariant::Classic => classic_visuals(),
        ThemeVariant::Dark => dark_visuals(),
        ThemeVariant::Slate => palette_visuals(&Palette::slate()),
        ThemeVariant::Custom => palette_visuals(custom),
    };
    ctx.set_visuals(v);
}

// ── Toggle / selectie kleuren ──────────────────────────────────────────────

/// Enige selected/toggled-ON fill. Blauw is uitsluitend voor toggled-ON state
/// (`feedback_ui_button_color_convention`). Momentane actie-knoppen krijgen GEEN
/// fill (default `egui::Button`).
pub(crate) const TL_SELECTED_FILL: Color32 = Color32::from_rgb(100, 160, 230);

/// Mode-/status-label in de frequency top-row (amber).
pub(crate) const TL_AMBER_TEXT: Color32 = Color32::from_rgb(255, 170, 40);

/// Gevaar/stop/TX-alert. NIET gebruiken voor "disabled" state.
pub(crate) const TL_DANGER_FILL: Color32 = Color32::from_rgb(200, 40, 40);

// ── Spectrum / waterval theme ──────────────────────────────────────────────
// Gedeeld door de hoofd-spectrum-plot én render_vrx_strip, zodat een wijziging
// aan de hoofdplot automatisch de VRX-plot meeneemt.

pub(crate) const SPECTRUM_BG: Color32 = Color32::from_rgb(10, 15, 30);
pub(crate) const SPECTRUM_LABEL_STRIP: Color32 = Color32::from_rgb(18, 22, 40);
pub(crate) const SPECTRUM_GRID_MAJOR: Color32 = Color32::from_rgb(60, 60, 85);
pub(crate) const SPECTRUM_GRID_MINOR: Color32 = Color32::from_rgb(80, 80, 110);
pub(crate) const SPECTRUM_FILTER_FILL: Color32 = Color32::from_rgb(25, 30, 45);
pub(crate) const SPECTRUM_FILTER_EDGE: Color32 =
    Color32::from_rgba_premultiplied(200, 200, 0, 120);
pub(crate) const SPECTRUM_VFO_TEXT: Color32 = Color32::from_rgb(255, 120, 120);
pub(crate) const SPECTRUM_VFO_LINE: Color32 =
    Color32::from_rgba_premultiplied(255, 50, 50, 180);
pub(crate) const SPECTRUM_AXIS_TEXT: Color32 = Color32::from_rgb(220, 220, 230);
pub(crate) const SPECTRUM_DB_TEXT: Color32 = Color32::from_rgb(200, 200, 210);
pub(crate) const SPECTRUM_SPAN_TEXT: Color32 = Color32::from_rgb(220, 220, 80);
pub(crate) const SPECTRUM_SMETER_TEXT: Color32 = Color32::from_rgb(0, 220, 0);
pub(crate) const WATERFALL_BG: Color32 = Color32::from_rgb(8, 10, 20);

// ── Spacing / layout ───────────────────────────────────────────────────────

/// Verticale gap tussen gestackte receiver-panelen.
/// Geverifieerd tegen RX joined-window (`mod.rs` add_space(2.0) tussen RX1/RX2 spectra).
pub(crate) const TL_PANEL_GAP_Y: f32 = 2.0;
/// Verticale gap tussen spectrum en waterval binnen één paneel.
pub(crate) const TL_INNER_GAP_Y: f32 = 2.0;
/// Hoogte van de spectrum label-strip (parity met hoofd-plot).
pub(crate) const SPECTRUM_LABEL_H: f32 = 18.0;
/// Breedte van de spectrum-control sliders (Ref/Range/Zoom/Pan/WF).
pub(crate) const TL_SLIDER_WIDTH: f32 = 80.0;

// ── Font-maten ─────────────────────────────────────────────────────────────

pub(crate) const TL_FREQ_FONT: f32 = 18.0;
pub(crate) const TL_MODE_STATUS_FONT: f32 = 16.0;
pub(crate) const TL_BW_STATUS_FONT: f32 = 12.0;
pub(crate) const TL_SEGMENT_FONT: f32 = 11.0;
pub(crate) const TL_CHANNEL_HEADER_FONT: f32 = 13.0;

// ── Widget-helpers ───────────────────────────────────────────────────────────

/// Gedeelde toggle/selected-button. Dwingt de huisregels af:
/// - blauwe `TL_SELECTED_FILL` ALLEEN wanneer `selected` (toggled-ON);
/// - OFF-state = default `egui::Button` (geen custom fill / geen "disabled"-grijs);
/// - hover-tekst is VERPLICHT (`feedback_ui_hover_always`).
///
/// Gebruik dit i.p.v. inline `Button::new(...).fill(...)` zodat alle windows
/// dezelfde toggle-stijl én hover krijgen. Retourneert de `Response` zodat de
/// caller `.clicked()` kan checken.
pub(crate) fn tl_toggle_button(
    ui: &mut egui::Ui,
    label: &str,
    selected: bool,
    enabled: bool,
    size: f32,
    hover: &str,
) -> egui::Response {
    let text = if selected {
        egui::RichText::new(label).size(size).strong()
    } else {
        egui::RichText::new(label).size(size)
    };
    let mut btn = egui::Button::new(text);
    if selected {
        btn = btn.fill(TL_SELECTED_FILL);
    }
    ui.add_enabled(enabled, btn).on_hover_text(hover)
}

/// Momentane actie-knop (geen toggle): default styling, GEEN fill, met
/// verplichte hover. Voor knoppen als "Copy VFO", "Refresh", A<>B swap
/// (`feedback_ui_button_color_convention`: geen blauw voor momentane acties).
pub(crate) fn tl_action_button(
    ui: &mut egui::Ui,
    label: &str,
    enabled: bool,
    size: f32,
    hover: &str,
) -> egui::Response {
    let btn = egui::Button::new(egui::RichText::new(label).size(size));
    ui.add_enabled(enabled, btn).on_hover_text(hover)
}

/// Gedeelde segmented-selector: een rij toggle-knoppen uit (waarde, label)-paren,
/// allemaal met dezelfde stijl en verplichte hover (via `tl_toggle_button`). De
/// knop van de huidige `selected`-waarde krijgt de blauwe ON-fill. Retourneert de
/// aangeklikte waarde (of `None`); de caller handelt de klik af, zodat de selector
/// vrij blijft van state/dispatch. Dedupliceert mode-/BW-keuzerijen (en is
/// herbruikbaar voor andere windows).
pub(crate) fn tl_segmented_selector<T: Copy + PartialEq>(
    ui: &mut egui::Ui,
    items: impl IntoIterator<Item = (T, String)>,
    selected: T,
    enabled: bool,
    size: f32,
    hover: &str,
) -> Option<T> {
    let mut clicked: Option<T> = None;
    for (val, label) in items {
        if tl_toggle_button(ui, &label, val == selected, enabled, size, hover).clicked() {
            clicked = Some(val);
        }
    }
    clicked
}
