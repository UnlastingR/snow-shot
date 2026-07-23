# Capture Flicker Lab

这是一个与 Snow Shot 主程序完全分离的 Windows 全屏选区闪烁实验。所有用例都：

- 使用鼠标当前所在显示器；
- 显示不透明的合成测试图，不读取屏幕内容；
- 按住左键快速拖动选区；
- 按 `Esc` 隐藏当前截图层，进程继续驻留；
- 按 `Alt+F12` 重置并重新打开截图层，可反复测试；
- 按 `Alt+Shift+F12` 结束当前用例；
- 不修改剪贴板。

## 测试顺序

请确保同一时间只运行一个用例，避免争抢 `Alt+F12`。依次运行，每个用例可重复
“拖动 5～10 秒 → `Esc` → `Alt+F12`”多轮，并记录“闪烁 / 不闪烁”：

1. `01-femtovg-border.exe`
   - Slint + Winit + FemtoVG/OpenGL
   - 只移动边框，没有动态蒙层
2. `02-femtovg-masks.exe`
   - Slint + Winit + FemtoVG/OpenGL
   - 四块动态蒙层，与 Snow Shot 当前选区呈现方式一致
3. `03-software-masks.exe`
   - Slint + Winit + software renderer
   - 与 02 完全相同的四块动态蒙层
4. `04-win32-gdi.exe`
   - 原生 Win32 + GDI 内存双缓冲
   - 不创建 Slint、Winit 或 OpenGL 对象

## 结果解释

| 结果 | 优先判断 |
| --- | --- |
| 01 就闪烁 | FemtoVG/OpenGL 的全屏连续 Present 路径 |
| 01 正常，02 闪烁 | 动态蒙层或局部损伤更新 |
| 02 闪烁，03 正常 | FemtoVG/OpenGL 或显卡驱动路径 |
| 03 闪烁，04 正常 | Slint/Winit 的窗口或软件呈现路径 |
| 04 也闪烁 | Windows/DWM/驱动层，或显示链路本身 |
| 四项都正常 | Snow Shot 的真实冻结纹理、会话切换或 DPI 路径 |
| 01/02 首次正常但重开闪，03/04 重开正常 | FemtoVG hide/show 表面复用；正式版不得再次显示已经隐藏过的捕获窗口 |
