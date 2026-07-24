use std::sync::mpsc::{self, Sender};
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use image::{DynamicImage, RgbaImage};
use snow_shot_capture::PixelRect;
use snow_shot_scroll::scroll_screenshot_service::{
    ScrollDirection, ScrollImageList, ScrollScreenshotConfig, ScrollScreenshotService,
};
use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateSolidBrush, DT_CENTER, DT_SINGLELINE, DT_VCENTER, DeleteObject, DrawTextW,
    EndPaint, FillRect, HGDIOBJ, PAINTSTRUCT, SetBkMode, SetTextColor, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_ESCAPE, VK_RETURN};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
    GetClientRect, IDC_ARROW, LoadCursorW, MSG, MSLLHOOKSTRUCT, PM_REMOVE, PeekMessageW,
    RegisterClassW, SW_SHOWNOACTIVATE, SetWindowsHookExW, ShowWindow, TranslateMessage,
    UnhookWindowsHookEx, WH_MOUSE_LL, WM_ERASEBKGND, WM_MOUSEWHEEL, WM_PAINT, WNDCLASSW,
    WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
};
use windows::core::{PCWSTR, w};

use crate::capture_workflow::{FrozenRegionFrame, capture_live_region};

const OVERLAY_CLASS_NAME: PCWSTR = w!("SnowShotScrollCaptureOverlay");
const OVERLAY_TITLE: PCWSTR = w!("Snow Shot 长截图");
const BORDER_THICKNESS: i32 = 2;
const TIP_WIDTH: i32 = 360;
const TIP_HEIGHT: i32 = 34;
const CAPTURE_IDLE_DELAY: Duration = Duration::from_millis(55);
const CAPTURE_MAX_DELAY: Duration = Duration::from_millis(140);
const LOOP_INTERVAL: Duration = Duration::from_millis(8);

static OVERLAY_CLASS: OnceLock<Result<u16, String>> = OnceLock::new();
static WHEEL_SENDER: OnceLock<Mutex<Option<Sender<ScrollImageList>>>> = OnceLock::new();

#[derive(Debug)]
pub(crate) struct ScrollCaptureRequest {
    pub(crate) monitor_origin_x: i32,
    pub(crate) monitor_origin_y: i32,
    pub(crate) monitor_width: u32,
    pub(crate) monitor_height: u32,
    pub(crate) region: PixelRect,
    pub(crate) initial_frame: FrozenRegionFrame,
}

#[derive(Debug)]
pub(crate) enum ScrollCaptureOutcome {
    Completed(FrozenRegionFrame),
    Cancelled,
}

