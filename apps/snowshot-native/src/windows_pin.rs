use std::mem::size_of;
use std::rc::Rc;
use std::sync::OnceLock;

use windows::Win32::Foundation::{HINSTANCE, HMODULE, HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_SHADER_RESOURCE, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION,
    D3D11_SUBRESOURCE_DATA, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11CreateDevice,
    ID3D11Device, ID3D11DeviceContext, ID3D11Resource, ID3D11Texture2D,
};
use windows::Win32::Graphics::DirectComposition::{
    DCOMPOSITION_BITMAP_INTERPOLATION_MODE_LINEAR, DCOMPOSITION_BORDER_MODE_HARD,
    DCompositionBoostCompositorClock, DCompositionCreateDevice3, IDCompositionDesktopDevice,
    IDCompositionDevice3, IDCompositionEffectGroup, IDCompositionScaleTransform,
    IDCompositionShadowEffect, IDCompositionSurface, IDCompositionTarget, IDCompositionVisual,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_ALPHA_MODE_PREMULTIPLIED, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{IDXGIDevice, IDXGISurface};
use windows::Win32::Graphics::Gdi::ValidateRect;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, ReleaseCapture, SetCapture, TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent,
    VK_CONTROL,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CS_DBLCLKS, CreateWindowExW, DefWindowProcW, DestroyWindow, GWLP_USERDATA, GetCursorPos,
    GetWindowLongPtrW, IDC_ARROW, IDC_HAND, IDC_SIZENESW, IDC_SIZENWSE, IsWindow, LoadCursorW,
    RegisterClassW, SW_HIDE, SW_SHOWNOACTIVATE, SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER,
    SetCursor, SetWindowLongPtrW, SetWindowPos, ShowWindow, WM_CAPTURECHANGED, WM_CLOSE,
    WM_ERASEBKGND, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_NCDESTROY,
    WM_PAINT, WM_SETCURSOR, WNDCLASSW, WS_EX_NOACTIVATE, WS_EX_NOREDIRECTIONBITMAP,
    WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};
use windows::core::{Interface, PCWSTR, w};

const PIN_CLASS_NAME: PCWSTR = w!("SnowShotDirectCompositionPin");
const PIN_WINDOW_TITLE: PCWSTR = w!("Snow Shot 贴图");
const SHADOW_EXTENT: i32 = 2;
const WM_MOUSELEAVE_MESSAGE: u32 = 0x02A3;
const MIN_CONTENT_WIDTH: f32 = 96.0;
const MIN_CONTENT_HEIGHT: f32 = 64.0;
const MAX_CONTENT_WIDTH: f32 = 4096.0;
const MAX_CONTENT_HEIGHT: f32 = 4096.0;

static PIN_CLASS: OnceLock<Result<u16, String>> = OnceLock::new();

pub(crate) struct PinFrame<'a> {
    pub(crate) rgba: &'a [u8],
    pub(crate) source_width: u32,
    pub(crate) source_height: u32,
    pub(crate) display_width: u32,
    pub(crate) display_height: u32,
    pub(crate) origin_x: i32,
    pub(crate) origin_y: i32,
}

#[derive(Clone)]
pub(crate) struct PinCompositor {
    d3d_device: ID3D11Device,
    d3d_context: ID3D11DeviceContext,
    desktop_device: IDCompositionDesktopDevice,
    dcomp_device: IDCompositionDevice3,
}

impl PinCompositor {
    pub(crate) fn new() -> Result<Rc<Self>, String> {
        let (d3d_device, d3d_context) = create_d3d_device()?;
        let dxgi_device: IDXGIDevice = d3d_device
            .cast()
            .map_err(|error| format!("无法获取 DXGI 设备：{error}"))?;
        // SAFETY: the DXGI device remains alive in this compositor for the lifetime of DComp.
        let desktop_device: IDCompositionDesktopDevice =
            unsafe { DCompositionCreateDevice3(&dxgi_device) }
                .map_err(|error| format!("无法创建 DirectComposition 设备：{error}"))?;
        let dcomp_device: IDCompositionDevice3 = desktop_device
            .cast()
            .map_err(|error| format!("无法获取 DirectComposition 3 接口：{error}"))?;

        Ok(Rc::new(Self {
            d3d_device,
            d3d_context,
            desktop_device,
            dcomp_device,
        }))
    }

    pub(crate) fn create_pin(self: &Rc<Self>, frame: PinFrame<'_>) -> Result<HWND, String> {
        if frame.source_width == 0 || frame.source_height == 0 {
            return Err("贴图源尺寸不能为空。".to_string());
        }
        let expected_len = frame.source_width as usize * frame.source_height as usize * 4;
        if frame.rgba.len() != expected_len {
            return Err("贴图像素数据与尺寸不一致。".to_string());
        }

        ensure_pin_class()?;
        let instance = module_instance()?;
        let window_width = frame.display_width.saturating_add(SHADOW_EXTENT as u32);
        let window_height = frame.display_height.saturating_add(SHADOW_EXTENT as u32);

        // SAFETY: class registration and all pointers passed here remain valid for the call.
        let hwnd = unsafe {
            CreateWindowExW(
                WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_NOREDIRECTIONBITMAP,
                PIN_CLASS_NAME,
                PIN_WINDOW_TITLE,
                WS_POPUP,
                frame.origin_x,
                frame.origin_y,
                window_width as i32,
                window_height as i32,
                None,
                None,
                Some(instance),
                None,
            )
        }
        .map_err(|error| format!("无法创建原生贴图窗口：{error}"))?;

        let result = (|| {
            let metrics = PinMetrics::for_window(hwnd);
            let composition = PinComposition::new(Rc::clone(self), hwnd, &frame, metrics)?;
            let state = Box::new(NativePinState {
                hwnd,
                rect: ScreenRect {
                    left: frame.origin_x as f32,
                    top: frame.origin_y as f32,
                    width: window_width as f32,
                    height: window_height as f32,
                },
                source_width: frame.source_width as f32,
                source_height: frame.source_height as f32,
                metrics,
                composition,
                interaction: None,
                close_pressed: false,
                hover_close: false,
                hover_corner: None,
                tracking_mouse: false,
            });
            let state_ptr = Box::into_raw(state);
            // SAFETY: the pointer is owned by the HWND and released exactly once at WM_NCDESTROY.
            unsafe {
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, state_ptr as isize);
                let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            }
            Ok(hwnd)
        })();

