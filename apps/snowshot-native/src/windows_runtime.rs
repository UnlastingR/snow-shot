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

use crate::capture_workflow::{
    CaptureWorkflowError, FrozenMonitorFrame, FrozenRegionFrame, capture_monitor_to_clipboard,
    copy_frozen_region_to_clipboard, extract_frozen_region, freeze_monitor_under_cursor,
    save_frozen_region_to_path,
};
use crate::{AppTray, AppWindow, CaptureWindow, PinWindow};

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
    pub fn start(
        app: &AppWindow,
        tray: &AppTray,
        capture: &CaptureWindow,
        pin: &PinWindow,
    ) -> Self {
        app.window()
            .on_close_requested(|| CloseRequestResponse::HideWindow);

        let busy = Arc::new(AtomicBool::new(false));
        let frame = Arc::new(Mutex::new(None));

        bind_pin_callbacks(pin);
        bind_capture_callbacks(app, capture, pin, Arc::clone(&busy), Arc::clone(&frame));
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
        format!("{SCREENSHOT_SHORTCUT} 已启用：拖动框选，可复制、保存或贴图；Esc 取消。").into(),
    );

    Some(HotkeyRegistration { manager, hotkey })
}

fn bind_capture_callbacks(
    app: &AppWindow,
    capture: &CaptureWindow,
    pin: &PinWindow,
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
    let pin_weak = pin.as_weak();
    let pin_busy = Arc::clone(&busy);
    let pin_frame = Arc::clone(&frame);
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
                let pin = pin_weak
                    .upgrade()
                    .ok_or_else(|| "贴图窗口已不可用。".to_string())?;
                present_pinned_frame(&pin, pinned, origin_x, origin_y)
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

fn bind_pin_callbacks(pin: &PinWindow) {
    let pin_weak = pin.as_weak();
    pin.on_move_requested(move |delta_x, delta_y| {
        let Some(pin) = pin_weak.upgrade() else {
            return;
        };
        let position = pin.window().position();
        let scale = pin.window().scale_factor();
        let delta_x = (delta_x * scale).round() as i32;
        let delta_y = (delta_y * scale).round() as i32;
        pin.window().set_position(PhysicalPosition::new(
            position.x.saturating_add(delta_x),
            position.y.saturating_add(delta_y),
        ));
    });

    let pin_weak = pin.as_weak();
    pin.on_close_clicked(move || hide_pin(&pin_weak));

    let pin_weak = pin.as_weak();
    pin.window().on_close_requested(move || {
        hide_pin(&pin_weak);
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

fn present_pinned_frame(
    pin: &PinWindow,
    frame: FrozenRegionFrame,
    origin_x: i32,
    origin_y: i32,
) -> Result<(u32, u32), String> {
    let mut pixels = SharedPixelBuffer::<Rgba8Pixel>::new(frame.width(), frame.height());
    if pixels.make_mut_bytes().len() != frame.rgba().len() {
        return Err("贴图尺寸与像素数据不一致。".to_string());
    }
    pixels.make_mut_bytes().copy_from_slice(frame.rgba());

    let (window_width, window_height) = fitted_pin_size(frame.width(), frame.height());
    pin.set_pinned_frame(Image::from_rgba8(pixels));
    pin.window()
        .set_position(PhysicalPosition::new(origin_x, origin_y));
    pin.window()
        .set_size(PhysicalSize::new(window_width, window_height));
    pin.show()
        .map_err(|error| format!("无法显示贴图窗口：{error}"))?;
    pin.invoke_focus_pin();

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

fn hide_pin(pin_weak: &slint::Weak<PinWindow>) {
    if let Some(pin) = pin_weak.upgrade() {
        let _ = pin.hide();
        pin.set_pinned_frame(Image::default());
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
    use super::fitted_pin_size;

    #[test]
    fn pin_window_caps_large_regions_without_changing_aspect_ratio() {
        assert_eq!(fitted_pin_size(1920, 1080), (960, 540));
    }

    #[test]
    fn pin_window_keeps_close_control_reachable_for_small_regions() {
        assert_eq!(fitted_pin_size(20, 10), (128, 64));
    }
}
