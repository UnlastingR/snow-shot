use half::prelude::f16;
use rayon::iter::{IndexedParallelIterator, ParallelIterator};
use rayon::slice::{ParallelSlice, ParallelSliceMut};
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Sender, channel};
use windows::Win32::Foundation::HWND;
use windows_capture::capture::{Context, GraphicsCaptureApiError, GraphicsCaptureApiHandler};
use windows_capture::frame::Frame;
use windows_capture::graphics_capture_api::{self, InternalCaptureControl};
use windows_capture::monitor::Monitor;
use windows_capture::settings::{
    CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings, MinimumUpdateIntervalSettings,
    SecondaryWindowSettings, Settings,
};

use crate::{CaptureError, PixelFormat, PixelRect};

/// 全局标志：标记系统是否支持 DrawBorderSettings::WithoutBorder
/// 默认值为 true，当遇到 BorderConfigUnsupported 错误时会设置为 false
static SUPPORTS_WITHOUT_BORDER: AtomicBool = AtomicBool::new(true);

/// 全局标志：标记系统是否支持 HDR 图像捕获
/// 默认值为 true，当遇到 HDR 捕获错误时会设置为 false
static SUPPORT_HDR_IMAGE: AtomicBool = AtomicBool::new(true);

struct CaptureFlags {
    on_frame_arrived: Sender<(Vec<u8>, usize, usize)>,
    crop_area: Option<PixelRect>,
}

struct WindowsCaptureImage {
    capture_info: Option<CaptureFlags>,
}

impl GraphicsCaptureApiHandler for WindowsCaptureImage {
    type Flags = CaptureFlags;
    type Error = String;

    fn new(ctx: Context<Self::Flags>) -> Result<Self, Self::Error> {
        Ok(Self {
            capture_info: Some(ctx.flags),
        })
    }

    fn on_frame_arrived(
        &mut self,
        frame: &mut Frame,
        capture_control: InternalCaptureControl,
    ) -> Result<(), Self::Error> {
        capture_control.stop();

        let capture_info = match self.capture_info.take() {
            Some(capture_info) => capture_info,
            None => {
                return Err(
                    "[WindowsCaptureImage::on_frame_arrived] capture_info is None".to_string(),
                );
            }
        };

        // Rgba16F 每个像素占用 8 个字节
        let mut origin_image = frame.buffer().map_err(|error| {
            format!("[WindowsCaptureImage::on_frame_arrived] failed to access frame: {error:?}")
        })?;

        let origin_image_width = origin_image.width() as usize;
        let origin_image_height = origin_image.height() as usize;
        let origin_image_row_pitch = origin_image.row_pitch() as usize;

        let orgin_image_buffer = origin_image.as_raw_buffer();

        let (origin_x_offset, origin_y_offset, crop_width, crop_height) =
            if let Some(crop_area) = capture_info.crop_area {
                (
                    crop_area.x() as usize,
                    crop_area.y() as usize,
                    crop_area.width() as usize,
                    crop_area.height() as usize,
                )
            } else {
                (0, 0, origin_image_width, origin_image_height)
            };

        let max_x = origin_x_offset.checked_add(crop_width);
        let max_y = origin_y_offset.checked_add(crop_height);
        if crop_width == 0
            || crop_height == 0
            || max_x.is_none_or(|max_x| max_x > origin_image_width)
            || max_y.is_none_or(|max_y| max_y > origin_image_height)
        {
            return Err(
                "[WindowsCaptureImage::on_frame_arrived] crop region exceeds captured frame"
                    .to_string(),
            );
        }

        // Rgba16F 每个像素占 8 字节；使用 row_pitch 跳过源图像的行对齐填充。
        let row_bytes = crop_width.checked_mul(8).ok_or_else(|| {
            "[WindowsCaptureImage::on_frame_arrived] crop row is too large".to_string()
        })?;
        let buffer_len = row_bytes.checked_mul(crop_height).ok_or_else(|| {
            "[WindowsCaptureImage::on_frame_arrived] crop buffer is too large".to_string()
        })?;
        let mut pixels = vec![0; buffer_len];

        for (y, target_row) in pixels.chunks_exact_mut(row_bytes).enumerate() {
            let origin_start = (origin_y_offset + y)
                .checked_mul(origin_image_row_pitch)
                .and_then(|start| start.checked_add(origin_x_offset * 8))
                .ok_or_else(|| {
                    "[WindowsCaptureImage::on_frame_arrived] source row offset overflowed"
                        .to_string()
                })?;
            let origin_end = origin_start.checked_add(row_bytes).ok_or_else(|| {
                "[WindowsCaptureImage::on_frame_arrived] source row end overflowed".to_string()
            })?;
            let source_row = orgin_image_buffer
                .get(origin_start..origin_end)
                .ok_or_else(|| {
                    "[WindowsCaptureImage::on_frame_arrived] source row exceeds frame buffer"
                        .to_string()
                })?;
            target_row.copy_from_slice(source_row);
        }

        match capture_info
            .on_frame_arrived
            .send((pixels, crop_width, crop_height))
        {
            Ok(()) => Ok(()),
            Err(_) => {
                log::error!("[WindowsCaptureImage::on_frame_arrived] failed to send pixels");

                Err("[WindowsCaptureImage::on_frame_arrived] failed to send pixels".to_string())
            }
        }
    }

