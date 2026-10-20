//! Win32 crosshair drag-to-select region overlay (Snipping Tool style).
//!
//! Replaces the WinRT `GraphicsCapturePicker` (which made the user pick a
//! WINDOW) with a per-monitor dimmed overlay: left-drag draws a rectangle,
//! mouse-up returns it in desktop PHYSICAL pixel coordinates.
//!
//! Doctrine honoured (geometry.rs): one overlay per monitor; the drag is
//! constrained to the monitor where it began (`constrained_drag` — clamp +
//! scale mapping), never a desktop-wide overlay. Coordinates are physical
//! pixels throughout: the selector forces PER_MONITOR_AWARE_V2 on its
//! thread so mouse coordinates are unscaled desktop pixels regardless of
//! what process-level DPI awareness the host (Tauri) picked.
//!
//! Cancel paths → `Ok(None)` (mapped to `CaptureError::Cancelled`, CLI exit
//! 2): Esc, right-click, or a left-click without a drag (zero-size). The
//! whole selection is bounded by `capture::OVERLAY_SELECT_TIMEOUT`.
//!
//! Native window code cannot be unit-tested headlessly; the pure mapping
//! math it relies on (`constrained_drag`, `monitor_local_crop`) lives in
//! non-cfg-gated geometry.rs and IS unit tested on every platform.
//!
//! UIPI limitation: an overlay cannot draw above an elevated (admin)
//! window — Windows blocks lower-integrity processes from drawing over
//! higher ones. Documented, not defeated.

use std::ffi::c_void;
use std::time::Instant;

use windows::core::{BOOL, PCWSTR};
use windows::Win32::Foundation::{
    COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM,
};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetDC, InvalidateRect,
    MonitorFromWindow, ReleaseDC, SelectObject, SetBkMode, SetTextColor, TextOutW, AC_SRC_ALPHA,
    AC_SRC_OVER, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, BLENDFUNCTION, DIB_RGB_COLORS, HBITMAP, HDC,
    HGDIOBJ, HMONITOR, MONITOR_DEFAULTTONEAREST, TRANSPARENT,
};
use windows::Win32::UI::HiDpi::{
    GetDpiForMonitor, SetThreadDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
    MDT_EFFECTIVE_DPI,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetWindowLongPtrW,
    LoadCursorW, MsgWaitForMultipleObjectsEx, PeekMessageW, RegisterClassW, SetCursor,
    SetForegroundWindow, SetWindowLongPtrW, SetWindowPos, ShowWindow, TranslateMessage,
    UpdateLayeredWindow, GWLP_USERDATA, HWND_TOPMOST, IDC_CROSS, MSG, MWMO_INPUTAVAILABLE,
    PM_REMOVE, QS_ALLINPUT, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, ULW_ALPHA, WM_KEYDOWN,
    WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE, WM_RBUTTONUP, WM_SETCURSOR, WNDCLASSW,
    WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};

use crate::capture::OVERLAY_SELECT_TIMEOUT;
use crate::core::Failure;
use crate::geometry::{constrained_drag, monitor_local_crop, CaptureRect};

/// Dimming alpha for the overlay background (~47% black).
const DIM_ALPHA: u8 = 120;
/// Accent colour (repo identity: hacker green #00FF41), BGRA byte order.
const ACCENT: [u8; 4] = [0x41, 0xFF, 0x00, 0xFF]; // B, G, R, A
/// Readout background (near-black), BGRA.
const READOUT_BG: [u8; 4] = [0x28, 0x28, 0x28, 0xFF];
/// Readout box geometry (physical px).
const READOUT_H: i32 = 24;

/// Selection border thickness in physical pixels: 1 per 100% scale, rounded.
fn border_px(scale: f64) -> i32 {
    (scale.round() as i32).max(1)
}

/// One monitor's overlay inputs: physical rect, DPI scale, HMONITOR.
#[derive(Clone, Copy)]
struct MonEntry {
    hmon: HMONITOR,
    rect: CaptureRect,
    scale: f64,
}

/// Per-window render scratch: a DIB sized to the monitor, reused for every
/// layered update during the drag.
struct Scratch {
    mem_dc: HDC,
    dib: HBITMAP,
    old: HGDIOBJ,
    base: *mut u8,
    w: i32,
    h: i32,
}

