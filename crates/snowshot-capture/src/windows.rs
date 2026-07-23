use image::DynamicImage;
use windows::Win32::Foundation::POINT;
use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;

use crate::{CaptureError, PixelFormat, PixelRect};

pub mod hdr;
pub mod monitor_hdr_info;

pub fn capture_monitor(
    monitor: &xcap::Monitor,
    region: Option<PixelRect>,
    pixel_format: PixelFormat,
) -> Result<DynamicImage, CaptureError> {
    match (region, pixel_format) {
        (Some(region), PixelFormat::Rgb8) => monitor
            .capture_region_rgb(region.x(), region.y(), region.width(), region.height())
            .map(DynamicImage::ImageRgb8)
            .map_err(|error| CaptureError::Backend(format!("monitor RGB region: {error:?}"))),
        (Some(region), PixelFormat::Rgba8) => monitor
            .capture_region(region.x(), region.y(), region.width(), region.height())
            .map(DynamicImage::ImageRgba8)
            .map_err(|error| CaptureError::Backend(format!("monitor RGBA region: {error:?}"))),
        (None, PixelFormat::Rgb8) => monitor
            .capture_image_rgb()
            .map(DynamicImage::ImageRgb8)
            .map_err(|error| CaptureError::Backend(format!("monitor RGB image: {error:?}"))),
        (None, PixelFormat::Rgba8) => monitor
            .capture_image()
            .map(DynamicImage::ImageRgba8)
            .map_err(|error| CaptureError::Backend(format!("monitor RGBA image: {error:?}"))),
    }
}

pub fn capture_monitor_under_cursor(
    pixel_format: PixelFormat,
) -> Result<DynamicImage, CaptureError> {
    let mut cursor = POINT::default();
    unsafe { GetCursorPos(&mut cursor) }
        .map_err(|error| CaptureError::Backend(format!("read cursor position: {error}")))?;

    let monitor = match xcap::Monitor::from_point(cursor.x, cursor.y) {
        Ok(monitor) => monitor,
        Err(point_error) => xcap::Monitor::all()
            .map_err(|error| {
                CaptureError::Backend(format!(
                    "find monitor at ({}, {}): {point_error}; enumerate monitors: {error}",
                    cursor.x, cursor.y
                ))
            })?
            .into_iter()
            .next()
            .ok_or_else(|| CaptureError::Backend("no monitor is available".to_string()))?,
    };

    capture_monitor(&monitor, None, pixel_format)
}

pub fn capture_window(
    window: &xcap::Window,
    pixel_format: PixelFormat,
) -> Result<DynamicImage, CaptureError> {
    let image = window
        .capture_image()
        .map_err(|error| CaptureError::Backend(format!("window RGBA image: {error:?}")))?;

    match pixel_format {
        PixelFormat::Rgb8 => Ok(DynamicImage::ImageRgb8(
            DynamicImage::ImageRgba8(image).to_rgb8(),
        )),
        PixelFormat::Rgba8 => Ok(DynamicImage::ImageRgba8(image)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "requires an interactive Windows desktop"]
    fn captures_monitor_under_cursor_on_interactive_desktop() {
        let image = capture_monitor_under_cursor(PixelFormat::Rgba8).unwrap();

        assert!(image.width() > 0);
        assert!(image.height() > 0);
    }
}
