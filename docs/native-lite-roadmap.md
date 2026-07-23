# Snow Shot Native Lite 重构路线

## 目标

- 保留现有 Rust 截图、窗口、剪贴板、全局快捷键与图像处理核心。
- 移除 Tauri WebView、React、Ant Design、Excalidraw 与 AI/翻译等非核心功能。
- 默认空闲常驻内存目标：Windows 11 amd64 为 25–45 MB；macOS arm64 为 30–60 MB。
- 截图编辑时目标：不含 OCR 模型时低于 100 MB。
- 沿用现有本地 OCR 实现，按需加载并在空闲后释放。

## 平台范围

- 第一开发与发布平台：Windows 11 amd64（`x86_64-pc-windows-msvc`）。
- 第二适配平台：macOS arm64（`aarch64-apple-darwin`）。
- 暂不适配 Linux、Windows arm64 和 macOS x86_64，也不将这些平台纳入当前发布验收。
- 核心接口继续隔离平台实现，但当前阶段不为范围外平台投入开发和测试资源。

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
  snowshot-ocr/        # 现有 OcrService 与 ONNX Runtime 路径
  snowshot-storage/    # 配置、历史记录、模型清单
apps/
  snowshot-native/     # Slint 桌面应用
```

核心 crate 不允许依赖 Tauri、WebView 或 JavaScript 运行时。

## OCR 路线

不新增 OCR Provider 抽象，不接入 PaddleOCR-VL、本地 Python/Docker 服务或远程 OCR API。沿用当前实现：

- 复用 `src-tauri/src-crates/app-services/src/ocr_service.rs` 中的 `OcrService`。
- 保留 `paddle-ocr-rs`、ONNX Runtime 和现有 PP-OCRv4 检测/识别模型及方向分类模型。
- 保留初始化、识别、释放、自定义模型文件、热启动和模型写入内存等现有能力。
- 抽离 Rust Core 时只移除 Tauri command 包装层，OCR 行为、模型格式和结果结构保持兼容。
- 将现有 OCR 模型目录和配置迁移到 Native Lite，不保留通用插件系统。
- 默认不预热模型；首次使用时加载，空闲 60–120 秒后释放会话。

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
- Windows 11 amd64 使用 Windows Performance Recorder/Process Explorer 建立主基线。
- macOS arm64 使用 Instruments 建立第二平台基线。

### Phase 1：抽离 Rust Core

- 将 Tauri command 变成普通 Rust service API。
- 为截图、剪贴板、窗口识别和 OCR 添加最小集成测试。
- 保持原 Tauri 前端可运行，作为迁移期间的行为对照。

当前进度：

- 已将现有 `OcrService`、图像预处理和识别结果类型抽离到独立的 `crates/snowshot-ocr`。
- 旧 `snow-shot-app-services::ocr_service` 路径继续兼容，Tauri OCR command 仅保留 IPC 适配。
- 已覆盖默认/自定义模型路径及 RGBA 转换单测；下一步抽离截图服务。

### Phase 2：Native Lite MVP

- Slint 主窗口、托盘和快捷键。
- 区域/窗口/全屏截图。
- 保存、复制、贴图。
- 接入现有 `OcrService` 本地 OCR。

### Phase 3：原生标注画布

- wgpu 渲染与命中测试。
- 矩形、箭头、画笔、文字、马赛克。
- 撤销/重做与高 DPI 多显示器适配。

### Phase 4：优化与发布

- 删除 Tauri、React、Excalidraw 与插件系统。
- 完成 Windows 11 amd64 主平台回归测试。
- 完成 macOS arm64 第二平台适配与回归测试。
- 发布 native-lite 预览版。

## 验收标准

- 无 WebView2/Chromium 子进程。
- AI、翻译、插件、视频和上传相关代码不进入最终二进制。
- 默认安装不下载任何大模型。
- 不启用 OCR 时，空闲常驻内存稳定在目标范围内。
- OCR 释放后，工作集能显著回落。
- Windows 11 amd64 完成截图、标注、OCR、复制和保存闭环。
- macOS arm64 完成同等功能闭环，且不存在 Rosetta 运行依赖。