impl Scratch {
    /// SAFETY: `mon_dc` must be a valid screen DC; dimensions > 0.
    unsafe fn new(mon_dc: HDC, w: i32, h: i32) -> Option<Self> {
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h, // top-down rows
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut base: *mut c_void = std::ptr::null_mut();
        // SAFETY: bmi outlives the call; base points into the DIB we own.
        let dib =
            unsafe { CreateDIBSection(Some(mon_dc), &bmi, DIB_RGB_COLORS, &mut base, None, 0) }
                .ok()?;
        if dib.is_invalid() || base.is_null() {
            return None;
        }
        // SAFETY: screen DC is compatible with this thread's DCs.
        let mem_dc: HDC = unsafe { CreateCompatibleDC(Some(mon_dc)) };
        if mem_dc.is_invalid() {
            // SAFETY: DIB created above and nothing selected into any DC.
            let _ = unsafe { DeleteObject(dib.into()) };
            return None;
        }
        // SAFETY: freshly created DC; selecting our DIB is safe.
        let old = unsafe { SelectObject(mem_dc, dib.into()) };
        Some(Scratch {
            mem_dc,
            dib,
            old,
            base: base as *mut u8,
            w,
            h,
        })
    }

    /// The DIB as a BGRA pixel slice.
    fn pixels(&mut self) -> &mut [[u8; 4]] {
        // SAFETY: base..base+w*h*4 is the DIB allocation (negative height
        // made it top-down); alive as long as `self`.
        unsafe {
            std::slice::from_raw_parts_mut(
                self.base as *mut [u8; 4],
                self.w as usize * self.h as usize,
            )
        }
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        // SAFETY: Win32 handle cleanup; each handle owned exclusively here.
        unsafe {
            let _ = SelectObject(self.mem_dc, self.old); // unselect DIB first
            let _ = DeleteDC(self.mem_dc);
            let _ = DeleteObject(self.dib.into());
        }
    }
}

enum Outcome {
    Selected(CaptureRect),
    Cancelled,
    TimedOut,
}

/// Shared state for the whole selection interaction (all overlay windows).
struct Shared {
    monitors: Vec<MonEntry>,
    scratch: Vec<Option<Scratch>>, // parallel to hwnds
    hwnds: Vec<HWND>,
    /// Drag in progress: (monitor index, logical start relative to monitor).
    drag: Option<(usize, (f64, f64))>,
    /// Current logical drag point, monitor-relative.
    cur: (f64, f64),
    done: Option<Outcome>,
}

impl Shared {
    fn monitor_index_of_hwnd(&self, hwnd: HWND) -> Option<usize> {
        // SAFETY: read-only monitor query.
        let hmon = unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) };
        self.monitors.iter().position(|m| m.hmon == hmon)
    }
}

fn class_name_wide() -> Vec<u16> {
    let mut v: Vec<u16> = "ClipOCRRegionOverlay".encode_utf16().collect();
    v.push(0);
    v
}

/// Extract signed (x, y) from a mouse-message LPARAM.
fn mouse_lparam(lp: LPARAM) -> (i32, i32) {
    let x = (lp.0 & 0xFFFF) as u16 as i16 as i32;
    let y = ((lp.0 >> 16) & 0xFFFF) as u16 as i16 as i32;
    (x, y)
}