pub(crate) fn run_scroll_capture(
    request: ScrollCaptureRequest,
) -> Result<ScrollCaptureOutcome, String> {
    let initial_width = request.initial_frame.width();
    let initial_height = request.initial_frame.height();
    let initial_rgba = request.initial_frame.rgba().to_vec();
    let initial_image = RgbaImage::from_raw(initial_width, initial_height, initial_rgba.clone())
        .ok_or_else(|| "长截图初始图像尺寸无效。".to_string())?;

    let mut service = ScrollScreenshotService::new();
    service.init(ScrollScreenshotConfig {
        direction: ScrollDirection::Vertical,
        min_size_delta: ((request.region.height() as f32 * 0.8).ceil() as i32).max(64),
        ..ScrollScreenshotConfig::default()
    });
    let _ = service.handle_image(
        DynamicImage::ImageRgba8(initial_image),
        ScrollImageList::Bottom,
    );

    let _overlay = ScrollOverlay::create(&request)?;
    let (wheel_sender, wheel_receiver) = mpsc::channel();
    let _hook = MouseHook::install(wheel_sender)?;
    let mut pending_direction = None;
    let mut last_wheel = Instant::now();
    let mut last_capture = Instant::now();
    let mut enter_was_down = false;
    let mut escape_was_down = false;

    loop {
        pump_messages();
        while let Ok(direction) = wheel_receiver.try_recv() {
            pending_direction = Some(direction);
            last_wheel = Instant::now();
        }

        if let Some(direction) = pending_direction
            && (last_wheel.elapsed() >= CAPTURE_IDLE_DELAY
                || last_capture.elapsed() >= CAPTURE_MAX_DELAY)
        {
            let frame = capture_live_region(
                request.monitor_origin_x,
                request.monitor_origin_y,
                request.region,
            )
            .map_err(|error| error.to_string())?;
            let image = RgbaImage::from_raw(frame.width(), frame.height(), frame.rgba().to_vec())
                .ok_or_else(|| "长截图采样图像尺寸无效。".to_string())?;
            let _ = service.handle_image(DynamicImage::ImageRgba8(image), direction);
            pending_direction = None;
            last_capture = Instant::now();
        }

        let enter_is_down = key_is_down(VK_RETURN.0);
        if enter_is_down && !enter_was_down {
            let image = service.export().unwrap_or_else(|| {
                DynamicImage::ImageRgba8(
                    RgbaImage::from_raw(initial_width, initial_height, initial_rgba.clone())
                        .expect("validated initial long screenshot frame"),
                )
            });
            let rgba = image.to_rgba8();
            let frame = FrozenRegionFrame::from_rgba(rgba.width(), rgba.height(), rgba.into_raw())
                .map_err(|error| error.to_string())?;
            return Ok(ScrollCaptureOutcome::Completed(frame));
        }
        enter_was_down = enter_is_down;

        let escape_is_down = key_is_down(VK_ESCAPE.0);
        if escape_is_down && !escape_was_down {
            return Ok(ScrollCaptureOutcome::Cancelled);
        }
        escape_was_down = escape_is_down;
        thread::sleep(LOOP_INTERVAL);
    }
}

fn key_is_down(key: u16) -> bool {
    // SAFETY: GetAsyncKeyState reads process-independent keyboard state without mutation.
    (unsafe { GetAsyncKeyState(key as i32) }) as u16 & 0x8000 != 0
}

struct MouseHook {
    handle: windows::Win32::UI::WindowsAndMessaging::HHOOK,
}

impl MouseHook {
    fn install(sender: Sender<ScrollImageList>) -> Result<Self, String> {
        let sender_slot = WHEEL_SENDER.get_or_init(|| Mutex::new(None));
        let mut slot = sender_slot
            .lock()
            .map_err(|_| "长截图滚轮监听状态不可用。".to_string())?;
        if slot.is_some() {
            return Err("已有长截图滚轮监听正在运行。".to_string());
        }
        *slot = Some(sender);
        drop(slot);

        let instance = module_instance()?;
        // SAFETY: the callback is static and the hook is removed by Drop on this same thread.
        let handle =
            unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook_proc), Some(instance), 0) }
                .map_err(|error| {
                    clear_wheel_sender();
                    format!("无法监听长截图滚轮：{error}")
                })?;
        Ok(Self { handle })
    }
}

impl Drop for MouseHook {
    fn drop(&mut self) {
        // SAFETY: handle was returned by SetWindowsHookExW and is unhooked exactly once.
        let _ = unsafe { UnhookWindowsHookEx(self.handle) };
        clear_wheel_sender();
    }
}

unsafe extern "system" fn mouse_hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 && wparam.0 as u32 == WM_MOUSEWHEEL {
        // SAFETY: WH_MOUSE_LL supplies an MSLLHOOKSTRUCT pointer for mouse messages.
        let event = unsafe { &*(lparam.0 as *const MSLLHOOKSTRUCT) };
        let delta = high_word_signed(event.mouseData);
        let direction = if delta >= 0 {
            ScrollImageList::Top
        } else {
            ScrollImageList::Bottom
        };
        if let Some(sender_slot) = WHEEL_SENDER.get()
            && let Ok(slot) = sender_slot.try_lock()
            && let Some(sender) = slot.as_ref()
        {
            let _ = sender.send(direction);
        }
    }

    // SAFETY: the hook is observational; every event must continue through the chain.
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

