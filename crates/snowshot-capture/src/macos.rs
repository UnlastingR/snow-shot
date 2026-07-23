use image::DynamicImage;

use crate::{CaptureError, PixelFormat, PixelRect, dynamic_image_from_bgra};

const DESKPAD_DISPLAY_NAME: &str = "DeskPad Display";

pub fn capture_monitor(
    monitor: &xcap::Monitor,
    region: Option<PixelRect>,
    excluded_window_id: Option<u32>,
    pixel_format: PixelFormat,
) -> Result<DynamicImage, CaptureError> {
    ensure_screen_capture_permission()?;

    if monitor.name().unwrap_or_default() == DESKPAD_DISPLAY_NAME {
        return Ok(placeholder_image(pixel_format));
    }

    let monitor_id = monitor
        .id()
        .map_err(|error| CaptureError::Backend(format!("failed to get monitor id: {error:?}")))?;
    let capture_area = match region {
        Some(region) => scap::capturer::Area {
            origin: scap::capturer::Point {
                x: region.x() as f64,
                y: region.y() as f64,
            },
            size: scap::capturer::Size {
                width: region.width() as f64,
                height: region.height() as f64,
            },
        },
        None => scap::capturer::Area {
            origin: scap::capturer::Point { x: 0.0, y: 0.0 },
            size: scap::capturer::Size {
                width: monitor.width().map_err(|error| {
                    CaptureError::Backend(format!("failed to get monitor width: {error:?}"))
                })? as f64,
                height: monitor.height().map_err(|error| {
                    CaptureError::Backend(format!("failed to get monitor height: {error:?}"))
                })? as f64,
            },
        },
    };
    let excluded_targets = excluded_window_id.map(|window_id| {
        vec![scap::Target::Window(scap::Window {
            id: window_id,
            title: "Snow Shot - Draw".to_string(),
            raw_handle: window_id,
        })]
    });
    let options = scap::capturer::Options {
        fps: 1,
        target: Some(scap::Target::Display(scap::Display {
            id: monitor_id,
            title: String::new(),
            raw_handle: core_graphics_helmer_fork::display::CGDisplay::new(monitor_id),
        })),
        show_cursor: false,
        show_highlight: true,
        excluded_targets,
        output_type: scap::frame::FrameType::BGRAFrame,
        output_resolution: scap::capturer::Resolution::Captured,
        crop_area: Some(capture_area),
        ..Default::default()
    };

    let mut capturer = scap::capturer::Capturer::build(options)
        .map_err(|error| CaptureError::Backend(format!("failed to build capturer: {error:?}")))?;
    capturer.start_capture();
    let frame_result = capturer.get_next_frame();
    capturer.stop_capture();
    let frame = frame_result
        .map_err(|error| CaptureError::Backend(format!("failed to capture frame: {error:?}")))?;

    let frame = match frame {
        scap::frame::Frame::BGRA(frame) => frame,
        _ => {
            return Err(CaptureError::Backend(
                "scap returned an unexpected frame type".to_string(),
            ));
        }
    };

    let width = u32::try_from(frame.width)
        .map_err(|_| CaptureError::Backend(format!("invalid frame width: {}", frame.width)))?;
    let height = u32::try_from(frame.height)
        .map_err(|_| CaptureError::Backend(format!("invalid frame height: {}", frame.height)))?;

    dynamic_image_from_bgra(&frame.data, width, height, pixel_format)
}

pub fn capture_focused_window(
    pixel_format: PixelFormat,
) -> Result<Option<DynamicImage>, CaptureError> {
    ensure_screen_capture_permission()?;

    let windows = xcap::Window::all()
        .map_err(|error| CaptureError::Backend(format!("failed to list windows: {error:?}")))?;
    let window = windows.iter().find(|window| {
        window.is_focused().unwrap_or(false)
            && window.y().unwrap_or(0) != 0
            && !window.title().unwrap_or_default().starts_with("Item-")
    });

    window
        .map(|window| capture_window(window, pixel_format))
        .transpose()
}

pub fn capture_window(
    window: &xcap::Window,
    pixel_format: PixelFormat,
) -> Result<DynamicImage, CaptureError> {
    ensure_screen_capture_permission()?;

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

fn ensure_screen_capture_permission() -> Result<(), CaptureError> {
    if scap::has_permission() {
        return Ok(());
    }

    let request_succeeded = scap::request_permission();
    Err(CaptureError::Backend(if request_succeeded {
        "screen capture permission was requested; restart the application to apply it".to_string()
    } else {
        "screen capture permission request was rejected".to_string()
    }))
}

fn placeholder_image(pixel_format: PixelFormat) -> DynamicImage {
    match pixel_format {
        PixelFormat::Rgb8 => DynamicImage::ImageRgb8(image::RgbImage::new(1, 1)),
        PixelFormat::Rgba8 => DynamicImage::ImageRgba8(image::RgbaImage::new(1, 1)),
    }
}
