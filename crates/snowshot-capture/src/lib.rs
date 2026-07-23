use std::fmt;

use image::DynamicImage;
use image::codecs::avif::AvifEncoder;
use image::codecs::jpeg::JpegEncoder;
use image::codecs::png::{CompressionType, FilterType, PngEncoder};
use image::codecs::webp::WebPEncoder;
use rayon::iter::{IndexedParallelIterator, ParallelIterator};
use rayon::slice::{ParallelSlice, ParallelSliceMut};

#[cfg(target_os = "macos")]
pub mod macos;
#[cfg(target_os = "windows")]
pub mod windows;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageEncoder {
    Webp,
    Png,
    Avif,
    Jpeg,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    Rgb8,
    Rgba8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PixelRect {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
}

impl PixelRect {
    pub fn new(x: u32, y: u32, width: u32, height: u32) -> Result<Self, CaptureError> {
        if width == 0 || height == 0 {
            return Err(CaptureError::InvalidRegion);
        }

        Ok(Self {
            x,
            y,
            width,
            height,
        })
    }

    pub fn from_bounds(
        min_x: i32,
        min_y: i32,
        max_x: i32,
        max_y: i32,
    ) -> Result<Self, CaptureError> {
        if min_x < 0 || min_y < 0 || max_x <= min_x || max_y <= min_y {
            return Err(CaptureError::InvalidRegion);
        }

        let width = i64::from(max_x) - i64::from(min_x);
        let height = i64::from(max_y) - i64::from(min_y);

        Self::new(min_x as u32, min_y as u32, width as u32, height as u32)
    }

    pub fn x(self) -> u32 {
        self.x
    }

    pub fn y(self) -> u32 {
        self.y
    }

    pub fn width(self) -> u32 {
        self.width
    }

    pub fn height(self) -> u32 {
        self.height
    }
}

#[derive(Debug)]
pub enum CaptureError {
    Backend(String),
    Encode(image::ImageError),
    InvalidRegion,
    RegionOutOfBounds {
        image_width: u32,
        image_height: u32,
        region: PixelRect,
    },
    ExpectedRgb8 {
        actual: image::ColorType,
    },
}

impl fmt::Display for CaptureError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Backend(error) => write!(formatter, "capture backend failed: {error}"),
            Self::Encode(error) => write!(formatter, "failed to encode image: {error}"),
            Self::InvalidRegion => write!(formatter, "capture region is empty or invalid"),
            Self::RegionOutOfBounds {
                image_width,
                image_height,
                region,
            } => write!(
                formatter,
                "capture region ({}, {}, {}x{}) exceeds image bounds {}x{}",
                region.x(),
                region.y(),
                region.width(),
                region.height(),
                image_width,
                image_height
            ),
            Self::ExpectedRgb8 { actual } => {
                write!(formatter, "expected an RGB8 image, received {actual:?}")
            }
        }
    }
}

impl std::error::Error for CaptureError {}

impl From<image::ImageError> for CaptureError {
    fn from(error: image::ImageError) -> Self {
        Self::Encode(error)
    }
}

pub fn encode_image(image: &DynamicImage, encoder: ImageEncoder) -> Result<Vec<u8>, CaptureError> {
    let mut buffer = Vec::with_capacity(image.as_bytes().len() / 8);

    match encoder {
        ImageEncoder::Jpeg => {
            image.write_with_encoder(JpegEncoder::new_with_quality(&mut buffer, 80))?;
        }
        ImageEncoder::Webp => {
            image.write_with_encoder(WebPEncoder::new_lossless(&mut buffer))?;
        }
        ImageEncoder::Png => {
            image.write_with_encoder(PngEncoder::new_with_quality(
                &mut buffer,
                CompressionType::Fast,
                FilterType::Paeth,
            ))?;
        }
        ImageEncoder::Avif => {
            image.write_with_encoder(AvifEncoder::new_with_speed_quality(&mut buffer, 10, 80))?;
        }
    }

    Ok(buffer)
}

