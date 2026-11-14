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
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use windows::core::w;
use windows::Foundation::{AsyncStatus, IAsyncOperation};
use windows::Graphics::Imaging::{
    BitmapAlphaMode, BitmapDecoder, BitmapInterpolationMode, BitmapPixelFormat, BitmapTransform,
    ColorManagementMode, ExifOrientationMode,
};
use windows::Media::Control::{
    GlobalSystemMediaTransportControlsSessionManager,
    GlobalSystemMediaTransportControlsSessionPlaybackStatus,
};
use windows::Storage::Streams::IRandomAccessStreamReference;
use windows::Win32::Foundation::{
    GetLastError, BOOL, COLORREF, ERROR_ALREADY_EXISTS, FILETIME, HANDLE, HWND, LPARAM, LRESULT,
    POINT, RECT, SIZE, WPARAM,
};
use windows::Win32::Graphics::Dwm::DwmFlush;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::Win32::System::SystemInformation::{GetLocalTime, GlobalMemoryStatusEx, MEMORYSTATUSEX};
use windows::Win32::System::Threading::{CreateMutexW, GetSystemTimes, Sleep};
use windows::Win32::UI::HiDpi::GetDpiForSystem;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    keybd_event, ReleaseCapture, SetCapture, TrackMouseEvent, KEYEVENTF_KEYUP, TME_LEAVE,
    TRACKMOUSEEVENT, VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::*;

// ---- Layout (logical pixels at 96 DPI) -------------------------------------
const COLLAPSED_W: f32 = 112.0;
const COLLAPSED_W_MEDIA: f32 = 142.0; // collapsed pill is wider while a media session exists
const COLLAPSED_H: f32 = 34.0;
const EXPANDED_W: f32  = 340.0;
const EXPANDED_H: f32  = 114.0;
const TOP_MARGIN: f32  = 8.0;
const MAX_RADIUS: f32  = 26.0;

const TIME_FONT: f32    = 20.0;
const DATE_FONT: f32    = 12.5;
const TIME_BOX_H: f32   = 28.0;
const DATE_BOX_H: f32   = 22.0;
const TIME_TOP_COLLAPSED: f32 = (COLLAPSED_H - TIME_BOX_H) / 2.0;
const DATE_TOP_EXPANDED: f32  = 89.0;
const DATE_SLIDE: f32   = 8.0; // content slides up this far while fading in

// ---- Widgets (CPU & RAM rings, stacked on the right) ------------------------
const RING_CX: f32        = 269.0; // ring centre X
const RING_CY1: f32       = 43.0;  // CPU ring centre Y
const RING_CY2: f32       = 86.0;  // RAM ring centre Y
const RING_R: f32         = 15.0;
const RING_THICKNESS: f32 = 3.5;
const LABEL_CX: f32       = 311.0; // centre X of the "CPU" / "RAM" labels

const PCT_FONT: f32   = 10.0;
const LBL_FONT: f32   = 10.5;
const PCT_BOX_W: f32  = 32.0;
const PCT_BOX_H: f32  = 16.0;
const LBL_BOX_W: f32  = 36.0;
const LBL_BOX_H: f32  = 16.0;

// ---- Media: album art, title, slider, controls ------------------------------
// Expanded album art (also the animation target of the collapsed cover)
const ALBUM_ART_X: f32    = 13.0;
const ALBUM_ART_Y: f32    = 26.0;
const ALBUM_ART_SIZE: f32 = 62.0;
const ALBUM_ART_RADIUS: f32 = 15.0;

// Collapsed cover (left edge of the pill)
const COLL_ART_SIZE: f32   = 24.0;
const COLL_ART_PAD: f32    = 5.0;
const COLL_ART_RADIUS: f32 = 6.0;

// Collapsed audio visualizer (right side of the pill)
const VIZ_RIGHT_PAD: f32 = 19.0;

const MEDIA_TEXT_X: f32   = 85.0;
const MEDIA_TEXT_W: f32   = 141.0;
const MEDIA_TITLE_H: f32  = 20.0;
const MEDIA_TITLE_CY: f32 = 43.0;

const SLIDER_X0: f32 = 85.0;
const SLIDER_X1: f32 = 226.0;
const SLIDER_Y: f32  = 56.0;
const SLIDER_H: f32  = 3.5;

const BTN_HIT_R: f32 = 12.0; // play button hover circle radius
const BTN_SEC_R: f32 = 12.0; // prev/next hover circle radius

const CX_PREV: f32 = 131.0;
const CX_PLAY: f32 = 155.0;
const CX_NEXT: f32 = 179.0;
const CY_BTN: f32  = 76.0;

const CMD_TOGGLE: u8 = 1;
const CMD_NEXT: u8 = 2;
const CMD_PREV: u8 = 3;

// Queue of pending media commands, so quick successive clicks are not coalesced into one.
static MEDIA_CMDS: Mutex<Vec<u8>> = Mutex::new(Vec::new());
static MEDIA_ACTIVE: AtomicBool = AtomicBool::new(false);
static MEDIA_SEEK: AtomicU32 = AtomicU32::new(0); // 0 = none, else permille + 1
static DRAGGING: AtomicBool = AtomicBool::new(false); // seek slider is being dragged
static DRAG_FRAC: AtomicU32 = AtomicU32::new(0); // slider position shown while dragging, permille + 1
static HOVER_BTN: AtomicU8 = AtomicU8::new(0);
const NO_PRESS: u8 = 255;
static PRESSED_BTN: AtomicU8 = AtomicU8::new(NO_PRESS); // button that received WM_LBUTTONDOWN

const VK_MEDIA_NEXT_TRACK: VIRTUAL_KEY = VIRTUAL_KEY(0xB0);
const VK_MEDIA_PREV_TRACK: VIRTUAL_KEY = VIRTUAL_KEY(0xB1);
const VK_MEDIA_PLAY_PAUSE: VIRTUAL_KEY = VIRTUAL_KEY(0xB3);

/// Slider fraction (0..1) for a mouse message's client X coordinate.
fn slider_frac(lparam: LPARAM) -> f32 {
    let s = unsafe { GetDpiForSystem() } as f32 / 96.0;
    let mx = (lparam.0 & 0xFFFF) as i16 as f32 / s;
    ((mx - SLIDER_X0) / (SLIDER_X1 - SLIDER_X0)).clamp(0.0, 1.0)
}

fn push_media_cmd(cmd: u8) {
    if let Ok(mut q) = MEDIA_CMDS.lock() {
        if q.len() < 16 {
            q.push(cmd);
        }
    }
}

/// Used when no media session can be controlled directly: send the global media key instead.
fn send_fallback_key(cmd: u8) {
    match cmd {
        CMD_TOGGLE => send_media_key(VK_MEDIA_PLAY_PAUSE),
        CMD_NEXT => send_media_key(VK_MEDIA_NEXT_TRACK),
        CMD_PREV => send_media_key(VK_MEDIA_PREV_TRACK),
        _ => {}
    }
}

fn send_media_key(vk: VIRTUAL_KEY) {
    unsafe {
        keybd_event(vk.0 as u8, 0, Default::default(), 0);
        keybd_event(vk.0 as u8, 0, KEYEVENTF_KEYUP, 0);
    }
}

#[derive(Clone, Default)]
#[allow(dead_code)]
struct MediaInfo {
    title: String,
    artist: String,
    is_playing: bool,
    has_session: bool,
    thumbnail: Option<Arc<Vec<u8>>>, // shared, so cloning MediaInfo every frame stays cheap
    thumb_w: u32,
    thumb_h: u32,
    position_secs: f32,
    duration_secs: f32,
    sampled_at: Option<Instant>,
}

/// Current time as a Windows FILETIME value (100 ns ticks since 1601).
fn now_filetime() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| (d.as_nanos() / 100) as i64)
        .unwrap_or(0)
        + 116_444_736_000_000_000
}

/// Edge length of the decoded (square) album-art texture.
const THUMB_SIZE: u32 = 96;
/// Give up on a WinRT async call after this long, so one hung media app cannot stall the thread.
const ASYNC_TIMEOUT: Duration = Duration::from_secs(3);
/// How often to retry decoding a track's thumbnail while it is still missing.
const MAX_THUMB_TRIES: u32 = 10;

/// Wait for a WinRT async operation with a timeout (cancelling it when the time is up).
fn wait_timeout<T: windows::core::RuntimeType + 'static>(
    op: &IAsyncOperation<T>,
    timeout: Duration,
) -> Option<T> {
    let start = Instant::now();
    while op.Status().ok()? == AsyncStatus::Started {
        if start.elapsed() >= timeout {
            let _ = op.Cancel();
            return None;
        }
        thread::sleep(Duration::from_millis(5));
    }
    op.GetResults().ok()
}

/// True when two thumbnails are not the same shared texture.
fn thumb_changed(a: &Option<Arc<Vec<u8>>>, b: &Option<Arc<Vec<u8>>>) -> bool {
    match (a, b) {
        (Some(x), Some(y)) => !Arc::ptr_eq(x, y),
        (None, None) => false,
        _ => true,
    }
}

