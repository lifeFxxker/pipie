# PiPie

<p align="left">
  <a href="https://github.com/lifeFxxker/pipie/releases"><img src="https://img.shields.io/github/v/release/lifeFxxker/pipie?color=black&label=release" alt="Release"></a>
  <img src="https://img.shields.io/badge/platform-Windows%2010%2F11%20x64-1f1f1f" alt="Platform">
  <img src="https://img.shields.io/badge/language-Rust-1f1f1f" alt="Rust">
  <img src="https://img.shields.io/badge/license-MIT-1f1f1f" alt="License">
</p>

**[English](README.en.md) · 简体中文**

pipie 是一款 Windows 的轮盘启动器。按鼠标侧键，轮盘在光标处弹出；悬停应用，它的窗口贴着应用展开，带实时预览。单文件、无运行时、无安装程序。

## 演示

![交互演示](design/preview-demo.gif)

悬停应用，外环展开它的窗口；按住内环图标拖动即可排序。

## 资源占用

实测环境：Windows 11（build 26200）、i3-12100F（4 核 8 线程）、16 GB 内存、`PiPie.exe` release 构建（745 KB）。

| 状态 | CPU | 内存（工作集） | 提交内存 | 句柄 | 线程 |
| --- | --- | --- | --- | --- | --- |
| 待机（仅托盘，20 秒平均） | 0.01% | 0.2 MB | 11.4 MB | 370 | 13 |
| 轮盘打开（5 秒平均） | 2.15% | 4.9 MB | 13.1 MB | 366 | 12 |

- CPU 是整机占用百分比，不是单核百分比。
- 待机工作集只有 0.2 MB，是因为轮盘隐藏后程序主动把页面交还系统（`SetProcessWorkingSetSize`）。真实占用看提交内存 11.4 MB，代价是首次呼出会有一次轻微缺页。
- 打开时的 2% 来自软件渲染与玻璃背景采样，画面是逐帧重绘的。

## 特性

- **在光标处弹出** —— 不用把鼠标挪到屏幕角落。
- **内外两层** —— 内环放应用，外环把它当前打开的窗口排成胶囊，贴着应用而不是散满一圈。
- **实时窗口预览** —— 悬停胶囊显示 DWM 实时画面，最小化的窗口也有最后一帧，不是截图。
- **拖进去就固定** —— 快捷方式、`.exe`、文件夹、文件都能拖到内环；排序、重命名、移除在同一界面完成。
- **最多 24 个应用** —— 内环一屏 12 格，超出后滚轮连续滚动，图标在缝隙处淡入淡出。
- **单文件零依赖** —— 745 KB 的 `PiPie.exe`，不需要 .NET 或任何运行时。
- **多种呼出方式** —— 默认鼠标侧键，可换中键、长按右键，或录制组合键。

## 截图

| 内环 | 外环 —— 某应用的打开窗口 |
| :---: | :---: |
| ![内环](design/preview-ring.png) | ![外环](design/preview-chips.png) |
| 正在运行的应用，图标下方有一个圆点。 | 悬停应用，它的窗口就以胶囊形式贴着它展开。 |

| 实时窗口预览 | 设置 |
| :---: | :---: |
| ![实时预览](design/preview-live.png) | ![设置](design/preview-apps.png) |
| 悬停窗口胶囊，显示 DWM 实时缩略图。 | 扁平、纸面风格的偏好设置。 |

> 未签名，首次运行 SmartScreen 会提示"更多信息 → 仍要运行"。只从本仓库 Releases 下载。

## 安装

从 [Releases](https://github.com/lifeFxxker/pipie/releases) 下载 `pipie-vX.Y.Z-win64.zip`，解压到任意目录，运行 `PiPie.exe`。无需安装。

### 从源码构建

需要 Windows 10/11 x64、[Rust](https://rustup.rs/)、MSVC 工具链（`winget install Microsoft.VisualStudio.BuildTools`）与 Windows SDK（用于嵌入图标，缺失时跳过）。

```bash
cargo build --release      # -> target/release/PiPie.exe
python package.py          # -> dist/pipie-v<版本>-win64.zip
```

> **不要改掉 `Cargo.toml` 里的 `lto = "thin"`。** 换成 fat LTO（`"true"`）后当前工具链会错误编译本项目的 `unsafe` FFI，启动即崩（`0xC0000409`）。

## 使用

- **呼出** —— 按住鼠标侧键，在光标处弹出轮盘；松开启动高亮的应用，按 `Esc` 或其它键取消。
- **固定** —— 把快捷方式、`.exe`、文件夹或文件拖到内环磁贴上。
- **归窗** —— 把一个窗口拖到轮盘上，归到它所属的应用下。
- **预览** —— 悬停应用展开它的窗口，悬停胶囊显示实时画面。
- **滚动** —— 应用超过一屏时，滚轮连续滚动内环，一格一个。
- **设置** —— 托盘图标右键 → 设置。

## 配置

`%APPDATA%\PiPie\settings.ini`，首次运行自动创建，日常用设置页改即可。日志在 `%LOCALAPPDATA%\PiPie\logs\`。从旧版 Pigo 升级时，旧配置会在首次启动时自动复制过来。

## 许可证

[MIT](LICENSE)。可自由使用、修改、分发，禁止商用。
未来若推出付费服务，客户端仍保持开源 —— 边界见 [LICENSING.md](LICENSING.md)。