    fn on_closed(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }
}

/// 将线性颜色值转换为 sRGB 颜色值
#[inline]
fn linear_to_srgb_byte(linear: f32) -> u8 {
    let srgb = if linear <= 0.0031308 {
        12.92 * linear
    } else {
        1.055 * linear.powf(1.0 / 2.4) - 0.055
    };

    if srgb < 0.0 {
        0
    } else if srgb > 1.0 {
        255
    } else {
        (srgb * 255.0) as u8
    }
}

#[inline]
fn half_from_le_bytes(bytes: &[u8]) -> f32 {
    f16::from_bits(u16::from_le_bytes([bytes[0], bytes[1]])).to_f32()
}

fn convert_rgba16f_to_8bit(
    rgba16f_image: &[u8],
    pixel_format: PixelFormat,
    hdr_scale: f32,
) -> Result<Vec<u8>, CaptureError> {
    if !rgba16f_image.len().is_multiple_of(8) {
        return Err(CaptureError::Backend(format!(
            "RGBA16F buffer length {} is not divisible by 8",
            rgba16f_image.len()
        )));
    }

    let pixel_count = rgba16f_image.len() / 8;
    let channel_count = match pixel_format {
        PixelFormat::Rgb8 => 3,
        PixelFormat::Rgba8 => 4,
    };
    let output_len = pixel_count
        .checked_mul(channel_count)
        .ok_or_else(|| CaptureError::Backend("HDR output buffer is too large".to_string()))?;
    let mut output = vec![0; output_len];

    match pixel_format {
        PixelFormat::Rgb8 => output
            .par_chunks_exact_mut(3)
            .zip(rgba16f_image.par_chunks_exact(8))
            .for_each(|(target, source)| {
                target[0] = linear_to_srgb_byte(half_from_le_bytes(&source[0..2]) * hdr_scale);
                target[1] = linear_to_srgb_byte(half_from_le_bytes(&source[2..4]) * hdr_scale);
                target[2] = linear_to_srgb_byte(half_from_le_bytes(&source[4..6]) * hdr_scale);
            }),
        PixelFormat::Rgba8 => output
            .par_chunks_exact_mut(4)
            .zip(rgba16f_image.par_chunks_exact(8))
            .for_each(|(target, source)| {
                target[0] = linear_to_srgb_byte(half_from_le_bytes(&source[0..2]) * hdr_scale);
                target[1] = linear_to_srgb_byte(half_from_le_bytes(&source[2..4]) * hdr_scale);
                target[2] = linear_to_srgb_byte(half_from_le_bytes(&source[4..6]) * hdr_scale);
                target[3] = (half_from_le_bytes(&source[6..8]).clamp(0.0, 1.0) * 255.0) as u8;
            }),
    }

    Ok(output)
}

