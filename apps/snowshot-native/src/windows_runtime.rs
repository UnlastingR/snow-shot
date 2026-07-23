use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use global_hotkey::hotkey::{Code, HotKey, Modifiers};
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};
use slint::{
    CloseRequestResponse, ComponentHandle, Image, PhysicalPosition, PhysicalSize, Rgba8Pixel,
    SharedPixelBuffer,
};
use snow_shot_capture::PixelRect;
use windows::Win32::Foundation::HWND;

use crate::capture_workflow::{
    CaptureWorkflowError, FrozenMonitorFrame, FrozenRegionFrame, capture_monitor_to_clipboard,
    copy_frozen_region_to_clipboard, extract_frozen_region, freeze_monitor_under_cursor,
    save_frozen_region_to_path,
};
use crate::resize_geometry::project_size_to_aspect;
use crate::windows_pin::{
    PinCompositor, PinFrame, destroy_pin_window, hide_pin_window, is_pin_window, show_pin_window,
};
use crate::{AppTray, AppWindow, CaptureWindow};

const SCREENSHOT_SHORTCUT: &str = "Alt+F12";
const PIN_VISIBILITY_SHORTCUT: &str = "Alt+F11";
#[cfg(test)]
const PIN_MIN_WIDTH: f32 = 96.0;
#[cfg(test)]
const PIN_MIN_HEIGHT: f32 = 64.0;
#[cfg(test)]
const PIN_MAX_WIDTH: f32 = 4096.0;
#[cfg(test)]
const PIN_MAX_HEIGHT: f32 = 4096.0;
type SharedFrame = Arc<Mutex<Option<FrozenMonitorFrame>>>;
type SharedPins = Rc<RefCell<PinCollection>>;

#[derive(Debug, Clone, Copy, PartialEq)]
struct FloatPoint {
    x: f32,
    y: f32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct FloatRect {
    left: f32,
    top: f32,
    right: f32,
    bottom: f32,
}

impl FloatRect {
    fn width(self) -> f32 {
        self.right - self.left
    }

    fn height(self) -> f32 {
        self.bottom - self.top
    }

