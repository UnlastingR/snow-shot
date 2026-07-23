use global_hotkey::hotkey::{Code, HotKey, Modifiers};
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};
use slint::{
    BackendSelector, ComponentHandle, Image, PhysicalPosition, PhysicalSize, Rgba8Pixel,
    SharedPixelBuffer,
};
use windows::Win32::Foundation::{POINT, RECT};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromPoint,
};
use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;

slint::slint! {
    export component FlickerLabWindow inherits Window {
        in property <image> test-frame;
        in property <string> case-label;
        in property <string> escape-action;
        in property <bool> show-masks;
        callback quit-requested();
        callback reset-selection();

        private property <length> anchor-x: 0px;
        private property <length> anchor-y: 0px;
        private property <length> cursor-x: 0px;
        private property <length> cursor-y: 0px;
        private property <bool> selecting: false;
        private property <bool> has-selection: false;
        private property <length> selection-left: min(root.anchor-x, root.cursor-x);
        private property <length> selection-top: min(root.anchor-y, root.cursor-y);
        private property <length> selection-right: max(root.anchor-x, root.cursor-x);
        private property <length> selection-bottom: max(root.anchor-y, root.cursor-y);
        private property <length> selection-width: root.selection-right - root.selection-left;
        private property <length> selection-height: root.selection-bottom - root.selection-top;
        private property <bool> selection-visible: root.selecting || root.has-selection;

        title: root.case-label;
        no-frame: true;
        always-on-top: true;
        background: #17243a;
        forward-focus: keyboard-focus;

        reset-selection => {
            root.anchor-x = 0px;
            root.anchor-y = 0px;
            root.cursor-x = 0px;
            root.cursor-y = 0px;
            root.selecting = false;
            root.has-selection = false;
        }

        Image {
            width: root.width;
            height: root.height;
            source: root.test-frame;
            image-fit: fill;
        }

        input := TouchArea {
            mouse-cursor: crosshair;

            pointer-event(event) => {
                if (event.kind == PointerEventKind.down && event.button == PointerEventButton.left) {
                    root.anchor-x = max(0px, min(root.width, self.mouse-x));
                    root.anchor-y = max(0px, min(root.height, self.mouse-y));
                    root.cursor-x = root.anchor-x;
                    root.cursor-y = root.anchor-y;
                    root.selecting = true;
                    root.has-selection = false;
                    keyboard-focus.focus();
                }

                if (event.kind == PointerEventKind.up && root.selecting) {
                    root.cursor-x = max(0px, min(root.width, self.mouse-x));
                    root.cursor-y = max(0px, min(root.height, self.mouse-y));
                    root.selecting = false;
                    root.has-selection = root.selection-width >= 3px
                        && root.selection-height >= 3px;
                }

                if (event.kind == PointerEventKind.cancel) {
                    root.selecting = false;
                }
            }

            moved => {
                if (root.selecting) {
                    root.cursor-x = max(0px, min(root.width, self.mouse-x));
                    root.cursor-y = max(0px, min(root.height, self.mouse-y));
                }
            }
        }

        if root.show-masks && !root.selection-visible : Rectangle {
            background: #00000070;
        }

        if root.show-masks && root.selection-visible : Rectangle {
            x: 0px;
            y: 0px;
            width: root.width;
            height: root.selection-top;
            background: #00000070;
        }

        if root.show-masks && root.selection-visible : Rectangle {
            x: 0px;
            y: root.selection-bottom;
            width: root.width;
            height: max(0px, root.height - root.selection-bottom);
            background: #00000070;
        }

        if root.show-masks && root.selection-visible : Rectangle {
            x: 0px;
            y: root.selection-top;
            width: root.selection-left;
            height: root.selection-height;
            background: #00000070;
        }

        if root.show-masks && root.selection-visible : Rectangle {
            x: root.selection-right;
            y: root.selection-top;
            width: max(0px, root.width - root.selection-right);
            height: root.selection-height;
            background: #00000070;
        }

        if root.selection-visible : Rectangle {
            x: root.selection-left;
            y: root.selection-top;
            width: root.selection-width;
            height: root.selection-height;
            background: transparent;
            border-width: 2px;
            border-color: #45a7ff;
        }

        Rectangle {
            x: 18px;
            y: 18px;
            width: 660px;
            height: 42px;
            border-radius: 7px;
            background: #101724dd;

            Text {
                text: root.case-label
                    + "  |  Esc " + root.escape-action
                    + " · Alt+F12 重开 · Alt+Shift+F12 结束";
                color: white;
                font-size: 16px;
                horizontal-alignment: center;
                vertical-alignment: center;
            }
        }

        keyboard-focus := FocusScope {
            KeyBinding {
                keys: @keys(Escape);
                activated => { root.quit-requested(); }
            }
        }
    }
}