/// 处理捕获的图像数据
fn process_captured_image(
    receiver: std::sync::mpsc::Receiver<(Vec<u8>, usize, usize)>,
    sdr_white_level: u32,
    pixel_format: PixelFormat,
) -> Result<image::DynamicImage, CaptureError> {
    let (rgba16f_image, image_width, image_height) = match receiver.recv() {
        Ok(image) => image,
        Err(e) => {
            return Err(CaptureError::Backend(format!(
                "[snowshot_capture::windows::hdr] failed to receive image: {:?}",
                e
            )));
        }
    };

    if sdr_white_level == 0 {
        return Err(CaptureError::Backend(
            "HDR monitor reported an SDR white level of zero".to_string(),
        ));
    }
    let expected_len = image_width
        .checked_mul(image_height)
        .and_then(|pixel_count| pixel_count.checked_mul(8))
        .ok_or_else(|| CaptureError::Backend("HDR frame dimensions are too large".to_string()))?;
    if rgba16f_image.len() != expected_len {
        return Err(CaptureError::Backend(format!(
            "RGBA16F buffer length {} does not match {}x{} frame",
            rgba16f_image.len(),
            image_width,
            image_height
        )));
    }

    let hdr_scale = 1000.0 / (sdr_white_level as f32);
    let image_pixels = convert_rgba16f_to_8bit(&rgba16f_image, pixel_format, hdr_scale)?;
    let image_width = image_width as u32;
    let image_height = image_height as u32;

    match pixel_format {
        PixelFormat::Rgb8 => image::RgbImage::from_raw(image_width, image_height, image_pixels)
            .map(image::DynamicImage::ImageRgb8)
            .ok_or_else(|| CaptureError::Backend("failed to create RGB8 HDR image".to_string())),
        PixelFormat::Rgba8 => image::RgbaImage::from_raw(image_width, image_height, image_pixels)
            .map(image::DynamicImage::ImageRgba8)
            .ok_or_else(|| CaptureError::Backend("failed to create RGBA8 HDR image".to_string())),
    }
}