        if result.is_err() {
            // SAFETY: hwnd was created by this function and has not been destroyed yet.
            let _ = unsafe { DestroyWindow(hwnd) };
        }
        result
    }

    fn create_surface(
        &self,
        width: u32,
        height: u32,
        premultiplied_bgra: &[u8],
    ) -> Result<IDCompositionSurface, String> {
        let expected_len = width as usize * height as usize * 4;
        if premultiplied_bgra.len() != expected_len {
            return Err("DirectComposition 纹理尺寸不一致。".to_string());
        }

        // SAFETY: all dimensions and formats are valid and supported by DirectComposition.
        let surface = unsafe {
            self.dcomp_device.CreateSurface(
                width,
                height,
                DXGI_FORMAT_B8G8R8A8_UNORM,
                DXGI_ALPHA_MODE_PREMULTIPLIED,
            )
        }
        .map_err(|error| format!("无法创建 DirectComposition 表面：{error}"))?;

        let texture = self.create_texture(width, height, premultiplied_bgra)?;
        let source_resource: ID3D11Resource = texture
            .cast()
            .map_err(|error| format!("无法获取源纹理资源：{error}"))?;
        let mut update_offset = POINT::default();
        // SAFETY: the first update covers the entire non-virtual surface as required by DComp.
        let target_surface: IDXGISurface =
            unsafe { surface.BeginDraw(None, &mut update_offset) }
                .map_err(|error| format!("无法开始上传 DirectComposition 表面：{error}"))?;
        let target_resource: ID3D11Resource = target_surface
            .cast()
            .map_err(|error| format!("无法获取目标纹理资源：{error}"))?;

        // SAFETY: source and target use identical formats and dimensions.
        unsafe {
            self.d3d_context.CopySubresourceRegion(
                &target_resource,
                0,
                update_offset.x.max(0) as u32,
                update_offset.y.max(0) as u32,
                0,
                &source_resource,
                0,
                None,
            );
        }
        // SAFETY: this matches the successful BeginDraw above.
        unsafe { surface.EndDraw() }
            .map_err(|error| format!("无法完成 DirectComposition 表面上传：{error}"))?;
        // SAFETY: flushing only submits this compositor's one-time texture copy.
        unsafe {
            self.d3d_context.Flush();
        }

        Ok(surface)
    }

    fn create_texture(
        &self,
        width: u32,
        height: u32,
        premultiplied_bgra: &[u8],
    ) -> Result<ID3D11Texture2D, String> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let data = D3D11_SUBRESOURCE_DATA {
            pSysMem: premultiplied_bgra.as_ptr().cast(),
            SysMemPitch: width.saturating_mul(4),
            SysMemSlicePitch: 0,
        };
        let mut texture = None;
        // SAFETY: desc and initial data describe the full source pixel buffer.
        unsafe {
            self.d3d_device
                .CreateTexture2D(&desc, Some(&data), Some(&mut texture))
        }
        .map_err(|error| format!("无法创建 D3D11 贴图纹理：{error}"))?;
        texture.ok_or_else(|| "D3D11 未返回贴图纹理。".to_string())
    }

    fn create_visual(&self) -> Result<IDCompositionVisual, String> {
        // SAFETY: the compositor device is valid and owned by this object.
        let visual = unsafe { self.dcomp_device.CreateVisual() }
            .map_err(|error| format!("无法创建 DirectComposition Visual：{error}"))?;
        visual
            .cast()
            .map_err(|error| format!("无法获取 DirectComposition Visual 接口：{error}"))
    }
}

pub(crate) fn is_pin_window(hwnd: HWND) -> bool {
    // SAFETY: IsWindow accepts stale HWND values and reports whether they are still valid.
    unsafe { IsWindow(Some(hwnd)).as_bool() }
}

pub(crate) fn show_pin_window(hwnd: HWND) {
    if is_pin_window(hwnd) {
        // SAFETY: hwnd belongs to a live pin in this process.
        unsafe {
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        }
    }
}

pub(crate) fn hide_pin_window(hwnd: HWND) {
    if is_pin_window(hwnd) {
        // SAFETY: hwnd belongs to a live pin in this process.
        unsafe {
            let _ = ShowWindow(hwnd, SW_HIDE);
        }
    }
}

pub(crate) fn destroy_pin_window(hwnd: HWND) {
    if is_pin_window(hwnd) {
        // SAFETY: hwnd belongs to a live pin in this process.
        let _ = unsafe { DestroyWindow(hwnd) };
    }
}

struct PinComposition {
    compositor: Rc<PinCompositor>,
    _target: IDCompositionTarget,
    _root: IDCompositionVisual,
    _image_visual: IDCompositionVisual,
    shadow_visual: IDCompositionVisual,
    close_visual: IDCompositionVisual,
    handle_visuals: Vec<IDCompositionVisual>,
    scale_transform: IDCompositionScaleTransform,
    shadow_effect: IDCompositionShadowEffect,
    close_effect: IDCompositionEffectGroup,
    handle_effects: Vec<IDCompositionEffectGroup>,
    _image_surface: IDCompositionSurface,
    _close_surface: IDCompositionSurface,
    _handle_surface: IDCompositionSurface,
    source_width: f32,
    source_height: f32,
    metrics: PinMetrics,
}

