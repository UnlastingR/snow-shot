# Snow Shot Native Lite UI Preview

这是 Snow UI 设计系统的原生 Slint 应用入口。Windows 版已接入第一条真实运行链路：

- `Alt+F12` 全局快捷键冻结鼠标所在显示器，框选后写入 Windows 剪贴板。
- 触发区域截图时保持设置窗口可见，并将它原样包含在冻结画面中。
- 浮动工具栏支持复制、PNG 原生保存和多窗口置顶贴图。
- 选区完成后可在框内拖动，或从四角缩放；Shift 保持比例，Ctrl 保持中心，两个修饰键可组合。
- 贴图支持拖动、四角等比缩放，以及鼠标位于贴图上时使用 Ctrl+滚轮缩放。
- 每张贴图可独立关闭；右侧和下侧带 2px 模糊阴影。
- `Alt+F11` 全局隐藏或显示当前全部贴图。
- 托盘菜单保留直接复制鼠标所在显示器的备用入口。
- 托盘菜单支持截图、打开设置和退出。
- 关闭设置窗口时隐藏到托盘，不退出后台运行时。
- 设置页“开始截图”按钮复用同一 Rust workflow。

当前仍未接入智能窗口识别、标注画布、本地 OCR 页面和配置持久化。

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
- `src/capture_workflow.rs`：截图 Core 到剪贴板、PNG 保存和贴图的应用级 workflow
- `../../design/snow-ui.tokens.json`：框架无关的设计 Token
- `../../docs/native-lite-design-system.md`：设计和交互规范

当前程序保持为独立 Cargo package，暂不加入现有 `src-tauri` workspace，避免在 Rust Core 尚未抽离完成前影响原 Tauri 构建。

## 平台范围

- 主开发与发布平台：Windows 11 amd64
- 第二适配平台：macOS arm64
- 暂不适配其他系统和架构

## 下一步

1. 补齐智能窗口识别。
2. 迁移现有 `OcrService`、ONNX Runtime 与 PP-OCRv4 模型路径。
3. 接入配置持久化与开机启动。
4. 完成标注画布和撤销/重做。