pub fn capture_hdr_image(
    monitor: &xcap::Monitor,
    sdr_white_level: u32,
    window: Option<HWND>,
    crop_area: Option<PixelRect>,
    pixel_format: PixelFormat,
) -> Result<image::DynamicImage, CaptureError> {
    // 检查系统是否支持 HDR 图像捕获
    if !SUPPORT_HDR_IMAGE.load(Ordering::Relaxed) {
        return Err(CaptureError::Backend(
            "[snowshot_capture::windows::hdr] HDR image capture is not supported on this system"
                .to_string(),
        ));
    }

    let (sender, receiver) = channel();

    // 根据全局标志选择边框设置
    let draw_border_setting = if SUPPORTS_WITHOUT_BORDER.load(Ordering::Relaxed) {
        DrawBorderSettings::WithoutBorder
    } else {
        DrawBorderSettings::Default
    };

    let monitor_handle = monitor.id().map_err(|error| {
        CaptureError::Backend(format!("failed to get monitor handle: {error:?}"))
    })? as *mut c_void;
    let capture_monitor = Monitor::from_raw_hmonitor(monitor_handle);
    let window = window.map(|window| windows_capture::window::Window::from_raw_hwnd(window.0));

    let start_result: Result<(), GraphicsCaptureApiError<String>> = match window {
        Some(window) => {
            let settings = Settings::new(
                window,
                CursorCaptureSettings::WithoutCursor,
                draw_border_setting,
                SecondaryWindowSettings::Default,
                MinimumUpdateIntervalSettings::Default,
                DirtyRegionSettings::Default,
                windows_capture::settings::ColorFormat::Rgba16F,
                CaptureFlags {
                    on_frame_arrived: sender,
                    crop_area,
                },
            );

            WindowsCaptureImage::start(settings)
        }
        None => {
            let settings = Settings::new(
                capture_monitor,
                CursorCaptureSettings::WithoutCursor,
                draw_border_setting,
                SecondaryWindowSettings::Default,
                MinimumUpdateIntervalSettings::Default,
                DirtyRegionSettings::Default,
                windows_capture::settings::ColorFormat::Rgba16F,
                CaptureFlags {
                    on_frame_arrived: sender,
                    crop_area,
                },
            );

            WindowsCaptureImage::start(settings)
        }
    };

    // 尝试启动捕获器

    match start_result {
        Ok(_capturer) => {
            // 启动成功，处理捕获的图像
            process_captured_image(receiver, sdr_white_level, pixel_format)
        }
        Err(e) => match e {
            GraphicsCaptureApiError::GraphicsCaptureApiError(
                graphics_capture_api::Error::BorderConfigUnsupported,
            ) => {
                log::warn!(
                    "[snowshot_capture::windows::hdr] BorderConfigUnsupported detected, falling back to Default border setting"
                );

                // 标记系统不支持 WithoutBorder，后续请求将直接使用 Default
                SUPPORTS_WITHOUT_BORDER.store(false, Ordering::Relaxed);

                // 使用 Default 设置重试
                let (retry_sender, retry_receiver) = channel();

                let start_result: Result<(), GraphicsCaptureApiError<String>> = match window {
                    Some(window) => {
                        let settings = Settings::new(
                            window,
                            CursorCaptureSettings::WithoutCursor,
                            DrawBorderSettings::Default,
                            SecondaryWindowSettings::Default,
                            MinimumUpdateIntervalSettings::Default,
                            DirtyRegionSettings::Default,
                            windows_capture::settings::ColorFormat::Rgba16F,
                            CaptureFlags {
                                on_frame_arrived: retry_sender,
                                crop_area,
                            },
                        );

                        WindowsCaptureImage::start(settings)
                    }
                    None => {
                        let settings = Settings::new(
                            capture_monitor,
                            CursorCaptureSettings::WithoutCursor,
                            DrawBorderSettings::Default,
                            SecondaryWindowSettings::Default,
                            MinimumUpdateIntervalSettings::Default,
                            DirtyRegionSettings::Default,
                            windows_capture::settings::ColorFormat::Rgba16F,
                            CaptureFlags {
                                on_frame_arrived: retry_sender,
                                crop_area,
                            },
                        );

                        WindowsCaptureImage::start(settings)
                    }
                };

                // 重试启动捕获器
                match start_result {
                    Ok(_capturer) => {
                        // 重试成功，处理捕获的图像
                        process_captured_image(retry_receiver, sdr_white_level, pixel_format)
                    }
                    Err(retry_e) => {
                        // 重试失败，标记系统不支持 HDR 图像捕获
                        SUPPORT_HDR_IMAGE.store(false, Ordering::Relaxed);

                        log::error!(
                            "[snowshot_capture::windows::hdr] HDR image capture failed after retry, marking as unsupported: {:?}",
                            retry_e
                        );

                        Err(CaptureError::Backend(format!(
                            "[snowshot_capture::windows::hdr] failed to start capturer after retry: {:?}",
                            retry_e
                        )))
                    }
                }
            }
            _ => {
                // 标记系统不支持 HDR 图像捕获，后续请求将直接返回错误
                SUPPORT_HDR_IMAGE.store(false, Ordering::Relaxed);

                log::error!(
                    "[snowshot_capture::windows::hdr] HDR image capture failed, marking as unsupported: {:?}",
                    e
                );

                Err(CaptureError::Backend(format!(
                    "[snowshot_capture::windows::hdr] failed to start capturer: {:?}",
                    e
                )))
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rgba16f_pixel(red: f32, green: f32, blue: f32, alpha: f32) -> Vec<u8> {
        [red, green, blue, alpha]
            .into_iter()
            .flat_map(|value| f16::from_f32(value).to_bits().to_le_bytes())
            .collect()
    }

    #[test]
    fn converts_rgba16f_black_with_opaque_alpha() {
        let input = rgba16f_pixel(0.0, 0.0, 0.0, 1.0);
        let output = convert_rgba16f_to_8bit(&input, PixelFormat::Rgba8, 1.0).unwrap();

        assert_eq!(output, vec![0, 0, 0, 255]);
    }

    #[test]
    fn applies_hdr_scale_before_srgb_conversion() {
        let input = rgba16f_pixel(0.25, 0.0, 0.0, 1.0);
        let output = convert_rgba16f_to_8bit(&input, PixelFormat::Rgb8, 4.0).unwrap();

        assert!(output[0] >= 254);
        assert_eq!(&output[1..], &[0, 0]);
    }

    #[test]
    fn rejects_incomplete_rgba16f_pixels() {
        let error = convert_rgba16f_to_8bit(&[0; 7], PixelFormat::Rgb8, 1.0).unwrap_err();

        assert!(error.to_string().contains("not divisible by 8"));
    }
}
