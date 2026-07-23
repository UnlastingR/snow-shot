use log;
pub use snow_shot_app_services::ocr_service::OcrDetectResult;
use snow_shot_app_services::ocr_service::OcrService;
use std::io::Cursor;
use std::path::PathBuf;
use tokio::sync::Mutex;

pub async fn ocr_init(
    orc_plugin_path: PathBuf,
    ocr_service: tauri::State<'_, Mutex<OcrService>>,
    det_model: Option<String>,
    cls_model: Option<String>,
    rec_model: Option<String>,
    hot_start: bool,
    ocr_model_write_to_memory: bool,
) -> Result<(), String> {
    let mut ocr_service = ocr_service.lock().await;

    ocr_service
        .init_models(
            orc_plugin_path,
            det_model,
            cls_model,
            rec_model,
            hot_start,
            ocr_model_write_to_memory,
        )
        .await?;

    Ok(())
}

pub async fn ocr_detect_core(
    ocr_service: tauri::State<'_, Mutex<OcrService>>,
    image: image::DynamicImage,
    scale_factor: f32,
    detect_angle: bool,
) -> Result<OcrDetectResult, String> {
    let mut ocr_service = ocr_service.lock().await;
    ocr_service.detect(image, scale_factor, detect_angle).await
}

pub async fn ocr_detect(
    ocr_service: tauri::State<'_, Mutex<OcrService>>,
    request: tauri::ipc::Request<'_>,
) -> Result<OcrDetectResult, String> {
    log::info!("[ocr_detect] start detect");

    let image_data = match request.body() {
        tauri::ipc::InvokeBody::Raw(data) => data,
        _ => return Err("[ocr_detect] Invalid request body".to_string()),
    };

    let mut image = match image::load(Cursor::new(image_data), image::ImageFormat::Png) {
        Ok(image) => image,
        Err(_) => return Err("[ocr_detect] Invalid image".to_string()),
    };

    let mut scale_factor: f32 = match request.headers().get("x-scale-factor") {
        Some(header) => match header.to_str() {
            Ok(scale_factor) => scale_factor.parse::<f32>().unwrap(),
            Err(_) => return Err("[ocr_detect] Invalid scale factor".to_string()),
        },
        None => return Err("[ocr_detect] Missing scale factor".to_string()),
    };

    // 分辨率过小的图片识别可能有问题，当 scale_factor 低于 1.5 时，放大图片使有效缩放达到 1.5
    let target_scale_factor = 1.5;
    if scale_factor < target_scale_factor && scale_factor > 0.0 {
        scale_factor = target_scale_factor;
        let resize_factor = target_scale_factor / scale_factor;
        image = image.resize(
            (image.width() as f32 * resize_factor) as u32,
            (image.height() as f32 * resize_factor) as u32,
            image::imageops::FilterType::Lanczos3,
        );
    }

    let detect_angle = match request.headers().get("x-detect-angle") {
        Some(header) => match header.to_str() {
            Ok(detect_angle) => detect_angle.parse::<bool>().unwrap(),
            Err(_) => return Err("[ocr_detect] Invalid detect angle".to_string()),
        },
        None => return Err("[ocr_detect] Missing detect angle".to_string()),
    };

    ocr_detect_core(ocr_service, image, scale_factor, detect_angle).await
}

#[cfg(target_os = "windows")]
pub async fn ocr_detect_with_shared_buffer(
    ocr_service: tauri::State<'_, Mutex<OcrService>>,
    shared_buffer_service: tauri::State<'_, std::sync::Arc<snow_shot_webview::SharedBufferService>>,
    channel_id: String,
    scale_factor: f32,
    detect_angle: bool,
) -> Result<OcrDetectResult, String> {
    log::info!("[ocr_detect_with_shared_buffer] start detect");

    let image_data = match shared_buffer_service.receive_data(channel_id) {
        Ok(image_data) => image_data,
        Err(e) => {
            return Err(format!(
                "[ocr_detect_with_shared_buffer] Failed to receive image data: {}",
                e
            ));
        }
    };

    let image_width = u32::from_le_bytes(
        image_data[image_data.len() - 8..image_data.len() - 4]
            .try_into()
            .unwrap(),
    );
    let image_height = u32::from_le_bytes(
        image_data[image_data.len() - 4..image_data.len()]
            .try_into()
            .unwrap(),
    );

    ocr_detect_core(
        ocr_service,
        image::DynamicImage::ImageRgba8(
            match image::RgbaImage::from_raw(image_width, image_height, image_data) {
                Some(image) => image,
                None => return Err("[ocr_detect_with_shared_buffer] Invalid image".to_string()),
            },
        ),
        scale_factor,
        detect_angle,
    )
    .await
}

pub async fn ocr_release(ocr_service: tauri::State<'_, Mutex<OcrService>>) -> Result<(), String> {
    let mut ocr_service = ocr_service.lock().await;

    ocr_service.release_session().await?;

    Ok(())
}

pub async fn list_ocr_model_files(dir_path: PathBuf) -> Result<Vec<String>, String> {
    let mut entries = tokio::fs::read_dir(&dir_path)
        .await
        .map_err(|e| format!("[list_ocr_model_files] Failed to read dir: {}", e))?;

    let mut files = Vec::new();
    while let Ok(Some(entry)) = entries.next_entry().await {
        let file_name = entry.file_name();
        if let Some(name) = file_name.to_str() {
            if name.ends_with(".onnx") {
                files.push(name.to_string());
            }
        }
    }
    files.sort();
    Ok(files)
}
