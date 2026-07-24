use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak as RcWeak};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use global_hotkey::hotkey::{Code, HotKey, Modifiers};
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};
use slint::{
    CloseRequestResponse, Color, ComponentHandle, Image, PhysicalPosition, PhysicalSize,
    RenderingState, Rgba8Pixel, SharedPixelBuffer, Timer, TimerMode,
};
use snow_shot_annotate::{
    AnnotationDocument, AnnotationTool, ElementStyle, LayerCommand, OcrBlock, OcrLayerStyle, Point,
    RgbaColor, StylePatch, TextAlignment,
};
use snow_shot_capture::PixelRect;
use snow_shot_ocr::OcrDetectResult;
use snow_shot_window::{WindowRect, WindowTarget, list_windows};
use windows::Win32::Foundation::{HWND, POINT};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, VK_CONTROL, VK_DOWN, VK_ESCAPE, VK_LBUTTON, VK_LEFT, VK_RIGHT, VK_SHIFT,
    VK_UP,
};
use windows::Win32::UI::WindowsAndMessaging::{GetCursorPos, SetCursorPos};

use crate::capture_workflow::{
    CaptureWorkflowError, FrozenMonitorFrame, FrozenRegionFrame, capture_monitor_to_clipboard,
    copy_region_frame_to_clipboard, extract_frozen_region, freeze_monitor_under_cursor,
    save_region_frame_to_path,
};
use crate::native_settings::NativeSettings;
use crate::ocr_workflow::OcrWorker;
use crate::resize_geometry::project_size_to_aspect;
use crate::windows_pin::{
    PinCompositor, PinFrame, destroy_pin_window, hide_pin_window, is_pin_window, show_pin_window,
};
use crate::{AppTray, AppWindow, CaptureWindow};

const SCREENSHOT_SHORTCUT: &str = "Alt+F12";
const PIN_VISIBILITY_SHORTCUT: &str = "Alt+F11";
const ESCAPE_POLL_INTERVAL: Duration = Duration::from_millis(16);
#[cfg(test)]
const PIN_MIN_WIDTH: f32 = 96.0;
#[cfg(test)]
const PIN_MIN_HEIGHT: f32 = 64.0;
#[cfg(test)]
const PIN_MAX_WIDTH: f32 = 4096.0;
#[cfg(test)]
const PIN_MAX_HEIGHT: f32 = 4096.0;
type SharedCaptureSession = Arc<CaptureSession>;
type SharedPins = Rc<RefCell<PinCollection>>;
type SharedCaptureWindow = Rc<RefCell<Option<CaptureWindow>>>;
type WeakCaptureWindow = RcWeak<RefCell<Option<CaptureWindow>>>;
type SharedOcrContext = Rc<OcrContext>;

#[derive(Default)]
struct CaptureSession {
    busy: AtomicBool,
    visible: AtomicBool,
    finishing: AtomicBool,
    frame: Mutex<Option<FrozenMonitorFrame>>,
    window_targets: Mutex<Vec<WindowTarget>>,
    annotation: Mutex<Option<AnnotationState>>,
    settings: Mutex<NativeSettings>,
    sampled_color: Mutex<Option<SampledColor>>,
    floating_panels: Mutex<FloatingPanelPositions>,
    color_format: AtomicU8,
}

#[derive(Debug, Default, Clone, Copy)]
struct FloatingPanelPositions {
    toolbar: Option<FloatPoint>,
    properties: Option<FloatPoint>,
}

#[derive(Debug, Clone, Copy)]
struct SampledColor {
    color: RgbaColor,
    x: i32,
    y: i32,
}

struct AnnotationState {
    region: PixelRect,
    document: AnnotationDocument,
    tool: i32,
    previous_tool: i32,
    color: RgbaColor,
    stroke_width: f32,
    style: ElementStyle,
}

struct AnnotationUiSnapshot {
    width: u32,
    height: u32,
    pixels: Vec<u8>,
    tool: i32,
    can_undo: bool,
    can_redo: bool,
    color: RgbaColor,
    color_label: String,
    selected_ocr_text: String,
    ocr_manual_color: bool,
    ocr_text: String,
    ocr_visible: bool,
    stroke_color: RgbaColor,
    fill_color: RgbaColor,
    text_color: RgbaColor,
    stroke_hex: String,
    fill_hex: String,
    text_hex: String,
    stroke_width: f32,
    opacity_percent: f32,
    brush_size: f32,
    effect_percent: f32,
    font_size: f32,
    bold: bool,
    text_alignment: i32,
    serial_number: f32,
    selected_is_serial: bool,
    selected_tool: i32,
    ocr_available: bool,
    selected_text: String,
    has_selected_element: bool,
}

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

#[derive(Debug, Clone, Copy, PartialEq)]
struct WindowPreview {
    id: u32,
    rect: FloatRect,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResizeEdge {
    Top,
    Right,
    Bottom,
    Left,
}

impl ResizeEdge {
    fn from_mode(mode: i32) -> Option<Self> {
        match mode {
            5 => Some(Self::Top),
            6 => Some(Self::Right),
            7 => Some(Self::Bottom),
            8 => Some(Self::Left),
            _ => None,
        }
    }