impl PinComposition {
    fn new(
        compositor: Rc<PinCompositor>,
        hwnd: HWND,
        frame: &PinFrame<'_>,
        metrics: PinMetrics,
    ) -> Result<Self, String> {
        let image_pixels = rgba_to_premultiplied_bgra(frame.rgba);
        let close_pixels = close_button_pixels(metrics.close_size);
        let handle_pixels = resize_handle_pixels(metrics.handle_size);
        let image_surface =
            compositor.create_surface(frame.source_width, frame.source_height, &image_pixels)?;
        let close_surface =
            compositor.create_surface(metrics.close_size, metrics.close_size, &close_pixels)?;
        let handle_surface =
            compositor.create_surface(metrics.handle_size, metrics.handle_size, &handle_pixels)?;

        // SAFETY: hwnd is a live window owned by this process.
        let target = unsafe { compositor.desktop_device.CreateTargetForHwnd(hwnd, true) }
            .map_err(|error| format!("无法绑定 DirectComposition 目标窗口：{error}"))?;
        let root = compositor.create_visual()?;
        let shadow_visual = compositor.create_visual()?;
        let image_visual = compositor.create_visual()?;
        let close_visual = compositor.create_visual()?;
        let handle_visuals = (0..4)
            .map(|_| compositor.create_visual())
            .collect::<Result<Vec<_>, _>>()?;

        // SAFETY: all objects come from the same DirectComposition device.
        let scale_transform = unsafe { compositor.dcomp_device.CreateScaleTransform() }
            .map_err(|error| format!("无法创建贴图合成缩放：{error}"))?;
        // SAFETY: IDCompositionDevice3 supports compositor-side shadow effects on Windows 11.
        let shadow_effect = unsafe { compositor.dcomp_device.CreateShadowEffect() }
            .map_err(|error| format!("无法创建贴图合成阴影：{error}"))?;
        // SAFETY: effect groups are compositor-owned scalar property containers.
        let close_effect = unsafe { compositor.dcomp_device.CreateEffectGroup() }
            .map_err(|error| format!("无法创建关闭按钮透明度效果：{error}"))?;
        let handle_effects = (0..4)
            .map(|_| {
                // SAFETY: effect group creation has no external pointer inputs.
                unsafe { compositor.dcomp_device.CreateEffectGroup() }
                    .map_err(|error| format!("无法创建缩放手柄透明度效果：{error}"))
            })
            .collect::<Result<Vec<_>, _>>()?;

        let configure_visual_tree = || -> windows::core::Result<()> {
            // SAFETY: all surfaces, visuals, transforms and effects share the same DComp device.
            unsafe {
                shadow_effect.SetStandardDeviation2(2.0)?;
                shadow_effect.SetRed2(0.0)?;
                shadow_effect.SetGreen2(0.0)?;
                shadow_effect.SetBlue2(0.0)?;
                shadow_effect.SetAlpha2(0.40)?;

                shadow_visual.SetContent(&image_surface)?;
                shadow_visual.SetTransform(&scale_transform)?;
                shadow_visual.SetEffect(&shadow_effect)?;

                image_visual.SetContent(&image_surface)?;
                image_visual.SetTransform(&scale_transform)?;
                image_visual
                    .SetBitmapInterpolationMode(DCOMPOSITION_BITMAP_INTERPOLATION_MODE_LINEAR)?;
                image_visual.SetBorderMode(DCOMPOSITION_BORDER_MODE_HARD)?;

                close_visual.SetContent(&close_surface)?;
                close_effect.SetOpacity2(0.32)?;
                close_visual.SetEffect(&close_effect)?;

                for (visual, effect) in handle_visuals.iter().zip(&handle_effects) {
                    visual.SetContent(&handle_surface)?;
                    effect.SetOpacity2(0.22)?;
                    visual.SetEffect(effect)?;
                }

                root.AddVisual(&shadow_visual, false, None::<&IDCompositionVisual>)?;
                root.AddVisual(&image_visual, true, &shadow_visual)?;
                let mut reference = image_visual.clone();
                for visual in &handle_visuals {
                    root.AddVisual(visual, true, &reference)?;
                    reference = visual.clone();
                }
                root.AddVisual(&close_visual, true, &reference)?;
                target.SetRoot(&root)?;
            }
            Ok(())
        };
        configure_visual_tree()
            .map_err(|error| format!("无法配置 DirectComposition 视觉树：{error}"))?;

        let composition = Self {
            compositor,
            _target: target,
            _root: root,
            _image_visual: image_visual,
            shadow_visual,
            close_visual,
            handle_visuals,
            scale_transform,
            shadow_effect,
            close_effect,
            handle_effects,
            _image_surface: image_surface,
            _close_surface: close_surface,
            _handle_surface: handle_surface,
            source_width: frame.source_width as f32,
            source_height: frame.source_height as f32,
            metrics,
        };
        composition.set_layout(frame.display_width as f32, frame.display_height as f32)?;
        composition.commit()?;
        composition.wait_for_commit_completion()?;
        Ok(composition)
    }

    fn set_layout(&self, content_width: f32, content_height: f32) -> Result<(), String> {
        let scale_x = (content_width / self.source_width).max(0.001);
        let scale_y = (content_height / self.source_height).max(0.001);
        let shadow_scale = scale_x.min(scale_y).max(0.001);
        let shadow_local_offset_x = SHADOW_EXTENT as f32 / scale_x;
        let shadow_local_offset_y = SHADOW_EXTENT as f32 / scale_y;
        let shadow_local_blur = 2.0 / shadow_scale;
        let close_x =
            (content_width - self.metrics.close_size as f32 - self.metrics.close_margin as f32)
                .max(0.0);
        let close_y = self.metrics.close_margin as f32;
        let handle_size = self.metrics.handle_size as f32;
        let right = (content_width - handle_size).max(0.0);
        let bottom = (content_height - handle_size).max(0.0);
        let handle_positions = [(0.0, 0.0), (right, 0.0), (0.0, bottom), (right, bottom)];

        let update_layout = || -> windows::core::Result<()> {
            // SAFETY: properties are updated transactionally and committed together below.
            unsafe {
                self.scale_transform.SetScaleX2(scale_x)?;
                self.scale_transform.SetScaleY2(scale_y)?;
                self.shadow_visual.SetOffsetX2(shadow_local_offset_x)?;
                self.shadow_visual.SetOffsetY2(shadow_local_offset_y)?;
                self.shadow_effect
                    .SetStandardDeviation2(shadow_local_blur)?;
                self.close_visual.SetOffsetX2(close_x)?;
                self.close_visual.SetOffsetY2(close_y)?;
                for (visual, (x, y)) in self.handle_visuals.iter().zip(handle_positions) {
                    visual.SetOffsetX2(x)?;
                    visual.SetOffsetY2(y)?;
                }
            }
            Ok(())
        };
        update_layout().map_err(|error| format!("无法更新 DirectComposition 贴图布局：{error}"))?;
        Ok(())
    }

