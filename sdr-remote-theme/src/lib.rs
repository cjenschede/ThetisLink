// SPDX-License-Identifier: GPL-2.0-or-later

//! Shared UI theme for the ThetisLink egui applications (client, server and
//! the chat window both of them open).
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

/// The one line of the client, the server and the chat: above and below a
/// settings group, and in place of every `ui.separator()` (owner, 2026-10-15:
/// "overal de dikke lijn", and "server en chat ook ja";
/// `every_line_is_the_rule` keeps `ui.separator()` out). egui's own separator
/// (one thin pixel in the faint border colour, a few pixels of room) was hard
/// to see and sat close to the text (owner, 2026-09-18): this one is drawn in
/// the theme's weak text colour, so it follows all four themes, a little
/// heavier, with more room on both sides.
///
/// In a row it is the upright line egui would draw there, with egui's width,
/// so a row of buttons does not get wider - only the stroke is this one's.
/// The panels at the edges of a window do not draw egui's border line
/// (`show_separator_line(false)`); where their edge needs a line, it is this
/// one, so there is one kind of line.
///
/// A little see-through (owner, 2026-10-15): the lines are there to find the
/// groups, and when they are too present you see lines and have to look for
/// what is between them.
pub fn rule(ui: &mut egui::Ui) {
    const ROOM: f32 = 6.0;
    let stroke = rule_stroke(ui);
    if ui.layout().main_dir().is_horizontal() {
        ui.scope(|ui| {
            ui.visuals_mut().widgets.noninteractive.bg_stroke = stroke;
            ui.add(egui::Separator::default());
        });
        return;
    }
    // A window that sizes itself to its content measures it first; a line
    // that takes the full width then would set that width (review of 200).
    // egui's own line takes none in that pass.
    let width = if ui.is_sizing_pass() { 0.0 } else { ui.available_width() };
    ui.add_space(ROOM);
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, stroke.width), egui::Sense::hover());
    ui.painter().hline(rect.x_range(), rect.center().y, stroke);
    ui.add_space(ROOM);
}

/// The stroke of the one line: the theme's weak text colour, a little heavier
/// than egui's own and a little see-through. `rule` draws with it, and so does
/// the edge of a `group` - one kind of line (owner, 2026-10-15).
fn rule_stroke(ui: &egui::Ui) -> egui::Stroke {
    const STROKE: f32 = 1.5;
    // 0.75 in build 201, 0.5 to try (owner, 2026-10-15).
    const RULE_OPACITY: f32 = 0.5;
    egui::Stroke::new(STROKE, ui.visuals().weak_text_color().gamma_multiply(RULE_OPACITY))
}

/// A box around a group of controls - a tuner, a board, RX1 - with its edge
/// in the stroke of `rule`, in place of egui's own thin grey one (owner,
/// 2026-10-15: "Kader houden, rand als de lijn"). Spacing and corners are
/// egui's group's, so nothing moves. `every_line_is_the_rule` keeps
/// `Frame::group` and `ui.group` out of the screens.
pub fn group(ui: &egui::Ui) -> egui::Frame {
    egui::Frame::group(ui.style()).stroke(rule_stroke(ui))
}

/// The ground of a window that is not the main one, in the theme's panel
/// colour, under its panels. eframe wipes such a window (an immediate
/// viewport) with black, not with `App::clear_color`, and with the panels' own
/// border lines off (build 201) that showed as a thin black line in the seam
/// above a Yaesu window's PTT bar (owner, 2026-10-15). egui's panels paint in
/// this same background layer, after this. The client's pop-outs call it before
/// their content, and the chat calls it first in `render_body`, so its window
/// from the server gets it too (review of 208). A main window gets the same
/// colour from `clear_color`; that one also covers the moment of a resize.
pub fn paint_window_ground(ctx: &egui::Context) {
    let fill = ctx.style().visuals.panel_fill;
    ctx.layer_painter(egui::LayerId::background()).rect_filled(ctx.screen_rect(), 0.0, fill);
}

/// One colour of the custom theme, picked without see-through: the saved
/// settings keep red, green and blue only, so a colour with alpha came back
/// darker after a restart, and a see-through ground showed the seams as dark
/// lines (review of 203). Client and server both pick with this one button.
pub fn colour_button(ui: &mut egui::Ui, colour: &mut Color32) -> egui::Response {
    let mut rgb = [colour.r(), colour.g(), colour.b()];
    let response = ui.color_edit_button_srgb(&mut rgb);
    if response.changed() {
        *colour = Color32::from_rgb(rgb[0], rgb[1], rgb[2]);
    }
    response
}

#[cfg(test)]
mod rule_tests {
    use std::path::{Path, PathBuf};