pub enum Renderer {
    FemtoVg,
    Software,
}

struct LabHotkeys {
    _manager: GlobalHotKeyManager,
    _hotkeys: [HotKey; 2],
}

pub fn run_slint_case(
    renderer: Renderer,
    case_label: &str,
    show_masks: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let renderer_name = match renderer {
        Renderer::FemtoVg => "femtovg",
        Renderer::Software => "software",
    };
    BackendSelector::new()
        .backend_name("winit".into())
        .renderer_name(renderer_name.into())
        .select()?;

    let monitor = monitor_rect()?;
    let width = u32::try_from(monitor.right - monitor.left)?;
    let height = u32::try_from(monitor.bottom - monitor.top)?;
    let app = FlickerLabWindow::new()?;
    app.set_case_label(case_label.into());
    app.set_escape_action("隐藏".into());
    app.set_show_masks(show_masks);
    app.set_test_frame(Image::from_rgba8(test_pattern(width, height)));
    position_slint_window(&app, monitor);
    app.invoke_reset_selection();

    let app_weak = app.as_weak();
    app.on_quit_requested(move || {
        if let Some(app) = app_weak.upgrade() {
            let _ = app.hide();
        }
    });
    let _hotkeys = register_slint_hotkeys(&app)?;

    app.show()?;
    slint::run_event_loop_until_quit()?;
    GlobalHotKeyEvent::set_event_handler(None::<fn(GlobalHotKeyEvent)>);
    Ok(())
}

fn register_slint_hotkeys(
    app: &FlickerLabWindow,
) -> Result<LabHotkeys, Box<dyn std::error::Error>> {
    let show_hotkey = HotKey::new(Some(Modifiers::ALT), Code::F12);
    let quit_hotkey = HotKey::new(Some(Modifiers::ALT | Modifiers::SHIFT), Code::F12);
    let manager = GlobalHotKeyManager::new()?;
    manager.register(show_hotkey)?;
    manager.register(quit_hotkey)?;

    let show_hotkey_id = show_hotkey.id();
    let quit_hotkey_id = quit_hotkey.id();
    let app_weak = app.as_weak();
    GlobalHotKeyEvent::set_event_handler(Some(move |event: GlobalHotKeyEvent| {
        if event.state != HotKeyState::Pressed {
            return;
        }

        if event.id == show_hotkey_id {
            let app_weak = app_weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(app) = app_weak.upgrade() {
                    app.invoke_reset_selection();
                    if let Ok(monitor) = monitor_rect() {
                        position_slint_window(&app, monitor);
                    }
                    let _ = app.show();
                    app.window().request_redraw();
                }
            });
        } else if event.id == quit_hotkey_id {
            let _ = slint::invoke_from_event_loop(|| {
                let _ = slint::quit_event_loop();
            });
        }
    }));

    Ok(LabHotkeys {
        _manager: manager,
        _hotkeys: [show_hotkey, quit_hotkey],
    })
}

fn position_slint_window(app: &FlickerLabWindow, monitor: RECT) {
    let width = u32::try_from(monitor.right - monitor.left).unwrap_or(1);
    let height = u32::try_from(monitor.bottom - monitor.top).unwrap_or(1);
    app.window()
        .set_position(PhysicalPosition::new(monitor.left, monitor.top));
    app.window().set_size(PhysicalSize::new(width, height));
}

pub fn monitor_rect() -> Result<RECT, Box<dyn std::error::Error>> {
    let mut cursor = POINT::default();
    // SAFETY: cursor points to writable stack memory.
    unsafe { GetCursorPos(&mut cursor) }?;
    // SAFETY: the cursor point is valid and the fallback always returns a monitor.
    let monitor = unsafe { MonitorFromPoint(cursor, MONITOR_DEFAULTTONEAREST) };
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    // SAFETY: monitor is live and info has the required size field.
    unsafe { GetMonitorInfoW(monitor, &mut info) }.ok()?;
    Ok(info.rcMonitor)
}

fn test_pattern(width: u32, height: u32) -> SharedPixelBuffer<Rgba8Pixel> {
    let mut pixels = SharedPixelBuffer::<Rgba8Pixel>::new(width, height);
    let bytes = pixels.make_mut_bytes();
    for y in 0..height {
        for x in 0..width {
            let index = ((y * width + x) * 4) as usize;
            let grid = x % 96 < 2 || y % 96 < 2;
            bytes[index] = if grid {
                232
            } else {
                28 + ((x / 16) % 96) as u8
            };
            bytes[index + 1] = if grid {
                238
            } else {
                44 + ((y / 16) % 80) as u8
            };
            bytes[index + 2] = if grid {
                248
            } else {
                92 + (((x + y) / 32) % 96) as u8
            };
            bytes[index + 3] = 255;
        }
    }
    pixels
}