pub fn crop_rgb_image(
    image: &DynamicImage,
    region: PixelRect,
) -> Result<DynamicImage, CaptureError> {
    let rgb_image = image.as_rgb8().ok_or_else(|| CaptureError::ExpectedRgb8 {
        actual: image.color(),
    })?;

    let max_x = region
        .x()
        .checked_add(region.width())
        .ok_or(CaptureError::InvalidRegion)?;
    let max_y = region
        .y()
        .checked_add(region.height())
        .ok_or(CaptureError::InvalidRegion)?;

    if max_x > image.width() || max_y > image.height() {
        return Err(CaptureError::RegionOutOfBounds {
            image_width: image.width(),
            image_height: image.height(),
            region,
        });
    }

    Ok(DynamicImage::ImageRgb8(
        image::imageops::crop_imm(
            rgb_image,
            region.x(),
            region.y(),
            region.width(),
            region.height(),
        )
        .to_image(),
    ))
}

pub fn crop_rgba_pixels(
    pixels: &[u8],
    image_width: u32,
    image_height: u32,
    region: PixelRect,
) -> Result<Vec<u8>, CaptureError> {
    let expected_len = image_width
        .checked_mul(image_height)
        .and_then(|pixels| pixels.checked_mul(4))
        .and_then(|bytes| usize::try_from(bytes).ok())
        .ok_or(CaptureError::InvalidRegion)?;

    if pixels.len() != expected_len {
        return Err(CaptureError::Backend(format!(
            "RGBA buffer length {} does not match {image_width}x{image_height} image",
            pixels.len()
        )));
    }

    let max_x = region
        .x()
        .checked_add(region.width())
        .ok_or(CaptureError::InvalidRegion)?;
    let max_y = region
        .y()
        .checked_add(region.height())
        .ok_or(CaptureError::InvalidRegion)?;

    if max_x > image_width || max_y > image_height {
        return Err(CaptureError::RegionOutOfBounds {
            image_width,
            image_height,
            region,
        });
    }

    let source_stride = usize::try_from(image_width)
        .ok()
        .and_then(|width| width.checked_mul(4))
        .ok_or(CaptureError::InvalidRegion)?;
    let row_start = usize::try_from(region.x())
        .ok()
        .and_then(|x| x.checked_mul(4))
        .ok_or(CaptureError::InvalidRegion)?;
    let row_len = usize::try_from(region.width())
        .ok()
        .and_then(|width| width.checked_mul(4))
        .ok_or(CaptureError::InvalidRegion)?;
    let output_len = row_len
        .checked_mul(region.height() as usize)
        .ok_or(CaptureError::InvalidRegion)?;
    let mut cropped = Vec::with_capacity(output_len);

    for row in region.y()..max_y {
        let start = usize::try_from(row)
            .ok()
            .and_then(|row| row.checked_mul(source_stride))
            .and_then(|offset| offset.checked_add(row_start))
            .ok_or(CaptureError::InvalidRegion)?;
        let end = start
            .checked_add(row_len)
            .ok_or(CaptureError::InvalidRegion)?;
        cropped.extend_from_slice(&pixels[start..end]);
    }

    Ok(cropped)
}

pub fn bgra_to_rgb(bgra_data: &[u8]) -> Vec<u8> {
    let pixel_count = bgra_data.len() / 4;
    let mut rgb_data = vec![0; pixel_count * 3];

    rgb_data
        .par_chunks_mut(3)
        .zip(bgra_data.par_chunks_exact(4))
        .for_each(|(rgb, bgra)| {
            rgb.copy_from_slice(&[bgra[2], bgra[1], bgra[0]]);
        });

    rgb_data
}

pub fn bgra_to_rgba(bgra_data: &[u8]) -> Vec<u8> {
    let pixel_count = bgra_data.len() / 4;
    let mut rgba_data = vec![0; pixel_count * 4];

    rgba_data
        .par_chunks_mut(4)
        .zip(bgra_data.par_chunks_exact(4))
        .for_each(|(rgba, bgra)| {
            rgba.copy_from_slice(&[bgra[2], bgra[1], bgra[0], bgra[3]]);
        });

    rgba_data
}