    fn center(self) -> FloatPoint {
        FloatPoint {
            x: (self.left + self.right) / 2.0,
            y: (self.top + self.bottom) / 2.0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResizeCorner {
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

impl ResizeCorner {
    fn from_mode(mode: i32) -> Option<Self> {
        match mode {
            1 => Some(Self::TopLeft),
            2 => Some(Self::TopRight),
            3 => Some(Self::BottomLeft),
            4 => Some(Self::BottomRight),
            _ => None,
        }
    }

    fn horizontal_sign(self) -> f32 {
        match self {
            Self::TopLeft | Self::BottomLeft => -1.0,
            Self::TopRight | Self::BottomRight => 1.0,
        }
    }

    fn vertical_sign(self) -> f32 {
        match self {
            Self::TopLeft | Self::TopRight => -1.0,
            Self::BottomLeft | Self::BottomRight => 1.0,
        }
    }

    fn point(self, rect: FloatRect) -> FloatPoint {
        match self {
            Self::TopLeft => FloatPoint {
                x: rect.left,
                y: rect.top,
            },
            Self::TopRight => FloatPoint {
                x: rect.right,
                y: rect.top,
            },
            Self::BottomLeft => FloatPoint {
                x: rect.left,
                y: rect.bottom,
            },
            Self::BottomRight => FloatPoint {
                x: rect.right,
                y: rect.bottom,
            },
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct ResizeLimits {
    bounds: Option<FloatRect>,
    min_width: f32,
    min_height: f32,
    max_width: f32,
    max_height: f32,
}

struct PinEntry {
    hwnd: HWND,
}

#[derive(Default)]
struct PinCollection {
    entries: Vec<PinEntry>,
    hidden_by_shortcut: bool,
    compositor: Option<Rc<PinCompositor>>,
}

impl PinCollection {
    fn prune_closed(&mut self) {
        self.entries.retain(|entry| is_pin_window(entry.hwnd));
        if self.entries.is_empty() {
            self.hidden_by_shortcut = false;
        }
    }
}

impl Drop for PinCollection {
    fn drop(&mut self) {
        for entry in self.entries.drain(..) {
            destroy_pin_window(entry.hwnd);
        }
    }
}

struct HotkeyRegistration {
    manager: GlobalHotKeyManager,
    hotkeys: Vec<HotKey>,
}

impl Drop for HotkeyRegistration {
    fn drop(&mut self) {
        for hotkey in self.hotkeys.drain(..) {
            let _ = self.manager.unregister(hotkey);
        }
    }
}

pub struct WindowsRuntime {
    _hotkeys: Option<HotkeyRegistration>,
}

impl WindowsRuntime {
    pub fn start(app: &AppWindow, tray: &AppTray, capture: &CaptureWindow) -> Self {
        app.window()
            .on_close_requested(|| CloseRequestResponse::HideWindow);

        let busy = Arc::new(AtomicBool::new(false));
        let frame = Arc::new(Mutex::new(None));
        let pins = Rc::new(RefCell::new(PinCollection::default()));

        bind_capture_callbacks(
            app,
            capture,
            Arc::clone(&busy),
            Arc::clone(&frame),
            Rc::clone(&pins),
        );
        bind_pin_visibility_callback(app, Rc::clone(&pins));
        bind_app_callbacks(app, capture, Arc::clone(&busy), Arc::clone(&frame));
        bind_tray_callbacks(app, tray, capture, Arc::clone(&busy), Arc::clone(&frame));

        let hotkeys = register_global_hotkeys(app, capture, busy, frame);

        Self { _hotkeys: hotkeys }
    }
}

fn register_global_hotkeys(
    app: &AppWindow,
    capture: &CaptureWindow,
    busy: Arc<AtomicBool>,
    frame: SharedFrame,
) -> Option<HotkeyRegistration> {
    let screenshot_hotkey = HotKey::new(Some(Modifiers::ALT), Code::F12);
    let pin_visibility_hotkey = HotKey::new(Some(Modifiers::ALT), Code::F11);
    let manager = match GlobalHotKeyManager::new() {
        Ok(manager) => manager,
        Err(error) => {
            app.set_runtime_status(
                format!("{SCREENSHOT_SHORTCUT} 注册失败：{error}。仍可使用界面或托盘截图。").into(),
            );
            return None;
        }
    };

    if let Err(error) = manager.register(screenshot_hotkey) {
        app.set_runtime_status(
            format!("{SCREENSHOT_SHORTCUT} 注册失败：{error}。仍可使用界面或托盘截图。").into(),
        );
        return None;
    }

    let screenshot_hotkey_id = screenshot_hotkey.id();
    let mut hotkeys = vec![screenshot_hotkey];
    let pin_visibility_hotkey_id = match manager.register(pin_visibility_hotkey) {
        Ok(()) => {
            hotkeys.push(pin_visibility_hotkey);
            Some(pin_visibility_hotkey.id())
        }
        Err(error) => {
            app.set_runtime_status(
                format!(
                    "{PIN_VISIBILITY_SHORTCUT} 注册失败：{error}。{SCREENSHOT_SHORTCUT} 截图仍可使用。"
                )
                .into(),
            );
            None
        }
    };

    let app_weak = app.as_weak();
    let capture_weak = capture.as_weak();
    GlobalHotKeyEvent::set_event_handler(Some(move |event: GlobalHotKeyEvent| {
        if event.state != HotKeyState::Pressed {
            return;
        }

        if event.id == screenshot_hotkey_id {
            let app_weak = app_weak.clone();
            let capture_weak = capture_weak.clone();
            let busy = Arc::clone(&busy);
            let frame = Arc::clone(&frame);
            let _ = slint::invoke_from_event_loop(move || {
                request_region_capture(app_weak, capture_weak, busy, frame);
            });
        } else if pin_visibility_hotkey_id.is_some_and(|id| event.id == id) {
            let app_weak = app_weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(app) = app_weak.upgrade() {
                    app.invoke_toggle_pin_visibility_requested();
                }
            });
        }
    }));
    if pin_visibility_hotkey_id.is_some() {
        app.set_runtime_status(
            format!("{SCREENSHOT_SHORTCUT} 截图；{PIN_VISIBILITY_SHORTCUT} 隐藏或显示全部贴图。")
                .into(),
        );
    }

    Some(HotkeyRegistration { manager, hotkeys })
}

fn bind_capture_callbacks(
    app: &AppWindow,
    capture: &CaptureWindow,
    busy: Arc<AtomicBool>,
    frame: SharedFrame,
    pins: SharedPins,
) {
    let app_weak = app.as_weak();
    let capture_weak = capture.as_weak();
    let confirm_busy = Arc::clone(&busy);
    let confirm_frame = Arc::clone(&frame);
    capture.on_selection_confirmed(move |left, top, right, bottom| {
        let result = confirm_frame
            .lock()
            .map_err(|_| "截图会话状态不可用。".to_string())
            .and_then(|guard| {
                let frame = guard
                    .as_ref()
                    .ok_or_else(|| format!("截图会话已结束，请重新按 {SCREENSHOT_SHORTCUT}。"))?;
                let region = normalized_region(frame, left, top, right, bottom)
                    .map_err(|error| error.to_string())?;
                copy_frozen_region_to_clipboard(frame, region).map_err(|error| error.to_string())
            });

        finish_region_capture(
            &app_weak,
            &capture_weak,
            &confirm_busy,
            &confirm_frame,
            match result {
                Ok(summary) => format!(
                    "已复制 {}×{} 区域截图到剪贴板。按 {SCREENSHOT_SHORTCUT} 可再次截图。",
                    summary.width(),
                    summary.height()
                ),
                Err(error) => error,
            },
        );
    });

    let app_weak = app.as_weak();
    let capture_weak = capture.as_weak();
    let save_busy = Arc::clone(&busy);
    let save_frame = Arc::clone(&frame);
    capture.on_selection_save_requested(move |left, top, right, bottom| {
        let region = save_frame
            .lock()
            .map_err(|_| "截图会话状态不可用。".to_string())
            .and_then(|guard| {
                let frame = guard
                    .as_ref()
                    .ok_or_else(|| format!("截图会话已结束，请重新按 {SCREENSHOT_SHORTCUT}。"))?;
                normalized_region(frame, left, top, right, bottom)
                    .map_err(|error| error.to_string())
            });

        let region = match region {
            Ok(region) => region,
            Err(error) => {
                finish_region_capture(&app_weak, &capture_weak, &save_busy, &save_frame, error);
                return;
            }
        };

        if let Some(capture) = capture_weak.upgrade() {
            let _ = capture.hide();
        }

        let Some(path) = rfd::FileDialog::new()
            .add_filter("PNG 图片", &["png"])
            .set_file_name("snow-shot.png")
            .set_title("保存 Snow Shot 截图")
            .save_file()
        else {
            resume_region_capture(
                &app_weak,
                &capture_weak,
                &save_busy,
                &save_frame,
                "已取消保存，当前选区仍可继续处理。",
            );
            return;
        };

        let result = save_frame
            .lock()
            .map_err(|_| "截图会话状态不可用。".to_string())
            .and_then(|guard| {
                let frame = guard
                    .as_ref()
                    .ok_or_else(|| format!("截图会话已结束，请重新按 {SCREENSHOT_SHORTCUT}。"))?;
                save_frozen_region_to_path(frame, region, &path).map_err(|error| error.to_string())
            });

        match result {
            Ok(summary) => finish_region_capture(
                &app_weak,
                &capture_weak,
                &save_busy,
                &save_frame,
                format!(
                    "已保存 {}×{} 区域截图到 {}。",
                    summary.width(),
                    summary.height(),
                    path.display()
                ),
            ),
            Err(error) => {
                resume_region_capture(&app_weak, &capture_weak, &save_busy, &save_frame, &error)
            }
        }
    });

    let app_weak = app.as_weak();
    let capture_weak = capture.as_weak();
    let pin_busy = Arc::clone(&busy);
    let pin_frame = Arc::clone(&frame);
    let presented_pins = Rc::clone(&pins);
    capture.on_selection_pin_requested(move |left, top, right, bottom| {
        let result = pin_frame
            .lock()
            .map_err(|_| "截图会话状态不可用。".to_string())
            .and_then(|guard| {
                let frame = guard
                    .as_ref()
                    .ok_or_else(|| format!("截图会话已结束，请重新按 {SCREENSHOT_SHORTCUT}。"))?;
                let region = normalized_region(frame, left, top, right, bottom)
                    .map_err(|error| error.to_string())?;
                let pinned =
                    extract_frozen_region(frame, region).map_err(|error| error.to_string())?;
                let region_x = i32::try_from(region.x()).unwrap_or(i32::MAX);
                let region_y = i32::try_from(region.y()).unwrap_or(i32::MAX);

                Ok((
                    frame.origin_x().saturating_add(region_x),
                    frame.origin_y().saturating_add(region_y),
                    pinned,
                ))
            })
            .and_then(|(origin_x, origin_y, pinned)| {
                create_pinned_frame(&presented_pins, pinned, origin_x, origin_y)
            });

        match result {
            Ok((width, height)) => finish_region_capture(
                &app_weak,
                &capture_weak,
                &pin_busy,
                &pin_frame,
                format!("已创建 {width}×{height} 置顶贴图。"),
            ),
            Err(error) => set_status(&app_weak, &error),
        }
    });

    let capture_weak = capture.as_weak();
    capture.on_selection_transform_requested(
        move |mode,
              start_left,
              start_top,
              start_right,
              start_bottom,
              start_pointer_x,
              start_pointer_y,
              pointer_x,
              pointer_y,
              canvas_width,
              canvas_height,
              preserve_aspect,
              centered| {
            let start = FloatRect {
                left: start_left,
                top: start_top,
                right: start_right,
                bottom: start_bottom,
            };
            let transformed = transform_selection(
                mode,
                start,
                FloatPoint {
                    x: start_pointer_x,
                    y: start_pointer_y,
                },
                FloatPoint {
                    x: pointer_x,
                    y: pointer_y,
                },
                FloatRect {
                    left: 0.0,
                    top: 0.0,
                    right: canvas_width,
                    bottom: canvas_height,
                },
                preserve_aspect,
                centered,
            );

            if let (Some(capture), Some(transformed)) = (capture_weak.upgrade(), transformed) {
                capture.invoke_apply_selection(
                    transformed.left,
                    transformed.top,
                    transformed.right,
                    transformed.bottom,
                );
            }
        },
    );

    let app_weak = app.as_weak();
    let capture_weak = capture.as_weak();
    let cancel_busy = Arc::clone(&busy);
    let cancel_frame = Arc::clone(&frame);
    capture.on_cancelled(move || {
        finish_region_capture(
            &app_weak,
            &capture_weak,
            &cancel_busy,
            &cancel_frame,
            "已取消区域截图。".to_string(),
        );
    });

    let app_weak = app.as_weak();
    let capture_weak = capture.as_weak();
    capture.window().on_close_requested(move || {
        if let Some(capture) = capture_weak.upgrade() {
            capture.set_frozen_frame(Image::default());
        }
        clear_frame(&frame);
        busy.store(false, Ordering::Release);
        set_status(&app_weak, "已取消区域截图。");
        CloseRequestResponse::HideWindow
    });
}

fn bind_pin_visibility_callback(app: &AppWindow, pins: SharedPins) {
    let app_weak = app.as_weak();
    app.on_toggle_pin_visibility_requested(move || {
        toggle_pin_visibility(&app_weak, &pins);
    });
}

fn bind_app_callbacks(
    app: &AppWindow,
    capture: &CaptureWindow,
    busy: Arc<AtomicBool>,
    frame: SharedFrame,
) {
    let app_weak = app.as_weak();
    let capture_weak = capture.as_weak();
    app.on_capture_clicked(move || {
        request_region_capture(
            app_weak.clone(),
            capture_weak.clone(),
            Arc::clone(&busy),
            Arc::clone(&frame),
        );
    });

    let app_weak = app.as_weak();
    app.on_autostart_clicked(move || {
        set_status(&app_weak, "开机启动将在配置持久化接入后启用。");
    });

    let app_weak = app.as_weak();
    app.on_close_behavior_clicked(move || {
        set_status(&app_weak, "关闭设置窗口时会隐藏到系统托盘。");
    });

    let app_weak = app.as_weak();
    app.on_ocr_config_clicked(move || {
        set_status(&app_weak, "OCR Core 已保留，原生配置页将在截图闭环后接入。");
    });
}

fn bind_tray_callbacks(
    app: &AppWindow,
    tray: &AppTray,
    capture: &CaptureWindow,
    busy: Arc<AtomicBool>,
    frame: SharedFrame,
) {
    let app_weak = app.as_weak();
    tray.on_open_settings(move || show_settings(&app_weak));

    let app_weak = app.as_weak();
    let capture_weak = capture.as_weak();
    let capture_busy = Arc::clone(&busy);
    tray.on_capture_clicked(move || {
        request_region_capture(
            app_weak.clone(),
            capture_weak.clone(),
            Arc::clone(&capture_busy),
            Arc::clone(&frame),
        );
    });

    let app_weak = app.as_weak();
    tray.on_full_monitor_clicked(move || {
        request_full_monitor_copy(app_weak.clone(), Arc::clone(&busy));
    });

    tray.on_quit_requested(|| {
        let _ = slint::quit_event_loop();
    });
}

fn request_region_capture(
    app_weak: slint::Weak<AppWindow>,
    capture_weak: slint::Weak<CaptureWindow>,
    busy: Arc<AtomicBool>,
    frame: SharedFrame,
) {
    if !begin_capture(&app_weak, &busy) {
        return;
    }

    let worker_app = app_weak.clone();
    let worker_busy = Arc::clone(&busy);
    let worker_frame = Arc::clone(&frame);
    let spawn_result = thread::Builder::new()
        .name("snowshot-region-capture".to_string())
        .spawn(move || {
            let result = freeze_monitor_under_cursor();

            match result {
                Ok(frozen) => {
                    let event_busy = Arc::clone(&worker_busy);
                    let event_frame = Arc::clone(&worker_frame);
                    let event_capture = capture_weak.clone();
                    let invoke_result = worker_app.upgrade_in_event_loop(move |app| {
                        let Some(capture) = event_capture.upgrade() else {
                            event_busy.store(false, Ordering::Release);
                            app.set_runtime_status("区域截图窗口已不可用。".into());
                            return;
                        };

                        match present_frozen_frame(&capture, &frozen) {
                            Ok(()) => {
                                if let Ok(mut current) = event_frame.lock() {
                                    *current = Some(frozen);
                                    app.set_runtime_status(
                                        "区域截图中：拖动框选，可复制、保存或贴图；Esc 取消。"
                                            .into(),
                                    );
                                } else {
                                    event_busy.store(false, Ordering::Release);
                                    let _ = capture.hide();
                                    capture.set_frozen_frame(Image::default());
                                    app.set_runtime_status("截图会话状态不可用。".into());
                                }
                            }
                            Err(error) => {
                                event_busy.store(false, Ordering::Release);
                                app.set_runtime_status(error.into());
                            }
                        }
                    });

                    if invoke_result.is_err() {
                        worker_busy.store(false, Ordering::Release);
                    }
                }
                Err(error) => {
                    worker_busy.store(false, Ordering::Release);
                    let status = error.to_string();
                    let _ = worker_app.upgrade_in_event_loop(move |app| {
                        app.set_runtime_status(status.into());
                    });
                }
            }
        });

    if let Err(error) = spawn_result {
        busy.store(false, Ordering::Release);
        set_status(&app_weak, &format!("无法启动截图任务：{error}"));
    }
}

fn request_full_monitor_copy(app_weak: slint::Weak<AppWindow>, busy: Arc<AtomicBool>) {
    if !begin_capture(&app_weak, &busy) {
        return;
    }

    let worker_app = app_weak.clone();
    let worker_busy = Arc::clone(&busy);
    let spawn_result = thread::Builder::new()
        .name("snowshot-monitor-copy".to_string())
        .spawn(move || {
            let result = capture_monitor_to_clipboard();
            worker_busy.store(false, Ordering::Release);

            let status = match result {
                Ok(summary) => format!(
                    "已复制 {}×{} 显示器截图到剪贴板。",
                    summary.width(),
                    summary.height()
                ),
                Err(error) => error.to_string(),
            };
            let _ = worker_app.upgrade_in_event_loop(move |app| {
                app.set_runtime_status(status.into());
            });
        });

    if let Err(error) = spawn_result {
        busy.store(false, Ordering::Release);
        set_status(&app_weak, &format!("无法启动截图任务：{error}"));
    }
}

fn begin_capture(app_weak: &slint::Weak<AppWindow>, busy: &AtomicBool) -> bool {
    if busy
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        set_status(app_weak, "已有截图任务正在执行。");
        return false;
    }

    true
}

fn present_frozen_frame(capture: &CaptureWindow, frame: &FrozenMonitorFrame) -> Result<(), String> {
    let mut pixels = SharedPixelBuffer::<Rgba8Pixel>::new(frame.width(), frame.height());
    if pixels.make_mut_bytes().len() != frame.rgba().len() {
        return Err("冻结帧尺寸与像素数据不一致。".to_string());
    }
    pixels.make_mut_bytes().copy_from_slice(frame.rgba());

    capture
        .window()
        .set_position(PhysicalPosition::new(frame.origin_x(), frame.origin_y()));
    capture
        .window()
        .set_size(PhysicalSize::new(frame.width(), frame.height()));
    capture.set_frozen_frame(Image::from_rgba8(pixels));
    capture.invoke_prepare_selection();
    capture
        .show()
        .map_err(|error| format!("无法显示区域截图窗口：{error}"))?;
    capture.invoke_focus_selection();

    Ok(())
}

fn create_pinned_frame(
    pins: &SharedPins,
    frame: FrozenRegionFrame,
    origin_x: i32,
    origin_y: i32,
) -> Result<(u32, u32), String> {
    let compositor = {
        let mut collection = pins.borrow_mut();
        collection.prune_closed();
        if collection.compositor.is_none() {
            collection.compositor = Some(PinCompositor::new()?);
        }
        Rc::clone(
            collection
                .compositor
                .as_ref()
                .expect("compositor initialized above"),
        )
    };

    let (window_width, window_height) = fitted_pin_size(frame.width(), frame.height());
    let hwnd = compositor.create_pin(PinFrame {
        rgba: frame.rgba(),
        source_width: frame.width(),
        source_height: frame.height(),
        display_width: window_width,
        display_height: window_height,
        origin_x,
        origin_y,
    })?;

    let pins_to_restore = {
        let mut collection = pins.borrow_mut();
        let restore = collection.hidden_by_shortcut;
        collection.hidden_by_shortcut = false;
        restore.then(|| {
            collection
                .entries
                .iter()
                .map(|entry| entry.hwnd)
                .collect::<Vec<_>>()
        })
    };
    if let Some(pins_to_restore) = pins_to_restore {
        for pin_hwnd in pins_to_restore {
            show_pin_window(pin_hwnd);
        }
    }

    pins.borrow_mut().entries.push(PinEntry { hwnd });

    Ok((frame.width(), frame.height()))
}

fn fitted_pin_size(width: u32, height: u32) -> (u32, u32) {
    const MAX_WIDTH: f32 = 960.0;
    const MAX_HEIGHT: f32 = 720.0;
    const MIN_WIDTH: f32 = 96.0;
    const MIN_HEIGHT: f32 = 64.0;

    let width = width as f32;
    let height = height as f32;
    let scale = if width > MAX_WIDTH || height > MAX_HEIGHT {
        (MAX_WIDTH / width).min(MAX_HEIGHT / height)
    } else {
        (MIN_WIDTH / width).max(MIN_HEIGHT / height).max(1.0)
    };
    let width = (width * scale).round() as u32;
    let height = (height * scale).round() as u32;

    (width.max(1), height.max(1))
}

fn normalized_region(
    frame: &FrozenMonitorFrame,
    left: f32,
    top: f32,
    right: f32,
    bottom: f32,
) -> Result<PixelRect, CaptureWorkflowError> {
    let left = left.clamp(0.0, 1.0);
    let top = top.clamp(0.0, 1.0);
    let right = right.clamp(left, 1.0);
    let bottom = bottom.clamp(top, 1.0);
    let x = (left * frame.width() as f32).floor() as u32;
    let y = (top * frame.height() as f32).floor() as u32;
    let max_x = ((right * frame.width() as f32).ceil() as u32).min(frame.width());
    let max_y = ((bottom * frame.height() as f32).ceil() as u32).min(frame.height());

    PixelRect::new(x, y, max_x.saturating_sub(x), max_y.saturating_sub(y))
        .map_err(CaptureWorkflowError::Capture)
}

fn transform_selection(
    mode: i32,
    start: FloatRect,
    start_pointer: FloatPoint,
    pointer: FloatPoint,
    bounds: FloatRect,
    preserve_aspect: bool,
    centered: bool,
) -> Option<FloatRect> {
    if mode == 0 {
        return Some(move_rect_within_bounds(
            start,
            pointer.x - start_pointer.x,
            pointer.y - start_pointer.y,
            bounds,
        ));
    }

    let corner = ResizeCorner::from_mode(mode)?;
    let start_corner = corner.point(start);
    let effective_pointer = FloatPoint {
        x: start_corner.x + pointer.x - start_pointer.x,
        y: start_corner.y + pointer.y - start_pointer.y,
    };
    let aspect = start.width() / start.height();
    Some(resize_rect_from_pointer(
        start,
        effective_pointer,
        corner,
        preserve_aspect,
        centered,
        aspect,
        ResizeLimits {
            bounds: Some(bounds),
            min_width: 6.0,
            min_height: 6.0,
            max_width: bounds.width(),
            max_height: bounds.height(),
        },
    ))
}

fn move_rect_within_bounds(
    rect: FloatRect,
    delta_x: f32,
    delta_y: f32,
    bounds: FloatRect,
) -> FloatRect {
    let left = (rect.left + delta_x).clamp(bounds.left, bounds.right - rect.width());
    let top = (rect.top + delta_y).clamp(bounds.top, bounds.bottom - rect.height());

    FloatRect {
        left,
        top,
        right: left + rect.width(),
        bottom: top + rect.height(),
    }
}

fn resize_rect_from_pointer(
    start: FloatRect,
    pointer: FloatPoint,
    corner: ResizeCorner,
    preserve_aspect: bool,
    centered: bool,
    aspect: f32,
    limits: ResizeLimits,
) -> FloatRect {
    let horizontal_sign = corner.horizontal_sign();
    let vertical_sign = corner.vertical_sign();
    let center = start.center();
    let (fixed_x, fixed_y, raw_width, raw_height) = if centered {
        (
            center.x,
            center.y,
            horizontal_sign * (pointer.x - center.x) * 2.0,
            vertical_sign * (pointer.y - center.y) * 2.0,
        )
    } else {
        let fixed_x = if horizontal_sign > 0.0 {
            start.left
        } else {
            start.right
        };
        let fixed_y = if vertical_sign > 0.0 {
            start.top
        } else {
            start.bottom
        };
        (
            fixed_x,
            fixed_y,
            horizontal_sign * (pointer.x - fixed_x),
            vertical_sign * (pointer.y - fixed_y),
        )
    };

    let (available_width, available_height) = match limits.bounds {
        Some(bounds) if centered => (
            ((fixed_x - bounds.left).min(bounds.right - fixed_x) * 2.0).max(1.0),
            ((fixed_y - bounds.top).min(bounds.bottom - fixed_y) * 2.0).max(1.0),
        ),
        Some(bounds) => (
            if horizontal_sign > 0.0 {
                bounds.right - fixed_x
            } else {
                fixed_x - bounds.left
            }
            .max(1.0),
            if vertical_sign > 0.0 {
                bounds.bottom - fixed_y
            } else {
                fixed_y - bounds.top
            }
            .max(1.0),
        ),
        None => (limits.max_width, limits.max_height),
    };
    let max_width = limits.max_width.min(available_width).max(1.0);
    let max_height = limits.max_height.min(available_height).max(1.0);

    let (width, height) = if preserve_aspect {
        let aspect = if aspect.is_finite() && aspect > 0.0 {
            aspect
        } else {
            start.width() / start.height()
        };
        let (_, projected_height) = project_size_to_aspect(raw_width, raw_height, aspect);
        let mut height = projected_height.max(f32::MIN_POSITIVE);
        let mut width = height * aspect;

        let grow_for_minimum = (limits.min_width / width)
            .max(limits.min_height / height)
            .max(1.0);
        width *= grow_for_minimum;
        height *= grow_for_minimum;

        let shrink_for_maximum = (max_width / width).min(max_height / height).min(1.0);
        width *= shrink_for_maximum;
        height *= shrink_for_maximum;
        (width.max(1.0), height.max(1.0))
    } else {
        (
            raw_width.clamp(limits.min_width.min(max_width), max_width),
            raw_height.clamp(limits.min_height.min(max_height), max_height),
        )
    };

    if centered {
        FloatRect {
            left: fixed_x - width / 2.0,
            top: fixed_y - height / 2.0,
            right: fixed_x + width / 2.0,
            bottom: fixed_y + height / 2.0,
        }
    } else {
        let (left, right) = if horizontal_sign > 0.0 {
            (fixed_x, fixed_x + width)
        } else {
            (fixed_x - width, fixed_x)
        };
        let (top, bottom) = if vertical_sign > 0.0 {
            (fixed_y, fixed_y + height)
        } else {
            (fixed_y - height, fixed_y)
        };
        FloatRect {
            left,
            top,
            right,
            bottom,
        }
    }
}

#[cfg(test)]
fn scale_rect_around_point(
    rect: FloatRect,
    anchor: FloatPoint,
    requested_factor: f32,
    limits: ResizeLimits,
) -> FloatRect {
    let width = rect.width().max(1.0);
    let height = rect.height().max(1.0);
    let minimum_factor = (limits.min_width / width)
        .max(limits.min_height / height)
        .max(f32::MIN_POSITIVE);
    let maximum_factor = (limits.max_width / width)
        .min(limits.max_height / height)
        .max(minimum_factor);
    let factor = requested_factor.clamp(minimum_factor, maximum_factor);
    let anchor_x = ((anchor.x - rect.left) / width).clamp(0.0, 1.0);
    let anchor_y = ((anchor.y - rect.top) / height).clamp(0.0, 1.0);
    let new_width = width * factor;
    let new_height = height * factor;
    let left = anchor.x - anchor_x * new_width;
    let top = anchor.y - anchor_y * new_height;

    FloatRect {
        left,
        top,
        right: left + new_width,
        bottom: top + new_height,
    }
}

fn finish_region_capture(
    app_weak: &slint::Weak<AppWindow>,
    capture_weak: &slint::Weak<CaptureWindow>,
    busy: &AtomicBool,
    frame: &SharedFrame,
    status: String,
) {
    if let Some(capture) = capture_weak.upgrade() {
        let _ = capture.hide();
        capture.set_frozen_frame(Image::default());
    }
    clear_frame(frame);
    busy.store(false, Ordering::Release);
    set_status(app_weak, &status);
}

fn resume_region_capture(
    app_weak: &slint::Weak<AppWindow>,
    capture_weak: &slint::Weak<CaptureWindow>,
    busy: &AtomicBool,
    frame: &SharedFrame,
    status: &str,
) {
    let show_result = capture_weak
        .upgrade()
        .ok_or_else(|| "区域截图窗口已不可用。".to_string())
        .and_then(|capture| {
            capture
                .show()
                .map_err(|error| format!("无法恢复区域截图窗口：{error}"))?;
            capture.invoke_focus_selection();
            Ok(())
        });

    if let Err(error) = show_result {
        clear_frame(frame);
        busy.store(false, Ordering::Release);
        set_status(app_weak, &error);
    } else {
        set_status(app_weak, status);
    }
}

fn clear_frame(frame: &SharedFrame) {
    if let Ok(mut current) = frame.lock() {
        current.take();
    }
}

fn toggle_pin_visibility(app_weak: &slint::Weak<AppWindow>, pins: &SharedPins) {
    let (should_hide, pin_windows) = {
        let mut collection = pins.borrow_mut();
        collection.prune_closed();
        if collection.entries.is_empty() {
            set_status(app_weak, "当前没有可隐藏的贴图。");
            return;
        }
        (
            !collection.hidden_by_shortcut,
            collection
                .entries
                .iter()
                .map(|entry| entry.hwnd)
                .collect::<Vec<_>>(),
        )
    };

    for hwnd in &pin_windows {
        if should_hide {
            hide_pin_window(*hwnd);
        } else {
            show_pin_window(*hwnd);
        }
    }

    pins.borrow_mut().hidden_by_shortcut = should_hide;

    let count = pin_windows.len();
    if should_hide {
        set_status(
            app_weak,
            &format!("{PIN_VISIBILITY_SHORTCUT}：已隐藏 {count} 张贴图。"),
        );
    } else {
        set_status(
            app_weak,
            &format!("{PIN_VISIBILITY_SHORTCUT}：已显示 {count} 张贴图。"),
        );
    }
}

fn show_settings(app_weak: &slint::Weak<AppWindow>) {
    if let Some(app) = app_weak.upgrade() {
        let _ = app.show();
        app.window().request_redraw();
    }
}

fn set_status(app_weak: &slint::Weak<AppWindow>, status: &str) {
    if let Some(app) = app_weak.upgrade() {
        app.set_runtime_status(status.into());
    }
}

#[cfg(test)]
mod tests {
    use super::{
        FloatPoint, FloatRect, PIN_MAX_HEIGHT, PIN_MAX_WIDTH, PIN_MIN_HEIGHT, PIN_MIN_WIDTH,
        ResizeCorner, ResizeLimits, fitted_pin_size, resize_rect_from_pointer,
        scale_rect_around_point, transform_selection,
    };

    fn assert_rect(actual: FloatRect, expected: FloatRect) {
        const EPSILON: f32 = 0.001;
        assert!((actual.left - expected.left).abs() < EPSILON);
        assert!((actual.top - expected.top).abs() < EPSILON);
        assert!((actual.right - expected.right).abs() < EPSILON);
        assert!((actual.bottom - expected.bottom).abs() < EPSILON);
    }

    #[test]
    fn pin_window_caps_large_regions_without_changing_aspect_ratio() {
        assert_eq!(fitted_pin_size(1920, 1080), (960, 540));
    }

    #[test]
    fn pin_window_keeps_close_control_reachable_for_small_regions() {
        assert_eq!(fitted_pin_size(20, 10), (128, 64));
    }

    #[test]
    fn selection_move_stays_inside_canvas() {
        let moved = transform_selection(
            0,
            FloatRect {
                left: 10.0,
                top: 10.0,
                right: 30.0,
                bottom: 30.0,
            },
            FloatPoint { x: 20.0, y: 20.0 },
            FloatPoint { x: 95.0, y: 95.0 },
            FloatRect {
                left: 0.0,
                top: 0.0,
                right: 100.0,
                bottom: 100.0,
            },
            false,
            false,
        )
        .unwrap();

        assert_rect(
            moved,
            FloatRect {
                left: 80.0,
                top: 80.0,
                right: 100.0,
                bottom: 100.0,
            },
        );
    }

    #[test]
    fn selection_corner_resize_tracks_pointer_without_modifiers() {
        let resized = transform_selection(
            4,
            FloatRect {
                left: 10.0,
                top: 10.0,
                right: 50.0,
                bottom: 30.0,
            },
            FloatPoint { x: 50.0, y: 30.0 },
            FloatPoint { x: 80.0, y: 60.0 },
            FloatRect {
                left: 0.0,
                top: 0.0,
                right: 200.0,
                bottom: 200.0,
            },
            false,
            false,
        )
        .unwrap();

        assert_rect(
            resized,
            FloatRect {
                left: 10.0,
                top: 10.0,
                right: 80.0,
                bottom: 60.0,
            },
        );
    }

    #[test]
    fn shift_preserves_selection_aspect_ratio() {
        let resized = transform_selection(
            4,
            FloatRect {
                left: 10.0,
                top: 10.0,
                right: 50.0,
                bottom: 30.0,
            },
            FloatPoint { x: 50.0, y: 30.0 },
            FloatPoint { x: 70.0, y: 60.0 },
            FloatRect {
                left: 0.0,
                top: 0.0,
                right: 200.0,
                bottom: 200.0,
            },
            true,
            false,
        )
        .unwrap();

        assert_rect(
            resized,
            FloatRect {
                left: 10.0,
                top: 10.0,
                right: 78.0,
                bottom: 44.0,
            },
        );
    }

    #[test]
    fn control_resizes_selection_around_center() {
        let resized = transform_selection(
            4,
            FloatRect {
                left: 30.0,
                top: 30.0,
                right: 70.0,
                bottom: 50.0,
            },
            FloatPoint { x: 70.0, y: 50.0 },
            FloatPoint { x: 80.0, y: 70.0 },
            FloatRect {
                left: 0.0,
                top: 0.0,
                right: 200.0,
                bottom: 200.0,
            },
            false,
            true,
        )
        .unwrap();

        assert_rect(
            resized,
            FloatRect {
                left: 20.0,
                top: 10.0,
                right: 80.0,
                bottom: 70.0,
            },
        );
    }

    #[test]
    fn shift_and_control_preserve_ratio_and_center() {
        let resized = transform_selection(
            4,
            FloatRect {
                left: 80.0,
                top: 80.0,
                right: 120.0,
                bottom: 100.0,
            },
            FloatPoint { x: 120.0, y: 100.0 },
            FloatPoint { x: 130.0, y: 120.0 },
            FloatRect {
                left: 0.0,
                top: 0.0,
                right: 200.0,
                bottom: 200.0,
            },
            true,
            true,
        )
        .unwrap();

        assert_rect(
            resized,
            FloatRect {
                left: 64.0,
                top: 72.0,
                right: 136.0,
                bottom: 108.0,
            },
        );
    }

    #[test]
    fn pin_corner_resize_always_preserves_ratio() {
        let resized = resize_rect_from_pointer(
            FloatRect {
                left: 0.0,
                top: 0.0,
                right: 400.0,
                bottom: 200.0,
            },
            FloatPoint { x: 600.0, y: 400.0 },
            ResizeCorner::BottomRight,
            true,
            false,
            2.0,
            ResizeLimits {
                bounds: None,
                min_width: PIN_MIN_WIDTH,
                min_height: PIN_MIN_HEIGHT,
                max_width: PIN_MAX_WIDTH,
                max_height: PIN_MAX_HEIGHT,
            },
        );

        assert_rect(
            resized,
            FloatRect {
                left: 0.0,
                top: 0.0,
                right: 640.0,
                bottom: 320.0,
            },
        );
    }

    #[test]
    fn selection_resize_preserves_pointer_offset_inside_handle() {
        let start = FloatRect {
            left: 10.0,
            top: 10.0,
            right: 50.0,
            bottom: 30.0,
        };
        let unchanged = transform_selection(
            4,
            start,
            FloatPoint { x: 47.0, y: 27.0 },
            FloatPoint { x: 47.0, y: 27.0 },
            FloatRect {
                left: 0.0,
                top: 0.0,
                right: 200.0,
                bottom: 200.0,
            },
            false,
            false,
        )
        .unwrap();

        assert_rect(unchanged, start);
    }

    #[test]
    fn ctrl_wheel_zoom_keeps_pointer_anchor_fixed() {
        let scaled = scale_rect_around_point(
            FloatRect {
                left: 100.0,
                top: 100.0,
                right: 300.0,
                bottom: 200.0,
            },
            FloatPoint { x: 100.0, y: 100.0 },
            1.1,
            ResizeLimits {
                bounds: None,
                min_width: PIN_MIN_WIDTH,
                min_height: PIN_MIN_HEIGHT,
                max_width: PIN_MAX_WIDTH,
                max_height: PIN_MAX_HEIGHT,
            },
        );

        assert_rect(
            scaled,
            FloatRect {
                left: 100.0,
                top: 100.0,
                right: 320.0,
                bottom: 210.0,
            },
        );
    }
}