    fn point(self, rect: FloatRect) -> FloatPoint {
        let center = rect.center();
        match self {
            Self::Top => FloatPoint {
                x: center.x,
                y: rect.top,
            },
            Self::Right => FloatPoint {
                x: rect.right,
                y: center.y,
            },
            Self::Bottom => FloatPoint {
                x: center.x,
                y: rect.bottom,
            },
            Self::Left => FloatPoint {
                x: rect.left,
                y: center.y,
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

struct OcrContext {
    worker: Option<OcrWorker>,
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
    _escape_timer: Timer,
    _capture_window: SharedCaptureWindow,
    _ocr_context: SharedOcrContext,
}

impl WindowsRuntime {
    pub fn start(app: &AppWindow, tray: &AppTray) -> Self {
        app.window()
            .on_close_requested(|| CloseRequestResponse::HideWindow);

        let session = Arc::new(CaptureSession {
            settings: Mutex::new(NativeSettings::load()),
            ..CaptureSession::default()
        });
        let pins = Rc::new(RefCell::new(PinCollection::default()));
        let capture_window = Rc::new(RefCell::new(None));
        let ocr_context = Rc::new(OcrContext {
            worker: OcrWorker::start()
                .map_err(|error| app.set_runtime_status(error.into()))
                .ok(),
        });

        bind_capture_ready_callback(
            app,
            Rc::clone(&capture_window),
            Arc::clone(&session),
            Rc::clone(&pins),
            Rc::clone(&ocr_context),
        );
        bind_pin_visibility_callback(app, Rc::clone(&pins));
        bind_app_callbacks(app, Arc::clone(&session));
        bind_tray_callbacks(app, tray, Arc::clone(&session));

        let hotkeys = register_global_hotkeys(app, Arc::clone(&session));
        let escape_timer =
            watch_capture_escape(Rc::downgrade(&capture_window), Arc::clone(&session));

        Self {
            _hotkeys: hotkeys,
            _escape_timer: escape_timer,
            _capture_window: capture_window,
            _ocr_context: ocr_context,
        }
    }
}

fn register_global_hotkeys(
    app: &AppWindow,
    session: SharedCaptureSession,
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
    GlobalHotKeyEvent::set_event_handler(Some(move |event: GlobalHotKeyEvent| {
        if event.state != HotKeyState::Pressed {
            return;
        }

        if event.id == screenshot_hotkey_id {
            let app_weak = app_weak.clone();
            let session = Arc::clone(&session);
            let _ = slint::invoke_from_event_loop(move || {
                request_region_capture(app_weak, session);
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

fn watch_capture_escape(capture_window: WeakCaptureWindow, session: SharedCaptureSession) -> Timer {
    let timer = Timer::default();
    let mut escape_was_down = false;
    let mut shift_was_down = false;
    let mut shift_was_used = false;
    timer.start(TimerMode::Repeated, ESCAPE_POLL_INTERVAL, move || {
        if !session.visible.load(Ordering::Acquire) {
            escape_was_down = false;
            shift_was_down = false;
            shift_was_used = false;
            return;
        }

        // SAFETY: GetAsyncKeyState reads process-independent keyboard state without mutation.
        let escape_is_down = unsafe { GetAsyncKeyState(VK_ESCAPE.0 as i32) } as u16 & 0x8000 != 0;
        if escape_is_down
            && !escape_was_down
            && let Some(slot) = capture_window.upgrade()
            && let Some(capture) = slot.borrow().as_ref()
        {
            capture.invoke_handle_native_escape();
        }
        escape_was_down = escape_is_down;

        // Modifier-only shortcuts are not represented by Slint KeyBinding. Poll Shift and only
        // cycle the color format when it was pressed and released without participating in
        // selection resizing, another shortcut, or cursor movement.
        let shift_is_down = unsafe { GetAsyncKeyState(VK_SHIFT.0 as i32) } as u16 & 0x8000 != 0;
        if shift_is_down && !shift_was_down {
            shift_was_used = false;
        }
        if shift_is_down
            && [
                VK_LBUTTON.0 as i32,
                VK_CONTROL.0 as i32,
                VK_LEFT.0 as i32,
                VK_RIGHT.0 as i32,
                VK_UP.0 as i32,
                VK_DOWN.0 as i32,
                b'W' as i32,
                b'A' as i32,
                b'S' as i32,
                b'D' as i32,
                b'C' as i32,
                b'Z' as i32,
            ]
            .into_iter()
            .any(|key| unsafe { GetAsyncKeyState(key) } as u16 & 0x8000 != 0)
        {
            shift_was_used = true;
        }
        if !shift_is_down
            && shift_was_down
            && !shift_was_used
            && let Some(slot) = capture_window.upgrade()
            && let Some(capture) = slot.borrow().as_ref()
            && !capture.get_text_input_active()
        {
            capture.invoke_color_format_cycle_requested();
        }
        shift_was_down = shift_is_down;
    });
    timer
}

fn bind_capture_callbacks(
    app: &AppWindow,
    capture: &CaptureWindow,
    capture_window: WeakCaptureWindow,
    session: SharedCaptureSession,
    pins: SharedPins,
    ocr: SharedOcrContext,
) {
    let app_weak = app.as_weak();
    let confirm_capture_window = capture_window.clone();
    let confirm_session = Arc::clone(&session);
    capture.on_selection_confirmed(move |left, top, right, bottom| {
        let result = selection_region(&confirm_session, left, top, right, bottom)
            .and_then(|region| selected_region_frame(&confirm_session, region))
            .and_then(|frame| {
                copy_region_frame_to_clipboard(&frame).map_err(|error| error.to_string())
            });

        finish_region_capture(
            app_weak.clone(),
            confirm_capture_window.clone(),
            Arc::clone(&confirm_session),
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
    let save_capture_window = capture_window.clone();
    let save_session = Arc::clone(&session);
    let save_pins = Rc::clone(&pins);
    let save_ocr = Rc::clone(&ocr);
    capture.on_selection_save_requested(move |left, top, right, bottom| {
        let selection = FloatRect {
            left,
            top,
            right,
            bottom,
        };
        let selected = selection_region(&save_session, left, top, right, bottom)
            .and_then(|region| selected_region_frame(&save_session, region));

        let selected = match selected {
            Ok(selected) => selected,
            Err(error) => {
                finish_region_capture(
                    app_weak.clone(),
                    save_capture_window.clone(),
                    Arc::clone(&save_session),
                    error,
                );
                return;
            }
        };

        let dialog_app = app_weak.clone();
        let dialog_capture_window = save_capture_window.clone();
        let dialog_session = Arc::clone(&save_session);
        let dialog_pins = Rc::clone(&save_pins);
        let dialog_ocr = Rc::clone(&save_ocr);
        suspend_region_capture(
            save_capture_window.clone(),
            Arc::clone(&save_session),
            move || {
                let Some(path) = rfd::FileDialog::new()
                    .add_filter("PNG 图片", &["png"])
                    .set_file_name("snow-shot.png")
                    .set_title("保存 Snow Shot 截图")
                    .save_file()
                else {
                    resume_region_capture(
                        dialog_app,
                        dialog_capture_window,
                        dialog_session,
                        dialog_pins,
                        dialog_ocr,
                        Some(selection),
                        "已取消保存，当前选区仍可继续处理。",
                    );
                    return;
                };

                let result =
                    save_region_frame_to_path(&selected, &path).map_err(|error| error.to_string());

                match result {
                    Ok(summary) => finish_region_capture(
                        dialog_app,
                        dialog_capture_window,
                        dialog_session,
                        format!(
                            "已保存 {}×{} 区域截图到 {}。",
                            summary.width(),
                            summary.height(),
                            path.display()
                        ),
                    ),
                    Err(error) => resume_region_capture(
                        dialog_app,
                        dialog_capture_window,
                        dialog_session,
                        dialog_pins,
                        dialog_ocr,
                        Some(selection),
                        &error,
                    ),
                }
            },
        );
    });

    let app_weak = app.as_weak();
    let pin_capture_window = capture_window.clone();
    let pin_session = Arc::clone(&session);
    let presented_pins = Rc::clone(&pins);
    capture.on_selection_pin_requested(move |left, top, right, bottom| {
        let result = selection_region(&pin_session, left, top, right, bottom)
            .and_then(|region| {
                let origin = pin_session
                    .frame
                    .lock()
                    .map_err(|_| "截图会话状态不可用。".to_string())
                    .and_then(|guard| {
                        let frame = guard.as_ref().ok_or_else(|| {
                            format!("截图会话已结束，请重新按 {SCREENSHOT_SHORTCUT}。")
                        })?;
                        let region_x = i32::try_from(region.x()).unwrap_or(i32::MAX);
                        let region_y = i32::try_from(region.y()).unwrap_or(i32::MAX);
                        Ok((
                            frame.origin_x().saturating_add(region_x),
                            frame.origin_y().saturating_add(region_y),
                        ))
                    })?;
                let pinned = selected_region_frame(&pin_session, region)?;
                Ok((origin.0, origin.1, pinned))
            })
            .and_then(|(origin_x, origin_y, pinned)| {
                create_pinned_frame(&presented_pins, pinned, origin_x, origin_y)
            });

        match result {
            Ok((width, height)) => finish_region_capture(
                app_weak.clone(),
                pin_capture_window.clone(),
                Arc::clone(&pin_session),
                format!("已创建 {width}×{height} 置顶贴图。"),
            ),
            Err(error) => set_status(&app_weak, &error),
        }
    });

    let app_weak = app.as_weak();
    let ocr_session = Arc::clone(&session);
    let ocr_context = Rc::clone(&ocr);
    let ocr_capture = capture.as_weak();
    capture.on_selection_ocr_requested(move |left, top, right, bottom| {
        let selected =
            selection_region(&ocr_session, left, top, right, bottom).and_then(|region| {
                let frame = {
                    let guard = ocr_session
                        .frame
                        .lock()
                        .map_err(|_| "截图会话状态不可用。".to_string())?;
                    let frozen = guard.as_ref().ok_or_else(|| {
                        format!("截图会话已结束，请重新按 {SCREENSHOT_SHORTCUT}。")
                    })?;
                    extract_frozen_region(frozen, region).map_err(|error| error.to_string())?
                };
                ensure_annotation_state(&ocr_session, region, 0)?;
                Ok((region, frame))
            });

        let (region, frame) = match selected {
            Ok(selected) => selected,
            Err(error) => {
                set_status(&app_weak, &error);
                return;
            }
        };
        let Some(capture) = ocr_capture.upgrade() else {
            set_status(&app_weak, "区域截图窗口已不可用。");
            return;
        };
        let Some(worker) = ocr_context.worker.as_ref() else {
            set_status(&app_weak, "OCR 后台任务未能启动。");
            return;
        };

        capture.set_ocr_busy(true);
        capture.set_ocr_status("正在准备 OCR…".into());
        let progress_capture = ocr_capture.clone();
        let progress_app = app_weak.clone();
        let completion_capture = ocr_capture.clone();
        let completion_app = app_weak.clone();
        let completion_session = Arc::clone(&ocr_session);
        if let Err(error) = worker.detect(
            frame,
            move |status| {
                let capture = progress_capture.clone();
                let app = progress_app.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(capture) = capture.upgrade() {
                        capture.set_ocr_busy(true);
                        capture.set_ocr_status(status.clone().into());
                    }
                    set_status(&app, &status);
                });
            },
            move |result| {
                let capture = completion_capture.clone();
                let app = completion_app.clone();
                let session = Arc::clone(&completion_session);
                let converted = result.map(ocr_blocks_from_result);
                let _ = slint::invoke_from_event_loop(move || match converted {
                    Ok(blocks) => {
                        let count = blocks.len();
                        let snapshot = session
                            .annotation
                            .lock()
                            .map_err(|_| "标注会话状态不可用。".to_string())
                            .and_then(|mut annotation| {
                                let annotation = annotation
                                    .as_mut()
                                    .filter(|annotation| annotation.region == region)
                                    .ok_or_else(|| "OCR 对应的选区已经变化。".to_string())?;
                                annotation.document.set_ocr_blocks(blocks);
                                Ok(annotation_snapshot(annotation))
                            });
                        match snapshot {
                            Ok(snapshot) => {
                                let status = if count == 0 {
                                    "识别完成，没有发现文字。".to_string()
                                } else {
                                    format!("识别完成，共 {count} 个文字块。")
                                };
                                if let Some(capture) = capture.upgrade() {
                                    capture.set_ocr_busy(false);
                                    capture.set_ocr_status(status.clone().into());
                                    if let Err(error) =
                                        apply_annotation_snapshot(&capture, snapshot)
                                    {
                                        set_status(&app, &error);
                                        return;
                                    }
                                }
                                set_status(&app, &status);
                            }
                            Err(error) => {
                                if let Some(capture) = capture.upgrade() {
                                    capture.set_ocr_busy(false);
                                    capture.set_ocr_status(error.clone().into());
                                }
                                set_status(&app, &error);
                            }
                        }
                    }
                    Err(error) => {
                        let status = format!("OCR 失败：{error}");
                        if let Some(capture) = capture.upgrade() {
                            capture.set_ocr_busy(false);
                            capture.set_ocr_status(status.clone().into());
                        }
                        set_status(&app, &status);
                    }
                });
            },
        ) {
            capture.set_ocr_busy(false);
            capture.set_ocr_status(error.clone().into());
            set_status(&app_weak, &error);
        }
    });

    let app_weak = app.as_weak();
    let copy_session = Arc::clone(&session);
    capture.on_ocr_copy_requested(move || {
        let text = copy_session
            .annotation
            .lock()
            .ok()
            .and_then(|annotation| {
                annotation
                    .as_ref()
                    .map(|annotation| annotation.document.ocr_plain_text())
            })
            .unwrap_or_default();
        let status = if text.trim().is_empty() {
            "当前没有可复制的 OCR 文本。".to_string()
        } else {
            match snow_shot_clipboard::write_text(text.as_str()) {
                Ok(()) => "已复制 OCR 文本到剪贴板。".to_string(),
                Err(error) => format!("复制 OCR 文本失败：{error}"),
            }
        };
        set_status(&app_weak, &status);
    });

    let app_weak = app.as_weak();
    let annotation_capture = capture.as_weak();
    let annotation_session = Arc::clone(&session);
    capture.on_annotation_tool_requested(move |tool, left, top, right, bottom| {
        let resolved_tool = if tool == -1 {
            annotation_session
                .annotation
                .lock()
                .ok()
                .and_then(|annotation| {
                    annotation
                        .as_ref()
                        .map(|annotation| annotation.previous_tool)
                })
                .unwrap_or(0)
        } else {
            tool
        };
        let result =
            selection_region(&annotation_session, left, top, right, bottom).and_then(|region| {
                ensure_annotation_state(&annotation_session, region, resolved_tool).map(Some)
            });

        match result {
            Ok(Some(snapshot)) => {
                if let Some(capture) = annotation_capture.upgrade()
                    && let Err(error) = apply_annotation_snapshot(&capture, snapshot)
                {
                    set_status(&app_weak, &error);
                }
            }
            Ok(None) => {
                if let Some(capture) = annotation_capture.upgrade() {
                    capture.set_annotation_tool(0);
                }
            }
            Err(error) => set_status(&app_weak, &error),
        }
    });

    let floating_session = Arc::clone(&session);
    capture.on_floating_position_changed(move |panel, x, y| {
        if let Ok(mut positions) = floating_session.floating_panels.lock() {
            let position = Some(FloatPoint {
                x: x.max(0.0),
                y: y.max(0.0),
            });
            match panel {
                0 => positions.toolbar = position,
                1 => positions.properties = position,
                _ => {}
            }
        }
    });

    let app_weak = app.as_weak();
    let annotation_capture = capture.as_weak();
    let annotation_session = Arc::clone(&session);
    capture.on_annotation_pointer_event(
        move |phase, normalized_x, normalized_y, preserve_aspect, centered| {
            let active_color_slot = annotation_capture
                .upgrade()
                .map(|capture| capture.get_active_color_slot())
                .unwrap_or(0);
            let frame_origin = annotation_session
                .frame
                .lock()
                .ok()
                .and_then(|frame| {
                    frame
                        .as_ref()
                        .map(|frame| (frame.origin_x(), frame.origin_y()))
                })
                .unwrap_or((0, 0));
            let result = annotation_session
                .annotation
                .lock()
                .map_err(|_| "标注会话状态不可用。".to_string())
                .and_then(|mut guard| {
                    let annotation = guard
                        .as_mut()
                        .ok_or_else(|| "请先选择一个标注工具。".to_string())?;
                    let point = Point::new(
                        normalized_x.clamp(0.0, 1.0) * annotation.document.width() as f32,
                        normalized_y.clamp(0.0, 1.0) * annotation.document.height() as f32,
                    );

                    if annotation.tool == 7 {
                        if phase == 0 {
                            annotation.color = annotation.document.sample_original(point);
                            let patch = match active_color_slot {
                                1 => {
                                    annotation.style.fill = annotation.color;
                                    StylePatch {
                                        fill: Some(annotation.color),
                                        ..StylePatch::default()
                                    }
                                }
                                2 => {
                                    annotation.style.text = annotation.color;
                                    StylePatch {
                                        text: Some(annotation.color),
                                        ..StylePatch::default()
                                    }
                                }
                                _ => {
                                    annotation.style.stroke = annotation.color;
                                    StylePatch {
                                        stroke: Some(annotation.color),
                                        ..StylePatch::default()
                                    }
                                }
                            };
                            if annotation.document.selected_ocr_id().is_some() {
                                let mut style = annotation.document.ocr_style().clone();
                                style.manual_text_color = Some(annotation.color);
                                annotation.document.set_ocr_style(style);
                            } else {
                                annotation.document.update_selected_style(&patch);
                            }
                            if let Ok(mut sample) = annotation_session.sampled_color.lock() {
                                *sample = Some(SampledColor {
                                    color: annotation.color,
                                    x: frame_origin
                                        .0
                                        .saturating_add(annotation.region.x() as i32)
                                        .saturating_add(point.x.round() as i32),
                                    y: frame_origin
                                        .1
                                        .saturating_add(annotation.region.y() as i32)
                                        .saturating_add(point.y.round() as i32),
                                });
                            }
                        }
                        return Ok(annotation_snapshot(annotation));
                    }

                    let tool = annotation_tool(annotation.tool)
                        .ok_or_else(|| "当前标注工具不可用。".to_string())?;
                    match phase {
                        0 => annotation.document.begin_with_style(
                            tool,
                            point,
                            annotation.style.clone(),
                            preserve_aspect,
                            centered,
                        ),
                        1 => annotation.document.update_with_modifiers(
                            point,
                            preserve_aspect,
                            centered,
                        ),
                        2 => {
                            annotation.document.commit_with_modifiers(
                                point,
                                preserve_aspect,
                                centered,
                            );
                        }
                        3 => annotation.document.cancel_active(),
                        _ => {}
                    }
                    Ok(annotation_snapshot(annotation))
                });

            match result {
                Ok(snapshot) => {
                    if phase == 0 {
                        let picked =
                            annotation_session
                                .annotation
                                .lock()
                                .ok()
                                .and_then(|annotation| {
                                    annotation.as_ref().and_then(|annotation| {
                                        (annotation.tool == 7).then(|| {
                                            let persist = if annotation
                                                .document
                                                .selected_ocr_id()
                                                .is_some()
                                            {
                                                PersistedAnnotationStyle::Ocr(
                                                    annotation.document.ocr_style().clone(),
                                                )
                                            } else {
                                                PersistedAnnotationStyle::Tool(
                                                    annotation.previous_tool,
                                                    annotation.style.clone(),
                                                )
                                            };
                                            (annotation.color, persist)
                                        })
                                    })
                                });
                        if let Some((color, persist)) = picked {
                            let _ = persist_annotation_style(&annotation_session, Some(persist));
                            let format = annotation_session.color_format.load(Ordering::Acquire);
                            let sample = SampledColor { color, x: 0, y: 0 };
                            let value = format_color_value(sample.color, format);
                            let _ = snow_shot_clipboard::write_text(&value);
                        }
                    }
                    if phase == 2 {
                        let next_serial =
                            annotation_session
                                .annotation
                                .lock()
                                .ok()
                                .and_then(|annotation| {
                                    annotation.as_ref().and_then(|annotation| {
                                        (annotation.tool == 4)
                                            .then(|| annotation.document.next_serial_number())
                                    })
                                });
                        if let Some(next_serial) = next_serial
                            && let Ok(mut settings) = annotation_session.settings.lock()
                        {
                            settings.set_serial_number(next_serial);
                            let _ = settings.save();
                        }
                    }
                    if let Some(capture) = annotation_capture.upgrade()
                        && let Err(error) = apply_annotation_snapshot(&capture, snapshot)
                    {
                        set_status(&app_weak, &error);
                    }
                }
                Err(error) => set_status(&app_weak, &error),
            }
        },
    );

    let app_weak = app.as_weak();
    let annotation_capture = capture.as_weak();
    let annotation_session = Arc::clone(&session);
    capture.on_annotation_undo_requested(move || {
        let result = annotation_session
            .annotation
            .lock()
            .map_err(|_| "标注会话状态不可用。".to_string())
            .and_then(|mut guard| {
                let annotation = guard
                    .as_mut()
                    .ok_or_else(|| "当前没有可撤销的标注。".to_string())?;
                annotation.document.undo();
                Ok(annotation_snapshot(annotation))
            });
        match result {
            Ok(snapshot) => {
                if let Some(capture) = annotation_capture.upgrade()
                    && let Err(error) = apply_annotation_snapshot(&capture, snapshot)
                {
                    set_status(&app_weak, &error);
                }
            }
            Err(error) => set_status(&app_weak, &error),
        }
    });

    let app_weak = app.as_weak();
    let annotation_capture = capture.as_weak();
    let annotation_session = Arc::clone(&session);
    capture.on_annotation_redo_requested(move || {
        let result = annotation_session
            .annotation
            .lock()
            .map_err(|_| "标注会话状态不可用。".to_string())
            .and_then(|mut guard| {
                let annotation = guard
                    .as_mut()
                    .ok_or_else(|| "当前没有可重做的标注。".to_string())?;
                annotation.document.redo();
                Ok(annotation_snapshot(annotation))
            });
        match result {
            Ok(snapshot) => {
                if let Some(capture) = annotation_capture.upgrade()
                    && let Err(error) = apply_annotation_snapshot(&capture, snapshot)
                {
                    set_status(&app_weak, &error);
                }
            }
            Err(error) => set_status(&app_weak, &error),
        }
    });

    let annotation_capture = capture.as_weak();
    let annotation_session = Arc::clone(&session);
    capture.on_annotation_reset_requested(move || {
        if let Ok(mut annotation) = annotation_session.annotation.lock() {
            annotation.take();
        }
        if let Some(capture) = annotation_capture.upgrade() {
            clear_annotation_ui(&capture);
        }
    });

    let app_weak = app.as_weak();
    let style_capture = capture.as_weak();
    let style_session = Arc::clone(&session);
    capture.on_annotation_color_requested(move |slot, value| {
        match update_annotation_color(&style_session, slot, value.as_str()) {
            Ok(snapshot) => {
                if let Some(capture) = style_capture.upgrade()
                    && let Err(error) = apply_annotation_snapshot(&capture, snapshot)
                {
                    set_status(&app_weak, &error);
                }
            }
            Err(error) => set_status(&app_weak, &error),
        }
    });

    let app_weak = app.as_weak();
    let alpha_capture = capture.as_weak();
    let alpha_session = Arc::clone(&session);
    capture.on_annotation_color_alpha_requested(move |slot, value| {
        match update_annotation_color_alpha(&alpha_session, slot, value) {
            Ok(snapshot) => {
                if let Some(capture) = alpha_capture.upgrade()
                    && let Err(error) = apply_annotation_snapshot(&capture, snapshot)
                {
                    set_status(&app_weak, &error);
                }
            }
            Err(error) => set_status(&app_weak, &error),
        }
    });

    let app_weak = app.as_weak();
    let style_capture = capture.as_weak();
    let style_session = Arc::clone(&session);
    capture.on_annotation_style_value_requested(move |field, value| {
        match update_annotation_style_value(&style_session, field, value) {
            Ok(snapshot) => {
                if let Some(capture) = style_capture.upgrade()
                    && let Err(error) = apply_annotation_snapshot(&capture, snapshot)
                {
                    set_status(&app_weak, &error);
                }
            }
            Err(error) => set_status(&app_weak, &error),
        }
    });

    let app_weak = app.as_weak();
    let text_capture = capture.as_weak();
    let text_session = Arc::clone(&session);
    capture.on_annotation_text_requested(move |text| {
        match update_annotation_text(&text_session, text.to_string()) {
            Ok(snapshot) => {
                if let Some(capture) = text_capture.upgrade()
                    && let Err(error) = apply_annotation_snapshot(&capture, snapshot)
                {
                    set_status(&app_weak, &error);
                }
            }
            Err(error) => set_status(&app_weak, &error),
        }
    });

    let app_weak = app.as_weak();
    let layer_capture = capture.as_weak();
    let layer_session = Arc::clone(&session);
    capture.on_annotation_layer_requested(move |command| {
        match update_annotation_layer(&layer_session, command) {
            Ok(snapshot) => {
                if let Some(capture) = layer_capture.upgrade()
                    && let Err(error) = apply_annotation_snapshot(&capture, snapshot)
                {
                    set_status(&app_weak, &error);
                }
            }
            Err(error) => set_status(&app_weak, &error),
        }
    });

    let app_weak = app.as_weak();
    let delete_capture = capture.as_weak();
    let delete_session = Arc::clone(&session);
    capture.on_annotation_delete_requested(move || {
        match delete_selected_annotation(&delete_session) {
            Ok(snapshot) => {
                if let Some(capture) = delete_capture.upgrade()
                    && let Err(error) = apply_annotation_snapshot(&capture, snapshot)
                {
                    set_status(&app_weak, &error);
                }
            }
            Err(error) => set_status(&app_weak, &error),
        }
    });

    let app_weak = app.as_weak();
    let visibility_capture = capture.as_weak();
    let visibility_session = Arc::clone(&session);
    capture.on_ocr_visibility_requested(move |visible| {
        let result = visibility_session
            .annotation
            .lock()
            .map_err(|_| "标注会话状态不可用。".to_string())
            .and_then(|mut annotation| {
                let annotation = annotation
                    .as_mut()
                    .ok_or_else(|| "当前没有 OCR 图层。".to_string())?;
                annotation.document.set_ocr_visible(visible);
                Ok((
                    annotation_snapshot(annotation),
                    annotation.document.ocr_style().clone(),
                ))
            });
        match result {
            Ok((snapshot, style)) => {
                if let Err(error) = persist_annotation_style(
                    &visibility_session,
                    Some(PersistedAnnotationStyle::Ocr(style)),
                ) {
                    set_status(&app_weak, &error);
                }
                if let Some(capture) = visibility_capture.upgrade()
                    && let Err(error) = apply_annotation_snapshot(&capture, snapshot)
                {
                    set_status(&app_weak, &error);
                }
            }
            Err(error) => set_status(&app_weak, &error),
        }
    });

    let app_weak = app.as_weak();
    let auto_color_capture = capture.as_weak();
    let auto_color_session = Arc::clone(&session);
    capture.on_ocr_auto_color_requested(move || {
        let result = auto_color_session
            .annotation
            .lock()
            .map_err(|_| "标注会话状态不可用。".to_string())
            .and_then(|mut annotation| {
                let annotation = annotation
                    .as_mut()
                    .ok_or_else(|| "当前没有 OCR 图层。".to_string())?;
                let mut style = annotation.document.ocr_style().clone();
                style.manual_text_color = None;
                annotation.document.set_ocr_style(style.clone());
                Ok((annotation_snapshot(annotation), style))
            });
        match result {
            Ok((snapshot, style)) => {
                if let Err(error) = persist_annotation_style(
                    &auto_color_session,
                    Some(PersistedAnnotationStyle::Ocr(style)),
                ) {
                    set_status(&app_weak, &error);
                }
                if let Some(capture) = auto_color_capture.upgrade()
                    && let Err(error) = apply_annotation_snapshot(&capture, snapshot)
                {
                    set_status(&app_weak, &error);
                }
            }
            Err(error) => set_status(&app_weak, &error),
        }
    });

    let app_weak = app.as_weak();
    let context_capture_window = capture_window.clone();
    let context_session = Arc::clone(&session);
    capture.on_context_copy_requested(move |force_image, left, top, right, bottom| {
        if !force_image {
            let selected_text = context_session
                .annotation
                .lock()
                .ok()
                .and_then(|annotation| {
                    annotation.as_ref().and_then(|annotation| {
                        annotation
                            .document
                            .selected_ocr_text()
                            .map(ToOwned::to_owned)
                    })
                });
            if let Some(text) = selected_text {
                let status = match snow_shot_clipboard::write_text(&text) {
                    Ok(()) => "已复制所选 OCR 文字。".to_string(),
                    Err(error) => format!("复制 OCR 文字失败：{error}"),
                };
                set_status(&app_weak, &status);
                return;
            }
        }
        let result = selection_region(&context_session, left, top, right, bottom)
            .and_then(|region| selected_region_frame(&context_session, region))
            .and_then(|frame| {
                copy_region_frame_to_clipboard(&frame).map_err(|error| error.to_string())
            });
        finish_region_capture(
            app_weak.clone(),
            context_capture_window.clone(),
            Arc::clone(&context_session),
            match result {
                Ok(summary) => format!(
                    "已复制 {}×{} 区域截图到剪贴板。",
                    summary.width(),
                    summary.height()
                ),
                Err(error) => error,
            },
        );
    });

    let app_weak = app.as_weak();
    let sample_capture = capture.as_weak();
    let sample_session = Arc::clone(&session);
    capture.on_cursor_sample_requested(move |x, y, canvas_width, canvas_height| {
        let result = magnifier_snapshot(&sample_session, x, y, canvas_width, canvas_height);
        match result {
            Ok((pixels, sample, label)) => {
                if let Ok(mut current) = sample_session.sampled_color.lock() {
                    *current = Some(sample);
                }
                if let Some(capture) = sample_capture.upgrade() {
                    let mut buffer = SharedPixelBuffer::<Rgba8Pixel>::new(17, 17);
                    buffer.make_mut_bytes().copy_from_slice(&pixels);
                    capture.set_magnifier_frame(Image::from_rgba8(buffer));
                    capture.set_magnifier_label(label.into());
                    capture.set_magnifier_visible(true);
                    capture.set_color_format(
                        sample_session.color_format.load(Ordering::Acquire) as i32
                    );
                }
            }
            Err(error) => set_status(&app_weak, &error),
        }
    });

    let format_capture = capture.as_weak();
    let format_session = Arc::clone(&session);
    capture.on_color_format_cycle_requested(move || {
        let next = (format_session.color_format.load(Ordering::Acquire) + 1) % 5;
        format_session.color_format.store(next, Ordering::Release);
        if let Some(capture) = format_capture.upgrade() {
            capture.set_color_format(next as i32);
            if let Ok(sample) = format_session.sampled_color.lock()
                && let Some(sample) = *sample
            {
                capture.set_magnifier_label(format_sample(sample, next).into());
            }
        }
    });

    let app_weak = app.as_weak();
    let copy_color_session = Arc::clone(&session);
    capture.on_color_copy_requested(move || {
        let sample = copy_color_session
            .sampled_color
            .lock()
            .ok()
            .and_then(|sample| *sample);
        let status = match sample {
            Some(sample) => {
                let format = copy_color_session.color_format.load(Ordering::Acquire);
                let value = format_color_value(sample.color, format);
                match snow_shot_clipboard::write_text(&value) {
                    Ok(()) => format!("已复制色值 {value}。"),
                    Err(error) => format!("复制色值失败：{error}"),
                }
            }
            None => "当前没有可复制的取色结果。".to_string(),
        };
        set_status(&app_weak, &status);
    });

    capture.on_cursor_nudge_requested(move |delta_x, delta_y| {
        let mut point = POINT::default();
        // SAFETY: GetCursorPos and SetCursorPos operate on the process desktop cursor.
        if unsafe { GetCursorPos(&mut point) }.is_ok() {
            let _ = unsafe {
                SetCursorPos(
                    point.x.saturating_add(delta_x),
                    point.y.saturating_add(delta_y),
                )
            };
        }
    });

    let capture_weak = capture.as_weak();
    let preview_session = Arc::clone(&session);
    let last_window_target = Cell::new(None::<u32>);
    capture.on_window_target_requested(move |x, y, canvas_width, canvas_height| {
        let preview =
            window_preview_for_pointer(&preview_session, x, y, canvas_width, canvas_height);
        if let Some(capture) = capture_weak.upgrade() {
            match preview {
                Some(preview) if last_window_target.get() != Some(preview.id) => {
                    last_window_target.set(Some(preview.id));
                    capture.invoke_apply_window_preview(
                        preview.rect.left,
                        preview.rect.top,
                        preview.rect.right,
                        preview.rect.bottom,
                    );
                }
                Some(_) => {}
                None if last_window_target.take().is_some() => {
                    capture.invoke_clear_window_preview();
                }
                None => {}
            }
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
    let cancel_capture_window = capture_window.clone();
    let cancel_session = Arc::clone(&session);
    capture.on_cancelled(move || {
        finish_region_capture(
            app_weak.clone(),
            cancel_capture_window.clone(),
            Arc::clone(&cancel_session),
            "已取消区域截图。".to_string(),
        );
    });

    let capture_weak = capture.as_weak();
    capture.window().on_close_requested(move || {
        if let Some(capture) = capture_weak.upgrade() {
            capture.invoke_cancelled();
        }
        CloseRequestResponse::KeepWindowShown
    });
}

fn bind_capture_ready_callback(
    app: &AppWindow,
    capture_window: SharedCaptureWindow,
    session: SharedCaptureSession,
    pins: SharedPins,
    ocr: SharedOcrContext,
) {
    let app_weak = app.as_weak();
    app.on_capture_ready(move || {
        let Some(app) = app_weak.upgrade() else {
            session.busy.store(false, Ordering::Release);
            clear_capture_data(&session);
            return;
        };

        match create_capture_window(
            &app,
            &capture_window,
            Arc::clone(&session),
            Rc::clone(&pins),
            Rc::clone(&ocr),
            None,
        ) {
            Ok(()) => app.set_runtime_status("正在准备区域截图窗口…".into()),
            Err(error) => {
                session.visible.store(false, Ordering::Release);
                session.busy.store(false, Ordering::Release);
                clear_capture_data(&session);
                app.set_runtime_status(error.into());
            }
        }
    });
}

fn create_capture_window(
    app: &AppWindow,
    capture_window: &SharedCaptureWindow,
    session: SharedCaptureSession,
    pins: SharedPins,
    ocr: SharedOcrContext,
    selection: Option<FloatRect>,
) -> Result<(), String> {
    capture_window.borrow_mut().take();

    let capture = CaptureWindow::new().map_err(|error| format!("无法创建区域截图窗口：{error}"))?;
    if let Ok(settings) = session.settings.lock() {
        capture.set_last_shape(settings.last_shape);
        capture.set_last_line(settings.last_line);
        capture.set_last_pen(settings.last_pen);
        capture.set_last_privacy(settings.last_privacy);
    }
    if let Ok(positions) = session.floating_panels.lock() {
        if let Some(position) = positions.toolbar {
            capture.set_toolbar_position_x(position.x);
            capture.set_toolbar_position_y(position.y);
        }
        if let Some(position) = positions.properties {
            capture.set_property_position_x(position.x);
            capture.set_property_position_y(position.y);
        }
    }
    bind_capture_callbacks(
        app,
        &capture,
        Rc::downgrade(capture_window),
        Arc::clone(&session),
        pins,
        ocr,
    );

    let (origin_x, origin_y) = {
        let frame = session
            .frame
            .lock()
            .map_err(|_| "截图会话状态不可用。".to_string())?;
        let frame = frame
            .as_ref()
            .ok_or_else(|| format!("截图会话已结束，请重新按 {SCREENSHOT_SHORTCUT}。"))?;
        prepare_frozen_frame(&capture, frame)?;
        restore_annotation_ui(&capture, &session)?;
        (frame.origin_x(), frame.origin_y())
    };
    capture
        .window()
        .set_position(PhysicalPosition::new(origin_x, origin_y));
    capture.window().set_size(PhysicalSize::new(1, 1));

    let ready_app = app.as_weak();
    let ready_capture_window = Rc::downgrade(capture_window);
    let ready_session = Arc::clone(&session);
    let mut first_frame_pending = true;
    capture
        .window()
        .set_rendering_notifier(move |state, _| {
            if !first_frame_pending || !matches!(state, RenderingState::AfterRendering) {
                return;
            }
            first_frame_pending = false;

            let ready_app = ready_app.clone();
            let ready_capture_window = ready_capture_window.clone();
            let ready_session = Arc::clone(&ready_session);
            Timer::single_shot(Duration::ZERO, move || {
                activate_capture_window(ready_app, ready_capture_window, ready_session, selection);
            });
        })
        .map_err(|error| format!("无法监听区域截图窗口首帧：{error}"))?;

    *capture_window.borrow_mut() = Some(capture);
    let show_result = capture_window
        .borrow()
        .as_ref()
        .ok_or_else(|| "区域截图窗口未能进入活动会话。".to_string())?
        .show()
        .map_err(|error| format!("无法创建区域截图窗口首帧：{error}"));
    if show_result.is_err() {
        capture_window.borrow_mut().take();
    }
    show_result
}

fn bind_pin_visibility_callback(app: &AppWindow, pins: SharedPins) {
    let app_weak = app.as_weak();
    app.on_toggle_pin_visibility_requested(move || {
        toggle_pin_visibility(&app_weak, &pins);
    });
}

fn bind_app_callbacks(app: &AppWindow, session: SharedCaptureSession) {
    let app_weak = app.as_weak();
    app.on_capture_clicked(move || {
        request_region_capture(app_weak.clone(), Arc::clone(&session));
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
        set_status(
            &app_weak,
            "选区工具栏已接入本地 PP-OCRv4；首次识别会自动安装官方模型。",
        );
    });
}

fn bind_tray_callbacks(app: &AppWindow, tray: &AppTray, session: SharedCaptureSession) {
    let app_weak = app.as_weak();
    tray.on_open_settings(move || show_settings(&app_weak));

    let app_weak = app.as_weak();
    let capture_session = Arc::clone(&session);
    tray.on_capture_clicked(move || {
        request_region_capture(app_weak.clone(), Arc::clone(&capture_session));
    });

    let app_weak = app.as_weak();
    tray.on_full_monitor_clicked(move || {
        request_full_monitor_copy(app_weak.clone(), Arc::clone(&session));
    });

    tray.on_quit_requested(|| {
        let _ = slint::quit_event_loop();
    });
}

fn request_region_capture(app_weak: slint::Weak<AppWindow>, session: SharedCaptureSession) {
    if !begin_capture(&app_weak, &session) {
        return;
    }

    let worker_app = app_weak.clone();
    let worker_session = Arc::clone(&session);
    let spawn_result = thread::Builder::new()
        .name("snowshot-region-capture".to_string())
        .spawn(move || {
            let result = freeze_monitor_under_cursor();

            match result {
                Ok(frozen) => {
                    let window_targets = list_windows(&[]).unwrap_or_default();
                    if let Ok(mut targets) = worker_session.window_targets.lock() {
                        *targets = window_targets;
                    }
                    let frame_stored = worker_session
                        .frame
                        .lock()
                        .map(|mut current| {
                            *current = Some(frozen);
                        })
                        .is_ok();
                    if !frame_stored {
                        worker_session.busy.store(false, Ordering::Release);
                        let _ = worker_app.upgrade_in_event_loop(|app| {
                            app.set_runtime_status("截图会话状态不可用。".into());
                        });
                        return;
                    }

                    let invoke_result = worker_app.upgrade_in_event_loop(move |app| {
                        app.invoke_capture_ready();
                    });

                    if invoke_result.is_err() {
                        clear_capture_data(&worker_session);
                        worker_session.busy.store(false, Ordering::Release);
                    }
                }
                Err(error) => {
                    worker_session.busy.store(false, Ordering::Release);
                    let status = error.to_string();
                    let _ = worker_app.upgrade_in_event_loop(move |app| {
                        app.set_runtime_status(status.into());
                    });
                }
            }
        });

    if let Err(error) = spawn_result {
        session.busy.store(false, Ordering::Release);
        set_status(&app_weak, &format!("无法启动截图任务：{error}"));
    }
}

fn request_full_monitor_copy(app_weak: slint::Weak<AppWindow>, session: SharedCaptureSession) {
    if !begin_capture(&app_weak, &session) {
        return;
    }

    let worker_app = app_weak.clone();
    let worker_session = Arc::clone(&session);
    let spawn_result = thread::Builder::new()
        .name("snowshot-monitor-copy".to_string())
        .spawn(move || {
            let result = capture_monitor_to_clipboard();
            worker_session.busy.store(false, Ordering::Release);

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
        session.busy.store(false, Ordering::Release);
        set_status(&app_weak, &format!("无法启动截图任务：{error}"));
    }
}

fn begin_capture(app_weak: &slint::Weak<AppWindow>, session: &CaptureSession) -> bool {
    if session
        .busy
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        set_status(app_weak, "已有截图任务正在执行。");
        return false;
    }

    session.visible.store(false, Ordering::Release);
    session.finishing.store(false, Ordering::Release);
    clear_capture_data(session);
    true
}

fn prepare_frozen_frame(capture: &CaptureWindow, frame: &FrozenMonitorFrame) -> Result<(), String> {
    let mut pixels = SharedPixelBuffer::<Rgba8Pixel>::new(frame.width(), frame.height());
    if pixels.make_mut_bytes().len() != frame.rgba().len() {
        return Err("冻结帧尺寸与像素数据不一致。".to_string());
    }
    pixels.make_mut_bytes().copy_from_slice(frame.rgba());
    capture.set_frozen_frame(Image::from_rgba8(pixels));
    capture.invoke_prepare_selection();
    capture.set_presentation_active(false);
    Ok(())
}

fn activate_capture_window(
    app_weak: slint::Weak<AppWindow>,
    capture_window: WeakCaptureWindow,
    session: SharedCaptureSession,
    selection: Option<FloatRect>,
) {
    let result = capture_window
        .upgrade()
        .ok_or_else(|| "区域截图窗口已不可用。".to_string())
        .and_then(|capture_window| {
            let capture = capture_window
                .borrow()
                .as_ref()
                .map(ComponentHandle::as_weak)
                .ok_or_else(|| "区域截图窗口未能进入活动会话。".to_string())?;
            let capture = capture
                .upgrade()
                .ok_or_else(|| "区域截图窗口已不可用。".to_string())?;
            let frame = session
                .frame
                .lock()
                .map_err(|_| "截图会话状态不可用。".to_string())?;
            let frame = frame
                .as_ref()
                .ok_or_else(|| format!("截图会话已结束，请重新按 {SCREENSHOT_SHORTCUT}。"))?;

            capture
                .window()
                .set_position(PhysicalPosition::new(frame.origin_x(), frame.origin_y()));
            capture
                .window()
                .set_size(PhysicalSize::new(frame.width(), frame.height()));
            if let Some(selection) = selection {
                capture.invoke_apply_selection(
                    selection.left,
                    selection.top,
                    selection.right,
                    selection.bottom,
                );
            }
            capture.set_presentation_active(true);
            capture.window().request_redraw();
            session.visible.store(true, Ordering::Release);
            focus_capture_window(capture.as_weak(), Arc::clone(&session));
            Ok(())
        });

    match result {
        Ok(()) => set_status(
            &app_weak,
            "区域截图中：拖动框选，可复制、保存或贴图；Esc 取消。",
        ),
        Err(error) => {
            session.visible.store(false, Ordering::Release);
            session.busy.store(false, Ordering::Release);
            clear_capture_data(&session);
            if let Some(capture_window) = capture_window.upgrade() {
                if let Some(capture) = capture_window.borrow().as_ref() {
                    let _ = capture.hide();
                }
                capture_window.borrow_mut().take();
            }
            set_status(&app_weak, &error);
        }
    }
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

fn window_preview_for_pointer(
    session: &CaptureSession,
    x: f32,
    y: f32,
    canvas_width: f32,
    canvas_height: f32,
) -> Option<WindowPreview> {
    if !x.is_finite()
        || !y.is_finite()
        || !canvas_width.is_finite()
        || !canvas_height.is_finite()
        || canvas_width <= 0.0
        || canvas_height <= 0.0
    {
        return None;
    }

    let targets = session.window_targets.lock().ok()?;
    let frame = session.frame.lock().ok()?;
    let frame = frame.as_ref()?;
    if frame.width() == 0 || frame.height() == 0 {
        return None;
    }

    let pixel_x = ((x / canvas_width).clamp(0.0, 1.0) * frame.width() as f32)
        .floor()
        .min(frame.width().saturating_sub(1) as f32) as i64
        + frame.origin_x() as i64;
    let pixel_y = ((y / canvas_height).clamp(0.0, 1.0) * frame.height() as f32)
        .floor()
        .min(frame.height().saturating_sub(1) as f32) as i64
        + frame.origin_y() as i64;

    let target = targets.iter().find(|target| {
        let rect = target.rect();
        pixel_x >= rect.min_x() as i64
            && pixel_x < rect.max_x() as i64
            && pixel_y >= rect.min_y() as i64
            && pixel_y < rect.max_y() as i64
    })?;

    let rect = normalized_window_rect(
        frame.origin_x(),
        frame.origin_y(),
        frame.width(),
        frame.height(),
        target.rect(),
    )?;

    Some(WindowPreview {
        id: target.id(),
        rect,
    })
}

fn normalized_window_rect(
    frame_x: i32,
    frame_y: i32,
    frame_width: u32,
    frame_height: u32,
    window: WindowRect,
) -> Option<FloatRect> {
    if frame_width == 0 || frame_height == 0 {
        return None;
    }

    let frame_left = frame_x as i64;
    let frame_top = frame_y as i64;
    let frame_right = frame_left + frame_width as i64;
    let frame_bottom = frame_top + frame_height as i64;
    let left = (window.min_x() as i64).max(frame_left);
    let top = (window.min_y() as i64).max(frame_top);
    let right = (window.max_x() as i64).min(frame_right);
    let bottom = (window.max_y() as i64).min(frame_bottom);
    if right <= left || bottom <= top {
        return None;
    }

    Some(FloatRect {
        left: (left - frame_left) as f32 / frame_width as f32,
        top: (top - frame_top) as f32 / frame_height as f32,
        right: (right - frame_left) as f32 / frame_width as f32,
        bottom: (bottom - frame_top) as f32 / frame_height as f32,
    })
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

fn selection_region(
    session: &CaptureSession,
    left: f32,
    top: f32,
    right: f32,
    bottom: f32,
) -> Result<PixelRect, String> {
    let frame = session
        .frame
        .lock()
        .map_err(|_| "截图会话状态不可用。".to_string())?;
    let frame = frame
        .as_ref()
        .ok_or_else(|| format!("截图会话已结束，请重新按 {SCREENSHOT_SHORTCUT}。"))?;
    normalized_region(frame, left, top, right, bottom).map_err(|error| error.to_string())
}

fn selected_region_frame(
    session: &CaptureSession,
    region: PixelRect,
) -> Result<FrozenRegionFrame, String> {
    if let Ok(annotation) = session.annotation.lock()
        && let Some(annotation) = annotation.as_ref()
        && annotation.region == region
    {
        return FrozenRegionFrame::from_rgba(
            annotation.document.width(),
            annotation.document.height(),
            annotation.document.pixels().to_vec(),
        )
        .map_err(|error| error.to_string());
    }

    let frame = session
        .frame
        .lock()
        .map_err(|_| "截图会话状态不可用。".to_string())?;
    let frame = frame
        .as_ref()
        .ok_or_else(|| format!("截图会话已结束，请重新按 {SCREENSHOT_SHORTCUT}。"))?;
    extract_frozen_region(frame, region).map_err(|error| error.to_string())
}

fn ocr_blocks_from_result(result: OcrDetectResult) -> Vec<OcrBlock> {
    let scale = result.scale_factor.max(f32::MIN_POSITIVE);
    result
        .text_blocks
        .into_iter()
        .filter_map(|block| {
            let [first, second, third, fourth] = block.box_points.as_slice() else {
                return None;
            };
            Some(OcrBlock {
                id: snow_shot_annotate::ElementId(0),
                points: [
                    Point::new(first.x as f32 / scale, first.y as f32 / scale),
                    Point::new(second.x as f32 / scale, second.y as f32 / scale),
                    Point::new(third.x as f32 / scale, third.y as f32 / scale),
                    Point::new(fourth.x as f32 / scale, fourth.y as f32 / scale),
                ],
                text: block.text,
                box_score: block.box_score,
                text_score: block.text_score,
            })
        })
        .collect()
}

fn magnifier_snapshot(
    session: &CaptureSession,
    x: f32,
    y: f32,
    canvas_width: f32,
    canvas_height: f32,
) -> Result<(Vec<u8>, SampledColor, String), String> {
    let frame = session
        .frame
        .lock()
        .map_err(|_| "截图会话状态不可用。".to_string())?;
    let frame = frame
        .as_ref()
        .ok_or_else(|| format!("截图会话已结束，请重新按 {SCREENSHOT_SHORTCUT}。"))?;
    let (center_x, center_y) = map_canvas_point_to_frame(
        x,
        y,
        canvas_width,
        canvas_height,
        frame.width(),
        frame.height(),
    )
    .ok_or_else(|| "取色画布尺寸无效。".to_string())?;
    let mut pixels = vec![0_u8; 17 * 17 * 4];
    for target_y in 0..17_u32 {
        for target_x in 0..17_u32 {
            let source_x =
                (center_x as i32 + target_x as i32 - 8).clamp(0, frame.width() as i32 - 1) as u32;
            let source_y =
                (center_y as i32 + target_y as i32 - 8).clamp(0, frame.height() as i32 - 1) as u32;
            let source_index =
                ((source_y as usize * frame.width() as usize) + source_x as usize) * 4;
            let target_index = ((target_y as usize * 17) + target_x as usize) * 4;
            pixels[target_index..target_index + 4]
                .copy_from_slice(&frame.rgba()[source_index..source_index + 4]);
        }
    }
    let center_index = ((center_y as usize * frame.width() as usize) + center_x as usize) * 4;
    let sample = SampledColor {
        color: RgbaColor::new(
            frame.rgba()[center_index],
            frame.rgba()[center_index + 1],
            frame.rgba()[center_index + 2],
            frame.rgba()[center_index + 3],
        ),
        x: frame.origin_x().saturating_add(center_x as i32),
        y: frame.origin_y().saturating_add(center_y as i32),
    };
    let format = session.color_format.load(Ordering::Acquire);
    let label = format_sample(sample, format);
    Ok((pixels, sample, label))
}

fn map_canvas_point_to_frame(
    x: f32,
    y: f32,
    canvas_width: f32,
    canvas_height: f32,
    frame_width: u32,
    frame_height: u32,
) -> Option<(u32, u32)> {
    if canvas_width <= 0.0 || canvas_height <= 0.0 || frame_width == 0 || frame_height == 0 {
        return None;
    }
    Some((
        ((x / canvas_width).clamp(0.0, 1.0) * frame_width.saturating_sub(1) as f32).round() as u32,
        ((y / canvas_height).clamp(0.0, 1.0) * frame_height.saturating_sub(1) as f32).round()
            as u32,
    ))
}

fn format_sample(sample: SampledColor, format: u8) -> String {
    let value = format_color_value(sample.color, format);
    format!("({}, {})  {value}", sample.x, sample.y)
}

fn format_color_value(color: RgbaColor, format: u8) -> String {
    match format % 5 {
        0 => color.display_hex(),
        1 => color.format_rgb(),
        2 => color.format_hsv(),
        3 => color.format_hsl(),
        _ => color.format_cmyk(),
    }
}

fn ensure_annotation_state(
    session: &CaptureSession,
    region: PixelRect,
    tool: i32,
) -> Result<AnnotationUiSnapshot, String> {
    let selected_tool = annotation_tool(tool).unwrap_or(AnnotationTool::Select);
    let (style, ocr_style, serial_number) = {
        let mut settings = session
            .settings
            .lock()
            .map_err(|_| "Native 样式设置不可用。".to_string())?;
        settings.remember_tool(tool);
        let style = settings.style(selected_tool);
        let ocr_style = settings.ocr_style.clone();
        let serial_number = settings.serial_number;
        let _ = settings.save();
        (style, ocr_style, serial_number)
    };
    if let Ok(mut annotation) = session.annotation.lock()
        && let Some(annotation) = annotation.as_mut()
        && annotation.region == region
    {
        let previous_tool = annotation.tool;
        if tool == 7 && previous_tool != 7 {
            annotation.previous_tool = previous_tool;
        } else if tool != 7 {
            annotation.previous_tool = tool;
        }
        annotation.tool = tool;
        annotation.style = style;
        annotation.color = annotation.style.stroke;
        annotation.stroke_width = annotation.style.stroke_width;
        return Ok(annotation_snapshot(annotation));
    }

    let selected = {
        let frame = session
            .frame
            .lock()
            .map_err(|_| "截图会话状态不可用。".to_string())?;
        let frame = frame
            .as_ref()
            .ok_or_else(|| format!("截图会话已结束，请重新按 {SCREENSHOT_SHORTCUT}。"))?;
        extract_frozen_region(frame, region).map_err(|error| error.to_string())?
    };
    let mut document = AnnotationDocument::new(
        selected.width(),
        selected.height(),
        selected.rgba().to_vec(),
    )
    .map_err(|error| error.to_string())?;
    document.configure_ocr_style(ocr_style);
    document.configure_next_serial_number(serial_number);
    let annotation = AnnotationState {
        region,
        document,
        tool,
        previous_tool: if tool == 7 { 0 } else { tool },
        color: style.stroke,
        stroke_width: style.stroke_width,
        style,
    };
    let snapshot = annotation_snapshot(&annotation);
    let mut current = session
        .annotation
        .lock()
        .map_err(|_| "标注会话状态不可用。".to_string())?;
    *current = Some(annotation);
    Ok(snapshot)
}

fn annotation_tool(tool: i32) -> Option<AnnotationTool> {
    match tool {
        0 => Some(AnnotationTool::Select),
        1 => Some(AnnotationTool::Pen),
        2 => Some(AnnotationTool::Line),
        3 => Some(AnnotationTool::Arrow),
        4 => Some(AnnotationTool::SerialNumber),
        5 => Some(AnnotationTool::Mosaic),
        6 => Some(AnnotationTool::Blur),
        8 => Some(AnnotationTool::Rectangle),
        9 => Some(AnnotationTool::Ellipse),
        10 => Some(AnnotationTool::Highlighter),
        11 => Some(AnnotationTool::Eraser),
        12 => Some(AnnotationTool::Diamond),
        13 => Some(AnnotationTool::Text),
        _ => None,
    }
}

const fn annotation_tool_id(tool: AnnotationTool) -> i32 {
    match tool {
        AnnotationTool::Select => 0,
        AnnotationTool::Pen => 1,
        AnnotationTool::Line => 2,
        AnnotationTool::Arrow => 3,
        AnnotationTool::SerialNumber => 4,
        AnnotationTool::Mosaic => 5,
        AnnotationTool::Blur => 6,
        AnnotationTool::Rectangle => 8,
        AnnotationTool::Ellipse => 9,
        AnnotationTool::Highlighter => 10,
        AnnotationTool::Eraser => 11,
        AnnotationTool::Diamond => 12,
        AnnotationTool::Text => 13,
    }
}

fn update_annotation_color(
    session: &CaptureSession,
    slot: i32,
    value: &str,
) -> Result<AnnotationUiSnapshot, String> {
    let color = RgbaColor::parse(value)?;
    let (snapshot, persist) = {
        let mut annotation = session
            .annotation
            .lock()
            .map_err(|_| "标注会话状态不可用。".to_string())?;
        let annotation = annotation
            .as_mut()
            .ok_or_else(|| "当前没有可编辑的标注。".to_string())?;
        if annotation.document.selected_ocr_id().is_some() {
            let mut style = annotation.document.ocr_style().clone();
            style.manual_text_color = Some(color);
            annotation.document.set_ocr_style(style.clone());
            (
                annotation_snapshot(annotation),
                Some(PersistedAnnotationStyle::Ocr(style)),
            )
        } else {
            let patch = match slot {
                0 => StylePatch {
                    stroke: Some(color),
                    ..StylePatch::default()
                },
                1 => StylePatch {
                    fill: Some(color),
                    ..StylePatch::default()
                },
                2 => StylePatch {
                    text: Some(color),
                    ..StylePatch::default()
                },
                _ => return Err("未知的颜色属性。".to_string()),
            };
            let persist = if annotation.document.selected_id().is_some() {
                annotation.document.update_selected_style(&patch);
                None
            } else {
                match slot {
                    0 => annotation.style.stroke = color,
                    1 => annotation.style.fill = color,
                    2 => annotation.style.text = color,
                    _ => {}
                }
                annotation.color = annotation.style.stroke;
                Some(PersistedAnnotationStyle::Tool(
                    annotation.tool,
                    annotation.style.clone(),
                ))
            };
            (annotation_snapshot(annotation), persist)
        }
    };
    persist_annotation_style(session, persist)?;
    Ok(snapshot)
}

fn update_annotation_color_alpha(
    session: &CaptureSession,
    slot: i32,
    alpha_percent: f32,
) -> Result<AnnotationUiSnapshot, String> {
    let mut color = {
        let annotation = session
            .annotation
            .lock()
            .map_err(|_| "标注会话状态不可用。".to_string())?;
        let annotation = annotation
            .as_ref()
            .ok_or_else(|| "当前没有可编辑的标注。".to_string())?;
        if annotation.document.selected_ocr_id().is_some() {
            annotation
                .document
                .ocr_style()
                .manual_text_color
                .unwrap_or(annotation.style.text)
        } else {
            let style = annotation
                .document
                .selected_style()
                .unwrap_or(&annotation.style);
            match slot {
                0 => style.stroke,
                1 => style.fill,
                2 => style.text,
                _ => return Err("未知的颜色属性。".to_string()),
            }
        }
    };
    color.alpha = (alpha_percent.clamp(0.0, 100.0) * 2.55).round() as u8;
    update_annotation_color(session, slot, &color.display_hex())
}

enum PersistedAnnotationStyle {
    Tool(i32, ElementStyle),
    Ocr(OcrLayerStyle),
    Serial(u32),
}

fn persist_annotation_style(
    session: &CaptureSession,
    persist: Option<PersistedAnnotationStyle>,
) -> Result<(), String> {
    let Some(persist) = persist else {
        return Ok(());
    };
    let mut settings = session
        .settings
        .lock()
        .map_err(|_| "Native 样式设置不可用。".to_string())?;
    match persist {
        PersistedAnnotationStyle::Tool(tool_id, style) => {
            if let Some(tool) = annotation_tool(tool_id) {
                settings.set_style(tool, style);
            }
        }
        PersistedAnnotationStyle::Ocr(style) => settings.set_ocr_style(style),
        PersistedAnnotationStyle::Serial(number) => settings.set_serial_number(number),
    }
    settings.save()
}

fn update_annotation_style_value(
    session: &CaptureSession,
    field: i32,
    value: f32,
) -> Result<AnnotationUiSnapshot, String> {
    let (snapshot, persist) = {
        let mut annotation = session
            .annotation
            .lock()
            .map_err(|_| "标注会话状态不可用。".to_string())?;
        let annotation = annotation
            .as_mut()
            .ok_or_else(|| "当前没有可编辑的标注。".to_string())?;
        if annotation.document.selected_ocr_id().is_some() {
            let mut style = annotation.document.ocr_style().clone();
            match field {
                1 => style.opacity = (value / 100.0).clamp(0.0, 1.0),
                3 => style.blur_strength = (value / 55.0).clamp(0.2, 3.0),
                _ => return Err("该属性不适用于 OCR 图层。".to_string()),
            }
            annotation.document.set_ocr_style(style.clone());
            (
                annotation_snapshot(annotation),
                Some(PersistedAnnotationStyle::Ocr(style)),
            )
        } else if field == 7 {
            let number = value.round().max(1.0) as u32;
            annotation.document.update_serial_number(number);
            (
                annotation_snapshot(annotation),
                Some(PersistedAnnotationStyle::Serial(number)),
            )
        } else {
            let patch = match field {
                0 => StylePatch {
                    stroke_width: Some(value),
                    ..StylePatch::default()
                },
                1 => StylePatch {
                    opacity: Some(value / 100.0),
                    ..StylePatch::default()
                },
                2 => StylePatch {
                    brush_size: Some(value),
                    ..StylePatch::default()
                },
                3 => StylePatch {
                    effect_strength: Some(value / 100.0),
                    ..StylePatch::default()
                },
                4 => StylePatch {
                    font_size: Some(value),
                    ..StylePatch::default()
                },
                5 => StylePatch {
                    bold: Some(value >= 0.5),
                    ..StylePatch::default()
                },
                6 => StylePatch {
                    alignment: Some(match value.round() as i32 {
                        1 => TextAlignment::Center,
                        2 => TextAlignment::Right,
                        _ => TextAlignment::Left,
                    }),
                    ..StylePatch::default()
                },
                _ => return Err("未知的样式属性。".to_string()),
            };
            let persist = if annotation.document.selected_id().is_some() {
                annotation.document.update_selected_style(&patch);
                None
            } else {
                match field {
                    0 => annotation.style.stroke_width = value.clamp(1.0, 64.0),
                    1 => annotation.style.opacity = (value / 100.0).clamp(0.0, 1.0),
                    2 => annotation.style.brush_size = value.clamp(2.0, 256.0),
                    3 => annotation.style.effect_strength = (value / 100.0).clamp(0.05, 1.0),
                    4 => annotation.style.font_size = value.clamp(8.0, 160.0),
                    5 => annotation.style.bold = value >= 0.5,
                    6 => {
                        annotation.style.alignment = match value.round() as i32 {
                            1 => TextAlignment::Center,
                            2 => TextAlignment::Right,
                            _ => TextAlignment::Left,
                        }
                    }
                    _ => {}
                }
                annotation.stroke_width = annotation.style.stroke_width;
                Some(PersistedAnnotationStyle::Tool(
                    annotation.tool,
                    annotation.style.clone(),
                ))
            };
            (annotation_snapshot(annotation), persist)
        }
    };
    persist_annotation_style(session, persist)?;
    Ok(snapshot)
}

fn update_annotation_text(
    session: &CaptureSession,
    text: String,
) -> Result<AnnotationUiSnapshot, String> {
    let mut annotation = session
        .annotation
        .lock()
        .map_err(|_| "标注会话状态不可用。".to_string())?;
    let annotation = annotation
        .as_mut()
        .ok_or_else(|| "当前没有文本标注。".to_string())?;
    if !annotation.document.update_selected_text(text) {
        return Err("请先选择文本标注。".to_string());
    }
    Ok(annotation_snapshot(annotation))
}

fn update_annotation_layer(
    session: &CaptureSession,
    command: i32,
) -> Result<AnnotationUiSnapshot, String> {
    let command = match command {
        0 => LayerCommand::Back,
        1 => LayerCommand::Backward,
        2 => LayerCommand::Forward,
        3 => LayerCommand::Front,
        _ => return Err("未知的图层操作。".to_string()),
    };
    let mut annotation = session
        .annotation
        .lock()
        .map_err(|_| "标注会话状态不可用。".to_string())?;
    let annotation = annotation
        .as_mut()
        .ok_or_else(|| "当前没有可调整的元素。".to_string())?;
    annotation.document.apply_layer_command(command);
    Ok(annotation_snapshot(annotation))
}

fn delete_selected_annotation(session: &CaptureSession) -> Result<AnnotationUiSnapshot, String> {
    let mut annotation = session
        .annotation
        .lock()
        .map_err(|_| "标注会话状态不可用。".to_string())?;
    let annotation = annotation
        .as_mut()
        .ok_or_else(|| "当前没有可删除的元素。".to_string())?;
    if !annotation.document.delete_selected() {
        return Err("请先选择一个标注元素。".to_string());
    }
    Ok(annotation_snapshot(annotation))
}

fn annotation_snapshot(annotation: &AnnotationState) -> AnnotationUiSnapshot {
    let selected_ocr = annotation.document.selected_ocr_id().is_some();
    let mut style = annotation
        .document
        .selected_style()
        .cloned()
        .unwrap_or_else(|| annotation.style.clone());
    if selected_ocr {
        let ocr_style = annotation.document.ocr_style();
        if let Some(color) = ocr_style.manual_text_color {
            style.text = color;
        }
        style.opacity = ocr_style.opacity;
        style.effect_strength = ocr_style.blur_strength;
    }
    AnnotationUiSnapshot {
        width: annotation.document.width(),
        height: annotation.document.height(),
        pixels: annotation.document.preview_pixels().to_vec(),
        tool: annotation.tool,
        can_undo: annotation.document.can_undo(),
        can_redo: annotation.document.can_redo(),
        color: annotation.color,
        color_label: annotation.color.display_hex(),
        selected_ocr_text: annotation
            .document
            .selected_ocr_text()
            .unwrap_or_default()
            .to_string(),
        ocr_manual_color: annotation.document.ocr_style().manual_text_color.is_some(),
        ocr_text: annotation.document.ocr_plain_text(),
        ocr_visible: annotation.document.ocr_style().visible
            && !annotation.document.ocr_blocks().is_empty(),
        stroke_color: style.stroke,
        fill_color: style.fill,
        text_color: style.text,
        stroke_hex: style.stroke.display_hex(),
        fill_hex: style.fill.display_hex(),
        text_hex: style.text.display_hex(),
        stroke_width: style.stroke_width,
        opacity_percent: style.opacity * 100.0,
        brush_size: style.brush_size,
        effect_percent: if selected_ocr {
            style.effect_strength * 55.0
        } else {
            style.effect_strength * 100.0
        },
        font_size: style.font_size,
        bold: style.bold,
        text_alignment: match style.alignment {
            TextAlignment::Left => 0,
            TextAlignment::Center => 1,
            TextAlignment::Right => 2,
        },
        serial_number: annotation
            .document
            .selected_serial_number()
            .unwrap_or_else(|| annotation.document.next_serial_number())
            as f32,
        selected_is_serial: annotation.document.selected_serial_number().is_some(),
        selected_tool: annotation
            .document
            .selected_tool()
            .map(annotation_tool_id)
            .unwrap_or(0),
        ocr_available: !annotation.document.ocr_blocks().is_empty(),
        selected_text: annotation
            .document
            .selected_text()
            .unwrap_or_default()
            .to_string(),
        has_selected_element: annotation.document.selected_id().is_some()
            || annotation.document.selected_ocr_id().is_some(),
    }
}

fn apply_annotation_snapshot(
    capture: &CaptureWindow,
    snapshot: AnnotationUiSnapshot,
) -> Result<(), String> {
    let mut pixels = SharedPixelBuffer::<Rgba8Pixel>::new(snapshot.width, snapshot.height);
    if pixels.make_mut_bytes().len() != snapshot.pixels.len() {
        return Err("标注帧尺寸与像素数据不一致。".to_string());
    }
    pixels.make_mut_bytes().copy_from_slice(&snapshot.pixels);
    capture.set_annotation_frame(Image::from_rgba8(pixels));
    capture.set_annotation_visible(true);
    capture.set_annotation_tool(snapshot.tool);
    capture.set_annotation_can_undo(snapshot.can_undo);
    capture.set_annotation_can_redo(snapshot.can_redo);
    capture.set_annotation_color(Color::from_rgb_u8(
        snapshot.color.red,
        snapshot.color.green,
        snapshot.color.blue,
    ));
    capture.set_annotation_color_label(snapshot.color_label.into());
    capture.set_ocr_selected_text(snapshot.selected_ocr_text.into());
    capture.set_ocr_manual_color(snapshot.ocr_manual_color);
    capture.set_ocr_text(snapshot.ocr_text.into());
    capture.set_ocr_visible(snapshot.ocr_visible);
    capture.set_style_stroke_color(slint_color(snapshot.stroke_color));
    capture.set_style_fill_color(slint_color(snapshot.fill_color));
    capture.set_style_text_color(slint_color(snapshot.text_color));
    capture.set_style_stroke_hex(snapshot.stroke_hex.into());
    capture.set_style_fill_hex(snapshot.fill_hex.into());
    capture.set_style_text_hex(snapshot.text_hex.into());
    capture.set_style_stroke_width(snapshot.stroke_width);
    capture.set_style_opacity(snapshot.opacity_percent);
    capture.set_style_brush_size(snapshot.brush_size);
    capture.set_style_effect_strength(snapshot.effect_percent);
    capture.set_style_font_size(snapshot.font_size);
    capture.set_style_bold(snapshot.bold);
    capture.set_style_text_alignment(snapshot.text_alignment);
    capture.set_style_serial_number(snapshot.serial_number);
    capture.set_selected_is_serial(snapshot.selected_is_serial);
    capture.set_selected_annotation_tool(snapshot.selected_tool);
    capture.set_ocr_available(snapshot.ocr_available);
    capture.set_selected_text(snapshot.selected_text.into());
    capture.set_has_selected_element(snapshot.has_selected_element);
    capture.window().request_redraw();
    Ok(())
}

fn clear_annotation_ui(capture: &CaptureWindow) {
    capture.set_annotation_visible(false);
    capture.set_annotation_tool(0);
    capture.set_annotation_can_undo(false);
    capture.set_annotation_can_redo(false);
    capture.set_annotation_color(Color::from_rgb_u8(232, 68, 68));
    capture.set_annotation_color_label("#E84444".into());
    capture.set_ocr_selected_text(String::new().into());
    capture.set_ocr_manual_color(false);
    capture.set_ocr_text(String::new().into());
    capture.set_ocr_visible(false);
    capture.set_ocr_busy(false);
    capture.set_ocr_status(String::new().into());
    capture.set_style_stroke_color(Color::from_argb_u8(255, 232, 68, 68));
    capture.set_style_fill_color(Color::from_argb_u8(28, 232, 68, 68));
    capture.set_style_text_color(Color::from_argb_u8(255, 232, 68, 68));
    capture.set_style_stroke_hex("#E84444".into());
    capture.set_style_fill_hex("#E844441C".into());
    capture.set_style_text_hex("#E84444".into());
    capture.set_style_stroke_width(6.0);
    capture.set_style_opacity(100.0);
    capture.set_style_brush_size(22.0);
    capture.set_style_effect_strength(55.0);
    capture.set_style_font_size(24.0);
    capture.set_style_bold(false);
    capture.set_style_text_alignment(0);
    capture.set_style_serial_number(1.0);
    capture.set_selected_is_serial(false);
    capture.set_selected_annotation_tool(0);
    capture.set_ocr_available(false);
    capture.set_selected_text(String::new().into());
    capture.set_has_selected_element(false);
}

fn slint_color(color: RgbaColor) -> Color {
    Color::from_argb_u8(color.alpha, color.red, color.green, color.blue)
}

fn restore_annotation_ui(capture: &CaptureWindow, session: &CaptureSession) -> Result<(), String> {
    let snapshot = session
        .annotation
        .lock()
        .map_err(|_| "标注会话状态不可用。".to_string())?
        .as_ref()
        .map(annotation_snapshot);
    match snapshot {
        Some(snapshot) => apply_annotation_snapshot(capture, snapshot),
        None => {
            clear_annotation_ui(capture);
            Ok(())
        }
    }
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

    let aspect = start.width() / start.height();
    let limits = ResizeLimits {
        bounds: Some(bounds),
        min_width: 6.0,
        min_height: 6.0,
        max_width: bounds.width(),
        max_height: bounds.height(),
    };
    if let Some(corner) = ResizeCorner::from_mode(mode) {
        let start_corner = corner.point(start);
        let effective_pointer = FloatPoint {
            x: start_corner.x + pointer.x - start_pointer.x,
            y: start_corner.y + pointer.y - start_pointer.y,
        };
        return Some(resize_rect_from_pointer(
            start,
            effective_pointer,
            corner,
            preserve_aspect,
            centered,
            aspect,
            limits,
        ));
    }

    let edge = ResizeEdge::from_mode(mode)?;
    let start_edge = edge.point(start);
    let effective_pointer = FloatPoint {
        x: start_edge.x + pointer.x - start_pointer.x,
        y: start_edge.y + pointer.y - start_pointer.y,
    };
    Some(resize_rect_from_edge(
        start,
        effective_pointer,
        edge,
        preserve_aspect,
        centered,
        aspect,
        limits,
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

fn resize_rect_from_edge(
    start: FloatRect,
    pointer: FloatPoint,
    edge: ResizeEdge,
    preserve_aspect: bool,
    centered: bool,
    aspect: f32,
    limits: ResizeLimits,
) -> FloatRect {
    let center = start.center();
    let aspect = if aspect.is_finite() && aspect > 0.0 {
        aspect
    } else {
        start.width() / start.height()
    };
    let bounds = limits.bounds;
    let centered_available_width = bounds.map_or(limits.max_width, |bounds| {
        ((center.x - bounds.left).min(bounds.right - center.x) * 2.0).max(1.0)
    });
    let centered_available_height = bounds.map_or(limits.max_height, |bounds| {
        ((center.y - bounds.top).min(bounds.bottom - center.y) * 2.0).max(1.0)
    });

    match edge {
        ResizeEdge::Left | ResizeEdge::Right => {
            let sign = if edge == ResizeEdge::Right { 1.0 } else { -1.0 };
            let fixed_x = if centered {
                center.x
            } else if sign > 0.0 {
                start.left
            } else {
                start.right
            };
            let raw_width = sign * (pointer.x - fixed_x) * if centered { 2.0 } else { 1.0 };
            let primary_available = bounds.map_or(limits.max_width, |bounds| {
                if centered {
                    centered_available_width
                } else if sign > 0.0 {
                    bounds.right - fixed_x
                } else {
                    fixed_x - bounds.left
                }
                .max(1.0)
            });
            let mut max_width = limits.max_width.min(primary_available).max(1.0);
            let mut min_width = limits.min_width;
            if preserve_aspect {
                max_width = max_width
                    .min(limits.max_height * aspect)
                    .min(centered_available_height * aspect);
                min_width = min_width.max(limits.min_height * aspect);
            }
            let min_width = min_width.min(max_width).max(1.0);
            let width = raw_width.clamp(min_width, max_width);
            let height = if preserve_aspect {
                width / aspect
            } else {
                start.height()
            };
            let (left, right) = if centered {
                (center.x - width / 2.0, center.x + width / 2.0)
            } else if sign > 0.0 {
                (fixed_x, fixed_x + width)
            } else {
                (fixed_x - width, fixed_x)
            };
            let (top, bottom) = if preserve_aspect {
                (center.y - height / 2.0, center.y + height / 2.0)
            } else {
                (start.top, start.bottom)
            };
            FloatRect {
                left,
                top,
                right,
                bottom,
            }
        }
        ResizeEdge::Top | ResizeEdge::Bottom => {
            let sign = if edge == ResizeEdge::Bottom {
                1.0
            } else {
                -1.0
            };
            let fixed_y = if centered {
                center.y
            } else if sign > 0.0 {
                start.top
            } else {
                start.bottom
            };
            let raw_height = sign * (pointer.y - fixed_y) * if centered { 2.0 } else { 1.0 };
            let primary_available = bounds.map_or(limits.max_height, |bounds| {
                if centered {
                    centered_available_height
                } else if sign > 0.0 {
                    bounds.bottom - fixed_y
                } else {
                    fixed_y - bounds.top
                }
                .max(1.0)
            });
            let mut max_height = limits.max_height.min(primary_available).max(1.0);
            let mut min_height = limits.min_height;
            if preserve_aspect {
                max_height = max_height
                    .min(limits.max_width / aspect)
                    .min(centered_available_width / aspect);
                min_height = min_height.max(limits.min_width / aspect);
            }
            let min_height = min_height.min(max_height).max(1.0);
            let height = raw_height.clamp(min_height, max_height);
            let width = if preserve_aspect {
                height * aspect
            } else {
                start.width()
            };
            let (top, bottom) = if centered {
                (center.y - height / 2.0, center.y + height / 2.0)
            } else if sign > 0.0 {
                (fixed_y, fixed_y + height)
            } else {
                (fixed_y - height, fixed_y)
            };
            let (left, right) = if preserve_aspect {
                (center.x - width / 2.0, center.x + width / 2.0)
            } else {
                (start.left, start.right)
            };
            FloatRect {
                left,
                top,
                right,
                bottom,
            }
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
    app_weak: slint::Weak<AppWindow>,
    capture_window: WeakCaptureWindow,
    session: SharedCaptureSession,
    status: String,
) {
    if session
        .finishing
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return;
    }

    session.visible.store(false, Ordering::Release);
    if let Some(capture_window) = capture_window.upgrade() {
        if let Some(capture) = capture_window.borrow().as_ref() {
            capture.set_presentation_active(false);
            let _ = capture.hide();
        }
        Timer::single_shot(Duration::ZERO, move || {
            capture_window.borrow_mut().take();
            complete_region_capture(&app_weak, &session, status);
        });
        return;
    }

    complete_region_capture(&app_weak, &session, status);
}

fn resume_region_capture(
    app_weak: slint::Weak<AppWindow>,
    capture_window: WeakCaptureWindow,
    session: SharedCaptureSession,
    pins: SharedPins,
    ocr: SharedOcrContext,
    selection: Option<FloatRect>,
    status: &str,
) {
    let show_result = capture_window
        .upgrade()
        .ok_or_else(|| "区域截图窗口已不可用。".to_string())
        .and_then(|capture_window| {
            let app = app_weak
                .upgrade()
                .ok_or_else(|| "设置窗口已不可用。".to_string())?;
            create_capture_window(
                &app,
                &capture_window,
                Arc::clone(&session),
                pins,
                ocr,
                selection,
            )
        });

    if let Err(error) = show_result {
        clear_capture_data(&session);
        session.busy.store(false, Ordering::Release);
        set_status(&app_weak, &error);
    } else {
        set_status(&app_weak, status);
    }
}

fn suspend_region_capture(
    capture_window: WeakCaptureWindow,
    session: SharedCaptureSession,
    after_retired: impl FnOnce() + 'static,
) {
    session.visible.store(false, Ordering::Release);
    if let Some(capture_window) = capture_window.upgrade() {
        if let Some(capture) = capture_window.borrow().as_ref() {
            capture.set_presentation_active(false);
            let _ = capture.hide();
        }
        Timer::single_shot(Duration::ZERO, move || {
            capture_window.borrow_mut().take();
            after_retired();
        });
    } else {
        after_retired();
    }
}

fn focus_capture_window(capture_weak: slint::Weak<CaptureWindow>, session: SharedCaptureSession) {
    Timer::single_shot(Duration::from_millis(1), move || {
        if session.visible.load(Ordering::Acquire)
            && let Some(capture) = capture_weak.upgrade()
        {
            capture.invoke_focus_selection();
            capture.window().request_redraw();
        }
    });
}

fn complete_region_capture(
    app_weak: &slint::Weak<AppWindow>,
    session: &CaptureSession,
    status: String,
) {
    clear_capture_data(session);
    session.finishing.store(false, Ordering::Release);
    session.busy.store(false, Ordering::Release);
    set_status(app_weak, &status);
}

fn clear_frame(session: &CaptureSession) {
    if let Ok(mut current) = session.frame.lock() {
        current.take();
    }
}

fn clear_window_targets(session: &CaptureSession) {
    if let Ok(mut targets) = session.window_targets.lock() {
        targets.clear();
    }
}

fn clear_capture_data(session: &CaptureSession) {
    clear_frame(session);
    clear_window_targets(session);
    if let Ok(mut annotation) = session.annotation.lock() {
        annotation.take();
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
        FloatPoint, FloatRect, OcrDetectResult, PIN_MAX_HEIGHT, PIN_MAX_WIDTH, PIN_MIN_HEIGHT,
        PIN_MIN_WIDTH, ResizeCorner, ResizeLimits, WindowRect, fitted_pin_size,
        map_canvas_point_to_frame, normalized_window_rect, ocr_blocks_from_result,
        resize_rect_from_pointer, scale_rect_around_point, transform_selection,
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
    fn window_preview_clips_to_the_current_monitor() {
        let preview = normalized_window_rect(
            -1920,
            0,
            1920,
            1080,
            WindowRect::new(-2000, -50, -1000, 500).unwrap(),
        )
        .unwrap();

        assert_rect(
            preview,
            FloatRect {
                left: 0.0,
                top: 0.0,
                right: 920.0 / 1920.0,
                bottom: 500.0 / 1080.0,
            },
        );
    }

    #[test]
    fn window_preview_rejects_windows_outside_the_current_monitor() {
        let preview = normalized_window_rect(
            0,
            0,
            1920,
            1080,
            WindowRect::new(-800, 100, -20, 900).unwrap(),
        );

        assert!(preview.is_none());
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
    fn selection_right_edge_resize_ignores_perpendicular_pointer_motion() {
        let resized = transform_selection(
            6,
            FloatRect {
                left: 10.0,
                top: 10.0,
                right: 50.0,
                bottom: 30.0,
            },
            FloatPoint { x: 50.0, y: 20.0 },
            FloatPoint { x: 80.0, y: 140.0 },
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
                bottom: 30.0,
            },
        );
    }

    #[test]
    fn control_resizes_top_edge_around_the_center() {
        let resized = transform_selection(
            5,
            FloatRect {
                left: 80.0,
                top: 80.0,
                right: 120.0,
                bottom: 100.0,
            },
            FloatPoint { x: 100.0, y: 80.0 },
            FloatPoint { x: 180.0, y: 70.0 },
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
                left: 80.0,
                top: 70.0,
                right: 120.0,
                bottom: 110.0,
            },
        );
    }

    #[test]
    fn shift_resizes_bottom_edge_with_original_aspect() {
        let resized = transform_selection(
            7,
            FloatRect {
                left: 80.0,
                top: 80.0,
                right: 120.0,
                bottom: 100.0,
            },
            FloatPoint { x: 100.0, y: 100.0 },
            FloatPoint { x: 170.0, y: 120.0 },
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
                left: 60.0,
                top: 80.0,
                right: 140.0,
                bottom: 120.0,
            },
        );
    }

    #[test]
    fn shift_and_control_resize_left_edge_with_aspect_and_center() {
        let resized = transform_selection(
            8,
            FloatRect {
                left: 80.0,
                top: 80.0,
                right: 120.0,
                bottom: 100.0,
            },
            FloatPoint { x: 80.0, y: 90.0 },
            FloatPoint { x: 60.0, y: 150.0 },
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
                left: 60.0,
                top: 70.0,
                right: 140.0,
                bottom: 110.0,
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

    #[test]
    fn ocr_box_points_are_mapped_back_from_detector_scale() {
        let result: OcrDetectResult = serde_json::from_value(serde_json::json!({
            "scale_factor": 1.5,
            "text_blocks": [{
                "box_points": [
                    {"x": 30, "y": 45},
                    {"x": 180, "y": 45},
                    {"x": 180, "y": 90},
                    {"x": 30, "y": 90}
                ],
                "box_score": 0.98,
                "angle_index": 0,
                "angle_score": 0.99,
                "text": "坐标",
                "text_score": 0.97
            }]
        }))
        .unwrap();
        let blocks = ocr_blocks_from_result(result);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].points[0].x, 20.0);
        assert_eq!(blocks[0].points[0].y, 30.0);
        assert_eq!(blocks[0].points[2].x, 120.0);
        assert_eq!(blocks[0].points[2].y, 60.0);
        assert!(blocks[0].angle_radians().abs() < f32::EPSILON);
    }

    #[test]
    fn magnifier_center_maps_to_the_same_physical_pixel_at_common_dpi_scales() {
        for scale in [1.0_f32, 1.25, 1.5, 2.0] {
            let frame_width = 2400_u32;
            let frame_height = 1600_u32;
            let canvas_width = frame_width as f32 / scale;
            let canvas_height = frame_height as f32 / scale;
            let mapped = map_canvas_point_to_frame(
                800.0 / scale,
                600.0 / scale,
                canvas_width,
                canvas_height,
                frame_width,
                frame_height,
            )
            .unwrap();
            assert_eq!(mapped, (800, 600), "scale {scale}");
        }
        assert_eq!(
            map_canvas_point_to_frame(-100.0, 9_999.0, 100.0, 100.0, 400, 300),
            Some((0, 299))
        );
    }
}
