use std::fmt;
use std::path::Path;

use snow_shot_capture::{
    ImageEncoder, PixelFormat, PixelRect, crop_rgba_pixels, encode_rgba_pixels,
};

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

#[derive(Debug)]
pub struct FrozenRegionFrame {
    width: u32,
    height: u32,
    rgba: Vec<u8>,
}

impl FrozenRegionFrame {
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
    Save(std::io::Error),
}

impl fmt::Display for CaptureWorkflowError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Capture(error) => write!(formatter, "截图失败：{error}"),
            Self::Clipboard(error) => write!(formatter, "写入剪贴板失败：{error}"),
            Self::Save(error) => write!(formatter, "保存截图失败：{error}"),
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

pub fn extract_frozen_region(
    frame: &FrozenMonitorFrame,
    region: PixelRect,
) -> Result<FrozenRegionFrame, CaptureWorkflowError> {
    let rgba = crop_rgba_pixels(frame.rgba(), frame.width(), frame.height(), region)
        .map_err(CaptureWorkflowError::Capture)?;

    Ok(FrozenRegionFrame {
        width: region.width(),
        height: region.height(),
        rgba,
    })
}

pub fn save_frozen_region_to_path(
    frame: &FrozenMonitorFrame,
    region: PixelRect,
    path: &Path,
) -> Result<CaptureSummary, CaptureWorkflowError> {
    let cropped = crop_rgba_pixels(frame.rgba(), frame.width(), frame.height(), region)
        .map_err(CaptureWorkflowError::Capture)?;
    let png = encode_rgba_pixels(&cropped, region.width(), region.height(), ImageEncoder::Png)
        .map_err(CaptureWorkflowError::Capture)?;
    std::fs::write(path, png).map_err(CaptureWorkflowError::Save)?;

    Ok(CaptureSummary {
        width: region.width(),
        height: region.height(),
    })
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn saves_selected_region_as_png() {
        let frame = FrozenMonitorFrame {
            origin_x: 0,
            origin_y: 0,
            width: 2,
            height: 2,
            rgba: vec![
                255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 255,
            ],
        };
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "snow-shot-save-test-{}-{nonce}.png",
            std::process::id()
        ));

        let summary =
            save_frozen_region_to_path(&frame, PixelRect::new(1, 0, 1, 2).unwrap(), &path).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        std::fs::remove_file(&path).unwrap();

        assert_eq!(summary.width(), 1);
        assert_eq!(summary.height(), 2);
        assert!(bytes.starts_with(b"\x89PNG\r\n\x1a\n"));
    }

    #[test]
    fn extracts_selected_region_for_pin_window() {
        let frame = FrozenMonitorFrame {
            origin_x: 0,
            origin_y: 0,
            width: 2,
            height: 2,
            rgba: vec![
                255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 255,
            ],
        };

        let region = extract_frozen_region(&frame, PixelRect::new(0, 1, 2, 1).unwrap()).unwrap();

        assert_eq!(region.width(), 2);
        assert_eq!(region.height(), 1);
        assert_eq!(region.rgba(), &[0, 0, 255, 255, 255, 255, 255, 255]);
    }
}
