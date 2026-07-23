# Snow Shot Native Lite UI Preview

这是 Snow UI 设计系统的原生 Slint 应用入口。Windows 版已接入第一条真实运行链路：

- `Alt+F12` 全局快捷键冻结鼠标所在显示器，框选后写入 Windows 剪贴板。
- 光标悬停时智能识别最上层窗口，单击即可选中完整窗口；目标切换使用轻量 S 曲线过渡，持续移动不会重复重启动画。
- 触发区域截图时保持设置窗口可见，并将它原样包含在冻结画面中。
- 覆盖层使用不透明全屏窗口与四块外围蒙层，框选期间每 8ms 合并一次指针更新，选区内部始终实时显示冻结原图。
- 每次截图创建全新的 FemtoVG 覆盖窗口，并在完成或取消后销毁；不复用经历过 hide/show 的 GPU 渲染表面。
- Esc 同时由窗口焦点和截图期间的非侵入式按键状态检测处理。
- 浮动工具栏支持复制、PNG 原生保存和多窗口置顶贴图。
- 选区完成后可在框内拖动，或从上下左右及四角缩放；Shift 保持比例，Ctrl 保持中心，两个修饰键可组合。
- 贴图支持拖动、不可见四角命中区等比缩放，以及鼠标位于贴图上时使用 Ctrl+滚轮缩放。
- 贴图最小保持初始显示尺寸的 30%，且宽高不低于 96×64 物理像素，避免阴影和关闭按钮在过小尺寸下失真。
- 等比缩放将光标位移连续投影到对角线方向，不在横向/纵向控制之间切换。
- 每张贴图可独立关闭；右侧和下侧使用 6px 高斯模糊、完整衰减且点击穿透的柔和阴影。
- Windows 贴图使用原生 Win32 + DirectComposition：截图纹理只上传一次，缩放期间只提交 GPU 合成变换，不触发 Slint 窗口重绘。
- `Alt+F11` 全局隐藏或显示当前全部贴图。
- 托盘菜单保留直接复制鼠标所在显示器的备用入口。
- 托盘菜单支持截图、打开设置和退出。
- 关闭设置窗口时隐藏到托盘，不退出后台运行时。
- 设置页“开始截图”按钮复用同一 Rust workflow。

当前仍未接入标注画布、本地 OCR 页面和配置持久化。

## 运行

```bash
cd apps/snowshot-native
cargo run
```

## 文件

- `ui/theme/`：颜色、间距、字号和尺寸 Token
- `ui/components/`：无业务状态的按钮、导航、分隔线和设置行
- `ui/pages/`：设置壳与页面组合，只向窗口层暴露业务 callback
- `ui/native-app.slint`：Slint 编译入口，统一导出窗口和托盘
- `ui/app-tray.slint`：系统托盘与菜单 callback
- `ui/app-window.slint`：窗口属性、页面挂载和 Rust callback 边界
- `assets/tray-icon.svg`：Native App 自有托盘资源，不依赖 Tauri 图标目录
- `src/windows_runtime.rs`：Windows 快捷键、托盘 callback 和后台任务调度
- `src/windows_pin.rs`：Windows 原生贴图窗口、DirectComposition 视觉树和输入命中
- `src/capture_workflow.rs`：截图 Core 到剪贴板、PNG 保存和贴图的应用级 workflow
- `../../design/snow-ui.tokens.json`：框架无关的设计 Token
- `../../docs/native-lite-design-system.md`：设计和交互规范

当前程序保持为独立 Cargo package，暂不加入现有 `src-tauri` workspace，避免在 Rust Core 尚未抽离完成前影响原 Tauri 构建。

## 平台范围

- 主开发与发布平台：Windows 11 amd64
- 第二适配平台：macOS arm64
- 暂不适配其他系统和架构

## 下一步

1. 迁移现有 `OcrService`、ONNX Runtime 与 PP-OCRv4 模型路径。
2. 接入配置持久化与开机启动。
3. 完成标注画布和撤销/重做。