/// SAFETY: `Shared` is only touched between `select_region_inner`'s setup
/// and teardown, on the same thread that runs the message pump below, so
/// the raw pointer is never shared across threads and is cleared before
/// `Shared` is dropped.
unsafe extern "system" fn overlay_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // SAFETY: USERDATA is set/cleared on this thread only (see above).
    let shared_ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut Shared;
    let shared = if shared_ptr.is_null() {
        None
    } else {
        // SAFETY: pointer valid for the duration of the pump.
        Some(unsafe { &mut *shared_ptr })
    };
    let Some(shared) = shared else {
        return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
    };
    match msg {
        WM_SETCURSOR => {
            // SAFETY: load-only cursor resource; a failed load falls back
            // to the default cursor rather than aborting the selection.
            if let Ok(cross) = unsafe { LoadCursorW(None, IDC_CROSS) } {
                unsafe { SetCursor(Some(cross)) };
            }
            LRESULT(1)
        }
        WM_LBUTTONDOWN => {
            let Some(idx) = shared.monitor_index_of_hwnd(hwnd) else {
                return LRESULT(0);
            };
            let scale = shared.monitors[idx].scale;
            let (px, py) = mouse_lparam(lparam);
            shared.drag = Some((idx, (f64::from(px) / scale, f64::from(py) / scale)));
            shared.cur = (f64::from(px) / scale, f64::from(py) / scale);
            // SAFETY: this window belongs to our thread.
            unsafe { SetCapture(hwnd) };
            redraw(shared, idx);
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            if let Some((idx, _)) = shared.drag {
                let scale = shared.monitors[idx].scale;
                let (px, py) = mouse_lparam(lparam);
                shared.cur = (f64::from(px) / scale, f64::from(py) / scale);
                redraw(shared, idx);
            }
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            if let Some((idx, start)) = shared.drag.take() {
                // SAFETY: paired with the SetCapture on button-down.
                let _ = unsafe { ReleaseCapture() };
                let mon = &shared.monitors[idx];
                shared.done = Some(
                    match constrained_drag(mon.rect, mon.scale, start, shared.cur) {
                        Some(rect) => Outcome::Selected(rect),
                        // Zero-size or fully-outside drag = plain click: cancel.
                        None => Outcome::Cancelled,
                    },
                );
            } else {
                shared.done = Some(Outcome::Cancelled);
            }
            LRESULT(0)
        }
        WM_RBUTTONUP => {
            shared.drag = None;
            shared.done = Some(Outcome::Cancelled);
            LRESULT(0)
        }
        WM_KEYDOWN => {
            // Esc. Best effort: only received when an overlay has keyboard
            // focus (select_region_inner raises the first one); right-click
            // is the always-available cancel.
            if wparam.0 as u16 == 0x1B {
                /* VK_ESCAPE */
                shared.drag = None;
                shared.done = Some(Outcome::Cancelled);
            }
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

/// Redraw `idx`'s overlay from the drag state and push it to the screen.
fn redraw(shared: &mut Shared, idx: usize) {
    let Some(Some(scratch)) = shared.scratch.get_mut(idx) else {
        return;
    };
    let mon = &shared.monitors[idx];
    let scale = mon.scale;
    let sel: Option<(i32, i32, i32, i32)> = match shared.drag {
        Some((di, start)) if di == idx => constrained_drag(mon.rect, mon.scale, start, shared.cur)
            .map(|r| {
                (
                    r.x - mon.rect.x,
                    r.y - mon.rect.y,
                    r.width as i32,
                    r.height as i32,
                )
            }),
        // No drag (initial dim frame) or drag on another monitor.
        _ => None,
    };
    draw_layer(scratch, scale, sel);
    let hwnd = shared.hwnds[idx];
    // SAFETY: window + DC owned by this thread; ULW_ALPHA pushes per-pixel
    // alpha from the DIB (position/size fixed at creation).
    let _ = unsafe {
        UpdateLayeredWindow(
            hwnd,
            None,
            Some(&POINT {
                x: mon.rect.x,
                y: mon.rect.y,
            }),
            Some(&SIZE {
                cx: mon.rect.width as i32,
                cy: mon.rect.height as i32,
            }),
            Some(scratch.mem_dc),
            Some(&POINT { x: 0, y: 0 }),
            COLORREF(0),
            Some(&BLENDFUNCTION {
                BlendOp: AC_SRC_OVER as u8,
                BlendFlags: 0,
                SourceConstantAlpha: 255,
                AlphaFormat: AC_SRC_ALPHA as u8,
            }),
            ULW_ALPHA,
        )
    };
}

/// Paint the DIB: dim everything, punch a transparent hole for the
/// selection, accent border, and a live `W × H` readout box.
fn draw_layer(scratch: &mut Scratch, scale: f64, sel: Option<(i32, i32, i32, i32)>) {
    // Copy the DC + dims BEFORE the mutable pixel borrow below (HDC and
    // i32 are Copy); the GDI text pass then needs no re-borrow.
    let dc = scratch.mem_dc;
    let mon_w = scratch.w;
    let mon_h = scratch.h;
    let w = scratch.w as usize;
    let h = scratch.h as usize;
    let px = scratch.pixels();
    // ~47% black dim everywhere first (single fill pass).
    px.fill([0, 0, 0, DIM_ALPHA]);
    let Some((sx, sy, sw, sh)) = sel else {
        return;
    };
    if sw <= 0 || sh <= 0 {
        return; // degenerate: dim only
    }
    let (x0, x1) = (sx.max(0) as usize, (sx + sw).min(w as i32).max(0) as usize);
    let (y0, y1) = (sy.max(0) as usize, (sy + sh).min(h as i32).max(0) as usize);
    if x1 <= x0 || y1 <= y0 {
        return;
    }
    // Transparent hole (selection interior).
    for y in y0..y1 {
        let row = &mut px[y * w..(y + 1) * w];
        row[x0..x1].fill([0, 0, 0, 0]);
    }
    // Opaque accent border strips around the hole.
    let b = border_px(scale) as usize;
    let strips: [(usize, usize, usize, usize); 4] = [
        (x0, y0, x1, (y0 + b).min(h)),      // top
        (x0, y1.saturating_sub(b), x1, y1), // bottom
        (x0, y0, (x0 + b).min(w), y1),      // left
        (x1.saturating_sub(b), y0, x1, y1), // right
    ];
    for (rx0, ry0, rx1, ry1) in strips {
        for y in ry0..ry1 {
            for x in rx0..rx1 {
                px[y * w + x] = ACCENT;
            }
        }
    }
    // Live `W × H` readout box near the selection corner, kept on-monitor.
    let label = format!("{sw} × {sh}");
    let box_w = estimate_text_w(&label) + 16;
    let bx = (sx + sw + 8).min(mon_w - box_w - 4).max(4);
    let by = (sy + sh + 8).min(mon_h - READOUT_H - 4).max(4);
    let (bx0, bx1) = (
        bx.max(0) as usize,
        (bx + box_w).min(w as i32).max(0) as usize,
    );
    let (by0, by1) = (
        by.max(0) as usize,
        (by + READOUT_H).min(h as i32).max(0) as usize,
    );
    if bx1 <= bx0 || by1 <= by0 {
        return;
    }
    for y in by0..by1 {
        for x in bx0..bx1 {
            px[y * w + x] = READOUT_BG;
        }
    }
    // Text: GDI into the same DIB via its DC. GDI leaves alpha=0 in the
    // bytes it writes, so the box rows are forced opaque AFTER drawing.
    let wide: Vec<u16> = label.encode_utf16().collect();
    // SAFETY: DC is our scratch DC; the DIB is selected into it.
    unsafe {
        let _ = SetBkMode(dc, TRANSPARENT);
        // COLORREF is 0x00BBGGRR.
        let _ = SetTextColor(
            dc,
            COLORREF((ACCENT[2] as u32) << 16 | (ACCENT[1] as u32) << 8 | ACCENT[0] as u32),
        );
        let _ = TextOutW(dc, bx0 as i32 + 8, by0 as i32 + 4, &wide);
    }
    for y in by0..by1 {
        for x in bx0..bx1 {
            px[y * w + x][3] = 255;
        }
    }
}

/// Rough label width for the readout (system font ≈ 8 px per char).
fn estimate_text_w(s: &str) -> i32 {
    8 * s.chars().count() as i32
}

/// Enumerate physical monitor rects via `EnumDisplayMonitors` +
/// `GetMonitorInfoW`. Rects are desktop PHYSICAL pixels and may have
/// negative origins.
pub fn enumerate_monitors() -> Result<Vec<(HMONITOR, CaptureRect)>, Failure> {
    extern "system" fn enum_proc(hmon: HMONITOR, _: HDC, rect: *mut RECT, lparam: LPARAM) -> BOOL {
        // SAFETY: lparam is the Vec pointer we passed; rect is the caller's.
        let out = unsafe { &mut *(lparam.0 as *mut Vec<(HMONITOR, CaptureRect)>) };
        let r = unsafe { *rect };
        out.push((
            hmon,
            CaptureRect {
                x: r.left,
                y: r.top,
                width: (r.right - r.left).max(0) as u32,
                height: (r.bottom - r.top).max(0) as u32,
            },
        ));
        BOOL(1)
    }
    let mut out: Vec<(HMONITOR, CaptureRect)> = Vec::new();
    // SAFETY: callback + lparam point at our Vec for the call's duration.
    let ok = unsafe {
        windows::Win32::Graphics::Gdi::EnumDisplayMonitors(
            None,
            None,
            Some(enum_proc),
            LPARAM(&mut out as *mut _ as isize),
        )
    };
    if !ok.as_bool() || out.is_empty() {
        return Err(Failure::Capture);
    }
    Ok(out)
}

fn monitor_scale(hmon: HMONITOR) -> f64 {
    let mut dpi_x = 0u32;
    let mut dpi_y = 0u32;
    // SAFETY: out-params are valid; hmon from enumeration.
    let ok = unsafe { GetDpiForMonitor(hmon, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y) };
    if ok.is_ok() && dpi_x > 0 {
        f64::from(dpi_x) / 96.0
    } else {
        1.0 // pre-1607 fallback: treat as 100%
    }
}

/// The Win32 region selector: implements the geometry.rs platform seam.
pub struct Win32RegionSelector;

impl Win32RegionSelector {
    /// Show per-monitor overlays and run the modal drag interaction.
    /// Returns `Ok(Some(rect))` in desktop physical pixels, `Ok(None)` on
    /// cancel (Esc / right-click / click without drag), `Err(Timeout)` when
    /// [`OVERLAY_SELECT_TIMEOUT`] elapses. Synchronous by design: it pumps
    /// messages; callers run it on a blocking thread, matching the existing
    /// picker flow.
    pub fn select_region(&self) -> Result<Option<CaptureRect>, Failure> {
        // Force physical-pixel coordinates on this thread for the whole
        // interaction, regardless of the host process's DPI awareness.
        // SAFETY: thread-local context switch, restored right after.
        let old =
            unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
        let result = self.select_region_inner();
        if !old.is_invalid() {
            // SAFETY: restore the previously-current context.
            unsafe { SetThreadDpiAwarenessContext(old) };
        }
        result
    }

    fn select_region_inner(&self) -> Result<Option<CaptureRect>, Failure> {
        let monitors: Vec<MonEntry> = enumerate_monitors()?
            .into_iter()
            .map(|(hmon, rect)| MonEntry {
                hmon,
                rect,
                scale: monitor_scale(hmon),
            })
            .collect();

        // SAFETY: module handle of our own process.
        let hinstance =
            // SAFETY: module handle of our own process.
            unsafe { windows::Win32::System::LibraryLoader::GetModuleHandleW(None) }
            .map_err(|_| Failure::Capture)?;
        let cname = class_name_wide();
        let wc = WNDCLASSW {
            lpfnWndProc: Some(overlay_wndproc),
            hInstance: HINSTANCE(hinstance.0),
            lpszClassName: PCWSTR(cname.as_ptr()),
            ..Default::default()
        };
        // SAFETY: wc is fully initialised above.
        let atom = unsafe { RegisterClassW(&wc) };
        if atom == 0 {
            return Err(Failure::Capture);
        }

        let mut shared = Shared {
            monitors,
            scratch: Vec::new(),
            hwnds: Vec::new(),
            drag: None,
            cur: (0.0, 0.0),
            done: None,
        };

        // One topmost layered tool window per monitor + its render scratch.
        // SAFETY: screen DC for DIB creation.
        let screen_dc = unsafe { GetDC(None) };
        for idx in 0..shared.monitors.len() {
            let mon = shared.monitors[idx];
            // SAFETY: class registered above; ex-style/tool/topmost keeps
            // the overlay off the taskbar and above normal windows.
            let hwnd = unsafe {
                CreateWindowExW(
                    WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                    PCWSTR(cname.as_ptr()),
                    None,
                    WS_POPUP,
                    mon.rect.x,
                    mon.rect.y,
                    mon.rect.width as i32,
                    mon.rect.height as i32,
                    None,
                    None,
                    Some(HINSTANCE(hinstance.0)),
                    None,
                )
            }
            .map_err(|_| Failure::Capture)?;
            // SAFETY: our window, this thread.
            unsafe {
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, &mut shared as *mut Shared as isize);
                let _ = SetWindowPos(
                    hwnd,
                    Some(HWND_TOPMOST),
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                );
                let _ = ShowWindow(
                    hwnd,
                    windows::Win32::UI::WindowsAndMessaging::SW_SHOWNOACTIVATE,
                );
            }
            let scratch =
                // SAFETY: screen DC valid; monitor dimensions > 0.
                unsafe { Scratch::new(screen_dc, mon.rect.width as i32, mon.rect.height as i32) };
            shared.hwnds.push(hwnd);
            shared.scratch.push(scratch);
        }
        // SAFETY: paired with GetDC above.
        let _ = unsafe { ReleaseDC(None, screen_dc) };

        // Initial dim frame on every overlay.
        for idx in 0..shared.hwnds.len() {
            redraw(&mut shared, idx);
        }
        // Raise the first overlay so Esc has a shot at reaching us (best
        // effort: foreground activation may be denied; right-click always
        // cancels).
        if let Some(&first) = shared.hwnds.first() {
            // SAFETY: our window.
            let _ = unsafe { SetForegroundWindow(first) };
        }

        // Message pump, bounded by the deadline. MsgWait + Peek avoids both
        // a busy loop and the GetMessage-cannot-be-unblocked problem.
        let deadline = Instant::now() + OVERLAY_SELECT_TIMEOUT;
        let mut msg = MSG::default();
        while shared.done.is_none() {
            if Instant::now() >= deadline {
                shared.done = Some(Outcome::TimedOut);
                break;
            }
            // SAFETY: no handles; wake on any input for this thread.
            unsafe {
                MsgWaitForMultipleObjectsEx(Some(&[]), 100, QS_ALLINPUT, MWMO_INPUTAVAILABLE);
            }
            // SAFETY: msg buffer valid for the call.
            while unsafe { PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE) }.as_bool() {
                // SAFETY: standard translate/dispatch of a removed message.
                unsafe {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
                if shared.done.is_some() {
                    break;
                }
            }
        }

        // Result BEFORE dropping Shared (the wndproc reads it until destroy).
        let outcome = shared.done.take();

        // Destroy every overlay and pump until the destroys settle, so no
        // overlay pixel can land in the capture. USERDATA is cleared per
        // window BEFORE Shared goes out of scope.
        for &hwnd in &shared.hwnds {
            // SAFETY: our window on this thread.
            unsafe {
                let _ = DestroyWindow(hwnd);
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                let _ = InvalidateRect(Some(hwnd), None, true);
            }
        }
        let settle_by = Instant::now() + std::time::Duration::from_secs(1);
        while Instant::now() < settle_by {
            // SAFETY: msg buffer valid.
            while unsafe { PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE) }.as_bool() {
                // SAFETY: standard translate/dispatch.
                unsafe {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        // Let the compositor retire the last overlay frame before capture.
        std::thread::sleep(crate::capture::OVERLAY_HIDE_GRACE);

        match outcome {
            Some(Outcome::Selected(rect)) => {
                // Defensive double-check against the tested pure mapping: a
                // clamped selection must be fully inside one monitor. Fail
                // loudly rather than crop a wrong region.
                let rects: Vec<CaptureRect> = shared.monitors.iter().map(|m| m.rect).collect();
                monitor_local_crop(&rects, rect)
                    .map(|_| Some(rect))
                    .ok_or(Failure::Capture)
            }
            Some(Outcome::Cancelled) => Ok(None),
            Some(Outcome::TimedOut) | None => Err(Failure::Timeout),
        }
    }
}

#[async_trait::async_trait]
impl crate::geometry::OverlaySelector for Win32RegionSelector {
    /// Async seam (see geometry.rs). The Windows capture path calls the
    /// concrete [`Win32RegionSelector::select_region`] from a blocking
    /// thread; this impl keeps the trait wired for async callers.
    async fn select(&self, monitors: &[CaptureRect]) -> Result<Option<CaptureRect>, Failure> {
        let _ = monitors; // re-enumerated inside for HMONITOR identity/DPI
        tokio::task::spawn_blocking(|| Win32RegionSelector.select_region())
            .await
            .map_err(|_| Failure::Capture)?
    }
}
