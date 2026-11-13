#![windows_subsystem = "windows"]

//! Atoll: a smooth "dynamic island" clock for Windows.
//!
//! How it differs from the GDI-region version:
//!  * The window NEVER moves or resizes. It is a fixed-size layered window
//!    (per-pixel alpha) and only the pill drawn inside it changes size.
//!  * The pill is rasterised with a signed-distance function using float sizes,
//!    so edges and corners are anti-aliased and grow in sub-pixel steps.
//!  * Text is rendered once into 4x supersampled masks and box-filtered down at
//!    a sub-pixel vertical offset each frame, so it glides instead of jumping.
//!  * Time comes from QueryPerformanceCounter, frames are paced by DwmFlush
//!    (the compositor's vblank), and the motion is a critically damped spring
//!    that can be interrupted at any moment without snapping.
//!  * One progress value drives geometry and date fade in every frame; the clock
//!    remains anchored while the pill expands.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};

use windows::core::w;
use windows::Win32::Foundation::{
    COLORREF, HANDLE, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM,
};
use windows::Win32::Graphics::Dwm::DwmFlush;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows::Win32::System::Threading::Sleep;
use windows::Win32::UI::HiDpi::GetDpiForSystem;
use windows::Win32::UI::Input::KeyboardAndMouse::{TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT};
use windows::Win32::UI::WindowsAndMessaging::*;

// ---- Layout (logical pixels at 96 DPI) -------------------------------------
const COLLAPSED_W: f32 = 180.0;
const COLLAPSED_H: f32 = 36.0;
const EXPANDED_W: f32 = 380.0;
const EXPANDED_H: f32 = 100.0;
const TOP_MARGIN: f32 = 8.0;
const MAX_RADIUS: f32 = 22.0;

const TIME_FONT: f32 = 18.0;
const DATE_FONT: f32 = 14.0;
const TIME_BOX_H: f32 = 28.0;
const DATE_BOX_H: f32 = 22.0;
const TIME_TOP_COLLAPSED: f32 = (COLLAPSED_H - TIME_BOX_H) / 2.0;
const DATE_TOP_EXPANDED: f32 = 58.0;
const DATE_SLIDE: f32 = 8.0; // date slides up this far while fading in

// ---- Motion ----------------------------------------------------------------
// Critically damped spring: no overshoot, no hard stop, fully interruptible.
// Higher OMEGA = faster/snappier, lower = softer. ~20 settles in about 0.3 s.
const OMEGA: f32 = 20.0;

// Text supersampling factor (mask is rendered at SS x size, then box-filtered).
const SS: i32 = 4;

const WM_MOUSELEAVE: u32 = 0x02A3;

static TARGET_EXPANDED: AtomicBool = AtomicBool::new(false);
static TRACKING: AtomicBool = AtomicBool::new(false);

fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

/// Quintic smoothstep (zero velocity and acceleration at both ends).
fn smooth_step(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    t * t * t * (t * (t * 6.0 - 15.0) + 10.0)
}

fn make_bmi(w: i32, h: i32) -> BITMAPINFO {
    BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w,
            biHeight: -h, // top-down
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    }
}

