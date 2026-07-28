use std::path::PathBuf;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use image::imageops::FilterType;
use image::{DynamicImage, RgbaImage};
use snow_shot_capture::PixelRect;
use snow_shot_scroll::scroll_screenshot_service::{
    ScrollDirection, ScrollImageList, ScrollScreenshotConfig, ScrollScreenshotService,
};
use windows::Win32::Foundation::{
    COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM,
};
use windows::Win32::Graphics::Gdi::{
    AC_SRC_ALPHA, AC_SRC_OVER, ANTIALIASED_QUALITY, BI_RGB, BITMAPINFO, BITMAPINFOHEADER,
    BLENDFUNCTION, BeginPaint, CLIP_DEFAULT_PRECIS, CreateCompatibleDC, CreateDIBSection,
    CreateFontW, CreateSolidBrush, DEFAULT_CHARSET, DEFAULT_PITCH, DIB_RGB_COLORS, DT_CALCRECT,
    DT_SINGLELINE, DeleteDC, DeleteObject, DrawTextW, EndPaint, FillRect, GdiFlush, GetDC, HGDIOBJ,
    OUT_DEFAULT_PRECIS, PAINTSTRUCT, ReleaseDC, SelectObject, SetBkMode, SetTextColor, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_ESCAPE, VK_RETURN};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
    GetClientRect, GetCursorPos, IDC_ARROW, IDC_HAND, LoadCursorW, MSG, MSLLHOOKSTRUCT, PM_REMOVE,
    PeekMessageW, RegisterClassW, SW_HIDE, SW_SHOWNOACTIVATE, SetWindowsHookExW, ShowWindow,
    TranslateMessage, ULW_ALPHA, UnhookWindowsHookEx, UpdateLayeredWindow, WH_MOUSE_LL,
    WM_ERASEBKGND, WM_LBUTTONUP, WM_MOUSEWHEEL, WM_PAINT, WNDCLASSW, WS_EX_LAYERED,
    WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
};
use windows::core::{PCWSTR, w};

use crate::capture_workflow::{FrozenRegionFrame, capture_live_region, save_region_frame_to_path};

const OVERLAY_CLASS_NAME: PCWSTR = w!("SnowShotScrollCaptureOverlay");
const LAYERED_CLASS_NAME: PCWSTR = w!("SnowShotScrollCaptureLayered");
const ACTION_CLASS_NAME: PCWSTR = w!("SnowShotScrollCaptureActions");
const OVERLAY_TITLE: PCWSTR = w!("Snow Shot 长截图");
const BORDER_THICKNESS: i32 = 2;
// Mirrors the original scroll-screenshot UI: a 128px thumbnail strip beside the
// selection (`THUMBNAIL_WIDTH`), an 8px gap (`token.marginXS`) and a 32% black
// mask over stitched content outside the current capture edge.
const STRIP_BASE_WIDTH: f32 = 128.0;
const STRIP_GAP: f32 = 8.0;
const EDGE_MASK_NUMERATOR: u32 = 174; // keep 68% brightness ≈ rgba(0,0,0,0.32) overlay
const PILL_ALPHA: u32 = 115; // antd colorBgMask rgba(0,0,0,0.45)
const PILL_TEXT: &str = "滚动页面拼接长图，Enter 完成，Esc 取消";
// The action bar mirrors the original draw toolbar during scroll capture: a
// white rounded card whose save/cancel/copy buttons stay clickable while the
// annotation tools are disabled. Text colors follow the original buttons —
// neutral save, antd error red cancel, Snow Shot teal copy.
const ACTION_SAVE: i32 = 1;
const ACTION_CANCEL: i32 = 2;
const ACTION_COPY: i32 = 3;
const ACTION_LABELS: [&str; 3] = ["保存", "取消", "复制"];
const ACTION_TEXT_COLORS: [u32; 3] = [0x0026_2626, 0x0022_13CF, 0x00A6_B813]; // 0x00BBGGRR
const ACTION_HOVER_BACKGROUND: [u8; 3] = [245, 245, 245];
const CAPTURE_IDLE_DELAY: Duration = Duration::from_millis(55);
const CAPTURE_MAX_DELAY: Duration = Duration::from_millis(140);
const LOOP_INTERVAL: Duration = Duration::from_millis(8);

