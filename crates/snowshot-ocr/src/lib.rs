use std::path::{Path, PathBuf};

use image::DynamicImage;
use ort::session::builder::SessionBuilder;
use paddle_ocr_rs::ocr_lite::OcrLite;
use paddle_ocr_rs::ocr_result::TextBlock;
use rayon::iter::{IndexedParallelIterator, ParallelIterator};
use rayon::slice::{ParallelSlice, ParallelSliceMut};
use serde::{Deserialize, Serialize};

pub const DEFAULT_DET_MODEL: &str = "ch_PP-OCRv4_det_infer.onnx";
pub const DEFAULT_CLS_MODEL: &str = "ch_ppocr_mobile_v2.0_cls_infer.onnx";
pub const DEFAULT_REC_MODEL: &str = "ch_PP-OCRv4_rec_infer.onnx";

pub struct OcrService {
    hot_start: bool,
    ocr_core: Option<OcrLite>,
    det_model: Option<(PathBuf, Option<Vec<u8>>)>,
    rec_model: Option<(PathBuf, Option<Vec<u8>>)>,
    cls_model: Option<(PathBuf, Option<Vec<u8>>)>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Copy, PartialOrd, Serialize, Deserialize)]
pub enum OcrModel {
    RapidOcrV4,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct OcrDetectResult {
    pub text_blocks: Vec<TextBlock>,
    pub scale_factor: f32,
}

impl Default for OcrService {
    fn default() -> Self {
        Self::new()
    }
}

impl OcrDetectResult {
    pub fn plain_text(&self) -> String {
        let mut merged = String::new();
        for text in self
            .text_blocks
            .iter()
            .map(|block| block.text.trim())
            .filter(|text| !text.is_empty())
        {
            if let Some(previous) = merged.chars().last() {
                let current = text.chars().next().unwrap_or_default();
                if preserves_line_break(previous, text) {
                    merged.push('\n');
                } else if !is_cjk(previous) && !is_cjk(current) {
                    merged.push(' ');
                }
            }
            merged.push_str(text);
        }
        merged
    }
}

fn preserves_line_break(previous: char, current_line: &str) -> bool {
    matches!(
        previous,
        '。' | '！' | '？' | '；' | '：' | '!' | '?' | ';' | ':'
    ) || current_line
        .chars()
        .next()
        .is_some_and(|character| matches!(character, '-' | '•' | '·' | '●' | '○'))
        || current_line
            .chars()
            .next()
            .is_some_and(|character| character.is_ascii_digit())
            && current_line
                .chars()
                .skip_while(|character| character.is_ascii_digit())
                .next()
                .is_some_and(|character| matches!(character, '.' | ')' | '、'))
}

fn is_cjk(character: char) -> bool {
    matches!(
        character as u32,
        0x3400..=0x4DBF
            | 0x4E00..=0x9FFF
            | 0xF900..=0xFAFF
            | 0x3040..=0x30FF
            | 0xAC00..=0xD7AF
    )
}

impl OcrService {
    pub fn new() -> Self {
        Self {
            hot_start: false,
            ocr_core: None,
            det_model: None,
            rec_model: None,
            cls_model: None,
        }
    }

    async fn read_model_data(
        &self,
        det_path: &Path,
        cls_path: &Path,
        rec_path: &Path,
    ) -> Result<(Vec<u8>, Vec<u8>, Vec<u8>), String> {
        let (det_result, cls_result, rec_result) = tokio::join!(
            tokio::fs::read(det_path),
            tokio::fs::read(cls_path),
            tokio::fs::read(rec_path)
        );

        Ok((
            det_result.map_err(|e| {
                format!(
                    "[OcrService::read_model_data] Failed to read det model data: {}",
                    e
                )
            })?,
            cls_result.map_err(|e| {
                format!(
                    "[OcrService::read_model_data] Failed to read cls model data: {}",
                    e
                )
            })?,
            rec_result.map_err(|e| {
                format!(
                    "[OcrService::read_model_data] Failed to read rec model data: {}",
                    e
                )
            })?,
        ))
    }

    fn build_session(builder: SessionBuilder) -> Result<SessionBuilder, ort::Error> {
        let num_thread = num_cpus::get_physical();
        builder
            .with_inter_threads(num_thread)?
            .with_intra_threads(num_thread)?
            .with_optimization_level(ort::session::builder::GraphOptimizationLevel::Level3)
    }