    fn set_hover(&self, close_hot: bool, corner_hot: Option<ResizeCorner>) -> Result<(), String> {
        let update_opacity = || -> windows::core::Result<()> {
            // SAFETY: opacity values are finite and effects belong to this device.
            unsafe {
                self.close_effect
                    .SetOpacity2(if close_hot { 1.0 } else { 0.32 })?;
                for (index, effect) in self.handle_effects.iter().enumerate() {
                    let corner = ResizeCorner::from_index(index);
                    effect.SetOpacity2(if Some(corner) == corner_hot {
                        1.0
                    } else {
                        0.22
                    })?;
                }
            }
            Ok(())
        };
        update_opacity()
            .map_err(|error| format!("无法更新 DirectComposition 悬停效果：{error}"))?;
        self.commit()
    }

    fn commit(&self) -> Result<(), String> {
        // SAFETY: all pending properties belong to the live composition device.
        unsafe { self.compositor.dcomp_device.Commit() }
            .map_err(|error| format!("DirectComposition 提交失败：{error}"))
    }

    fn wait_for_commit_completion(&self) -> Result<(), String> {
        // SAFETY: waiting is used only for the initial presentation, before the HWND is shown.
        unsafe { self.compositor.dcomp_device.WaitForCommitCompletion() }
            .map_err(|error| format!("DirectComposition 首帧等待失败：{error}"))
    }
}

struct NativePinState {
    hwnd: HWND,
    rect: ScreenRect,
    source_width: f32,
    source_height: f32,
    metrics: PinMetrics,
    composition: PinComposition,
    interaction: Option<PointerInteraction>,
    close_pressed: bool,
    hover_close: bool,
    hover_corner: Option<ResizeCorner>,
    tracking_mouse: bool,
}

impl NativePinState {
    fn pointer_down(&mut self, point: POINT) -> bool {
        let (client_x, client_y) = self.client_point(point);
        if self.is_over_close(client_x, client_y) {
            self.close_pressed = true;
            self.hover_close = true;
            let _ = self.composition.set_hover(true, None);
            return false;
        }

        let corner = self.corner_at(client_x, client_y);
        self.interaction = Some(match corner {
            Some(corner) => PointerInteraction::Resize {
                start_pointer: point,
                start_rect: self.rect,
                corner,
            },
            None => PointerInteraction::Move {
                start_pointer: point,
                start_rect: self.rect,
            },
        });
        self.hover_corner = corner;
        let _ = self.composition.set_hover(false, corner);
        true
    }

    fn pointer_move(&mut self, point: POINT) {
        if let Some(interaction) = self.interaction {
            match interaction {
                PointerInteraction::Move {
                    start_pointer,
                    start_rect,
                } => self.move_from(start_pointer, start_rect, point),
                PointerInteraction::Resize {
                    start_pointer,
                    start_rect,
                    corner,
                } => {
                    let rect = resize_from_corner(
                        start_rect,
                        start_pointer,
                        point,
                        corner,
                        self.source_width / self.source_height,
                    );
                    let _ = self.apply_rect(rect);
                }
            }
            return;
        }

        let (client_x, client_y) = self.client_point(point);
        let close_hot = self.is_over_close(client_x, client_y);
        let corner_hot = if close_hot {
            None
        } else {
            self.corner_at(client_x, client_y)
        };
        if close_hot != self.hover_close || corner_hot != self.hover_corner {
            self.hover_close = close_hot;
            self.hover_corner = corner_hot;
            let _ = self.composition.set_hover(close_hot, corner_hot);
        }
    }

    fn pointer_up(&mut self, point: POINT) -> bool {
        let close_clicked = if self.close_pressed {
            let (client_x, client_y) = self.client_point(point);
            self.is_over_close(client_x, client_y)
        } else {
            false
        };
        self.close_pressed = false;
        self.interaction = None;
        // SAFETY: this ends the compositor boost started in pointer_down.
        unsafe {
            let _ = DCompositionBoostCompositorClock(false);
        }
        close_clicked
    }

    fn cancel_interaction(&mut self) {
        self.close_pressed = false;
        self.interaction = None;
        // SAFETY: disabling a compositor boost is safe even if it is already disabled.
        unsafe {
            let _ = DCompositionBoostCompositorClock(false);
        }
    }

    fn wheel_zoom(&mut self, point: POINT, wheel_delta: i16) {
        if wheel_delta == 0 {
            return;
        }
        let steps = wheel_delta as f32 / 120.0;
        let factor = 1.1_f32.powf(steps);
        let current_content_width = self.rect.content_width();
        let current_content_height = self.rect.content_height();
        let target_content_width =
            (current_content_width * factor).clamp(MIN_CONTENT_WIDTH, MAX_CONTENT_WIDTH);
        let target_content_height =
            (current_content_height * factor).clamp(MIN_CONTENT_HEIGHT, MAX_CONTENT_HEIGHT);
        let applied_factor = (target_content_width / current_content_width)
            .min(target_content_height / current_content_height);
        let width = current_content_width * applied_factor + SHADOW_EXTENT as f32;
        let height = current_content_height * applied_factor + SHADOW_EXTENT as f32;
        let relative_x = (point.x as f32 - self.rect.left) / self.rect.width.max(1.0);
        let relative_y = (point.y as f32 - self.rect.top) / self.rect.height.max(1.0);
        let rect = ScreenRect {
            left: point.x as f32 - relative_x * width,
            top: point.y as f32 - relative_y * height,
            width,
            height,
        };
        let _ = self.apply_rect(rect);
    }

