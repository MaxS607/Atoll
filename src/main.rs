#![windows_subsystem = "windows"]

//! Atoll – minimale "Dynamic Island" für Windows.
//! - schwarze Pille oben mittig, immer im Vordergrund
//! - Hover: klappt animiert auf (Uhrzeit + Datum)
//! - Rechtsklick: beenden

use std::sync::atomic::{AtomicBool, AtomicI32, AtomicI64, Ordering};

use windows::core::w;
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::SystemInformation::{GetLocalTime, GetTickCount64};
use windows::Win32::UI::HiDpi::GetDpiForSystem;
use windows::Win32::UI::Input::KeyboardAndMouse::{TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT};
use windows::Win32::UI::WindowsAndMessaging::*;

// Größen in logischen Pixeln (bei 96 DPI)
const COLLAPSED_W: i32 = 180;
const COLLAPSED_H: i32 = 36;
const EXPANDED_W: i32 = 380;
const EXPANDED_H: i32 = 100;
const TOP_MARGIN: i32 = 8;

const TIMER_ID: usize = 1;
const WM_MOUSELEAVE: u32 = 0x02A3;
const EXPAND_DURATION_MS: u64 = 320;
const COLLAPSE_DURATION_MS: u64 = 260;

static EXPANDED: AtomicBool = AtomicBool::new(false);
static TRACKING: AtomicBool = AtomicBool::new(false);
static LAST_SEC: AtomicI32 = AtomicI32::new(-1);
static SCALE_PCT: AtomicI32 = AtomicI32::new(100);
static ANIMATION_FROM: AtomicI32 = AtomicI32::new(0);
static ANIMATION_STARTED: AtomicI64 = AtomicI64::new(0);
// The timer publishes one state for both native geometry and paint.  Sampling
// the clock independently in WM_PAINT can otherwise draw a frame ahead of the
// resized surface.
static RENDER_PROGRESS: AtomicI32 = AtomicI32::new(0);
static CUR_W: AtomicI32 = AtomicI32::new(0);
static CUR_H: AtomicI32 = AtomicI32::new(0);

fn scaled(v: i32) -> i32 {
    v * SCALE_PCT.load(Ordering::Relaxed) / 100
}

fn main() -> windows::core::Result<()> {
    unsafe {
        let _ = SetProcessDPIAware();
        SCALE_PCT.store((GetDpiForSystem() * 100 / 96) as i32, Ordering::Relaxed);

        let hinstance = GetModuleHandleW(None)?;
        let class_name = w!("AtollClass");

        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: hinstance.into(),
            lpszClassName: class_name,
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            ..Default::default()
        };
        RegisterClassW(&wc);

        CUR_W.store(scaled(COLLAPSED_W), Ordering::Relaxed);
        CUR_H.store(scaled(COLLAPSED_H), Ordering::Relaxed);

        let hwnd = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            class_name,
            w!("Atoll"),
            WS_POPUP,
            0,
            0,
            scaled(COLLAPSED_W),
            scaled(COLLAPSED_H),
            None,
            None,
            hinstance,
            None,
        )?;

        apply_geometry(hwnd, scaled(COLLAPSED_W), scaled(COLLAPSED_H));
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        SetTimer(hwnd, TIMER_ID, 16, None);

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    Ok(())
}

/// Position (oben mittig) und abgerundete Form setzen.
unsafe fn apply_geometry(hwnd: HWND, w: i32, h: i32) {
    let screen_w = GetSystemMetrics(SM_CXSCREEN);
    let x = (screen_w - w) / 2;
    let y = scaled(TOP_MARGIN);
    let _ = SetWindowPos(
        hwnd,
        HWND_TOPMOST,
        x,
        y,
        w,
        h,
        SWP_NOACTIVATE | SWP_NOSENDCHANGING,
    );
    let rgn = CreateRoundRectRgn(0, 0, w + 1, h + 1, h.min(scaled(44)), h.min(scaled(44)));
    // Windows übernimmt das Region-Handle
    let _ = SetWindowRgn(hwnd, rgn, true);
}

/// A quintic ease has zero velocity at both ends, avoiding a mechanical stop.
fn smooth_step(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    t * t * t * (t * (t * 6.0 - 15.0) + 10.0)
}

fn animation_progress(now: u64) -> f32 {
    let from = ANIMATION_FROM.load(Ordering::Relaxed) as f32 / 10_000.0;
    let expanding = EXPANDED.load(Ordering::Relaxed);
    let started = ANIMATION_STARTED.load(Ordering::Relaxed).max(0) as u64;
    let duration = if expanding {
        EXPAND_DURATION_MS
    } else {
        COLLAPSE_DURATION_MS
    };
    let eased = smooth_step(now.saturating_sub(started) as f32 / duration as f32);

    if expanding {
        from + (1.0 - from) * eased
    } else {
        from * (1.0 - eased)
    }
}

/// Retarget from the current in-between value, so edge crossings never snap.
unsafe fn set_expanded(expanded: bool) {
    if EXPANDED.load(Ordering::Relaxed) == expanded {
        return;
    }
    let now = GetTickCount64();
    let current = animation_progress(now);
    ANIMATION_FROM.store((current * 10_000.0).round() as i32, Ordering::Relaxed);
    ANIMATION_STARTED.store(now as i64, Ordering::Relaxed);
    EXPANDED.store(expanded, Ordering::Relaxed);
}

fn lerp_i32(from: i32, to: i32, progress: f32) -> i32 {
    (from as f32 + (to - from) as f32 * progress).round() as i32
}