    pub async fn init_session(&mut self) -> Result<(), String> {
        let ((det_path, det_model_data), (cls_path, cls_model_data), (rec_path, rec_model_data)) = (
            self.det_model
                .as_ref()
                .expect("[OcrService::init_ocr_core] Det model is not loaded"),
            self.cls_model
                .as_ref()
                .expect("[OcrService::init_ocr_core] Cls model is not loaded"),
            self.rec_model
                .as_ref()
                .expect("[OcrService::init_ocr_core] Rec model is not loaded"),
        );

        let mut ocr_core = OcrLite::new();

        if let (Some(det_model_data), Some(cls_model_data), Some(rec_model_data)) =
            (det_model_data, cls_model_data, rec_model_data)
        {
            ocr_core.init_models_from_memory_custom(
                det_model_data,
                cls_model_data,
                rec_model_data,
                Self::build_session,
            )
        } else {
            let (det_model_data, cls_model_data, rec_model_data) =
                self.read_model_data(det_path, cls_path, rec_path).await?;

            ocr_core.init_models_from_memory_custom(
                det_model_data.as_ref(),
                cls_model_data.as_ref(),
                rec_model_data.as_ref(),
                Self::build_session,
            )
        }
        .map_err(|e| format!("[OcrService::init_ocr_core] Failed to init models: {}", e))?;

        self.ocr_core.replace(ocr_core);

        Ok(())
    }

    pub async fn init_models(
        &mut self,
        ocr_model_path: PathBuf,
        det_model_name: Option<String>,
        cls_model_name: Option<String>,
        rec_model_name: Option<String>,
        hot_start: bool,
        ocr_model_write_to_memory: bool,
    ) -> Result<(), String> {
        let det_file = det_model_name.unwrap_or_else(|| DEFAULT_DET_MODEL.to_string());
        let cls_file = cls_model_name.unwrap_or_else(|| DEFAULT_CLS_MODEL.to_string());
        let rec_file = rec_model_name.unwrap_or_else(|| DEFAULT_REC_MODEL.to_string());

        log::info!(
            "[OcrService::init_models] ocr_model_path: {:?}, det: {}, cls: {}, rec: {}, hot_start: {:?}, ocr_model_write_to_memory: {:?}",
            ocr_model_path,
            det_file,
            cls_file,
            rec_file,
            hot_start,
            ocr_model_write_to_memory
        );

        let det_model_path = ocr_model_path.join(&det_file);
        let cls_model_path = ocr_model_path.join(&cls_file);
        let rec_model_path = ocr_model_path.join(&rec_file);

        let (det_model_config, cls_model_config, rec_model_config) = if ocr_model_write_to_memory {
            let (det_result, cls_result, rec_result) = self
                .read_model_data(&det_model_path, &cls_model_path, &rec_model_path)
                .await?;

            (
                Some((det_model_path, Some(det_result))),
                Some((cls_model_path, Some(cls_result))),
                Some((rec_model_path, Some(rec_result))),
            )
        } else {
            (
                Some((det_model_path, None)),
                Some((cls_model_path, None)),
                Some((rec_model_path, None)),
            )
        };

        self.det_model = det_model_config;
        self.cls_model = cls_model_config;
        self.rec_model = rec_model_config;
        self.hot_start = hot_start;

        if self.hot_start {
            self.init_session().await?;
        } else {
            self.ocr_core.take();
        }

        Ok(())
    }

    pub async fn detect(
        &mut self,
        image: DynamicImage,
        scale_factor: f32,
        detect_angle: bool,
    ) -> Result<OcrDetectResult, String> {
        let mut scale_factor = scale_factor;
        let mut image = image;

        // Preserve the existing minimum effective scale used by the legacy implementation.
        let target_scale_factor = 1.5;
        if scale_factor < target_scale_factor && scale_factor > 0.0 {
            let resize_factor = target_scale_factor / scale_factor;
            image = image.resize(
                (image.width() as f32 * resize_factor) as u32,
                (image.height() as f32 * resize_factor) as u32,
                image::imageops::FilterType::Lanczos3,
            );
            scale_factor = target_scale_factor;
        }

        let max_size = image.height().max(image.width());

        let image_buffer = match image {
            DynamicImage::ImageRgb8(image) => image,
            DynamicImage::ImageRgba8(image) => {
                let rgb_data = convert_rgba_to_rgb(image.as_raw());
                image::RgbImage::from_raw(image.width(), image.height(), rgb_data)
                    .ok_or_else(|| "[ocr_detect_core] Invalid image".to_string())?
            }
            _ => return Err("[ocr_detect_core] Invalid image".to_string()),
        };

        let ocr_result = self.get_session().await?.detect_angle_rollback(
            &image_buffer,
            50,
            max_size,
            0.5,
            0.3,
            1.6,
            detect_angle,
            false,
            0.9,
        );

        match ocr_result {
            Ok(ocr_result) => Ok(OcrDetectResult {
                text_blocks: ocr_result.text_blocks,
                scale_factor,
            }),
            Err(e) => Err(format!("[ocr_detect_core] Failed to detect text: {}", e)),
        }
    }

