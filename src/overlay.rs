//! Win32 overlay windows: region selector, recording border and the floating stop bar.
//! All of them live on the tray thread and report back by posting messages to `notify`.

use std::sync::Once;
use std::time::Instant;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture, SetFocus, VK_ESCAPE, VK_RETURN};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::capture::Region;

/// lParam = `Box<Region>` raw pointer, or 0 if cancelled.
pub const WM_APP_SELECTED: u32 = WM_APP + 1;
pub const WM_APP_STOP: u32 = WM_APP + 2;

const BORDER: i32 = 2;
const WM_MOUSELEAVE: u32 = 0x02A3;
const KEY: COLORREF = COLORREF(0x00FF00FF);

fn rgb(r: u8, g: u8, b: u8) -> COLORREF {
    COLORREF(r as u32 | (g as u32) << 8 | (b as u32) << 16)
}

fn hinstance() -> HINSTANCE {
    unsafe { GetModuleHandleW(None).map(|h| HINSTANCE(h.0)).unwrap_or_default() }
}

fn register(name: PCWSTR, proc_: WNDPROC, cursor: PCWSTR) {
    unsafe {
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: proc_,
            hInstance: hinstance(),
            hCursor: LoadCursorW(None, cursor).unwrap_or_default(),
            lpszClassName: name,
            ..Default::default()
        };
        RegisterClassExW(&wc);
    }
}

unsafe fn state<T>(hwnd: HWND) -> Option<&'static mut T> {
    let p = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut T;
    p.as_mut()
}

unsafe fn free_state<T>(hwnd: HWND) {
    let p = SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0) as *mut T;
    if !p.is_null() {
        drop(Box::from_raw(p));
    }
}

fn exclude_from_capture(hwnd: HWND) {
    unsafe {
        let _ = SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE);
    }
}

fn ui_font(px: i32, bold: bool) -> HFONT {
    unsafe {
        CreateFontW(
            -px, 0, 0, 0,
            if bold { FW_SEMIBOLD.0 as i32 } else { FW_NORMAL.0 as i32 },
            0, 0, 0,
            DEFAULT_CHARSET, OUT_DEFAULT_PRECIS, CLIP_DEFAULT_PRECIS, CLEARTYPE_QUALITY,
            0, w!("Segoe UI"),
        )
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

// ───────────────────────────── Region selector ─────────────────────────────

struct Selector {
    notify: HWND,
    /// Virtual-screen origin; client (0,0) maps to it.
    vx: i32,
    vy: i32,
    bright: HBITMAP,
    dim: HBITMAP,
    mem: HDC,
    monitors: Vec<(HMONITOR, RECT)>,
    drag: Option<(POINT, POINT, RECT)>, // start, current, monitor (client coords)
    font: HFONT,
    hint_font: HFONT,
    done: bool,
}

/// Freezes the screen and lets the user drag a rectangle. Posts `WM_APP_SELECTED` to `notify`.
pub fn select_region(notify: HWND) -> windows::core::Result<HWND> {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| register(w!("s2i_selector"), Some(selector_proc), IDC_CROSS));

    unsafe {
        let vx = GetSystemMetrics(SM_XVIRTUALSCREEN);
        let vy = GetSystemMetrics(SM_YVIRTUALSCREEN);
        let vw = GetSystemMetrics(SM_CXVIRTUALSCREEN);
        let vh = GetSystemMetrics(SM_CYVIRTUALSCREEN);

        // Snapshot the desktop, plus a darkened copy for the unselected area.
        let screen = GetDC(None);
        let mem = CreateCompatibleDC(Some(screen));
        let bright = CreateCompatibleBitmap(screen, vw, vh);
        let dim = CreateCompatibleBitmap(screen, vw, vh);
        let old = SelectObject(mem, bright.into());
        let _ = BitBlt(mem, 0, 0, vw, vh, Some(screen), vx, vy, SRCCOPY | CAPTUREBLT);

        let tmp = CreateCompatibleDC(Some(screen));
        SelectObject(tmp, dim.into());
        let _ = BitBlt(tmp, 0, 0, vw, vh, Some(mem), 0, 0, SRCCOPY);
        let black = CreateCompatibleDC(Some(screen));
        let one = CreateCompatibleBitmap(screen, 1, 1);
        SelectObject(black, one.into());
        SetPixel(black, 0, 0, COLORREF(0));
        let blend = BLENDFUNCTION { BlendOp: AC_SRC_OVER as u8, BlendFlags: 0, SourceConstantAlpha: 120, AlphaFormat: 0 };
        let _ = GdiAlphaBlend(tmp, 0, 0, vw, vh, black, 0, 0, 1, 1, blend);
        let _ = DeleteDC(black);
        let _ = DeleteObject(one.into());
        let _ = DeleteDC(tmp);
        SelectObject(mem, old);
        ReleaseDC(None, screen);

        let mut monitors: Vec<(HMONITOR, RECT)> = Vec::new();
        unsafe extern "system" fn enum_mon(m: HMONITOR, _: HDC, r: *mut RECT, data: LPARAM) -> windows::core::BOOL {
            let v = &mut *(data.0 as *mut Vec<(HMONITOR, RECT)>);
            v.push((m, *r));
            TRUE
        }
        let _ = EnumDisplayMonitors(None, None, Some(enum_mon), LPARAM(&mut monitors as *mut _ as isize));
        for (_, r) in monitors.iter_mut() {
            let _ = OffsetRect(r, -vx, -vy);
        }

        let st = Box::new(Selector {
            notify,
            vx,
            vy,
            bright,
            dim,
            mem,
            monitors,
            drag: None,
            font: ui_font(15, true),
            hint_font: ui_font(16, false),
            done: false,
        });

        let hwnd = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
            w!("s2i_selector"),
            w!("screen2imgur region"),
            WS_POPUP,
            vx, vy, vw, vh,
            None, None, Some(hinstance()), None,
        )?;
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, Box::into_raw(st) as isize);
        exclude_from_capture(hwnd);
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetForegroundWindow(hwnd);
        let _ = SetFocus(Some(hwnd));
        Ok(hwnd)
    }
}

