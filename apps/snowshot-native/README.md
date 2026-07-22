# Snow Shot Native Lite UI Preview

这是 Snow UI 设计系统的原生 Slint 预览程序，目前只包含设置窗口骨架和基础组件，尚未接入截图、托盘、快捷键、OCR 或现有 Rust Core。

## 运行

```bash
cd apps/snowshot-native
cargo run
```

## 文件

- `ui/snow-ui.slint`：主题 Token 与基础组件
- `ui/app-window.slint`：设置窗口骨架
- `../../design/snow-ui.tokens.json`：框架无关的设计 Token
- `../../docs/native-lite-design-system.md`：设计和交互规范

当前程序保持为独立 Cargo package，暂不加入现有 `src-tauri` workspace，避免在 Rust Core 尚未抽离完成前影响原 Tauri 构建。

## 下一步

1. 将现有截图服务抽成不依赖 Tauri 的 Rust API。
2. 接入托盘和全局快捷键。
3. 将截图结果交给原生窗口显示。
4. 接入轻量 OCR Provider。
5. 完成截图浮动工具栏和标注画布。