fn decode_thumbnail(thumb_ref: &IRandomAccessStreamReference, size: u32) -> Option<(u32, u32, Vec<u8>)> {
    let stream = wait_timeout(&thumb_ref.OpenReadAsync().ok()?, ASYNC_TIMEOUT)?;
    let decoder = wait_timeout(&BitmapDecoder::CreateAsync(&stream).ok()?, ASYNC_TIMEOUT)?;
    let transform = BitmapTransform::new().ok()?;
    transform.SetScaledWidth(size).ok()?;
    transform.SetScaledHeight(size).ok()?;
    transform.SetInterpolationMode(BitmapInterpolationMode::Linear).ok()?;
    let pixel_data = wait_timeout(
        &decoder
            .GetPixelDataTransformedAsync(
                BitmapPixelFormat::Bgra8,
                BitmapAlphaMode::Premultiplied,
                &transform,
                ExifOrientationMode::RespectExifOrientation,
                ColorManagementMode::ColorManageToSRgb,
            )
            .ok()?,
        ASYNC_TIMEOUT,
    )?;
    let data = pixel_data.DetachPixelData().ok()?;
    Some((size, size, data.to_vec()))
}

struct MediaController {
    info: Arc<Mutex<MediaInfo>>,
}

impl MediaController {
    fn new() -> Self {
        let info = Arc::new(Mutex::new(MediaInfo::default()));
        let info_clone = Arc::clone(&info);

        thread::spawn(move || {
            unsafe {
                let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            }

            let mut manager: Option<GlobalSystemMediaTransportControlsSessionManager> = None;
            let mut last_title = String::new();
            let mut last_artist = String::new();
            let mut last_playing = false;
            let mut last_has_session = false;
            let mut last_thumb: Option<Arc<Vec<u8>>> = None;
            let mut thumb_tries = 0u32;
            let mut last_thumb_w = 0u32;
            let mut last_thumb_h = 0u32;

            loop {
                if manager.is_none() {
                    manager = GlobalSystemMediaTransportControlsSessionManager::RequestAsync()
                        .ok()
                        .and_then(|op| wait_timeout(&op, ASYNC_TIMEOUT));
                }

                let cmds: Vec<u8> = MEDIA_CMDS
                    .lock()
                    .map(|mut q| std::mem::take(&mut *q))
                    .unwrap_or_default();
                let seek = MEDIA_SEEK.swap(0, Ordering::Relaxed);

                if let Some(ref mgr) = manager {
                    if let Ok(session) = mgr.GetCurrentSession() {
                        for &cmd in &cmds {
                            match cmd {
                                CMD_TOGGLE => {
                                    let _ = session.TryTogglePlayPauseAsync();
                                }
                                CMD_NEXT => {
                                    let _ = session.TrySkipNextAsync();
                                }
                                CMD_PREV => {
                                    let _ = session.TrySkipPreviousAsync();
                                }
                                _ => {}
                            }
                        }

                        // Timeline (all values are 100 ns ticks)
                        let (tl_start, tl_end, tl_pos, tl_upd) = session
                            .GetTimelineProperties()
                            .ok()
                            .map(|tl| {
                                (
                                    tl.StartTime().map(|t| t.Duration).unwrap_or(0),
                                    tl.EndTime().map(|t| t.Duration).unwrap_or(0),
                                    tl.Position().map(|t| t.Duration).unwrap_or(0),
                                    tl.LastUpdatedTime().map(|d| d.UniversalTime).unwrap_or(0),
                                )
                            })
                            .unwrap_or((0, 0, 0, 0));

                        if seek > 0 && tl_end > tl_start {
                            let frac = (seek - 1) as f64 / 1000.0;
                            let target = tl_start + ((tl_end - tl_start) as f64 * frac) as i64;
                            let _ = session.TryChangePlaybackPositionAsync(target);
                        }

                        let is_playing = session
                            .GetPlaybackInfo()
                            .ok()
                            .and_then(|p| p.PlaybackStatus().ok())
                            .map(|s| s == GlobalSystemMediaTransportControlsSessionPlaybackStatus::Playing)
                            .unwrap_or(false);

                        let (title, artist, thumb_opt) = session
                            .TryGetMediaPropertiesAsync()
                            .ok()
                            .and_then(|op| wait_timeout(&op, ASYNC_TIMEOUT))
                            .map(|props| {
                                let t = props.Title().map(|s| s.to_string_lossy()).unwrap_or_default();
                                let a = props.Artist().map(|s| s.to_string_lossy()).unwrap_or_default();
                                let thumb = props.Thumbnail().ok();
                                (t, a, thumb)
                            })
                            .unwrap_or_default();

                        let is_new_track = title != last_title || artist != last_artist;
                        if is_new_track {
                            last_thumb = None;
                            last_thumb_w = 0;
                            last_thumb_h = 0;
                            thumb_tries = 0;
                        }
                        // Players often publish the title before the artwork, so keep retrying
                        // (a limited number of times) until a thumbnail decodes.
                        let mut thumb_changed_now = false;
                        if last_thumb.is_none() && thumb_tries < MAX_THUMB_TRIES {
                            if let Some(ref t_ref) = thumb_opt {
                                thumb_tries += 1;
                                if let Some((tw, th, data)) = decode_thumbnail(t_ref, THUMB_SIZE) {
                                    last_thumb = Some(Arc::new(data));
                                    last_thumb_w = tw;
                                    last_thumb_h = th;
                                    thumb_changed_now = true;
                                }
                            }
                        }

                        // Playback position, extrapolated from the time the app last reported it.
                        let duration_secs = (tl_end - tl_start).max(0) as f64 / 1e7;
                        let mut position_secs = (tl_pos - tl_start).max(0) as f64 / 1e7;
                        if is_playing && tl_upd > 0 {
                            let behind = (now_filetime() - tl_upd) as f64 / 1e7;
                            if behind > 0.0 && behind < 6.0 * 3600.0 {
                                position_secs += behind;
                            }
                        }
                        if duration_secs > 0.0 {
                            position_secs = position_secs.min(duration_secs);
                        }

                        if is_new_track || thumb_changed_now || is_playing != last_playing || !last_has_session {
                            last_title = title.clone();
                            last_artist = artist.clone();
                            last_playing = is_playing;
                            last_has_session = true;

                            if let Ok(mut lock) = info_clone.lock() {
                                *lock = MediaInfo {
                                    title,
                                    artist,
                                    is_playing,
                                    has_session: true,
                                    thumbnail: last_thumb.clone(),
                                    thumb_w: last_thumb_w,
                                    thumb_h: last_thumb_h,
                                    ..Default::default()
                                };
                            }
                        }

                        if let Ok(mut lock) = info_clone.lock() {
                            lock.position_secs = position_secs as f32;
                            lock.duration_secs = duration_secs as f32;
                            lock.sampled_at = Some(Instant::now());
                        }
                    } else {
                        // Fallback media keys if no active session
                        for &cmd in &cmds {
                            send_fallback_key(cmd);
                        }

                        if last_has_session {
                            last_has_session = false;
                            last_title.clear();
                            last_artist.clear();
                            last_playing = false;
                            last_thumb = None;
                            thumb_tries = 0;

                            if let Ok(mut lock) = info_clone.lock() {
                                *lock = MediaInfo::default();
                            }
                        }
                    }
                } else {
                    for &cmd in &cmds {
                        send_fallback_key(cmd);
                    }
                }

                thread::sleep(Duration::from_millis(150));
            }
        });

        Self { info }
    }

    fn get_info(&self) -> MediaInfo {
        self.info.lock().map(|g| g.clone()).unwrap_or_default()
    }
}

// ---- Motion ----------------------------------------------------------------
// Critically damped spring: no overshoot, no hard stop, fully interruptible.
// Higher OMEGA = faster/snappier, lower = softer. ~20 settles in about 0.3 s.
const OMEGA: f32 = 20.0;

// Text supersampling factor (mask is rendered at SS x size, then box-filtered).
const SS: i32 = 4;

const WM_MOUSELEAVE: u32 = 0x02A3;

static TARGET_EXPANDED: AtomicBool = AtomicBool::new(false);
static TRACKING: AtomicBool = AtomicBool::new(false);
static PINNED: AtomicBool = AtomicBool::new(false);

fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

/// Quintic smoothstep (zero velocity and acceleration at both ends).
fn smooth_step(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    t * t * t * (t * (t * 6.0 - 15.0) + 10.0)
}

fn filetime_to_u64(ft: &FILETIME) -> u64 {
    ((ft.dwHighDateTime as u64) << 32) | (ft.dwLowDateTime as u64)
}

struct SystemMonitor {
    prev_idle: u64,
    prev_kernel: u64,
    prev_user: u64,
    cpu_usage: f32,
    ram_usage: f32,
    last_sample_qpc: i64,
}

impl SystemMonitor {
    fn new() -> Self {
        let mut idle = FILETIME::default();
        let mut kernel = FILETIME::default();
        let mut user = FILETIME::default();
        unsafe {
            let _ = GetSystemTimes(Some(&mut idle), Some(&mut kernel), Some(&mut user));
        }
        let mut mon = Self {
            prev_idle: filetime_to_u64(&idle),
            prev_kernel: filetime_to_u64(&kernel),
            prev_user: filetime_to_u64(&user),
            cpu_usage: 0.0,
            ram_usage: 0.0,
            last_sample_qpc: qpc(),
        };
        mon.sample_ram();
        mon
    }

    fn sample_ram(&mut self) {
        let mut mem = MEMORYSTATUSEX {
            dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32,
            ..Default::default()
        };
        unsafe {
            if GlobalMemoryStatusEx(&mut mem).is_ok() {
                if mem.ullTotalPhys > 0 {
                    let used = mem.ullTotalPhys.saturating_sub(mem.ullAvailPhys);
                    self.ram_usage = ((used as f64 / mem.ullTotalPhys as f64) as f32).clamp(0.0, 1.0);
                } else {
                    self.ram_usage = (mem.dwMemoryLoad as f32 / 100.0).clamp(0.0, 1.0);
                }
            }
        }
    }

