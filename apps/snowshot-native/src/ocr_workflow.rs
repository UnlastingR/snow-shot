use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, SyncSender};
use std::thread;
use std::time::Duration;

use snow_shot_ocr::{OcrDetectResult, OcrService};

use crate::capture_workflow::FrozenRegionFrame;

const PLUGIN_VERSION: &str = "20251005";
const PLUGIN_NAME: &str = "rapid_ocr";
const MODEL_DOWNLOAD_URL: &str = "https://snowshot.top/plugins/20251005/windows_x64/rapid_ocr.zip";
const MAX_MODEL_ARCHIVE_BYTES: u64 = 128 * 1024 * 1024;
const OCR_IDLE_TIMEOUT: Duration = Duration::from_secs(90);
const MODEL_FILES: [&str; 3] = [
    snow_shot_ocr::DEFAULT_DET_MODEL,
    snow_shot_ocr::DEFAULT_CLS_MODEL,
    snow_shot_ocr::DEFAULT_REC_MODEL,
];

type OcrCompletion = Box<dyn FnOnce(Result<OcrDetectResult, String>) + Send>;

#[derive(Clone)]
pub struct OcrWorker {
    sender: SyncSender<OcrRequest>,
}

struct OcrRequest {
    frame: Option<FrozenRegionFrame>,
    progress: Arc<dyn Fn(String) + Send + Sync>,
    completion: Option<OcrCompletion>,
}

impl OcrWorker {
    pub fn start() -> Result<Self, String> {
        let (sender, receiver) = mpsc::sync_channel::<OcrRequest>(1);
        thread::Builder::new()
            .name("snowshot-ocr".to_string())
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        while let Ok(mut request) = receiver.recv() {
                            complete(&mut request, Err(format!("无法创建 OCR 运行时：{error}")));
                        }
                        return;
                    }
                };
                let mut service = OcrService::new();
                let mut configured_model_dir = None::<PathBuf>;
                let mut session_loaded = false;

                loop {
                    match receiver.recv_timeout(OCR_IDLE_TIMEOUT) {
                        Ok(mut request) => {
                            publish_progress(&request, "正在检查本地 OCR 模型…");
                            let model_dir = match ensure_models_installed() {
                                Ok(path) => path,
                                Err(error) => {
                                    complete(&mut request, Err(error));
                                    continue;
                                }
                            };

                            if configured_model_dir.as_ref() != Some(&model_dir) {
                                publish_progress(&request, "正在加载 PP-OCRv4 模型…");
                                let init_result = runtime.block_on(service.init_models(
                                    model_dir.clone(),
                                    None,
                                    None,
                                    None,
                                    false,
                                    false,
                                ));
                                if let Err(error) = init_result {
                                    complete(&mut request, Err(error));
                                    continue;
                                }
                                configured_model_dir = Some(model_dir);
                            }

                            let Some(frame) = request.frame.take() else {
                                complete(&mut request, Err("OCR 选区数据已被消费。".to_string()));
                                continue;
                            };
                            let (width, height, rgba) = frame.into_parts();
                            publish_progress(&request, &format!("正在识别 {width}×{height} 选区…"));
                            let result = runtime
                                .block_on(service.detect_rgba(rgba, width, height, 1.0, true));
                            session_loaded = result.is_ok();
                            complete(&mut request, result);
                        }
                        Err(mpsc::RecvTimeoutError::Timeout) => {
                            if session_loaded {
                                let _ = runtime.block_on(service.release_session());
                                session_loaded = false;
                            }
                        }
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                }
            })
            .map_err(|error| format!("无法启动 OCR 后台任务：{error}"))?;

        Ok(Self { sender })
    }

    pub fn detect(
        &self,
        frame: FrozenRegionFrame,
        progress: impl Fn(String) + Send + Sync + 'static,
        completion: impl FnOnce(Result<OcrDetectResult, String>) + Send + 'static,
    ) -> Result<(), String> {
        self.sender
            .try_send(OcrRequest {
                frame: Some(frame),
                progress: Arc::new(progress),
                completion: Some(Box::new(completion)),
            })
            .map_err(|error| match error {
                mpsc::TrySendError::Full(_) => "已有 OCR 任务正在运行，请稍候。".to_string(),
                mpsc::TrySendError::Disconnected(_) => "OCR 后台任务已停止。".to_string(),
            })
    }
}

fn publish_progress(request: &OcrRequest, status: &str) {
    (request.progress)(status.to_string());
}

fn complete(request: &mut OcrRequest, result: Result<OcrDetectResult, String>) {
    if let Some(completion) = request.completion.take() {
        completion(result);
    }
}

fn ensure_models_installed() -> Result<PathBuf, String> {
    let app_data = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .ok_or_else(|| "无法定位 Windows AppData 目录。".to_string())?;
    let base_dir = app_data.join("com.chao.snowshot");
    let model_dir = base_dir
        .join("plugins")
        .join(PLUGIN_VERSION)
        .join(PLUGIN_NAME);
    if models_are_ready(&model_dir) {
        return Ok(model_dir);
    }

    let download_dir = base_dir.join("pluginsDownloads").join(PLUGIN_VERSION);
    fs::create_dir_all(&download_dir).map_err(|error| format!("无法创建 OCR 下载目录：{error}"))?;
    let archive_path = download_dir.join(format!("{PLUGIN_NAME}.zip"));
    if !archive_path.is_file() {
        download_model_archive(&archive_path)?;
    }
    extract_models(&archive_path, &model_dir)?;

    if models_are_ready(&model_dir) {
        Ok(model_dir)
    } else {
        Err("OCR 模型包缺少 PP-OCRv4 必需文件。".to_string())
    }
}

