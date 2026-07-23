use std::fmt;

use image::codecs::avif::AvifEncoder;
use image::codecs::jpeg::JpegEncoder;
use image::codecs::png::{CompressionType, FilterType, PngEncoder};
use image::codecs::webp::WebPEncoder;
use image::DynamicImage;
use rayon::iter::{IndexedParallelIterator, ParallelIterator};
use rayon::slice::{ParallelSlice, ParallelSliceMut};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageEncoder {
    Webp,
    Png,
    Avif,
    Jpeg,
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
}
