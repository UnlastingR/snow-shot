#![cfg_attr(
    all(target_os = "windows", not(debug_assertions)),
    windows_subsystem = "windows"
)]

#[cfg(target_os = "windows")]
mod capture_workflow;
#[cfg(target_os = "windows")]
mod native_settings;
#[cfg(target_os = "windows")]
mod ocr_workflow;
#[cfg(target_os = "windows")]
mod resize_geometry;
#[cfg(target_os = "windows")]
mod windows_pin;
#[cfg(target_os = "windows")]
mod windows_runtime;
#[cfg(target_os = "windows")]
mod windows_scroll;

slint::include_modules!();

#[cfg(target_os = "windows")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use slint::ComponentHandle;

    let app = AppWindow::new()?;
    let tray = AppTray::new()?;
    let _runtime = windows_runtime::WindowsRuntime::start(&app, &tray);

    app.show()?;
    tray.show()?;
    slint::run_event_loop()?;

    Ok(())
}

#[cfg(not(target_os = "windows"))]
fn main() -> Result<(), slint::PlatformError> {
    AppWindow::new()?.run()
}
