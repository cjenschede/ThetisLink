// SPDX-License-Identifier: GPL-2.0-or-later
//! Whether filter edges fit a Thetis DSP mode. The client keeps a band memory
//! only with edges that fit (build 243), and the server sends Thetis only edges
//! that fit the mode it has or was just asked for (build 244).

/// Whether filter edges fit a Thetis DSP mode: the lower sideband modes (LSB,
/// CW-L, DIGL) below the carrier, the upper ones (USB, CW-U, DIGU) above it,
/// AM, SAM, DSB and DRM around it. FM and SPEC take any edges (Thetis derives
/// the FM filter itself). A band memory keeps and restores only edges that fit
/// (build 243): LSB with -3000..3000 or 25..3025 had been kept and was sent back
/// on every return to the band, and Thetis took it as it came.
pub fn edges_fit_mode(mode: u8, low: i32, high: i32) -> bool {
    if high <= low {
        return false;
    }
    match mode {
        0 | 3 | 9 => high <= 0,
        1 | 4 | 7 => low >= 0,
        2 | 6 | 10 | 11 => low < 0 && high > 0,
        _ => true,
    }
}

#[cfg(test)]
mod fit_tests {
    use super::edges_fit_mode;

    /// The two band memories of the owner's drive (2026-10-17) do not fit, the
    /// ordinary filters of every mode do (build 243).
    #[test]
    fn edges_fit_their_mode() {
        assert!(!edges_fit_mode(0, -3000, 3000), "LSB with an AM filter (160 m)");
        assert!(!edges_fit_mode(0, 25, 3025), "LSB with a USB filter (60 m)");
        for (mode, lo, hi) in [(0, -3100, -100), (9, -3000, -300), (3, -750, -250), (1, 25, 3025), (7, 300, 3000),
                               (4, 250, 750), (6, -5000, 5000), (10, -6000, 6000), (2, -3000, 3000), (11, -5000, 5000),
                               (5, -8000, 8000), (8, -1000, 4000)] {
            assert!(edges_fit_mode(mode, lo, hi), "mode {mode}: {lo}..{hi}");
        }
        assert!(!edges_fit_mode(1, -3100, -100), "USB with an LSB filter");
        assert!(!edges_fit_mode(10, 25, 3025), "SAM with a USB filter");
        assert!(!edges_fit_mode(6, -3100, -100), "AM with an LSB filter");
        assert!(!edges_fit_mode(1, 3000, 100), "no width");
    }
}