    fn sample_cpu(&mut self) {
        let mut idle = FILETIME::default();
        let mut kernel = FILETIME::default();
        let mut user = FILETIME::default();
        unsafe {
            if GetSystemTimes(Some(&mut idle), Some(&mut kernel), Some(&mut user)).is_ok() {
                let idle_u = filetime_to_u64(&idle);
                let kernel_u = filetime_to_u64(&kernel);
                let user_u = filetime_to_u64(&user);

                let idle_delta = idle_u.saturating_sub(self.prev_idle);
                let kernel_delta = kernel_u.saturating_sub(self.prev_kernel);
                let user_delta = user_u.saturating_sub(self.prev_user);

                let total_delta = kernel_delta + user_delta;
                if total_delta > 0 {
                    let busy_delta = total_delta.saturating_sub(idle_delta);
                    self.cpu_usage = (busy_delta as f32 / total_delta as f32).clamp(0.0, 1.0);
                }

                self.prev_idle = idle_u;
                self.prev_kernel = kernel_u;
                self.prev_user = user_u;
            }
        }
    }

    fn update(&mut self, now_qpc: i64, freq: f64) -> bool {
        let elapsed = (now_qpc - self.last_sample_qpc) as f64 / freq;
        if elapsed >= 0.5 {
            self.last_sample_qpc = now_qpc;
            self.sample_cpu();
            self.sample_ram();
            true
        } else {
            false
        }
    }
}

fn get_cpu_color(pct: f32) -> (f32, f32, f32) {
    if pct < 0.60 {
        (56.0, 189.0, 248.0) // Sky cyan
    } else if pct < 0.85 {
        (251.0, 191.0, 36.0) // Amber
    } else {
        (244.0, 63.0, 94.0)  // Coral red
    }
}

fn get_ram_color(pct: f32) -> (f32, f32, f32) {
    if pct < 0.70 {
        (168.0, 85.0, 247.0) // Purple
    } else if pct < 0.88 {
        (251.0, 146.0, 60.0) // Orange
    } else {
        (244.0, 63.0, 94.0)  // Coral red
    }
}

/// Returns (active_arc_coverage, track_coverage) for an analytical rounded-cap progress circle.
fn sample_progress_circle(
    fx: f32,
    fy: f32,
    cx: f32,
    cy: f32,
    r: f32,
    thickness: f32,
    progress: f32,
) -> (f32, f32) {
    let dx = fx - cx;
    let dy = fy - cy;
    let dist = dx.hypot(dy);
    let ht = thickness / 2.0;

    let d_ring = (dist - r).abs() - ht;
    if d_ring > 1.5 {
        return (0.0, 0.0);
    }
    let track_cov = (0.5 - d_ring).clamp(0.0, 1.0);

    let progress = progress.clamp(0.0, 1.0);
    if progress < 0.005 {
        return (0.0, track_cov);
    }

    let theta = progress * std::f32::consts::TAU;
    let mut alpha = dx.atan2(-dy);
    if alpha < 0.0 {
        alpha += std::f32::consts::TAU;
    }

    let d_arc = if theta >= std::f32::consts::TAU - 0.005 {
        d_ring
    } else if alpha <= theta {
        (dist - r).abs() - ht
    } else {
        let d0 = dx.hypot(dy + r);
        let ep1_x = r * theta.sin();
        let ep1_y = -r * theta.cos();
        let d1 = (dx - ep1_x).hypot(dy - ep1_y);
        d0.min(d1) - ht
    };

    let arc_cov = (0.5 - d_arc).clamp(0.0, 1.0);
    (arc_cov, track_cov)
}

fn sample_button_circle(fx: f32, fy: f32, cx: f32, cy: f32, r: f32) -> f32 {
    let d = (fx - cx).hypot(fy - cy) - r;
    (0.5 - d).clamp(0.0, 1.0)
}

fn sample_eq_bar(fx: f32, fy: f32, bx: f32, by: f32, bw: f32, bh: f32, r: f32) -> f32 {
    let qx = (fx - bx).abs() - (bw / 2.0 - r);
    let qy = (fy - by).abs() - (bh / 2.0 - r);
    let d = qx.max(0.0).hypot(qy.max(0.0)) + qx.max(qy).min(0.0) - r;
    (0.5 - d).clamp(0.0, 1.0)
}

// ---- Vector icon SDFs ------------------------------------------------------

/// AA coverage for a filled circle (positive inside).
fn sdf_circle_cov(fx: f32, fy: f32, cx: f32, cy: f32, r: f32) -> f32 {
    let d = (fx - cx).hypot(fy - cy) - r;
    (0.5 - d).clamp(0.0, 1.0)
}

/// AA coverage for a filled rounded box (half-widths hw, hh, corner radius r).
fn sdf_rounded_box_cov(fx: f32, fy: f32, cx: f32, cy: f32, hw: f32, hh: f32, r: f32) -> f32 {
    let qx = (fx - cx).abs() - hw + r;
    let qy = (fy - cy).abs() - hh + r;
    let d = qx.max(0.0).hypot(qy.max(0.0)) + qx.max(qy).min(0.0) - r;
    (0.5 - d).clamp(0.0, 1.0)
}

// ---- SVG icons ---------------------------------------------------------------
// Icons are written as SVG path data on a 24x24 viewBox. They are parsed once,
// rasterised into alpha masks at the current DPI scale, and then only sampled
// (bilinearly, so they can sit at sub-pixel positions while the pill animates).

const ICON_VIEW: f32 = 24.0;

#[derive(Clone, Copy)]
enum Tok {
    Cmd(char),
    Num(f32),
}

/// Rounded polygon as SVG path data: every corner is replaced by a quadratic curve.
fn rounded_poly(pts: &[(f32, f32)], r: f32) -> String {
    let n = pts.len();
    let mut d = String::new();
    for i in 0..n {
        let prev = pts[(i + n - 1) % n];
        let cur = pts[i];
        let next = pts[(i + 1) % n];
        let toward = |o: (f32, f32)| {
            let (dx, dy) = (o.0 - cur.0, o.1 - cur.1);
            let len = dx.hypot(dy).max(1e-4);
            let k = r.min(len / 2.0) / len;
            (cur.0 + dx * k, cur.1 + dy * k)
        };
        let a = toward(prev);
        let b = toward(next);
        d.push_str(&format!(
            "{}{:.3} {:.3} Q{:.3} {:.3} {:.3} {:.3} ",
            if i == 0 { "M" } else { "L" },
            a.0, a.1, cur.0, cur.1, b.0, b.1
        ));
    }
    d.push('Z');
    d
}

fn rounded_rect(x0: f32, y0: f32, x1: f32, y1: f32, r: f32) -> String {
    rounded_poly(&[(x0, y0), (x1, y0), (x1, y1), (x0, y1)], r)
}

fn take_nums(toks: &[Tok], idx: &mut usize, n: usize) -> Option<[f32; 6]> {
    let mut out = [0.0f32; 6];
    for slot in out.iter_mut().take(n) {
        match toks.get(*idx) {
            Some(Tok::Num(v)) => {
                *slot = *v;
                *idx += 1;
            }
            _ => return None,
        }
    }
    Some(out)
}

