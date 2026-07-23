use std::fmt;

use snow_shot_capture::{PixelFormat, PixelRect, crop_rgba_pixels};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CaptureSummary {
    width: u32,
    height: u32,
}

impl CaptureSummary {
    pub fn width(self) -> u32 {
        self.width
    }

    pub fn height(self) -> u32 {
        self.height
    }
}

#[derive(Debug)]
pub struct FrozenMonitorFrame {
    origin_x: i32,
    origin_y: i32,
    width: u32,
    height: u32,
    rgba: Vec<u8>,
}

impl FrozenMonitorFrame {
    pub fn origin_x(&self) -> i32 {
        self.origin_x
    }

    pub fn origin_y(&self) -> i32 {
        self.origin_y
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn rgba(&self) -> &[u8] {
        &self.rgba
    }
}

#[derive(Debug)]
pub enum CaptureWorkflowError {
    Capture(snow_shot_capture::CaptureError),
    Clipboard(snow_shot_clipboard::ClipboardError),
}

impl fmt::Display for CaptureWorkflowError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Capture(error) => write!(formatter, "截图失败：{error}"),
            Self::Clipboard(error) => write!(formatter, "写入剪贴板失败：{error}"),
        }
    }
}

impl std::error::Error for CaptureWorkflowError {}

pub fn freeze_monitor_under_cursor() -> Result<FrozenMonitorFrame, CaptureWorkflowError> {
    let capture =
        snow_shot_capture::windows::capture_monitor_under_cursor_with_position(PixelFormat::Rgba8)
            .map_err(CaptureWorkflowError::Capture)?;
    let origin_x = capture.origin_x();
    let origin_y = capture.origin_y();
    let rgba = capture.into_image().to_rgba8();

    Ok(FrozenMonitorFrame {
        origin_x,
        origin_y,
        width: rgba.width(),
        height: rgba.height(),
        rgba: rgba.into_raw(),
    })
}

pub fn copy_frozen_monitor_to_clipboard(
    frame: &FrozenMonitorFrame,
) -> Result<CaptureSummary, CaptureWorkflowError> {
    write_to_clipboard(frame.rgba(), frame.width(), frame.height())
}

pub fn copy_frozen_region_to_clipboard(
    frame: &FrozenMonitorFrame,
    region: PixelRect,
) -> Result<CaptureSummary, CaptureWorkflowError> {
    let cropped = crop_rgba_pixels(frame.rgba(), frame.width(), frame.height(), region)
        .map_err(CaptureWorkflowError::Capture)?;

    write_to_clipboard(&cropped, region.width(), region.height())
}

pub fn capture_monitor_to_clipboard() -> Result<CaptureSummary, CaptureWorkflowError> {
    let frame = freeze_monitor_under_cursor()?;
    copy_frozen_monitor_to_clipboard(&frame)
}

fn write_to_clipboard(
    rgba: &[u8],
    width: u32,
    height: u32,
) -> Result<CaptureSummary, CaptureWorkflowError> {
    snow_shot_clipboard::write_rgba_image(rgba, width, height)
        .map_err(CaptureWorkflowError::Clipboard)?;

    Ok(CaptureSummary { width, height })
}