fn sel_rect(start: POINT, cur: POINT) -> RECT {
    RECT {
        left: start.x.min(cur.x),
        top: start.y.min(cur.y),
        right: start.x.max(cur.x),
        bottom: start.y.max(cur.y),
    }
}

fn mouse_pt(lp: LPARAM) -> POINT {
    POINT { x: (lp.0 & 0xFFFF) as u16 as i16 as i32, y: ((lp.0 >> 16) & 0xFFFF) as u16 as i16 as i32 }
}

fn clamp_pt(p: POINT, r: &RECT) -> POINT {
    POINT { x: p.x.clamp(r.left, r.right), y: p.y.clamp(r.top, r.bottom) }
}

/// Area to repaint for a selection (rect + border + label).
fn sel_dirty(r: &RECT) -> RECT {
    RECT { left: r.left - 8, top: r.top - 40, right: r.right + 160, bottom: r.bottom + 40 }
}

unsafe fn finish_select(hwnd: HWND, st: &mut Selector, region: Option<Region>) {
    if st.done {
        return;
    }
    st.done = true;
    let lp = region.map(|r| Box::into_raw(Box::new(r)) as isize).unwrap_or(0);
    let _ = PostMessageW(Some(st.notify), WM_APP_SELECTED, WPARAM(0), LPARAM(lp));
    let _ = DestroyWindow(hwnd);
}

/// Turns a client-space rectangle into a capture region on `mon`.
fn make_region(st: &Selector, mon_handle: HMONITOR, mon: RECT, mut r: RECT) -> Region {
    // A click (or tiny drag) selects the whole monitor.
    if r.right - r.left < 8 && r.bottom - r.top < 8 {
        r = mon;
    }
    // H.264 needs even dimensions; keep a sane minimum.
    let min = 64;
    let mut w = (r.right - r.left).max(min).min(mon.right - mon.left);
    let mut h = (r.bottom - r.top).max(min).min(mon.bottom - mon.top);
    w &= !1;
    h &= !1;
    let x = r.left.min(mon.right - w).max(mon.left);
    let y = r.top.min(mon.bottom - h).max(mon.top);
    Region {
        monitor: mon_handle.0 as isize,
        x: x + st.vx,
        y: y + st.vy,
        w: w as u32,
        h: h as u32,
        mon_x: mon.left + st.vx,
        mon_y: mon.top + st.vy,
    }
}

