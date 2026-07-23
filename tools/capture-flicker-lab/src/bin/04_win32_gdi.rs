use std::mem::size_of;

use global_hotkey::hotkey::{Code, HotKey, Modifiers};
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};
use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, CreatePen, CreateSolidBrush,
    DeleteDC, DeleteObject, EndPaint, FillRect, GetMonitorInfoW, GetStockObject, HGDIOBJ,
    InvalidateRect, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromPoint, NULL_BRUSH,
    PAINTSTRUCT, PS_SOLID, Rectangle, SRCCOPY, SelectObject, SetBkMode, SetTextColor, TRANSPARENT,
    TextOutW, UpdateWindow,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture};
use windows::Win32::UI::WindowsAndMessaging::{
    CS_HREDRAW, CS_VREDRAW, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
    GWLP_USERDATA, GetClientRect, GetCursorPos, GetMessageW, GetWindowLongPtrW, IDC_CROSS,
    LoadCursorW, MSG, PostMessageW, PostQuitMessage, RegisterClassW, SW_HIDE, SW_SHOW,
    SWP_NOZORDER, SetForegroundWindow, SetWindowLongPtrW, SetWindowPos, ShowWindow,
    TranslateMessage, WM_APP, WM_DESTROY, WM_ERASEBKGND, WM_KEYDOWN, WM_LBUTTONDOWN, WM_LBUTTONUP,
    WM_MOUSEMOVE, WM_NCDESTROY, WM_PAINT, WNDCLASSW, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};
use windows::core::{PCWSTR, w};

const CLASS_NAME: PCWSTR = w!("SnowShotCaptureFlickerGdiLab");
const WINDOW_TITLE: PCWSTR = w!("04 · Win32 GDI 双缓冲对照组");
const WM_SHOW_CAPTURE: u32 = WM_APP + 1;
const WM_QUIT_LAB: u32 = WM_APP + 2;

#[derive(Default)]
struct State {
    anchor: POINT,
    cursor: POINT,
    selecting: bool,
    has_selection: bool,
}

