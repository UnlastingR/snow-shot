use std::collections::VecDeque;
use std::fmt;
use std::sync::OnceLock;

use ab_glyph::{FontArc, FontVec};
use fontdb::{Database, Family, Query, Weight};
use image::{Rgba, RgbaImage};
use imageproc::drawing::{draw_text_mut, text_size};
use imageproc::filter::gaussian_blur_f32;
use imageproc::geometric_transformations::{Interpolation, rotate_about_center};
use serde::{Deserialize, Serialize};

use crate::RgbaColor;

const MAX_HISTORY: usize = 64;
const MIN_STROKE_WIDTH: f32 = 1.0;
const MIN_ELEMENT_SIZE: f32 = 8.0;
const HANDLE_RADIUS: f32 = 7.0;
const OCR_ID_BASE: u64 = 1_u64 << 63;

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Point {
    pub x: f32,
    pub y: f32,
}

impl Point {
    pub const fn new(x: f32, y: f32) -> Self {
        Self { x, y }
    }

    fn distance(self, other: Self) -> f32 {
        ((self.x - other.x).powi(2) + (self.y - other.y).powi(2)).sqrt()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Rect {
    pub left: f32,
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
}

impl Rect {
    pub fn from_points(first: Point, second: Point) -> Self {
        Self {
            left: first.x.min(second.x),
            top: first.y.min(second.y),
            right: first.x.max(second.x),
            bottom: first.y.max(second.y),
        }
    }

    pub fn width(self) -> f32 {
        self.right - self.left
    }

    pub fn height(self) -> f32 {
        self.bottom - self.top
    }

    pub fn center(self) -> Point {
        Point::new(
            (self.left + self.right) / 2.0,
            (self.top + self.bottom) / 2.0,
        )
    }

    pub fn contains(self, point: Point) -> bool {
        point.x >= self.left
            && point.x <= self.right
            && point.y >= self.top
            && point.y <= self.bottom
    }

    pub fn expanded(self, amount: f32) -> Self {
        Self {
            left: self.left - amount,
            top: self.top - amount,
            right: self.right + amount,
            bottom: self.bottom + amount,
        }
    }

    fn normalized(self, width: u32, height: u32) -> Self {
        let mut rect = Self::from_points(
            Point::new(self.left, self.top),
            Point::new(self.right, self.bottom),
        );
        rect.left = rect.left.clamp(0.0, width.saturating_sub(1) as f32);
        rect.top = rect.top.clamp(0.0, height.saturating_sub(1) as f32);
        rect.right = rect.right.clamp(rect.left, width.saturating_sub(1) as f32);
        rect.bottom = rect.bottom.clamp(rect.top, height.saturating_sub(1) as f32);
        rect
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ElementId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AnnotationTool {
    Select,
    Pen,
    Line,
    Arrow,
    Rectangle,
    Ellipse,
    Diamond,
    Highlighter,
    SerialNumber,
    Text,
    Mosaic,
    Blur,
    Eraser,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TextAlignment {
    Left,
    Center,
    Right,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ElementStyle {
    pub stroke: RgbaColor,
    pub fill: RgbaColor,
    pub text: RgbaColor,
    pub stroke_width: f32,
    pub opacity: f32,
    pub brush_size: f32,
    pub effect_strength: f32,
    pub font_size: f32,
    pub bold: bool,
    pub alignment: TextAlignment,
}

impl ElementStyle {
    pub fn for_tool(tool: AnnotationTool, color: RgbaColor, stroke_width: f32) -> Self {
        let mut style = Self {
            stroke: color,
            fill: RgbaColor::TRANSPARENT,
            text: color,
            stroke_width: stroke_width.max(MIN_STROKE_WIDTH),
            opacity: 1.0,
            brush_size: stroke_width.max(18.0),
            effect_strength: 0.55,
            font_size: 24.0,
            bold: false,
            alignment: TextAlignment::Left,
        };
        match tool {
            AnnotationTool::Highlighter => {
                style.opacity = 0.35;
                style.stroke_width = stroke_width.max(18.0);
            }
            AnnotationTool::SerialNumber => {
                style.fill = color;
                style.stroke = RgbaColor::WHITE;
                style.text = RgbaColor::WHITE;
                style.font_size = 18.0;
            }
            AnnotationTool::Text => {
                style.text = color;
                style.stroke = RgbaColor::TRANSPARENT;
                style.font_size = 24.0;
            }
            AnnotationTool::Mosaic | AnnotationTool::Blur | AnnotationTool::Eraser => {
                style.brush_size = stroke_width.max(22.0);
            }
            _ => {}
        }
        style
    }
}

impl Default for ElementStyle {
    fn default() -> Self {
        Self::for_tool(AnnotationTool::Pen, RgbaColor::RED, 4.0)
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct StylePatch {
    pub stroke: Option<RgbaColor>,
    pub fill: Option<RgbaColor>,
    pub text: Option<RgbaColor>,
    pub stroke_width: Option<f32>,
    pub opacity: Option<f32>,
    pub brush_size: Option<f32>,
    pub effect_strength: Option<f32>,
    pub font_size: Option<f32>,
    pub bold: Option<bool>,
    pub alignment: Option<TextAlignment>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ElementKind {
    Freehand {
        points: Vec<Point>,
        highlighter: bool,
    },
    Line {
        start: Point,
        end: Point,
        arrow: bool,
    },
    Shape {
        start: Point,
        end: Point,
        shape: AnnotationTool,
    },
    SerialNumber {
        center: Point,
        number: u32,
    },
    Text {
        bounds: Rect,
        content: String,
    },
    EffectPath {
        points: Vec<Point>,
        blur: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AnnotationElement {
    pub id: ElementId,
    pub kind: ElementKind,
    pub style: ElementStyle,
}

impl AnnotationElement {
    pub fn bounds(&self) -> Rect {
        match &self.kind {
            ElementKind::Freehand { points, .. } | ElementKind::EffectPath { points, .. } => {
                bounds_for_points(points)
                    .unwrap_or(Rect::from_points(
                        Point::new(0.0, 0.0),
                        Point::new(0.0, 0.0),
                    ))
                    .expanded(self.style.stroke_width.max(self.style.brush_size) / 2.0)
            }
            ElementKind::Line { start, end, .. } | ElementKind::Shape { start, end, .. } => {
                Rect::from_points(*start, *end).expanded(self.style.stroke_width / 2.0)
            }
            ElementKind::SerialNumber { center, .. } => {
                let radius = (self.style.font_size * 0.9).clamp(13.0, 36.0);
                Rect::from_points(
                    Point::new(center.x - radius, center.y - radius),
                    Point::new(center.x + radius, center.y + radius),
                )
            }
            ElementKind::Text { bounds, .. } => *bounds,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OcrBlock {
    pub id: ElementId,
    pub points: [Point; 4],
    pub text: String,
    pub box_score: f32,
    pub text_score: f32,
}

impl OcrBlock {
    pub fn bounds(&self) -> Rect {
        bounds_for_points(&self.points).unwrap_or(Rect::from_points(
            Point::new(0.0, 0.0),
            Point::new(0.0, 0.0),
        ))
    }

    pub fn height(&self) -> f32 {
        self.points[0].distance(self.points[3]).max(1.0)
    }

    pub fn width(&self) -> f32 {
        self.points[0].distance(self.points[1]).max(1.0)
    }

    pub fn angle_radians(&self) -> f32 {
        let edge = Point::new(
            self.points[1].x - self.points[0].x,
            self.points[1].y - self.points[0].y,
        );
        edge.y.atan2(edge.x)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OcrLayerStyle {
    pub visible: bool,
    pub above_annotations: bool,
    pub manual_text_color: Option<RgbaColor>,
    pub opacity: f32,
    pub blur_strength: f32,
}

impl Default for OcrLayerStyle {
    fn default() -> Self {
        Self {
            visible: true,
            above_annotations: false,
            manual_text_color: None,
            opacity: 1.0,
            blur_strength: 1.0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionHandle {
    None,
    Move,
    TopLeft,
    Top,
    TopRight,
    Right,
    BottomRight,
    Bottom,
    BottomLeft,
    Left,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayerCommand {
    Forward,
    Backward,
    Front,
    Back,
}

#[derive(Debug, Clone, PartialEq)]
struct DocumentState {
    elements: Vec<AnnotationElement>,
    ocr_blocks: Vec<OcrBlock>,
    ocr_style: OcrLayerStyle,
    next_element_id: u64,
    next_serial_number: u32,
}

#[derive(Debug, Clone)]
enum ActiveOperation {
    Draw {
        baseline: DocumentState,
        element: AnnotationElement,
    },
    Transform {
        baseline: DocumentState,
        original: AnnotationElement,
        handle: SelectionHandle,
        start_pointer: Point,
        start_bounds: Rect,
    },
}

#[derive(Debug, Clone)]
pub struct AnnotationDocument {
    width: u32,
    height: u32,
    original: Vec<u8>,
    content_pixels: Vec<u8>,
    preview_pixels: Vec<u8>,
    state: DocumentState,
    undo: VecDeque<DocumentState>,
    redo: Vec<DocumentState>,
    active: Option<ActiveOperation>,
    active_base_pixels: Option<Vec<u8>>,
    selected: Option<ElementId>,
    selected_ocr: Option<ElementId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AnnotationError {
    InvalidDimensions,
    InvalidPixelLength { expected: usize, actual: usize },
}

impl fmt::Display for AnnotationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidDimensions => write!(formatter, "标注画布尺寸无效"),
            Self::InvalidPixelLength { expected, actual } => {
                write!(
                    formatter,
                    "标注像素长度无效：应为 {expected}，实际为 {actual}"
                )
            }
        }
    }
}

impl std::error::Error for AnnotationError {}

impl AnnotationDocument {
    pub fn new(width: u32, height: u32, pixels: Vec<u8>) -> Result<Self, AnnotationError> {
        if width == 0 || height == 0 {
            return Err(AnnotationError::InvalidDimensions);
        }
        let expected =
            expected_pixel_len(width, height).ok_or(AnnotationError::InvalidDimensions)?;
        if pixels.len() != expected {
            return Err(AnnotationError::InvalidPixelLength {
                expected,
                actual: pixels.len(),
            });
        }
        let state = DocumentState {
            elements: Vec::new(),
            ocr_blocks: Vec::new(),
            ocr_style: OcrLayerStyle::default(),
            next_element_id: 1,
            next_serial_number: 1,
        };
        Ok(Self {
            width,
            height,
            original: pixels.clone(),
            content_pixels: pixels.clone(),
            preview_pixels: pixels,
            state,
            undo: VecDeque::new(),
            redo: Vec::new(),
            active: None,
            active_base_pixels: None,
            selected: None,
            selected_ocr: None,
        })
    }

    pub const fn width(&self) -> u32 {
        self.width
    }

    pub const fn height(&self) -> u32 {
        self.height
    }

    pub fn pixels(&self) -> &[u8] {
        &self.content_pixels
    }

    pub fn preview_pixels(&self) -> &[u8] {
        &self.preview_pixels
    }

    pub fn elements(&self) -> &[AnnotationElement] {
        &self.state.elements
    }

    pub fn ocr_blocks(&self) -> &[OcrBlock] {
        &self.state.ocr_blocks
    }

    pub fn ocr_style(&self) -> &OcrLayerStyle {
        &self.state.ocr_style
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    pub fn selected_id(&self) -> Option<ElementId> {
        self.selected
    }

    pub fn selected_ocr_id(&self) -> Option<ElementId> {
        self.selected_ocr
    }

    pub fn selected_bounds(&self) -> Option<Rect> {
        self.selected
            .and_then(|id| self.element(id))
            .map(AnnotationElement::bounds)
            .or_else(|| {
                self.selected_ocr
                    .and_then(|id| self.ocr_block(id))
                    .map(OcrBlock::bounds)
            })
    }

    pub fn selected_ocr_text(&self) -> Option<&str> {
        self.selected_ocr
            .and_then(|id| self.ocr_block(id))
            .map(|block| block.text.as_str())
    }

    pub fn ocr_plain_text(&self) -> String {
        normalized_ocr_text(&self.state.ocr_blocks)
    }

    pub fn set_ocr_blocks(&mut self, mut blocks: Vec<OcrBlock>) {
        self.cancel_active();
        let baseline = self.state.clone();
        for (index, block) in blocks.iter_mut().enumerate() {
            block.id = ElementId(OCR_ID_BASE.saturating_add(index as u64));
        }
        self.state.ocr_blocks = blocks;
        self.selected = None;
        self.selected_ocr = None;
        self.record_state_change(baseline);
    }

    pub fn configure_ocr_style(&mut self, style: OcrLayerStyle) {
        self.state.ocr_style = style;
        self.refresh();
    }

    pub fn configure_next_serial_number(&mut self, number: u32) {
        self.state.next_serial_number = number.max(1);
    }

    pub fn next_serial_number(&self) -> u32 {
        self.state.next_serial_number
    }

    pub fn selected_serial_number(&self) -> Option<u32> {
        self.selected
            .and_then(|id| self.element(id))
            .and_then(|element| match &element.kind {
                ElementKind::SerialNumber { number, .. } => Some(*number),
                _ => None,
            })
    }

    pub fn update_serial_number(&mut self, number: u32) -> bool {
        let number = number.max(1);
        if let Some(id) = self.selected {
            let baseline = self.state.clone();
            let Some(element) = self
                .state
                .elements
                .iter_mut()
                .find(|element| element.id == id)
            else {
                return false;
            };
            let ElementKind::SerialNumber {
                number: selected, ..
            } = &mut element.kind
            else {
                return false;
            };
            if *selected == number {
                return false;
            }
            *selected = number;
            self.record_state_change(baseline);
            return true;
        }
        if self.state.next_serial_number == number {
            return false;
        }
        self.state.next_serial_number = number;
        true
    }

    pub fn clear_ocr(&mut self) {
        if self.state.ocr_blocks.is_empty() {
            return;
        }
        let baseline = self.state.clone();
        self.state.ocr_blocks.clear();
        self.selected_ocr = None;
        self.record_state_change(baseline);
    }

    pub fn set_ocr_visible(&mut self, visible: bool) {
        if self.state.ocr_style.visible == visible {
            return;
        }
        let baseline = self.state.clone();
        self.state.ocr_style.visible = visible;
        self.record_state_change(baseline);
    }

    pub fn set_ocr_style(&mut self, style: OcrLayerStyle) {
        if self.state.ocr_style == style {
            return;
        }
        let baseline = self.state.clone();
        self.state.ocr_style = style;
        self.record_state_change(baseline);
    }

    pub fn begin(
        &mut self,
        tool: AnnotationTool,
        point: Point,
        color: RgbaColor,
        stroke_width: f32,
    ) {
        self.begin_with_style(
            tool,
            point,
            ElementStyle::for_tool(tool, color, stroke_width),
            false,
            false,
        );
    }

    pub fn begin_with_style(
        &mut self,
        tool: AnnotationTool,
        point: Point,
        style: ElementStyle,
        preserve_aspect: bool,
        centered: bool,
    ) {
        self.cancel_active();
        let point = self.clamp_point(point);
        match tool {
            AnnotationTool::Select => {
                self.begin_selection_interaction(point, preserve_aspect, centered);
            }
            AnnotationTool::Eraser => {
                if let Some(id) = self.hit_element(point) {
                    let baseline = self.state.clone();
                    self.state.elements.retain(|element| element.id != id);
                    self.selected = None;
                    self.record_state_change(baseline);
                }
            }
            _ => {
                let id = ElementId(self.state.next_element_id);
                self.state.next_element_id = self.state.next_element_id.saturating_add(1);
                let kind = match tool {
                    AnnotationTool::Pen | AnnotationTool::Highlighter => ElementKind::Freehand {
                        points: vec![point],
                        highlighter: tool == AnnotationTool::Highlighter,
                    },
                    AnnotationTool::Line | AnnotationTool::Arrow => ElementKind::Line {
                        start: point,
                        end: point,
                        arrow: tool == AnnotationTool::Arrow,
                    },
                    AnnotationTool::Rectangle
                    | AnnotationTool::Ellipse
                    | AnnotationTool::Diamond => ElementKind::Shape {
                        start: point,
                        end: point,
                        shape: tool,
                    },
                    AnnotationTool::SerialNumber => ElementKind::SerialNumber {
                        center: point,
                        number: self.state.next_serial_number,
                    },
                    AnnotationTool::Text => ElementKind::Text {
                        bounds: Rect {
                            left: point.x,
                            top: point.y,
                            right: (point.x + 220.0).min(self.width as f32),
                            bottom: (point.y + style.font_size * 1.8).min(self.height as f32),
                        },
                        content: "文本".to_string(),
                    },
                    AnnotationTool::Mosaic | AnnotationTool::Blur => ElementKind::EffectPath {
                        points: vec![point],
                        blur: tool == AnnotationTool::Blur,
                    },
                    AnnotationTool::Select | AnnotationTool::Eraser => return,
                };
                self.active = Some(ActiveOperation::Draw {
                    baseline: self.state.clone(),
                    element: AnnotationElement { id, kind, style },
                });
                self.active_base_pixels =
                    (!self.state.ocr_style.above_annotations).then(|| self.content_pixels.clone());
                self.selected = None;
                self.selected_ocr = None;
                self.refresh_interaction();
            }
        }
    }

    pub fn update(&mut self, point: Point) {
        self.update_with_modifiers(point, false, false);
    }

    pub fn update_with_modifiers(&mut self, point: Point, preserve_aspect: bool, centered: bool) {
        let point = self.clamp_point(point);
        match self.active.as_mut() {
            Some(ActiveOperation::Draw { element, .. }) => match &mut element.kind {
                ElementKind::Freehand { points, .. } | ElementKind::EffectPath { points, .. } => {
                    if points.last().copied() != Some(point) {
                        points.push(point);
                    }
                }
                ElementKind::Line { end, .. } | ElementKind::Shape { end, .. } => *end = point,
                ElementKind::SerialNumber { center, .. } => *center = point,
                ElementKind::Text { bounds, .. } => {
                    bounds.right = point.x.max(bounds.left + MIN_ELEMENT_SIZE);
                    bounds.bottom = point.y.max(bounds.top + MIN_ELEMENT_SIZE);
                }
            },
            Some(ActiveOperation::Transform {
                original,
                handle,
                start_pointer,
                start_bounds,
                ..
            }) => {
                let transformed = transform_rect(
                    *start_bounds,
                    *handle,
                    *start_pointer,
                    point,
                    preserve_aspect,
                    centered,
                    self.width,
                    self.height,
                );
                let id = original.id;
                if let Some(element) = self
                    .state
                    .elements
                    .iter_mut()
                    .find(|element| element.id == id)
                {
                    *element = transform_element(original, *start_bounds, transformed);
                }
            }
            None => {}
        }
        self.refresh_interaction();
    }

    pub fn commit(&mut self, point: Point) -> bool {
        self.commit_with_modifiers(point, false, false)
    }

    pub fn commit_with_modifiers(
        &mut self,
        point: Point,
        preserve_aspect: bool,
        centered: bool,
    ) -> bool {
        self.update_with_modifiers(point, preserve_aspect, centered);
        let Some(active) = self.active.take() else {
            return false;
        };
        self.active_base_pixels = None;
        match active {
            ActiveOperation::Draw { baseline, element } => {
                if !element_has_content(&element) {
                    self.state = baseline;
                    self.refresh();
                    return false;
                }
                if matches!(element.kind, ElementKind::SerialNumber { .. }) {
                    self.state.next_serial_number = self.state.next_serial_number.saturating_add(1);
                }
                self.selected = Some(element.id);
                self.state.elements.push(element);
                self.record_state_change(baseline);
            }
            ActiveOperation::Transform {
                baseline, original, ..
            } => {
                let changed = self
                    .state
                    .elements
                    .iter()
                    .find(|element| element.id == original.id)
                    != Some(&original);
                if !changed {
                    self.state = baseline;
                    self.refresh();
                    return false;
                }
                self.record_state_change(baseline);
            }
        }
        true
    }

    pub fn cancel_active(&mut self) {
        if let Some(active) = self.active.take() {
            self.active_base_pixels = None;
            self.state = match active {
                ActiveOperation::Draw { baseline, .. }
                | ActiveOperation::Transform { baseline, .. } => baseline,
            };
            self.refresh();
        }
    }

    pub fn undo(&mut self) -> bool {
        self.cancel_active();
        let Some(previous) = self.undo.pop_back() else {
            return false;
        };
        self.redo.push(std::mem::replace(&mut self.state, previous));
        self.selected = None;
        self.selected_ocr = None;
        self.refresh();
        true
    }

    pub fn redo(&mut self) -> bool {
        self.cancel_active();
        let Some(next) = self.redo.pop() else {
            return false;
        };
        let current = std::mem::replace(&mut self.state, next);
        self.push_undo(current);
        self.selected = None;
        self.selected_ocr = None;
        self.refresh();
        true
    }

    pub fn sample(&self, point: Point) -> RgbaColor {
        self.sample_from(&self.content_pixels, point)
    }

    pub fn sample_original(&self, point: Point) -> RgbaColor {
        self.sample_from(&self.original, point)
    }

    fn sample_from(&self, pixels: &[u8], point: Point) -> RgbaColor {
        let point = self.clamp_point(point);
        let index = pixel_index(self.width, point.x.round() as u32, point.y.round() as u32);
        RgbaColor::new(
            pixels[index],
            pixels[index + 1],
            pixels[index + 2],
            pixels[index + 3],
        )
    }

    pub fn select_at(&mut self, point: Point) -> bool {
        let point = self.clamp_point(point);
        self.selected = self.hit_element(point);
        self.selected_ocr = if self.selected.is_none() {
            self.hit_ocr(point)
        } else {
            None
        };
        self.refresh_preview();
        self.selected.is_some() || self.selected_ocr.is_some()
    }

    pub fn clear_selection(&mut self) {
        self.selected = None;
        self.selected_ocr = None;
        self.refresh_preview();
    }

    pub fn selection_handle_at(&self, point: Point) -> SelectionHandle {
        self.selected_bounds()
            .map(|bounds| hit_selection_handle(bounds, point))
            .unwrap_or(SelectionHandle::None)
    }

    pub fn delete_selected(&mut self) -> bool {
        if let Some(id) = self.selected.take() {
            let baseline = self.state.clone();
            self.state.elements.retain(|element| element.id != id);
            self.record_state_change(baseline);
            return true;
        }
        false
    }

    pub fn apply_layer_command(&mut self, command: LayerCommand) -> bool {
        if let Some(id) = self.selected {
            let Some(index) = self
                .state
                .elements
                .iter()
                .position(|element| element.id == id)
            else {
                return false;
            };
            let destination = match command {
                LayerCommand::Forward => (index + 1).min(self.state.elements.len() - 1),
                LayerCommand::Backward => index.saturating_sub(1),
                LayerCommand::Front => self.state.elements.len() - 1,
                LayerCommand::Back => 0,
            };
            if destination == index {
                return false;
            }
            let baseline = self.state.clone();
            let element = self.state.elements.remove(index);
            self.state.elements.insert(destination, element);
            self.record_state_change(baseline);
            return true;
        }
        if self.selected_ocr.is_some() {
            let above = matches!(command, LayerCommand::Forward | LayerCommand::Front);
            if above == self.state.ocr_style.above_annotations {
                return false;
            }
            let baseline = self.state.clone();
            self.state.ocr_style.above_annotations = above;
            self.record_state_change(baseline);
            return true;
        }
        false
    }

    pub fn update_selected_style(&mut self, patch: &StylePatch) -> bool {
        let Some(id) = self.selected else {
            return false;
        };
        let baseline = self.state.clone();
        let Some(element) = self
            .state
            .elements
            .iter_mut()
            .find(|element| element.id == id)
        else {
            return false;
        };
        apply_style_patch(&mut element.style, patch);
        if self.state == baseline {
            return false;
        }
        self.record_state_change(baseline);
        true
    }

    pub fn update_selected_text(&mut self, text: String) -> bool {
        let Some(id) = self.selected else {
            return false;
        };
        let baseline = self.state.clone();
        let Some(element) = self
            .state
            .elements
            .iter_mut()
            .find(|element| element.id == id)
        else {
            return false;
        };
        let ElementKind::Text { content, .. } = &mut element.kind else {
            return false;
        };
        if *content == text {
            return false;
        }
        *content = text;
        self.record_state_change(baseline);
        true
    }

    pub fn selected_style(&self) -> Option<&ElementStyle> {
        self.selected
            .and_then(|id| self.element(id))
            .map(|element| &element.style)
    }

    pub fn selected_tool(&self) -> Option<AnnotationTool> {
        self.selected
            .and_then(|id| self.element(id))
            .map(|element| match &element.kind {
                ElementKind::Freehand { highlighter, .. } => {
                    if *highlighter {
                        AnnotationTool::Highlighter
                    } else {
                        AnnotationTool::Pen
                    }
                }
                ElementKind::Line { arrow, .. } => {
                    if *arrow {
                        AnnotationTool::Arrow
                    } else {
                        AnnotationTool::Line
                    }
                }
                ElementKind::Shape { shape, .. } => *shape,
                ElementKind::SerialNumber { .. } => AnnotationTool::SerialNumber,
                ElementKind::Text { .. } => AnnotationTool::Text,
                ElementKind::EffectPath { blur, .. } => {
                    if *blur {
                        AnnotationTool::Blur
                    } else {
                        AnnotationTool::Mosaic
                    }
                }
            })
    }

    pub fn selected_text(&self) -> Option<&str> {
        self.selected
            .and_then(|id| self.element(id))
            .and_then(|element| match &element.kind {
                ElementKind::Text { content, .. } => Some(content.as_str()),
                _ => None,
            })
    }

    fn begin_selection_interaction(
        &mut self,
        point: Point,
        _preserve_aspect: bool,
        _centered: bool,
    ) {
        let current_handle = self
            .selected
            .and_then(|id| self.element(id))
            .map(|element| hit_selection_handle(element.bounds(), point))
            .unwrap_or(SelectionHandle::None);
        let (id, handle) = if current_handle != SelectionHandle::None {
            (self.selected, current_handle)
        } else {
            let id = self.hit_element(point);
            (
                id,
                if id.is_some() {
                    SelectionHandle::Move
                } else {
                    SelectionHandle::None
                },
            )
        };
        self.selected = id;
        self.selected_ocr = if id.is_none() {
            self.hit_ocr(point)
        } else {
            None
        };
        let Some(id) = id else {
            self.refresh_preview();
            return;
        };
        let Some(original) = self.element(id).cloned() else {
            return;
        };
        self.active = Some(ActiveOperation::Transform {
            baseline: self.state.clone(),
            start_bounds: original.bounds(),
            original,
            handle,
            start_pointer: point,
        });
        if !self.state.ocr_style.above_annotations {
            let mut base_state = self.state.clone();
            base_state.elements.retain(|element| element.id != id);
            self.active_base_pixels = Some(self.render_state(&base_state, None));
        } else {
            self.active_base_pixels = None;
        }
        self.refresh_interaction();
    }

    fn element(&self, id: ElementId) -> Option<&AnnotationElement> {
        self.state.elements.iter().find(|element| element.id == id)
    }

    fn ocr_block(&self, id: ElementId) -> Option<&OcrBlock> {
        self.state.ocr_blocks.iter().find(|block| block.id == id)
    }

    fn hit_element(&self, point: Point) -> Option<ElementId> {
        self.state
            .elements
            .iter()
            .rev()
            .find(|element| hit_element(element, point))
            .map(|element| element.id)
    }

    fn hit_ocr(&self, point: Point) -> Option<ElementId> {
        if !self.state.ocr_style.visible {
            return None;
        }
        self.state
            .ocr_blocks
            .iter()
            .rev()
            .find(|block| point_in_quad(point, block.points))
            .map(|block| block.id)
    }

    fn clamp_point(&self, point: Point) -> Point {
        Point {
            x: point.x.clamp(0.0, self.width.saturating_sub(1) as f32),
            y: point.y.clamp(0.0, self.height.saturating_sub(1) as f32),
        }
    }

    fn record_state_change(&mut self, previous: DocumentState) {
        self.push_undo(previous);
        self.redo.clear();
        self.refresh();
    }

    fn push_undo(&mut self, state: DocumentState) {
        if self.undo.len() == MAX_HISTORY {
            self.undo.pop_front();
        }
        self.undo.push_back(state);
    }

    fn refresh(&mut self) {
        self.content_pixels = self.render_state(&self.state, self.active_element());
        self.refresh_preview();
    }

    fn refresh_interaction(&mut self) {
        let Some(base) = self.active_base_pixels.as_ref() else {
            self.refresh();
            return;
        };
        self.preview_pixels.clone_from(base);
        let element = match self.active.as_ref() {
            Some(ActiveOperation::Draw { element, .. }) => Some(element),
            Some(ActiveOperation::Transform { original, .. }) => self.element(original.id),
            None => None,
        };
        if let Some(element) = element.cloned() {
            render_element(&mut self.preview_pixels, self.width, self.height, &element);
        }
        self.draw_selection_preview();
    }

    fn refresh_preview(&mut self) {
        self.preview_pixels.clone_from(&self.content_pixels);
        self.draw_selection_preview();
    }

    fn draw_selection_preview(&mut self) {
        let selected_bounds = self
            .selected
            .and_then(|id| self.element(id))
            .map(AnnotationElement::bounds);
        if let Some(bounds) = selected_bounds {
            draw_selection(&mut self.preview_pixels, self.width, self.height, bounds);
        }
        let selected_ocr_points = self
            .selected_ocr
            .and_then(|id| self.ocr_block(id))
            .map(|block| block.points);
        if let Some(points) = selected_ocr_points {
            draw_quad_outline(
                &mut self.preview_pixels,
                self.width,
                self.height,
                points,
                RgbaColor::new(25, 190, 180, 255),
                2.0,
            );
        }
    }

    fn active_element(&self) -> Option<&AnnotationElement> {
        match self.active.as_ref() {
            Some(ActiveOperation::Draw { element, .. }) => Some(element),
            Some(ActiveOperation::Transform { original, .. }) => self.element(original.id),
            None => None,
        }
    }

    fn render_state(
        &self,
        state: &DocumentState,
        active_element: Option<&AnnotationElement>,
    ) -> Vec<u8> {
        let mut pixels = self.original.clone();
        if state.ocr_style.visible && !state.ocr_style.above_annotations {
            render_ocr_layer(
                &mut pixels,
                &self.original,
                self.width,
                self.height,
                &state.ocr_blocks,
                &state.ocr_style,
            );
        }
        for element in &state.elements {
            render_element(&mut pixels, self.width, self.height, element);
        }
        if let Some(element) = active_element {
            render_element(&mut pixels, self.width, self.height, element);
        }
        if state.ocr_style.visible && state.ocr_style.above_annotations {
            let source = pixels.clone();
            render_ocr_layer(
                &mut pixels,
                &source,
                self.width,
                self.height,
                &state.ocr_blocks,
                &state.ocr_style,
            );
        }
        pixels
    }
}

fn apply_style_patch(style: &mut ElementStyle, patch: &StylePatch) {
    if let Some(value) = patch.stroke {
        style.stroke = value;
    }
    if let Some(value) = patch.fill {
        style.fill = value;
    }
    if let Some(value) = patch.text {
        style.text = value;
    }
    if let Some(value) = patch.stroke_width {
        style.stroke_width = value.clamp(1.0, 64.0);
    }
    if let Some(value) = patch.opacity {
        style.opacity = value.clamp(0.0, 1.0);
    }
    if let Some(value) = patch.brush_size {
        style.brush_size = value.clamp(2.0, 256.0);
    }
    if let Some(value) = patch.effect_strength {
        style.effect_strength = value.clamp(0.05, 1.0);
    }
    if let Some(value) = patch.font_size {
        style.font_size = value.clamp(8.0, 160.0);
    }
    if let Some(value) = patch.bold {
        style.bold = value;
    }
    if let Some(value) = patch.alignment {
        style.alignment = value;
    }
}

fn element_has_content(element: &AnnotationElement) -> bool {
    match &element.kind {
        ElementKind::Freehand { points, .. } | ElementKind::EffectPath { points, .. } => {
            !points.is_empty()
        }
        ElementKind::Line { start, end, .. } | ElementKind::Shape { start, end, .. } => {
            start.distance(*end) >= 1.0
        }
        ElementKind::SerialNumber { .. } => true,
        ElementKind::Text { bounds, content } => {
            bounds.width() >= MIN_ELEMENT_SIZE
                && bounds.height() >= MIN_ELEMENT_SIZE
                && !content.is_empty()
        }
    }
}

fn hit_element(element: &AnnotationElement, point: Point) -> bool {
    let tolerance = element.style.stroke_width.max(8.0);
    match &element.kind {
        ElementKind::Freehand { points, .. } | ElementKind::EffectPath { points, .. } => points
            .windows(2)
            .any(|pair| distance_to_segment(point, pair[0], pair[1]) <= tolerance),
        ElementKind::Line { start, end, .. } => {
            distance_to_segment(point, *start, *end) <= tolerance
        }
        ElementKind::Shape { .. } | ElementKind::SerialNumber { .. } | ElementKind::Text { .. } => {
            element.bounds().expanded(4.0).contains(point)
        }
    }
}

fn distance_to_segment(point: Point, start: Point, end: Point) -> f32 {
    let dx = end.x - start.x;
    let dy = end.y - start.y;
    let length_squared = dx * dx + dy * dy;
    if length_squared <= f32::EPSILON {
        return point.distance(start);
    }
    let progress =
        (((point.x - start.x) * dx + (point.y - start.y) * dy) / length_squared).clamp(0.0, 1.0);
    point.distance(Point::new(start.x + dx * progress, start.y + dy * progress))
}

fn hit_selection_handle(bounds: Rect, point: Point) -> SelectionHandle {
    let positions = [
        (
            SelectionHandle::TopLeft,
            Point::new(bounds.left, bounds.top),
        ),
        (
            SelectionHandle::Top,
            Point::new((bounds.left + bounds.right) / 2.0, bounds.top),
        ),
        (
            SelectionHandle::TopRight,
            Point::new(bounds.right, bounds.top),
        ),
        (
            SelectionHandle::Right,
            Point::new(bounds.right, (bounds.top + bounds.bottom) / 2.0),
        ),
        (
            SelectionHandle::BottomRight,
            Point::new(bounds.right, bounds.bottom),
        ),
        (
            SelectionHandle::Bottom,
            Point::new((bounds.left + bounds.right) / 2.0, bounds.bottom),
        ),
        (
            SelectionHandle::BottomLeft,
            Point::new(bounds.left, bounds.bottom),
        ),
        (
            SelectionHandle::Left,
            Point::new(bounds.left, (bounds.top + bounds.bottom) / 2.0),
        ),
    ];
    positions
        .into_iter()
        .find(|(_, position)| position.distance(point) <= HANDLE_RADIUS * 1.6)
        .map(|(handle, _)| handle)
        .unwrap_or_else(|| {
            if bounds.contains(point) {
                SelectionHandle::Move
            } else {
                SelectionHandle::None
            }
        })
}

#[allow(clippy::too_many_arguments)]
fn transform_rect(
    bounds: Rect,
    handle: SelectionHandle,
    start_pointer: Point,
    pointer: Point,
    preserve_aspect: bool,
    centered: bool,
    canvas_width: u32,
    canvas_height: u32,
) -> Rect {
    let dx = pointer.x - start_pointer.x;
    let dy = pointer.y - start_pointer.y;
    if handle == SelectionHandle::Move {
        let width = bounds.width();
        let height = bounds.height();
        let left = (bounds.left + dx).clamp(0.0, canvas_width as f32 - width);
        let top = (bounds.top + dy).clamp(0.0, canvas_height as f32 - height);
        return Rect {
            left,
            top,
            right: left + width,
            bottom: top + height,
        };
    }

    let mut result = bounds;
    if matches!(
        handle,
        SelectionHandle::TopLeft | SelectionHandle::Left | SelectionHandle::BottomLeft
    ) {
        result.left += dx;
        if centered {
            result.right -= dx;
        }
    }
    if matches!(
        handle,
        SelectionHandle::TopRight | SelectionHandle::Right | SelectionHandle::BottomRight
    ) {
        result.right += dx;
        if centered {
            result.left -= dx;
        }
    }
    if matches!(
        handle,
        SelectionHandle::TopLeft | SelectionHandle::Top | SelectionHandle::TopRight
    ) {
        result.top += dy;
        if centered {
            result.bottom -= dy;
        }
    }
    if matches!(
        handle,
        SelectionHandle::BottomLeft | SelectionHandle::Bottom | SelectionHandle::BottomRight
    ) {
        result.bottom += dy;
        if centered {
            result.top -= dy;
        }
    }

    if preserve_aspect && bounds.height() > f32::EPSILON {
        let ratio = bounds.width() / bounds.height();
        let horizontal = matches!(handle, SelectionHandle::Left | SelectionHandle::Right);
        let vertical = matches!(handle, SelectionHandle::Top | SelectionHandle::Bottom);
        let (target_width, target_height) = if horizontal {
            (result.width().abs(), result.width().abs() / ratio)
        } else if vertical {
            (result.height().abs() * ratio, result.height().abs())
        } else {
            project_size_to_aspect(result.width().abs(), result.height().abs(), ratio)
        };
        if horizontal
            || (!vertical
                && matches!(
                    handle,
                    SelectionHandle::TopLeft
                        | SelectionHandle::TopRight
                        | SelectionHandle::BottomLeft
                        | SelectionHandle::BottomRight
                ))
        {
            let direction = if result.bottom >= result.top {
                1.0
            } else {
                -1.0
            };
            if matches!(
                handle,
                SelectionHandle::TopLeft | SelectionHandle::Top | SelectionHandle::TopRight
            ) {
                result.top = result.bottom - target_height * direction;
            } else if centered {
                let center = bounds.center().y;
                result.top = center - target_height / 2.0;
                result.bottom = center + target_height / 2.0;
            } else {
                result.bottom = result.top + target_height * direction;
            }
        }
        if vertical
            || (!horizontal
                && matches!(
                    handle,
                    SelectionHandle::TopLeft
                        | SelectionHandle::TopRight
                        | SelectionHandle::BottomLeft
                        | SelectionHandle::BottomRight
                ))
        {
            let direction = if result.right >= result.left {
                1.0
            } else {
                -1.0
            };
            if matches!(
                handle,
                SelectionHandle::TopLeft | SelectionHandle::Left | SelectionHandle::BottomLeft
            ) {
                result.left = result.right - target_width * direction;
            } else if centered {
                let center = bounds.center().x;
                result.left = center - target_width / 2.0;
                result.right = center + target_width / 2.0;
            } else {
                result.right = result.left + target_width * direction;
            }
        }
    }

    if result.width().abs() < MIN_ELEMENT_SIZE {
        result.right = result.left + MIN_ELEMENT_SIZE;
    }
    if result.height().abs() < MIN_ELEMENT_SIZE {
        result.bottom = result.top + MIN_ELEMENT_SIZE;
    }
    result.normalized(canvas_width, canvas_height)
}

fn project_size_to_aspect(raw_width: f32, raw_height: f32, aspect: f32) -> (f32, f32) {
    let aspect = if aspect.is_finite() && aspect > f32::EPSILON {
        aspect
    } else {
        1.0
    };
    let denominator = aspect * aspect + 1.0;
    let projected_height = (raw_width * aspect + raw_height) / denominator;
    (projected_height * aspect, projected_height)
}

fn transform_element(element: &AnnotationElement, from: Rect, to: Rect) -> AnnotationElement {
    let transform = |point: Point| {
        let normalized_x = if from.width().abs() <= f32::EPSILON {
            0.5
        } else {
            (point.x - from.left) / from.width()
        };
        let normalized_y = if from.height().abs() <= f32::EPSILON {
            0.5
        } else {
            (point.y - from.top) / from.height()
        };
        Point::new(
            to.left + normalized_x * to.width(),
            to.top + normalized_y * to.height(),
        )
    };
    let mut transformed = element.clone();
    match &mut transformed.kind {
        ElementKind::Freehand { points, .. } | ElementKind::EffectPath { points, .. } => {
            for point in points {
                *point = transform(*point);
            }
        }
        ElementKind::Line { start, end, .. } | ElementKind::Shape { start, end, .. } => {
            *start = transform(*start);
            *end = transform(*end);
        }
        ElementKind::SerialNumber { center, .. } => *center = transform(*center),
        ElementKind::Text { bounds, .. } => *bounds = to,
    }
    transformed
}

fn render_element(pixels: &mut [u8], width: u32, height: u32, element: &AnnotationElement) {
    let opacity = element.style.opacity.clamp(0.0, 1.0);
    let stroke = element.style.stroke.with_alpha_factor(opacity);
    let fill = element.style.fill.with_alpha_factor(opacity);
    let text = element.style.text.with_alpha_factor(opacity);
    match &element.kind {
        ElementKind::Freehand {
            points,
            highlighter,
        } => draw_polyline(
            pixels,
            width,
            height,
            points,
            if *highlighter {
                stroke.with_alpha_factor(0.45)
            } else {
                stroke
            },
            element.style.stroke_width,
        ),
        ElementKind::Line { start, end, arrow } => {
            if *arrow {
                draw_arrow(
                    pixels,
                    width,
                    height,
                    *start,
                    *end,
                    stroke,
                    element.style.stroke_width,
                );
            } else {
                draw_thick_line(
                    pixels,
                    width,
                    height,
                    *start,
                    *end,
                    stroke,
                    element.style.stroke_width,
                );
            }
        }
        ElementKind::Shape { start, end, shape } => {
            draw_shape(
                pixels,
                (width, height),
                (*start, *end),
                *shape,
                (stroke, fill),
                element.style.stroke_width,
            );
        }
        ElementKind::SerialNumber { center, number } => {
            draw_serial_number(pixels, width, height, *center, *number, &element.style)
        }
        ElementKind::Text { bounds, content } => draw_text_element(
            pixels,
            width,
            height,
            *bounds,
            content,
            &element.style,
            text,
        ),
        ElementKind::EffectPath { points, blur } => {
            let source = pixels.to_vec();
            if *blur {
                apply_blur_path(
                    pixels,
                    &source,
                    width,
                    height,
                    points,
                    element.style.brush_size,
                    element.style.effect_strength,
                );
            } else {
                apply_mosaic_path(
                    pixels,
                    &source,
                    width,
                    height,
                    points,
                    element.style.brush_size,
                    element.style.effect_strength,
                );
            }
        }
    }
}

fn render_ocr_layer(
    pixels: &mut [u8],
    source: &[u8],
    width: u32,
    height: u32,
    blocks: &[OcrBlock],
    style: &OcrLayerStyle,
) {
    for block in blocks {
        if block.text.trim().is_empty() || block.width() < 1.0 || block.height() < 1.0 {
            continue;
        }
        blur_ocr_block(pixels, source, width, height, block, style.blur_strength);
        let color = style
            .manual_text_color
            .unwrap_or_else(|| automatic_text_color(source, width, height, block))
            .with_alpha_factor(style.opacity);
        draw_ocr_text(pixels, width, height, block, color);
    }
}

fn blur_ocr_block(
    pixels: &mut [u8],
    source: &[u8],
    width: u32,
    height: u32,
    block: &OcrBlock,
    strength: f32,
) {
    let sigma = (block.height() * 0.10 * strength.clamp(0.2, 3.0)).clamp(1.5, 12.0);
    let padding = (sigma * 2.5).ceil() as i32;
    let bounds = block.bounds();
    let left = (bounds.left.floor() as i32 - padding).max(0) as u32;
    let top = (bounds.top.floor() as i32 - padding).max(0) as u32;
    let right = (bounds.right.ceil() as i32 + padding).min(width as i32 - 1) as u32;
    let bottom = (bounds.bottom.ceil() as i32 + padding).min(height as i32 - 1) as u32;
    let crop_width = right.saturating_sub(left).saturating_add(1);
    let crop_height = bottom.saturating_sub(top).saturating_add(1);
    let mut crop = RgbaImage::new(crop_width, crop_height);
    for crop_y in 0..crop_height {
        for crop_x in 0..crop_width {
            let index = pixel_index(width, left + crop_x, top + crop_y);
            crop.put_pixel(
                crop_x,
                crop_y,
                Rgba([
                    source[index],
                    source[index + 1],
                    source[index + 2],
                    source[index + 3],
                ]),
            );
        }
    }
    let blurred = gaussian_blur_f32(&crop, sigma);
    for y in bounds.top.floor().max(0.0) as u32
        ..=bounds.bottom.ceil().min(height.saturating_sub(1) as f32) as u32
    {
        for x in bounds.left.floor().max(0.0) as u32
            ..=bounds.right.ceil().min(width.saturating_sub(1) as f32) as u32
        {
            if !point_in_quad(Point::new(x as f32 + 0.5, y as f32 + 0.5), block.points) {
                continue;
            }
            let color = blurred.get_pixel(x - left, y - top).0;
            let index = pixel_index(width, x, y);
            pixels[index..index + 4].copy_from_slice(&color);
        }
    }
}

fn automatic_text_color(source: &[u8], width: u32, height: u32, block: &OcrBlock) -> RgbaColor {
    let bounds = block.bounds();
    let mut luminance = 0.0;
    let mut count = 0_u32;
    let step = ((block.width().min(block.height()) / 8.0).floor() as u32).max(1);
    for y in (bounds.top.max(0.0) as u32
        ..=bounds.bottom.min(height.saturating_sub(1) as f32) as u32)
        .step_by(step as usize)
    {
        for x in (bounds.left.max(0.0) as u32
            ..=bounds.right.min(width.saturating_sub(1) as f32) as u32)
            .step_by(step as usize)
        {
            if !point_in_quad(Point::new(x as f32 + 0.5, y as f32 + 0.5), block.points) {
                continue;
            }
            let index = pixel_index(width, x, y);
            luminance += RgbaColor::new(source[index], source[index + 1], source[index + 2], 255)
                .relative_luminance();
            count += 1;
        }
    }
    if count > 0 && luminance / (count as f32) < 0.42 {
        RgbaColor::WHITE
    } else {
        RgbaColor::BLACK
    }
}

fn draw_ocr_text(pixels: &mut [u8], width: u32, height: u32, block: &OcrBlock, color: RgbaColor) {
    let Some(font) = system_font(false) else {
        return;
    };
    let box_width = block.width().max(1.0);
    let box_height = block.height().max(1.0);
    let mut font_size = (box_height * 0.78).clamp(8.0, 120.0);
    while font_size > 8.0 {
        let (text_width, _) = text_size(font_size, font, block.text.as_str());
        if text_width as f32 <= box_width * 0.96 {
            break;
        }
        font_size -= 1.0;
    }
    let diagonal = (box_width * box_width + box_height * box_height)
        .sqrt()
        .ceil()
        .max(4.0) as u32
        + 8;
    let mut text_image = RgbaImage::new(diagonal, diagonal);
    let (text_width, text_height) = text_size(font_size, font, block.text.as_str());
    let text_x = (diagonal.saturating_sub(text_width) / 2) as i32;
    let text_y = (diagonal.saturating_sub(text_height) / 2) as i32;
    draw_text_mut(
        &mut text_image,
        Rgba([color.red, color.green, color.blue, color.alpha]),
        text_x,
        text_y,
        font_size,
        font,
        block.text.as_str(),
    );
    let rotated = rotate_about_center(
        &text_image,
        block.angle_radians(),
        Interpolation::Bilinear,
        Rgba([0, 0, 0, 0]),
    );
    let center = block.bounds().center();
    let origin_x = center.x.round() as i32 - diagonal as i32 / 2;
    let origin_y = center.y.round() as i32 - diagonal as i32 / 2;
    composite_rgba(pixels, width, height, &rotated, origin_x, origin_y);
}

fn draw_text_element(
    pixels: &mut [u8],
    width: u32,
    height: u32,
    bounds: Rect,
    content: &str,
    style: &ElementStyle,
    color: RgbaColor,
) {
    if style.fill.alpha > 0 {
        fill_rect(
            pixels,
            width,
            height,
            bounds,
            style.fill.with_alpha_factor(style.opacity),
        );
    }
    let Some(font) = system_font(style.bold) else {
        return;
    };
    let mut image = RgbaImage::new(
        bounds.width().ceil().max(1.0) as u32,
        bounds.height().ceil().max(1.0) as u32,
    );
    let (text_width, text_height) = text_size(style.font_size, font, content);
    let x = match style.alignment {
        TextAlignment::Left => 2,
        TextAlignment::Center => ((image.width().saturating_sub(text_width)) / 2) as i32,
        TextAlignment::Right => image.width().saturating_sub(text_width + 2) as i32,
    };
    let y = ((image.height().saturating_sub(text_height)) / 2) as i32;
    draw_text_mut(
        &mut image,
        Rgba([color.red, color.green, color.blue, color.alpha]),
        x,
        y,
        style.font_size,
        font,
        content,
    );
    composite_rgba(
        pixels,
        width,
        height,
        &image,
        bounds.left.round() as i32,
        bounds.top.round() as i32,
    );
}

fn system_font(bold: bool) -> Option<&'static FontArc> {
    static REGULAR_FONT: OnceLock<Option<FontArc>> = OnceLock::new();
    static BOLD_FONT: OnceLock<Option<FontArc>> = OnceLock::new();
    let slot = if bold { &BOLD_FONT } else { &REGULAR_FONT };
    slot.get_or_init(|| {
        let mut database = Database::new();
        database.load_system_fonts();
        let query = Query {
            families: &[
                Family::Name("Microsoft YaHei UI"),
                Family::Name("Microsoft YaHei"),
                Family::Name("Segoe UI"),
                Family::SansSerif,
            ],
            weight: if bold { Weight::BOLD } else { Weight::NORMAL },
            ..Query::default()
        };
        let id = database.query(&query)?;
        database
            .with_face_data(id, |data, index| {
                FontVec::try_from_vec_and_index(data.to_vec(), index)
                    .ok()
                    .map(FontArc::new)
            })
            .flatten()
    })
    .as_ref()
}

fn composite_rgba(
    pixels: &mut [u8],
    width: u32,
    height: u32,
    overlay: &RgbaImage,
    origin_x: i32,
    origin_y: i32,
) {
    for (x, y, pixel) in overlay.enumerate_pixels() {
        blend_pixel(
            pixels,
            width,
            height,
            origin_x + x as i32,
            origin_y + y as i32,
            RgbaColor::new(pixel[0], pixel[1], pixel[2], pixel[3]),
        );
    }
}

fn draw_shape(
    pixels: &mut [u8],
    canvas: (u32, u32),
    points: (Point, Point),
    shape: AnnotationTool,
    colors: (RgbaColor, RgbaColor),
    stroke_width: f32,
) {
    let (width, height) = canvas;
    let (start, end) = points;
    let (stroke, fill) = colors;
    let bounds = Rect::from_points(start, end);
    match shape {
        AnnotationTool::Rectangle => {
            fill_rect(pixels, width, height, bounds, fill);
            draw_rect_outline(pixels, width, height, bounds, stroke, stroke_width);
        }
        AnnotationTool::Ellipse => {
            fill_ellipse(pixels, width, height, bounds, fill);
            draw_ellipse_outline(pixels, width, height, bounds, stroke, stroke_width);
        }
        AnnotationTool::Diamond => {
            let center = bounds.center();
            let points = [
                Point::new(center.x, bounds.top),
                Point::new(bounds.right, center.y),
                Point::new(center.x, bounds.bottom),
                Point::new(bounds.left, center.y),
            ];
            fill_polygon(pixels, width, height, &points, fill);
            draw_quad_outline(pixels, width, height, points, stroke, stroke_width);
        }
        _ => {}
    }
}

fn draw_serial_number(
    pixels: &mut [u8],
    width: u32,
    height: u32,
    center: Point,
    number: u32,
    style: &ElementStyle,
) {
    let radius = (style.font_size * 0.9).clamp(13.0, 36.0);
    draw_disc(pixels, width, height, center, radius, style.fill);
    draw_ellipse_outline(
        pixels,
        width,
        height,
        Rect {
            left: center.x - radius,
            top: center.y - radius,
            right: center.x + radius,
            bottom: center.y + radius,
        },
        style.stroke,
        style.stroke_width,
    );
    let bounds = Rect {
        left: center.x - radius,
        top: center.y - radius,
        right: center.x + radius,
        bottom: center.y + radius,
    };
    draw_text_element(
        pixels,
        width,
        height,
        bounds,
        number.to_string().as_str(),
        &ElementStyle {
            fill: RgbaColor::TRANSPARENT,
            alignment: TextAlignment::Center,
            ..style.clone()
        },
        style.text,
    );
}

fn expected_pixel_len(width: u32, height: u32) -> Option<usize> {
    usize::try_from(width)
        .ok()?
        .checked_mul(usize::try_from(height).ok()?)?
        .checked_mul(4)
}

fn pixel_index(width: u32, x: u32, y: u32) -> usize {
    ((y as usize * width as usize) + x as usize) * 4
}

fn blend_pixel(pixels: &mut [u8], width: u32, height: u32, x: i32, y: i32, color: RgbaColor) {
    if x < 0 || y < 0 || x >= width as i32 || y >= height as i32 || color.alpha == 0 {
        return;
    }
    let index = pixel_index(width, x as u32, y as u32);
    let alpha = color.alpha as u32;
    let inverse = 255 - alpha;
    pixels[index] = ((color.red as u32 * alpha + pixels[index] as u32 * inverse + 127) / 255) as u8;
    pixels[index + 1] =
        ((color.green as u32 * alpha + pixels[index + 1] as u32 * inverse + 127) / 255) as u8;
    pixels[index + 2] =
        ((color.blue as u32 * alpha + pixels[index + 2] as u32 * inverse + 127) / 255) as u8;
    pixels[index + 3] = 255;
}

fn draw_disc(
    pixels: &mut [u8],
    width: u32,
    height: u32,
    center: Point,
    radius: f32,
    color: RgbaColor,
) {
    if color.alpha == 0 {
        return;
    }
    let radius = radius.max(0.5);
    let radius_squared = radius * radius;
    for y in (center.y - radius).floor() as i32..=(center.y + radius).ceil() as i32 {
        for x in (center.x - radius).floor() as i32..=(center.x + radius).ceil() as i32 {
            let dx = x as f32 + 0.5 - center.x;
            let dy = y as f32 + 0.5 - center.y;
            if dx * dx + dy * dy <= radius_squared {
                blend_pixel(pixels, width, height, x, y, color);
            }
        }
    }
}

fn draw_thick_line(
    pixels: &mut [u8],
    width: u32,
    height: u32,
    start: Point,
    end: Point,
    color: RgbaColor,
    stroke_width: f32,
) {
    let dx = end.x - start.x;
    let dy = end.y - start.y;
    let steps = dx.abs().max(dy.abs()).ceil().max(1.0) as u32;
    let radius = stroke_width.max(MIN_STROKE_WIDTH) / 2.0;
    for step in 0..=steps {
        let progress = step as f32 / steps as f32;
        draw_disc(
            pixels,
            width,
            height,
            Point::new(start.x + dx * progress, start.y + dy * progress),
            radius,
            color,
        );
    }
}

fn draw_polyline(
    pixels: &mut [u8],
    width: u32,
    height: u32,
    points: &[Point],
    color: RgbaColor,
    stroke_width: f32,
) {
    if let Some(first) = points.first().copied() {
        draw_disc(pixels, width, height, first, stroke_width / 2.0, color);
    }
    for pair in points.windows(2) {
        draw_thick_line(pixels, width, height, pair[0], pair[1], color, stroke_width);
    }
}

fn draw_arrow(
    pixels: &mut [u8],
    width: u32,
    height: u32,
    start: Point,
    end: Point,
    color: RgbaColor,
    stroke_width: f32,
) {
    draw_thick_line(pixels, width, height, start, end, color, stroke_width);
    let dx = end.x - start.x;
    let dy = end.y - start.y;
    let length = (dx * dx + dy * dy).sqrt();
    if length < 2.0 {
        return;
    }
    let head_length = (length * 0.24).clamp(stroke_width * 3.0, 28.0);
    let angle = dy.atan2(dx);
    for head_angle in [
        angle + std::f32::consts::PI - 0.55,
        angle + std::f32::consts::PI + 0.55,
    ] {
        draw_thick_line(
            pixels,
            width,
            height,
            end,
            Point::new(
                end.x + head_length * head_angle.cos(),
                end.y + head_length * head_angle.sin(),
            ),
            color,
            stroke_width,
        );
    }
}

fn fill_rect(pixels: &mut [u8], width: u32, height: u32, rect: Rect, color: RgbaColor) {
    if color.alpha == 0 {
        return;
    }
    for y in rect.top.floor().max(0.0) as i32..=rect.bottom.ceil().min(height as f32 - 1.0) as i32 {
        for x in
            rect.left.floor().max(0.0) as i32..=rect.right.ceil().min(width as f32 - 1.0) as i32
        {
            blend_pixel(pixels, width, height, x, y, color);
        }
    }
}

fn draw_rect_outline(
    pixels: &mut [u8],
    width: u32,
    height: u32,
    rect: Rect,
    color: RgbaColor,
    stroke_width: f32,
) {
    let points = [
        Point::new(rect.left, rect.top),
        Point::new(rect.right, rect.top),
        Point::new(rect.right, rect.bottom),
        Point::new(rect.left, rect.bottom),
    ];
    draw_quad_outline(pixels, width, height, points, color, stroke_width);
}

fn fill_ellipse(pixels: &mut [u8], width: u32, height: u32, rect: Rect, color: RgbaColor) {
    if color.alpha == 0 {
        return;
    }
    let center = rect.center();
    let radius_x = (rect.width() / 2.0).max(0.5);
    let radius_y = (rect.height() / 2.0).max(0.5);
    for y in rect.top.floor() as i32..=rect.bottom.ceil() as i32 {
        for x in rect.left.floor() as i32..=rect.right.ceil() as i32 {
            let dx = (x as f32 + 0.5 - center.x) / radius_x;
            let dy = (y as f32 + 0.5 - center.y) / radius_y;
            if dx * dx + dy * dy <= 1.0 {
                blend_pixel(pixels, width, height, x, y, color);
            }
        }
    }
}

fn draw_ellipse_outline(
    pixels: &mut [u8],
    width: u32,
    height: u32,
    rect: Rect,
    color: RgbaColor,
    stroke_width: f32,
) {
    let center = rect.center();
    let radius_x = rect.width().abs() / 2.0;
    let radius_y = rect.height().abs() / 2.0;
    let steps = (std::f32::consts::TAU * radius_x.max(radius_y))
        .ceil()
        .max(24.0) as u32;
    let mut previous = Point::new(center.x + radius_x, center.y);
    for step in 1..=steps {
        let angle = std::f32::consts::TAU * step as f32 / steps as f32;
        let current = Point::new(
            center.x + radius_x * angle.cos(),
            center.y + radius_y * angle.sin(),
        );
        draw_thick_line(
            pixels,
            width,
            height,
            previous,
            current,
            color,
            stroke_width,
        );
        previous = current;
    }
}

fn fill_polygon(pixels: &mut [u8], width: u32, height: u32, points: &[Point], color: RgbaColor) {
    if color.alpha == 0 {
        return;
    }
    let Some(bounds) = bounds_for_points(points) else {
        return;
    };
    for y in bounds.top.floor() as i32..=bounds.bottom.ceil() as i32 {
        for x in bounds.left.floor() as i32..=bounds.right.ceil() as i32 {
            if point_in_polygon(Point::new(x as f32 + 0.5, y as f32 + 0.5), points) {
                blend_pixel(pixels, width, height, x, y, color);
            }
        }
    }
}

fn draw_quad_outline(
    pixels: &mut [u8],
    width: u32,
    height: u32,
    points: [Point; 4],
    color: RgbaColor,
    stroke_width: f32,
) {
    for index in 0..4 {
        draw_thick_line(
            pixels,
            width,
            height,
            points[index],
            points[(index + 1) % 4],
            color,
            stroke_width,
        );
    }
}

fn draw_selection(pixels: &mut [u8], width: u32, height: u32, bounds: Rect) {
    let color = RgbaColor::new(25, 190, 180, 255);
    draw_rect_outline(pixels, width, height, bounds, color, 2.0);
    for point in [
        Point::new(bounds.left, bounds.top),
        Point::new((bounds.left + bounds.right) / 2.0, bounds.top),
        Point::new(bounds.right, bounds.top),
        Point::new(bounds.right, (bounds.top + bounds.bottom) / 2.0),
        Point::new(bounds.right, bounds.bottom),
        Point::new((bounds.left + bounds.right) / 2.0, bounds.bottom),
        Point::new(bounds.left, bounds.bottom),
        Point::new(bounds.left, (bounds.top + bounds.bottom) / 2.0),
    ] {
        draw_disc(pixels, width, height, point, 4.5, RgbaColor::WHITE);
        draw_disc(pixels, width, height, point, 3.0, color);
    }
}

fn path_samples(points: &[Point]) -> Vec<Point> {
    let mut samples = Vec::new();
    if let Some(first) = points.first().copied() {
        samples.push(first);
    }
    for pair in points.windows(2) {
        let dx = pair[1].x - pair[0].x;
        let dy = pair[1].y - pair[0].y;
        let steps = (dx.abs().max(dy.abs()) / 4.0).ceil().max(1.0) as u32;
        for step in 1..=steps {
            let progress = step as f32 / steps as f32;
            samples.push(Point::new(
                pair[0].x + dx * progress,
                pair[0].y + dy * progress,
            ));
        }
    }
    samples
}

fn point_in_path_brush(point: Point, samples: &[Point], radius: f32) -> bool {
    samples
        .iter()
        .any(|sample| point.distance(*sample) <= radius)
}

fn apply_mosaic_path(
    output: &mut [u8],
    source: &[u8],
    width: u32,
    height: u32,
    points: &[Point],
    brush_width: f32,
    strength: f32,
) {
    let samples = path_samples(points);
    let radius = brush_width / 2.0;
    let block_size = (brush_width / (5.0 - strength.clamp(0.05, 1.0) * 2.0))
        .round()
        .clamp(4.0, 28.0) as u32;
    for block_y in (0..height).step_by(block_size as usize) {
        for block_x in (0..width).step_by(block_size as usize) {
            let right = (block_x + block_size).min(width);
            let bottom = (block_y + block_size).min(height);
            let center = Point::new(
                (block_x + right) as f32 / 2.0,
                (block_y + bottom) as f32 / 2.0,
            );
            if !point_in_path_brush(center, &samples, radius + block_size as f32) {
                continue;
            }
            let mut sum = [0_u64; 4];
            let mut count = 0_u64;
            for y in block_y..bottom {
                for x in block_x..right {
                    let index = pixel_index(width, x, y);
                    for channel in 0..4 {
                        sum[channel] += source[index + channel] as u64;
                    }
                    count += 1;
                }
            }
            if count == 0 {
                continue;
            }
            let average = [
                (sum[0] / count) as u8,
                (sum[1] / count) as u8,
                (sum[2] / count) as u8,
                (sum[3] / count) as u8,
            ];
            for y in block_y..bottom {
                for x in block_x..right {
                    if point_in_path_brush(
                        Point::new(x as f32 + 0.5, y as f32 + 0.5),
                        &samples,
                        radius,
                    ) {
                        let index = pixel_index(width, x, y);
                        output[index..index + 4].copy_from_slice(&average);
                    }
                }
            }
        }
    }
}

fn apply_blur_path(
    output: &mut [u8],
    source: &[u8],
    width: u32,
    height: u32,
    points: &[Point],
    brush_width: f32,
    strength: f32,
) {
    let samples = path_samples(points);
    if samples.is_empty() {
        return;
    }
    let radius = brush_width / 2.0;
    let kernel = (2.0 + strength.clamp(0.05, 1.0) * 10.0).round() as i32;
    let bounds = bounds_for_points(&samples).unwrap().expanded(radius);
    let left = bounds.left.floor().max(0.0) as u32;
    let right = bounds.right.ceil().min(width.saturating_sub(1) as f32) as u32;
    let top = bounds.top.floor().max(0.0) as u32;
    let bottom = bounds.bottom.ceil().min(height.saturating_sub(1) as f32) as u32;
    for y in top..=bottom {
        for x in left..=right {
            if !point_in_path_brush(Point::new(x as f32 + 0.5, y as f32 + 0.5), &samples, radius) {
                continue;
            }
            let mut sum = [0_u64; 4];
            let mut count = 0_u64;
            for sample_y in (y as i32 - kernel).max(0)..=(y as i32 + kernel).min(height as i32 - 1)
            {
                for sample_x in
                    (x as i32 - kernel).max(0)..=(x as i32 + kernel).min(width as i32 - 1)
                {
                    let index = pixel_index(width, sample_x as u32, sample_y as u32);
                    for channel in 0..4 {
                        sum[channel] += source[index + channel] as u64;
                    }
                    count += 1;
                }
            }
            let index = pixel_index(width, x, y);
            for channel in 0..4 {
                output[index + channel] = (sum[channel] / count) as u8;
            }
        }
    }
}

fn point_in_quad(point: Point, quad: [Point; 4]) -> bool {
    point_in_polygon(point, &quad)
}

fn point_in_polygon(point: Point, polygon: &[Point]) -> bool {
    if polygon.len() < 3 {
        return false;
    }
    let mut inside = false;
    let mut previous = polygon.len() - 1;
    for current in 0..polygon.len() {
        let first = polygon[current];
        let second = polygon[previous];
        if ((first.y > point.y) != (second.y > point.y))
            && point.x
                < (second.x - first.x) * (point.y - first.y)
                    / (second.y - first.y).max(f32::EPSILON)
                    + first.x
        {
            inside = !inside;
        }
        previous = current;
    }
    inside
}

fn bounds_for_points(points: &[Point]) -> Option<Rect> {
    let first = points.first().copied()?;
    let mut bounds = Rect {
        left: first.x,
        top: first.y,
        right: first.x,
        bottom: first.y,
    };
    for point in &points[1..] {
        bounds.left = bounds.left.min(point.x);
        bounds.top = bounds.top.min(point.y);
        bounds.right = bounds.right.max(point.x);
        bounds.bottom = bounds.bottom.max(point.y);
    }
    Some(bounds)
}

fn normalized_ocr_text(blocks: &[OcrBlock]) -> String {
    struct OcrLine<'a> {
        blocks: Vec<&'a OcrBlock>,
        center_y: f32,
        top: f32,
        bottom: f32,
        left: f32,
        height: f32,
    }

    impl OcrLine<'_> {
        fn text(&self) -> String {
            let mut output = String::new();
            let mut previous = None::<&str>;
            for block in &self.blocks {
                let text = block.text.trim();
                if previous.is_some_and(|previous| needs_space(previous, text)) {
                    output.push(' ');
                }
                output.push_str(text);
                previous = Some(text);
            }
            output
        }
    }

    let mut sorted = blocks
        .iter()
        .filter(|block| !block.text.trim().is_empty())
        .collect::<Vec<_>>();
    sorted.sort_by(|first, second| {
        first
            .bounds()
            .center()
            .y
            .total_cmp(&second.bounds().center().y)
            .then_with(|| first.bounds().left.total_cmp(&second.bounds().left))
    });

    let mut lines = Vec::<OcrLine<'_>>::new();
    for block in sorted {
        let bounds = block.bounds();
        let center_y = bounds.center().y;
        let joins_current_line = lines.last().is_some_and(|line| {
            (center_y - line.center_y).abs() <= line.height.max(block.height()) * 0.55
        });
        if joins_current_line {
            let line = lines.last_mut().expect("line existence checked above");
            line.blocks.push(block);
            line.blocks
                .sort_by(|first, second| first.bounds().left.total_cmp(&second.bounds().left));
            line.top = line.top.min(bounds.top);
            line.bottom = line.bottom.max(bounds.bottom);
            line.left = line.left.min(bounds.left);
            line.height = line.height.max(block.height());
            line.center_y = (line.top + line.bottom) * 0.5;
        } else {
            lines.push(OcrLine {
                blocks: vec![block],
                center_y,
                top: bounds.top,
                bottom: bounds.bottom,
                left: bounds.left,
                height: block.height(),
            });
        }
    }

    let mut output = String::new();
    let mut previous: Option<(String, f32, f32, f32)> = None;
    for line in lines {
        let text = line.text();
        if let Some((previous_text, previous_bottom, previous_left, previous_height)) = &previous {
            let line_height = previous_height.max(line.height);
            let gap = line.top - previous_bottom;
            let indentation = line.left - previous_left;
            let paragraph = gap > line_height * 1.25
                || indentation > line_height * 0.85
                || indentation < -line_height * 1.4;
            if paragraph {
                output.push_str("\n\n");
            } else if starts_list_item(&text) {
                output.push('\n');
            } else if ends_with_hyphen(previous_text) {
                output.pop();
            } else if needs_space(previous_text, &text) {
                output.push(' ');
            }
        }
        output.push_str(&text);
        previous = Some((text, line.bottom, line.left, line.height));
    }
    output
}

fn ends_with_hyphen(value: &str) -> bool {
    value.ends_with('-') || value.ends_with('‐') || value.ends_with('‑')
}

fn needs_space(previous: &str, current: &str) -> bool {
    let previous = previous.chars().last().unwrap_or_default();
    let current = current.chars().next().unwrap_or_default();
    !is_cjk(previous)
        && !is_cjk(current)
        && !matches!(current, ',' | '.' | ';' | ':' | '!' | '?' | ')' | ']' | '}')
}

fn starts_list_item(current: &str) -> bool {
    current
        .chars()
        .next()
        .is_some_and(|character| matches!(character, '-' | '•' | '·' | '●' | '○'))
        || current
            .split_once(['.', ')', '、'])
            .is_some_and(|(prefix, _)| {
                !prefix.is_empty() && prefix.chars().all(|character| character.is_ascii_digit())
            })
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

#[cfg(test)]
mod tests {
    use super::*;

    fn document() -> AnnotationDocument {
        AnnotationDocument::new(320, 180, vec![255; 320 * 180 * 4]).unwrap()
    }

    #[test]
    fn retained_elements_can_be_selected_moved_and_undone() {
        let mut document = document();
        document.begin(
            AnnotationTool::Rectangle,
            Point::new(20.0, 30.0),
            RgbaColor::RED,
            4.0,
        );
        assert!(document.commit(Point::new(120.0, 90.0)));
        let id = document.elements()[0].id;

        document.begin_with_style(
            AnnotationTool::Select,
            Point::new(50.0, 50.0),
            ElementStyle::default(),
            false,
            false,
        );
        assert!(document.commit(Point::new(70.0, 65.0)));
        assert_eq!(document.selected_id(), Some(id));
        assert!(document.elements()[0].bounds().left > 30.0);
        assert!(document.undo());
        assert!(document.elements()[0].bounds().left < 30.0);
    }

    #[test]
    fn style_and_layer_changes_are_history_entries() {
        let mut document = document();
        for offset in [10.0, 40.0] {
            document.begin(
                AnnotationTool::Line,
                Point::new(offset, 10.0),
                RgbaColor::RED,
                3.0,
            );
            document.commit(Point::new(offset + 20.0, 40.0));
        }
        document.select_at(Point::new(20.0, 25.0));
        assert!(document.apply_layer_command(LayerCommand::Front));
        assert!(document.update_selected_style(&StylePatch {
            stroke: Some(RgbaColor::BLACK),
            ..StylePatch::default()
        }));
        assert_eq!(document.selected_style().unwrap().stroke, RgbaColor::BLACK);
        assert!(document.undo());
        assert_eq!(document.selected_style(), None);
    }

    #[test]
    fn every_layer_command_and_delete_updates_retained_order() {
        let mut document = document();
        for offset in [20.0, 60.0, 100.0] {
            document.begin(
                AnnotationTool::Line,
                Point::new(offset, 10.0),
                RgbaColor::RED,
                3.0,
            );
            assert!(document.commit(Point::new(offset, 80.0)));
        }
        let middle = document.elements()[1].id;
        assert!(document.select_at(Point::new(60.0, 40.0)));
        assert!(document.apply_layer_command(LayerCommand::Forward));
        assert_eq!(document.elements()[2].id, middle);
        assert!(document.apply_layer_command(LayerCommand::Back));
        assert_eq!(document.elements()[0].id, middle);
        assert!(document.apply_layer_command(LayerCommand::Front));
        assert_eq!(document.elements()[2].id, middle);
        assert!(document.apply_layer_command(LayerCommand::Backward));
        assert_eq!(document.elements()[1].id, middle);
        assert!(document.delete_selected());
        assert_eq!(document.elements().len(), 2);
        assert!(document.undo());
        assert_eq!(document.elements().len(), 3);
    }

    #[test]
    fn ocr_blocks_are_selectable_but_selection_is_not_exported() {
        let mut document = document();
        document.set_ocr_blocks(vec![OcrBlock {
            id: ElementId(0),
            points: [
                Point::new(10.0, 10.0),
                Point::new(100.0, 10.0),
                Point::new(100.0, 30.0),
                Point::new(10.0, 30.0),
            ],
            text: "测试".to_string(),
            box_score: 1.0,
            text_score: 1.0,
        }]);
        let output_before = document.pixels().to_vec();
        assert!(document.select_at(Point::new(20.0, 20.0)));
        assert_eq!(document.selected_ocr_text(), Some("测试"));
        assert_eq!(document.pixels(), output_before);
        assert_ne!(document.preview_pixels(), output_before);
    }

    #[test]
    fn ordinary_selection_controls_are_preview_only() {
        let mut document = document();
        document.begin(
            AnnotationTool::Rectangle,
            Point::new(30.0, 30.0),
            RgbaColor::RED,
            4.0,
        );
        assert!(document.commit(Point::new(130.0, 90.0)));
        let output = document.pixels().to_vec();
        assert_ne!(document.preview_pixels(), output);
        document.clear_selection();
        assert_eq!(document.preview_pixels(), output);
        assert_eq!(document.pixels(), output);
    }

    #[test]
    fn ocr_text_joins_cjk_and_preserves_paragraphs() {
        let mut document = document();
        document.set_ocr_blocks(vec![
            OcrBlock {
                id: ElementId(0),
                points: [
                    Point::new(0.0, 0.0),
                    Point::new(40.0, 0.0),
                    Point::new(40.0, 20.0),
                    Point::new(0.0, 20.0),
                ],
                text: "第一行".to_string(),
                box_score: 1.0,
                text_score: 1.0,
            },
            OcrBlock {
                id: ElementId(0),
                points: [
                    Point::new(42.0, 0.0),
                    Point::new(90.0, 0.0),
                    Point::new(90.0, 20.0),
                    Point::new(42.0, 20.0),
                ],
                text: "继续。".to_string(),
                box_score: 1.0,
                text_score: 1.0,
            },
            OcrBlock {
                id: ElementId(0),
                points: [
                    Point::new(0.0, 70.0),
                    Point::new(80.0, 70.0),
                    Point::new(80.0, 90.0),
                    Point::new(0.0, 90.0),
                ],
                text: "Next paragraph".to_string(),
                box_score: 1.0,
                text_score: 1.0,
            },
        ]);
        assert_eq!(document.ocr_plain_text(), "第一行继续。\n\nNext paragraph");
    }

    #[test]
    fn all_tools_create_retained_elements() {
        for tool in [
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
        ] {
            let mut document = document();
            document.begin(tool, Point::new(20.0, 20.0), RgbaColor::RED, 6.0);
            document.commit(Point::new(90.0, 70.0));
            assert_eq!(document.elements().len(), 1, "{tool:?}");
            assert_eq!(document.selected_tool(), Some(tool), "{tool:?}");
        }
    }

    #[test]
    fn every_selection_handle_resizes_without_losing_the_element() {
        let handles = [
            (Point::new(40.0, 40.0), Point::new(24.0, 22.0)),
            (Point::new(90.0, 40.0), Point::new(90.0, 22.0)),
            (Point::new(140.0, 40.0), Point::new(158.0, 22.0)),
            (Point::new(140.0, 70.0), Point::new(158.0, 70.0)),
            (Point::new(140.0, 100.0), Point::new(158.0, 118.0)),
            (Point::new(90.0, 100.0), Point::new(90.0, 118.0)),
            (Point::new(40.0, 100.0), Point::new(24.0, 118.0)),
            (Point::new(40.0, 70.0), Point::new(24.0, 70.0)),
        ];
        for (handle, target) in handles {
            let mut document = document();
            document.begin(
                AnnotationTool::Rectangle,
                Point::new(40.0, 40.0),
                RgbaColor::RED,
                4.0,
            );
            assert!(document.commit(Point::new(140.0, 100.0)));
            let before = document.elements()[0].bounds();
            document.begin_with_style(
                AnnotationTool::Select,
                handle,
                ElementStyle::default(),
                false,
                false,
            );
            assert!(document.commit(target));
            assert_ne!(document.elements()[0].bounds(), before, "{handle:?}");
            assert_eq!(document.elements().len(), 1);
        }
    }

    #[test]
    fn one_freehand_drag_is_one_history_entry() {
        let mut document = document();
        document.begin(
            AnnotationTool::Pen,
            Point::new(10.0, 10.0),
            RgbaColor::RED,
            4.0,
        );
        for step in 1..40 {
            document.update(Point::new(10.0 + step as f32, 10.0 + step as f32 * 0.5));
        }
        assert!(document.commit(Point::new(60.0, 40.0)));
        assert_eq!(document.elements().len(), 1);
        assert!(document.undo());
        assert!(document.elements().is_empty());
        assert!(!document.undo());
    }

    #[test]
    fn rotated_and_vertical_ocr_blocks_keep_geometry_and_auto_contrast() {
        let dark = vec![12_u8, 12, 12, 255]
            .into_iter()
            .cycle()
            .take(320 * 180 * 4)
            .collect::<Vec<_>>();
        let mut document = AnnotationDocument::new(320, 180, dark).unwrap();
        let rotated = OcrBlock {
            id: ElementId(0),
            points: [
                Point::new(30.0, 30.0),
                Point::new(140.0, 50.0),
                Point::new(136.0, 72.0),
                Point::new(26.0, 52.0),
            ],
            text: "旋转文字".to_string(),
            box_score: 0.96,
            text_score: 0.92,
        };
        let vertical = OcrBlock {
            id: ElementId(0),
            points: [
                Point::new(210.0, 20.0),
                Point::new(230.0, 20.0),
                Point::new(230.0, 140.0),
                Point::new(210.0, 140.0),
            ],
            text: "竖排".to_string(),
            box_score: 0.94,
            text_score: 0.91,
        };
        assert!(rotated.angle_radians().abs() > 0.1);
        assert!(vertical.height() > vertical.width());
        assert_eq!(
            automatic_text_color(
                document.pixels(),
                document.width(),
                document.height(),
                &rotated
            ),
            RgbaColor::WHITE
        );
        document.set_ocr_blocks(vec![rotated, vertical]);
        assert_eq!(document.ocr_blocks().len(), 2);
        assert_ne!(document.pixels(), document.original.as_slice());
    }

    #[test]
    fn hidden_ocr_style_survives_new_results_and_export_order_is_undoable() {
        let mut document = document();
        let mut style = OcrLayerStyle {
            visible: false,
            ..OcrLayerStyle::default()
        };
        document.configure_ocr_style(style.clone());
        assert!(!document.can_undo());
        document.set_ocr_blocks(vec![OcrBlock {
            id: ElementId(0),
            points: [
                Point::new(10.0, 10.0),
                Point::new(100.0, 10.0),
                Point::new(100.0, 30.0),
                Point::new(10.0, 30.0),
            ],
            text: "hidden".to_string(),
            box_score: 1.0,
            text_score: 1.0,
        }]);
        assert!(!document.ocr_style().visible);
        document.set_ocr_visible(true);
        assert!(document.select_at(Point::new(20.0, 20.0)));
        assert!(document.apply_layer_command(LayerCommand::Front));
        assert!(document.ocr_style().above_annotations);
        assert!(document.undo());
        assert!(!document.ocr_style().above_annotations);

        style.visible = true;
        document.configure_ocr_style(style);
        assert!(document.ocr_style().visible);
    }

    #[test]
    fn ocr_text_merges_latin_wraps_and_removes_line_hyphens() {
        let mut document = document();
        document.set_ocr_blocks(vec![
            OcrBlock {
                id: ElementId(0),
                points: [
                    Point::new(0.0, 0.0),
                    Point::new(60.0, 0.0),
                    Point::new(60.0, 18.0),
                    Point::new(0.0, 18.0),
                ],
                text: "cross-".to_string(),
                box_score: 1.0,
                text_score: 1.0,
            },
            OcrBlock {
                id: ElementId(0),
                points: [
                    Point::new(0.0, 20.0),
                    Point::new(80.0, 20.0),
                    Point::new(80.0, 38.0),
                    Point::new(0.0, 38.0),
                ],
                text: "platform".to_string(),
                box_score: 1.0,
                text_score: 1.0,
            },
        ]);
        assert_eq!(document.ocr_plain_text(), "crossplatform");
    }

    #[test]
    fn ocr_text_uses_spacing_and_indentation_instead_of_sentence_punctuation() {
        let mut document = document();
        let line = |left: f32, top: f32, text: &str| OcrBlock {
            id: ElementId(0),
            points: [
                Point::new(left, top),
                Point::new(left + 180.0, top),
                Point::new(left + 180.0, top + 18.0),
                Point::new(left, top + 18.0),
            ],
            text: text.to_string(),
            box_score: 1.0,
            text_score: 1.0,
        };
        document.set_ocr_blocks(vec![
            line(0.0, 0.0, "第一句。"),
            line(0.0, 20.0, "下一行仍属于同一段"),
            line(24.0, 50.0, "缩进开始新段"),
        ]);
        assert_eq!(
            document.ocr_plain_text(),
            "第一句。下一行仍属于同一段\n\n缩进开始新段"
        );
    }

    #[test]
    fn serial_numbers_can_be_edited_and_continue_from_configured_value() {
        let mut document = document();
        document.configure_next_serial_number(7);
        document.begin(
            AnnotationTool::SerialNumber,
            Point::new(40.0, 40.0),
            RgbaColor::RED,
            4.0,
        );
        assert!(document.commit(Point::new(40.0, 40.0)));
        assert_eq!(document.selected_serial_number(), Some(7));
        assert_eq!(document.next_serial_number(), 8);
        assert!(document.update_serial_number(42));
        assert_eq!(document.selected_serial_number(), Some(42));
        assert!(document.undo());
        assert_eq!(document.selected_serial_number(), None);
        assert_eq!(
            document.elements()[0].bounds().center(),
            Point::new(40.0, 40.0)
        );
    }

    #[test]
    #[ignore = "manual release-mode performance gate"]
    fn cached_preview_meets_2k_and_4k_core_frame_budgets() {
        fn average_update_ms(width: u32, height: u32, updates: u32) -> f64 {
            let mut document = AnnotationDocument::new(
                width,
                height,
                vec![255; width as usize * height as usize * 4],
            )
            .unwrap();
            document.begin(
                AnnotationTool::Pen,
                Point::new(20.0, 20.0),
                RgbaColor::RED,
                6.0,
            );
            let started = std::time::Instant::now();
            for step in 1..=updates {
                document.update(Point::new(
                    20.0 + step as f32 * 3.0,
                    20.0 + (step % 90) as f32 * 2.0,
                ));
            }
            started.elapsed().as_secs_f64() * 1000.0 / updates as f64
        }

        let average_2k = average_update_ms(2560, 1440, 120);
        let average_4k = average_update_ms(3840, 2160, 60);
        eprintln!("2K average: {average_2k:.2} ms; 4K average: {average_4k:.2} ms");
        assert!(average_2k <= 16.67, "2K average was {average_2k:.2} ms");
        assert!(average_4k <= 33.34, "4K average was {average_4k:.2} ms");
    }
}