pub fn dynamic_image_from_bgra(
    data: &[u8],
    width: u32,
    height: u32,
    pixel_format: PixelFormat,
) -> Result<DynamicImage, CaptureError> {
    let expected_len = (width as usize)
        .checked_mul(height as usize)
        .and_then(|pixel_count| pixel_count.checked_mul(4))
        .ok_or_else(|| CaptureError::Backend("BGRA frame dimensions are too large".to_string()))?;
    if data.len() != expected_len {
        return Err(CaptureError::Backend(format!(
            "BGRA buffer length {} does not match {width}x{height} frame",
            data.len()
        )));
    }

    match pixel_format {
        PixelFormat::Rgb8 => image::RgbImage::from_raw(width, height, bgra_to_rgb(data))
            .map(DynamicImage::ImageRgb8)
            .ok_or_else(|| CaptureError::Backend("failed to create RGB8 image".to_string())),
        PixelFormat::Rgba8 => image::RgbaImage::from_raw(width, height, bgra_to_rgba(data))
            .map(DynamicImage::ImageRgba8)
            .ok_or_else(|| CaptureError::Backend("failed to create RGBA8 image".to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::GenericImageView;

    fn sample_rgb_image() -> DynamicImage {
        DynamicImage::ImageRgb8(
            image::RgbImage::from_raw(
                3,
                2,
                vec![
                    1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18,
                ],
            )
            .unwrap(),
        )
    }

    #[test]
    fn crops_rgb_images_by_pixel_region() {
        let cropped =
            crop_rgb_image(&sample_rgb_image(), PixelRect::new(1, 0, 2, 2).unwrap()).unwrap();

        assert_eq!(cropped.dimensions(), (2, 2));
        assert_eq!(
            cropped.as_bytes(),
            &[4, 5, 6, 7, 8, 9, 13, 14, 15, 16, 17, 18]
        );
    }

    #[test]
    fn rejects_out_of_bounds_regions() {
        let error =
            crop_rgb_image(&sample_rgb_image(), PixelRect::new(2, 1, 2, 1).unwrap()).unwrap_err();

        assert!(matches!(error, CaptureError::RegionOutOfBounds { .. }));
    }

    #[test]
    fn crops_rgba_pixel_buffers_by_pixel_region() {
        let pixels = [
            1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24,
        ];

        let cropped = crop_rgba_pixels(&pixels, 3, 2, PixelRect::new(1, 0, 2, 2).unwrap()).unwrap();

        assert_eq!(
            cropped,
            [5, 6, 7, 8, 9, 10, 11, 12, 17, 18, 19, 20, 21, 22, 23, 24]
        );
    }

    #[test]
    fn rejects_rgba_pixel_buffers_with_wrong_length() {
        let error =
            crop_rgba_pixels(&[0; 15], 2, 2, PixelRect::new(0, 0, 1, 1).unwrap()).unwrap_err();

        assert!(error.to_string().contains("does not match 2x2 image"));
    }

    #[test]
    fn encodes_png_with_original_dimensions() {
        let encoded = encode_image(&sample_rgb_image(), ImageEncoder::Png).unwrap();
        let decoded =
            image::load_from_memory_with_format(&encoded, image::ImageFormat::Png).unwrap();

        assert_eq!(decoded.dimensions(), (3, 2));
    }

    #[test]
    fn converts_bgra_channels_without_unsafe_buffers() {
        let bgra = [3, 2, 1, 255, 30, 20, 10, 128];

        assert_eq!(bgra_to_rgb(&bgra), [1, 2, 3, 10, 20, 30]);
        assert_eq!(bgra_to_rgba(&bgra), [1, 2, 3, 255, 10, 20, 30, 128]);
    }

    #[test]
    fn creates_rgb_image_from_bgra_frame() {
        let image = dynamic_image_from_bgra(&[3, 2, 1, 255], 1, 1, PixelFormat::Rgb8).unwrap();

        assert_eq!(image.to_rgb8().as_raw(), &[1, 2, 3]);
    }

    #[test]
    fn rejects_bgra_frame_with_wrong_length() {
        let error = dynamic_image_from_bgra(&[0; 3], 1, 1, PixelFormat::Rgba8).unwrap_err();

        assert!(error.to_string().contains("does not match 1x1 frame"));
    }
}
