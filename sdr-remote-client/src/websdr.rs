// SPDX-License-Identifier: GPL-2.0-or-later

//! Embedded WebSDR/KiwiSDR window using wry WebView + tao window.
//!
//! Cross-platform: uses tao for windowing (Win32/Cocoa/X11) and wry for WebView
//! (WebView2 on Windows, WKWebView on macOS, WebKitGTK on Linux).
//! Detects SDR type from URL and uses appropriate JS commands.
//!
//! Uses a manual poll loop instead of event_loop.run() to avoid process::exit()
//! when the WebView window is closed (we're on a background thread).

use std::sync::mpsc;

use log::{info, warn};

/// Supported SDR web interface types.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum SdrType {
    WebSdr,  // websdr.org
    KiwiSdr, // KiwiSDR
}

impl SdrType {
    /// Detect SDR type from URL.
    pub fn detect(url: &str) -> Self {
        let lower = url.to_ascii_lowercase();
        if lower.contains(":8073") || lower.contains("kiwisdr") {
            SdrType::KiwiSdr
        } else {
            SdrType::WebSdr
        }
    }
}

/// Commands sent from UI thread to the WebView thread.
pub enum WebSdrCmd {
    #[allow(dead_code)]
    Navigate(String),
    Mute(bool),
    SetFreq(u64, u8),
    Close,
}

/// Spawn a WebSDR window on its own thread. Returns the sender for commands.
pub fn spawn_websdr_window(url: &str, sdr_type: SdrType) -> mpsc::Sender<WebSdrCmd> {
    let (tx, rx) = mpsc::channel::<WebSdrCmd>();
    let url = url.to_string();

    std::thread::Builder::new()
        .name("websdr-window".into())
        .spawn(move || {
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                run_webview_loop(&url, sdr_type, rx)
            })) {
                Ok(Ok(())) => info!("WebSDR window thread ended normally"),
                Ok(Err(e)) => warn!("WebSDR window error: {}", e),
                Err(panic) => {
                    let msg = panic.downcast_ref::<String>()
                        .map(|s| s.as_str())
                        .or_else(|| panic.downcast_ref::<&str>().copied())
                        .unwrap_or("unknown panic");
                    warn!("WebSDR window panicked: {}", msg);
                }
            }
        })
        .expect("spawn websdr thread");

    tx
}

// ── JS helpers: WebSDR.org ───────────────────────────────────────────────

fn websdr_mode_params(mode: u8) -> (&'static str, f64, f64) {
    match mode {
        0 => ("lsb", -2.7, -0.3),
        1 => ("usb", 0.3, 2.7),
        5 => ("fm", -6.0, 6.0),
        6 => ("am", -4.0, 4.0),
        _ => ("usb", 0.3, 2.7),
    }
}

fn websdr_js_set_freq(freq_hz: u64, mode: u8) -> String {
    let freq_khz = freq_hz as f64 / 1000.0;
    let (mode_str, lo, hi) = websdr_mode_params(mode);
    format!(
        "try {{ setfreqif('{}'); setmf('{}', {}, {}); }} catch(e) {{}}",
        freq_khz as u64, mode_str, lo, hi
    )
}

fn websdr_js_mute(mute: bool) -> String {
    format!("try {{ setmute({}); }} catch(e) {{}}", if mute { 1 } else { 0 })
}

fn websdr_init_script() -> &'static str {
    "setTimeout(function() { try { wfset(2); } catch(e) {} }, 4000);"
}

// ── JS helpers: KiwiSDR ──────────────────────────────────────────────────

fn kiwi_mode_str(mode: u8) -> &'static str {
    match mode {
        0 => "lsb",
        1 => "usb",
        5 => "nbfm",
        6 => "am",
        7 => "cw",
        _ => "usb",
    }
}