/// Minimal SVG path parser: M L H V Q C Z (absolute and relative). Curves are flattened.
fn parse_svg_path(d: &str) -> Vec<Vec<(f32, f32)>> {
    let b = d.as_bytes();
    let mut toks: Vec<Tok> = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let c = b[i] as char;
        if c.is_ascii_alphabetic() {
            toks.push(Tok::Cmd(c));
            i += 1;
        } else if c == '-' || c == '+' || c == '.' || c.is_ascii_digit() {
            let st = i;
            let mut seen_dot = c == '.';
            i += 1;
            while i < b.len() {
                let ch = b[i] as char;
                if ch.is_ascii_digit() {
                    i += 1;
                } else if ch == '.' && !seen_dot {
                    seen_dot = true;
                    i += 1;
                } else {
                    break;
                }
            }
            toks.push(Tok::Num(d[st..i].parse().unwrap_or(0.0)));
        } else {
            i += 1;
        }
    }

    const STEPS: usize = 12;
    let mut polys: Vec<Vec<(f32, f32)>> = Vec::new();
    let mut cur: Vec<(f32, f32)> = Vec::new();
    let (mut x, mut y) = (0.0f32, 0.0f32);
    let (mut sx, mut sy) = (0.0f32, 0.0f32);
    let mut cmd = ' ';
    let mut idx = 0usize;

    while idx < toks.len() {
        if let Tok::Cmd(c) = toks[idx] {
            cmd = c;
            idx += 1;
            if c == 'Z' || c == 'z' {
                if !cur.is_empty() {
                    polys.push(std::mem::take(&mut cur));
                }
                x = sx;
                y = sy;
                continue;
            }
        }
        let rel = cmd.is_ascii_lowercase();
        let (ox, oy) = if rel { (x, y) } else { (0.0, 0.0) };
        match cmd.to_ascii_uppercase() {
            'M' => {
                let Some(v) = take_nums(&toks, &mut idx, 2) else { break };
                if !cur.is_empty() {
                    polys.push(std::mem::take(&mut cur));
                }
                x = ox + v[0];
                y = oy + v[1];
                sx = x;
                sy = y;
                cur.push((x, y));
                cmd = if rel { 'l' } else { 'L' };
            }
            'L' => {
                let Some(v) = take_nums(&toks, &mut idx, 2) else { break };
                x = ox + v[0];
                y = oy + v[1];
                cur.push((x, y));
            }
            'H' => {
                let Some(v) = take_nums(&toks, &mut idx, 1) else { break };
                x = ox + v[0];
                cur.push((x, y));
            }
            'V' => {
                let Some(v) = take_nums(&toks, &mut idx, 1) else { break };
                y = oy + v[0];
                cur.push((x, y));
            }
            'Q' => {
                let Some(v) = take_nums(&toks, &mut idx, 4) else { break };
                let (c1x, c1y, ex, ey) = (ox + v[0], oy + v[1], ox + v[2], oy + v[3]);
                for k in 1..=STEPS {
                    let t = k as f32 / STEPS as f32;
                    let u = 1.0 - t;
                    cur.push((
                        u * u * x + 2.0 * u * t * c1x + t * t * ex,
                        u * u * y + 2.0 * u * t * c1y + t * t * ey,
                    ));
                }
                x = ex;
                y = ey;
            }
            'C' => {
                let Some(v) = take_nums(&toks, &mut idx, 6) else { break };
                let (c1x, c1y) = (ox + v[0], oy + v[1]);
                let (c2x, c2y) = (ox + v[2], oy + v[3]);
                let (ex, ey) = (ox + v[4], oy + v[5]);
                for k in 1..=STEPS {
                    let t = k as f32 / STEPS as f32;
                    let u = 1.0 - t;
                    cur.push((
                        u * u * u * x + 3.0 * u * u * t * c1x + 3.0 * u * t * t * c2x + t * t * t * ex,
                        u * u * u * y + 3.0 * u * u * t * c1y + 3.0 * u * t * t * c2y + t * t * t * ey,
                    ));
                }
                x = ex;
                y = ey;
            }
            _ => break,
        }
    }
    if !cur.is_empty() {
        polys.push(cur);
    }
    polys
}

/// Non-zero winding number of the point (px, py) against all sub-paths.
fn winding(polys: &[Vec<(f32, f32)>], px: f32, py: f32) -> i32 {
    let mut wn = 0;
    for poly in polys {
        let n = poly.len();
        if n < 3 {
            continue;
        }
        for i in 0..n {
            let a = poly[i];
            let b = poly[(i + 1) % n];
            let cross = (b.0 - a.0) * (py - a.1) - (px - a.0) * (b.1 - a.1);
            if a.1 <= py {
                if b.1 > py && cross > 0.0 {
                    wn += 1;
                }
            } else if b.1 <= py && cross < 0.0 {
                wn -= 1;
            }
        }
    }
    wn
}

/// Rasterise SVG path data into an n x n coverage mask; the 24x24 viewBox is centred
/// in the mask and scaled to `box_px` pixels.
fn rasterize_icon(d: &str, n: usize, box_px: f32) -> Vec<u8> {
    const K: usize = 8;
    let polys = parse_svg_path(d);
    let ppu = box_px / ICON_VIEW;
    let off = (n as f32 - box_px) / 2.0;
    let mut out = vec![0u8; n * n];
    for j in 0..n {
        for i in 0..n {
            let mut hit = 0u32;
            for sj in 0..K {
                for si in 0..K {
                    let u = (i as f32 + (si as f32 + 0.5) / K as f32 - off) / ppu;
                    let v = (j as f32 + (sj as f32 + 0.5) / K as f32 - off) / ppu;
                    if winding(&polys, u, v) != 0 {
                        hit += 1;
                    }
                }
            }
            out[j * n + i] = (hit as f32 * 255.0 / (K * K) as f32).round() as u8;
        }
    }
    out
}

/// Bilinear coverage lookup; the mask's top-left corner sits at screen (x0, y0).
fn sample_icon(mask: &[u8], n: usize, x0: f32, y0: f32, fx: f32, fy: f32) -> f32 {
    let tx = fx - x0 - 0.5;
    let ty = fy - y0 - 0.5;
    if tx < -1.0 || ty < -1.0 || tx > n as f32 || ty > n as f32 {
        return 0.0;
    }
    let ix = tx.floor();
    let iy = ty.floor();
    let (ax, ay) = (tx - ix, ty - iy);
    let (ix, iy) = (ix as i32, iy as i32);
    let get = |x: i32, y: i32| -> f32 {
        if x < 0 || y < 0 || x >= n as i32 || y >= n as i32 {
            0.0
        } else {
            mask[y as usize * n + x as usize] as f32 / 255.0
        }
    };
    let top = get(ix, iy) * (1.0 - ax) + get(ix + 1, iy) * ax;
    let bot = get(ix, iy + 1) * (1.0 - ax) + get(ix + 1, iy + 1) * ax;
    top * (1.0 - ay) + bot * ay
}

/// Edge length (logical px) of the square the 24x24 icon viewBox is drawn into.
const ICON_BOX: f32 = 26.0;

struct IconSet {
    n: usize,
    play: Vec<u8>,
    pause: Vec<u8>,
    prev: Vec<u8>,
    next: Vec<u8>,
}

impl IconSet {
    fn new(s: f32) -> Self {
        let box_px = ICON_BOX * s;
        let n = box_px.ceil() as usize + 2;

        let play = rounded_poly(&[(8.0, 5.0), (8.0, 19.0), (19.0, 12.0)], 2.2);
        let pause = format!(
            "{} {}",
            rounded_rect(6.5, 5.0, 10.5, 19.0, 1.5),
            rounded_rect(13.5, 5.0, 17.5, 19.0, 1.5)
        );
        let prev = format!(
            "{} {}",
            rounded_rect(5.0, 5.0, 7.6, 19.0, 1.2),
            rounded_poly(&[(19.5, 5.0), (19.5, 19.0), (9.5, 12.0)], 2.0)
        );
        let next = format!(
            "{} {}",
            rounded_rect(16.4, 5.0, 19.0, 19.0, 1.2),
            rounded_poly(&[(4.5, 5.0), (4.5, 19.0), (14.5, 12.0)], 2.0)
        );

        IconSet {
            n,
            play: rasterize_icon(&play, n, box_px),
            pause: rasterize_icon(&pause, n, box_px),
            prev: rasterize_icon(&prev, n, box_px),
            next: rasterize_icon(&next, n, box_px),
        }
    }
}

/// Bilinear sample of the THUMB_SIZE x THUMB_SIZE BGRA album-art texture at normalised (u, v). Returns (r, g, b).
fn sample_thumb(pix: &[u8], u: f32, v: f32) -> (f32, f32, f32) {
    const N: usize = THUMB_SIZE as usize;
    if pix.len() < N * N * 4 {
        return (0.0, 0.0, 0.0);
    }
    let tx = (u * N as f32 - 0.5).clamp(0.0, (N - 1) as f32);
    let ty = (v * N as f32 - 0.5).clamp(0.0, (N - 1) as f32);
    let x0 = tx.floor() as usize;
    let y0 = ty.floor() as usize;
    let x1 = (x0 + 1).min(N - 1);
    let y1 = (y0 + 1).min(N - 1);
    let (ax, ay) = (tx - x0 as f32, ty - y0 as f32);
    let px = |x: usize, y: usize| {
        let i = (y * N + x) * 4;
        (pix[i + 2] as f32, pix[i + 1] as f32, pix[i] as f32)
    };
    let (p00, p10, p01, p11) = (px(x0, y0), px(x1, y0), px(x0, y1), px(x1, y1));
    let mix = |a: f32, b: f32, t: f32| a + (b - a) * t;
    (
        mix(mix(p00.0, p10.0, ax), mix(p01.0, p11.0, ax), ay),
        mix(mix(p00.1, p10.1, ax), mix(p01.1, p11.1, ax), ay),
        mix(mix(p00.2, p10.2, ax), mix(p01.2, p11.2, ax), ay),
    )
}

/// Musical note ♫ fallback — simple two-circle-and-stem shape.
fn sample_note_icon(fx: f32, fy: f32, cx: f32, cy: f32, size: f32) -> f32 {
    let head1 = sdf_circle_cov(fx, fy, cx - size * 0.35, cy + size * 0.55, size * 0.28);
    let head2 = sdf_circle_cov(fx, fy, cx + size * 0.35, cy + size * 0.55, size * 0.28);
    let stem  = sdf_rounded_box_cov(fx, fy, cx - size * 0.07, cy - size * 0.05, size * 0.06, size * 0.60, size * 0.04);
    let beam  = sdf_rounded_box_cov(fx, fy, cx + size * 0.14, cy - size * 0.60, size * 0.28, size * 0.07, size * 0.04);
    (head1 + head2 + stem + beam).min(1.0)
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
    render_mask_ex(text, font_px, weight, hw, hh, DT_CENTER | DT_VCENTER | DT_SINGLELINE)
}

