// SPDX-License-Identifier: GPL-2.0-or-later
#![allow(dead_code)] // shared util with the client; not every item is used server-side
//! Validate a restored pop-out position against the *live* monitor layout.
//!
//! egui/eframe restore a saved pop-out position via
//! `ViewportBuilder::with_position`, but never expose the list of currently
//! connected monitors to the app. A window saved on a second monitor that is
//! later disconnected therefore re-opens off-screen (invisible). We query the
//! OS monitor work-areas directly and only re-apply a saved position when a
//! usable part of the window would land on a connected monitor - otherwise the
//! caller drops `with_position()` and the window opens on the primary monitor.
//!
//! Design: OS enumeration (not a wider plausibility band), validate by
//! *visible overlap* (not full containment), Windows now / macOS later, with
//! the manual `recenter_popouts()` button kept as a fallback. Comparison is
//! done in physical pixels because the OS reports monitor rects in
//! virtual-desktop pixels while egui works in logical points.

use egui::{Pos2, Vec2};

/// A monitor work-area rectangle in physical (virtual-desktop) pixels -
/// excludes the taskbar (`rcWork`), so we never count a window as "visible"
/// when only the part behind the taskbar overlaps.
#[derive(Clone, Copy, Debug)]
pub(crate) struct RectPx {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

/// Require at least this much of the window's top "drag zone" (in logical
/// points) to overlap a monitor work-area. Enough to grab the title bar and
/// drag the window - but not full containment, so a user may still park a
/// window deliberately half off-screen.
const MIN_VISIBLE_W: f32 = 120.0;
const MIN_VISIBLE_H: f32 = 32.0;
/// Height of the window's top band we treat as the recoverable "drag zone".
const DRAG_ZONE_H: f32 = 48.0;

/// True when enough of the window's top drag-zone overlaps a connected monitor
/// work-area for the saved position to be usable.
///
/// Returns `true` (i.e. accept the saved position) whenever the monitor layout
/// can't be determined - non-Windows builds, or an enumeration failure - so the
/// behaviour falls back to "trust the saved position" rather than wrongly
/// recentering a window the user can actually see.
pub(crate) fn saved_window_is_visible(pos: Pos2, size: Vec2, native_ppp: f32) -> bool {
    let areas = match monitor_work_areas_px() {
        Some(a) if !a.is_empty() => a,
        _ => return true, // unknown layout -> don't second-guess
    };
    let ppp = if native_ppp > 0.0 { native_ppp } else { 1.0 };

    // Window top drag-zone, converted logical points -> physical pixels.
    let wl = pos.x * ppp;
    let wt = pos.y * ppp;
    let wr = (pos.x + size.x) * ppp;
    let wb = (pos.y + DRAG_ZONE_H.min(size.y.max(0.0))) * ppp;
    let need_w = MIN_VISIBLE_W * ppp;
    let need_h = MIN_VISIBLE_H * ppp;

    areas.iter().any(|a| {
        let ix = (wr.min(a.right as f32) - wl.max(a.left as f32)).max(0.0);
        let iy = (wb.min(a.bottom as f32) - wt.max(a.top as f32)).max(0.0);
        ix >= need_w && iy >= need_h
    })
}

/// Enumerate the work-areas of all connected monitors in physical pixels.
/// `None` when the layout can't be queried (non-Windows, or the OS call fails).
#[cfg(target_os = "windows")]
pub(crate) fn monitor_work_areas_px() -> Option<Vec<RectPx>> {
    use windows::Win32::Foundation::{BOOL, LPARAM, RECT};
    use windows::Win32::Graphics::Gdi::{
        EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO,
    };

    unsafe extern "system" fn enum_cb(
        h: HMONITOR,
        _hdc: HDC,
        _clip: *mut RECT,
        data: LPARAM,
    ) -> BOOL {
        // `data` is the &mut Vec<RectPx> we passed as dwData.
        let out = &mut *(data.0 as *mut Vec<RectPx>);
        let mut mi = MONITORINFO {
            cbSize: core::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if GetMonitorInfoW(h, &mut mi).as_bool() {
            let r = mi.rcWork;
            out.push(RectPx {
                left: r.left,
                top: r.top,
                right: r.right,
                bottom: r.bottom,
            });
        }
        BOOL(1) // keep enumerating
    }

    let mut out: Vec<RectPx> = Vec::new();
    let ok = unsafe {
        EnumDisplayMonitors(
            HDC::default(),
            None,
            Some(enum_cb),
            LPARAM(&mut out as *mut Vec<RectPx> as isize),
        )
    };
    if ok.as_bool() {
        Some(out)
    } else {
        None
    }
}

#[cfg(not(target_os = "windows"))]
pub(crate) fn monitor_work_areas_px() -> Option<Vec<RectPx>> {
    // macOS (NSScreen/CoreGraphics) can be added here later; until then the
    // caller falls back to trusting the saved position + the manual recenter.
    None
}

/// System DPI scale (pixels-per-point) for the primary display. Used at STARTUP - before
/// an egui context exists (which is where `native_pixels_per_point` normally comes from) -
/// so the main window's saved logical rect can be converted to physical pixels for the
/// visibility check. Falls back to 1.0 on non-Windows or on failure; the visibility check
/// itself then trusts the saved position, so a wrong scale never hides a visible window.
#[cfg(target_os = "windows")]
pub(crate) fn system_ppp() -> f32 {
    use windows::Win32::UI::HiDpi::GetDpiForSystem;
    let dpi = unsafe { GetDpiForSystem() };
    if dpi > 0 {
        dpi as f32 / 96.0
    } else {
        1.0
    }
}

#[cfg(not(target_os = "windows"))]
pub(crate) fn system_ppp() -> f32 {
    1.0
}

/// Write the monitor layout to the log, once, at start-up.
///
/// A window that comes up blank, or off-screen, or on the wrong monitor is
/// always a question about coordinates - and the log travels with a problem
/// report, so the answer should already be in it. It was not: a report about a
/// blank window cost a round of guessing about which screen was where, and the
/// guess was wrong. One line at start-up settles that for every report after
/// this one.
///
/// Work-areas are virtual-desktop pixels, so a monitor left of or above the
/// primary shows negative values - which is exactly what such a report needs to
/// say. Nothing here identifies a person or a place; it is screen geometry.
pub(crate) fn log_monitor_layout() {
    match monitor_work_areas_px() {
        Some(areas) if !areas.is_empty() => {
            let list: Vec<String> = areas
                .iter()
                .map(|a| {
                    format!(
                        "[{},{} {}x{}]",
                        a.left,
                        a.top,
                        a.right - a.left,
                        a.bottom - a.top
                    )
                })
                .collect();
            log::info!(
                "Monitor layout: {} screen(s), work areas in virtual-desktop pixels: {} (scale {:.2}x)",
                areas.len(),
                list.join(" "),
                system_ppp()
            );
        }
        _ => log::info!("Monitor layout: could not be queried on this platform"),
    }
}