struct LabHotkeys {
    _manager: GlobalHotKeyManager,
    _hotkeys: [HotKey; 2],
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // SAFETY: this is called before creating any HWND or querying monitor coordinates.
    unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) }?;
    let monitor = monitor_rect()?;
    let module = unsafe { GetModuleHandleW(None) }?;
    let cursor = unsafe { LoadCursorW(None, IDC_CROSS) }?;
    let class = WNDCLASSW {
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: Some(window_proc),
        hInstance: HINSTANCE(module.0),
        hCursor: cursor,
        lpszClassName: CLASS_NAME,
        ..Default::default()
    };
    if unsafe { RegisterClassW(&class) } == 0 {
        return Err(windows::core::Error::from_win32().into());
    }

    let width = monitor.right - monitor.left;
    let height = monitor.bottom - monitor.top;
    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
            CLASS_NAME,
            WINDOW_TITLE,
            WS_POPUP,
            monitor.left,
            monitor.top,
            width,
            height,
            None,
            None,
            Some(HINSTANCE(module.0)),
            None,
        )
    }?;
    let state = Box::into_raw(Box::<State>::default());
    unsafe {
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, state as isize);
    }
    let _hotkeys = register_hotkeys(hwnd)?;
    show_capture(hwnd, state);

    let mut message = MSG::default();
    while unsafe { GetMessageW(&mut message, None, 0, 0) }.as_bool() {
        unsafe {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
    GlobalHotKeyEvent::set_event_handler(None::<fn(GlobalHotKeyEvent)>);
    Ok(())
}

fn register_hotkeys(hwnd: HWND) -> Result<LabHotkeys, Box<dyn std::error::Error>> {
    let show_hotkey = HotKey::new(Some(Modifiers::ALT), Code::F12);
    let quit_hotkey = HotKey::new(Some(Modifiers::ALT | Modifiers::SHIFT), Code::F12);
    let manager = GlobalHotKeyManager::new()?;
    manager.register(show_hotkey)?;
    manager.register(quit_hotkey)?;

    let show_hotkey_id = show_hotkey.id();
    let quit_hotkey_id = quit_hotkey.id();
    let hwnd_value = hwnd.0 as usize;
    GlobalHotKeyEvent::set_event_handler(Some(move |event: GlobalHotKeyEvent| {
        if event.state != HotKeyState::Pressed {
            return;
        }

        let hwnd = HWND(hwnd_value as *mut core::ffi::c_void);
        if event.id == show_hotkey_id {
            // SAFETY: hwnd belongs to this process and remains live until the message loop exits.
            let _ = unsafe { PostMessageW(Some(hwnd), WM_SHOW_CAPTURE, WPARAM(0), LPARAM(0)) };
        } else if event.id == quit_hotkey_id {
            // SAFETY: hwnd belongs to this process and remains live until the message loop exits.
            let _ = unsafe { PostMessageW(Some(hwnd), WM_QUIT_LAB, WPARAM(0), LPARAM(0)) };
        }
    }));

    Ok(LabHotkeys {
        _manager: manager,
        _hotkeys: [show_hotkey, quit_hotkey],
    })
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let state_ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut State;
    match message {
        WM_LBUTTONDOWN if !state_ptr.is_null() => {
            let point = point_from_lparam(lparam);
            let state = unsafe { &mut *state_ptr };
            state.anchor = point;
            state.cursor = point;
            state.selecting = true;
            state.has_selection = false;
            unsafe {
                SetCapture(hwnd);
                let _ = InvalidateRect(Some(hwnd), None, false);
            }
            LRESULT(0)
        }
        WM_MOUSEMOVE if !state_ptr.is_null() => {
            let state = unsafe { &mut *state_ptr };
            if state.selecting {
                state.cursor = point_from_lparam(lparam);
                unsafe {
                    let _ = InvalidateRect(Some(hwnd), None, false);
                }
            }
            LRESULT(0)
        }
        WM_LBUTTONUP if !state_ptr.is_null() => {
            let state = unsafe { &mut *state_ptr };
            if state.selecting {
                state.cursor = point_from_lparam(lparam);
                state.selecting = false;
                state.has_selection = true;
                unsafe {
                    let _ = ReleaseCapture();
                    let _ = InvalidateRect(Some(hwnd), None, false);
                }
            }
            LRESULT(0)
        }
        WM_KEYDOWN if wparam.0 as u32 == 0x1B => {
            if !state_ptr.is_null() {
                unsafe {
                    (*state_ptr).selecting = false;
                    let _ = ReleaseCapture();
                    let _ = ShowWindow(hwnd, SW_HIDE);
                }
            }
            LRESULT(0)
        }
        WM_SHOW_CAPTURE if !state_ptr.is_null() => {
            show_capture(hwnd, state_ptr);
            LRESULT(0)
        }
        WM_QUIT_LAB => {
            let _ = unsafe { DestroyWindow(hwnd) };
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_PAINT => {
            let state = if state_ptr.is_null() {
                State::default()
            } else {
                State {
                    anchor: unsafe { (*state_ptr).anchor },
                    cursor: unsafe { (*state_ptr).cursor },
                    selecting: unsafe { (*state_ptr).selecting },
                    has_selection: unsafe { (*state_ptr).has_selection },
                }
            };
            paint(hwnd, &state);
            LRESULT(0)
        }
        WM_DESTROY => {
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        WM_NCDESTROY => {
            if !state_ptr.is_null() {
                unsafe {
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                    drop(Box::from_raw(state_ptr));
                }
            }
            unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
        }
        _ => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
    }
}

fn paint(hwnd: HWND, state: &State) {
    let mut paint = PAINTSTRUCT::default();
    let target = unsafe { BeginPaint(hwnd, &mut paint) };
    let mut client = RECT::default();
    if unsafe { GetClientRect(hwnd, &mut client) }.is_err() {
        unsafe {
            let _ = EndPaint(hwnd, &paint);
        }
        return;
    }
    let width = client.right - client.left;
    let height = client.bottom - client.top;
    let buffer = unsafe { CreateCompatibleDC(Some(target)) };
    let bitmap = unsafe { CreateCompatibleBitmap(target, width, height) };
    let previous_bitmap = unsafe { SelectObject(buffer, HGDIOBJ(bitmap.0)) };

    draw_pattern(buffer, width, height);
    draw_selection(buffer, width, height, state);
    draw_label(buffer);
    let _ = unsafe { BitBlt(target, 0, 0, width, height, Some(buffer), 0, 0, SRCCOPY) };

    unsafe {
        SelectObject(buffer, previous_bitmap);
        let _ = DeleteObject(HGDIOBJ(bitmap.0));
        let _ = DeleteDC(buffer);
        let _ = EndPaint(hwnd, &paint);
    }
}

fn draw_pattern(dc: windows::Win32::Graphics::Gdi::HDC, width: i32, height: i32) {
    let mut y = 0;
    while y < height {
        let band = y / 32;
        let color = rgb(
            28 + (band * 13 % 96) as u8,
            52 + (band * 7 % 96) as u8,
            88 + (band * 11 % 112) as u8,
        );
        fill(
            dc,
            RECT {
                left: 0,
                top: y,
                right: width,
                bottom: (y + 32).min(height),
            },
            color,
        );
        y += 32;
    }
    let grid = rgb(220, 228, 242);
    let mut x = 0;
    while x < width {
        fill(
            dc,
            RECT {
                left: x,
                top: 0,
                right: (x + 2).min(width),
                bottom: height,
            },
            grid,
        );
        x += 96;
    }
    let mut y = 0;
    while y < height {
        fill(
            dc,
            RECT {
                left: 0,
                top: y,
                right: width,
                bottom: (y + 2).min(height),
            },
            grid,
        );
        y += 96;
    }
}

fn draw_selection(dc: windows::Win32::Graphics::Gdi::HDC, width: i32, height: i32, state: &State) {
    let visible = state.selecting || state.has_selection;
    let dim = rgb(24, 27, 34);
    if !visible {
        fill(
            dc,
            RECT {
                left: 0,
                top: 0,
                right: width,
                bottom: height,
            },
            dim,
        );
        return;
    }

    let selection = RECT {
        left: state.anchor.x.min(state.cursor.x).clamp(0, width),
        top: state.anchor.y.min(state.cursor.y).clamp(0, height),
        right: state.anchor.x.max(state.cursor.x).clamp(0, width),
        bottom: state.anchor.y.max(state.cursor.y).clamp(0, height),
    };
    fill(
        dc,
        RECT {
            left: 0,
            top: 0,
            right: width,
            bottom: selection.top,
        },
        dim,
    );
    fill(
        dc,
        RECT {
            left: 0,
            top: selection.bottom,
            right: width,
            bottom: height,
        },
        dim,
    );
    fill(
        dc,
        RECT {
            left: 0,
            top: selection.top,
            right: selection.left,
            bottom: selection.bottom,
        },
        dim,
    );
    fill(
        dc,
        RECT {
            left: selection.right,
            top: selection.top,
            right: width,
            bottom: selection.bottom,
        },
        dim,
    );

    let pen = unsafe { CreatePen(PS_SOLID, 2, rgb(46, 165, 255)) };
    let previous_pen = unsafe { SelectObject(dc, HGDIOBJ(pen.0)) };
    let previous_brush = unsafe { SelectObject(dc, GetStockObject(NULL_BRUSH)) };
    unsafe {
        let _ = Rectangle(
            dc,
            selection.left,
            selection.top,
            selection.right,
            selection.bottom,
        );
        SelectObject(dc, previous_brush);
        SelectObject(dc, previous_pen);
        let _ = DeleteObject(HGDIOBJ(pen.0));
    }
}

fn draw_label(dc: windows::Win32::Graphics::Gdi::HDC) {
    fill(
        dc,
        RECT {
            left: 18,
            top: 18,
            right: 760,
            bottom: 62,
        },
        rgb(16, 23, 36),
    );
    let label: Vec<u16> = "04 · Win32 GDI | Esc 隐藏 · Alt+F12 重开 · Alt+Shift+F12 结束"
        .encode_utf16()
        .collect();
    unsafe {
        SetBkMode(dc, TRANSPARENT);
        SetTextColor(dc, rgb(255, 255, 255));
        let _ = TextOutW(dc, 32, 32, &label);
    }
}

fn show_capture(hwnd: HWND, state: *mut State) {
    if let Ok(monitor) = monitor_rect() {
        if !state.is_null() {
            unsafe {
                *state = State::default();
            }
        }
        let width = monitor.right - monitor.left;
        let height = monitor.bottom - monitor.top;
        unsafe {
            let _ = SetWindowPos(
                hwnd,
                None,
                monitor.left,
                monitor.top,
                width,
                height,
                SWP_NOZORDER,
            );
            let _ = InvalidateRect(Some(hwnd), None, false);
            let _ = ShowWindow(hwnd, SW_SHOW);
            let _ = SetForegroundWindow(hwnd);
            let _ = UpdateWindow(hwnd);
        }
    }
}

fn fill(dc: windows::Win32::Graphics::Gdi::HDC, rect: RECT, color: COLORREF) {
    if rect.right <= rect.left || rect.bottom <= rect.top {
        return;
    }
    let brush = unsafe { CreateSolidBrush(color) };
    unsafe {
        FillRect(dc, &rect, brush);
        let _ = DeleteObject(HGDIOBJ(brush.0));
    }
}

fn rgb(red: u8, green: u8, blue: u8) -> COLORREF {
    COLORREF(red as u32 | (green as u32) << 8 | (blue as u32) << 16)
}

fn point_from_lparam(lparam: LPARAM) -> POINT {
    POINT {
        x: (lparam.0 as u16 as i16) as i32,
        y: ((lparam.0 >> 16) as u16 as i16) as i32,
    }
}

fn monitor_rect() -> Result<RECT, Box<dyn std::error::Error>> {
    let mut cursor = POINT::default();
    unsafe { GetCursorPos(&mut cursor) }?;
    let monitor = unsafe { MonitorFromPoint(cursor, MONITOR_DEFAULTTONEAREST) };
    let mut info = MONITORINFO {
        cbSize: size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    unsafe { GetMonitorInfoW(monitor, &mut info) }.ok()?;
    Ok(info.rcMonitor)
}