fn kiwi_js_set_freq(freq_hz: u64, mode: u8) -> String {
    let freq_khz = freq_hz as f64 / 1000.0;
    let mode_str = kiwi_mode_str(mode);
    format!(
        "try {{ freqmode_set_dsp_kHz({:.3}, '{}'); }} catch(e) {{}}",
        freq_khz, mode_str
    )
}

fn kiwi_js_mute(mute: bool) -> String {
    if mute {
        "try { kiwi.volume_f = 1e-6; kiwi.muted = true; \
         w3_show_hide('id-mute-no', false); w3_show_hide('id-mute-yes', true); } catch(e) {}".to_string()
    } else {
        "try { kiwi.muted = false; kiwi.volume_f = kiwi.volume / 100; \
         w3_show_hide('id-mute-no', true); w3_show_hide('id-mute-yes', false); } catch(e) {}".to_string()
    }
}

fn kiwi_init_script() -> &'static str {
    "setTimeout(function() { try { zoom_step(ext_zoom.NOM_IN); } catch(e) {} }, 4000);"
}

// ── Dispatch per SDR type ────────────────────────────────────────────────

fn js_set_freq(sdr_type: SdrType, freq_hz: u64, mode: u8) -> String {
    match sdr_type {
        SdrType::WebSdr => websdr_js_set_freq(freq_hz, mode),
        SdrType::KiwiSdr => kiwi_js_set_freq(freq_hz, mode),
    }
}

fn js_mute(sdr_type: SdrType, mute: bool) -> String {
    match sdr_type {
        SdrType::WebSdr => websdr_js_mute(mute),
        SdrType::KiwiSdr => kiwi_js_mute(mute),
    }
}

fn init_script(sdr_type: SdrType) -> &'static str {
    match sdr_type {
        SdrType::WebSdr => websdr_init_script(),
        SdrType::KiwiSdr => kiwi_init_script(),
    }
}

// ── Windows: Win32 message pump (manual poll, no process::exit) ──────────