static OVERLAY_CLASS: OnceLock<Result<u16, String>> = OnceLock::new();
static LAYERED_CLASS: OnceLock<Result<u16, String>> = OnceLock::new();
static ACTION_CLASS: OnceLock<Result<u16, String>> = OnceLock::new();
static WHEEL_SENDER: OnceLock<Mutex<Option<Sender<ScrollImageList>>>> = OnceLock::new();
/// Last click on the action bar (`ACTION_SAVE`/`ACTION_COPY`), consumed by the
/// capture loop; written by the bar's window procedure on the same thread.
static ACTION_CLICK: AtomicI32 = AtomicI32::new(0);

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
    Saved {
        width: u32,
        height: u32,
        path: PathBuf,
    },
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
    let seed_result = service.handle_image(
        DynamicImage::ImageRgba8(initial_image),
        ScrollImageList::Bottom,
    );

    let overlay = ScrollOverlay::create(&request)?;
    let dpi = overlay
        .windows
        .first()
        // SAFETY: the border HWND was just created and is owned by `overlay`.
        .map(|hwnd| unsafe { GetDpiForWindow(*hwnd) })
        .filter(|dpi| *dpi != 0)
        .unwrap_or(96);
    let ui_scale = dpi as f32 / 96.0;

    let sel_left = request
        .monitor_origin_x
        .saturating_add(i32::try_from(request.region.x()).unwrap_or(i32::MAX));
    let sel_top = request
        .monitor_origin_y
        .saturating_add(i32::try_from(request.region.y()).unwrap_or(i32::MAX));
    let sel_width = i32::try_from(request.region.width()).unwrap_or(i32::MAX);
    let sel_height = i32::try_from(request.region.height()).unwrap_or(i32::MAX);

    let pill = TipPill::create(sel_left, sel_top, sel_width, sel_height, dpi)?;
    let mut pill_visible = pill.is_some();

    let strip_width = ((STRIP_BASE_WIDTH * ui_scale).round() as i32).max(32);
    let strip_x = strip_screen_x(
        request.monitor_origin_x,
        request
            .monitor_origin_x
            .saturating_add(i32::try_from(request.monitor_width).unwrap_or(i32::MAX)),
        sel_left,
        sel_width,
        (STRIP_GAP * ui_scale).round() as i32,
        strip_width,
    );
    let mut strip = PreviewStrip::create(
        strip_x,
        sel_top,
        strip_width as u32,
        sel_height.max(1) as u32,
        strip_width as f32 / request.region.width() as f32,
    )?;
    strip.note_result(&service, seed_result);

    let monitor_top = request.monitor_origin_y;
    let monitor_bottom = request
        .monitor_origin_y
        .saturating_add(i32::try_from(request.monitor_height).unwrap_or(i32::MAX));
    let monitor_left = request.monitor_origin_x;
    let monitor_right = request
        .monitor_origin_x
        .saturating_add(i32::try_from(request.monitor_width).unwrap_or(i32::MAX));
    let gap = (STRIP_GAP * ui_scale).round() as i32;
    let mut bar = ActionBar::create(dpi)?;
    let bar_x = action_bar_x(
        monitor_left,
        monitor_right,
        sel_left,
        sel_width,
        bar.width as i32,
    );
    let bar_y = action_bar_y(
        monitor_top,
        monitor_bottom,
        sel_top,
        sel_height,
        gap,
        bar.height as i32,
    );
    bar.present_at(bar_x, bar_y)?;

    let (wheel_sender, wheel_receiver) = mpsc::channel();
    let _hook = MouseHook::install(wheel_sender)?;
    let mut pending_direction = None;
    let mut last_wheel = Instant::now();
    let mut last_capture = Instant::now();
    let mut enter_was_down = false;
    let mut escape_was_down = false;
    ACTION_CLICK.store(0, Ordering::Release);

    loop {
        pump_messages();
        bar.refresh_hover();
        while let Ok(direction) = wheel_receiver.try_recv() {
            // The pill sits inside the selection: hide it on the first scroll so
            // it never appears in sampled frames (captures start after the idle
            // delay below), matching the original `setShowTip(false)` on wheel.
            if pill_visible {
                if let Some(pill) = &pill {
                    pill.hide();
                }
                pill_visible = false;
            }
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
            let result = service.handle_image(DynamicImage::ImageRgba8(image), direction);
            strip.note_result(&service, result);
            pending_direction = None;
            last_capture = Instant::now();
        }

        let clicked = ACTION_CLICK.swap(0, Ordering::AcqRel);
        if clicked == ACTION_CANCEL {
            return Ok(ScrollCaptureOutcome::Cancelled);
        }
        let enter_is_down = key_is_down(VK_RETURN.0);
        if clicked == ACTION_COPY || (enter_is_down && !enter_was_down) {
            let frame = export_stitched_frame(
                &mut service,
                initial_width,
                initial_height,
                initial_rgba.clone(),
            )?;
            return Ok(ScrollCaptureOutcome::Completed(frame));
        }
        enter_was_down = enter_is_down;

        if clicked == ACTION_SAVE {
            // Mirror the original toolbar save flow: the session pauses for the
            // dialog and resumes when the user cancels it.
            overlay.set_visible(false);
            strip.set_visible(false);
            bar.set_visible(false);
            if pill_visible && let Some(pill) = &pill {
                pill.hide();
            }
            let choice = rfd::FileDialog::new()
                .add_filter("PNG 图片", &["png"])
                .set_file_name("snow-shot.png")
                .set_title("保存 Snow Shot 长截图")
                .save_file();
            match choice {
                Some(path) => {
                    let frame = export_stitched_frame(
                        &mut service,
                        initial_width,
                        initial_height,
                        initial_rgba.clone(),
                    )?;
                    let summary = save_region_frame_to_path(&frame, &path)
                        .map_err(|error| error.to_string())?;
                    return Ok(ScrollCaptureOutcome::Saved {
                        width: summary.width(),
                        height: summary.height(),
                        path,
                    });
                }
                None => {
                    overlay.set_visible(true);
                    strip.set_visible(true);
                    bar.set_visible(true);
                    if pill_visible && let Some(pill) = &pill {
                        pill.show();
                    }
                    // Scrolls made while the dialog was open must not trigger a
                    // burst of stale captures once the loop resumes.
                    while wheel_receiver.try_recv().is_ok() {}
                    pending_direction = None;
                    last_wheel = Instant::now();
                    last_capture = Instant::now();
                }
            }
        }

        let escape_is_down = key_is_down(VK_ESCAPE.0);
        if escape_is_down && !escape_was_down {
            return Ok(ScrollCaptureOutcome::Cancelled);
        }
        escape_was_down = escape_is_down;
        thread::sleep(LOOP_INTERVAL);
    }
}

