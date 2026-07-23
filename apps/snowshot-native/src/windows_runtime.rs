use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use global_hotkey::hotkey::{Code, HotKey, Modifiers};
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};
use slint::{
    CloseRequestResponse, ComponentHandle, Image, PhysicalPosition, PhysicalSize, Rgba8Pixel,
    SharedPixelBuffer,
};
use snow_shot_capture::PixelRect;

use crate::capture_workflow::{
    CaptureWorkflowError, FrozenMonitorFrame, capture_monitor_to_clipboard,
    copy_frozen_region_to_clipboard, freeze_monitor_under_cursor,
};
use crate::{AppTray, AppWindow, CaptureWindow};

const SCREENSHOT_SHORTCUT: &str = "Alt+F12";
type SharedFrame = Arc<Mutex<Option<FrozenMonitorFrame>>>;

struct HotkeyRegistration {
    manager: GlobalHotKeyManager,
    hotkey: HotKey,
}

impl Drop for HotkeyRegistration {
    fn drop(&mut self) {
        let _ = self.manager.unregister(self.hotkey);
    }
}

pub struct WindowsRuntime {
    _hotkey: Option<HotkeyRegistration>,
}

impl WindowsRuntime {
    pub fn start(app: &AppWindow, tray: &AppTray, capture: &CaptureWindow) -> Self {
        app.window()
            .on_close_requested(|| CloseRequestResponse::HideWindow);

        let busy = Arc::new(AtomicBool::new(false));
        let frame = Arc::new(Mutex::new(None));

        bind_capture_callbacks(app, capture, Arc::clone(&busy), Arc::clone(&frame));
        bind_app_callbacks(app, capture, Arc::clone(&busy), Arc::clone(&frame));
        bind_tray_callbacks(app, tray, capture, Arc::clone(&busy), Arc::clone(&frame));

        let hotkey = register_screenshot_hotkey(app, capture, busy, frame);

        Self { _hotkey: hotkey }
    }
}

fn register_screenshot_hotkey(
    app: &AppWindow,
    capture: &CaptureWindow,
    busy: Arc<AtomicBool>,
    frame: SharedFrame,
) -> Option<HotkeyRegistration> {
    let hotkey = HotKey::new(Some(Modifiers::ALT), Code::F12);
    let manager = match GlobalHotKeyManager::new() {
        Ok(manager) => manager,
        Err(error) => {
            app.set_runtime_status(
                format!("{SCREENSHOT_SHORTCUT} 注册失败：{error}。仍可使用界面或托盘截图。").into(),
            );
            return None;
        }
    };

    if let Err(error) = manager.register(hotkey) {
        app.set_runtime_status(
            format!("{SCREENSHOT_SHORTCUT} 注册失败：{error}。仍可使用界面或托盘截图。").into(),
        );
        return None;
    }

    let hotkey_id = hotkey.id();
    let app_weak = app.as_weak();
    let capture_weak = capture.as_weak();
    GlobalHotKeyEvent::set_event_handler(Some(move |event: GlobalHotKeyEvent| {
        if event.id == hotkey_id && event.state == HotKeyState::Pressed {
            let app_weak = app_weak.clone();
            let capture_weak = capture_weak.clone();
            let busy = Arc::clone(&busy);
            let frame = Arc::clone(&frame);
            let _ = slint::invoke_from_event_loop(move || {
                request_region_capture(app_weak, capture_weak, busy, frame);
            });
        }
    }));
    app.set_runtime_status(
        format!("{SCREENSHOT_SHORTCUT} 已启用：拖动框选，Enter 复制，Esc 取消。").into(),
    );

    Some(HotkeyRegistration { manager, hotkey })
}

fn bind_capture_callbacks(
    app: &AppWindow,
    capture: &CaptureWindow,
    busy: Arc<AtomicBool>,
    frame: SharedFrame,
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

    if let Some(app) = app_weak.upgrade() {
        app.set_runtime_status("正在冻结鼠标所在显示器…".into());
        let _ = app.hide();
    }

    let worker_app = app_weak.clone();
    let worker_busy = Arc::clone(&busy);
    let worker_frame = Arc::clone(&frame);
    let spawn_result = thread::Builder::new()
        .name("snowshot-region-capture".to_string())
        .spawn(move || {
            thread::sleep(Duration::from_millis(140));
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
                                        "区域截图中：拖动框选，Enter 复制，Esc 取消。".into(),
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
                                let _ = app.show();
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
                        let _ = app.show();
                    });
                }
            }
        });

    if let Err(error) = spawn_result {
        busy.store(false, Ordering::Release);
        set_status(&app_weak, &format!("无法启动截图任务：{error}"));
        show_settings(&app_weak);
    }
}

fn request_full_monitor_copy(app_weak: slint::Weak<AppWindow>, busy: Arc<AtomicBool>) {
    if !begin_capture(&app_weak, &busy) {
        return;
    }

    if let Some(app) = app_weak.upgrade() {
        app.set_runtime_status("正在截取鼠标所在显示器并复制…".into());
        let _ = app.hide();
    }

    let worker_app = app_weak.clone();
    let worker_busy = Arc::clone(&busy);
    let spawn_result = thread::Builder::new()
        .name("snowshot-monitor-copy".to_string())
        .spawn(move || {
            thread::sleep(Duration::from_millis(140));
            let result = capture_monitor_to_clipboard();
            worker_busy.store(false, Ordering::Release);

            let (status, show_on_error) = match result {
                Ok(summary) => (
                    format!(
                        "已复制 {}×{} 显示器截图到剪贴板。",
                        summary.width(),
                        summary.height()
                    ),
                    false,
                ),
                Err(error) => (error.to_string(), true),
            };
            let _ = worker_app.upgrade_in_event_loop(move |app| {
                app.set_runtime_status(status.into());
                if show_on_error {
                    let _ = app.show();
                }
            });
        });

    if let Err(error) = spawn_result {
        busy.store(false, Ordering::Release);
        set_status(&app_weak, &format!("无法启动截图任务：{error}"));
        show_settings(&app_weak);
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
    capture
        .show()
        .map_err(|error| format!("无法显示区域截图窗口：{error}"))?;
    capture.invoke_prepare_selection();
    capture.window().request_redraw();

    Ok(())
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

fn clear_frame(frame: &SharedFrame) {
    if let Ok(mut current) = frame.lock() {
        current.take();
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
