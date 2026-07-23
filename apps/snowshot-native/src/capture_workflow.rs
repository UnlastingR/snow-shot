use std::fmt;

use snow_shot_capture::PixelFormat;

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

pub fn capture_monitor_to_clipboard() -> Result<CaptureSummary, CaptureWorkflowError> {
    let image = snow_shot_capture::windows::capture_monitor_under_cursor(PixelFormat::Rgba8)
        .map_err(CaptureWorkflowError::Capture)?;
    let rgba = image.to_rgba8();
    let summary = CaptureSummary {
        width: rgba.width(),
        height: rgba.height(),
    };

    snow_shot_clipboard::write_rgba_image(rgba.as_raw(), summary.width, summary.height)
        .map_err(CaptureWorkflowError::Clipboard)?;

    Ok(summary)
}