    fn move_from(&mut self, start_pointer: POINT, start_rect: ScreenRect, point: POINT) {
        let left = start_rect.left + (point.x - start_pointer.x) as f32;
        let top = start_rect.top + (point.y - start_pointer.y) as f32;
        self.rect.left = left;
        self.rect.top = top;
        // SAFETY: this only moves the live native window and keeps its persistent DComp surface.
        let _ = unsafe {
            SetWindowPos(
                self.hwnd,
                None,
                left.round() as i32,
                top.round() as i32,
                0,
                0,
                SWP_NOACTIVATE | SWP_NOZORDER | SWP_NOSIZE,
            )
        };
    }

    fn apply_rect(&mut self, rect: ScreenRect) -> Result<(), String> {
        self.composition
            .set_layout(rect.content_width(), rect.content_height())?;
        // SAFETY: geometry is validated and the window remains owned by this state.
        unsafe {
            SetWindowPos(
                self.hwnd,
                None,
                rect.left.round() as i32,
                rect.top.round() as i32,
                rect.width.round().max(1.0) as i32,
                rect.height.round().max(1.0) as i32,
                SWP_NOACTIVATE | SWP_NOZORDER,
            )
        }
        .map_err(|error| format!("无法更新原生贴图窗口尺寸：{error}"))?;
        self.rect = rect;
        self.composition.commit()
    }

    fn client_point(&self, point: POINT) -> (f32, f32) {
        (
            point.x as f32 - self.rect.left,
            point.y as f32 - self.rect.top,
        )
    }

    fn is_over_close(&self, x: f32, y: f32) -> bool {
        let size = self.metrics.close_size as f32;
        let margin = self.metrics.close_margin as f32;
        let left = self.rect.content_width() - size - margin;
        x >= left && x < left + size && y >= margin && y < margin + size
    }

    fn corner_at(&self, x: f32, y: f32) -> Option<ResizeCorner> {
        let hit = self.metrics.corner_hit_size as f32;
        let content_width = self.rect.content_width();
        let content_height = self.rect.content_height();
        let left = x <= hit;
        let right = x >= content_width - hit;
        let top = y <= hit;
        let bottom = y >= content_height - hit;
        match (left, right, top, bottom) {
            (true, _, true, _) => Some(ResizeCorner::TopLeft),
            (_, true, true, _) => Some(ResizeCorner::TopRight),
            (true, _, _, true) => Some(ResizeCorner::BottomLeft),
            (_, true, _, true) => Some(ResizeCorner::BottomRight),
            _ => None,
        }
    }

    fn begin_mouse_tracking(&mut self) {
        if self.tracking_mouse {
            return;
        }
        let mut tracking = TRACKMOUSEEVENT {
            cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
            dwFlags: TME_LEAVE,
            hwndTrack: self.hwnd,
            dwHoverTime: 0,
        };
        // SAFETY: tracking points to a fully initialized TRACKMOUSEEVENT structure.
        if unsafe { TrackMouseEvent(&mut tracking) }.is_ok() {
            self.tracking_mouse = true;
        }
    }

    fn mouse_left(&mut self) {
        self.tracking_mouse = false;
        if self.interaction.is_none() {
            self.hover_close = false;
            self.hover_corner = None;
            let _ = self.composition.set_hover(false, None);
        }
    }

    fn update_cursor(&self, point: POINT) {
        let (x, y) = self.client_point(point);
        let cursor_id = if self.is_over_close(x, y) {
            IDC_HAND
        } else {
            match self.corner_at(x, y) {
                Some(ResizeCorner::TopLeft | ResizeCorner::BottomRight) => IDC_SIZENWSE,
                Some(ResizeCorner::TopRight | ResizeCorner::BottomLeft) => IDC_SIZENESW,
                None => IDC_ARROW,
            }
        };
        // SAFETY: cursor identifiers are predefined system resources.
        if let Ok(cursor) = unsafe { LoadCursorW(None, cursor_id) } {
            // SAFETY: cursor is a live shared system cursor.
            unsafe {
                SetCursor(Some(cursor));
            }
        }
    }
}