unsafe fn render_mask_ex(
    text: &str,
    font_px: i32,
    weight: i32,
    hw: i32,
    hh: i32,
    flags: DRAW_TEXT_FORMAT,
) -> Vec<u8> {
    let screen = GetDC(None);
    let dc = CreateCompatibleDC(screen);
    let bmi = make_bmi(hw, hh);
    let mut bits: *mut c_void = std::ptr::null_mut();
    let Ok(bmp) = CreateDIBSection(dc, &bmi, DIB_RGB_COLORS, &mut bits, HANDLE::default(), 0) else {
        // GDI is out of resources: draw nothing rather than crash the whole island.
        let _ = DeleteDC(dc);
        ReleaseDC(None, screen);
        return vec![0u8; (hw * hh).max(0) as usize];
    };
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
        flags,
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
    if mask.is_empty() {
        return 0.0;
    }
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

    cpu_label_mask: Vec<u8>,
    ram_label_mask: Vec<u8>,
    cpu_pct_mask: Vec<u8>,
    ram_pct_mask: Vec<u8>,
    cpu_pct_val: i32,
    ram_pct_val: i32,
    cpu_pct_hw: i32,
    cpu_pct_hh: i32,
    cpu_lbl_hw: i32,
    cpu_lbl_hh: i32,
    ram_pct_hw: i32,
    ram_pct_hh: i32,
    ram_lbl_hw: i32,
    ram_lbl_hh: i32,

    // Media text (title/artist rendered as GDI masks for sub-pixel AA)
    media_title: String,
    media_is_playing: bool,
    media_title_mask: Vec<u8>,
    media_title_hw: i32,
    media_title_hh: i32,

    // Album art (decoded BGRA premul, THUMB_SIZE x THUMB_SIZE), shared with the media thread
    thumb_pixels: Option<Arc<Vec<u8>>>,

    icons: IconSet,
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

        let pct_hw = (PCT_BOX_W * s).round() as i32 * SS;
        let pct_hh = (PCT_BOX_H * s).round() as i32 * SS;
        let lbl_hw = (LBL_BOX_W * s).round() as i32 * SS;
        let lbl_hh = (LBL_BOX_H * s).round() as i32 * SS;

        let cpu_label_mask = render_mask(
            "CPU",
            (LBL_FONT * s * SS as f32).round() as i32,
            600,
            lbl_hw,
            lbl_hh,
        );
        let ram_label_mask = render_mask(
            "RAM",
            (LBL_FONT * s * SS as f32).round() as i32,
            600,
            lbl_hw,
            lbl_hh,
        );

        let title_hw = (MEDIA_TEXT_W * s).round() as i32 * SS;
        let title_hh = (MEDIA_TITLE_H * s).round() as i32 * SS;

        let mut r = Renderer {
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

            cpu_label_mask,
            ram_label_mask,
            cpu_pct_mask: Vec::new(),
            ram_pct_mask: Vec::new(),
            cpu_pct_val: -1,
            ram_pct_val: -1,
            cpu_pct_hw: pct_hw,
            cpu_pct_hh: pct_hh,
            cpu_lbl_hw: lbl_hw,
            cpu_lbl_hh: lbl_hh,
            ram_pct_hw: pct_hw,
            ram_pct_hh: pct_hh,
            ram_lbl_hw: lbl_hw,
            ram_lbl_hh: lbl_hh,

            media_title: String::new(),
            media_is_playing: false,
            media_title_mask: Vec::new(),
            media_title_hw: title_hw,
            media_title_hh: title_hh,

            thumb_pixels: None,

            icons: IconSet::new(s),
        };
        r.update_metrics(0.0, 0.0);
        r
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
            700,
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

    /// Re-render metric percentage text masks only when integer percentage changes.
    unsafe fn update_metrics(&mut self, cpu_pct: f32, ram_pct: f32) {
        let cpu_int = (cpu_pct * 100.0).round() as i32;
        let ram_int = (ram_pct * 100.0).round() as i32;

        if cpu_int != self.cpu_pct_val {
            self.cpu_pct_val = cpu_int;
            let text = format!("{}%", cpu_int);
            self.cpu_pct_mask = render_mask(
                &text,
                (PCT_FONT * self.s * SS as f32).round() as i32,
                700,
                self.cpu_pct_hw,
                self.cpu_pct_hh,
            );
        }

        if ram_int != self.ram_pct_val {
            self.ram_pct_val = ram_int;
            let text = format!("{}%", ram_int);
            self.ram_pct_mask = render_mask(
                &text,
                (PCT_FONT * self.s * SS as f32).round() as i32,
                700,
                self.ram_pct_hw,
                self.ram_pct_hh,
            );
        }
    }

    /// Re-render the media title mask only when it changes.
    unsafe fn update_media(&mut self, title: &str, is_playing: bool) {
        if title != self.media_title {
            self.media_title = title.to_string();
            self.media_title_mask = render_mask_ex(
                title,
                (12.5 * self.s * SS as f32).round() as i32,
                600,
                self.media_title_hw,
                self.media_title_hh,
                DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS,
            );
        }
        self.media_is_playing = is_playing;
    }

    /// Draw one frame for animation progress `p` (0 = collapsed, 1 = expanded).
    /// `mw` (0..1) widens the collapsed pill while a media session exists.
    unsafe fn draw(
        &mut self,
        hwnd: HWND,
        p: f32,
        cpu_pct: f32,
        ram_pct: f32,
        media: &MediaInfo,
        hover_btn: u8,
        time_sec: f32,
        mw: f32,
    ) {
        self.update_text();

        let (w, h, s) = (self.w, self.h, self.s);
        let buf = std::slice::from_raw_parts_mut(self.bits, (w * h) as usize);
        buf.fill(0);

        // Pill geometry (floats; this is what makes the motion sub-pixel smooth).
        let coll_w = lerp(COLLAPSED_W, COLLAPSED_W_MEDIA, mw);
        let pw = lerp(coll_w, EXPANDED_W, p) * s;
        let ph = lerp(COLLAPSED_H, EXPANDED_H, p) * s;
        let rad = (ph / 2.0).min(MAX_RADIUS * s);
        let cx = w as f32 / 2.0;
        let cy = ph / 2.0;
        let (hx, hy) = (pw / 2.0 - rad, ph / 2.0 - rad);
        // Left edge of the fully expanded layout; all expanded X positions hang off this.
        let ox = cx - EXPANDED_W * s / 2.0;

        // The clock stays centred; date, rings and media controls fade/slide in.
        let fade = smooth_step((p - 0.28) / 0.72);
        let time_top = TIME_TOP_COLLAPSED * s; // clock never moves
        let slide = (1.0 - fade) * DATE_SLIDE * s;
        let date_top = (DATE_TOP_EXPANDED * s) + slide;
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

        if fade > 0.0 {
            self.update_metrics(cpu_pct, ram_pct);
        }

        let has_media = media.has_session && !media.title.is_empty();
        let show_media = fade > 0.0 && has_media;
        if show_media {
            self.update_media(&media.title, media.is_playing);
        }

        // Sync album art from MediaInfo (a cheap Arc clone, only when the texture changed)
        if (has_media || !media.has_session) && thumb_changed(&self.thumb_pixels, &media.thumbnail) {
            self.thumb_pixels = media.thumbnail.clone();
        }

        // Playback progress for the slider
        let progress = if media.duration_secs > 0.5 {
            let mut ps = media.position_secs;
            if media.is_playing {
                if let Some(t) = media.sampled_at {
                    ps += t.elapsed().as_secs_f32();
                }
            }
            (ps / media.duration_secs).clamp(0.0, 1.0)
        } else {
            0.0
        };

        // ---- Rings (CPU on top, RAM below, labels to the right) ----
        struct Ring<'a> {
            cy: f32,
            col: (f32, f32, f32),
            pct: f32,
            pct_mask: &'a [u8],
            pct_hw: i32,
            pct_hh: i32,
            pct_dy: i32,
            lbl_mask: &'a [u8],
            lbl_hw: i32,
            lbl_hh: i32,
            lbl_dy: i32,
        }

        let cx_ring = ox + RING_CX * s;
        let r_ring = RING_R * s;
        let thickness = RING_THICKNESS * s;
        let ring_x0 = (cx_ring - r_ring - thickness).floor() as i32;

        let pct_box_w = (PCT_BOX_W * s).round() as i32;
        let pct_box_h = (PCT_BOX_H * s).round() as i32;
        let pct_x0 = (cx_ring - pct_box_w as f32 / 2.0).round() as i32;
        let lbl_box_w = (LBL_BOX_W * s).round() as i32;
        let lbl_box_h = (LBL_BOX_H * s).round() as i32;
        let lbl_x0 = (ox + LABEL_CX * s - lbl_box_w as f32 / 2.0).round() as i32;
        let ring_x1 = lbl_x0 + lbl_box_w;

        let cy_r1 = RING_CY1 * s + slide;
        let cy_r2 = RING_CY2 * s + slide;
        let dy_of = |top: f32| (top * SS as f32).round() as i32;

        let rings = [
            Ring {
                cy: cy_r1,
                col: get_cpu_color(cpu_pct),
                pct: cpu_pct,
                pct_mask: &self.cpu_pct_mask,
                pct_hw: self.cpu_pct_hw,
                pct_hh: self.cpu_pct_hh,
                pct_dy: dy_of(cy_r1 - pct_box_h as f32 / 2.0),
                lbl_mask: &self.cpu_label_mask,
                lbl_hw: self.cpu_lbl_hw,
                lbl_hh: self.cpu_lbl_hh,
                lbl_dy: dy_of(cy_r1 - lbl_box_h as f32 / 2.0),
            },
            Ring {
                cy: cy_r2,
                col: get_ram_color(ram_pct),
                pct: ram_pct,
                pct_mask: &self.ram_pct_mask,
                pct_hw: self.ram_pct_hw,
                pct_hh: self.ram_pct_hh,
                pct_dy: dy_of(cy_r2 - pct_box_h as f32 / 2.0),
                lbl_mask: &self.ram_label_mask,
                lbl_hw: self.ram_lbl_hw,
                lbl_hh: self.ram_lbl_hh,
                lbl_dy: dy_of(cy_r2 - lbl_box_h as f32 / 2.0),
            },
        ];
        let ring_half_h = r_ring + thickness + 2.0;

        // ---- Album art: one rounded square that glides from the collapsed pill to its expanded spot ----
        let art_sz = lerp(COLL_ART_SIZE, ALBUM_ART_SIZE, p) * s;
        let art_x = lerp(cx - coll_w * s / 2.0 + COLL_ART_PAD * s, ox + ALBUM_ART_X * s, p);
        let art_cy = lerp(COLLAPSED_H / 2.0 * s, (ALBUM_ART_Y + ALBUM_ART_SIZE / 2.0) * s, p);
        let art_y = art_cy - art_sz / 2.0;
        let art_cx = art_x + art_sz / 2.0;
        let art_r = lerp(COLL_ART_RADIUS, ALBUM_ART_RADIUS, p) * s;

        // Title (left aligned, ellipsised by GDI)
        let media_text_w = (MEDIA_TEXT_W * s).round() as i32;
        let title_x0 = (ox + MEDIA_TEXT_X * s).round() as i32;
        let title_h = (self.media_title_hh / SS) as f32;
        let title_top = MEDIA_TITLE_CY * s + slide - title_h / 2.0;
        let title_dy = (title_top * SS as f32).round() as i32;

        // Slider
        let sl_x0 = ox + SLIDER_X0 * s;
        let sl_x1 = ox + SLIDER_X1 * s;
        let sl_y = SLIDER_Y * s + slide;
        let sl_px = lerp(sl_x0, sl_x1, progress);
        let sl_th = SLIDER_H * s / 2.0;

        // Buttons (SVG icon masks)
        let cx_prev = ox + CX_PREV * s;
        let cx_play = ox + CX_PLAY * s;
        let cx_next = ox + CX_NEXT * s;
        let btn_cy = CY_BTN * s + slide;
        let icons = &self.icons;
        let ic_n = icons.n;
        let ic_half = ic_n as f32 / 2.0;
        let play_mask: &[u8] = if self.media_is_playing { &icons.pause } else { &icons.play };

        // Collapsed audio visualizer (anchored inside the collapsed pill)
        let collapsed_fade = (1.0 - fade).clamp(0.0, 1.0);
        let show_viz = has_media && collapsed_fade > 0.0;
        let viz_cx = cx + (coll_w / 2.0 - VIZ_RIGHT_PAD) * s;
        let viz_cy = COLLAPSED_H / 2.0 * s;
        let (viz_alpha, eq_h1, eq_h2, eq_h3) = if media.is_playing {
            (
                1.0f32,
                (5.0 + 8.0 * ((time_sec * 8.0).sin() * 0.5 + 0.5)) * s,
                (8.0 + 12.0 * ((time_sec * 11.5 + 1.2).sin() * 0.5 + 0.5)) * s,
                (5.0 + 8.0 * ((time_sec * 9.2 + 2.5).sin() * 0.5 + 0.5)) * s,
            )
        } else {
            (0.5f32, 5.0 * s, 10.0 * s, 5.0 * s)
        };

        let x0 = ((cx - pw / 2.0).floor() as i32 - 1).max(0);
        let x1 = ((cx + pw / 2.0).ceil() as i32 + 1).min(w - 1);
        let y1 = (ph.ceil() as i32 + 1).min(h - 1);
        let hw = w * SS;

        const TRACK: (f32, f32, f32) = (112.0, 112.0, 120.0);

        for y in 0..=y1 {
            let in_time = y >= time_rows.0 && y <= time_rows.1;
            let in_date = fade > 0.0 && y >= date_rows.0 && y <= date_rows.1;
            let fy = y as f32 + 0.5;

            let in_art_y = has_media && (fy - art_cy).abs() < art_sz / 2.0 + 1.5;
            let in_viz_y = show_viz && (fy - viz_cy).abs() < 14.0 * s;
            let in_title_y = show_media && fy > title_top - 2.0 && fy < title_top + title_h + 2.0;
            let in_slider_y = show_media && (fy - sl_y).abs() < 9.0 * s;
            let in_btn_y = show_media && (fy - btn_cy).abs() < ic_half + 1.0;

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

                let mut r = 0.0f32;
                let mut g = 0.0f32;
                let mut b = 0.0f32;

                // ---- CPU / RAM rings with percentage and label ----
                if fade > 0.0 && x >= ring_x0 && x <= ring_x1 {
                    for rg in rings.iter() {
                        if (fy - rg.cy).abs() >= ring_half_h {
                            continue;
                        }
                        let (arc_cov, track_cov) =
                            sample_progress_circle(fx, fy, cx_ring, rg.cy, r_ring, thickness, rg.pct);
                        if track_cov > 0.0 || arc_cov > 0.0 {
                            let eff_track = track_cov * (1.0 - arc_cov) * fade;
                            let eff_arc = arc_cov * fade;
                            r += rg.col.0 * eff_arc + TRACK.0 * eff_track;
                            g += rg.col.1 * eff_arc + TRACK.1 * eff_track;
                            b += rg.col.2 * eff_arc + TRACK.2 * eff_track;
                        }

                        if x >= pct_x0 && x < pct_x0 + pct_box_w {
                            let c = sample(rg.pct_mask, rg.pct_hw, rg.pct_hh, x - pct_x0, y, rg.pct_dy) * fade;
                            if c > 0.0 {
                                r += 245.0 * c;
                                g += 245.0 * c;
                                b += 250.0 * c;
                            }
                        }

                        if x >= lbl_x0 && x < lbl_x0 + lbl_box_w {
                            let c = sample(rg.lbl_mask, rg.lbl_hw, rg.lbl_hh, x - lbl_x0, y, rg.lbl_dy) * fade;
                            if c > 0.0 {
                                r += 155.0 * c;
                                g += 160.0 * c;
                                b += 170.0 * c;
                            }
                        }
                    }
                }

                // ---- Clock & date (always visible) ----
                if in_time {
                    let c = 255.0 * sample(&self.time_mask, hw, self.time_hh, x, y, time_dy);
                    r += c;
                    g += c;
                    b += c;
                }
                if in_date {
                    let c = date_grey * sample(&self.date_mask, hw, self.date_hh, x, y, date_dy);
                    r += c;
                    g += c;
                    b += c;
                }

                // ---- Album art (collapsed: left of the clock, expanded: left column) ----
                if in_art_y && fx > art_x - 1.5 && fx < art_x + art_sz + 1.5 {
                    let art_cov = sdf_rounded_box_cov(fx, fy, art_cx, art_cy, art_sz / 2.0, art_sz / 2.0, art_r);
                    if art_cov > 0.0 {
                        if let Some(ref pix) = self.thumb_pixels {
                            let (pr, pg, pb) = sample_thumb(pix, (fx - art_x) / art_sz, (fy - art_y) / art_sz);
                            r += pr * art_cov;
                            g += pg * art_cov;
                            b += pb * art_cov;
                        } else {
                            // No art: light placeholder with a darker note
                            let nc = sample_note_icon(fx, fy, art_cx, art_cy, art_sz * 0.24);
                            let base = 214.0 - 110.0 * nc;
                            r += base * art_cov;
                            g += base * art_cov;
                            b += (base + 4.0) * art_cov;
                        }
                    }
                }

                // ---- Collapsed audio visualizer (inside the pill, right side) ----
                if in_viz_y && (fx - viz_cx).abs() < 12.0 * s {
                    let bw = 3.2 * s;
                    let br = bw / 2.0;
                    let gap = 6.6 * s;
                    let b1 = sample_eq_bar(fx, fy, viz_cx - gap, viz_cy, bw, eq_h1, br);
                    let b2 = sample_eq_bar(fx, fy, viz_cx,       viz_cy, bw, eq_h2, br);
                    let b3 = sample_eq_bar(fx, fy, viz_cx + gap, viz_cy, bw, eq_h3, br);
                    let tot = (b1 + b2 + b3).min(1.0) * collapsed_fade * viz_alpha;
                    if tot > 0.0 {
                        r += 30.0 * tot;
                        g += 176.0 * tot;
                        b += 255.0 * tot;
                    }
                }

                // ---- Title ----
                if in_title_y && x >= title_x0 && x < title_x0 + media_text_w {
                    let lx = x - title_x0;
                    let c = sample(&self.media_title_mask, self.media_title_hw, self.media_title_hh, lx, y, title_dy) * fade;
                    if c > 0.0 {
                        r += 245.0 * c;
                        g += 245.0 * c;
                        b += 250.0 * c;
                    }
                }

                // ---- Slider ----
                if in_slider_y && fx > sl_x0 - 7.0 * s && fx < sl_x1 + 7.0 * s {
                    let track = sdf_rounded_box_cov(fx, fy, (sl_x0 + sl_x1) / 2.0, sl_y, (sl_x1 - sl_x0) / 2.0, sl_th, sl_th);
                    let fill_hw = (sl_px - sl_x0) / 2.0;
                    let filled = if fill_hw > 0.5 {
                        sdf_rounded_box_cov(fx, fy, (sl_x0 + sl_px) / 2.0, sl_y, fill_hw, sl_th, sl_th)
                    } else {
                        0.0
                    };
                    let knob_r = (if hover_btn == 4 { 5.5 } else { 4.5 }) * s;
                    let knob = sdf_circle_cov(fx, fy, sl_px, sl_y, knob_r);
                    let base = (130.0 * (1.0 - filled) + 235.0 * filled) * track;
                    let v = (base * (1.0 - knob) + 255.0 * knob) * fade;
                    if v > 0.0 {
                        r += v;
                        g += v;
                        b += v;
                    }
                }

                // ---- Controls (SVG icon masks) ----
                if in_btn_y && fx > cx_prev - ic_half - 1.0 && fx < cx_next + ic_half + 1.0 {
                    if hover_btn == 1 {
                        let a = 0.18 * fade * sample_button_circle(fx, fy, cx_prev, btn_cy, BTN_SEC_R * s);
                        r += 255.0 * a;
                        g += 255.0 * a;
                        b += 255.0 * a;
                    }
                    if hover_btn == 2 {
                        let a = 0.18 * fade * sample_button_circle(fx, fy, cx_play, btn_cy, BTN_HIT_R * s);
                        r += 255.0 * a;
                        g += 255.0 * a;
                        b += 255.0 * a;
                    }
                    if hover_btn == 3 {
                        let a = 0.18 * fade * sample_button_circle(fx, fy, cx_next, btn_cy, BTN_SEC_R * s);
                        r += 255.0 * a;
                        g += 255.0 * a;
                        b += 255.0 * a;
                    }

                    let ic = sample_icon(&icons.prev, ic_n, cx_prev - ic_half, btn_cy - ic_half, fx, fy)
                        + sample_icon(play_mask, ic_n, cx_play - ic_half, btn_cy - ic_half, fx, fy)
                        + sample_icon(&icons.next, ic_n, cx_next - ic_half, btn_cy - ic_half, fx, fy);
                    let ic = ic.min(1.0) * fade;
                    if ic > 0.0 {
                        r += 255.0 * ic;
                        g += 255.0 * ic;
                        b += 255.0 * ic;
                    }
                }

                let ro = (r.min(255.0) * cov + 0.5) as u32;
                let go = (g.min(255.0) * cov + 0.5) as u32;
                let bo = (b.min(255.0) * cov + 0.5) as u32;
                let ao = (cov * 255.0 + 0.5) as u32;
                buf[(y * w + x) as usize] = (ao << 24) | (ro << 16) | (go << 8) | bo;
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

        // Only one island at a time: a second launch exits immediately.
        let _instance = CreateMutexW(None, BOOL(0), w!("Local\\AtollSingleInstance"))?;
        if GetLastError() == ERROR_ALREADY_EXISTS {
            return Ok(());
        }

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

        let mut sys_mon = SystemMonitor::new();
        let mut target_cpu = sys_mon.cpu_usage;
        let mut target_ram = sys_mon.ram_usage;
        let mut disp_cpu = target_cpu;
        let mut disp_ram = target_ram;

        let media_ctrl = MediaController::new();
        let mut last_media_info = media_ctrl.get_info();
        let mut last_hover_btn = HOVER_BTN.load(Ordering::Relaxed);

        let mut r = Renderer::new(win_w, win_h, s);
        r.draw(hwnd, 0.0, disp_cpu, disp_ram, &last_media_info, last_hover_btn, 0.0, 0.0); // layered windows show nothing until the first UpdateLayeredWindow
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);

        let mut freq = 0i64;
        let _ = QueryPerformanceFrequency(&mut freq);
        let freq = freq.max(1) as f64;

        // Spring state: position (progress) and velocity.
        let mut pos = 0.0f32;
        let mut vel = 0.0f32;
        let mut last = qpc();
        let mut was_animating = false;
        let mut drag_release: Option<i64> = None; // when the slider drag ended (keeps the knob put until the player catches up)
        let mut music_w = 0.0f32; // eases the collapsed pill wider while media is present
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
            let time_sec = (now as f64 / freq) as f32;
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

            // Sample CPU and RAM periodically
            if sys_mon.update(now, freq) {
                target_cpu = sys_mon.cpu_usage;
                target_ram = sys_mon.ram_usage;
            }

            // Smooth the displayed CPU and RAM values while visible
            let metrics_moving = if pos > 0.01 {
                let cpu_diff = (target_cpu - disp_cpu).abs();
                let ram_diff = (target_ram - disp_ram).abs();
                if cpu_diff > 0.002 {
                    disp_cpu += (target_cpu - disp_cpu) * (dt * 10.0).min(1.0);
                } else {
                    disp_cpu = target_cpu;
                }
                if ram_diff > 0.002 {
                    disp_ram += (target_ram - disp_ram) * (dt * 10.0).min(1.0);
                } else {
                    disp_ram = target_ram;
                }
                cpu_diff > 0.002 || ram_diff > 0.002
            } else {
                disp_cpu = target_cpu;
                disp_ram = target_ram;
                false
            };

            let mut media_info = media_ctrl.get_info();
            if !DRAGGING.load(Ordering::Relaxed) && DRAG_FRAC.load(Ordering::Relaxed) != 0 {
                let t0 = *drag_release.get_or_insert(now);
                if (now - t0) as f64 / freq > 0.6 {
                    DRAG_FRAC.store(0, Ordering::Relaxed);
                    drag_release = None;
                }
            } else {
                drag_release = None;
            }
            let drag_raw = DRAG_FRAC.load(Ordering::Relaxed);
            if drag_raw != 0 && media_info.duration_secs > 0.5 {
                media_info.position_secs = (drag_raw - 1) as f32 / 1000.0 * media_info.duration_secs;
                media_info.sampled_at = None;
            }
            let hover_btn = HOVER_BTN.load(Ordering::Relaxed);
            MEDIA_ACTIVE.store(media_info.has_session && !media_info.title.is_empty(), Ordering::Relaxed);

            let want_w = if media_info.has_session && !media_info.title.is_empty() { 1.0f32 } else { 0.0 };
            let mw_moving = (want_w - music_w).abs() > 1e-3;
            if mw_moving {
                music_w += (want_w - music_w) * (dt * 14.0).min(1.0);
                if (want_w - music_w).abs() <= 1e-3 {
                    music_w = want_w;
                }
            }

            let media_changed = media_info.title != last_media_info.title
                || media_info.artist != last_media_info.artist
                || media_info.is_playing != last_media_info.is_playing
                || media_info.has_session != last_media_info.has_session
                || thumb_changed(&media_info.thumbnail, &last_media_info.thumbnail)
                || hover_btn != last_hover_btn;

            last_media_info = media_info.clone();
            last_hover_btn = hover_btn;

            let eq_animating = media_info.has_session && media_info.is_playing && pos < 0.95;
            let animating = unsettled || (pos > 0.01 && metrics_moving) || eq_animating || mw_moving;
            // Keep the slider ticking while expanded and playing (idle loop wakes every 200 ms).
            let slider_live = (pos > 0.5 && media_info.has_session && media_info.is_playing) || drag_raw != 0;

            // Clock text changes once a minute; check cheaply and redraw if needed.
            let t = GetLocalTime();
            let minute_key = t.wHour as u32 * 60 + t.wMinute as u32;

            if animating || was_animating || media_changed || slider_live || minute_key != last_minute_key {
                last_minute_key = minute_key;
                r.draw(
                    hwnd,
                    pos.clamp(0.0, 1.0),
                    disp_cpu,
                    disp_ram,
                    &media_info,
                    hover_btn,
                    time_sec,
                    music_w,
                );
            }
            was_animating = animating;

            if animating {
                // Block until the next compositor vblank: real vsync pacing.
                if DwmFlush().is_err() {
                    Sleep(8);
                }
            } else {
                // Idle: sleep until input arrives or 200 ms pass (for monitoring and minute rollover).
                MsgWaitForMultipleObjects(None, false, 200, QS_ALLINPUT);
            }
        }
    }
}