fn clear_wheel_sender() {
    if let Some(sender_slot) = WHEEL_SENDER.get()
        && let Ok(mut slot) = sender_slot.lock()
    {
        slot.take();
    }
}

fn pump_messages() {
    let mut message = MSG::default();
    // SAFETY: the worker owns its message queue and message storage for each iteration.
    while unsafe { PeekMessageW(&mut message, None, 0, 0, PM_REMOVE) }.as_bool() {
        unsafe {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
}

struct ScrollOverlay {
    windows: Vec<HWND>,
}

impl ScrollOverlay {
    fn create(request: &ScrollCaptureRequest) -> Result<Self, String> {
        ensure_overlay_class()?;
        let instance = module_instance()?;
        let left = request
            .monitor_origin_x
            .saturating_add(i32::try_from(request.region.x()).unwrap_or(i32::MAX));
        let top = request
            .monitor_origin_y
            .saturating_add(i32::try_from(request.region.y()).unwrap_or(i32::MAX));
        let width = i32::try_from(request.region.width()).unwrap_or(i32::MAX);
        let height = i32::try_from(request.region.height()).unwrap_or(i32::MAX);
        let mut rects = vec![
            (
                left - BORDER_THICKNESS,
                top - BORDER_THICKNESS,
                width + 4,
                BORDER_THICKNESS,
            ),
            (
                left - BORDER_THICKNESS,
                top + height,
                width + 4,
                BORDER_THICKNESS,
            ),
            (left - BORDER_THICKNESS, top, BORDER_THICKNESS, height),
            (left + width, top, BORDER_THICKNESS, height),
        ];
        if let Some((tip_x, tip_y)) = tip_position(request) {
            rects.push((tip_x, tip_y, TIP_WIDTH, TIP_HEIGHT));
        }

        let mut windows = Vec::with_capacity(rects.len());
        for (x, y, window_width, window_height) in rects {
            // SAFETY: the registered class and module remain valid for the process lifetime.
            let hwnd = unsafe {
                CreateWindowExW(
                    WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TRANSPARENT,
                    OVERLAY_CLASS_NAME,
                    OVERLAY_TITLE,
                    WS_POPUP,
                    x,
                    y,
                    window_width.max(1),
                    window_height.max(1),
                    None,
                    None,
                    Some(instance),
                    None,
                )
            }
            .map_err(|error| format!("无法创建长截图提示层：{error}"))?;
            // SAFETY: overlay windows are topmost, non-activating, and destroyed by Drop.
            unsafe {
                let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            }
            windows.push(hwnd);
        }
        Ok(Self { windows })
    }
}

impl Drop for ScrollOverlay {
    fn drop(&mut self) {
        for hwnd in self.windows.drain(..) {
            // SAFETY: these HWND values were created and are owned by this overlay.
            let _ = unsafe { DestroyWindow(hwnd) };
        }
    }
}

unsafe extern "system" fn overlay_window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_ERASEBKGND => LRESULT(1),
        WM_PAINT => {
            let mut paint = PAINTSTRUCT::default();
            // SAFETY: paint is writable and paired with EndPaint below.
            let dc = unsafe { BeginPaint(hwnd, &mut paint) };
            let mut client = RECT::default();
            let _ = unsafe { GetClientRect(hwnd, &mut client) };
            // COLORREF stores colors as 0x00BBGGRR; this is Snow Shot teal #13B8A6.
            let brush = unsafe { CreateSolidBrush(COLORREF(0x00A6_B813)) };
            unsafe {
                FillRect(dc, &client, brush);
            }
            if client.bottom - client.top >= TIP_HEIGHT - 2 {
                let mut text: Vec<u16> = "长截图中：滚动页面，Enter 完成，Esc 取消"
                    .encode_utf16()
                    .collect();
                unsafe {
                    SetBkMode(dc, TRANSPARENT);
                    SetTextColor(dc, COLORREF(0x00FF_FFFF));
                    DrawTextW(
                        dc,
                        &mut text,
                        &mut client,
                        DT_CENTER | DT_VCENTER | DT_SINGLELINE,
                    );
                }
            }
            unsafe {
                let _ = EndPaint(hwnd, &paint);
                let _ = DeleteObject(HGDIOBJ(brush.0));
            }
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
    }
}

fn ensure_overlay_class() -> Result<u16, String> {
    OVERLAY_CLASS
        .get_or_init(|| {
            let instance = module_instance()?;
            let cursor = unsafe { LoadCursorW(None, IDC_ARROW) }
                .map_err(|error| format!("无法加载长截图光标：{error}"))?;
            let class = WNDCLASSW {
                lpfnWndProc: Some(overlay_window_proc),
                hInstance: instance,
                hCursor: cursor,
                lpszClassName: OVERLAY_CLASS_NAME,
                ..Default::default()
            };
            let atom = unsafe { RegisterClassW(&class) };
            if atom == 0 {
                Err(format!(
                    "无法注册长截图提示窗口类：{}",
                    windows::core::Error::from_win32()
                ))
            } else {
                Ok(atom)
            }
        })
        .clone()
}

fn module_instance() -> Result<HINSTANCE, String> {
    let module = unsafe { GetModuleHandleW(None) }
        .map_err(|error| format!("无法获取应用模块句柄：{error}"))?;
    Ok(HINSTANCE(module.0))
}

fn tip_position(request: &ScrollCaptureRequest) -> Option<(i32, i32)> {
    let region_left = request
        .monitor_origin_x
        .saturating_add(i32::try_from(request.region.x()).ok()?);
    let region_top = request
        .monitor_origin_y
        .saturating_add(i32::try_from(request.region.y()).ok()?);
    let region_bottom = region_top.saturating_add(i32::try_from(request.region.height()).ok()?);
    let monitor_right = request
        .monitor_origin_x
        .saturating_add(i32::try_from(request.monitor_width).ok()?);
    let monitor_bottom = request
        .monitor_origin_y
        .saturating_add(i32::try_from(request.monitor_height).ok()?);
    let x = region_left
        .min(monitor_right - TIP_WIDTH)
        .max(request.monitor_origin_x);
    if region_bottom + 8 + TIP_HEIGHT <= monitor_bottom {
        Some((x, region_bottom + 8))
    } else if region_top - 8 - TIP_HEIGHT >= request.monitor_origin_y {
        Some((x, region_top - 8 - TIP_HEIGHT))
    } else {
        None
    }
}

fn high_word_signed(value: u32) -> i16 {
    ((value >> 16) & 0xffff) as u16 as i16
}

#[cfg(test)]
mod tests {
    use super::{ScrollCaptureRequest, high_word_signed, tip_position};
    use crate::capture_workflow::FrozenRegionFrame;
    use snow_shot_capture::PixelRect;

    fn request(region: PixelRect) -> ScrollCaptureRequest {
        ScrollCaptureRequest {
            monitor_origin_x: -1920,
            monitor_origin_y: 0,
            monitor_width: 1920,
            monitor_height: 1080,
            initial_frame: FrozenRegionFrame::from_rgba(
                region.width(),
                region.height(),
                vec![0; region.width() as usize * region.height() as usize * 4],
            )
            .unwrap(),
            region,
        }
    }

    #[test]
    fn decodes_signed_wheel_delta() {
        assert_eq!(high_word_signed(120_u32 << 16), 120);
        assert_eq!(high_word_signed(((-120_i16) as u16 as u32) << 16), -120);
    }

    #[test]
    fn places_tip_outside_selected_region() {
        let bottom_space = request(PixelRect::new(100, 100, 800, 500).unwrap());
        assert_eq!(tip_position(&bottom_space), Some((-1820, 608)));

        let top_space = request(PixelRect::new(100, 700, 800, 360).unwrap());
        assert_eq!(tip_position(&top_space), Some((-1820, 658)));
    }

    #[test]
    fn omits_tip_when_selection_fills_monitor_height() {
        let request = request(PixelRect::new(0, 0, 1920, 1080).unwrap());
        assert_eq!(tip_position(&request), None);
    }
}