fn export_stitched_frame(
    service: &mut ScrollScreenshotService,
    initial_width: u32,
    initial_height: u32,
    initial_rgba: Vec<u8>,
) -> Result<FrozenRegionFrame, String> {
    let image = service.export().unwrap_or_else(|| {
        DynamicImage::ImageRgba8(
            RgbaImage::from_raw(initial_width, initial_height, initial_rgba)
                .expect("validated initial long screenshot frame"),
        )
    });
    let rgba = image.to_rgba8();
    FrozenRegionFrame::from_rgba(rgba.width(), rgba.height(), rgba.into_raw())
        .map_err(|error| error.to_string())
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
        let rects = vec![
            (
                left - BORDER_THICKNESS,
                top - BORDER_THICKNESS,
                width + BORDER_THICKNESS * 2,
                BORDER_THICKNESS,
            ),
            (
                left - BORDER_THICKNESS,
                top + height,
                width + BORDER_THICKNESS * 2,
                BORDER_THICKNESS,
            ),
            (left - BORDER_THICKNESS, top, BORDER_THICKNESS, height),
            (left + width, top, BORDER_THICKNESS, height),
        ];

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

    fn set_visible(&self, visible: bool) {
        for hwnd in &self.windows {
            // SAFETY: these HWND values were created and are owned by this overlay.
            unsafe {
                let _ = ShowWindow(*hwnd, if visible { SW_SHOWNOACTIVATE } else { SW_HIDE });
            }
        }
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
                let _ = EndPaint(hwnd, &paint);
                let _ = DeleteObject(HGDIOBJ(brush.0));
            }
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
    }
}

// SAFETY: layered windows receive all content via UpdateLayeredWindow.
unsafe extern "system" fn layered_window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
}

/// Click-through top-most window whose pixels come from a premultiplied BGRA
/// buffer pushed through `UpdateLayeredWindow`.
struct LayeredWindow {
    hwnd: HWND,
}

impl LayeredWindow {
    fn create() -> Result<Self, String> {
        ensure_layered_class()?;
        let instance = module_instance()?;
        // SAFETY: the registered class and module remain valid for the process lifetime.
        let hwnd = unsafe {
            CreateWindowExW(
                WS_EX_TOPMOST
                    | WS_EX_TOOLWINDOW
                    | WS_EX_NOACTIVATE
                    | WS_EX_TRANSPARENT
                    | WS_EX_LAYERED,
                LAYERED_CLASS_NAME,
                OVERLAY_TITLE,
                WS_POPUP,
                0,
                0,
                1,
                1,
                None,
                None,
                Some(instance),
                None,
            )
        }
        .map_err(|error| format!("无法创建长截图预览层：{error}"))?;
        Ok(Self { hwnd })
    }

    fn present(&self, x: i32, y: i32, width: u32, height: u32, bgra: &[u8]) -> Result<(), String> {
        present_layered(self.hwnd, x, y, width, height, bgra)
    }

    fn hide(&self) {
        // SAFETY: the window is owned by this instance.
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_HIDE);
        }
    }

    fn show(&self) {
        // SAFETY: the window is owned by this instance; layered content persists.
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_SHOWNOACTIVATE);
        }
    }
}

/// Pushes a premultiplied BGRA buffer to a layered window at a screen position.
fn present_layered(
    hwnd: HWND,
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    bgra: &[u8],
) -> Result<(), String> {
    debug_assert_eq!(bgra.len(), width as usize * height as usize * 4);
    // SAFETY: every GDI object created below is released before returning and
    // the DIB pointer is only written while the section is selected.
    unsafe {
        let screen_dc = GetDC(None);
        let memory_dc = CreateCompatibleDC(Some(screen_dc));
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width as i32,
                biHeight: -(height as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits = std::ptr::null_mut();
        let dib = CreateDIBSection(Some(screen_dc), &info, DIB_RGB_COLORS, &mut bits, None, 0)
            .map_err(|error| {
                let _ = DeleteDC(memory_dc);
                ReleaseDC(None, screen_dc);
                format!("无法创建长截图预览位图：{error}")
            })?;
        std::ptr::copy_nonoverlapping(bgra.as_ptr(), bits as *mut u8, bgra.len());
        let previous = SelectObject(memory_dc, HGDIOBJ(dib.0));

        let destination = POINT { x, y };
        let size = SIZE {
            cx: width as i32,
            cy: height as i32,
        };
        let source = POINT { x: 0, y: 0 };
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: 255,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };
        let update = UpdateLayeredWindow(
            hwnd,
            Some(screen_dc),
            Some(&destination as *const POINT),
            Some(&size as *const SIZE),
            Some(memory_dc),
            Some(&source as *const POINT),
            COLORREF(0),
            Some(&blend as *const BLENDFUNCTION),
            ULW_ALPHA,
        );

        SelectObject(memory_dc, previous);
        let _ = DeleteObject(HGDIOBJ(dib.0));
        let _ = DeleteDC(memory_dc);
        ReleaseDC(None, screen_dc);
        update.map_err(|error| format!("无法更新长截图预览层：{error}"))?;
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
    }
    Ok(())
}

impl Drop for LayeredWindow {
    fn drop(&mut self) {
        // SAFETY: the HWND was created and is exclusively owned by this instance.
        let _ = unsafe { DestroyWindow(self.hwnd) };
    }
}

/// Centered instruction pill matching the original `touch-area-tip`: white text
/// on a rounded 45% black mask background.
struct TipPill {
    window: LayeredWindow,
}

