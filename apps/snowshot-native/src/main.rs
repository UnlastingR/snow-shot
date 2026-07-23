#![cfg_attr(
    all(target_os = "windows", not(debug_assertions)),
    windows_subsystem = "windows"
)]

#[cfg(target_os = "windows")]
mod capture_workflow;
#[cfg(target_os = "windows")]
mod windows_runtime;

slint::include_modules!();

#[cfg(target_os = "windows")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use slint::ComponentHandle;

    let app = AppWindow::new()?;
    let tray = AppTray::new()?;
    let capture = CaptureWindow::new()?;
    let _runtime = windows_runtime::WindowsRuntime::start(&app, &tray, &capture);

    app.show()?;
    tray.show()?;
    slint::run_event_loop()?;

    Ok(())
}

#[cfg(not(target_os = "windows"))]
fn main() -> Result<(), slint::PlatformError> {
    AppWindow::new()?.run()
}
