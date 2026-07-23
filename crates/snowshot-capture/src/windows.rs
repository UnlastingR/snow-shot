use image::DynamicImage;

use crate::{CaptureError, PixelFormat, PixelRect};

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
