# Snow Shot Native Lite 重构路线

## 目标

- 保留现有 Rust 截图、窗口、剪贴板、全局快捷键与图像处理核心。
- 移除 Tauri WebView、React、Ant Design、Excalidraw 与 AI/翻译等非核心功能。
- 默认空闲常驻内存目标：Windows 25–45 MB；macOS/Linux 30–60 MB。
- 截图编辑时目标：不含 OCR 模型时低于 100 MB。
- OCR 按需加载，用完释放；主程序不捆绑大模型。

## 功能范围

### 保留

- 区域截图、全屏截图、窗口截图
- 智能窗口/元素识别
- 置顶贴图
- 基础标注：矩形、椭圆、箭头、画笔、文字、马赛克
- 复制、保存、撤销/重做
- 滚动截图（第二阶段）
- OCR
- 托盘、全局快捷键、开机启动

### 移除

- AI 对话
- 翻译
- 通用插件系统和热加载页面
- 视频录制（首版移除，可后续独立插件化）
- S3 上传和内置 HTTP 服务（首版移除）
- WebView 共享缓冲区与全部前端网页依赖

## UI 技术路线

首选 `Slint + wgpu`：

- Slint：设置页、托盘窗口、截图工具栏、OCR 结果面板
- wgpu：截图画布、标注图层和 GPU 合成
- Rust：屏幕捕获、窗口识别、图像编码、剪贴板、配置、OCR 调度

若 Slint 在复杂文本编辑或无障碍方面无法满足要求，备选为 Qt Quick/QML；Rust 核心保持独立，不与 UI 框架耦合。

## Rust 工作区建议

```text
crates/
  snowshot-core/       # 纯业务接口和数据结构
  snowshot-capture/    # xcap/scap 与各平台截图实现
  snowshot-platform/   # 快捷键、托盘、窗口、剪贴板
  snowshot-canvas/     # 标注模型、撤销栈、wgpu 渲染
  snowshot-ocr/        # OCR provider trait 与调度
  snowshot-storage/    # 配置、历史记录、模型清单
apps/
  snowshot-native/     # Slint 桌面应用
```

核心 crate 不允许依赖 Tauri、WebView 或 JavaScript 运行时。

## OCR 架构

定义统一 Provider：

```rust
pub trait OcrProvider: Send + Sync {
    fn id(&self) -> &'static str;
    fn is_available(&self) -> bool;
    async fn recognize(&self, image: OcrImage) -> Result<OcrResult, OcrError>;
    async fn unload(&self) -> Result<(), OcrError> { Ok(()) }
}
```

### 默认：轻量本地 OCR

- 保留当前 ONNX Runtime 路线。
- 升级到 PP-OCRv5 mobile 或兼容的轻量 ONNX 模型。
- 模型不常驻内存；首次 OCR 时加载，空闲后自动释放。
- 安装包可不包含模型，首次使用时让用户选择下载。

### 可选：PaddleOCR-VL

用户口中的“百度 ultimate OCR”应为百度飞桨的 **PaddleOCR-VL**。它是 0.9B 级视觉语言模型，适合复杂文档、公式、表格和版面解析，不适合直接嵌入几十 MB 常驻的小工具。

提供两种可选接入：

1. 远程 API：用户填写服务地址、模型名和密钥。
2. 本地服务：应用检测本机 Docker/Python 服务，并连接 `http://127.0.0.1:<port>`；安装、下载和运行均需用户主动确认。

主程序只包含 HTTP 客户端和服务管理元数据，不捆绑 Paddle/Python/大模型。

## 内存策略

- 启动时不初始化 OCR、视频、插件、HTTP 服务或截图缓存。
- 主窗口按需创建，关闭后销毁而不是隐藏 WebView。
- 截图 RGBA 缓冲区使用 `Arc<[u8]>`/GPU texture 避免多次复制。
- 撤销栈保存操作参数和局部 tile，不为每一步保存完整位图。
- OCR 会话按需加载，60–120 秒无请求后释放。
- 缩略图使用尺寸上限与 LRU；历史记录不在内存中持有原图。
- Release 使用 `panic = "abort"`、LTO、strip；按平台裁剪依赖特性。

## 阶段计划

### Phase 0：基线测量

- 记录当前空闲、主窗口、截图、编辑、OCR 峰值内存。
- 用 Windows Performance Recorder/Process Explorer、macOS Instruments、Linux heaptrack 建立基线。

### Phase 1：抽离 Rust Core

- 将 Tauri command 变成普通 Rust service API。
- 为截图、剪贴板、窗口识别和 OCR 添加最小集成测试。
- 保持原 Tauri 前端可运行，作为迁移期间的行为对照。

### Phase 2：Native Lite MVP

- Slint 主窗口、托盘和快捷键。
- 区域/窗口/全屏截图。
- 保存、复制、贴图。
- 轻量 OCR。

### Phase 3：原生标注画布

- wgpu 渲染与命中测试。
- 矩形、箭头、画笔、文字、马赛克。
- 撤销/重做与高 DPI 多显示器适配。

### Phase 4：可选 PaddleOCR-VL

- Provider 配置页面。
- 远程 API 接入。
- 可选本地服务安装向导和健康检查。

### Phase 5：优化与发布

- 删除 Tauri、React、Excalidraw 与插件系统。
- 三平台内存回归测试。
- 发布 native-lite 预览版。

## 验收标准

- 无 WebView2/Chromium 子进程。
- AI、翻译、插件、视频和上传相关代码不进入最终二进制。
- 默认安装不下载任何大模型。
- 不启用 OCR 时，空闲常驻内存稳定在目标范围内。
- OCR 释放后，工作集能显著回落。
- Windows/macOS/Linux 均能完成截图、标注、复制和保存闭环。