impl TipPill {
    fn create(
        sel_left: i32,
        sel_top: i32,
        sel_width: i32,
        sel_height: i32,
        dpi: u32,
    ) -> Result<Option<Self>, String> {
        let (width, height, pixels) = render_pill_bitmap(PILL_TEXT, dpi)?;
        if width as i32 + 16 > sel_width || height as i32 + 16 > sel_height {
            return Ok(None);
        }
        let window = LayeredWindow::create()?;
        window.present(
            sel_left + (sel_width - width as i32) / 2,
            sel_top + (sel_height - height as i32) / 2,
            width,
            height,
            &pixels,
        )?;
        Ok(Some(Self { window }))
    }

    fn hide(&self) {
        self.window.hide();
    }

    fn show(&self) {
        self.window.show();
    }
}

/// Clickable save/cancel/copy bar shown beside the selection, standing in for
/// the original draw toolbar whose action buttons stay usable during scroll
/// capture while the annotation tools are disabled.
struct ActionBar {
    hwnd: HWND,
    width: u32,
    height: u32,
    dpi: u32,
    x: i32,
    y: i32,
    hover: i32,
}

impl ActionBar {
    fn create(dpi: u32) -> Result<Self, String> {
        ensure_action_class()?;
        let instance = module_instance()?;
        // SAFETY: the registered class and module remain valid for the process
        // lifetime. No WS_EX_TRANSPARENT: this window must receive clicks.
        let hwnd = unsafe {
            CreateWindowExW(
                WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_LAYERED,
                ACTION_CLASS_NAME,
                OVERLAY_TITLE,
                WS_POPUP,
                0,
                0,
                1,
                1,
                None,
                None,
                Some(instance),
                None,
            )
        }
        .map_err(|error| format!("无法创建长截图操作栏：{error}"))?;
        let (width, height, _) = render_action_bar_bitmap(dpi, 0)?;
        Ok(Self {
            hwnd,
            width,
            height,
            dpi,
            x: 0,
            y: 0,
            hover: 0,
        })
    }

    fn present_at(&mut self, x: i32, y: i32) -> Result<(), String> {
        self.x = x;
        self.y = y;
        self.repaint()
    }

    fn repaint(&self) -> Result<(), String> {
        let (width, height, pixels) = render_action_bar_bitmap(self.dpi, self.hover)?;
        present_layered(self.hwnd, self.x, self.y, width, height, &pixels)
    }

    /// Polls the cursor against the bar rect and repaints when the hovered
    /// button changes; cheaper and simpler than TrackMouseEvent bookkeeping.
    fn refresh_hover(&mut self) {
        let mut cursor = POINT::default();
        // SAFETY: GetCursorPos writes to the provided POINT.
        if unsafe { GetCursorPos(&mut cursor) }.is_err() {
            return;
        }
        let inside = cursor.x >= self.x
            && cursor.x < self.x + self.width as i32
            && cursor.y >= self.y
            && cursor.y < self.y + self.height as i32;
        let hover = if inside {
            action_zone(cursor.x - self.x, self.width as i32)
        } else {
            0
        };
        if hover != self.hover {
            self.hover = hover;
            let _ = self.repaint();
        }
    }

    fn set_visible(&self, visible: bool) {
        // SAFETY: the window is owned by this instance.
        unsafe {
            let _ = ShowWindow(self.hwnd, if visible { SW_SHOWNOACTIVATE } else { SW_HIDE });
        }
    }
}

impl Drop for ActionBar {
    fn drop(&mut self) {
        // SAFETY: the HWND was created and is exclusively owned by this instance.
        let _ = unsafe { DestroyWindow(self.hwnd) };
    }
}

// SAFETY: runs on the capture thread that owns the window; only touches atomics.
unsafe extern "system" fn action_window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if message == WM_LBUTTONUP {
        let mut client = RECT::default();
        // SAFETY: client is writable and hwnd is the window receiving this message.
        let _ = unsafe { GetClientRect(hwnd, &mut client) };
        let x = (lparam.0 & 0xffff) as u16 as i16 as i32;
        ACTION_CLICK.store(action_zone(x, client.right.max(1)), Ordering::Release);
        return LRESULT(0);
    }
    unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
}

/// Maps an x offset inside the bar to `ACTION_SAVE`/`ACTION_CANCEL`/`ACTION_COPY`.
/// All three labels are two CJK glyphs wide, so the buttons split evenly.
fn action_zone(x: i32, width: i32) -> i32 {
    let zone = (x * 3 / width.max(1)).clamp(0, 2);
    zone + 1
}

/// Right-aligns the bar to the selection like the original toolbar default.
fn action_bar_x(
    monitor_left: i32,
    monitor_right: i32,
    sel_left: i32,
    sel_width: i32,
    bar_width: i32,
) -> i32 {
    (sel_left + sel_width - bar_width)
        .clamp(monitor_left, (monitor_right - bar_width).max(monitor_left))
}

/// Places the bar below the selection, flipping above when there is no room,
/// mirroring the original toolbar's below-with-above-fallback placement.
fn action_bar_y(
    monitor_top: i32,
    monitor_bottom: i32,
    sel_top: i32,
    sel_height: i32,
    gap: i32,
    bar_height: i32,
) -> i32 {
    let below = sel_top + sel_height + gap;
    if below + bar_height <= monitor_bottom {
        return below;
    }
    let above = sel_top - gap - bar_height;
    if above >= monitor_top {
        return above;
    }
    (monitor_bottom - bar_height).max(monitor_top)
}