/// Render `text` white-on-black with grayscale AA into a hi-res mask (0..255 coverage).
unsafe fn render_mask(text: &str, font_px: i32, weight: i32, hw: i32, hh: i32) -> Vec<u8> {
    let screen = GetDC(None);
    let dc = CreateCompatibleDC(screen);
    let bmi = make_bmi(hw, hh);
    let mut bits: *mut c_void = std::ptr::null_mut();
    let bmp = CreateDIBSection(dc, &bmi, DIB_RGB_COLORS, &mut bits, HANDLE::default(), 0)
        .expect("CreateDIBSection (mask)");
    let old_bmp = SelectObject(dc, bmp);

    let font = CreateFontW(
        -font_px,
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
        ANTIALIASED_QUALITY.0 as u32, // grayscale AA: ClearType colour fringes don't survive resampling
        0,
        w!("Segoe UI"),
    );
    let old_font = SelectObject(dc, font);

    SetBkMode(dc, TRANSPARENT);
    SetTextColor(dc, COLORREF(0x00FF_FFFF));
    let mut rect = RECT {
        left: 0,
        top: 0,
        right: hw,
        bottom: hh,
    };
    let mut wide: Vec<u16> = text.encode_utf16().collect();
    DrawTextW(
        dc,
        &mut wide,
        &mut rect,
        DT_CENTER | DT_VCENTER | DT_SINGLELINE,
    );
    let _ = GdiFlush();

    let px = std::slice::from_raw_parts(bits as *const u32, (hw * hh) as usize);
    let mask: Vec<u8> = px.iter().map(|p| (p & 0xFF) as u8).collect();

    SelectObject(dc, old_font);
    let _ = DeleteObject(font);
    SelectObject(dc, old_bmp);
    let _ = DeleteObject(bmp);
    let _ = DeleteDC(dc);
    ReleaseDC(None, screen);
    mask
}

/// Box-filter one destination pixel from a hi-res mask, shifted down by `dy_hi` hi-res rows.
fn sample(mask: &[u8], hw: i32, hh: i32, x: i32, y: i32, dy_hi: i32) -> f32 {
    let mut sum = 0u32;
    for k in 0..SS {
        let r = y * SS + k - dy_hi;
        if r < 0 || r >= hh {
            continue;
        }
        let row = (r * hw + x * SS) as usize;
        for j in 0..SS as usize {
            sum += mask[row + j] as u32;
        }
    }
    sum as f32 / (SS * SS * 255) as f32
}

struct Renderer {
    w: i32,
    h: i32,
    s: f32,
    mem_dc: HDC,
    bits: *mut u32,
    time_mask: Vec<u8>,
    date_mask: Vec<u8>,
    time_hh: i32,
    date_hh: i32,
    key: u32,
}

impl Renderer {
    unsafe fn new(w: i32, h: i32, s: f32) -> Self {
        let screen = GetDC(None);
        let mem_dc = CreateCompatibleDC(screen);
        let bmi = make_bmi(w, h);
        let mut bits: *mut c_void = std::ptr::null_mut();
        let bmp = CreateDIBSection(
            mem_dc,
            &bmi,
            DIB_RGB_COLORS,
            &mut bits,
            HANDLE::default(),
            0,
        )
        .expect("CreateDIBSection (frame)");
        SelectObject(mem_dc, bmp);
        ReleaseDC(None, screen);
        Renderer {
            w,
            h,
            s,
            mem_dc,
            bits: bits as *mut u32,
            time_mask: Vec::new(),
            date_mask: Vec::new(),
            time_hh: (TIME_BOX_H * s).round() as i32 * SS,
            date_hh: (DATE_BOX_H * s).round() as i32 * SS,
            key: u32::MAX,
        }
    }

    /// Re-render the text masks only when the visible text changes.
    unsafe fn update_text(&mut self) {
        let t = GetLocalTime();
        let key = ((((t.wYear as u32 * 13 + t.wMonth as u32) * 32 + t.wDay as u32) * 24
            + t.wHour as u32)
            * 60)
            + t.wMinute as u32;
        if key == self.key {
            return;
        }
        self.key = key;

        const DAYS: [&str; 7] = ["So", "Mo", "Di", "Mi", "Do", "Fr", "Sa"];
        let time = format!("{:02}:{:02}", t.wHour, t.wMinute);
        let date = format!(
            "{} {:02}.{:02}.{}",
            DAYS[(t.wDayOfWeek as usize) % 7],
            t.wDay,
            t.wMonth,
            t.wYear
        );
        let hw = self.w * SS;
        self.time_mask = render_mask(
            &time,
            (TIME_FONT * self.s * SS as f32).round() as i32,
            600,
            hw,
            self.time_hh,
        );
        self.date_mask = render_mask(
            &date,
            (DATE_FONT * self.s * SS as f32).round() as i32,
            400,
            hw,
            self.date_hh,
        );
    }

