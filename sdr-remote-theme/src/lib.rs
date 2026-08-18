// SPDX-License-Identifier: GPL-2.0-or-later

//! Shared UI theme for the ThetisLink egui applications (client and server).
//!
//! Step 1 of the theme migration switches only the egui base visuals: panel and
//! window fills, widget fills and the default text colour. Everything that
//! already reads from `ui.visuals()` follows along automatically. Colours that
//! are hardcoded per call-site are moved to a shared palette in later steps.
//!
//! This crate exists because client and server must be able to show the same
//! theme: `sdr-remote-logic` cannot host it, because that is the crate shared
//! with Android and it must not become dependent on egui.

use egui::Color32;

/// Selectable UI theme. Classic reproduces the original light look.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ThemeVariant {
    Classic,
    Dark,
    Slate,
    Custom,
}

impl ThemeVariant {
    pub const ALL: [ThemeVariant; 4] = [
        ThemeVariant::Classic,
        ThemeVariant::Dark,
        ThemeVariant::Slate,
        ThemeVariant::Custom,
    ];

    pub fn label(self) -> &'static str {
        match self {
            ThemeVariant::Classic => "Classic (light)",
            ThemeVariant::Dark => "Dark",
            ThemeVariant::Slate => "Slate",
            ThemeVariant::Custom => "Custom",
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            ThemeVariant::Classic => "classic",
            ThemeVariant::Dark => "dark",
            ThemeVariant::Slate => "slate",
            ThemeVariant::Custom => "custom",
        }
    }

    pub fn from_str(s: &str) -> ThemeVariant {
        match s.trim().to_ascii_lowercase().as_str() {
            "dark" => ThemeVariant::Dark,
            "slate" => ThemeVariant::Slate,
            "custom" => ThemeVariant::Custom,
            _ => ThemeVariant::Classic,
        }
    }
}

/// User-editable base colours. Step-1 scope: the slots that fully drive the egui
/// base visuals (so a Custom theme applies everywhere that reads `ui.visuals()`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Palette {
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
    pub fn slate() -> Self {
        Self {
            background: Color32::from_rgb(28, 32, 42),
            widget: Color32::from_rgb(52, 60, 76),
            text: Color32::from_rgb(210, 216, 228),
            accent: Color32::from_rgb(86, 132, 204),
        }
    }

    /// Serialise to one conf value: `bg,widget,text,accent` (each `rrggbb`).
    pub fn to_config_string(self) -> String {
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
    pub fn from_config_string(s: &str) -> Option<Self> {
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
pub fn apply_visuals(ctx: &egui::Context, variant: ThemeVariant, custom: &Palette) {
    let v = match variant {
        ThemeVariant::Classic => classic_visuals(),
        ThemeVariant::Dark => dark_visuals(),
        ThemeVariant::Slate => palette_visuals(&Palette::slate()),
        ThemeVariant::Custom => palette_visuals(custom),
    };
    ctx.set_visuals(v);
}