#[derive(Clone, Copy)]
enum PointerInteraction {
    Move {
        start_pointer: POINT,
        start_rect: ScreenRect,
    },
    Resize {
        start_pointer: POINT,
        start_rect: ScreenRect,
        corner: ResizeCorner,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ResizeCorner {
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

impl ResizeCorner {
    fn from_index(index: usize) -> Self {
        match index {
            0 => Self::TopLeft,
            1 => Self::TopRight,
            2 => Self::BottomLeft,
            _ => Self::BottomRight,
        }
    }

    fn signs(self) -> (f32, f32) {
        match self {
            Self::TopLeft => (-1.0, -1.0),
            Self::TopRight => (1.0, -1.0),
            Self::BottomLeft => (-1.0, 1.0),
            Self::BottomRight => (1.0, 1.0),
        }
    }
}

#[derive(Clone, Copy)]
struct ScreenRect {
    left: f32,
    top: f32,
    width: f32,
    height: f32,
}

impl ScreenRect {
    fn right(self) -> f32 {
        self.left + self.width
    }

    fn bottom(self) -> f32 {
        self.top + self.height
    }

    fn content_width(self) -> f32 {
        (self.width - SHADOW_EXTENT as f32).max(1.0)
    }

    fn content_height(self) -> f32 {
        (self.height - SHADOW_EXTENT as f32).max(1.0)
    }
}

#[derive(Clone, Copy)]
struct PinMetrics {
    close_size: u32,
    close_margin: u32,
    handle_size: u32,
    corner_hit_size: u32,
}

impl PinMetrics {
    fn for_window(hwnd: HWND) -> Self {
        // SAFETY: hwnd is a live window. A zero DPI result falls back to 96.
        let dpi = unsafe { GetDpiForWindow(hwnd) }.max(96);
        let scale = dpi as f32 / 96.0;
        Self {
            close_size: (28.0 * scale).round().max(20.0) as u32,
            close_margin: (4.0 * scale).round().max(3.0) as u32,
            handle_size: (12.0 * scale).round().max(10.0) as u32,
            corner_hit_size: (14.0 * scale).round().max(12.0) as u32,
        }
    }
}

fn resize_from_corner(
    start_rect: ScreenRect,
    start_pointer: POINT,
    pointer: POINT,
    corner: ResizeCorner,
    aspect: f32,
) -> ScreenRect {
    let start_width = start_rect.content_width();
    let start_height = start_rect.content_height();
    let (horizontal_sign, vertical_sign) = corner.signs();
    let horizontal_delta =
        (pointer.x - start_pointer.x) as f32 * horizontal_sign / start_width.max(1.0);
    let vertical_delta =
        (pointer.y - start_pointer.y) as f32 * vertical_sign / start_height.max(1.0);
    let dominant_delta = if horizontal_delta.abs() >= vertical_delta.abs() {
        horizontal_delta
    } else {
        vertical_delta
    };
    let min_scale = (MIN_CONTENT_WIDTH / start_width)
        .max(MIN_CONTENT_HEIGHT / start_height)
        .min(1.0);
    let max_scale = (MAX_CONTENT_WIDTH / start_width)
        .min(MAX_CONTENT_HEIGHT / start_height)
        .max(1.0);
    let scale = (1.0 + dominant_delta).clamp(min_scale, max_scale);
    let content_width = (start_width * scale).clamp(MIN_CONTENT_WIDTH, MAX_CONTENT_WIDTH);
    let content_height = (content_width / aspect).clamp(MIN_CONTENT_HEIGHT, MAX_CONTENT_HEIGHT);
    let width = content_width + SHADOW_EXTENT as f32;
    let height = content_height + SHADOW_EXTENT as f32;
    let (left, top) = match corner {
        ResizeCorner::TopLeft => (start_rect.right() - width, start_rect.bottom() - height),
        ResizeCorner::TopRight => (start_rect.left, start_rect.bottom() - height),
        ResizeCorner::BottomLeft => (start_rect.right() - width, start_rect.top),
        ResizeCorner::BottomRight => (start_rect.left, start_rect.top),
    };
    ScreenRect {
        left,
        top,
        width,
        height,
    }
}

unsafe extern "system" fn pin_window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let state_ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut NativePinState;
    if state_ptr.is_null() {
        // SAFETY: state is attached immediately after CreateWindowExW returns.
        return unsafe { DefWindowProcW(hwnd, message, wparam, lparam) };
    }

    match message {
        WM_LBUTTONDOWN => {
            if let Some(point) = screen_cursor_position() {
                // SAFETY: GWLP_USERDATA owns this unique Box; the borrow ends before SetCapture.
                let should_boost = unsafe { (&mut *state_ptr).pointer_down(point) };
                // SAFETY: hwnd is live, and capture is released on button-up or capture loss.
                unsafe {
                    SetCapture(hwnd);
                    if should_boost {
                        let _ = DCompositionBoostCompositorClock(true);
                    }
                }
            }
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            // SAFETY: GWLP_USERDATA owns this unique Box for the duration of the message.
            let state = unsafe { &mut *state_ptr };
            state.begin_mouse_tracking();
            if let Some(point) = screen_cursor_position() {
                state.pointer_move(point);
            }
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            let close_clicked = {
                // SAFETY: the mutable borrow ends before ReleaseCapture or DestroyWindow.
                let state = unsafe { &mut *state_ptr };
                if let Some(point) = screen_cursor_position() {
                    state.pointer_up(point)
                } else {
                    state.cancel_interaction();
                    false
                }
            };
            // SAFETY: no state borrow is live while capture-change messages can re-enter.
            let _ = unsafe { ReleaseCapture() };
            if close_clicked {
                // SAFETY: WM_NCDESTROY detaches and releases the state.
                let _ = unsafe { DestroyWindow(hwnd) };
            }
            LRESULT(0)
        }
        WM_CAPTURECHANGED => {
            // SAFETY: the state remains attached unless WM_NCDESTROY is being handled.
            unsafe { (&mut *state_ptr).cancel_interaction() };
            LRESULT(0)
        }
        WM_MOUSELEAVE_MESSAGE => {
            // SAFETY: GWLP_USERDATA owns this unique Box for the duration of the message.
            unsafe { (&mut *state_ptr).mouse_left() };
            LRESULT(0)
        }
        WM_MOUSEWHEEL => {
            // SAFETY: GetKeyState reads the current modifier state.
            if unsafe { GetKeyState(VK_CONTROL.0 as i32) } < 0
                && let Some(point) = screen_cursor_position()
            {
                // SAFETY: GWLP_USERDATA owns this unique Box for the duration of the message.
                unsafe { (&mut *state_ptr).wheel_zoom(point, high_word_signed(wparam.0)) };
                LRESULT(0)
            } else {
                // SAFETY: unhandled wheel input uses system default behavior.
                unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
            }
        }
        WM_SETCURSOR => {
            if let Some(point) = screen_cursor_position() {
                // SAFETY: GWLP_USERDATA owns this unique Box for the duration of the message.
                unsafe { (&*state_ptr).update_cursor(point) };
                LRESULT(1)
            } else {
                // SAFETY: cursor fallback uses system default behavior.
                unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
            }
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_PAINT => {
            // SAFETY: DirectComposition owns all visible pixels; validate without repainting.
            let _ = unsafe { ValidateRect(Some(hwnd), None) };
            LRESULT(0)
        }
        WM_CLOSE => {
            // SAFETY: WM_NCDESTROY detaches and releases the state.
            let _ = unsafe { DestroyWindow(hwnd) };
            LRESULT(0)
        }
        WM_NCDESTROY => {
            // SAFETY: detach first to prevent re-entrancy, then release the unique Box.
            unsafe {
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                drop(Box::from_raw(state_ptr));
            }
            // SAFETY: the system still owns the HWND teardown after user state is released.
            unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
        }
        _ => {
            // SAFETY: unhandled messages use the system default behavior without touching state.
            unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
        }
    }
}

fn create_d3d_device() -> Result<(ID3D11Device, ID3D11DeviceContext), String> {
    let create = |driver_type| {
        let mut device = None;
        let mut context = None;
        // SAFETY: output pointers are valid and the system selects a supported feature level.
        let result = unsafe {
            D3D11CreateDevice(
                None,
                driver_type,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
        };
        result.map(|()| (device, context))
    };

    let (device, context) = create(D3D_DRIVER_TYPE_HARDWARE)
        .or_else(|_| create(D3D_DRIVER_TYPE_WARP))
        .map_err(|error| format!("无法创建 D3D11 设备：{error}"))?;
    Ok((
        device.ok_or_else(|| "D3D11 未返回设备。".to_string())?,
        context.ok_or_else(|| "D3D11 未返回立即上下文。".to_string())?,
    ))
}

fn ensure_pin_class() -> Result<(), String> {
    match PIN_CLASS.get_or_init(register_pin_class) {
        Ok(_) => Ok(()),
        Err(error) => Err(error.clone()),
    }
}

fn register_pin_class() -> Result<u16, String> {
    let instance = module_instance()?;
    // SAFETY: loading a shared system cursor does not transfer ownership.
    let cursor = unsafe { LoadCursorW(None, IDC_ARROW) }
        .map_err(|error| format!("无法加载贴图窗口光标：{error}"))?;
    let class = WNDCLASSW {
        style: CS_DBLCLKS,
        lpfnWndProc: Some(pin_window_proc),
        hInstance: instance,
        hCursor: cursor,
        lpszClassName: PIN_CLASS_NAME,
        ..Default::default()
    };
    // SAFETY: class fields remain valid for the duration of registration.
    let atom = unsafe { RegisterClassW(&class) };
    if atom == 0 {
        Err(format!(
            "无法注册原生贴图窗口类：{}",
            windows::core::Error::from_win32()
        ))
    } else {
        Ok(atom)
    }
}

fn module_instance() -> Result<HINSTANCE, String> {
    // SAFETY: None requests the current executable module.
    let module = unsafe { GetModuleHandleW(None) }
        .map_err(|error| format!("无法获取应用模块句柄：{error}"))?;
    Ok(HINSTANCE(module.0))
}

fn screen_cursor_position() -> Option<POINT> {
    let mut point = POINT::default();
    // SAFETY: point is writable for the duration of the call.
    unsafe { GetCursorPos(&mut point) }.ok()?;
    Some(point)
}

fn high_word_signed(value: usize) -> i16 {
    ((value >> 16) & 0xffff) as u16 as i16
}

fn rgba_to_premultiplied_bgra(rgba: &[u8]) -> Vec<u8> {
    let mut output = Vec::with_capacity(rgba.len());
    for pixel in rgba.chunks_exact(4) {
        let alpha = pixel[3] as u16;
        output.push(((pixel[2] as u16 * alpha + 127) / 255) as u8);
        output.push(((pixel[1] as u16 * alpha + 127) / 255) as u8);
        output.push(((pixel[0] as u16 * alpha + 127) / 255) as u8);
        output.push(pixel[3]);
    }
    output
}

fn close_button_pixels(size: u32) -> Vec<u8> {
    let mut pixels = vec![0_u8; size as usize * size as usize * 4];
    let radius = (size as f32 * 0.20).max(3.0);
    let center = (size as f32 - 1.0) / 2.0;
    let line_half = (size as f32 * 0.24).max(4.0);
    let line_width = (size as f32 * 0.075).max(1.5);

    for y in 0..size {
        for x in 0..size {
            let inside = rounded_rect_contains(x as f32, y as f32, size as f32, radius);
            let dx = x as f32 - center;
            let dy = y as f32 - center;
            let on_x = dx.abs() <= line_half
                && dy.abs() <= line_half
                && ((dx - dy).abs() <= line_width || (dx + dy).abs() <= line_width);
            let color = if on_x {
                [255, 255, 255, 255]
            } else if inside {
                [36, 39, 43, 235]
            } else {
                [0, 0, 0, 0]
            };
            write_bgra_pixel(&mut pixels, size, x, y, color);
        }
    }
    pixels
}

fn resize_handle_pixels(size: u32) -> Vec<u8> {
    let mut pixels = vec![0_u8; size as usize * size as usize * 4];
    let border = (size / 6).max(2);
    for y in 0..size {
        for x in 0..size {
            let edge = x < border || y < border || x >= size - border || y >= size - border;
            let color = if edge {
                [16, 185, 129, 255]
            } else {
                [255, 255, 255, 255]
            };
            write_bgra_pixel(&mut pixels, size, x, y, color);
        }
    }
    pixels
}

fn rounded_rect_contains(x: f32, y: f32, size: f32, radius: f32) -> bool {
    let clamped_x = x.clamp(radius, size - radius);
    let clamped_y = y.clamp(radius, size - radius);
    let dx = x - clamped_x;
    let dy = y - clamped_y;
    dx * dx + dy * dy <= radius * radius
}

fn write_bgra_pixel(pixels: &mut [u8], width: u32, x: u32, y: u32, rgba: [u8; 4]) {
    let index = ((y * width + x) * 4) as usize;
    let alpha = rgba[3] as u16;
    pixels[index] = ((rgba[2] as u16 * alpha + 127) / 255) as u8;
    pixels[index + 1] = ((rgba[1] as u16 * alpha + 127) / 255) as u8;
    pixels[index + 2] = ((rgba[0] as u16 * alpha + 127) / 255) as u8;
    pixels[index + 3] = rgba[3];
}

#[cfg(test)]
mod tests {
    use super::{
        PinCompositor, PinFrame, ResizeCorner, ScreenRect, destroy_pin_window, hide_pin_window,
        high_word_signed, resize_from_corner, rgba_to_premultiplied_bgra,
    };
    use windows::Win32::Foundation::{POINT, RECT};
    use windows::Win32::UI::WindowsAndMessaging::{
        GWLP_USERDATA, GetWindowLongPtrW, GetWindowRect,
    };
    use windows::core::Interface;

    struct PinWindowGuard(windows::Win32::Foundation::HWND);

    impl Drop for PinWindowGuard {
        fn drop(&mut self) {
            destroy_pin_window(self.0);
        }
    }

    #[test]
    fn converts_rgba_to_premultiplied_bgra() {
        assert_eq!(
            rgba_to_premultiplied_bgra(&[200, 100, 50, 128]),
            vec![25, 50, 100, 128]
        );
    }

    #[test]
    fn reads_signed_wheel_delta() {
        assert_eq!(high_word_signed((120_u16 as usize) << 16), 120);
        assert_eq!(high_word_signed(((-120_i16) as u16 as usize) << 16), -120);
    }

    #[test]
    fn native_corner_resize_preserves_content_aspect() {
        let start = ScreenRect {
            left: 100.0,
            top: 100.0,
            width: 402.0,
            height: 202.0,
        };
        let resized = resize_from_corner(
            start,
            POINT { x: 100, y: 100 },
            POINT { x: 0, y: 50 },
            ResizeCorner::TopLeft,
            2.0,
        );
        let ratio = resized.content_width() / resized.content_height();
        assert!((ratio - 2.0).abs() < 0.001);
        assert_eq!(resized.right(), start.right());
        assert_eq!(resized.bottom(), start.bottom());
    }

    #[test]
    fn native_resize_reuses_the_persistent_image_surface() -> Result<(), String> {
        const SOURCE_WIDTH: u32 = 320;
        const SOURCE_HEIGHT: u32 = 180;
        let mut rgba = vec![0_u8; (SOURCE_WIDTH * SOURCE_HEIGHT * 4) as usize];
        for (index, pixel) in rgba.chunks_exact_mut(4).enumerate() {
            pixel[0] = (index % SOURCE_WIDTH as usize) as u8;
            pixel[1] = (index / SOURCE_WIDTH as usize) as u8;
            pixel[2] = 160;
            pixel[3] = 255;
        }

        let compositor = PinCompositor::new()?;
        let hwnd = compositor.create_pin(PinFrame {
            rgba: &rgba,
            source_width: SOURCE_WIDTH,
            source_height: SOURCE_HEIGHT,
            display_width: SOURCE_WIDTH,
            display_height: SOURCE_HEIGHT,
            origin_x: -10_000,
            origin_y: -10_000,
        })?;
        let _window = PinWindowGuard(hwnd);
        hide_pin_window(hwnd);

        // SAFETY: create_pin attaches a NativePinState Box to this live test HWND.
        let state_ptr =
            unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut super::NativePinState;
        if state_ptr.is_null() {
            return Err("测试贴图没有关联原生状态。".to_string());
        }
        // SAFETY: the test owns the HWND and does not dispatch competing state mutations.
        let initial_surface = unsafe { (&*state_ptr).composition._image_surface.as_raw() };

        for step in 0..60 {
            let width = 320.0 + step as f32 * 4.0;
            let height = width * SOURCE_HEIGHT as f32 / SOURCE_WIDTH as f32;
            // SAFETY: the test is the sole mutator and the HWND remains guarded.
            unsafe {
                (&mut *state_ptr).apply_rect(ScreenRect {
                    left: -10_000.0,
                    top: -10_000.0,
                    width: width + super::SHADOW_EXTENT as f32,
                    height: height + super::SHADOW_EXTENT as f32,
                })?;
            }
        }

        // SAFETY: the state and its composition tree remain alive through the HWND guard.
        let state = unsafe { &*state_ptr };
        state.composition.wait_for_commit_completion()?;
        assert_eq!(
            state.composition._image_surface.as_raw(),
            initial_surface,
            "连续缩放不得替换或重新上传截图 Surface"
        );

        let mut rect = RECT::default();
        // SAFETY: hwnd is live and rect is writable for this call.
        unsafe { GetWindowRect(hwnd, &mut rect) }
            .map_err(|error| format!("无法读取测试贴图尺寸：{error}"))?;
        assert_eq!(rect.right - rect.left, 558);
        assert_eq!(rect.bottom - rect.top, 315);
        Ok(())
    }

    #[test]
    fn one_compositor_owns_multiple_independent_pin_windows() -> Result<(), String> {
        let rgba = vec![255_u8; 64 * 40 * 4];
        let compositor = PinCompositor::new()?;
        let create = |origin_x| {
            compositor.create_pin(PinFrame {
                rgba: &rgba,
                source_width: 64,
                source_height: 40,
                display_width: 160,
                display_height: 100,
                origin_x,
                origin_y: -10_000,
            })
        };

        let first = create(-10_000)?;
        let _first_guard = PinWindowGuard(first);
        let second = create(-9_800)?;
        let _second_guard = PinWindowGuard(second);
        hide_pin_window(first);
        hide_pin_window(second);

        assert_ne!(first, second);
        assert!(super::is_pin_window(first));
        assert!(super::is_pin_window(second));
        super::show_pin_window(second);
        destroy_pin_window(first);
        assert!(!super::is_pin_window(first));
        assert!(super::is_pin_window(second));
        Ok(())
    }
}