    /// Draw one frame for animation progress `p` (0 = collapsed, 1 = expanded).
    unsafe fn draw(&mut self, hwnd: HWND, p: f32) {
        self.update_text();

        let (w, h, s) = (self.w, self.h, self.s);
        let buf = std::slice::from_raw_parts_mut(self.bits, (w * h) as usize);
        buf.fill(0);

        // Pill geometry (floats; this is what makes the motion sub-pixel smooth).
        let pw = lerp(COLLAPSED_W, EXPANDED_W, p) * s;
        let ph = lerp(COLLAPSED_H, EXPANDED_H, p) * s;
        let rad = (ph / 2.0).min(MAX_RADIUS * s);
        let cx = w as f32 / 2.0;
        let cy = ph / 2.0;
        let (hx, hy) = (pw / 2.0 - rad, ph / 2.0 - rad);

        // Keep the clock anchored at its collapsed position. Only the date
        // animates into view as the pill expands.
        let fade = smooth_step((p - 0.28) / 0.72);
        let time_top = TIME_TOP_COLLAPSED * s;
        let date_top = (DATE_TOP_EXPANDED + (1.0 - fade) * DATE_SLIDE) * s;
        let time_dy = (time_top * SS as f32).round() as i32;
        let date_dy = (date_top * SS as f32).round() as i32;
        let time_rows = (
            time_top.floor() as i32 - 1,
            time_top.ceil() as i32 + self.time_hh / SS + 1,
        );
        let date_rows = (
            date_top.floor() as i32 - 1,
            date_top.ceil() as i32 + self.date_hh / SS + 1,
        );
        let date_grey = 180.0 * fade;

        let x0 = ((cx - pw / 2.0).floor() as i32 - 1).max(0);
        let x1 = ((cx + pw / 2.0).ceil() as i32 + 1).min(w - 1);
        let y1 = (ph.ceil() as i32 + 1).min(h - 1);
        let hw = w * SS;

        for y in 0..=y1 {
            let in_time = y >= time_rows.0 && y <= time_rows.1;
            let in_date = fade > 0.0 && y >= date_rows.0 && y <= date_rows.1;
            let fy = y as f32 + 0.5;
            for x in x0..=x1 {
                let fx = x as f32 + 0.5;

                // Rounded-rect signed distance -> anti-aliased coverage.
                let qx = (fx - cx).abs() - hx;
                let qy = (fy - cy).abs() - hy;
                let d = qx.max(0.0).hypot(qy.max(0.0)) + qx.max(qy).min(0.0) - rad;
                let cov = (0.5 - d).clamp(0.0, 1.0);
                if cov <= 0.0 {
                    continue;
                }

                let mut c = 0.0f32;
                if in_time {
                    c += 255.0 * sample(&self.time_mask, hw, self.time_hh, x, y, time_dy);
                }
                if in_date {
                    c += date_grey * sample(&self.date_mask, hw, self.date_hh, x, y, date_dy);
                }
                let v = (c.min(255.0) * cov + 0.5) as u32;
                let a = (cov * 255.0 + 0.5) as u32;
                buf[(y * w + x) as usize] = (a << 24) | (v << 16) | (v << 8) | v;
                // premultiplied
            }
        }

        let size = SIZE { cx: w, cy: h };
        let src = POINT { x: 0, y: 0 };
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: 255,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };
        let _ = UpdateLayeredWindow(
            hwnd,
            HDC::default(),
            None,
            Some(&size),
            self.mem_dc,
            Some(&src),
            COLORREF(0),
            Some(&blend),
            ULW_ALPHA,
        );
    }
}

fn qpc() -> i64 {
    let mut v = 0i64;
    unsafe {
        let _ = QueryPerformanceCounter(&mut v);
    }
    v
}