/// Renders the action bar as an opaque white rounded card with evenly split
/// text buttons; `hover` (1-based zone) tints that button's background.
fn render_action_bar_bitmap(dpi: u32, hover: i32) -> Result<(u32, u32, Vec<u8>), String> {
    let scale = dpi as f32 / 96.0;
    let pad_x = (14.0 * scale).round() as i32;
    let pad_y = (8.0 * scale).round() as i32;
    let radius = (8.0 * scale).round() as i32;

    // SAFETY: all GDI objects are created and released in this scope; the DIB
    // bits stay valid while the section is selected into the memory DC.
    unsafe {
        let screen_dc = GetDC(None);
        let memory_dc = CreateCompatibleDC(Some(screen_dc));
        let font = CreateFontW(
            -((14.0 * scale).round() as i32),
            0,
            0,
            0,
            400,
            0,
            0,
            0,
            DEFAULT_CHARSET,
            OUT_DEFAULT_PRECIS,
            CLIP_DEFAULT_PRECIS,
            ANTIALIASED_QUALITY,
            DEFAULT_PITCH.0 as u32,
            w!("Microsoft YaHei UI"),
        );
        let previous_font = SelectObject(memory_dc, HGDIOBJ(font.0));

        let mut label_width = 1;
        let mut label_height = 1;
        for label in ACTION_LABELS {
            let mut wide: Vec<u16> = label.encode_utf16().collect();
            let mut measure = RECT::default();
            DrawTextW(
                memory_dc,
                &mut wide,
                &mut measure,
                DT_CALCRECT | DT_SINGLELINE,
            );
            label_width = label_width.max(measure.right - measure.left);
            label_height = label_height.max(measure.bottom - measure.top);
        }
        let button_width = label_width + pad_x * 2;
        let width = (button_width * ACTION_LABELS.len() as i32) as u32;
        let height = (label_height + pad_y * 2) as u32;

        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width as i32,
                biHeight: -(height as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits = std::ptr::null_mut();
        let dib = CreateDIBSection(Some(screen_dc), &info, DIB_RGB_COLORS, &mut bits, None, 0)
            .map_err(|error| {
                SelectObject(memory_dc, previous_font);
                let _ = DeleteObject(HGDIOBJ(font.0));
                let _ = DeleteDC(memory_dc);
                ReleaseDC(None, screen_dc);
                format!("无法创建长截图操作栏位图：{error}")
            })?;
        let previous_bitmap = SelectObject(memory_dc, HGDIOBJ(dib.0));

        let byte_count = width as usize * height as usize * 4;
        let pixel_bits = std::slice::from_raw_parts_mut(bits as *mut u8, byte_count);
        for (pixel, chunk) in pixel_bits.chunks_exact_mut(4).enumerate() {
            let x = (pixel % width as usize) as i32;
            let background = if hover > 0 && action_zone(x, width as i32) == hover {
                ACTION_HOVER_BACKGROUND
            } else {
                [255, 255, 255]
            };
            chunk[0] = background[0];
            chunk[1] = background[1];
            chunk[2] = background[2];
            chunk[3] = 255;
        }

        SetBkMode(memory_dc, TRANSPARENT);
        for (index, label) in ACTION_LABELS.iter().enumerate() {
            SetTextColor(memory_dc, COLORREF(ACTION_TEXT_COLORS[index]));
            let mut wide: Vec<u16> = label.encode_utf16().collect();
            let left = button_width * index as i32;
            let mut text_rect = RECT {
                left: left + pad_x,
                top: pad_y,
                right: left + pad_x + label_width,
                bottom: pad_y + label_height,
            };
            DrawTextW(memory_dc, &mut wide, &mut text_rect, DT_SINGLELINE);
        }
        let _ = GdiFlush();

        let mut pixels = std::slice::from_raw_parts(bits as *const u8, byte_count).to_vec();
        for y in 0..height as i32 {
            for x in 0..width as i32 {
                let index = (y * width as i32 + x) as usize * 4;
                if rounded_rect_contains(x, y, width as i32, height as i32, radius) {
                    pixels[index + 3] = 255;
                } else {
                    pixels[index..index + 4].fill(0);
                }
            }
        }
        // Hairline separators between the buttons, matching the toolbar splitter.
        for separator in 1..ACTION_LABELS.len() as i32 {
            let x = button_width * separator;
            for y in (height as i32 / 4)..(height as i32 * 3 / 4) {
                let index = (y * width as i32 + x) as usize * 4;
                pixels[index] = 235;
                pixels[index + 1] = 235;
                pixels[index + 2] = 235;
            }
        }

        SelectObject(memory_dc, previous_bitmap);
        SelectObject(memory_dc, previous_font);
        let _ = DeleteObject(HGDIOBJ(dib.0));
        let _ = DeleteObject(HGDIOBJ(font.0));
        let _ = DeleteDC(memory_dc);
        ReleaseDC(None, screen_dc);
        Ok((width, height, pixels))
    }
}

/// Renders the pill text into a premultiplied BGRA bitmap. GDI cannot emit
/// alpha, so coverage is recovered from the luminance of white-on-black text
/// and blended over the pill's constant background alpha.
fn render_pill_bitmap(text: &str, dpi: u32) -> Result<(u32, u32, Vec<u8>), String> {
    let scale = dpi as f32 / 96.0;
    let pad_x = (16.0 * scale).round() as i32;
    let pad_y = (7.0 * scale).round() as i32;
    let radius = (6.0 * scale).round() as i32;
    let mut wide: Vec<u16> = text.encode_utf16().collect();

    // SAFETY: all GDI objects are created and released in this scope; the DIB
    // bits stay valid while the section is selected into the memory DC.
    unsafe {
        let screen_dc = GetDC(None);
        let memory_dc = CreateCompatibleDC(Some(screen_dc));
        let font = CreateFontW(
            -((14.0 * scale).round() as i32),
            0,
            0,
            0,
            400,
            0,
            0,
            0,
            DEFAULT_CHARSET,
            OUT_DEFAULT_PRECIS,
            CLIP_DEFAULT_PRECIS,
            ANTIALIASED_QUALITY,
            DEFAULT_PITCH.0 as u32,
            w!("Microsoft YaHei UI"),
        );
        let previous_font = SelectObject(memory_dc, HGDIOBJ(font.0));

        let mut measure = RECT::default();
        DrawTextW(
            memory_dc,
            &mut wide,
            &mut measure,
            DT_CALCRECT | DT_SINGLELINE,
        );
        let text_width = (measure.right - measure.left).max(1);
        let text_height = (measure.bottom - measure.top).max(1);
        let width = (text_width + pad_x * 2) as u32;
        let height = (text_height + pad_y * 2) as u32;

        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width as i32,
                biHeight: -(height as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits = std::ptr::null_mut();
        let dib = CreateDIBSection(Some(screen_dc), &info, DIB_RGB_COLORS, &mut bits, None, 0)
            .map_err(|error| {
                SelectObject(memory_dc, previous_font);
                let _ = DeleteObject(HGDIOBJ(font.0));
                let _ = DeleteDC(memory_dc);
                ReleaseDC(None, screen_dc);
                format!("无法创建长截图提示位图：{error}")
            })?;
        let previous_bitmap = SelectObject(memory_dc, HGDIOBJ(dib.0));

        let byte_count = width as usize * height as usize * 4;
        std::ptr::write_bytes(bits as *mut u8, 0, byte_count);
        SetBkMode(memory_dc, TRANSPARENT);
        SetTextColor(memory_dc, COLORREF(0x00FF_FFFF));
        let mut text_rect = RECT {
            left: pad_x,
            top: pad_y,
            right: pad_x + text_width,
            bottom: pad_y + text_height,
        };
        DrawTextW(memory_dc, &mut wide, &mut text_rect, DT_SINGLELINE);
        let _ = GdiFlush();

        let mut pixels = std::slice::from_raw_parts(bits as *const u8, byte_count).to_vec();
        for y in 0..height {
            for x in 0..width {
                let index = (y * width + x) as usize * 4;
                if !rounded_rect_contains(x as i32, y as i32, width as i32, height as i32, radius) {
                    pixels[index..index + 4].fill(0);
                    continue;
                }
                let coverage = pixels[index].max(pixels[index + 1]).max(pixels[index + 2]) as u32;
                let alpha = coverage + PILL_ALPHA * (255 - coverage) / 255;
                pixels[index] = coverage as u8;
                pixels[index + 1] = coverage as u8;
                pixels[index + 2] = coverage as u8;
                pixels[index + 3] = alpha as u8;
            }
        }

        SelectObject(memory_dc, previous_bitmap);
        SelectObject(memory_dc, previous_font);
        let _ = DeleteObject(HGDIOBJ(dib.0));
        let _ = DeleteObject(HGDIOBJ(font.0));
        let _ = DeleteDC(memory_dc);
        ReleaseDC(None, screen_dc);
        Ok((width, height, pixels))
    }
}

fn rounded_rect_contains(x: i32, y: i32, width: i32, height: i32, radius: i32) -> bool {
    if x < 0 || y < 0 || x >= width || y >= height {
        return false;
    }
    let corner_x = if x < radius {
        radius - 1 - x
    } else if x >= width - radius {
        x - (width - radius)
    } else {
        return true;
    };
    let corner_y = if y < radius {
        radius - 1 - y
    } else if y >= height - radius {
        y - (height - radius)
    } else {
        return true;
    };
    corner_x * corner_x + corner_y * corner_y <= radius * radius
}

/// One stitched thumbnail segment; `top` is its offset in stitch coordinates
/// (origin = top edge of the first captured frame).
struct StripSegment {
    top: i32,
    thumb: RgbaImage,
}

/// Live preview of the stitched result beside the selection, mirroring the
/// original `thumbnail-list`: segments accumulate in capture order and content
/// outside the current viewport is dimmed by the capture-edge mask.
struct PreviewStrip {
    window: LayeredWindow,
    screen_x: i32,
    screen_y: i32,
    width: u32,
    height: u32,
    scale: f32,
    segments: Vec<StripSegment>,
    viewport: (i32, i32),
    edge_down: bool,
}

type ScrollHandleResult = (
    Option<(i32, Option<ScrollImageList>)>,
    bool,
    ScrollImageList,
);

impl PreviewStrip {
    fn create(
        screen_x: i32,
        screen_y: i32,
        width: u32,
        height: u32,
        scale: f32,
    ) -> Result<Self, String> {
        Ok(Self {
            window: LayeredWindow::create()?,
            screen_x,
            screen_y,
            width,
            height,
            scale,
            segments: Vec::new(),
            viewport: (0, 0),
            edge_down: true,
        })
    }

    fn note_result(&mut self, service: &ScrollScreenshotService, result: ScrollHandleResult) {
        let Some((edge_position, appended)) = result.0 else {
            return;
        };
        let frame_size = service.image_height as i32;
        self.viewport = if edge_position >= 0 {
            (edge_position - frame_size, edge_position)
        } else {
            (edge_position, edge_position + frame_size)
        };
        self.edge_down = edge_position >= 0;

        if let Some(list) = appended {
            let segment = match list {
                ScrollImageList::Bottom => service.bottom_image_list.last(),
                ScrollImageList::Top => service.top_image_list.last(),
            };
            if let Some(segment) = segment {
                let thumb_height = ((segment.image.height() as f32 * self.scale).round() as u32)
                    .clamp(1, self.height.max(1) * 4);
                let thumb = image::imageops::resize(
                    &segment.image.to_rgba8(),
                    self.width.max(1),
                    thumb_height,
                    FilterType::Triangle,
                );
                let top = match list {
                    // Cropped bottom segments cover [bottom_before - overlay, bottom_after).
                    ScrollImageList::Bottom => {
                        service.bottom_image_size - segment.image.height() as i32
                    }
                    // Cropped top segments cover [-top_after, -top_after + height).
                    ScrollImageList::Top => -service.top_image_size,
                };
                self.segments.push(StripSegment { top, thumb });
            }
        }

        let pixels = compose_strip(
            &self.segments,
            service.top_image_size,
            service.bottom_image_size,
            self.viewport,
            self.edge_down,
            self.scale,
            self.width,
            self.height,
        );
        let _ = self.window.present(
            self.screen_x,
            self.screen_y,
            self.width,
            self.height,
            &pixels,
        );
    }

    fn set_visible(&self, visible: bool) {
        if visible {
            self.window.show();
        } else {
            self.window.hide();
        }
    }
}

/// Composes the strip buffer (premultiplied BGRA): segments painted in capture
/// order, auto-scrolled so the active capture edge stays visible, and content
/// outside the viewport dimmed like the original 32% black edge mask.
#[allow(clippy::too_many_arguments)]
fn compose_strip(
    segments: &[StripSegment],
    top_size: i32,
    bottom_size: i32,
    viewport: (i32, i32),
    edge_down: bool,
    scale: f32,
    width: u32,
    height: u32,
) -> Vec<u8> {
    let width_px = width as usize;
    let height_px = height as i32;
    let mut buffer = vec![0u8; width_px * height as usize * 4];
    let to_px = |value: i32| ((value + top_size) as f32 * scale).round() as i32;
    let content_px = ((top_size + bottom_size) as f32 * scale).round() as i32;
    let viewport_top = to_px(viewport.0);
    let viewport_bottom = to_px(viewport.1);
    let offset = if content_px <= height_px {
        0
    } else if edge_down {
        (viewport_bottom - height_px).clamp(0, content_px - height_px)
    } else {
        viewport_top.clamp(0, content_px - height_px)
    };

    for segment in segments {
        let segment_top = to_px(segment.top) - offset;
        let copy_width = (segment.thumb.width() as usize).min(width_px);
        for row in 0..segment.thumb.height() as i32 {
            let destination_y = segment_top + row;
            if destination_y < 0 || destination_y >= height_px {
                continue;
            }
            let source = segment.thumb.as_raw();
            let source_start = row as usize * segment.thumb.width() as usize * 4;
            let destination_start = destination_y as usize * width_px * 4;
            for x in 0..copy_width {
                let s = source_start + x * 4;
                let d = destination_start + x * 4;
                buffer[d] = source[s + 2];
                buffer[d + 1] = source[s + 1];
                buffer[d + 2] = source[s];
                buffer[d + 3] = 255;
            }
        }
    }

    for y in 0..height_px {
        let stitch_y = y + offset;
        if stitch_y >= viewport_top && stitch_y < viewport_bottom {
            continue;
        }
        let row_start = y as usize * width_px * 4;
        for x in 0..width_px {
            let index = row_start + x * 4;
            if buffer[index + 3] == 0 {
                continue;
            }
            buffer[index] = (buffer[index] as u32 * EDGE_MASK_NUMERATOR / 255) as u8;
            buffer[index + 1] = (buffer[index + 1] as u32 * EDGE_MASK_NUMERATOR / 255) as u8;
            buffer[index + 2] = (buffer[index + 2] as u32 * EDGE_MASK_NUMERATOR / 255) as u8;
        }
    }

    buffer
}

/// Places the strip beside the selection like the original (right of the
/// selection, `marginXS` gap); falls back to the left edge when the monitor
/// has no room on the right.
fn strip_screen_x(
    monitor_left: i32,
    monitor_right: i32,
    sel_left: i32,
    sel_width: i32,
    gap: i32,
    strip_width: i32,
) -> i32 {
    let right_candidate = sel_left + sel_width + gap;
    if right_candidate + strip_width <= monitor_right {
        return right_candidate;
    }
    let left_candidate = sel_left - gap - strip_width;
    if left_candidate >= monitor_left {
        return left_candidate;
    }
    (monitor_right - strip_width).max(monitor_left)
}

fn ensure_overlay_class() -> Result<u16, String> {
    OVERLAY_CLASS
        .get_or_init(|| register_class(OVERLAY_CLASS_NAME, overlay_window_proc, IDC_ARROW))
        .clone()
}

fn ensure_layered_class() -> Result<u16, String> {
    LAYERED_CLASS
        .get_or_init(|| register_class(LAYERED_CLASS_NAME, layered_window_proc, IDC_ARROW))
        .clone()
}

fn ensure_action_class() -> Result<u16, String> {
    ACTION_CLASS
        .get_or_init(|| register_class(ACTION_CLASS_NAME, action_window_proc, IDC_HAND))
        .clone()
}

fn register_class(
    name: PCWSTR,
    proc: unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT,
    cursor: PCWSTR,
) -> Result<u16, String> {
    let instance = module_instance()?;
    let cursor = unsafe { LoadCursorW(None, cursor) }
        .map_err(|error| format!("无法加载长截图光标：{error}"))?;
    let class = WNDCLASSW {
        lpfnWndProc: Some(proc),
        hInstance: instance,
        hCursor: cursor,
        lpszClassName: name,
        ..Default::default()
    };
    let atom = unsafe { RegisterClassW(&class) };
    if atom == 0 {
        Err(format!(
            "无法注册长截图窗口类：{}",
            windows::core::Error::from_win32()
        ))
    } else {
        Ok(atom)
    }
}

fn module_instance() -> Result<HINSTANCE, String> {
    let module = unsafe { GetModuleHandleW(None) }
        .map_err(|error| format!("无法获取应用模块句柄：{error}"))?;
    Ok(HINSTANCE(module.0))
}

fn high_word_signed(value: u32) -> i16 {
    ((value >> 16) & 0xffff) as u16 as i16
}

#[cfg(test)]
mod tests {
    use super::{
        ACTION_CANCEL, ACTION_COPY, ACTION_SAVE, StripSegment, action_bar_x, action_bar_y,
        action_zone, compose_strip, high_word_signed, rounded_rect_contains, strip_screen_x,
    };
    use image::RgbaImage;

    #[test]
    fn decodes_signed_wheel_delta() {
        assert_eq!(high_word_signed(120_u32 << 16), 120);
        assert_eq!(high_word_signed(((-120_i16) as u16 as u32) << 16), -120);
    }

    #[test]
    fn places_strip_right_of_selection_with_left_fallback() {
        assert_eq!(strip_screen_x(0, 1920, 100, 800, 8, 160), 908);
        assert_eq!(strip_screen_x(0, 1920, 1800, 100, 8, 160), 1632);
        // Selection spanning the whole monitor pins the strip to the right edge.
        assert_eq!(strip_screen_x(0, 1920, 0, 1920, 8, 160), 1760);
    }

    #[test]
    fn rounded_rect_keeps_core_and_trims_corners() {
        assert!(rounded_rect_contains(10, 0, 40, 20, 6));
        assert!(rounded_rect_contains(0, 10, 40, 20, 6));
        assert!(!rounded_rect_contains(0, 0, 40, 20, 6));
        assert!(!rounded_rect_contains(39, 19, 40, 20, 6));
    }

    #[test]
    fn compose_strip_masks_content_outside_viewport() {
        let thumb = RgbaImage::from_pixel(4, 6, image::Rgba([200, 100, 50, 255]));
        let segments = vec![StripSegment { top: 0, thumb }];
        let buffer = compose_strip(&segments, 0, 6, (0, 3), false, 1.0, 4, 6);

        // Inside the viewport: original colors in BGRA order, opaque.
        assert_eq!(&buffer[0..4], &[50, 100, 200, 255]);
        // Outside the viewport: dimmed to 68% brightness, still opaque.
        let masked = &buffer[5 * 4 * 4..5 * 4 * 4 + 4];
        assert_eq!(masked, &[34, 68, 136, 255]);
        // Rows never covered by a segment stay fully transparent.
        let empty = compose_strip(&[], 0, 6, (0, 3), false, 1.0, 4, 6);
        assert!(empty.iter().all(|byte| *byte == 0));
    }

    #[test]
    fn action_zone_splits_buttons_evenly() {
        assert_eq!(action_zone(0, 300), ACTION_SAVE);
        assert_eq!(action_zone(99, 300), ACTION_SAVE);
        assert_eq!(action_zone(100, 300), ACTION_CANCEL);
        assert_eq!(action_zone(299, 300), ACTION_COPY);
    }

    #[test]
    fn action_bar_sits_below_selection_with_above_fallback() {
        assert_eq!(action_bar_y(0, 1080, 100, 400, 8, 40), 508);
        // No room below: flips above the selection.
        assert_eq!(action_bar_y(0, 1080, 700, 360, 8, 40), 652);
        // Selection covers the monitor: pinned to the bottom edge.
        assert_eq!(action_bar_y(0, 1080, 0, 1080, 8, 40), 1040);
    }

    #[test]
    fn action_bar_right_aligns_to_selection_within_monitor() {
        assert_eq!(action_bar_x(0, 1920, 100, 800, 200), 700);
        // Clamped inside the monitor when the selection hugs the left edge.
        assert_eq!(action_bar_x(0, 1920, 0, 100, 200), 0);
    }

    #[test]
    fn compose_strip_scrolls_to_active_edge() {
        let thumb = RgbaImage::from_pixel(2, 8, image::Rgba([255, 255, 255, 255]));
        let segments = vec![StripSegment { top: 0, thumb }];
        // Content (8px) exceeds the 4px strip; scrolling down keeps the bottom
        // edge of the viewport (stitch px 8) at the strip bottom.
        let buffer = compose_strip(&segments, 0, 8, (4, 8), true, 1.0, 2, 4);
        // Visible rows are stitch px 4..8 — all inside the viewport, unmasked.
        assert!(buffer.chunks_exact(4).all(|px| px == [255, 255, 255, 255]));
    }
}