    pub async fn detect_rgba(
        &mut self,
        rgba: Vec<u8>,
        width: u32,
        height: u32,
        scale_factor: f32,
        detect_angle: bool,
    ) -> Result<OcrDetectResult, String> {
        let image = image::RgbaImage::from_raw(width, height, rgba)
            .ok_or_else(|| "[OcrService::detect_rgba] Invalid RGBA image".to_string())?;
        self.detect(DynamicImage::ImageRgba8(image), scale_factor, detect_angle)
            .await
    }

    /// Release the ONNX session, or rebuild it when hot start is enabled.
    pub async fn release_session(&mut self) -> Result<(), String> {
        if self.hot_start {
            self.init_session().await?;
        } else {
            self.ocr_core.take();
        }

        Ok(())
    }

    pub async fn get_session(&mut self) -> Result<&mut OcrLite, String> {
        if self.ocr_core.is_none() {
            self.init_session().await?;
        }

        Ok(self.ocr_core.as_mut().unwrap())
    }
}

fn convert_rgba_to_rgb(image: &[u8]) -> Vec<u8> {
    let pixel_count = image.len() / 4;
    let mut rgb_data = vec![0; pixel_count * 3];

    rgb_data
        .par_chunks_mut(3)
        .zip(image.par_chunks_exact(4))
        .for_each(|(rgb, rgba)| {
            rgb.copy_from_slice(&rgba[..3]);
        });

    rgb_data
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rgba_conversion_preserves_rgb_channels() {
        let rgba = [1, 2, 3, 255, 10, 20, 30, 128];
        assert_eq!(convert_rgba_to_rgb(&rgba), [1, 2, 3, 10, 20, 30]);
    }

    #[test]
    fn plain_text_merges_wrapped_lines_and_preserves_sentence_breaks() {
        let result = OcrDetectResult {
            text_blocks: vec![
                TextBlock {
                    box_points: Vec::new(),
                    box_score: 1.0,
                    angle_index: 0,
                    angle_score: 1.0,
                    text: " 第一行 ".to_string(),
                    text_score: 1.0,
                },
                TextBlock {
                    box_points: Vec::new(),
                    box_score: 1.0,
                    angle_index: 0,
                    angle_score: 1.0,
                    text: " ".to_string(),
                    text_score: 1.0,
                },
                TextBlock {
                    box_points: Vec::new(),
                    box_score: 1.0,
                    angle_index: 0,
                    angle_score: 1.0,
                    text: "第二行。".to_string(),
                    text_score: 1.0,
                },
                TextBlock {
                    box_points: Vec::new(),
                    box_score: 1.0,
                    angle_index: 0,
                    angle_score: 1.0,
                    text: "Next line".to_string(),
                    text_score: 1.0,
                },
            ],
            scale_factor: 1.0,
        };

        assert_eq!(result.plain_text(), "第一行第二行。\nNext line");
    }

    #[tokio::test]
    async fn rgba_detection_rejects_invalid_buffer_before_loading_models() {
        let mut service = OcrService::new();
        let error = service
            .detect_rgba(vec![0; 3], 1, 1, 1.0, true)
            .await
            .unwrap_err();

        assert!(error.contains("Invalid RGBA image"));
    }

    #[tokio::test]
    async fn default_model_paths_are_preserved_without_loading_a_session() {
        let model_dir = PathBuf::from("models");
        let mut service = OcrService::new();

        service
            .init_models(model_dir.clone(), None, None, None, false, false)
            .await
            .unwrap();

        assert_eq!(
            service.det_model.as_ref().map(|model| &model.0),
            Some(&model_dir.join(DEFAULT_DET_MODEL))
        );
        assert_eq!(
            service.cls_model.as_ref().map(|model| &model.0),
            Some(&model_dir.join(DEFAULT_CLS_MODEL))
        );
        assert_eq!(
            service.rec_model.as_ref().map(|model| &model.0),
            Some(&model_dir.join(DEFAULT_REC_MODEL))
        );
        assert!(service.ocr_core.is_none());
    }

    #[tokio::test]
    async fn custom_model_names_are_preserved() {
        let model_dir = PathBuf::from("custom-models");
        let mut service = OcrService::new();

        service
            .init_models(
                model_dir.clone(),
                Some("det.onnx".to_string()),
                Some("cls.onnx".to_string()),
                Some("rec.onnx".to_string()),
                false,
                false,
            )
            .await
            .unwrap();

        assert_eq!(
            service.det_model.as_ref().map(|model| &model.0),
            Some(&model_dir.join("det.onnx"))
        );
        assert_eq!(
            service.cls_model.as_ref().map(|model| &model.0),
            Some(&model_dir.join("cls.onnx"))
        );
        assert_eq!(
            service.rec_model.as_ref().map(|model| &model.0),
            Some(&model_dir.join("rec.onnx"))
        );
    }
}
