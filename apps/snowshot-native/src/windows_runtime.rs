use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use global_hotkey::hotkey::{Code, HotKey};
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};
use slint::{CloseRequestResponse, ComponentHandle};

use crate::capture_workflow::capture_monitor_to_clipboard;
use crate::{AppTray, AppWindow};

const SCREENSHOT_SHORTCUT: &str = "F1";

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
    pub fn start(app: &AppWindow, tray: &AppTray) -> Self {
        app.window()
            .on_close_requested(|| CloseRequestResponse::HideWindow);

        let busy = Arc::new(AtomicBool::new(false));
        bind_app_callbacks(app, Arc::clone(&busy));
        bind_tray_callbacks(app, tray, Arc::clone(&busy));

        let hotkey = register_screenshot_hotkey(app, busy);

        Self { _hotkey: hotkey }
    }
}

fn register_screenshot_hotkey(
    app: &AppWindow,
    busy: Arc<AtomicBool>,
) -> Option<HotkeyRegistration> {
    let hotkey = HotKey::new(None, Code::F1);
    let manager = match GlobalHotKeyManager::new() {
        Ok(manager) => manager,
        Err(error) => {
            app.set_runtime_status(
                format!("F1 注册失败：{error}。仍可使用界面或托盘截图。").into(),
            );
            return None;
        }
    };

    if let Err(error) = manager.register(hotkey) {
        app.set_runtime_status(format!("F1 注册失败：{error}。仍可使用界面或托盘截图。").into());
        return None;
    }

    let hotkey_id = hotkey.id();
    let app_weak = app.as_weak();
    GlobalHotKeyEvent::set_event_handler(Some(move |event: GlobalHotKeyEvent| {
        if event.id == hotkey_id && event.state == HotKeyState::Pressed {
            let app_weak = app_weak.clone();
            let busy = Arc::clone(&busy);
            let _ = slint::invoke_from_event_loop(move || request_capture(app_weak, busy));
        }
    }));
    app.set_runtime_status(
        format!("{SCREENSHOT_SHORTCUT} 已启用：截取鼠标所在显示器并复制到剪贴板。").into(),
    );

    Some(HotkeyRegistration { manager, hotkey })
}

fn bind_app_callbacks(app: &AppWindow, busy: Arc<AtomicBool>) {
    let app_weak = app.as_weak();
    app.on_capture_clicked(move || {
        request_capture(app_weak.clone(), Arc::clone(&busy));
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

fn bind_tray_callbacks(app: &AppWindow, tray: &AppTray, busy: Arc<AtomicBool>) {
    let app_weak = app.as_weak();
    tray.on_open_settings(move || show_settings(&app_weak));

    let app_weak = app.as_weak();
    tray.on_capture_clicked(move || {
        request_capture(app_weak.clone(), Arc::clone(&busy));
    });

    tray.on_quit_requested(|| {
        let _ = slint::quit_event_loop();
    });
}

fn request_capture(app_weak: slint::Weak<AppWindow>, busy: Arc<AtomicBool>) {
    if busy
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        set_status(&app_weak, "已有截图任务正在执行。");
        return;
    }

    if let Some(app) = app_weak.upgrade() {
        app.set_runtime_status("正在隐藏设置窗口并截取鼠标所在显示器…".into());
        let _ = app.hide();
    }

    let worker_app = app_weak.clone();
    let worker_busy = Arc::clone(&busy);
    let spawn_result = thread::Builder::new()
        .name("snowshot-capture".to_string())
        .spawn(move || {
            thread::sleep(Duration::from_millis(140));
            let result = capture_monitor_to_clipboard();
            worker_busy.store(false, Ordering::Release);

            let status = match result {
                Ok(summary) => format!(
                    "已复制 {}×{} 截图到剪贴板。按 F1 可再次截图。",
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
        show_settings(&app_weak);
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
