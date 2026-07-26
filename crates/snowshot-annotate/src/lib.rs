mod color;
mod document;

pub use color::{CmykColor, HslColor, HsvColor, RgbaColor};
pub use document::{
    AnnotationDocument, AnnotationElement, AnnotationError, AnnotationTool, ElementId, ElementKind,
    ElementStyle, LayerCommand, OcrBlock, OcrLayerStyle, Point, Rect, SelectionHandle, StylePatch,
    TextAlignment, apply_style_patch,
};