/// Centre the (fixed-size) window at the top of the primary monitor.
unsafe fn reposition(hwnd: HWND) {
    let s = GetDpiForSystem() as f32 / 96.0;
    let win_w = (EXPANDED_W * s).ceil() as i32 + 2;
    let x = (GetSystemMetrics(SM_CXSCREEN) - win_w) / 2;
    let y = (TOP_MARGIN * s).round() as i32;
    let _ = SetWindowPos(hwnd, None, x, y, 0, 0, SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE);
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

                let s = GetDpiForSystem() as f32 / 96.0;
                let mx = (lparam.0 & 0xFFFF) as i16 as f32 / s;
                let my = ((lparam.0 >> 16) & 0xFFFF) as i16 as f32 / s;
                let mut btn = 0u8;
                if MEDIA_ACTIVE.load(Ordering::Relaxed) {
                    if (my - CY_BTN).abs() <= BTN_HIT_R {
                        if (mx - CX_PREV).abs() <= BTN_SEC_R {
                            btn = 1; // Prev
                        } else if (mx - CX_PLAY).abs() <= BTN_HIT_R {
                            btn = 2; // Play/Pause
                        } else if (mx - CX_NEXT).abs() <= BTN_SEC_R {
                            btn = 3; // Next
                        }
                    } else if (my - SLIDER_Y).abs() <= 8.0 && mx >= SLIDER_X0 - 4.0 && mx <= SLIDER_X1 + 4.0 {
                        btn = 4; // Seek slider
                    }
                }
                if DRAGGING.load(Ordering::Relaxed) {
                    let pm = (slider_frac(lparam) * 1000.0).round() as u32 + 1;
                    DRAG_FRAC.store(pm, Ordering::Relaxed);
                    btn = 4;
                }
                HOVER_BTN.store(btn, Ordering::Relaxed);
                LRESULT(0)
            }
            WM_MOUSELEAVE => {
                TRACKING.store(false, Ordering::Relaxed);
                if DRAGGING.load(Ordering::Relaxed) {
                    return LRESULT(0); // keep the island open while dragging
                }
                HOVER_BTN.store(0, Ordering::Relaxed);
                if !PINNED.load(Ordering::Relaxed) {
                    TARGET_EXPANDED.store(false, Ordering::Relaxed);
                }
                LRESULT(0)
            }
            WM_DISPLAYCHANGE => {
                // Resolution or monitor layout changed: keep the island centred.
                reposition(hwnd);
                LRESULT(0)
            }
            WM_LBUTTONDOWN => {
                PRESSED_BTN.store(HOVER_BTN.load(Ordering::Relaxed), Ordering::Relaxed);
                if HOVER_BTN.load(Ordering::Relaxed) == 4 && MEDIA_ACTIVE.load(Ordering::Relaxed) {
                    DRAGGING.store(true, Ordering::Relaxed);
                    DRAG_FRAC.store((slider_frac(lparam) * 1000.0).round() as u32 + 1, Ordering::Relaxed);
                    let _ = SetCapture(hwnd);
                }
                LRESULT(0)
            }
            WM_LBUTTONUP => {
                if DRAGGING.swap(false, Ordering::Relaxed) {
                    let _ = ReleaseCapture();
                    PRESSED_BTN.store(NO_PRESS, Ordering::Relaxed);
                    let pm = (slider_frac(lparam) * 1000.0).round() as u32 + 1;
                    MEDIA_SEEK.store(pm, Ordering::Relaxed);
                    DRAG_FRAC.store(pm, Ordering::Relaxed);
                    TRACKING.store(false, Ordering::Relaxed);
                    let mut pt = POINT::default();
                    if GetCursorPos(&mut pt).is_ok()
                        && WindowFromPoint(pt) != hwnd
                        && !PINNED.load(Ordering::Relaxed)
                    {
                        TARGET_EXPANDED.store(false, Ordering::Relaxed);
                        HOVER_BTN.store(0, Ordering::Relaxed);
                    }
                    return LRESULT(0);
                }
                let btn = HOVER_BTN.load(Ordering::Relaxed);
                // A click needs both press and release on the same target; releasing over the
                // island after pressing elsewhere (e.g. dropping a drag) is ignored.
                if PRESSED_BTN.swap(NO_PRESS, Ordering::Relaxed) != btn {
                    return LRESULT(0);
                }
                match btn {
                    1 => push_media_cmd(CMD_PREV),
                    2 => push_media_cmd(CMD_TOGGLE),
                    3 => push_media_cmd(CMD_NEXT),
                    4 => {} // slider clicks are handled as drags (WM_LBUTTONDOWN starts the drag)
                    _ => {
                        let p = !PINNED.load(Ordering::Relaxed);
                        PINNED.store(p, Ordering::Relaxed);
                        TARGET_EXPANDED.store(p || TRACKING.load(Ordering::Relaxed), Ordering::Relaxed);
                    }
                }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_progress_circle_full_and_empty() {
        // Center at (50, 50), radius 20, thickness 4
        // At progress 0.0, active arc coverage should be 0.0
        let (arc, track) = sample_progress_circle(50.0, 30.0, 50.0, 50.0, 20.0, 4.0, 0.0);
        assert_eq!(arc, 0.0);
        assert!(track > 0.0);

        // At progress 1.0 (full circle), top point should be covered by both arc and track
        let (arc, track) = sample_progress_circle(50.0, 30.0, 50.0, 50.0, 20.0, 4.0, 1.0);
        assert!(arc > 0.0);
        assert!(track > 0.0);

        // Outside ring: far away (0, 0)
        let (arc, track) = sample_progress_circle(0.0, 0.0, 50.0, 50.0, 20.0, 4.0, 0.5);
        assert_eq!(arc, 0.0);
        assert_eq!(track, 0.0);
    }

    #[test]
    fn test_progress_circle_half() {
        // 50% progress: arc spans 12 o'clock clockwise to 6 o'clock (right half).
        // 3 o'clock point (70.0, 50.0) should have high arc coverage.
        let (arc_right, _) = sample_progress_circle(70.0, 50.0, 50.0, 50.0, 20.0, 4.0, 0.5);
        assert!(arc_right > 0.5, "Right half at 3 o'clock should have high arc coverage");

        // 9 o'clock point (30.0, 50.0) is on the unreached half -> arc should be 0.0, track > 0.0.
        let (arc_left, track_left) = sample_progress_circle(30.0, 50.0, 50.0, 50.0, 20.0, 4.0, 0.5);
        assert_eq!(arc_left, 0.0, "Left half at 9 o'clock should have 0 arc coverage");
        assert!(track_left > 0.5, "Left half at 9 o'clock should have track coverage");
    }

    #[test]
    fn test_filetime_conversion() {
        let ft = FILETIME {
            dwLowDateTime: 0x1234_5678,
            dwHighDateTime: 0x0000_0001,
        };
        assert_eq!(filetime_to_u64(&ft), 0x0000_0001_1234_5678);
    }

    #[test]
    fn test_eq_bar_and_button_circle() {
        // Center of circle at (100, 100), radius 10
        let cov_in = sample_button_circle(100.0, 100.0, 100.0, 100.0, 10.0);
        let cov_out = sample_button_circle(150.0, 150.0, 100.0, 100.0, 10.0);
        assert!(cov_in > 0.5);
        assert_eq!(cov_out, 0.0);

        // Eq bar at (50, 50), width 4, height 10, radius 1
        let bar_in = sample_eq_bar(50.0, 50.0, 50.0, 50.0, 4.0, 10.0, 1.0);
        let bar_out = sample_eq_bar(80.0, 80.0, 50.0, 50.0, 4.0, 10.0, 1.0);
        assert!(bar_in > 0.5);
        assert_eq!(bar_out, 0.0);
    }

    #[test]
    fn test_svg_path_absolute_square() {
        let polys = parse_svg_path("M0 0 L10 0 L10 10 L0 10 Z");
        assert_eq!(polys.len(), 1);
        assert_eq!(polys[0].len(), 4);
        assert_ne!(winding(&polys, 5.0, 5.0), 0);
        assert_eq!(winding(&polys, 15.0, 5.0), 0);
    }

    #[test]
    fn test_svg_path_relative_triangle() {
        let polys = parse_svg_path("m0 0 l10 0 l0 10 z");
        assert_eq!(polys.len(), 1);
        assert_ne!(winding(&polys, 7.0, 2.0), 0);
        assert_eq!(winding(&polys, 2.0, 7.0), 0);
    }

    #[test]
    fn test_rounded_rect_path_and_raster() {
        let d = rounded_rect(4.0, 4.0, 20.0, 20.0, 1.5);
        let polys = parse_svg_path(&d);
        assert_ne!(winding(&polys, 12.0, 12.0), 0);
        assert_eq!(winding(&polys, 1.0, 1.0), 0);

        let mask = rasterize_icon(&d, 24, 24.0);
        assert_eq!(mask.len(), 24 * 24);
        assert_eq!(mask[12 * 24 + 12], 255, "centre should be fully covered");
        assert_eq!(mask[0], 0, "corner outside the shape should be empty");
    }

    #[test]
    fn test_sample_thumb() {
        let n = THUMB_SIZE as usize;
        // BGRA = (10, 20, 30, 255) everywhere -> (r, g, b) = (30, 20, 10)
        let pix: Vec<u8> = (0..n * n).flat_map(|_| [10u8, 20, 30, 255]).collect();
        assert_eq!(sample_thumb(&pix, 0.5, 0.5), (30.0, 20.0, 10.0));
        assert_eq!(sample_thumb(&pix, 0.0, 1.0), (30.0, 20.0, 10.0));
        // Too-short buffers must not panic.
        assert_eq!(sample_thumb(&[0u8; 4], 0.5, 0.5), (0.0, 0.0, 0.0));
    }

    #[test]
    fn test_thumb_changed() {
        let a = Some(Arc::new(vec![1u8]));
        let b = Some(Arc::new(vec![1u8]));
        assert!(!thumb_changed(&a, &a.clone()));
        assert!(thumb_changed(&a, &b), "different allocations count as changed");
        assert!(thumb_changed(&a, &None));
        assert!(!thumb_changed(&None, &None));
    }

    #[test]
    fn test_now_filetime_is_plausible() {
        // 2020-01-01 is about 1.3217e17 in FILETIME ticks.
        assert!(now_filetime() > 132_000_000_000_000_000);
    }

    #[test]
    fn test_media_cmd_queue_keeps_every_click() {
        MEDIA_CMDS.lock().unwrap().clear();
        push_media_cmd(CMD_NEXT);
        push_media_cmd(CMD_NEXT);
        let drained = std::mem::take(&mut *MEDIA_CMDS.lock().unwrap());
        assert_eq!(drained, vec![CMD_NEXT, CMD_NEXT]);
    }
}