fn main() -> windows::core::Result<()> {
    unsafe {
        let _ = SetProcessDPIAware();
        let s = GetDpiForSystem() as f32 / 96.0;

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

        // Fixed window size = the expanded size. The window is never moved or resized again.
        let win_w = (EXPANDED_W * s).ceil() as i32 + 2;
        let win_h = (EXPANDED_H * s).ceil() as i32 + 2;
        let x = (GetSystemMetrics(SM_CXSCREEN) - win_w) / 2;
        let y = (TOP_MARGIN * s).round() as i32;

        let hwnd = CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            class_name,
            w!("Atoll"),
            WS_POPUP,
            x,
            y,
            win_w,
            win_h,
            None,
            None,
            hinstance,
            None,
        )?;

        let mut r = Renderer::new(win_w, win_h, s);
        r.draw(hwnd, 0.0); // layered windows show nothing until the first UpdateLayeredWindow
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);

        let mut freq = 0i64;
        let _ = QueryPerformanceFrequency(&mut freq);
        let freq = freq.max(1) as f64;

        // Spring state: position (progress) and velocity.
        let mut pos = 0.0f32;
        let mut vel = 0.0f32;
        let mut last = qpc();
        let mut was_animating = false;
        let initial_time = GetLocalTime();
        let mut last_minute_key = initial_time.wHour as u32 * 60 + initial_time.wMinute as u32;

        let mut msg = MSG::default();
        loop {
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                if msg.message == WM_QUIT {
                    return Ok(());
                }
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }

            let now = qpc();
            let target = if TARGET_EXPANDED.load(Ordering::Relaxed) {
                1.0f32
            } else {
                0.0
            };
            let mut dt = ((now - last) as f64 / freq) as f32;
            last = now;
            // First frame after idle: don't integrate a huge gap.
            dt = if was_animating {
                dt.clamp(0.0, 1.0 / 20.0)
            } else {
                1.0 / 120.0
            };

            let unsettled = (target - pos).abs() > 5e-4 || vel.abs() > 5e-3;
            if unsettled {
                // Fixed small substeps keep the integration stable and frame-rate independent.
                let n = ((dt * 240.0).ceil() as i32).max(1);
                let hstep = dt / n as f32;
                for _ in 0..n {
                    let acc = OMEGA * OMEGA * (target - pos) - 2.0 * OMEGA * vel;
                    vel += acc * hstep;
                    pos += vel * hstep;
                }
                if (target - pos).abs() <= 5e-4 && vel.abs() <= 5e-3 {
                    pos = target;
                    vel = 0.0;
                }
            }
            let animating = unsettled;

            // Clock text changes once a minute; check cheaply and redraw if needed.
            let t = GetLocalTime();
            let minute_key = t.wHour as u32 * 60 + t.wMinute as u32;

            if animating || was_animating || minute_key != last_minute_key {
                last_minute_key = minute_key;
                r.draw(hwnd, pos.clamp(0.0, 1.0));
            }
            was_animating = animating;

            if animating {
                // Block until the next compositor vblank: real vsync pacing.
                if DwmFlush().is_err() {
                    Sleep(8);
                }
            } else {
                // Idle: sleep until input arrives or 200 ms pass (for the minute rollover).
                MsgWaitForMultipleObjects(None, false, 200, QS_ALLINPUT);
            }
        }
    }
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            // Fully transparent pixels of a per-pixel-alpha layered window are click-through,
            // so these only fire while the cursor is over the visible pill.
            WM_MOUSEMOVE => {
                TARGET_EXPANDED.store(true, Ordering::Relaxed);
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
                TARGET_EXPANDED.store(false, Ordering::Relaxed);
                LRESULT(0)
            }
            WM_RBUTTONUP => {
                let _ = DestroyWindow(hwnd);
                LRESULT(0)
            }
            WM_ERASEBKGND => LRESULT(1),
            WM_DESTROY => {
                PostQuitMessage(0);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}
