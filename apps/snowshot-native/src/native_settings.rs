use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use snow_shot_annotate::{AnnotationTool, ElementStyle, OcrLayerStyle, RgbaColor};
use windows::Win32::Storage::FileSystem::{
    MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
};
use windows::core::HSTRING;

const SETTINGS_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NativeSettings {
    pub version: u32,
    pub tool_styles: BTreeMap<String, ElementStyle>,
    #[serde(default)]
    pub ocr_style: OcrLayerStyle,
    pub last_shape: i32,
    pub last_line: i32,
    pub last_pen: i32,
    pub last_privacy: i32,
}

impl Default for NativeSettings {
    fn default() -> Self {
        let mut settings = Self {
            version: SETTINGS_VERSION,
            tool_styles: BTreeMap::new(),
            ocr_style: OcrLayerStyle::default(),
            last_shape: 8,
            last_line: 3,
            last_pen: 1,
            last_privacy: 5,
        };
        for tool in [
            AnnotationTool::Select,
            AnnotationTool::Pen,
            AnnotationTool::Line,
            AnnotationTool::Arrow,
            AnnotationTool::Rectangle,
            AnnotationTool::Ellipse,
            AnnotationTool::Diamond,
            AnnotationTool::Highlighter,
            AnnotationTool::SerialNumber,
            AnnotationTool::Text,
            AnnotationTool::Mosaic,
            AnnotationTool::Blur,
            AnnotationTool::Eraser,
        ] {
            let mut style = ElementStyle::for_tool(tool, RgbaColor::RED, 6.0);
            match tool {
                AnnotationTool::Rectangle | AnnotationTool::Ellipse | AnnotationTool::Diamond => {
                    style.fill = RgbaColor::new(232, 68, 68, 28);
                }
                AnnotationTool::Highlighter => {
                    style.stroke = RgbaColor::new(255, 214, 10, 255);
                }
                AnnotationTool::Text => {
                    style.text = RgbaColor::new(30, 30, 30, 255);
                }
                _ => {}
            }
            settings
                .tool_styles
                .insert(tool_key(tool).to_string(), style);
        }
        settings
    }
}

impl NativeSettings {
    pub fn load() -> Self {
        let Ok(path) = settings_path() else {
            return Self::default();
        };
        let Ok(bytes) = fs::read(path) else {
            return Self::default();
        };
        let Ok(settings) = serde_json::from_slice::<Self>(&bytes) else {
            return Self::default();
        };
        if settings.version == SETTINGS_VERSION {
            settings
        } else {
            Self::default()
        }
    }

    pub fn style(&self, tool: AnnotationTool) -> ElementStyle {
        self.tool_styles
            .get(tool_key(tool))
            .cloned()
            .unwrap_or_else(|| ElementStyle::for_tool(tool, RgbaColor::RED, 6.0))
    }

    pub fn set_style(&mut self, tool: AnnotationTool, style: ElementStyle) {
        self.tool_styles.insert(tool_key(tool).to_string(), style);
    }

    pub fn set_ocr_style(&mut self, style: OcrLayerStyle) {
        self.ocr_style = style;
    }

    pub fn remember_tool(&mut self, tool_id: i32) {
        match tool_id {
            8 | 9 | 12 => self.last_shape = tool_id,
            2 | 3 => self.last_line = tool_id,
            1 | 10 => self.last_pen = tool_id,
            5 | 6 => self.last_privacy = tool_id,
            _ => {}
        }
    }

    pub fn save(&self) -> Result<(), String> {
        let path = settings_path()?;
        let directory = path
            .parent()
            .ok_or_else(|| "Native 设置目录无效。".to_string())?;
        fs::create_dir_all(directory)
            .map_err(|error| format!("无法创建 Native 设置目录：{error}"))?;
        let temporary = path.with_extension("json.tmp");
        let bytes = serde_json::to_vec_pretty(self)
            .map_err(|error| format!("无法序列化 Native 设置：{error}"))?;
        fs::write(&temporary, bytes).map_err(|error| format!("无法写入 Native 设置：{error}"))?;
        let source = HSTRING::from(temporary.as_os_str());
        let destination = HSTRING::from(path.as_os_str());
        // SAFETY: both paths point to owned files in the application data directory.
        unsafe {
            MoveFileExW(
                &source,
                &destination,
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        }
        .map_err(|error| format!("无法原子提交 Native 设置：{error}"))
    }
}

fn settings_path() -> Result<PathBuf, String> {
    std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .map(|path| path.join("Snow Shot").join("native-lite.json"))
        .ok_or_else(|| "无法定位 Windows AppData 目录。".to_string())
}

fn tool_key(tool: AnnotationTool) -> &'static str {
    match tool {
        AnnotationTool::Select => "select",
        AnnotationTool::Pen => "pen",
        AnnotationTool::Line => "line",
        AnnotationTool::Arrow => "arrow",
        AnnotationTool::Rectangle => "rectangle",
        AnnotationTool::Ellipse => "ellipse",
        AnnotationTool::Diamond => "diamond",
        AnnotationTool::Highlighter => "highlighter",
        AnnotationTool::SerialNumber => "serial",
        AnnotationTool::Text => "text",
        AnnotationTool::Mosaic => "mosaic",
        AnnotationTool::Blur => "blur",
        AnnotationTool::Eraser => "eraser",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_tool_has_an_independent_default_style() {
        let settings = NativeSettings::default();
        assert_ne!(
            settings.style(AnnotationTool::Pen),
            settings.style(AnnotationTool::Highlighter)
        );
        assert_ne!(
            settings.style(AnnotationTool::Rectangle),
            settings.style(AnnotationTool::Text)
        );
    }

    #[test]
    fn grouped_tools_remember_their_last_variant() {
        let mut settings = NativeSettings::default();
        settings.remember_tool(12);
        settings.remember_tool(2);
        settings.remember_tool(10);
        settings.remember_tool(6);
        assert_eq!(
            (
                settings.last_shape,
                settings.last_line,
                settings.last_pen,
                settings.last_privacy,
            ),
            (12, 2, 10, 6)
        );
    }

    #[test]
    fn older_version_one_files_receive_new_field_defaults() {
        let settings: NativeSettings = serde_json::from_str(
            r#"{
                "version": 1,
                "tool_styles": {},
                "last_shape": 8,
                "last_line": 3,
                "last_pen": 1,
                "last_privacy": 5
            }"#,
        )
        .unwrap();
        assert_eq!(settings.ocr_style, OcrLayerStyle::default());
    }

    #[test]
    fn ocr_style_is_persistable_without_serial_memory() {
        let mut settings = NativeSettings::default();
        settings.ocr_style.visible = false;
        settings.ocr_style.blur_strength = 1.75;
        let encoded = serde_json::to_vec(&settings).unwrap();
        let decoded: NativeSettings = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(decoded.ocr_style, settings.ocr_style);
        assert!(
            !String::from_utf8(encoded)
                .unwrap()
                .contains("serial_number")
        );
    }
}