unsafe extern "system" fn selector_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let Some(st) = state::<Selector>(hwnd) else {
        return DefWindowProcW(hwnd, msg, wp, lp);
    };
    match msg {
        WM_LBUTTONDOWN => {
            let p = mouse_pt(lp);
            let mon = st
                .monitors
                .iter()
                .find(|(_, r)| PtInRect(r, p).as_bool())
                .map(|(_, r)| *r)
                .unwrap_or(st.monitors.first().map(|m| m.1).unwrap_or_default());
            st.drag = Some((p, p, mon));
            SetCapture(hwnd);
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            if let Some((start, cur, mon)) = st.drag {
                let p = clamp_pt(mouse_pt(lp), &mon);
                if p != cur {
                    let old = sel_dirty(&sel_rect(start, cur));
                    let new = sel_dirty(&sel_rect(start, p));
                    let mut u = RECT::default();
                    let _ = UnionRect(&mut u, &old, &new);
                    let _ = InvalidateRect(Some(hwnd), Some(&u), false);
                    st.drag = Some((start, p, mon));
                }
            }
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            let _ = ReleaseCapture();
            if let Some((start, _, mon)) = st.drag.take() {
                let p = clamp_pt(mouse_pt(lp), &mon);
                let handle = st
                    .monitors
                    .iter()
                    .find(|(_, r)| *r == mon)
                    .map(|(h, _)| *h)
                    .unwrap_or_default();
                let region = make_region(st, handle, mon, sel_rect(start, p));
                finish_select(hwnd, st, Some(region));
            }
            LRESULT(0)
        }
        WM_RBUTTONDOWN => {
            finish_select(hwnd, st, None);
            LRESULT(0)
        }
        WM_KEYDOWN => {
            if wp.0 == VK_ESCAPE.0 as usize {
                finish_select(hwnd, st, None);
            } else if wp.0 == VK_RETURN.0 as usize {
                // Enter = whole monitor under the cursor.
                let mut p = POINT::default();
                let _ = GetCursorPos(&mut p);
                let p = POINT { x: p.x - st.vx, y: p.y - st.vy };
                if let Some((h, r)) = st.monitors.iter().find(|(_, r)| PtInRect(r, p).as_bool()).copied() {
                    let region = make_region(st, h, r, RECT { left: p.x, top: p.y, right: p.x, bottom: p.y });
                    finish_select(hwnd, st, Some(region));
                }
            }
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let hdc = BeginPaint(hwnd, &mut ps);
            paint_selector(hdc, ps.rcPaint, st);
            let _ = EndPaint(hwnd, &ps);
            LRESULT(0)
        }
        WM_DESTROY => {
            if !st.done {
                st.done = true;
                let _ = PostMessageW(Some(st.notify), WM_APP_SELECTED, WPARAM(0), LPARAM(0));
            }
            let _ = DeleteObject(st.bright.into());
            let _ = DeleteObject(st.dim.into());
            let _ = DeleteDC(st.mem);
            let _ = DeleteObject(st.font.into());
            let _ = DeleteObject(st.hint_font.into());
            free_state::<Selector>(hwnd);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

unsafe fn paint_selector(hdc: HDC, clip: RECT, st: &Selector) {
    let (cw, ch) = (clip.right - clip.left, clip.bottom - clip.top);
    if cw <= 0 || ch <= 0 {
        return;
    }
    // Draw into a back buffer covering just the dirty rect.
    let back = CreateCompatibleDC(Some(hdc));
    let bmp = CreateCompatibleBitmap(hdc, cw, ch);
    let old_bmp = SelectObject(back, bmp.into());
    let _ = SetViewportOrgEx(back, -clip.left, -clip.top, None);

    let old_src = SelectObject(st.mem, st.dim.into());
    let _ = BitBlt(back, clip.left, clip.top, cw, ch, Some(st.mem), clip.left, clip.top, SRCCOPY);

    // Hint at the top of each monitor.
    SetBkMode(back, TRANSPARENT);
    SelectObject(back, st.hint_font.into());
    let hint = wide("Drag to select a region   ·   Click or Enter = whole monitor   ·   Esc / right-click = cancel");
    for (_, m) in &st.monitors {
        let mut r = RECT { left: m.left, top: m.top + 24, right: m.right, bottom: m.top + 64 };
        let pill_w = 720.min(m.right - m.left - 20);
        let pill = RECT {
            left: (m.left + m.right) / 2 - pill_w / 2,
            top: r.top,
            right: (m.left + m.right) / 2 + pill_w / 2,
            bottom: r.bottom,
        };
        let br = CreateSolidBrush(rgb(24, 24, 24));
        FillRect(back, &pill, br);
        let _ = DeleteObject(br.into());
        SetTextColor(back, rgb(235, 235, 235));
        DrawTextW(back, &mut hint.clone(), &mut r, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
    }

    if let Some((start, cur, _)) = st.drag {
        let s = sel_rect(start, cur);
        SelectObject(st.mem, st.bright.into());
        let _ = BitBlt(back, s.left, s.top, s.right - s.left, s.bottom - s.top, Some(st.mem), s.left, s.top, SRCCOPY);

        let red = CreateSolidBrush(rgb(255, 64, 64));
        let outer = RECT { left: s.left - BORDER, top: s.top - BORDER, right: s.right + BORDER, bottom: s.bottom + BORDER };
        // FrameRect draws 1px; repeat inset for thickness.
        let mut f = outer;
        for _ in 0..BORDER {
            FrameRect(back, &f, red);
            let _ = InflateRect(&mut f, -1, -1);
        }
        let _ = DeleteObject(red.into());

        let w = (s.right - s.left) & !1;
        let h = (s.bottom - s.top) & !1;
        let label = wide(&format!("{w} × {h}"));
        SelectObject(back, st.font.into());
        let mut lr = RECT { left: s.left, top: s.bottom + 6, right: s.left + 150, bottom: s.bottom + 30 };
        let mut measure = lr;
        DrawTextW(back, &mut label.clone(), &mut measure, DT_CALCRECT | DT_SINGLELINE);
        lr.right = lr.left + (measure.right - measure.left) + 16;
        let bg = CreateSolidBrush(rgb(24, 24, 24));
        FillRect(back, &lr, bg);
        let _ = DeleteObject(bg.into());
        SetTextColor(back, rgb(255, 255, 255));
        DrawTextW(back, &mut label.clone(), &mut lr, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
    }
    SelectObject(st.mem, old_src);

    let _ = SetViewportOrgEx(back, 0, 0, None);
    let _ = BitBlt(hdc, clip.left, clip.top, cw, ch, Some(back), 0, 0, SRCCOPY);
    SelectObject(back, old_bmp);
    let _ = DeleteObject(bmp.into());
    let _ = DeleteDC(back);
}

// ───────────────────────────── Recording UI ─────────────────────────────

struct Bar {
    notify: HWND,
    started: Instant,
    scale: f32,
    font: HFONT,
    stop_rect: RECT,
    hover_stop: bool,
}

pub struct RecordingUi {
    border: HWND,
    bar: HWND,
}

impl RecordingUi {
    pub fn show(region: &Region, notify: HWND) -> windows::core::Result<Self> {
        static ONCE: Once = Once::new();
        ONCE.call_once(|| {
            register(w!("s2i_border"), Some(border_proc), IDC_ARROW);
            register(w!("s2i_bar"), Some(bar_proc), IDC_ARROW);
        });
        unsafe {
            let border = CreateWindowExW(
                WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_NOACTIVATE,
                w!("s2i_border"),
                w!("screen2imgur border"),
                WS_POPUP,
                region.x - BORDER,
                region.y - BORDER,
                region.w as i32 + BORDER * 2,
                region.h as i32 + BORDER * 2,
                None, None, Some(hinstance()), None,
            )?;
            SetLayeredWindowAttributes(border, KEY, 0, LWA_COLORKEY)?;
            exclude_from_capture(border);
            let _ = ShowWindow(border, SW_SHOWNOACTIVATE);

            let bar = CreateWindowExW(
                WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                w!("s2i_bar"),
                w!("screen2imgur recording"),
                WS_POPUP,
                0, 0, 10, 10,
                None, None, Some(hinstance()), None,
            )?;
            let dpi = GetDpiForWindow(bar).max(96);
            let scale = dpi as f32 / 96.0;
            let (bw, bh) = ((168.0 * scale) as i32, (34.0 * scale) as i32);

            // Below the region if it fits, else above, else inside the top-right corner.
            let mon = MonitorFromPoint(POINT { x: region.x, y: region.y }, MONITOR_DEFAULTTONEAREST);
            let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
            let _ = GetMonitorInfoW(mon, &mut mi);
            let work = mi.rcWork;
            let gap = (6.0 * scale) as i32;
            let bottom = region.y + region.h as i32 + BORDER;
            let x = (region.x + region.w as i32 - bw).clamp(work.left, (work.right - bw).max(work.left));
            let y = if bottom + gap + bh <= work.bottom {
                bottom + gap
            } else if region.y - BORDER - gap - bh >= work.top {
                region.y - BORDER - gap - bh
            } else {
                region.y + gap
            };
            let st = Box::new(Bar {
                notify,
                started: Instant::now(),
                scale,
                font: ui_font((14.0 * scale) as i32, true),
                stop_rect: RECT { left: bw - (70.0 * scale) as i32, top: (5.0 * scale) as i32, right: bw - (5.0 * scale) as i32, bottom: bh - (5.0 * scale) as i32 },
                hover_stop: false,
            });
            SetWindowLongPtrW(bar, GWLP_USERDATA, Box::into_raw(st) as isize);
            let _ = SetWindowPos(bar, Some(HWND_TOPMOST), x, y, bw, bh, SWP_NOACTIVATE);
            exclude_from_capture(bar);
            let _ = ShowWindow(bar, SW_SHOWNOACTIVATE);
            SetTimer(Some(bar), 1, 250, None);
            Ok(Self { border, bar })
        }
    }

    pub fn close(self) {
        unsafe {
            let _ = DestroyWindow(self.border);
            let _ = DestroyWindow(self.bar);
        }
    }
}

unsafe extern "system" fn border_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let hdc = BeginPaint(hwnd, &mut ps);
            let mut rc = RECT::default();
            let _ = GetClientRect(hwnd, &mut rc);
            let red = CreateSolidBrush(rgb(255, 48, 48));
            FillRect(hdc, &rc, red);
            let _ = DeleteObject(red.into());
            let hole = RECT { left: BORDER, top: BORDER, right: rc.right - BORDER, bottom: rc.bottom - BORDER };
            let key = CreateSolidBrush(KEY);
            FillRect(hdc, &hole, key);
            let _ = DeleteObject(key.into());
            let _ = EndPaint(hwnd, &ps);
            LRESULT(0)
        }
        WM_NCHITTEST => LRESULT(HTTRANSPARENT as isize),
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

unsafe extern "system" fn bar_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let Some(st) = state::<Bar>(hwnd) else {
        return DefWindowProcW(hwnd, msg, wp, lp);
    };
    match msg {
        WM_TIMER => {
            let _ = InvalidateRect(Some(hwnd), None, false);
            LRESULT(0)
        }
        WM_NCHITTEST => {
            // Drag the bar by anything except the Stop button.
            let mut p = POINT { x: (lp.0 & 0xFFFF) as i16 as i32, y: ((lp.0 >> 16) & 0xFFFF) as i16 as i32 };
            let _ = ScreenToClient(hwnd, &mut p);
            if PtInRect(&st.stop_rect, p).as_bool() {
                LRESULT(HTCLIENT as isize)
            } else {
                LRESULT(HTCAPTION as isize)
            }
        }
        WM_MOUSEMOVE => {
            if !st.hover_stop {
                st.hover_stop = true;
                let mut tme = windows::Win32::UI::Input::KeyboardAndMouse::TRACKMOUSEEVENT {
                    cbSize: std::mem::size_of::<windows::Win32::UI::Input::KeyboardAndMouse::TRACKMOUSEEVENT>() as u32,
                    dwFlags: windows::Win32::UI::Input::KeyboardAndMouse::TME_LEAVE,
                    hwndTrack: hwnd,
                    dwHoverTime: 0,
                };
                let _ = windows::Win32::UI::Input::KeyboardAndMouse::TrackMouseEvent(&mut tme);
                let _ = InvalidateRect(Some(hwnd), None, false);
            }
            LRESULT(0)
        }
        WM_MOUSELEAVE => {
            st.hover_stop = false;
            let _ = InvalidateRect(Some(hwnd), None, false);
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            let _ = PostMessageW(Some(st.notify), WM_APP_STOP, WPARAM(0), LPARAM(0));
            LRESULT(0)
        }
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        WM_ERASEBKGND => LRESULT(1),
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let hdc = BeginPaint(hwnd, &mut ps);
            let mut rc = RECT::default();
            let _ = GetClientRect(hwnd, &mut rc);
            let back = CreateCompatibleDC(Some(hdc));
            let bmp = CreateCompatibleBitmap(hdc, rc.right, rc.bottom);
            let old = SelectObject(back, bmp.into());

            let bg = CreateSolidBrush(rgb(28, 28, 30));
            FillRect(back, &rc, bg);
            let _ = DeleteObject(bg.into());

            let s = st.scale;
            // Pulsing red dot.
            let secs = st.started.elapsed().as_secs();
            let dot = CreateSolidBrush(if st.started.elapsed().as_millis() % 1000 < 600 { rgb(255, 59, 48) } else { rgb(140, 30, 26) });
            let old_brush = SelectObject(back, dot.into());
            let old_pen = SelectObject(back, GetStockObject(NULL_PEN));
            let (cx, cy, r) = ((14.0 * s) as i32, rc.bottom / 2, (5.0 * s) as i32);
            let _ = Ellipse(back, cx - r, cy - r, cx + r, cy + r);
            SelectObject(back, old_pen);
            SelectObject(back, old_brush);
            let _ = DeleteObject(dot.into());

            SetBkMode(back, TRANSPARENT);
            SelectObject(back, st.font.into());
            SetTextColor(back, rgb(240, 240, 240));
            let mut tr = RECT { left: (26.0 * s) as i32, top: 0, right: st.stop_rect.left, bottom: rc.bottom };
            let mut t = wide(&format!("{:02}:{:02}", secs / 60, secs % 60));
            DrawTextW(back, &mut t, &mut tr, DT_LEFT | DT_VCENTER | DT_SINGLELINE);

            let btn = CreateSolidBrush(if st.hover_stop { rgb(230, 60, 50) } else { rgb(200, 45, 40) });
            FillRect(back, &st.stop_rect, btn);
            let _ = DeleteObject(btn.into());
            SetTextColor(back, rgb(255, 255, 255));
            let mut br = st.stop_rect;
            let mut stop = wide("■ Stop");
            DrawTextW(back, &mut stop, &mut br, DT_CENTER | DT_VCENTER | DT_SINGLELINE);

            let _ = BitBlt(hdc, 0, 0, rc.right, rc.bottom, Some(back), 0, 0, SRCCOPY);
            SelectObject(back, old);
            let _ = DeleteObject(bmp.into());
            let _ = DeleteDC(back);
            let _ = EndPaint(hwnd, &ps);
            LRESULT(0)
        }
        WM_DESTROY => {
            let _ = KillTimer(Some(hwnd), 1);
            let _ = DeleteObject(st.font.into());
            free_state::<Bar>(hwnd);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}