fn lerp_color(from: u8, to: u8, progress: f32) -> u8 {
    lerp_i32(from as i32, to as i32, progress) as u8
}

unsafe fn draw_text(hdc: HDC, text: &str, rect: &mut RECT, height: i32, weight: i32) {
    let font = CreateFontW(
        -height,
        0,
        0,
        0,
        weight,
        0,
        0,
        0,
        DEFAULT_CHARSET.0 as u32,
        OUT_DEFAULT_PRECIS.0 as u32,
        CLIP_DEFAULT_PRECIS.0 as u32,
        CLEARTYPE_QUALITY.0 as u32,
        0,
        w!("Segoe UI"),
    );
    let old = SelectObject(hdc, font);
    let mut wide: Vec<u16> = text.encode_utf16().collect();
    DrawTextW(hdc, &mut wide, rect, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
    SelectObject(hdc, old);
    let _ = DeleteObject(font);
}

unsafe fn paint(hwnd: HWND) {
    let mut ps = PAINTSTRUCT::default();
    let hdc = BeginPaint(hwnd, &mut ps);

    let mut rc = RECT::default();
    let _ = GetClientRect(hwnd, &mut rc);
    let (w, h) = (rc.right - rc.left, rc.bottom - rc.top);

    // Double Buffering gegen Flackern
    let mem = CreateCompatibleDC(hdc);
    let bmp = CreateCompatibleBitmap(hdc, w, h);
    let old_bmp = SelectObject(mem, bmp);

    let brush = CreateSolidBrush(COLORREF(0x0000_0000));
    FillRect(mem, &rc, brush);
    let _ = DeleteObject(brush);

    SetBkMode(mem, TRANSPARENT);
    SetTextColor(mem, COLORREF(0x00FF_FFFF));

    let t = GetLocalTime();
    let time = format!("{:02}:{:02}", t.wHour, t.wMinute);

    let progress = RENDER_PROGRESS.load(Ordering::Relaxed) as f32 / 10_000.0;

    let mut time_rect = RECT {
        left: 0,
        top: lerp_i32(0, h / 10, progress),
        right: w,
        bottom: lerp_i32(h, h * 6 / 10, progress),
    };
    draw_text(
        mem,
        &time,
        &mut time_rect,
        lerp_i32(h / 2, h * 40 / 100, progress),
        600,
    );

    const DAYS: [&str; 7] = ["So", "Mo", "Di", "Mi", "Do", "Fr", "Sa"];
    let date = format!(
        "{} {:02}.{:02}.{}",
        DAYS[(t.wDayOfWeek as usize) % 7],
        t.wDay,
        t.wMonth,
        t.wYear
    );
    let date_fade = smooth_step(((progress - 0.28) / 0.72).clamp(0.0, 1.0));
    if date_fade > 0.0 {
        let mut date_rect = RECT {
            left: 0,
            top: h * 6 / 10,
            right: w,
            bottom: h - h / 10,
        };
        let grey = lerp_color(0, 180, date_fade) as u32;
        SetTextColor(mem, COLORREF(grey | (grey << 8) | (grey << 16)));
        draw_text(mem, &date, &mut date_rect, h * 18 / 100, 400);
    }

    let _ = BitBlt(hdc, 0, 0, w, h, mem, 0, 0, SRCCOPY);

    SelectObject(mem, old_bmp);
    let _ = DeleteObject(bmp);
    let _ = DeleteDC(mem);
    let _ = EndPaint(hwnd, &ps);
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_TIMER => {
                let progress = animation_progress(GetTickCount64());
                RENDER_PROGRESS.store((progress * 10_000.0).round() as i32, Ordering::Relaxed);
                let nw = lerp_i32(scaled(COLLAPSED_W), scaled(EXPANDED_W), progress);
                let nh = lerp_i32(scaled(COLLAPSED_H), scaled(EXPANDED_H), progress);
                let target = if EXPANDED.load(Ordering::Relaxed) {
                    1.0
                } else {
                    0.0
                };
                let animating = (progress - target).abs() > 0.0001;
                let cw = CUR_W.load(Ordering::Relaxed);
                let ch = CUR_H.load(Ordering::Relaxed);

                let sec = GetLocalTime().wMinute as i32 * 100 + GetLocalTime().wSecond as i32;
                let sec_changed = LAST_SEC.swap(sec, Ordering::Relaxed) != sec;

                if nw != cw || nh != ch {
                    apply_geometry(hwnd, nw, nh);
                    CUR_W.store(nw, Ordering::Relaxed);
                    CUR_H.store(nh, Ordering::Relaxed);
                    let _ = InvalidateRect(hwnd, None, false);
                } else if sec_changed || animating {
                    let _ = InvalidateRect(hwnd, None, false);
                }
                LRESULT(0)
            }
            WM_MOUSEMOVE => {
                set_expanded(true);
                if !TRACKING.swap(true, Ordering::Relaxed) {
                    let mut tme = TRACKMOUSEEVENT {
                        cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                        dwFlags: TME_LEAVE,
                        hwndTrack: hwnd,
                        dwHoverTime: 0,
                    };
                    let _ = TrackMouseEvent(&mut tme);
                }
                LRESULT(0)
            }
            WM_MOUSELEAVE => {
                TRACKING.store(false, Ordering::Relaxed);
                set_expanded(false);
                LRESULT(0)
            }
            WM_RBUTTONUP => {
                let _ = DestroyWindow(hwnd);
                LRESULT(0)
            }
            WM_ERASEBKGND => LRESULT(1),
            WM_PAINT => {
                paint(hwnd);
                LRESULT(0)
            }
            WM_DESTROY => {
                PostQuitMessage(0);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}