fn models_are_ready(model_dir: &Path) -> bool {
    MODEL_FILES.iter().all(|name| {
        model_dir
            .join(name)
            .metadata()
            .is_ok_and(|metadata| metadata.is_file() && metadata.len() > 0)
    })
}

fn download_model_archive(archive_path: &Path) -> Result<(), String> {
    let part_path = archive_path.with_extension("zip.part");
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(180))
        .build()
        .map_err(|error| format!("无法创建 OCR 下载客户端：{error}"))?;
    let mut response = client
        .get(MODEL_DOWNLOAD_URL)
        .send()
        .and_then(reqwest::blocking::Response::error_for_status)
        .map_err(|error| format!("无法下载 OCR 模型：{error}"))?;
    if response
        .content_length()
        .is_some_and(|length| length > MAX_MODEL_ARCHIVE_BYTES)
    {
        return Err("OCR 模型包超过安全大小限制。".to_string());
    }

    let mut file =
        File::create(&part_path).map_err(|error| format!("无法创建 OCR 模型临时文件：{error}"))?;
    let copied = io::copy(&mut response, &mut file)
        .map_err(|error| format!("下载 OCR 模型时写入失败：{error}"))?;
    if copied == 0 || copied > MAX_MODEL_ARCHIVE_BYTES {
        return Err("OCR 模型包大小无效。".to_string());
    }
    drop(file);
    fs::rename(&part_path, archive_path).map_err(|error| format!("无法提交 OCR 模型包：{error}"))
}

fn extract_models(archive_path: &Path, model_dir: &Path) -> Result<(), String> {
    fs::create_dir_all(model_dir).map_err(|error| format!("无法创建 OCR 模型目录：{error}"))?;
    let archive_file =
        File::open(archive_path).map_err(|error| format!("无法读取 OCR 模型包：{error}"))?;
    let mut archive = zip::ZipArchive::new(archive_file)
        .map_err(|error| format!("OCR 模型包格式无效：{error}"))?;

    for model_name in MODEL_FILES {
        let mut entry_index = None;
        let mut entry_error = None;
        for index in 0..archive.len() {
            match archive.by_index(index) {
                Ok(entry)
                    if entry
                        .name()
                        .rsplit(['/', '\\'])
                        .next()
                        .is_some_and(|name| name.eq_ignore_ascii_case(model_name)) =>
                {
                    entry_index = Some(index);
                    break;
                }
                Ok(_) => {}
                Err(error) => entry_error = Some(error.to_string()),
            }
        }
        let entry_index = entry_index.ok_or_else(|| {
            entry_error.map_or_else(
                || format!("OCR 模型包缺少 {model_name}"),
                |error| format!("无法读取 OCR 模型包条目：{error}"),
            )
        })?;
        let mut entry = archive
            .by_index(entry_index)
            .map_err(|error| format!("无法读取 OCR 模型 {model_name}：{error}"))?;
        if entry.size() == 0 || entry.size() > MAX_MODEL_ARCHIVE_BYTES {
            return Err(format!("OCR 模型 {model_name} 大小无效。"));
        }
        let target_path = model_dir.join(model_name);
        let part_path = target_path.with_extension("onnx.part");
        let mut target = File::create(&part_path)
            .map_err(|error| format!("无法创建 OCR 模型 {model_name}：{error}"))?;
        io::copy(&mut entry, &mut target)
            .map_err(|error| format!("无法解压 OCR 模型 {model_name}：{error}"))?;
        drop(target);
        if target_path.exists() {
            fs::remove_file(&target_path)
                .map_err(|error| format!("无法替换 OCR 模型 {model_name}：{error}"))?;
        }
        fs::rename(&part_path, &target_path)
            .map_err(|error| format!("无法提交 OCR 模型 {model_name}：{error}"))?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn model_directory_matches_legacy_plugin_layout() {
        let app_data = PathBuf::from(r"C:\Users\Test\AppData\Roaming");
        let model_dir = app_data
            .join("com.chao.snowshot")
            .join("plugins")
            .join(PLUGIN_VERSION)
            .join(PLUGIN_NAME);

        assert!(model_dir.ends_with(r"plugins\20251005\rapid_ocr"));
    }

    #[test]
    fn extracts_xz_models_by_file_name_instead_of_archive_root() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let test_root =
            std::env::temp_dir().join(format!("snowshot-ocr-xz-{}-{unique}", std::process::id()));
        fs::create_dir_all(&test_root).unwrap();
        let archive_path = test_root.join("rapid_ocr.zip");
        let model_dir = test_root.join("models");

        let archive_file = File::create(&archive_path).unwrap();
        let mut writer = zip::ZipWriter::new(archive_file);
        let options =
            zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Xz);
        for (index, model_name) in MODEL_FILES.iter().enumerate() {
            writer
                .start_file(format!("different-root/{model_name}"), options)
                .unwrap();
            writer
                .write_all(format!("model-{index}").as_bytes())
                .unwrap();
        }
        writer.finish().unwrap();

        extract_models(&archive_path, &model_dir).unwrap();

        for (index, model_name) in MODEL_FILES.iter().enumerate() {
            assert_eq!(
                fs::read(model_dir.join(model_name)).unwrap(),
                format!("model-{index}").as_bytes()
            );
        }

        fs::remove_dir_all(test_root).unwrap();
    }
}