    fn sources(krate: &str) -> Vec<PathBuf> {
        fn walk(dir: &Path, files: &mut Vec<PathBuf>) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    walk(&path, files);
                } else if path.extension().is_some_and(|e| e == "rs") {
                    files.push(path);
                }
            }
        }
        let mut files = Vec::new();
        walk(&Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join(krate).join("src"), &mut files);
        files
    }

    /// Every line in the client, the server and the chat is `rule` (owner,
    /// 2026-10-15: "overal de dikke lijn", "server en chat ook ja"), and no
    /// panel draws egui's own border line next to it. Read from disk rather
    /// than a hand-written list of files, so a new screen is seen too; the
    /// count of files read per crate keeps the walk honest.
    #[test]
    fn every_line_is_the_rule() {
        let crates = [
            ("sdr-remote-client", 40),
            ("sdr-remote-server", 40),
            ("sdr-remote-chat", 3),
            ("sdr-remote-theme", 1),
            // Draws in the windows of the client and the server too (review of 208).
            ("sdr-remote-layout", 1),
        ];
        let thin = [concat!(".separator", "()"), concat!("Separator::", "default()")];
        let allowed = concat!("ui.add(egui::Separator", "::default());");
        let panels = [concat!("TopBottomPanel", "::"), concat!("SidePanel", "::")];
        // egui's own box with its thin grey edge (build 211), on any name for
        // the `Ui` and any closure - `cols[0].group(|ui| ...)`, `ui.group(move |ui|
        // ...)` (reviews of 211 and 212).
        let boxes = [concat!("Frame::", "group("), concat!(".group", "(")];
        let box_allowed = concat!("egui::Frame::", "group(ui.style()).stroke(rule_stroke(ui))");
        let mut box_seen = 0;
        let colour = concat!("color_edit", "_button");
        let mut allowed_seen = 0;
        let mut panels_seen = 0;
        let mut colour_seen = 0;
        for (krate, at_least) in crates {
            let files = sources(krate);
            assert!(files.len() >= at_least, "{krate}: only {} source files found", files.len());
            for file in &files {
                let text = std::fs::read_to_string(file).unwrap();
                for line in text.lines() {
                    if line.trim_start().starts_with("//") {
                        continue;
                    }
                    for pattern in thin {
                        if line.contains(pattern) {
                            if krate == "sdr-remote-theme" && line.trim() == allowed {
                                allowed_seen += 1;
                                continue;
                            }
                            panic!("{}: `{}` draws egui's own thin line; use `rule`", file.display(), line.trim());
                        }
                    }
                    if boxes.iter().any(|b| line.contains(b)) {
                        if krate == "sdr-remote-theme" && line.trim() == box_allowed {
                            box_seen += 1;
                        } else {
                            panic!("{}: `{}` draws egui's own box edge; use `group`", file.display(), line.trim());
                        }
                    }
                    if line.contains(colour) {
                        // One colour button, without alpha (review of 208): a
                        // second one could pick a see-through ground again.
                        assert_eq!(krate, "sdr-remote-theme", "{}: `{}` picks a colour itself; use `colour_button`", file.display(), line.trim());
                        colour_seen += 1;
                    }
                    if panels.iter().any(|p| line.contains(p)) {
                        panels_seen += 1;
                        assert!(
                            line.contains(".show_separator_line(false)"),
                            "{}: `{}` draws egui's own border line; turn it off and use `rule` where the edge needs one",
                            file.display(),
                            line.trim()
                        );
                    }
                }
            }
        }
        // The one line inside `rule` itself, and not a second copy of it
        // elsewhere in this file (review of 200).
        assert_eq!(allowed_seen, 1, "egui's own line is drawn outside `rule`");
        // Five panels in the client, three in the chat: the check above ran.
        assert!(panels_seen >= 8, "only {panels_seen} panels found");
        assert_eq!(colour_seen, 1, "the colour button inside `colour_button`");
        assert_eq!(box_seen, 1, "egui's box inside `group`");
    }

    /// The chat paints its window's ground before anything else, so the window
    /// the server opens for it has no black seams between its panels (review of
    /// 208: the client's pop-outs painted it, the server's chat window did not).
    #[test]
    fn the_chat_paints_its_ground_first() {
        let panel = include_str!("../../sdr-remote-chat/src/panel.rs");
        let start = panel.find("pub fn render_body(").expect("render_body");
        let body = &panel[panel[start..].find('{').unwrap() + start + 1..];
        let first = body.lines().map(str::trim).find(|l| !l.is_empty() && !l.starts_with("//")).unwrap();
        assert_eq!(first, "sdr_remote_theme::paint_window_ground(ctx);");
    }
}
