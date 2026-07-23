# Snow Shot Native Lite UI Preview

这是 Snow UI 设计系统的原生 Slint 应用入口。Windows 版已接入第一条真实运行链路：

- `Alt+F12` 全局快捷键冻结鼠标所在显示器，框选后写入 Windows 剪贴板。
- 托盘菜单保留直接复制鼠标所在显示器的备用入口。
- 托盘菜单支持截图、打开设置和退出。
- 关闭设置窗口时隐藏到托盘，不退出后台运行时。
- 设置页“开始截图”按钮复用同一 Rust workflow。

当前仍未接入区域选框、标注画布、贴图、OCR 页面和配置持久化。

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
- `src/capture_workflow.rs`：截图 Core 到剪贴板 Core 的应用级 workflow
- `../../design/snow-ui.tokens.json`：框架无关的设计 Token
- `../../docs/native-lite-design-system.md`：设计和交互规范

当前程序保持为独立 Cargo package，暂不加入现有 `src-tauri` workspace，避免在 Rust Core 尚未抽离完成前影响原 Tauri 构建。

## 平台范围

- 主开发与发布平台：Windows 11 amd64
- 第二适配平台：macOS arm64
- 暂不适配其他系统和架构

## 下一步

1. 接入区域选框和截图浮动工具栏。
2. 将截图结果交给原生窗口显示并支持保存。
3. 接入贴图窗口。
4. 迁移现有 `OcrService`、ONNX Runtime 与 PP-OCRv4 模型路径。
5. 完成标注画布和撤销/重做。