#[cfg(target_os = "windows")]
fn run_webview_loop(url: &str, sdr_type: SdrType, rx: mpsc::Receiver<WebSdrCmd>) -> anyhow::Result<()> {
    use wry::raw_window_handle;

    // Raw Win32 FFI — minimal set for window creation + message pump
    #[repr(C)]
    struct WNDCLASSEXW {
        cb_size: u32, style: u32,
        lpfn_wnd_proc: Option<unsafe extern "system" fn(isize, u32, usize, isize) -> isize>,
        cb_cls_extra: i32, cb_wnd_extra: i32, h_instance: isize,
        h_icon: isize, h_cursor: isize, hbr_background: isize,
        lpsz_menu_name: *const u16, lpsz_class_name: *const u16, h_icon_sm: isize,
    }
    #[repr(C)]
    struct MSG {
        hwnd: isize, message: u32, w_param: usize, l_param: isize,
        time: u32, pt_x: i32, pt_y: i32,
    }
    const WS_OVERLAPPEDWINDOW: u32 = 0x00CF0000;
    const WS_VISIBLE: u32 = 0x10000000;
    const CW_USEDEFAULT: i32 = 0x80000000u32 as i32;
    const SW_SHOW: i32 = 5;
    const PM_REMOVE: u32 = 0x0001;
    const WM_QUIT: u32 = 0x0012;

    extern "system" {
        fn RegisterClassExW(lpwcx: *const WNDCLASSEXW) -> u16;
        fn CreateWindowExW(
            ex_style: u32, class_name: *const u16, window_name: *const u16,
            style: u32, x: i32, y: i32, width: i32, height: i32,
            parent: isize, menu: isize, instance: isize, param: isize,
        ) -> isize;
        fn ShowWindow(hwnd: isize, cmd: i32) -> i32;
        fn UpdateWindow(hwnd: isize) -> i32;
        fn DestroyWindow(hwnd: isize) -> i32;
        fn IsWindow(hwnd: isize) -> i32;
        fn PeekMessageW(msg: *mut MSG, hwnd: isize, min: u32, max: u32, remove: u32) -> i32;
        fn TranslateMessage(msg: *const MSG) -> i32;
        fn DispatchMessageW(msg: *const MSG) -> isize;
        fn DefWindowProcW(hwnd: isize, msg: u32, wparam: usize, lparam: isize) -> isize;
        fn LoadCursorW(instance: isize, name: *const u16) -> isize;
        fn GetStockObject(obj: i32) -> isize;
        fn UnregisterClassW(class_name: *const u16, instance: isize) -> i32;
        fn GetModuleHandleW(name: *const u16) -> isize;
    }

    unsafe extern "system" fn wnd_proc(hwnd: isize, msg: u32, wparam: usize, lparam: isize) -> isize {
        DefWindowProcW(hwnd, msg, wparam, lparam)
    }

    struct HwndWrapper(isize);
    unsafe impl Send for HwndWrapper {}
    impl raw_window_handle::HasWindowHandle for HwndWrapper {
        fn window_handle(&self) -> Result<raw_window_handle::WindowHandle<'_>, raw_window_handle::HandleError> {
            let handle = raw_window_handle::Win32WindowHandle::new(
                std::num::NonZeroIsize::new(self.0).unwrap(),
            );
            let raw = raw_window_handle::RawWindowHandle::Win32(handle);
            Ok(unsafe { raw_window_handle::WindowHandle::borrow_raw(raw) })
        }
    }

    fn to_wide(s: &str) -> Vec<u16> {
        use std::ffi::OsStr;
        use std::os::windows::ffi::OsStrExt;
        OsStr::new(s).encode_wide().chain(Some(0)).collect()
    }

    let class_name = to_wide("ThetisLinkWebSDR");
    let title = match sdr_type {
        SdrType::WebSdr => "ThetisLink WebSDR",
        SdrType::KiwiSdr => "ThetisLink KiwiSDR",
    };
    let window_title = to_wide(title);

    unsafe {
        let hinstance = GetModuleHandleW(std::ptr::null());
        let wc = WNDCLASSEXW {
            cb_size: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: 0x0003, // CS_HREDRAW | CS_VREDRAW
            lpfn_wnd_proc: Some(wnd_proc),
            cb_cls_extra: 0, cb_wnd_extra: 0, h_instance: hinstance,
            h_icon: 0, h_cursor: LoadCursorW(0, 32512 as *const u16),
            hbr_background: GetStockObject(0),
            lpsz_menu_name: std::ptr::null(), lpsz_class_name: class_name.as_ptr(), h_icon_sm: 0,
        };

        if RegisterClassExW(&wc) == 0 {
            anyhow::bail!("RegisterClassExW failed");
        }

        let hwnd = CreateWindowExW(
            0, class_name.as_ptr(), window_title.as_ptr(),
            WS_OVERLAPPEDWINDOW | WS_VISIBLE,
            CW_USEDEFAULT, CW_USEDEFAULT, 1024, 768,
            0, 0, hinstance, 0,
        );
        if hwnd == 0 {
            UnregisterClassW(class_name.as_ptr(), hinstance);
            anyhow::bail!("CreateWindowExW failed");
        }

        ShowWindow(hwnd, SW_SHOW);
        UpdateWindow(hwnd);

        let hwnd_wrapper = HwndWrapper(hwnd);
        let webview = match wry::WebViewBuilder::new()
            .with_url(url)
            .with_autoplay(true)
            .with_initialization_script(init_script(sdr_type))
            .build(&hwnd_wrapper)
        {
            Ok(wv) => wv,
            Err(e) => {
                DestroyWindow(hwnd);
                UnregisterClassW(class_name.as_ptr(), hinstance);
                anyhow::bail!("WebView2 niet beschikbaar: {}", e);
            }
        };

        info!("WebSDR window opened ({:?}): {}", sdr_type, url);

        let mut msg: MSG = std::mem::zeroed();
        loop {
            while PeekMessageW(&mut msg, 0, 0, 0, PM_REMOVE) != 0 {
                if msg.message == WM_QUIT {
                    info!("WebSDR window closed by user");
                    UnregisterClassW(class_name.as_ptr(), hinstance);
                    return Ok(());
                }
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            if IsWindow(hwnd) == 0 {
                info!("WebSDR window destroyed");
                UnregisterClassW(class_name.as_ptr(), hinstance);
                return Ok(());
            }
            while let Ok(cmd) = rx.try_recv() {
                match cmd {
                    WebSdrCmd::Navigate(new_url) => { let _ = webview.load_url(&new_url); }
                    WebSdrCmd::Mute(mute) => { let _ = webview.evaluate_script(&js_mute(sdr_type, mute)); }
                    WebSdrCmd::SetFreq(freq_hz, mode) => { let _ = webview.evaluate_script(&js_set_freq(sdr_type, freq_hz, mode)); }
                    WebSdrCmd::Close => {
                        info!("WebSDR window closing (command)");
                        DestroyWindow(hwnd);
                        UnregisterClassW(class_name.as_ptr(), hinstance);
                        return Ok(());
                    }
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(16));
        }
    }
}

// ── macOS: Safari + AppleScript JS injection ────────────────────────────
// Opens URL in Safari, then uses osascript to inject JavaScript for mute
// and freq sync. Requires: Safari → Develop → Allow JavaScript from Apple Events.

#[cfg(target_os = "macos")]
fn run_webview_loop(url: &str, sdr_type: SdrType, rx: mpsc::Receiver<WebSdrCmd>) -> anyhow::Result<()> {
    info!("WebSDR opening in Safari ({:?}): {}", sdr_type, url);
    let _ = open::that(url);

    // Give Safari time to open the tab
    std::thread::sleep(std::time::Duration::from_secs(2));

    loop {
        match rx.recv() {
            Ok(WebSdrCmd::Close) => {
                info!("WebSDR Safari session closed");
                return Ok(());
            }
            Ok(WebSdrCmd::Mute(mute)) => {
                let js = js_mute(sdr_type, mute);
                safari_eval_js(&js);
            }
            Ok(WebSdrCmd::SetFreq(freq_hz, mode)) => {
                let js = js_set_freq(sdr_type, freq_hz, mode);
                safari_eval_js(&js);
            }
            Ok(WebSdrCmd::Navigate(new_url)) => {
                safari_eval_js(&format!("window.location.href = '{}';", new_url));
            }
            Err(_) => return Ok(()),
        }
    }
}

/// Execute JavaScript in Safari's front tab via AppleScript.
/// Requires: Safari → Develop → Allow JavaScript from Apple Events.
#[cfg(target_os = "macos")]
fn safari_eval_js(js: &str) {
    // Escape single quotes and backslashes for AppleScript string
    let escaped = js.replace('\\', "\\\\").replace('\'', "\\'");
    let script = format!(
        "tell application \"Safari\" to do JavaScript '{}' in current tab of front window",
        escaped
    );
    match std::process::Command::new("osascript")
        .arg("-e")
        .arg(&script)
        .output()
    {
        Ok(output) => {
            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                if !stderr.is_empty() {
                    warn!("Safari JS eval failed: {}", stderr.trim());
                }
            }
        }
        Err(e) => warn!("osascript failed: {}", e),
    }
}

// ── Linux: external browser fallback ─────────────────────────────────────

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn run_webview_loop(url: &str, sdr_type: SdrType, rx: mpsc::Receiver<WebSdrCmd>) -> anyhow::Result<()> {
    info!("WebSDR opening in external browser ({:?}): {}", sdr_type, url);
    let _ = open::that(url);

    loop {
        match rx.recv() {
            Ok(WebSdrCmd::Close) => {
                info!("WebSDR external browser session closed");
                return Ok(());
            }
            Ok(_) => {}
            Err(_) => return Ok(()),
        }
    }
}
