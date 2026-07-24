use image::DynamicImage;
use windows::Win32::Foundation::POINT;
use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;

use crate::{CaptureError, PixelFormat, PixelRect};

pub mod hdr;
pub mod monitor_hdr_info;

#[derive(Debug)]
pub struct PositionedMonitorCapture {
    origin_x: i32,
    origin_y: i32,
    image: DynamicImage,
}

impl PositionedMonitorCapture {
    pub fn origin_x(&self) -> i32 {
        self.origin_x
    }

    pub fn origin_y(&self) -> i32 {
        self.origin_y
    }

    pub fn image(&self) -> &DynamicImage {
        &self.image
    }

    pub fn into_image(self) -> DynamicImage {
        self.image
    }
}

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
    capture_monitor_under_cursor_with_position(pixel_format)
        .map(PositionedMonitorCapture::into_image)
}

pub fn capture_monitor_under_cursor_with_position(
    pixel_format: PixelFormat,
) -> Result<PositionedMonitorCapture, CaptureError> {
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

    let origin_x = monitor
        .x()
        .map_err(|error| CaptureError::Backend(format!("read monitor x position: {error}")))?;
    let origin_y = monitor
        .y()
        .map_err(|error| CaptureError::Backend(format!("read monitor y position: {error}")))?;
    let image = capture_monitor(&monitor, None, pixel_format)?;

    Ok(PositionedMonitorCapture {
        origin_x,
        origin_y,
        image,
    })
}

pub fn capture_monitor_region_at_origin(
    origin_x: i32,
    origin_y: i32,
    region: PixelRect,
    pixel_format: PixelFormat,
) -> Result<DynamicImage, CaptureError> {
    let sample_x = origin_x
        .saturating_add(i32::try_from(region.x()).unwrap_or(i32::MAX))
        .saturating_add(i32::try_from(region.width() / 2).unwrap_or(i32::MAX));
    let sample_y = origin_y
        .saturating_add(i32::try_from(region.y()).unwrap_or(i32::MAX))
        .saturating_add(i32::try_from(region.height() / 2).unwrap_or(i32::MAX));
    let monitor = xcap::Monitor::from_point(sample_x, sample_y).map_err(|error| {
        CaptureError::Backend(format!(
            "find monitor for region at ({sample_x}, {sample_y}): {error}"
        ))
    })?;
    let actual_x = monitor
        .x()
        .map_err(|error| CaptureError::Backend(format!("read monitor x position: {error}")))?;
    let actual_y = monitor
        .y()
        .map_err(|error| CaptureError::Backend(format!("read monitor y position: {error}")))?;
    if actual_x != origin_x || actual_y != origin_y {
        return Err(CaptureError::Backend(format!(
            "monitor origin changed from ({origin_x}, {origin_y}) to ({actual_x}, {actual_y})"
        )));
    }

    capture_monitor(&monitor, Some(region), pixel_format)
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
        let capture = capture_monitor_under_cursor_with_position(PixelFormat::Rgba8).unwrap();
        let image = capture.image();

        assert!(image.width() > 0);
        assert!(image.height() > 0);
    }
}
