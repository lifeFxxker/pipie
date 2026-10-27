#![allow(non_snake_case)]
#![allow(non_camel_case_types)]
//! release 构建切到 **Windows GUI 子系统** —— 不切的话双击会弹一个黑色控制台
//! 窗口挂在任务栏上，托盘程序看着像坏了。
//!
//! `cfg_attr(not(debug_assertions))` = **只在 release 关控制台**，
//! `cargo run`（debug）仍保留 stdout —— 启动横幅 / `[dump]` / `[rename]`
//! 等诊断信息都靠 stdout 看。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! PiPie —— 原生 Win32 圆形双环菜单启动器（自带软件渲染，零运行时依赖）
//!
//! 视觉（照搬 TheRing，参考 design/demos/demo-thering.html 与 design/thering_cover.jpg）：
//!   一整块圆形玻璃盘（盘面跟随壁纸染色）+ 应用图标立在盘面一圈
//!   中心子圆显示悬停应用名 + 项目数；悬停图标带圆角方框选中环
//!   悬停应用的内容以胶囊 chip 从盘沿外侧切向扇出；图标带角标
//! 结构：内环 = 常用应用（每个槽位 = 应用图标）；外环 = 该应用的内容项
//! 生命周期：托盘菜单 + 全局热键 Ctrl+Alt+R 可开启/停止监听
//!   停止后卸载鼠标低级钩子（UnhookWindowsHookEx），零监听开销
//!
//! 操作：按住中键（或侧键）呼出 → 悬停应用展开内容 → 松开执行；Esc / 点其他键取消

use std::ffi::c_void;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicIsize, AtomicU8, AtomicU32, Ordering};
use std::time::Instant;

use windows::core::PCWSTR;
use windows::core::PWSTR;
use windows::core as windows_core;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::{GetModuleFileNameW, GetModuleHandleW};
use windows::Win32::System::Threading::{CreateMutexW, GetCurrentProcess, GetCurrentThreadId, SetProcessWorkingSetSize};
use windows::Win32::System::SystemInformation::GetTickCount;
use windows::Win32::UI::Input::KeyboardAndMouse::{RegisterHotKey, UnregisterHotKey, MOD_ALT, MOD_CONTROL};
use windows::Win32::UI::Shell::{AssocQueryStringW, DragAcceptFiles, DragFinish, DragQueryFileW, DragQueryPoint, HDROP, ShellExecuteW, Shell_NotifyIconW, NOTIFYICONDATAW, NIF_MESSAGE, NIF_ICON, NIF_TIP, NIM_ADD, NIM_DELETE, SHGetFileInfoW, SHFILEINFOW, SHGFI_ICON, SHGFI_LARGEICON, SHGFI_USEFILEATTRIBUTES, ASSOCF_NONE, ASSOCSTR_EXECUTABLE};
use windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_NORMAL;
use windows::Win32::System::Com::{DVASPECT_CONTENT, FORMATETC, TYMED_HGLOBAL};
use windows::Win32::System::DataExchange::{CloseClipboard, GetClipboardData, OpenClipboard};
use windows::Win32::System::Memory::{GlobalLock, GlobalUnlock};
use windows::Win32::System::DataExchange::RegisterClipboardFormatW;
use windows::Win32::System::Ole::{DROPEFFECT, IDropTarget, IDropTarget_Impl, RegisterDragDrop, ReleaseStgMedium};
use windows::Win32::System::Com::IDataObject;
use windows::Win32::System::SystemServices::MODIFIERKEYS_FLAGS;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::Win32::UI::WindowsAndMessaging::PostThreadMessageW;

// ───────────────────────── 几何（对齐 demo-thering.html） ─────────────────────────

/// ══ 全局界面缩放 ══
/// 下面所有尺寸都写成「基准值 × K」。**要整体放大/缩小，只改这一个数。**
/// 基准值是 K = 1.0 时的手感尺寸（盘面半径 178、图标 56 …）。
/// 改完 `cargo build --release`，产物即 `target/release/PiPie.exe`（单文件，拷走就能用）。
const K: f32 = 0.66;
/// 图标单独缩放：比布局缩得少 → 盘子变小了但图标相对更大、更清楚。
/// ⚠️ 这个值算出来的是**底板边长**（不是图标本身，图标还要再除 `TILE_PAD_K`）。
/// 2026-10-07 用户反馈「有点太小了，边框可以大一些，图标也大一点点」
/// → 0.554（底板 31）→ **0.640（底板 35.8，图标 29.9）**。
/// 上限提醒：盘径 `DISC_R_MAX = 145.2`。再往上（KI 0.68 → 盘径 141）会因为顶到上限
/// 被布局**反过来压缩**，12 个应用时盘径反而变小 —— 别再无脑加。
const KI: f32 = 0.640;
const KC: f32 = 1.34;   // chip 内容缩放（胶囊高度 / 图标方块 / 字号）

const CANVAS: i32 = (720.0 * K) as i32; // 渲染画布边长：最远点 = r_out + 段内内容的半长
const CC: f32 = CANVAS as f32 / 2.0;    // 画布中心

// 盘面半径**上限**。⚠️ 220K 时 12 个应用正好顶到，底板被从 35.8 压到 30.8、图标只剩
// 25.6px——肉眼就是「图标又小又糊」（用户 2026-10-08 反馈）。抬到 240K 之后 12 个应用
// 还能保住整块底板：图标 25.6 → 29.1px（+13%），盘径 285 → 316px（窗口 475 装得下）。
// 相邻底板间距仍然 = `GAP_K × 图标`，并没有变挤。
const DISC_R_MAX: f32 = 240.0 * K;
const R_ICON_MIN: f32 = 132.0 * K; // 图标环半径下限（条目少时别缩成一个点）
const ICON_DRAW: f32 = 56.0 * KI;  // 图标绘制尺寸上限（真实图标直接立在盘面上）
/// 相邻图标之间要留的空隙 = 图标边长 × GAP_K。
/// 原实现的目标只是「刚好不相撞」→ 12 个应用时间隙/图标 ≈ 0.28，图标挤成一片看着乱。
/// 对标参考（TheRing）实测间隙/图标 = 0.91；12 个应用在同盘径下取 0.45 已是现实上限。
const GAP_K: f32 = 0.45;
/// 圆角方块在**切向**的最坏投影系数（方块在 45° 处 ≈1.414，圆角半径越大越小）。
/// 按 TILE_RAD_K 的圆角实测 ≈1.22。
const TANG: f32 = 1.22;
/// 环带在图标内外各留的宽度：DISC_R = 图标环 + 图标半宽 + BAND_PAD。
const BAND_PAD: f32 = 28.0; // 图标外沿 → 盘边留白（用户反馈贴边，18 → 24 → 28）
/// 环带底色浓度。**这是白底图标（记事本那页白纸）能在浅色盘面上显形的前提** ——
/// 纯白盘上它整片消失，这也是「很多图标糊/看不见」的一半原因。
const BAND_DARK: f32 = 0.28;
/// 圆角方块：圆角半径 / 边长。参考图那种「整齐」的核心就是这个统一轮廓。
const TILE_RAD_K: f32 = 0.24;
/// **底板边长 / 图标边长** —— 底板的内边距系数。
///
/// 2026-10-07 用户澄清：「不是让你缩小图标吗，我不是让你缩小图标框，
/// **只缩小里面的图标**，这样框与图标就能有点间距了，好看」。
/// 之前那轮改错了：底板和图标**同比缩小**，间距恒为 0，等于白改。
/// 现在底板边长**锁死**（几何 / 盘径 / 图标间隙全都不变），
/// 只把图标按这个系数缩进去 → 底板露出一圈均匀白边。
/// 那圈白边本身就是「统一圆角方块」的一部分 —— 图标再乱，收敛后的轮廓一样。
/// `1.0` = 旧行为（无留白）。当前 `1.20` → 31px 底板里放 25.8px 图标，每边约 2.6px。
const TILE_PAD_K: f32 = 1.20;
/// `normalize_to` 第③步的裁切圆角要跟着缩：纹理是按**图标**尺寸裁的，
/// 不改的话图标放大时圆角会与底板的圆角对不上（露白缝）。
const TILE_TEX_PAD_K: f32 = TILE_PAD_K;
const ICON_MIN: f32 = 30.0;     // 图标缩放下限
/// 图标纹理尺寸：**2 倍超采样**。256→82 用 box 下采样一次到位，
/// 绘制 82→33（k≈2.5）走面积平均 —— 边缘接近像素完美锐利。
const ICON_PX: usize = (ICON_DRAW * 2.05) as usize;
/// mip 链**保留上限**（内存优化，2026-10-12）。
///
/// `pick()` 取「≥ size 的最小一级」，而图标**实际最大只画到 ~35px**：
///   - 内环 `icon_side = icon_draw * scale / TILE_PAD_K`，`scale = (0.30+0.70·ap)·(1+0.10·hot)`
///     上限 ≈ 1.18 → `35.84 × 1.18 / 1.20 ≈ 35.2`
///   - 幽灵 `icon_draw × 0.86` ≈ 30.8；外环 chip `CHIP_ICON × 1.06` ≈ 23.4
/// 所以 > MIP_CAP 的级**永远不会被命中**（73px 起那条链 32 级共 187KB，其中 62% 是死重）。
/// 取 44 给足余量（44 > 35.2，且留着 43/41/39/37 几级兜住动画回弹的超调）。
/// ⚠️ 若以后把图标画得更大（比如设置页用 48px），**必须同步抬高这个值**，
/// 否则 `pick` 会退到最大级硬放大 → 发虚。
const MIP_CAP: usize = 44;
const TILE_R: f32 = 17.0 * K;   // 无图标时的字形方块圆角
const HUB_R: f32 = 66.0 * K;    // 中心头像球半径
/// 外环段（梯形环）的厚度 = 旧胶囊的高度 —— 「甲」方案保持原尺寸不变。
const CHIP_H: f32 = 34.0 * K * KC;
const CHIP_PAD_L: f32 = 6.0 * K * KC;   // 左内边距
// 左端图标边长。⚠️ 不是随手定的：外环带动画的图标就画这么大，19.5px 时肉眼
// 已经偏小发虚（用户 2026-10-08 反馈）。环带厚 `RING_T = 30`，22.1 两侧各留 ~4px。
const CHIP_ICON: f32 = 25.0 * K * KC;
const CHIP_GAP: f32 = 8.0 * K * KC;     // 图标与文字间距
const CHIP_PAD_R: f32 = 14.0 * K * KC;  // 右内边距
const CHIP_MIN_GAP: f32 = 12.0 * K * KC; // 相邻 chip 最小切向间隙(px)
/// 贴弧胶囊的倾角 = **圆环在该处的切线角**（`t` 归一到 (-90°,90°]，见 `chip_layout`）。
/// 胶囊的长轴必须严格平行于圆环 —— 3 点 / 9 点方向就该是竖着的。
/// ⚠️ 之前这里是 `t * 0.82` 再夹到 ±58°，结果 3/9 点只倾斜 58°，看着是「斜着的」，
/// 和相邻的胶囊对不齐。用户的原话：「应该是平行于圆环，为什么斜着？」
/// 外环布局：**一律贴身扇面**，不再有「超过阈值就整圈均布」的退路
/// （用户 2026-10-17 定稿：「外环不应该是均匀分布的，而是以默认间距在对应内环附近分布」）。
/// 这个常量只表示**扇面允许的最大跨度** —— 留 24° 缺口，首尾别贴脸。
const CHIP_FAN_MAX: f32 = 336.0;
/// 压缝时的最小缝（px）：比 `CHIP_MIN_GAP` 小，但别真贴到一起（圆角会咬）。
const CHIP_GAP_MIN: f32 = 2.0 * K * KC;

/// 一屏槽位数 = 盘面几何上限（`DISC_R_MAX` 封顶后再加就只能把图标压糊）。
const RING_SLOTS: usize = 12;
/// 内环**数据**上限。超过一屏（`RING_SLOTS`）的应用不裁掉，靠滚轮连续滚动看到。
const MAX_APPS: usize = 24;
const MAX_ITEMS: usize = 12; // 外环可围满一整圈
/// 滚动每帧向目标追赶的比例（与 `app_hot` 同一档手感：0.28 ≈ 130ms 收敛）。
const RING_SCROLL_K: f32 = 0.28;
/// 淡出中的磁贴往盘心**内沉**的距离（画布 px）。
/// 滚出去的图标是「退回盘里」，不是在原地变透明 —— 光有 alpha 交叉会看成两层重影。
const SCROLL_SINK: f32 = 6.0;

const WM_APP_SHOW: u32 = WM_APP + 1;
const WM_APP_HOVER: u32 = WM_APP + 2;
const WM_APP_COMMIT: u32 = WM_APP + 3;
const WM_APP_CANCEL: u32 = WM_APP + 4;
const WM_APP_TRAY: u32 = WM_APP + 5;
const WM_APP_DUMP: u32 = WM_APP + 6;
const WM_APP_DEBUGPOS: u32 = WM_APP + 7; // 仅自动化测试：直接设置悬停坐标
const WM_APP_DRAGEVENT: u32 = WM_APP + 11;
const WM_APP_DROPTEST: u32 = WM_APP + 12; // 诊断：给定屏幕坐标，回报 drop_force_app 的解析结果（拖放用不了自动化，只能这样验） // 拖放事件（w=0 进入/1 移动/2 离开/3 落下）
const WM_APP_KEY: u32 = WM_APP + 9;     // 键盘动作（w = 动作码）
const WM_APP_CLICK: u32 = WM_APP + 10;  // 常驻模式下左键点击
const WM_APP_DRAGSTEP: u32 = WM_APP + 13; // 按住应用图标拖动中
const WM_APP_DRAGEND: u32 = WM_APP + 14;  // 拖动松手（盘外 = 请求移除）
const WM_APP_DRAGSIM: u32 = WM_APP + 15;  // 仅测试：驱动拖出移除状态机
const WM_APP_CHAR: u32 = WM_APP + 16;    // 改名模式：一个已转换好的字符（w = codepoint）
const WM_APP_EDITSIM: u32 = WM_APP + 17; // 仅测试：驱动改名状态机
const WM_APP_CHIPPOS: u32 = WM_APP + 22; // 仅测试：回报 0 号胶囊的**屏幕**坐标
                                        // 返回值 = (y << 16) | x
                                        //（单击路径要靠真光标才能触发 update_hover，
                                        //  测试必须知道胶囊在哪才能把光标挪过去）
                                        // w = 阶段（0 双击进入 / 1 追加字符 / 2 退格
                                        //        3 提交 / 4 取消 / 5 提交空名）
const WM_APP_DISMISS_FG: u32 = WM_APP + 18;
// 长按右键呼出菜单时，被「放行」的那个 UP 会让前台程序弹出它自己的右键菜单 ——
// 这里补一次 Esc 关掉它（**窗口消息**，不是输入注入，见处理器处注释）。
const WM_APP_RHOLDSIM: u32 = WM_APP + 19;
/// 只在内部用：叫醒钩子线程去同步键盘钩子的「线程消息」（内容无所谓）。
const WM_APP_HOOK_SYNC: u32 = WM_APP + 21;
/// 后台「这个网址是不是已经开着」查完了（结果在 `LOAD_HIT`，动作在处理器里做）。
/// ⚠️ 这个查询要联网+UI Automation，**绝不能同步跑在 UI 线程上**（会冻住整圈 2~3 秒）。
const WM_APP_LOADED: u32 = WM_APP + 23;
/// 仅测试：跑一次与钩子**同一条**的例外判定并把结果打出来
/// （呼出键走全局钩子，合成事件驱动不了，只能这样验判定这一环）。
const WM_APP_EXEMSIM: u32 = WM_APP + 24;
const WM_APP_TRIGCAP: u32 = WM_APP + 20; // 自定义触发：钩子捕获到输入后通知 UI 线程落盘
const WM_APP_TRIGSIM: u32 = WM_APP + 25; // 仅测试：模拟捕获到键盘组合 Ctrl+W（自定义触发现为键盘-only）
const WM_APP_TRIGBAD: u32 = WM_APP + 26; // 自定义触发：录制态按了无效键（左/右键），通知 UI 显式反馈
const WM_APP_DROPSIM: u32 = WM_APP + 27; // 仅测试：塞入 DROP_PT/DROP_PAYLOAD，供 WM_APP_DRAGEVENT 驱动（w=(y<<16)|x，l=载荷种类）
const WM_APP_WINARM: u32 = WM_APP + 28;  // 仅测试：用给定 hwnd 武装窗口捕获（l = hwnd isize）
const WM_APP_WINDROP: u32 = WM_APP + 29; // 仅测试：模拟「松手」，按当前光标把捕获落盘
/// 后台「启动 / 聚焦窗口」做完了 → UI 线程收尾（清 loading、收起菜单）。
/// 与 `WM_APP_LOADED`（网址查询那条路）分开：那条还要切标签，这条只管收尾。
const WM_APP_LAUNCHED: u32 = WM_APP + 30;

/// 在线升级：后台查版本完成（结果放在 `UPDATE_RESULT`，UI 线程取走后刷新关于页）。
const WM_APP_UPDATE_DONE: u32 = WM_APP + 31;
/// 后台检查结果：(状态 2=已最新 / 3=有新版本 / 4=失败, latest 版本号, 提示/原因)
static UPDATE_RESULT: std::sync::Mutex<Option<(u8, String, String)>> =
    std::sync::Mutex::new(None);
/// 仅测试：回报**内环第 w 个图标**中心的屏幕坐标（高 16 位 y、低 16 位 x）。
/// 外环有 `WM_APP_CHIPPOS`，内环一直没有 —— 自动化点图标只能靠估半径，
/// 估错就点到空白处（点了没反应，还以为是功能坏了）。
const WM_APP_APPPOS: u32 = WM_APP + 32;
/// 仅测试：直接把指定 hwnd 的缩略图弹在左上角，用来确认
/// 「最小化窗口的 DWM 缩略图到底有没有画面」——不依赖任何 UI 定位。
const WM_APP_THUMBSIM: u32 = WM_APP + 34;
/// 滚轮一格（w = ±1）：内环**连续滚动**（超过一屏 `RING_SLOTS` 个应用时才生效）。
/// 只能从低级钩子来 —— 菜单不抢焦点，`WM_MOUSEWHEEL` 到不了我们的窗口过程。
const WM_APP_WHEEL: u32 = WM_APP + 35;
/// 仅测试：把滚动偏移**直接**设成 w（不动画），用于「滚到第 N 页后命中/绘制是否还对」的回归。
const WM_APP_SCROLLSIM: u32 = WM_APP + 36;
/// 仅测试：按 (w, l) 走一遍**真实**的 hover 命中路径，返回命中的应用下标 +1（0 = 未命中）。
/// 配合 `WM_APP_SCROLLSIM` + `WM_APP_APPPOS` 才能回归「滚过之后再点，点的是不是画出来的那个」。
const WM_APP_HITTEST: u32 = WM_APP + 37;
/// 后台 UIA 探测（文件夹路径 / 网址）跑完了 → UI 线程把结果回填进刚落盘的窗口条目。
/// 只在**松手落盘那一次**起探测，所以这条消息一次拖动最多来一回。
const WM_APP_WINPROBE: u32 = WM_APP + 38;
/// 后台抓 `<title>` 跑完了 → **设置窗口**把新名字回填进那条网址条目（见 `title_fetch_start`）。
const WM_APP_TITLE: u32 = WM_APP + 39;

/// `loading.1` 用这个哨兵值表示「loading 挂在内环**应用本体**上」（图标位置），
/// 而不是某个外环胶囊 —— 与 `commit_chip == usize::MAX`（内环提交）同一套约定。
const LOADING_HUB: usize = usize::MAX;
/// `loading_kind`：网址查询（收尾走 `WM_APP_LOADED`，要切标签）
const LOAD_KIND_URL: u8 = 0;
/// 启动程序 / 打开文件夹（收尾只需收起菜单）
const LOAD_KIND_LAUNCH: u8 = 1;
/// 窗口条目：聚焦或重开
const LOAD_KIND_WINDOW: u8 = 2;
// 仅测试：直接驱动右键状态机（`w`：0=喂 RBUTTONDOWN / 1=喂 RBUTTONUP / 2=查询）。
// 为什么不注入真实鼠标事件来测：测试进程注入输入 + 我们装着全局钩子，
// 两边会互相卡死（实测整个测试挂死）。直接喂事件码安全且确定。
// 返回值打包：bit0=本次被消费 / bit1=R_DOWN_AT 非 0 / bit2=按下时菜单开着
//            / bits8..=REINJECT_POSTED 计数 / bits16..=R_DOWN_SEEN 计数
                                        // l = 附加码点（阶段 1 用）

// ── 拖出移除：鼠标钩子只记录「按在哪 / 有没有动」这些原始事实，
//    至于「抓到的是哪个应用」「在不在盘外」全部由 UI 线程判定（它才有 State）。
//    动作用消息回 UI 线程，和渲染同线程，不碰跨线程状态。
static LB_DOWN: AtomicBool = AtomicBool::new(false);   // 左键当前按着
static LB_MOVED: AtomicBool = AtomicBool::new(false);  // 已越过拖动阈值
static DRAG_POSTED: AtomicBool = AtomicBool::new(false); // 队列里只留一条拖动通知（防消息风暴）
static LB_DOWN_X: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);
static LB_DOWN_Y: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);

/// 拖动阈值（px）：按下后移动超过它才算「拖」而不是「点」
const DRAG_THRESH: f32 = 8.0;
/// 确认移除的倒计时（秒）= 撤销窗口。到点落盘；期间点「取消」才放回。
/// ⚠️ 收起/关掉轮盘**不再放回**，而是立即落盘确认删除（用户 2026-10-12 要求）。
const REMOVE_GRACE: f64 = 5.0;
/// 拖出**多远**才算「请求移除」：胶囊环外沿再往外这么多 px。
/// ⚠️ 原来判定用的是**盘面边缘**（`in_disc`，半径 ≈133），而图标环外沿 ≈105 ——
/// 只差 28px，正常换位稍微往外一甩就被当成「松手就移除」，头像立刻变紧张、
/// 气泡也喊「松手就移除」（用户 2026-10-14 报：换位和移除没区分开）。
/// 现在**界内摆动一律当换位**，要移除得明显往外拖出去。
const REMOVE_PULL: f32 = 20.0;
/// 发呆判定（秒）：菜单开着但这么久没有任何输入，头像进入发呆
const IDLE_MOOD_AFTER: f64 = 6.0;
/// 菜单开着、且**没有任何输入**超过这么久就自动收起（非常驻模式）。
/// 安全阀：菜单一旦悬着，所有鼠标事件都得先过一遍钩子判断。
const IDLE_CLOSE_AFTER: f64 = 12.0;
// 菜单收起后过多久裁剪一次工作集（秒）。
//
// 为什么需要：任务管理器显示的是**工作集**，而我们空闲时的驻留里大部分是
// **共享页** —— 映射进来的 `msyh.ttc`（18.8MB 的字体文件，只用到了几十个字形）
// 和一堆 DLL 的代码页。实测：0 应用时提交仅 5.8MB，工作集却是 19.2MB。
// `SetProcessWorkingSetSize(-1,-1)` 把这些页还给系统，下次要画时再换回来
// （几十毫秒，用户无感）—— **零画质代价**，但任务管理器立刻从 30M+ 掉到个位数。
//
// ⚠️ 别设太短：裁剪后第一次打开环会有一次轻微换页抖动（~50ms）。
const TRIM_AFTER: f64 = 3.0;
/// 松手后「被拖的那个」从光标滑进槽位的时长（秒）。
/// ⚠️ 只给它做动画：其余图标在拖动期间已按预览排序让位就位了，松手后**不能再动**。
const LAND_SECS: f64 = 0.22;
/// 启动时「亮个相」：头像说这句、环亮这么久（秒）后自动收起。
/// ⚠️ 每次启动都播（用户 2026-10-14 要求）—— 这是「我在这儿」的存在感提示，
///    不是一次性引导；一次性引导在设置页的 coach marks。
const REVEAL_MSG: &str = "需要就快捷键叫我哦";
const REVEAL_SECS: f64 = 2.0;

/// onboarding 教学气泡的停留时长。
/// ⚠️ 必须是**定时收**而不是"等下个气泡顶掉"：教学气泡会压住右上角那一块图标，
/// 挂太久就变成遮挡（2026-10-14 出图后发现的，用户拍板 3.5 秒）。
const ONB_SECS: f64 = 3.5;
/// 「发现新版本」气泡的停留时长（和启动「亮个相」同一档：只是点个头，别赖着）。
const UPD_BUB_SECS: f64 = 2.0;

/// 越过移除线后，虚线环的旋转速度（度/秒）。90°/s ≈ 4 秒一圈：一秒正好移 6 格（15°/格），
/// 慢到不刺眼、快到余光能跟住"它在转"。（再快就糊成闪烁噪声了）
const RM_SPIN_DEG_S: f32 = 90.0;
/// 每帧的旋转步长（帧定时器固定 16ms，见 `TIMER_FRAME` 的 SetTimer 调用）。
const RM_SPIN_STEP: f32 = RM_SPIN_DEG_S * 0.016;
/// 移除区虚线环的段数。⚠️ **别加到 48 以上** —— 间距太小会让旋转读不出来（见上面注释）。
const RM_ZONE_DASHES: usize = 24;
/// 拖动时是否画出移除区虚线圈。用户 2026-10-15 要求**先隐藏**（判定照旧，只是不画）。
const RM_ZONE_SHOW: bool = false;

// 键盘动作码
const KA_CLOSE: usize = 1;  // Esc
const KA_CONFIRM: usize = 2; // Enter
const KA_PREV: usize = 3;   // ← ↑ Shift+Tab
const KA_NEXT: usize = 4;   // → ↓ Tab
const KA_PASTE: usize = 5;  // V：把剪贴板内容加入环
const KA_EDIT_COMMIT: usize = 6; // 改名时按 Enter
const KA_EDIT_CANCEL: usize = 7; // 改名时按 Esc
const KA_EDIT_BACK: usize = 8;   // 改名时按 Backspace
const KA_EDIT_DEL: usize = 9;    // 改名时按 Delete

// ───────────────────── 拖放入环（OLE DropTarget） ─────────────────────
// IDropTarget 回调都到达注册线程（我们的 UI 线程），但 State 不能在
// COM 实现里直接碰 —— 回调只写这些静态量并 post 消息，全部状态变更
// 在 host_proc 的 WM_APP_DRAGEVENT 里做（与渲染同线程，无竞争）。
static DRAG_ACTIVE: AtomicBool = AtomicBool::new(false); // OLE 拖放进行中（环上）
static DRAG_SEEN: AtomicBool = AtomicBool::new(false);   // 本次呼出期间见过拖放
static DROP_PT: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);
static DROP_PAYLOAD: std::sync::Mutex<Vec<DropItem>> = std::sync::Mutex::new(Vec::new());

// ── 「网址是不是已经开着」的**后台查询**（见 switch_bg）
static LOAD_HIT: AtomicBool = AtomicBool::new(false); // 后台查询结果：命中已开标签

static WC_ICON_CACHE: std::sync::Mutex<Vec<(String, Option<IconTex>)>> =
    std::sync::Mutex::new(Vec::new());

// ── 窗口缩略图（悬停外环胶囊时弹的实时小窗）──
/// 缩略图宿主窗口句柄（懒创建，全生命周期复用）
static THUMB_HWND: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);
/// 当前已注册的源窗口 —— 变了才重新 `DwmRegisterThumbnail`
static THUMB_SRC: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);
/// 悬停键（`ai*1000+ci`）、首次悬停时刻、是否已显示
static THUMB_KEY: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(usize::MAX);
static THUMB_SINCE: std::sync::Mutex<f64> = std::sync::Mutex::new(0.0);
/// 上次摆的位置（打包成 i64）。每帧都 `SetWindowPos` 是浪费，还会刷 WM_WINDOWPOSCHANGED。
static THUMB_POS: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(i64::MIN);
/// 卡片上要显示的说明（缩略图不可用时）。空 = 正常显示缩略图。
static THUMB_NOTE: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// 预览卡片尺寸（含 8px 内边距）
const THUMB_W: i32 = 256;
const THUMB_H: i32 = 168;
/// 悬停多久才弹（防止掠过时闪一下）
const THUMB_DELAY: f64 = 0.12;

/// url → 网页标题 的小缓存：第二次点同一个网址就不用再联网（省掉最贵的 0.5~1.2 秒）。
/// ⚠️ 只信缓存会漏判（标题会变）→ 缓存没命中才去联网重抓一次。
static SWITCH_CACHE: std::sync::Mutex<Vec<(String, String)>> = std::sync::Mutex::new(Vec::new());

/// 图标解码用的 **256×256 暂存区**（`shell_icon_hires` 里 `GetDIBits` 的原始位图）。
///
/// 内存优化（2026-10-12）：原来每个图标都 `vec![0u8; 256*256*4]`（256KB）申请一块、
/// 用完即弃。问题是 **Windows 堆不会主动把释放的内存还给 OS** —— 块留在已提交堆里，
/// 实测 20 个图标就多占约 5MB 常驻。这里全程复用**同一块**，只申请一次。
/// ⚠️ 调用方必须在一处用完即放（本函数内同步用完，不跨调用持有）。
static ICON_SCRATCH: std::sync::Mutex<Vec<u8>> = std::sync::Mutex::new(Vec::new());

/// 当前前台程序**命中例外名单**（或处于全屏）→ 呼出键一律放行。
/// ⚠️ 由 **UI 线程**定时刷新（`update_exempt`）；钩子回调只做一次原子读 ——
///    绝不可以在回调里 OpenProcess/查进程名（那会拖慢整条输入链，正是我们以前
///    「鼠标卡 5~6 秒」的同类错误）。
static EXEMPT: AtomicBool = AtomicBool::new(false);

fn cache_get(url: &str) -> Option<String> {
    SWITCH_CACHE
        .lock()
        .ok()?
        .iter()
        .find(|(u, _)| u == url)
        .map(|(_, t)| t.clone())
}

fn cache_put(url: &str, title: &str) {
    if let Ok(mut c) = SWITCH_CACHE.lock() {
        if let Some(e) = c.iter_mut().find(|(u, _)| u == url) {
            e.1 = title.to_string();
        } else {
            if c.len() >= 64 {
                c.remove(0);
            }
            c.push((url.to_string(), title.to_string()));
        }
    }
}
static DROP_MODE_ARMED: AtomicBool = AtomicBool::new(false); // 菜单处于拖放等待态（钩子线程读）
/// 上次收到 OLE 拖放事件的时刻（`GetTickCount()` ms）。DragEnter/DragOver/DragLeave 都刷新它。
/// 用来给 `DROP_MODE_ARMED` 做**超时兜底** —— 见 `on_timer` 里的说明。
static DROP_ARM_AT: AtomicU32 = AtomicU32::new(0);
/// `DROP_MODE_ARMED` 静默超过这么久（无任何 OLE 事件）就判定拖放会话已异常结束，自动解除。
const DROP_ARM_TTL_MS: u32 = 3000;

/// 拖放链路诊断日志（LocalAppData\PiPie\logs\draglog.txt）
fn drag_log(msg: &str) {
    use std::io::Write;
    let dir = log_dir();
    let _ = std::fs::create_dir_all(&dir);
    let path = format!("{}\\draglog.txt", dir);
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "[{:?}] {}", std::time::SystemTime::now(), msg);
    }
}

// ───────────────────── 输入卡顿诊断 ─────────────────────
// 背景：用户报告「闲置二十分钟回来鼠标卡 ~30 秒自愈」。要区分三种根因：
//   A. 我们的钩子回调本身慢（页入/逻辑）      → HOOK_SLOW
//   B. 我们整个进程被冻结（待机/换页）        → WATCHDOG_GAP
//   C. 输入在别处被卡（系统唤醒/别的钩子）    → 看门狗心跳连续 + A/B 都没有
// 铁律：钩子回调里**绝不碰文件和锁**（那会反过来制造卡顿）——
// 慢事件只打包进无锁环形缓冲，由看门狗线程每秒落盘 LocalAppData\PiPie\logs。

const LOG_RING_SLOTS: usize = 128; // 2 的幂，取模用掩码
static LOG_HEAD: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static LOG_RING: [std::sync::atomic::AtomicU64; LOG_RING_SLOTS] = {
    const Z: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    [Z; LOG_RING_SLOTS]
};
/// 上次钩子回调进入的时刻（GetTickCount 口径，算事件间隔用）
static LAST_HOOK_TICK: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

const DK_HOOK_SLOW: u8 = 1; // aux = 鼠标钩子回调耗时 ms（>30ms 才记）
const DK_HOOK_GAP: u8 = 2;  // aux = 距上次鼠标事件的间隔 ms（>30s 才记）
const DK_UI_SLOW: u8 = 3;   // aux = 一帧渲染耗时 ms（>100ms 才记）
const DK_MENU_OPEN: u8 = 4; // 菜单呼出
const DK_MENU_CLOSE: u8 = 5; // 菜单收起
const DK_HOOK_ON: u8 = 6;   // 钩子安装成功
const DK_KEY_SLOW: u8 = 7;  // aux = 键盘钩子回调耗时 ms（>30ms 才记）
const DK_HOOK_LOST: u8 = 8; // 钩子线程意外退场（内核里可能残留死钩子 → 已自愈重启）
const DK_SNAPSHOT: u8 = 9; // 周期性状态快照（aux = 位掩码，见 STATE_* ）
const DK_MOUSE_IDLE: u8 = 10; // 鼠标静默间隔（**5s 阈值**，比 HOOK_GAP 细 6 倍）
const DK_POST_DROP: u8 = 11; // post 失败（窗口已销毁 / 队列满）
const DK_HB: u8 = 12;        // 看门狗心跳（aux = 距上次心跳 ms；证明线程还活着）

// ── 状态快照的位掩码（aux 只有 24 位，够放下 20 个开关）──
const S_MENU_OPEN: u32 = 1 << 0;
const S_LOCKED: u32 = 1 << 1;
const S_DRAG_ACTIVE: u32 = 1 << 2;
const S_DROP_ARMED: u32 = 1 << 3;
const S_LB_DOWN: u32 = 1 << 4;
const S_LB_MOVED: u32 = 1 << 5;
const S_HOVER_POSTED: u32 = 1 << 6;
const S_DRAG_POSTED: u32 = 1 << 7;
const S_EDIT_ARMED: u32 = 1 << 8;
const S_HIDING: u32 = 1 << 9;
const S_ENTERING: u32 = 1 << 10;
const S_CTX_OPEN: u32 = 1 << 11;
const S_PENDING_RM: u32 = 1 << 12;
const S_HAS_FOCUS_APP: u32 = 1 << 13;
const S_HAS_HOVER_CHIP: u32 = 1 << 14;
const S_HAS_HIT_APP: u32 = 1 << 15;
const S_EXEMPT: u32 = 1 << 16;
const S_TRIG_CAP: u32 = 1 << 17;

/// 把全部跨线程关键状态打包成 24 位掩码。
/// **为什么必须有它**：出问题时「鼠标点不动」可能源自任何一个原子量卡住
/// （MENU_OPEN 忘了清 / LOCKED 残留 / LB_DOWN 没复位…），只看日志事件推不出来。
/// 有了这份快照，出问题那一刻直接读位就知道是哪一位卡住了。
fn state_bits(hiding: bool, entering: bool, pending_rm: bool,
              focus: bool, hover_chip: bool, hit_app: bool) -> u32 {
    let mut b = 0u32;
    if MENU_OPEN.load(Ordering::Relaxed) { b |= S_MENU_OPEN }
    if LOCKED.load(Ordering::Relaxed) { b |= S_LOCKED }
    if DRAG_ACTIVE.load(Ordering::Relaxed) { b |= S_DRAG_ACTIVE }
    if DROP_MODE_ARMED.load(Ordering::Relaxed) { b |= S_DROP_ARMED }
    if LB_DOWN.load(Ordering::Relaxed) { b |= S_LB_DOWN }
    if LB_MOVED.load(Ordering::Relaxed) { b |= S_LB_MOVED }
    if HOVER_POSTED.load(Ordering::Relaxed) { b |= S_HOVER_POSTED }
    if DRAG_POSTED.load(Ordering::Relaxed) { b |= S_DRAG_POSTED }
    if EDIT_ARMED.load(Ordering::Relaxed) { b |= S_EDIT_ARMED }
    if hiding { b |= S_HIDING }
    if entering { b |= S_ENTERING }
    if CTX_OPEN.load(Ordering::Relaxed) { b |= S_CTX_OPEN }
    if pending_rm { b |= S_PENDING_RM }
    if focus { b |= S_HAS_FOCUS_APP }
    if hover_chip { b |= S_HAS_HOVER_CHIP }
    if hit_app { b |= S_HAS_HIT_APP }
    // 这两个不在 UI 私有状态里，是跨线程原子量，本线程直接读：
    // 「有时按了没反应」的两个头号嫌疑人，之前日志里**一个位都没有**。
    if EXEMPT.load(Ordering::Relaxed) { b |= S_EXEMPT }
    if CAPTURE_TRIGGER.load(Ordering::Relaxed) { b |= S_TRIG_CAP }
    b
}

/// 打包 [tick:32][kind:8][aux:24]，写进环形缓冲（等待看门狗落盘）
fn diag_push(kind: u8, aux: u32) {
    let tick = unsafe { GetTickCount() };
    let v = ((tick as u64) << 32)
        | ((kind as u64 & 0xff) << 24)
        | (aux as u64 & 0x00ff_ffff);
    let i = LOG_HEAD.fetch_add(1, Ordering::Relaxed) & (LOG_RING_SLOTS as u64 - 1);
    LOG_RING[i as usize].store(v, Ordering::Release);
}

fn diag_kind_name(kind: u8) -> &'static str {
    match kind {
        DK_HOOK_SLOW => "HOOK_SLOW",
        DK_HOOK_GAP => "HOOK_GAP",
        DK_UI_SLOW => "UI_SLOW",
        DK_MENU_OPEN => "MENU_OPEN",
        DK_MENU_CLOSE => "MENU_CLOSE",
        DK_HOOK_ON => "HOOK_ON",
        DK_KEY_SLOW => "KEY_SLOW",
        DK_HOOK_LOST => "HOOK_LOST",
        DK_SNAPSHOT => "SNAPSHOT",
        DK_MOUSE_IDLE => "MOUSE_IDLE",
        DK_POST_DROP => "POST_DROP",
        DK_HB => "HB",
        _ => "?",
    }
}

/// 本地时间戳（GetLocalTime，不引入 chrono）
fn diag_now() -> String {
    #[repr(C)]
    struct ST {
        year: u16, month: u16, dow: u16, day: u16,
        hour: u16, minute: u16, second: u16, ms: u16,
    }
    extern "system" {
        #[link_name = "GetLocalTime"]
        fn get_local_time(st: *mut ST);
    }
    let mut st = ST { year: 0, month: 0, dow: 0, day: 0, hour: 0, minute: 0, second: 0, ms: 0 };
    unsafe { get_local_time(&mut st) };
    format!("{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}",
        st.year, st.month, st.day, st.hour, st.minute, st.second, st.ms)
}

/// 看门狗线程用它落盘（带时间戳）
fn diag_line(path: &str, s: &str) {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "[{}] {}", diag_now(), s);
    }
}

/// 把状态掩码展开成 `MENU_OPEN|LB_DOWN` 这样的可读文本。
/// 出事后翻日志，一眼能看出「哪个开关忘了复位」。
fn state_str(b: u32) -> String {
    const NAMES: [(u32, &str); 18] = [
        (S_MENU_OPEN, "MENU_OPEN"),
        (S_LOCKED, "LOCKED"),
        (S_DRAG_ACTIVE, "DRAG_ACTIVE"),
        (S_DROP_ARMED, "DROP_ARMED"),
        (S_LB_DOWN, "LB_DOWN"),
        (S_LB_MOVED, "LB_MOVED"),
        (S_HOVER_POSTED, "HOVER_POSTED"),
        (S_DRAG_POSTED, "DRAG_POSTED"),
        (S_EDIT_ARMED, "EDIT_ARMED"),
        (S_HIDING, "HIDING"),
        (S_ENTERING, "ENTERING"),
        (S_CTX_OPEN, "CTX_OPEN"),
        (S_PENDING_RM, "PENDING_RM"),
        (S_HAS_FOCUS_APP, "focus"),
        (S_HAS_HOVER_CHIP, "hover_chip"),
        (S_HAS_HIT_APP, "hit_app"),
        (S_EXEMPT, "EXEMPT"),
        (S_TRIG_CAP, "TRIG_CAPTURE"),
    ];
    let mut v = Vec::new();
    for (bit, name) in NAMES {
        if b & bit != 0 {
            v.push(name);
        }
    }
    if v.is_empty() {
        "（全清）".to_string()
    } else {
        v.join("|")
    }
}

#[derive(Clone)]
struct DropItem {
    name: String,
    target: String,
    is_url: bool,
    is_dir: bool,
}

/// 拖入时"认不出宿主"→ 内环要新建的那个应用（画成**半透明幽灵磁贴**）。
/// 丢到它上面才真正建这个内环。
#[derive(Clone)]
struct GhostApp {
    /// 幽灵磁贴上显示的名字
    name: String,
    /// 新建内环的 target（= 宿主 exe 路径；无宿主时 = 载荷自身，直接成为顶层应用）
    target: String,
    /// true：载荷进它的**外环**（宿主已识别）；false：载荷直接成为这个顶层应用
    put_items: bool,
    /// 幽灵磁贴上显示的**真图标**（宿主 exe / 载荷的图标）。
    /// ⚠️ 必须画真图标：只画首字母（如 Edge 画个 "M"）用户根本看不懂这是啥。
    icon: Option<IconTex>,
}

/// 抓取中（用户正把某个窗口标题栏拖向环）的窗口快照。
/// 拖动期间由 `on_timer` 每帧轮询 `GetGUIThreadInfo().hwndMoveSize` 探测；
/// 松手（hwndMoveSize 变 null）且光标在盘面内 → 落盘成一条窗口条目。
struct WindowCapture {
    /// 被拖动窗口的句柄（运行期；落盘时不再需要）
    hwnd: isize,
    /// 宿主 exe（小写全路径；内环按它路由：认得出→高亮，认不出→幽灵新建）
    exe: String,
    /// 窗口标题（条目名）
    title: String,
    /// 重开命令行（进程命令行，exe + 参数）
    relaunch: String,
    /// 真=由 WM_APP_WINARM 测试通道武装：poll 不得自动清理/落盘，专等 WM_APP_WINDROP
    test: bool,
}

fn pack_pt(x: i32, y: i32) -> i64 {
    ((x as i64) << 32) | (y as i64 & 0xffff_ffff)
}
fn unpack_pt(v: i64) -> (i32, i32) {
    ((v >> 32) as i32, v as i32)
}
const HOTKEY_TOGGLE: i32 = 1;
const TIMER_FRAME: usize = 1;
/// 「单击胶囊 → 延迟 `DBL_WAIT_MS` 再执行」用的定时器。
/// 为什么必须延迟：单击与双击在事件层面**无法区分**，只有等第二击来了才知道。
/// 若不等，双击的第一击就会把应用启动（用户 2026-10-07 报的正是这个）。
const TIMER_CLICK: usize = 2;
/// 「例外名单每 250ms 刷一次前台进程」用的**常驻**定时器。
/// ⚠️ 必须独立于渲染帧定时器（`TIMER_FRAME`）：后者在菜单关掉 / 静止时会停，
/// 若 EXEMPT 只靠帧定时器刷新，关掉菜单后 EXEMPT 就冻结在旧值 ——
/// 表现正是用户 2026-10-11 报的「黑名单里能开一次轮盘、之后切到哪都打不开」。
/// 这个定时器在 MAIN_HWND 上只设一次、永不杀，保证 EXEMPT 始终跟前台同步。
const TIMER_EXEMPT: usize = 3;

static EPOCH: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
fn now_s() -> f64 {
    EPOCH.get_or_init(Instant::now).elapsed().as_secs_f64()
}

// ───────────────────── 全局 ─────────────────────

static MAIN_HWND: AtomicIsize = AtomicIsize::new(0);
static RING_HWND: AtomicIsize = AtomicIsize::new(0);
static MENU_OPEN: AtomicBool = AtomicBool::new(false);
// 鼠标坐标由钩子写入原子量，主线程统一消费（避免移动消息风暴）
static HOOK_HANDLE: AtomicIsize = AtomicIsize::new(0);
static HOOK_TID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
static CURSOR_X: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);
static CURSOR_Y: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);
static HOVER_POSTED: AtomicBool = AtomicBool::new(false);
static TXT_W: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);
/// 键盘钩子句柄（与鼠标钩子同线程安装）
static KEY_HOOK_HANDLE: AtomicIsize = AtomicIsize::new(0);
/// 钩子线程自己持有的句柄副本。用于**线程退场时自我摘钩**——
/// 全局 `HOOK_HANDLE` 可能在 `stop_hook()` 里被 `swap(0)` 拿走，
/// 那样线程退出时就已经不知道自己的句柄是什么了。
static MOUSE_HOOK_LOCAL: AtomicIsize = AtomicIsize::new(0);
static KEY_HOOK_LOCAL: AtomicIsize = AtomicIsize::new(0);
/// 键盘钩子「是否该挂着」。**键盘钩子只在菜单开着时才需要**（Esc/Enter/方向键/Tab，
/// 以及改名时的字符输入）；菜单关着时它在回调里什么都不做，却一直挂在系统输入链上。
///
/// 实测（2026-10-08，用户报「程序启动后偶发打字卡顿，有时五六秒才打出来」）：
/// 日志里 `KEY_SLOW` 成簇出现，耗时 1000 / 3000 / 6813 / **10704ms**，
/// 且**全部发生在菜单关着的时候** —— 那时 `key_hook_impl` 只做两次原子读 +
/// `CallNextHookEx`，所以时间全花在**链上别人的钩子**上（Windows 会等下游钩子返回，
/// 超时才把按键放行）。一个「什么都不做」的钩子让每次按键都可能等几秒，纯属自伤。
/// → 改成**按需装卸**：菜单开时装、关时摘。打字路径上就没有我们的钩子了。
static KEY_HOOK_WANT: AtomicBool = AtomicBool::new(false);
/// `set_running` 把「是否在监听」同步到这里，看门狗线程才不用碰 `st`（跨线程）。
static ST_RUNNING_FLAG: AtomicBool = AtomicBool::new(false);
/// `post()` 失败的节流位（见 post 里的说明）
static POST_DROP_LAST: AtomicBool = AtomicBool::new(false);
/// 仅测试： 里强制使用哪个 chip（-1 = 不强制，走真实 hover）。
/// 见使用处注释。**默认 -1，正常路径完全不受影响。**
static FORCE_CHIP: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(-1);
/// UI 线程私有状态的**跨线程镜像**。看门狗要定期落状态快照，但不能碰
/// `&mut State`（跨线程可变引用 = UB），所以 UI 线程每帧把几个关键值推到这里。
/// `usize::MAX` 表示 None（与 Option<usize> 的判别一致）。
static SNAP_HIDING: AtomicBool = AtomicBool::new(false);
static SNAP_ENTERING: AtomicBool = AtomicBool::new(false);
static SNAP_PENDING_RM: AtomicBool = AtomicBool::new(false);
static SNAP_FOCUS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(usize::MAX);
static SNAP_HOVER_CHIP: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(usize::MAX);
static SNAP_HIT_APP: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(usize::MAX);
/// 常驻模式已锁定（跨线程可见，供钩子线程判断左键语义）
static LOCKED: AtomicBool = AtomicBool::new(false);

const MENU_TOGGLE: u16 = 100;
const MENU_QUIT: u16 = 103;
const MENU_PERSIST: u16 = 106;
const MENU_SETTINGS: u16 = 107;

// ───────────────────────── 模型 ─────────────────────────

#[derive(Clone)]
struct ItemEntry {
    name: String,
    target: String,
    /// 外环段里的**真图标**（mip 链纹理；与内环同一套提取管线）
    icon: Option<IconTex>,
    /// 配色覆盖（GRADS 下标）；None = 随名字自动（grad_for）。
    /// ini 持久化编码在 target 尾部：`target|N`（'|' 在 URL/路径里不合法，够用）
    color: Option<usize>,
    /// 条目种类：0 = 普通（target 即启动目标：URL/文件/exe）；1 = 捕获的窗口
    /// （点开 = 聚焦活着的活动窗口，否则用 `relaunch` 命令行重开）。
    kind: u8,
    /// 窗口关闭后的重开命令（进程命令行，exe + 参数）；kind==1 时有效。
    relaunch: String,
    /// 运行期：当前还活着的窗口句柄（跨会话失效，**不持久化**）。
    /// 0 = 无效；点击时若 `IsWindow` 有效 → 聚焦，否则走 `relaunch`。
    live_hwnd: isize,
}

#[derive(Clone)]
struct AppEntry {
    name: String,
    target: String,
    icon: Option<IconTex>, // mip 链纹理（加载时按 1/1.4 逐级降采样）
    items: Vec<ItemEntry>, // 外环内容（悬停展开）
    grad: usize,           // 磁贴渐变配色索引
}

/// 待确认移除的内容。设计要点：**请求移除的那一刻就从 `st.apps` 摘下**，
/// 盘面上立刻看不见；之后三分歧——
///   倒计时到点 → 落盘（`commit_remove`）
///   点「取消」 → 原位放回（`cancel_remove`）
///   收起/关掉轮盘 → 撤销窗口结束，**立即落盘确认删除**（`commit_all_pending`）
/// 所以这里必须把完整条目和原槽位都揣着，放回时一个字节都不丢。
enum PendingRm {
    /// 应用图标：pos = 原槽位
    App { pos: usize, entry: AppEntry, deadline: f64 },
    /// 外环胶囊：app = 所属应用槽位，pos = item 原序号
    Chip { app: usize, pos: usize, entry: ItemEntry, deadline: f64 },
}

impl PendingRm {
    fn deadline(&self) -> f64 {
        match self {
            PendingRm::App { deadline, .. } | PendingRm::Chip { deadline, .. } => *deadline,
        }
    }
}

/// 最早到期的时刻 —— 取消按钮的倒计时条用它。
/// 用**最早**的那个（而不是最后加入的那个）才对得上用户看到的事：
/// 进度条走完时，确实有一个真的消失了。
fn rm_soonest(st: &State) -> Option<f64> {
    st.pending_rms.iter().map(|p| p.deadline()).reduce(f64::min)
}

struct State {
    // 数据
    apps: Vec<AppEntry>,
    ini_path: String,
    /// ⚠️ 名字有历史包袱：这个不是「液态玻璃」，是**深色主题**（`apply_theme` 按 theme 写它）。
    /// 真正的玻璃材质开关是下面的 `glass`。改名要动 20+ 处，暂时留着。
    glass_liquid: bool,
    /// 材质 = 液态玻璃（外观页可选；ini `glass=`）。玻璃不可用（抓屏失败）会退回纸面。
    glass: bool,
    /// 玻璃背景实时刷新（ini `glass_live=`，缺省开）。关掉 = 只抓呼出那一刻的快照，
    /// 好处是**录屏/截图里能看到轮盘**（实时刷新靠 `WDA_EXCLUDEFROMCAPTURE` 把自己
    /// 排除出抓屏，见 `glass_live_start`）。
    glass_live: bool,
    /// 抓屏 + 模糊后的桌面快照（`CANVAS²×4` BGRA，无预乘）。
    /// **两块拼一起**：`[0..n]` = 抓到的原图、`[n..2n]` = 模糊图（`n = CANVAS²×4`）。
    /// 每个内环应用"是否正在运行"（图标下方那个小圆点）。呼出时算一次。
    app_running: [bool; MAX_APPS],
    /// 每个内环应用**最前面那个窗口**的 hwnd（0 = 没有）。悬停预览用，
    /// 和 `app_running` 同一趟 EnumWindows 一起算出来。
    app_hwnd: [isize; MAX_APPS],
    /// 空 = 玻璃不可用 → 盘面走纸面。菜单收起时还回去（1.8MB）。
    glass_bg: Vec<u8>,
    running: bool,
    /// 常驻模式：松开触发键后菜单保持显示，点击/回车确认，Esc/点空白关闭
    /// （TheRing 1.0.3 的「常驻模式」）
    persistent: bool,
    /// 是否挂钩子（设置页的「监听」开关；从 ini 的 listening= 读）
    listening: bool,
    /// 当前是否已进入常驻锁定态
    locked: bool,
    /// 拖放入环模式：拖放进行中挂起 commit/cancel 语义
    drop_mode: bool,
    /// 拖放悬停时高亮的目标应用（落到谁身上就加进谁的外环）
    drop_target_app: Option<usize>,
    /// 拖入内容"认得出来的宿主内环"（>=0 时那个内环高亮，引导用户往上丢）
    drop_suggest: Option<usize>,
    /// 「拖进来的东西会加到外环末尾」那个幽灵胶囊的淡入（0..1）。
    /// 没有它 = 幽灵在拖动开始时**瞬间**冒出来，和胶囊的平滑让位对不上。
    chip_ghost_a: f32,
    /// 拖入内容"认不出宿主"→ 内环要新建的幽灵磁贴（半透明，丢上去才建）
    drop_ghost: Option<GhostApp>,
    /// 拖窗口进环：当前正被拖动、等待落盘的窗口（None = 没有窗口在拖）
    window_capture: Option<WindowCapture>,
    /// 光标当前正悬停的磁贴（高亮严格跟随：移出即灭，与粘性的 active_app 解耦）
    hover_now: Option<usize>,

    // 交互状态
    active_app: Option<usize>, // 锁定的当前应用（移向外环不丢失）
    hover_chip: Option<usize>,
    /// 指针当前是否**真的**落在某个应用图标热区里（区别于粘性的 active_app）。
    /// 点击/松开必须用它判定，否则点屏幕任意位置都会启动 active_app。
    hit_app: Option<usize>,

    // ── 双击胶囊改名 ──
    /// 正在编辑的条目：Some((应用下标, 条目下标))
    editing: Option<(usize, usize)>,
    /// 输入缓冲（UTF-8）。中文经 IME 合成后由 WM_CHAR / WM_IME_CHAR 追加进来
    edit_buf: String,
    /// 光标闪烁的相位（now_s 口径）
    edit_caret: f64,
    /// 首次按键的时刻 —— 用来判「空缓冲时全选 vs 追加」
    edit_started: f64,
    /// 上一次双击的时刻 + 位置（判双击的第二击）
    edit_last_click: f64,
    edit_last_pos: (i32, i32),
    /// 「单击胶囊，等一下看有没有第二击」：排了延迟执行的目标 (app, chip)。
    /// Some = 有一击待执行（`TIMER_CLICK` 到点后才真正 commit）；
    /// 第二击到来时会被清掉并改为改名。
    pending_click: Option<(usize, usize)>,

    // ── 拖出移除 ──
    /// 头像情绪：0 常态 / 1 紧张（正拖到盘外）/ 2 生气（等待确认移除）
    ///           / 3 发呆（长时间无输入）/ 4 疑惑（盘内拖动）/ 5 忙碌（拖放悬停）
    mood: u8,
    mood_t: f64,          // 进入当前情绪的时刻（驱动过渡与抖动相位）
    mood_k: f32,          // 情绪过渡进度 0→1（帧率无关）
    mood_prev: u8,
    /// 短暂情绪的保持期限（now_s 口径；到点回 0）。0 = 无
    mood_hold: f64,
    // ── 头像气泡：本作**唯一**的反馈通道 ──
    // 起因：所有静默失败都只是 `println!`，用户完全看不到（拖重复、落点没命中、
    // 真删掉那一刻……）。凡是「用户该知道的事」一律走气泡，不再往 stdout 打。
    /// 主行（宽度按文字量收缩，超上限自动省略号）
    bub_main: String,
    /// 副行（空 = 不画）
    bub_sub: String,
    /// 状态消息到期时刻（now_s 口径）。>0 = 有状态消息，**压住** hover 名字
    bub_until: f64,
    /// 出现 / 消失渐变 0→1
    bub_a: f32,
    /// 延迟收菜单的时刻（让气泡显示完再收，别一闪就没）。0 = 不延迟
    hide_at: f64,
    /// 菜单收起（或从未打开）之后的时刻；超过 `TRIM_AFTER` 就裁一次工作集
    trim_at: f64,
    /// 正被拖出的应用（None = 没在拖）
    drag_app: Option<usize>,
    /// 拖拽中光标下方的**目标槽位**（松手时把拖起项插入到这里、其余顺移）。
    /// 内环/外环分开：跨层排序（应用 ↔ 外环条目）没有意义。
    drag_over_app: Option<usize>,
    drag_over_chip: Option<usize>,
    /// 正被拖出的外环胶囊 (应用槽位, item 序号)
    drag_chip: Option<(usize, usize)>,
    /// 被拖图标跟随的画布坐标
    drag_x: f32,
    drag_y: f32,
    /// 拖出淡出进度 0→1（1 = 完全淡掉）
    drag_fade: f32,
    /// 移除区虚线环的**旋转相位**（度，0..360）。越过阈值后转起来 = "已上膛"。
    rm_spin: f32,
    /// 旋转的淡入权重 0..1（越过阈值 → 1，回来 → 0），兼作线宽/浓度加成
    rm_spin_k: f32,
    /// 正在后台查「这个网址是不是已经开着」：`(应用, 条目)` = 哪一段显示 loading 动画。
    /// 期间菜单**保持可见**（宠物切「忙碌」表情），查完再隐藏并动作。
    loading: Option<(usize, usize)>,
    /// loading 起始时刻（兜底：后台万一卡死，菜单不能永远挂着）
    loading_t0: f64,
    /// loading 的类型（`LOAD_KIND_*`）—— 决定收尾走哪条路
    loading_kind: u8,
    /// 上面那次查询对应的网址（查完要拿它决定「切过去」还是「照常打开」）
    loading_url: String,
    // ── 例外名单（黑白名单，二选一生效）──
    // ⚠️ 只会**有一张**生效：`exc_mode` 决定当前是黑名单还是白名单；
    //    选边方式 = 直接点击对应卡片的标题（卡片头），不是独立的切换器。
    //    判定见 `update_exempt`：黑名单命中 → 不生效；白名单非空且未命中 → 不生效。
    /// 0 = 黑名单（名单里的**不生效**）／1 = 白名单（**只有**名单里的生效）
    exc_mode: u8,
    /// 黑名单条目：进程映像文件名，小写（如 `steam.exe`）
    exc: Vec<String>,
    /// 白名单条目（与黑名单**并存**于配置，但同一时刻只有 `exc_mode` 指定的那张真正生效）
    exc_w: Vec<String>,
    /// 前台窗口占满所在显示器时自动放行（省得手动加一堆游戏进程）
    exc_fs: bool,
    /// 交互页「显示应用名称」：鼠标悬停内环图标时，用头像气泡报出应用名。
    /// 关掉就只看图标（气泡仍会提示操作结果 —— 那部分不该被这个开关影响）。
    hover_name: bool,
    /// 上次刷新「当前前台程序是否被例外命中」的时刻 —— 由 **UI 线程**定时刷
    exc_t: f64,
    /// 等待确认移除的内容 —— **已经从盘面摘下**，所以立即不可见；
    /// 超时落盘、点取消或收起菜单都原位放回
    /// 待确认移除的队列。**可以同时有多个** —— 拖走一个之后要能立刻拖下一个，
    /// 不必等它 5 秒倒计时走完（用户 2026-10-09 明确要求）。
    /// 「取消」只作用于**最后加入**的那个；各自按自己的 `deadline` 落盘。
    pending_rms: Vec<PendingRm>,
    /// 取消按钮的出现进度 0→1
    cancel_a: f32,
    /// 最近一次输入（光标移动/点击）的时刻（now_s 口径，发呆判定用）
    last_input: f64,
    /// 上次见到的光标（判定「输入有没有变」）
    cur_x: i32,
    cur_y: i32,

    // 动画
    entering: bool,
    ent0: f64,
    hiding: bool,
    hide0: f64,
    switch0: f64,        // 悬停切换时刻（chip 错峰出现基准）
    // 头像（圆球表情）：呼吸 / 眨眼 / 眼神跟随鼠标
    gaze: [f32; 2],           // 平滑后的注视方向（-1..1）
    blink_q: Vec<(f64, f32)>, // 眨眼关键帧队列 (时刻, 开合度)
    blink_next: f64,          // 下次自动眨眼时刻
    face0: f64,               // 呼吸相位基准
    face_prev: f64,           // 上一帧时刻（求 dt）
    face_n: u32,              // 帧计数（节流：头像 30fps 足够）
    app_anim: [f32; MAX_APPS], // 非悬停压暗渐变
    app_hot: [f32; MAX_APPS], // hover/选中动效渐变（0→1）
    /// 每个应用**拖动重排时当前显示的槽位**（连续值，不是角度 —— 角度会绕 360° 回绕，
    /// 绕回来就没法判断这一格到底该不该淡出）。
    /// 只在**正在拖动**时被读：平滑滑到「预览槽位」→ 图标让位开缝（不是瞬间跳）。
    /// 平时的槽位直接由 `slot_of(st, i)` 给出（滚动偏移已经在那里面动画了）。
    /// `NAN` = 还没初始化过（首帧直接落位，不做动画）。
    app_slot_s: [f32; MAX_APPS],
    // ── 内环连续滚动（超过一屏 `RING_SLOTS` 个应用才生效）──
    /// **当前**滚动偏移，单位 = 槽位（1.0 = 转过一格）。绘制 / 命中 / 外环锚点全走它，
    /// 而且是唯一的动画源 —— 别再往 `app_slot_s` 上叠第二层跟随，那会变成双重拖影。
    ring_off: f32,
    /// 滚轮累加出来的**目标**偏移。`on_timer` 每帧把 `ring_off` 追上来（指数跟随）。
    ring_to: f32,
    // ── 松手落位动画：只有「被拖的那一个」需要动 ──
    /// 正在落位的应用下标（`usize::MAX` = 无）
    land_app: usize,
    /// 落位起点时刻（now_s 口径）
    land_t0: f64,
    /// 落位起点 = 松手时的画布坐标
    land_from: (f32, f32),
    chip_anim: [f32; MAX_ITEMS], // chip 出现渐变
    chip_hot: [f32; MAX_ITEMS], // chip hover 动效渐变（0→1）
    /// 每个外环胶囊**当前显示的角范围**（度）。平时就是 `chip_layout` 给的值；
    /// 拖动重排时平滑滑到「预览布局」→ 胶囊让位开缝（与内环 `app_slot_s` 同款）。
    /// `NAN` = 未初始化（首帧直接落位）。
    chip_seg_a: [(f32, f32); MAX_ITEMS],
    /// 上面那组角度属于哪个应用 —— 换应用就整组重置（不同应用的条目下标没有可比性）
    chip_seg_app: usize,
    /// dump 模式：把合成壁纸铺在盘面下方，便于离线核对观感
    dump_bg: bool,
    commit_pulse: f32,
    commit_app: usize,
    commit_chip: usize,

    // 窗口
    win_x: i32,
    win_y: i32,

    // 渲染资源
    bg_bits: *mut u8,        // 抓屏+模糊后的桌面背景（CANVAS² BGRA）
    bg_dc: HDC,
    bg_bmp: HBITMAP,
    dbi_bits: *mut u8, // 合成输出（CANVAS² BGRA）
    dbi_dc: HDC,
    dbi_bmp: HBITMAP,
    txt_dc: HDC, // 文字遮罩临时 DIB
    txt_bits: *mut u8,
    txt_bmp: HBITMAP,
    font_name: HFONT,  // 17px 600 中心名称
    font_label: HFONT, // 11px 500 小标签
    font_row: HFONT,   // 13px 500 chip 文字
    font_hint: HFONT,  // 10px 400
    font_head: HFONT,  // 14px 600
    font_cnt: HFONT,   // 12px 400 项目数
    font_glyph: HFONT, // 20px 700 无图标时的首字母
    // ⚠️ 气泡字体**刻意不乘 K**。上面那几个都带 `* K`（0.66）或 `* K * KC`，
    //    名义 px ≠ 实际 px ——「12px」的 font_cnt 实际只有 7.9px，
    //    早期气泡直接借它 → 用户报「文字太小根本看不清」（2026-10-14）。
    //    气泡是**读的字**，尺寸必须是绝对像素。K 是编译期常量，不乘也不会漂。
    font_bub: HFONT,   // 14px 500 气泡主行
    font_bub_s: HFONT, // 12px 400 气泡副行
    font_name3: HFONT, // 以下为 3 倍超采样字体（draw_text_rot 旋转文字用）
    font_label3: HFONT,
    font_row3: HFONT,
    font_hint3: HFONT,
    font_head3: HFONT,
    font_cnt3: HFONT,
    font_glyph3: HFONT,


    // ── 设置窗口 ──
    /// 0 亮色 / 1 暗色 / 2 跟随系统（写进 ini 的 dark=）
    theme: i32,
    key_middle: bool,
    key_side: bool,
    key_rhold: bool,
    // ── 自定义触发（录制下一次输入：鼠标键 / 键盘组合键）──
    trig_custom_kind: u8,   // 0 无 / 1 鼠标 / 2 键盘
    trig_custom_btn: u8,    // 鼠标键：1 左 2 右 3 中 4 X1 5 X2（右键不收录，长按右键专属）
    trig_custom_vk: u32,    // 键盘虚拟键码
    trig_custom_mods: u8,   // 修饰键：1 Ctrl 2 Alt 4 Shift 8 Win
    trig_capturing: bool,   // 录制态（设置页显示「请按下…」）
    trig_capture_bad: u8,   // 录制态内按了无效键：1 左键 / 2 右键（设置页显式提示「不可用作触发键」）
    set_page: usize,
    set_hover: Ctl,
    /// 上一个 hover 的控件（配合 set_hov_a 做淡出）
    set_hover_prev: Ctl,
    /// hover 过渡进度 0→1（新 hover 淡入、旧的用 1-它 淡出）
    set_hov_a: f32,
    /// 切页淡入进度 0→1
    set_page_a: f32,
    /// 三个开关的滑动动画值（0 关 / 1 开）：常驻、监听、开机自启
    /// 开关动画量：0 常驻 / 1 监听 / 2 开机自启（交互页）/ 3 全屏放行(游戏模式)（交互页）
    sw_a: [f32; 5],
    /// 悬停中的「?」tooltip id（0 = 不显示）；见 `tip_lines`
    tip: u8,
    /// 开机自启（ini `autostart`；**缺失视为开** → 默认勾选）。
    /// ⚠️ ini 是唯一真相，启动时同步到注册表；用户主动关才写 0 + 删注册表项。
    autostart: bool,
    /// 设置页聚光灯引导：-1 = 未开启；0..=3 = 当前步（见 `guide_target_rect`）
    guide_step: i32,
    /// 引导是否已看过：首启只自动播一次（ini `guide_seen` 持久化）
    guide_seen: bool,

    // ── 在线升级（v1：只查版本）──
    /// 启动时静默检查更新（ini `update_auto`，默认开）
    update_auto: bool,
    /// 上次检查的 unix 秒（ini `update_last_check`）；自动检查 ≥6h 才再查
    update_last_check: f64,
    /// 检查态：0 空闲 / 1 检查中 / 2 已最新 / 3 有新版本 / 4 失败
    update_st: u8,
    /// latest release 的版本号（去掉 v）
    update_latest: String,
    /// 失败原因 / 提示文案（给人看）
    update_msg: String,
    /// 后台检查发现「确实有更新」的那个版本号（ini `update_avail_ver`，空 = 没有）。
    /// ⚠️ 必须落盘：提醒是在**下一次呼出轮盘**时才发生的，中间可能已经重启过。
    update_avail_ver: String,
    /// 已经用头像气泡提醒过的最新版本（ini `update_bub_ver`）—— 每个版本最多提醒一次就靠它
    update_bub_ver: String,

    // ── 拖入引导（onboarding）四步阶梯（用户 2026-10-14 定稿）──
    // ①② 由「**真的做了那个动作**」置位 → 槽位是 self-clearing 的，不需要任何"已读"状态；
    // ③④ 由「播过一次」置位（本来就没有对应动作可等）。
    /// ① 用户拖入过东西（应用 / 文件 / 窗口）
    /// ⚠️ **不能用 `apps.is_empty()` 判** —— `default_ini()` 自带 8 个应用，
    /// 「空环」永远不会出现。这正是必须独立标志的原因。
    onb_drag: bool,
    /// 环上的应用**与出厂默认那8 个不完全一致**（用户自己加过/删过/改过）。
    /// ⚠️ **不落盘**：每次 `load_ini` 都按当前 `[apps]` 段重算 —— 它是「配置长什么样」
    ///    的**推导量**，存进 ini 就会出现"删了应用但标志还在、于是再也不出引导"这种脏状态。
    onb_customized: bool,
    /// ③ 移除 / 换位引导已播过
    onb_move: bool,
    /// ④ 双击改名引导已播过
    onb_edit: bool,
    /// ⑤ 「应用多到一屏装不下 → 用滚轮滚」引导已播过（只在超过 `RING_SLOTS` 后才可能播）
    onb_scroll: bool,
    /// 最近 UIA **探测失败**的窗口（hwnd → 失败时刻 tick）。
    /// 负缓存是必需的：UIA 探测实测 15~3300ms（冷启动更慢），探不到（受保护窗口 /
    /// 公司机器把无障碍关了）时若不记住这次失败，就会每次都重撞整棵元素树。
    /// 以前这里存的是「探到的值」，但判命中写的是 `!v.is_empty()` —— 失败存空串
    /// 等于永远不命中，实测每次 arm 都重跑 40~90ms，全白跑。
    /// 现在探测只在**落盘那一次**发起（`wincap_probe_start`），只留失败记录。
    wincap_failed: Vec<(isize, u32)>,
    /// 上一帧时刻（求 dt，帧率无关）
    set_last: Instant,
    /// 是否已注册 WM_MOUSELEAVE 通知
    set_tracking: bool,
    /// 应用页当前展开的应用（usize::MAX = 都没展开）
    set_expanded: usize,
    set_scroll: i32,
    /// 应用页「＋ 网址」内联输入框：正在编辑哪个应用的条目列表（usize::MAX = 没在编辑）
    set_edit_ai: usize,
    /// 内联输入框的缓冲（UTF-8）；中文由 IME 合成后以 `WM_CHAR` / `WM_IME_CHAR` 追加
    set_edit_buf: String,
    /// 后台抓 `<title>` 进行中的网址：这些条目名先显示「正在取标题」
    title_busy: Vec<String>,
    /// 待确认的删除（`Ctl::AppDel` / `Ctl::ItemDel` / `Ctl::ExcDel` 之一；
    /// `Ctl::None` = 没有卡）。存的就是「要删谁」，确认时照着它执行，不再反查坐标。
    set_confirm: Ctl,
    // 设置窗口字体：固定 px，**不跟 K 缩放** —— 设置页是独立窗口，不该跟着环一起缩。
    // 字号 ×SS：文字是在 SS 倍的 DIB 上画的，降采样后才回到标称字号（同时拿到 AA）。
    sf_title: HFONT,
    /// 「关于」页的产品名（20px/700）—— 比 `sf_title` 大一档，页面才有"产品页"的分量。
    sf_h1: HFONT,
    sf_nav: HFONT,
    sf_body: HFONT,
    sf_small: HFONT,
    sf_bold: HFONT,
    // 设置窗口的超采样缓冲：hi = SS 倍（GDI 画布），lo = 1 倍（降采样结果，直接上屏）
    s_hi_bits: *mut u8,
    s_hi_dc: HDC,
    s_hi_bmp: HBITMAP,
    s_lo_bits: *mut u8,
    /// 「关于」页 logo 的临时画布（`CANVAS`² × 4，懒分配）。
    /// 为什么要有它：头像的 SDF（`flat_circle`/`rot_rrect`）按**全局 CANVAS** 定宽，
    /// 只能画在那么大的画布上，画完再把中心那一小块搬到 hi 缓冲去。
    /// 常驻会白占 ~900KB，所以关设置窗（`setup_free`）就清掉。
    hub_buf: Vec<u8>,
    s_lo_dc: HDC,
    s_lo_bmp: HBITMAP,
    /// 分层窗口的屏幕位置（UpdateLayeredWindow 要用）
    set_x: i32,
    set_y: i32,
}

impl State {
    fn new() -> Self {
        State {
            apps: Vec::new(),
            ini_path: String::new(),
            glass_liquid: true,
            app_running: [false; MAX_APPS],
            app_hwnd: [0; MAX_APPS],
            glass: true,
            glass_live: true,
            glass_bg: Vec::new(),
            running: false,
            persistent: true,
            listening: true,
            locked: false,
            drop_mode: false,
            drop_target_app: None,
            drop_suggest: None,
            chip_ghost_a: 0.0,
            drop_ghost: None,
            window_capture: None,
            hover_now: None,
            active_app: None,
            hover_chip: None,
            hit_app: None,
            editing: None,
            edit_buf: String::new(),
            edit_caret: 0.0,
            edit_started: 0.0,
            edit_last_click: -99.0,
            edit_last_pos: (0, 0),
            pending_click: None,
            mood: 0,
            mood_t: 0.0,
            mood_k: 1.0,
            mood_prev: 0,
            mood_hold: 0.0,
            bub_main: String::new(),
            bub_sub: String::new(),
            bub_until: 0.0,
            bub_a: 0.0,
            hide_at: 0.0,
            trim_at: 0.0,
            drag_app: None,
            drag_over_app: None,
            drag_over_chip: None,
            drag_chip: None,
            drag_x: 0.0,
            drag_y: 0.0,
            drag_fade: 0.0,
            rm_spin: 0.0,
            rm_spin_k: 0.0,
            loading: None,
            loading_t0: 0.0,
            loading_kind: 0,
            loading_url: String::new(),
            exc_mode: 0,
            exc: Vec::new(),
            exc_w: Vec::new(),
            exc_fs: false,
            hover_name: false,
            exc_t: 0.0,
            pending_rms: Vec::new(),
            cancel_a: 0.0,
            last_input: 0.0,
            cur_x: 0,
            cur_y: 0,
            entering: false,
            ent0: 0.0,
            hiding: false,
            hide0: 0.0,
            switch0: 0.0,
            gaze: [0.0, 0.0],
            blink_q: Vec::new(),
            blink_next: 0.0,
            face0: 0.0,
            face_prev: 0.0,
            face_n: 0,
            app_anim: [0.0; MAX_APPS],
            app_hot: [0.0; MAX_APPS],
            app_slot_s: [f32::NAN; MAX_APPS],
            ring_off: 0.0,
            ring_to: 0.0,
            land_app: usize::MAX,
            land_t0: 0.0,
            land_from: (0.0, 0.0),
            chip_anim: [0.0; MAX_ITEMS],
            chip_hot: [0.0; MAX_ITEMS],
            chip_seg_a: [(f32::NAN, f32::NAN); MAX_ITEMS],
            chip_seg_app: usize::MAX,
            dump_bg: false,
            commit_pulse: 0.0,
            commit_app: usize::MAX,
            commit_chip: usize::MAX,
            win_x: 0,
            win_y: 0,
            bg_bits: ptr::null_mut(),
            bg_dc: HDC(ptr::null_mut()),
            bg_bmp: HBITMAP(ptr::null_mut()),
            dbi_bits: ptr::null_mut(),
            dbi_dc: HDC(ptr::null_mut()),
            dbi_bmp: HBITMAP(ptr::null_mut()),
            txt_dc: HDC(ptr::null_mut()),
            txt_bits: ptr::null_mut(),
            txt_bmp: HBITMAP(ptr::null_mut()),
            font_name: HFONT(ptr::null_mut()),
            font_label: HFONT(ptr::null_mut()),
            font_row: HFONT(ptr::null_mut()),
            font_hint: HFONT(ptr::null_mut()),
            font_head: HFONT(ptr::null_mut()),
            font_cnt: HFONT(ptr::null_mut()),
            font_glyph: HFONT(ptr::null_mut()),
            font_bub: HFONT(ptr::null_mut()),
            font_bub_s: HFONT(ptr::null_mut()),
            font_name3: HFONT(ptr::null_mut()),
            font_label3: HFONT(ptr::null_mut()),
            font_row3: HFONT(ptr::null_mut()),
            font_hint3: HFONT(ptr::null_mut()),
            font_head3: HFONT(ptr::null_mut()),
            font_cnt3: HFONT(ptr::null_mut()),
            font_glyph3: HFONT(ptr::null_mut()),

            // 设置
            theme: 1,
            // 默认只开「鼠标侧键」呼出（用户 2026-10-18 拍板）：中键在浏览器/软件里
            // 常被用作「新标签/平移」，易误触；右键长按也不开。
            key_middle: false,
            key_side: true,
            key_rhold: false,
            trig_custom_kind: 0,
            trig_custom_btn: 0,
            trig_custom_vk: 0,
            trig_custom_mods: 0,
            trig_capturing: false,
            trig_capture_bad: 0,
            set_page: 0,
            set_hover: Ctl::None,
            set_hover_prev: Ctl::None,
            set_hov_a: 1.0,
            set_page_a: 1.0,
            sw_a: [1.0, 1.0, 0.0, 0.0, 1.0],
            tip: 0,
            // 默认开：ini 里没有 `autostart=` 时就是它（用户主动关才变 false）
            autostart: true,
            guide_step: -1,
            guide_seen: false,
            // 在线升级：默认启动静默检查（只查不装）；last_check=0 → 首次启动必查
            update_auto: true,
            update_last_check: 0.0,
            update_st: 0,
            update_latest: String::new(),
            update_msg: String::new(),
            update_avail_ver: String::new(),
            update_bub_ver: String::new(),
            onb_drag: false,
            onb_customized: false,
            onb_move: false,
            onb_edit: false,
            onb_scroll: false,
            wincap_failed: Vec::new(),
            set_last: Instant::now(),
            set_tracking: false,
            set_expanded: usize::MAX,
            set_scroll: 0,
            set_edit_ai: usize::MAX,
            set_edit_buf: String::new(),
            set_confirm: Ctl::None,
            title_busy: Vec::new(),
            sf_title: HFONT(ptr::null_mut()),
            sf_h1: HFONT(ptr::null_mut()),
            sf_nav: HFONT(ptr::null_mut()),
            sf_body: HFONT(ptr::null_mut()),
            sf_small: HFONT(ptr::null_mut()),
            sf_bold: HFONT(ptr::null_mut()),
            s_hi_bits: ptr::null_mut(),
            s_hi_dc: HDC(ptr::null_mut()),
            s_hi_bmp: HBITMAP(ptr::null_mut()),
            s_lo_bits: ptr::null_mut(),
            hub_buf: Vec::new(),
            s_lo_dc: HDC(ptr::null_mut()),
            s_lo_bmp: HBITMAP(ptr::null_mut()),
            set_x: 0,
            set_y: 0,
        }
    }
}

// 磁贴配色：全部为中性灰阶（无彩色）。
// 仅用于「没有真实图标」的字母磁贴，以及外环 chip 左端的字母方块。
// 真实 app 图标是彩色内容，不在这里染灰。
const GRADS: [((u8, u8, u8), (u8, u8, u8)); 14] = [
    ((62, 62, 66), (62, 62, 66)),     // 0
    ((84, 84, 89), (84, 84, 89)),     // 1
    ((44, 44, 48), (44, 44, 48)),     // 2
    ((110, 110, 115), (110, 110, 115)), // 3
    ((92, 92, 97), (92, 92, 97)),     // 4
    ((70, 70, 74), (70, 70, 74)),     // 5
    ((122, 122, 127), (122, 122, 127)), // 6
    ((54, 54, 58), (54, 54, 58)),     // 7
    ((86, 86, 91), (86, 86, 91)),     // 8  默认
    ((122, 122, 127), (122, 122, 127)), // 9
    ((98, 98, 103), (98, 98, 103)),   // 10
    ((84, 84, 89), (84, 84, 89)),     // 11
    ((98, 98, 103), (98, 98, 103)),   // 12
    ((68, 68, 72), (68, 68, 72)),     // 13
];

/// ── 配色：黑白灰（无彩色）──
/// 亮色：浅灰底 + 白纸 + 近黑墨；强调色直接用墨色（不再是蓝色）
const P_WALL: (u8, u8, u8) = (238, 238, 240); // #EEEEF0
const P_WALL2: (u8, u8, u8) = (227, 227, 230); // #E3E3E6
const P_PAPER: (u8, u8, u8) = (255, 255, 255);
const P_INK: (u8, u8, u8) = (28, 28, 30); // #1C1C1E
const P_ACTION: (u8, u8, u8) = (28, 28, 30); // 强调 = 墨色
/// 暗色：中性深灰（不带蓝调）
const D_WALL: (u8, u8, u8) = (22, 22, 24); // #161618
const D_WALL2: (u8, u8, u8) = (15, 15, 17); // #0F0F11
const D_PAPER: (u8, u8, u8) = (36, 36, 39); // #242427
const D_INK: (u8, u8, u8) = (236, 236, 238); // #ECECEE
/// 深色下「**反色块**」的底色（实心块 + 反色文字的那种元件）。
///
/// ⚠️ 不能拿 `D_INK` 当填充用：那是**文字色**。当它变成一大块实心（头像球、
/// 侧栏选中的 pill、反色按钮、开关轨道、「生效中」标签、tooltip 气泡）时，
/// 在深色界面上白得扎眼 —— 用户 2026-10-14 报「深色模式按钮等元素的反色太亮」。
/// 压到约 85% 亮度：仍然明确是「亮的那一块」，但不刺眼。
/// ⚠️ 文字色 `ink` **不动** —— 那是可读性，改暗了就是自杀。
const D_BLOCK: (u8, u8, u8) = (202, 202, 205); // #CACACD
const D_ACTION: (u8, u8, u8) = (236, 236, 238); // 强调 = 墨色

#[derive(Clone, Copy)]
struct Pal {
    wall: (u8, u8, u8),
    wall2: (u8, u8, u8),
    paper: (u8, u8, u8),
    ink: (u8, u8, u8),
    /// 「反色块」底色 = 实心块元件（头像球 / 选中 pill / 反色按钮 / 开关轨道）的填充。
    /// 浅色下与 `ink` 同值；**深色下比 `ink` 暗**（见 `D_BLOCK` 的注释）。
    /// ⚠️ 只用于**填充**；文字/描边继续用 `ink`。
    block: (u8, u8, u8),
    action: (u8, u8, u8),
    /// control-edge = color-mix(ink 62%, paper)
    edge: (u8, u8, u8),
}

fn mixc(a: (u8, u8, u8), b: (u8, u8, u8), t: f32) -> (u8, u8, u8) {
    let t = t.clamp(0.0, 1.0);
    (
        (a.0 as f32 + (b.0 as f32 - a.0 as f32) * t) as u8,
        (a.1 as f32 + (b.1 as f32 - a.1 as f32) * t) as u8,
        (a.2 as f32 + (b.2 as f32 - a.2 as f32) * t) as u8,
    )
}

fn pal_of(dark: bool) -> Pal {
    if dark {
        let edge = mixc(D_INK, D_PAPER, 0.38);
        Pal {
            wall: D_WALL,
            wall2: D_WALL2,
            paper: D_PAPER,
            ink: D_INK,
            block: D_BLOCK,
            action: D_ACTION,
            edge,
        }
    } else {
        let edge = mixc(P_INK, P_PAPER, 0.38);
        // 浅色下「反色块」就是墨色本身 —— 和以前完全一致，不动
        Pal {
            wall: P_WALL,
            wall2: P_WALL2,
            paper: P_PAPER,
            ink: P_INK,
            block: P_INK,
            action: P_ACTION,
            edge,
        }
    }
}

/// 投影色：中性近黑（不带蓝调）
const SHADOW: (u8, u8, u8) = (16, 16, 18);

/** 覆盖率：隐式边缘 1px 抗锯齿（sdf<0 在内部） */
#[inline]
fn cov1(sdf: f32) -> f32 {
    (0.5 - sdf).clamp(0.0, 1.0)
}

// ───────────────────── 基础工具 ─────────────────────

/// 4x 超采样覆盖率的子样本偏移
#[allow(dead_code)]
const SUB: [(f32, f32); 4] = [(-0.25, -0.25), (0.25, -0.25), (-0.25, 0.25), (0.25, 0.25)];

#[inline]
fn clamp01(v: f32) -> f32 {
    v.clamp(0.0, 1.0)
}

#[inline]
fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

/// easeOutCubic
#[inline]
fn ease_out(v: f32) -> f32 {
    1.0 - (1.0 - v).powi(3)
}

/// easeOutBack（入场「弹出」手感）
#[inline]
fn ease_out_back(v: f32) -> f32 {
    const C1: f32 = 1.70158;
    const C3: f32 = C1 + 1.0;
    1.0 + C3 * (v - 1.0).powi(3) + C1 * (v - 1.0).powi(2)
}

/// source-over 合成（按 alpha 归一化，非预乘）
#[inline]
fn blend_over(buf: &mut [u8], o: usize, cr: f32, cg: f32, cb: f32, a: f32) {
    if a <= 0.0 {
        return;
    }
    let a = a.min(1.0);
    let da = buf[o + 3] as f32 / 255.0;
    let oa = a + da * (1.0 - a);
    if oa <= 0.0001 {
        return;
    }
    let mix = |src: f32, dst: f32| ((src * a + dst * da * (1.0 - a)) / oa).clamp(0.0, 255.0);
    buf[o] = mix(cb, buf[o] as f32) as u8;
    buf[o + 1] = mix(cg, buf[o + 1] as f32) as u8;
    buf[o + 2] = mix(cr, buf[o + 2] as f32) as u8;
    buf[o + 3] = (oa * 255.0) as u8;
}

/// 圆角矩形 SDF（**内部为正**）
fn sdf_rounded_rect(dx: f32, dy: f32, half_w: f32, half_h: f32, r: f32) -> f32 {
    let qx = dx.abs() - half_w + r;
    let qy = dy.abs() - half_h + r;
    let ox = qx.max(0.0);
    let oy = qy.max(0.0);
    r - (ox * ox + oy * oy).sqrt() - qx.min(0.0).max(qy.min(0.0))
}

/// 圆 SDF（**内部为正**）
#[allow(dead_code)]
#[inline]
fn circle_sdf(dx: f32, dy: f32, r: f32) -> f32 {
    r - (dx * dx + dy * dy).sqrt()
}

/// 4x 超采样覆盖率
#[allow(dead_code)]
fn cov4(f: impl Fn(f32, f32) -> f32, dx: f32, dy: f32) -> f32 {
    let mut c = 0u32;
    for (ox, oy) in SUB {
        if f(dx + ox, dy + oy) > 0.0 {
            c += 1;
        }
    }
    c as f32 / 4.0
}

/// 扁平圆：**向下偏移的双层模糊投影** → 描边（可半透明）→ 填充。全部纯像素。
///
/// 投影必须「偏移 + 模糊」，不能做成以圆心为中心的对�称光晕 ——
/// 那样等于给盘面套了一圈半透明的黑边，看起来又粗又脏（上一版的「大黑边」）。
#[allow(clippy::too_many_arguments)]
unsafe fn flat_circle(
    buf: &mut [u8],
    cx: f32,
    cy: f32,
    r: f32,
    fill: (u8, u8, u8),
    bw: f32,
    border: (u8, u8, u8),
    border_a: f32,
    sh_dy: f32,
    sh_a: f32,
    sh_blur: f32,
    am: f32,
) {
    let ext = r + bw + sh_dy.abs() + sh_blur * 1.7 + 2.0;
    let y0 = (cy - ext).max(0.0) as usize;
    let y1 = ((cy + ext).min(CANVAS as f32 - 1.0)) as usize;
    let x0 = (cx - ext).max(0.0) as usize;
    let x1 = ((cx + ext).min(CANVAS as f32 - 1.0)) as usize;
    let w = CANVAS as usize;
    let sh = SHADOW;
    let rb = r + bw;
    // 投影扩散层最多只能覆盖到 (r - bw - |sh_dy|*1.55 - sh_blur*1.7 - 2)，
    // 深于这条线的像素**必然**落在填充区内部，算了也会被不透明填充盖掉。
    // 提前跳出可让整块盘的投影开销从 ~10 万像素降到 ~2 万。
    let sh_in = bw + sh_dy.abs() * 1.55 + sh_blur * 1.7 + 2.0;
    for py in y0..=y1 {
        let dy = py as f32 + 0.5 - cy;
        for px in x0..=x1 {
            let dx = px as f32 + 0.5 - cx;
            let d = (dx * dx + dy * dy).sqrt() - r;
            let o = (py * w + px) * 4;
            if sh_a > 0.001 && sh_blur > 0.1 && d > -sh_in {
                // 贴身层：偏移 1.0×
                let s1 = (dx * dx + (dy - sh_dy).powi(2)).sqrt() - rb;
                let b1 = sh_blur * 0.55;
                if s1 > -2.0 && s1 < b1 {
                    let t = 1.0 - (s1.max(0.0) / b1);
                    blend_over(buf, o, sh.0 as f32, sh.1 as f32, sh.2 as f32, t * t * sh_a * am);
                }
                // 扩散层：偏移 1.55×，更大更淡（模拟大半径高斯）
                let s2 = (dx * dx + (dy - sh_dy * 1.55).powi(2)).sqrt() - rb;
                let b2 = sh_blur * 1.7;
                if s2 > -2.0 && s2 < b2 {
                    let t = 1.0 - (s2.max(0.0) / b2);
                    blend_over(buf, o, sh.0 as f32, sh.1 as f32, sh.2 as f32, t * t * sh_a * 0.62 * am);
                }
            }
            let cb = cov1(d - bw) * border_a;
            if cb > 0.0 {
                blend_over(buf, o, border.0 as f32, border.1 as f32, border.2 as f32, cb * am);
            }
            let cf = cov1(d);
            if cf > 0.0 {
                blend_over(buf, o, fill.0 as f32, fill.1 as f32, fill.2 as f32, cf * am);
            }
        }
    }
}

/// 扁平圆角矩形：同 flat_circle
#[allow(clippy::too_many_arguments)]
unsafe fn flat_round_rect(
    buf: &mut [u8],
    cx: f32,
    cy: f32,
    half_w: f32,
    half_h: f32,
    rad: f32,
    fill: (u8, u8, u8),
    bw: f32,
    border: (u8, u8, u8),
    border_a: f32,
    sh_dy: f32,
    sh_a: f32,
    sh_blur: f32,
    am: f32,
) {
    let ext = half_w.max(half_h) + bw + sh_dy.abs() + sh_blur * 1.7 + 2.0;
    let y0 = (cy - ext).max(0.0) as usize;
    let y1 = ((cy + ext).min(CANVAS as f32 - 1.0)) as usize;
    let x0 = (cx - ext).max(0.0) as usize;
    let x1 = ((cx + ext).min(CANVAS as f32 - 1.0)) as usize;
    let w = CANVAS as usize;
    let sh = SHADOW;
    // 注意：sdf_rounded_rect 是「内部为正」，统一取负
    let sdf = |x: f32, y: f32| -sdf_rounded_rect(x, y, half_w, half_h, rad);
    // 投影早退：深于形状内部的像素算出来的投影必然被不透明填充盖掉
    let sh_in = bw + sh_dy.abs() * 1.55 + sh_blur * 1.7 + 2.0;
    for py in y0..=y1 {
        let dy = py as f32 + 0.5 - cy;
        for px in x0..=x1 {
            let dx = px as f32 + 0.5 - cx;
            let d = sdf(dx, dy);
            let o = (py * w + px) * 4;
            if sh_a > 0.001 && sh_blur > 0.1 && d > -sh_in {
                let s1 = sdf(dx, dy - sh_dy) - bw;
                let b1 = sh_blur * 0.55;
                if s1 > -2.0 && s1 < b1 {
                    let t = 1.0 - (s1.max(0.0) / b1);
                    blend_over(buf, o, sh.0 as f32, sh.1 as f32, sh.2 as f32, t * t * sh_a * am);
                }
                let s2 = sdf(dx, dy - sh_dy * 1.55) - bw;
                let b2 = sh_blur * 1.7;
                if s2 > -2.0 && s2 < b2 {
                    let t = 1.0 - (s2.max(0.0) / b2);
                    blend_over(buf, o, sh.0 as f32, sh.1 as f32, sh.2 as f32, t * t * sh_a * 0.62 * am);
                }
            }
            let cb = cov1(d - bw) * border_a;
            if cb > 0.0 {
                blend_over(buf, o, border.0 as f32, border.1 as f32, border.2 as f32, cb * am);
            }
            let cf = cov1(d);
            if cf > 0.0 {
                blend_over(buf, o, fill.0 as f32, fill.1 as f32, fill.2 as f32, cf * am);
            }
        }
    }
}

#[allow(dead_code)]
/// 球体径向渐变：光源在左上，中心亮 → 边缘暗，做出「绒球」的体积感。
/// 参数比例照搬 my-pet 的 `radialGradient cx=38% cy=32% r=75%`。
#[allow(clippy::too_many_arguments)]
unsafe fn ball_gradient(
    buf: &mut [u8],
    cx: f32,
    cy: f32,
    rx: f32,
    ry: f32,
    hi: (u8, u8, u8),
    mid: (u8, u8, u8),
    lo: (u8, u8, u8),
) {
    if rx <= 0.5 || ry <= 0.5 {
        return;
    }
    let w = CANVAS as usize;
    let lx = cx - 0.24 * rx; // 光源 x（对应 38%）
    let ly = cy - 0.36 * ry; // 光源 y（对应 32%）
    let rr = 1.5 * rx.max(ry);
    let ext = rx.max(ry) + 2.0;
    let y0 = (cy - ext).max(0.0) as usize;
    let y1 = ((cy + ext).min(CANVAS as f32 - 1.0)) as usize;
    let x0 = (cx - ext).max(0.0) as usize;
    let x1 = ((cx + ext).min(CANVAS as f32 - 1.0)) as usize;
    let px_scale = rx.min(ry).max(0.5);
    for py in y0..=y1 {
        let dy = py as f32 + 0.5 - cy;
        for px in x0..=x1 {
            let dx = px as f32 + 0.5 - cx;
            let q = ((dx / rx).powi(2) + (dy / ry).powi(2)).sqrt();
            let cov = cov1((q - 1.0) * px_scale);
            if cov <= 0.0 {
                continue;
            }
            let gx = (px as f32 + 0.5 - lx) / rr;
            let gy = (py as f32 + 0.5 - ly) / rr;
            let t = (gx * gx + gy * gy).sqrt().clamp(0.0, 1.0);
            let col = if t < 0.62 {
                mixc(hi, mid, t / 0.62)
            } else {
                mixc(mid, lo, (t - 0.62) / 0.38)
            };
            blend_over(buf, (py * w + px) * 4, col.0 as f32, col.1 as f32, col.2 as f32, cov);
        }
    }
}

/// 旋转的圆角矩形（绒球的眼睛是「微微倾斜的细长条」）。
#[allow(clippy::too_many_arguments)]
unsafe fn rot_rrect(
    buf: &mut [u8],
    cx: f32,
    cy: f32,
    hw: f32,
    hh: f32,
    rad: f32,
    rot: f32,
    col: (u8, u8, u8),
    a: f32,
) {
    if hw <= 0.05 || hh <= 0.05 || a <= 0.002 {
        return;
    }
    let w = CANVAS as usize;
    let (sn, cs) = rot.sin_cos();
    let ext = hw.max(hh) + 3.0;
    let y0 = (cy - ext).max(0.0) as usize;
    let y1 = ((cy + ext).min(CANVAS as f32 - 1.0)) as usize;
    let x0 = (cx - ext).max(0.0) as usize;
    let x1 = ((cx + ext).min(CANVAS as f32 - 1.0)) as usize;
    for py in y0..=y1 {
        let dy = py as f32 + 0.5 - cy;
        for px in x0..=x1 {
            let dx = px as f32 + 0.5 - cx;
            let lx = dx * cs + dy * sn;
            let ly = -dx * sn + dy * cs;
            // sdf_rounded_rect 内部为正 → 取负得「内部为负」
            let cov = cov1(-sdf_rounded_rect(lx, ly, hw, hh, rad));
            if cov <= 0.0 {
                continue;
            }
            blend_over(buf, (py * w + px) * 4, col.0 as f32, col.1 as f32, col.2 as f32, cov * a);
        }
    }
}

/// 旋转圆角矩形，支持描边与「向下偏移」的柔和投影。
///
/// 为什么需要它：外环 chip 要沿弧线**切线**排布（贴弧胶囊），
/// 轴对齐的 flat_round_rect 画出来还是「水平浮在旁边的药丸」。
/// 投影方向固定在**屏幕**竖直方向（不是局部坐标），否则 chip 转到侧面时
/// 影子会朝切线方向偏，看起来像悬空。
#[allow(clippy::too_many_arguments)]
#[allow(dead_code)] // 外环改梯形段后暂无调用者；它是有用的「旋转圆角矩形 + 投影」原语，留着
unsafe fn rot_rrect_ex(
    buf: &mut [u8],
    cx: f32,
    cy: f32,
    hw: f32,
    hh: f32,
    rad: f32,
    rot: f32,
    fill: (u8, u8, u8),
    bw: f32,
    border: (u8, u8, u8),
    border_a: f32,
    sh_dy: f32,
    sh_a: f32,
    sh_blur: f32,
    am: f32,
) {
    let w = CANVAS as usize;
    let (sn, cs) = rot.sin_cos();
    // 屏幕竖直偏移换算到局部坐标
    let (shx, shy) = (sh_dy * sn, sh_dy * cs);
    let ext = hw.max(hh) + bw + sh_dy.abs() + sh_blur * 1.7 + 3.0;
    let y0 = (cy - ext).max(0.0) as usize;
    let y1 = ((cy + ext).min(CANVAS as f32 - 1.0)) as usize;
    let x0 = (cx - ext).max(0.0) as usize;
    let x1 = ((cx + ext).min(CANVAS as f32 - 1.0)) as usize;
    let sh = SHADOW;
    // 投影早退：深于形状内部的像素算出来的投影必然被不透明填充盖掉
    let sh_in = bw + sh_dy.abs() * 1.55 + sh_blur * 1.7 + 2.0;
    for py in y0..=y1 {
        let dy = py as f32 + 0.5 - cy;
        for px in x0..=x1 {
            let dx = px as f32 + 0.5 - cx;
            // 旋回局部坐标：+x 沿切线，−y 朝外
            let lx = dx * cs + dy * sn;
            let ly = -dx * sn + dy * cs;
            let d = -sdf_rounded_rect(lx, ly, hw, hh, rad);
            let o = (py * w + px) * 4;
            if sh_a > 0.001 && sh_blur > 0.1 && d > -sh_in {
                let s1 = -sdf_rounded_rect(lx - shx, ly - shy, hw, hh, rad) - bw;
                let b1 = sh_blur * 0.55;
                if s1 > -2.0 && s1 < b1 {
                    let t = 1.0 - (s1.max(0.0) / b1);
                    blend_over(buf, o, sh.0 as f32, sh.1 as f32, sh.2 as f32, t * t * sh_a * am);
                }
                let s2 = -sdf_rounded_rect(lx - shx * 1.55, ly - shy * 1.55, hw, hh, rad) - bw;
                let b2 = sh_blur * 1.7;
                if s2 > -2.0 && s2 < b2 {
                    let t = 1.0 - (s2.max(0.0) / b2);
                    blend_over(buf, o, sh.0 as f32, sh.1 as f32, sh.2 as f32,
                        t * t * sh_a * 0.62 * am);
                }
            }
            let cb = cov1(d - bw) * border_a;
            if cb > 0.0 {
                blend_over(buf, o, border.0 as f32, border.1 as f32, border.2 as f32, cb * am);
            }
            let cf = cov1(d);
            if cf > 0.0 {
                blend_over(buf, o, fill.0 as f32, fill.1 as f32, fill.2 as f32, cf * am);
            }
        }
    }
}

#[allow(dead_code)]
/// 椭圆（可描边、可半透明填充）。用归一化距离近似 SDF，边缘 1px 抗锯齿。
/// 头像的眼睛需要「垂直压扁」做眨眼，所以不能只用圆。
#[allow(clippy::too_many_arguments)]
unsafe fn flat_ellipse(
    buf: &mut [u8],
    cx: f32,
    cy: f32,
    rx: f32,
    ry: f32,
    fill: (u8, u8, u8),
    fill_a: f32,
    bw: f32,
    border: (u8, u8, u8),
    border_a: f32,
    am: f32,
) {
    if rx <= 0.05 || ry <= 0.05 {
        return;
    }
    let w = CANVAS as usize;
    let ext = rx.max(ry) + bw + 2.0;
    let y0 = (cy - ext).max(0.0) as usize;
    let y1 = ((cy + ext).min(CANVAS as f32 - 1.0)) as usize;
    let x0 = (cx - ext).max(0.0) as usize;
    let x1 = ((cx + ext).min(CANVAS as f32 - 1.0)) as usize;
    // 把归一化半径差换算回像素量级，保证抗锯齿带宽稳定
    let px_scale = rx.min(ry).max(0.5);
    for py in y0..=y1 {
        let dy = py as f32 + 0.5 - cy;
        for px in x0..=x1 {
            let dx = px as f32 + 0.5 - cx;
            let q = ((dx / rx).powi(2) + (dy / ry).powi(2)).sqrt();
            let d = (q - 1.0) * px_scale; // < 0 在内部
            let o = (py * w + px) * 4;
            let cb = cov1(d - bw) * border_a;
            if cb > 0.0 {
                blend_over(buf, o, border.0 as f32, border.1 as f32, border.2 as f32, cb * am);
            }
            let cf = cov1(d) * fill_a;
            if cf > 0.0 {
                blend_over(buf, o, fill.0 as f32, fill.1 as f32, fill.2 as f32, cf * am);
            }
        }
    }
}

/// 盘面垂直渐变：上亮下暗，让白盘读作「一块实体纸」而不是一坨纯色。
/// 幅度必须很小（≤6%），大了就变成「渐变球」，反而不高级。
unsafe fn disc_gradient(
    buf: &mut [u8],
    cx: f32,
    cy: f32,
    r: f32,
    top: (u8, u8, u8),
    bot: (u8, u8, u8),
) {
    let w = CANVAS as usize;
    let y0 = (cy - r).max(0.0) as usize;
    let y1 = ((cy + r).min(CANVAS as f32 - 1.0)) as usize;
    let x0 = (cx - r).max(0.0) as usize;
    let x1 = ((cx + r).min(CANVAS as f32 - 1.0)) as usize;
    for py in y0..=y1 {
        let dy = py as f32 + 0.5 - cy;
        for px in x0..=x1 {
            let dx = px as f32 + 0.5 - cx;
            let c = cov1((dx * dx + dy * dy).sqrt() - r);
            if c <= 0.0 {
                continue;
            }
            let t = ((dy + r) / (2.0 * r).max(1.0)).clamp(0.0, 1.0);
            let col = (
                lerp(top.0 as f32, bot.0 as f32, t),
                lerp(top.1 as f32, bot.1 as f32, t),
                lerp(top.2 as f32, bot.2 as f32, t),
            );
            blend_over(buf, (py * w + px) * 4, col.0, col.1, col.2, c);
        }
    }
}

/// 圆环带（annulus）：给盘面一条「图标轨道」，让图标看起来是被安置的，
/// 而不是散落在一块白饼上。这是盘面从「一片空白」变成「有结构」的关键。
unsafe fn ring_band(
    buf: &mut [u8],
    cx: f32,
    cy: f32,
    r0: f32,
    r1: f32,
    fill: (u8, u8, u8),
    a: f32,
) {
    if a <= 0.002 || r1 <= r0 {
        return;
    }
    let w = CANVAS as usize;
    let ext = r1 + 2.0;
    let y0 = (cy - ext).max(0.0) as usize;
    let y1 = ((cy + ext).min(CANVAS as f32 - 1.0)) as usize;
    let x0 = (cx - ext).max(0.0) as usize;
    let x1 = ((cx + ext).min(CANVAS as f32 - 1.0)) as usize;
    for py in y0..=y1 {
        let dy = py as f32 + 0.5 - cy;
        for px in x0..=x1 {
            let dx = px as f32 + 0.5 - cx;
            let d = (dx * dx + dy * dy).sqrt();
            // 到环带最近边的距离（内部为正）
            let sdf = (d - r0).min(r1 - d);
            let c = cov1(-sdf);
            if c <= 0.0 {
                continue;
            }
            blend_over(buf, (py * w + px) * 4, fill.0 as f32, fill.1 as f32, fill.2 as f32, c * a);
        }
    }
}

#[allow(dead_code)]
/// 柔和圆角矩形色块（边缘 blur px 渐隐，无描边）
#[allow(clippy::too_many_arguments)]
unsafe fn soft_rrect(
    buf: &mut [u8],
    cx: f32,
    cy: f32,
    hw: f32,
    hh: f32,
    rad: f32,
    col: (u8, u8, u8),
    a: f32,
    blur: f32,
) {
    if a <= 0.002 {
        return;
    }
    let w = CANVAS as usize;
    let ext = hw.max(hh) + blur + 2.0;
    let y0 = (cy - ext).max(0.0) as usize;
    let y1 = ((cy + ext).min(CANVAS as f32 - 1.0)) as usize;
    let x0 = (cx - ext).max(0.0) as usize;
    let x1 = ((cx + ext).min(CANVAS as f32 - 1.0)) as usize;
    let b = blur.max(0.35);
    for py in y0..=y1 {
        let dy = py as f32 + 0.5 - cy;
        for px in x0..=x1 {
            let dx = px as f32 + 0.5 - cx;
            // sdf_rounded_rect 内部为正 → 取负得「内部为负」
            let d = -sdf_rounded_rect(dx, dy, hw, hh, rad);
            if d >= b {
                continue;
            }
            let t = 1.0 - (d.max(0.0) / b);
            blend_over(buf, (py * w + px) * 4, col.0 as f32, col.1 as f32, col.2 as f32, t * t * a);
        }
    }
}

// ───────────────────── 头像气泡 ─────────────────────
//
// 唯一的反馈通道：拖重复 / 落点没命中 / 真删掉那一刻，以前都只是 `println!`，
// 用户完全看不到。凡是「用户该知道的事」一律走这里。
//
// ⚠️ 几何是量出来的，不是拍的：画布只有 475×475，盘心 237.6、球半径 43.56，
// 所以气泡**左边缘钉死 x=262**（贴着球的右上）、最多长到 x=462 → 宽上限 150。
// 宽度**按文字量收缩**（短消息只有 ~90），不是一律撑满 —— 撑满就是「太大了」。
const BUB_X: f32 = 262.0;
/// 气泡**底边**（不含尾巴）钉在这里，往上长 —— 高了就往上蹿，不会压到头像球上
/// （球顶在 194，尾巴尖再从这儿往下探 13）。
const BUB_Y_BOT: f32 = 193.0;
const BUB_W_MAX: f32 = 190.0;
const BUB_W_MIN: f32 = 56.0;
const BUB_PAD: f32 = 12.0;
const BUB_R: f32 = 10.0;
/// ⚠️ 21/22 = 绝对像素的 `font_bub`(14px) / `font_bub_s`(12px)。
/// 不能用 12/10 —— 那两个是 `font_cnt`(12*K=7.9px) / `font_hint`(10*K=6.6px)，
/// 借来用就是「小到看不清」（用户 2026-10-14 报）。
const BUB_MAIN_PX: i32 = 21;
const BUB_MAIN_LH: f32 = 19.0;
const BUB_SUB_PX: i32 = 22;
const BUB_SUB_LH: f32 = 16.0;

/// 说一句话。`secs > 0` = 状态消息（**压住** hover 名字，到点自己消失）；
/// `secs <= 0` = 常驻（如「松手就移除」），必须由调用方用 `hush` 收掉。
fn say(st: &mut State, main: &str, sub: &str, secs: f64) {
    st.bub_main = main.to_string();
    st.bub_sub = sub.to_string();
    st.bub_until = if secs > 0.0 { now_s() + secs } else { f64::INFINITY };
}

/// 收掉常驻消息（拖回盘内 / 松手放弃时用）。
///
/// ⚠️ 只认**常驻**（`is_infinite`）—— 定时消息让它自己到期，别被顺手清掉。
fn hush(st: &mut State) {
    if st.bub_until.is_infinite() {
        st.bub_until = 0.0;
        st.bub_main.clear();
        st.bub_sub.clear();
    }
}

/// 拖进来的东西叫什么（给气泡用：得告诉用户**加了什么 / 什么没加成**）。
fn drop_name(items: &[DropItem]) -> String {
    match items.first() {
        Some(it) if !it.name.is_empty() => clamp_name(&it.name),
        Some(it) => name_from_path(&it.target),
        None => String::new(),
    }
}

/// 当前该显示什么。优先级：**状态消息 > hover 内环名字**。
/// hover 名字是低优先级常驻态，状态消息在时它被压住 —— 不然两条会互相顶掉。
fn bub_text(st: &State) -> Option<(String, String)> {
    if st.bub_until > now_s() {
        return Some((st.bub_main.clone(), st.bub_sub.clone()));
    }
    // hover 内环 → 报出应用名（交互页「显示应用名称」可关）。
    // 图标常常是灰色方块（认不出 exe 时）或通用图标，光看图猜不出来是哪个应用。
    if st.hover_name {
        if let Some(i) = st.hover_now {
            if let Some(a) = st.apps.get(i) {
                // 应用只报名字。曾经带一行「N 个常用项」—— 用户 2026-10-14 明确说不要：
                // 悬停时外环胶囊已经把条目**全摆出来了**，这行纯噪音。
                return Some((a.name.clone(), String::new()));
            }
        }
    }
    None
}

/// 实心三角形（3×3 超采样抗锯齿）。气泡尾巴没有现成原语可用，
/// 又不能拿 `rot_rrect` 的菱形凑 —— 那条边会和本体的描边打架，露出一段错位的线。
unsafe fn flat_tri(
    buf: &mut [u8],
    p0: (f32, f32),
    p1: (f32, f32),
    p2: (f32, f32),
    col: (u8, u8, u8),
    a: f32,
) {
    let minx = p0.0.min(p1.0).min(p2.0).floor().max(0.0) as i32;
    let maxx = p0.0.max(p1.0).max(p2.0).ceil().min(CANVAS as f32 - 1.0) as i32;
    let miny = p0.1.min(p1.1).min(p2.1).floor().max(0.0) as i32;
    let maxy = p0.1.max(p1.1).max(p2.1).ceil().min(CANVAS as f32 - 1.0) as i32;
    if minx > maxx || miny > maxy {
        return;
    }
    for y in miny..=maxy {
        for x in minx..=maxx {
            let mut hit = 0i32;
            for sy in 0..3 {
                for sx in 0..3 {
                    let px = x as f32 + (sx as f32 + 0.5) / 3.0;
                    let py = y as f32 + (sy as f32 + 0.5) / 3.0;
                    let s0 = (p1.0 - p0.0) * (py - p0.1) - (px - p0.0) * (p1.1 - p0.1);
                    let s1 = (p2.0 - p1.0) * (py - p1.1) - (px - p1.0) * (p2.1 - p1.1);
                    let s2 = (p0.0 - p2.0) * (py - p2.1) - (px - p2.0) * (p0.1 - p2.1);
                    if (s0 >= 0.0 && s1 >= 0.0 && s2 >= 0.0)
                        || (s0 <= 0.0 && s1 <= 0.0 && s2 <= 0.0)
                    {
                        hit += 1;
                    }
                }
            }
            if hit == 0 {
                continue;
            }
            let o = (y as usize * CANVAS as usize + x as usize) * 4;
            blend_over(buf, o, col.0 as f32, col.1 as f32, col.2 as f32,
                       hit as f32 / 9.0 * a);
        }
    }
}

/// 量文字宽度（GDI `DT_CALCRECT`）。气泡宽度是按文字量收缩的，
/// 用 `est_text_w` 估算偏了就会「字溢出气泡」或「气泡虚胖」—— 这里必须准。
unsafe fn text_w(st: &State, text: &str, px: i32) -> i32 {
    if st.txt_dc.0.is_null() || text.is_empty() {
        return 0;
    }
    let dc = HDC(st.txt_dc.0);
    let font = match px {
        20 => st.font_glyph,
        0..=10 => st.font_hint,
        11 => st.font_label,
        12 => st.font_cnt,
        13 => st.font_row,
        14 => st.font_head,
        21 => st.font_bub,
        22 => st.font_bub_s,
        _ => st.font_name,
    };
    let _ = SelectObject(dc, HGDIOBJ(font.0));
    let mut chars: Vec<u16> = text.encode_utf16().collect();
    let mut rect = RECT { left: 0, top: 0, right: CANVAS - 8, bottom: 40 };
    // DT_CALCRECT 只算不画，不会污染 `draw_text` 共用的那块 DC
    let _ = DrawTextW(dc, &mut chars, &mut rect, DT_SINGLELINE | DT_NOPREFIX | DT_CALCRECT);
    (rect.right - rect.left).max(0)
}

/// 诊断用（`RL_BUBDBG=1`）：当前 px 码实际选中的字体有多高。
/// GDI 会自己挑最接近的可用尺寸，**名义 px ≠ 实际 px** —— 这个项目里
/// 上面那批字体全带 `* K` 缩放，光看代码里的数字会被骗（2026-10-14 就栽在这）。
unsafe fn font_px(st: &State, px: i32) -> i32 {
    use windows::Win32::Graphics::Gdi::{GetTextMetricsW, TEXTMETRICW};
    if st.txt_dc.0.is_null() {
        return 0;
    }
    let dc = HDC(st.txt_dc.0);
    let font = match px {
        20 => st.font_glyph,
        0..=10 => st.font_hint,
        11 => st.font_label,
        12 => st.font_cnt,
        13 => st.font_row,
        14 => st.font_head,
        21 => st.font_bub,
        22 => st.font_bub_s,
        _ => st.font_name,
    };
    let _ = SelectObject(dc, HGDIOBJ(font.0));
    let mut tm = TEXTMETRICW::default();
    if GetTextMetricsW(dc, &mut tm).as_bool() {
        tm.tmHeight
    } else {
        -1
    }
}

/// 画气泡。必须在 `draw_scene` **最后**调（压在所有内容之上）。
/// 盘面内圈本来就是空的，所以它只会临时盖住 1~2 个外环胶囊。
unsafe fn draw_bubble(st: &mut State, buf: &mut [u8], pal: &Pal) {
    let (main, sub) = match bub_text(st) {
        Some(v) => v,
        None => return,
    };
    let am = st.bub_a;
    if am <= 0.01 || main.is_empty() {
        return;
    }
    let mw = text_w(st, &main, BUB_MAIN_PX) as f32;
    let sw = if sub.is_empty() { 0.0 } else { text_w(st, &sub, BUB_SUB_PX) as f32 };
    let tw = mw.max(sw).min(BUB_W_MAX - BUB_PAD * 2.0);
    let w = (tw + BUB_PAD * 2.0).max(BUB_W_MIN);
    if std::env::var("RL_BUBDBG").is_ok() {
        let (lm, ls) = unsafe {
            (font_px(st, BUB_MAIN_PX), font_px(st, BUB_SUB_PX))
        };
        eprintln!("[bub] main={:?} mw={} | sub={:?} sw={} | tw={} w={} | font身高 {} / {}",
            main, mw, sub, sw, tw, w, lm, ls);
    }
    let gap = 4.0;
    let h = BUB_PAD * 2.0 + BUB_MAIN_LH
        + if sub.is_empty() { 0.0 } else { BUB_SUB_LH + gap }
        - 2.0;
    // 底边钉死、往上长：字号变大后 h 变高，若还按「顶边固定」就会往下压到头像球上
    let y0 = BUB_Y_BOT - h;

    // 本体：与内环磁贴 / 外环胶囊同一套 token（块色底 + `mix(ink 34%)` 描边 + 硬投影）
    flat_round_rect(
        buf,
        BUB_X + w * 0.5,
        y0 + h * 0.5,
        w * 0.5,
        h * 0.5,
        BUB_R,
        pal.paper,
        1.0,
        mixc(pal.paper, pal.ink, 0.34),
        am,
        2.0,
        0.35 * am,
        2.0,
        am,
    );
    // 尾巴：指向头像球右上。画在**本体之后**，本体盖住它的上半，露出的是干净三角。
    flat_tri(
        buf,
        (BUB_X + 10.0, BUB_Y_BOT - 2.0),
        (BUB_X + 30.0, BUB_Y_BOT - 2.0),
        (BUB_X - 7.0, BUB_Y_BOT + 13.0),
        pal.paper,
        am,
    );
    // ⚠️ 文字必须画在本体**之后**：`draw_text` 会跳过 `alpha == 0` 的像素，
    //    先画字等于一个字都不出。
    let tx = (BUB_X + BUB_PAD) as i32;
    let max_w = (w - BUB_PAD * 2.0 + 1.0) as i32;
    let a8 = (255.0 * am) as u8;
    draw_text(
        st, buf, tx, (y0 + BUB_PAD + BUB_MAIN_LH * 0.5) as i32,
        &main, BUB_MAIN_PX, 400, pal.ink.0, pal.ink.1, pal.ink.2, a8, max_w, 0,
    );
    if !sub.is_empty() {
        // 副行提到 0.68 —— 0.55 在深色下太灰，读起来费劲
        let sc = mixc(pal.paper, pal.ink, 0.68);
        draw_text(
            st, buf, tx, (y0 + BUB_PAD + BUB_MAIN_LH + gap + BUB_SUB_LH * 0.5) as i32,
            &sub, BUB_SUB_PX, 400, sc.0, sc.1, sc.2, a8, max_w, 0,
        );
    }
}

/// 中心头像（GrokBot 式表情球）：**环中心和「关于」页的 logo 共用这一个函数** ——
/// 两处长得必须完全一样（用户 2026-10-16：「关于里那个 logo 改为轮盘中心的头像」）。
///
/// `buf` 必须是 `CANVAS×CANVAS` 的 BGRA 预乘缓冲：`flat_circle` / `rot_rrect` 按
/// **全局 `CANVAS`** 定宽（写死了），所以只能在那个尺寸的画布上画；调用方要小图就
/// 自己从中心裁一块缩下去。`r` = 球半径（入场 `pop`、让位缩放之类的**外部**系数由调用方乘好）。
unsafe fn draw_hub_face(st: &State, buf: &mut [u8], cx: f32, cy: f32, r: f32, pal: &Pal, now: f64) {
    let ft = (now - st.face0) as f32;
    let open = blink_open(&st.blink_q, now).clamp(0.0, 1.45);

    // ── 情绪（拖出移除）────────────────────────────────────────────
    // 参数逐条抄自 my-pet `common/ui/avatar/avatar_data.py` 的 EMOTION_SEED，
    // 单位从它的脸半径 114.27px 归一到本项目的球半径（px2r）。
    //
    //  紧张 1 = 慌张 id 17「圆睁眼环高频轮换，目光乱晃，整球细颤」
    //  生气 2 = 生气 id 21「怒目圆睁，身体细微发抖」（不涨红，见下）
    //  发呆 3 = 发呆 id 04「双眼各望各的，缓慢游移」
    //  疑惑 4 = 疑惑 id 11「头微倾，一眼放大一眼收小」
    //  忙碌 5 = 处理中忙碌 id 32「目光小幅循环往复」
    //
    // ⚠️ 抖动幅度**放大 2.5 倍**：my-pet 的脸直径 228px，本项目的球只有 87px，
    //    同样的相对位移在小球上只有 0.7px，肉眼读不出来。
    // ⚠️ my-pet 的眼形靠 23 组眼环形状切换，本项目只有药丸眼，
    //    用「整体放大 / 单眼缩放 / 倾角」近似。
    struct MoodPose {
        body_x: f32,             // 整球水平位移（R 单位，已含放大）
        gaze_x: f32,             // 双眼共享注视偏移（gaze 单位）
        gaze_y: f32,
        eye_l: (f32, f32, f32),  // 左眼 (dx R, dy R, 缩放)
        eye_r: (f32, f32, f32),  // 右眼
        tilt: f32,               // 双眼整体倾角（弧度，「头微倾」的近似）
        esc: f32,                // 双眼整体放大
        breathe: f32,            // 呼吸幅度（my-pet body.breathe 原值）
    }
    impl MoodPose {
        fn calm() -> Self {
            MoodPose {
                body_x: 0.0, gaze_x: 0.0, gaze_y: 0.0,
                eye_l: (0.0, 0.0, 1.0), eye_r: (0.0, 0.0, 1.0),
                tilt: 0.0, esc: 1.0, breathe: 0.024,
            }
        }
    }
    const PET_FACE_R: f64 = 114.27;
    const JIT_AMP: f64 = 2.5;
    let px2r = |v: f64| (v / PET_FACE_R * JIT_AMP) as f32;
    let tau = std::f64::consts::TAU;
    let mk = st.mood_k.min(1.0);
    let mt = (now - st.mood_t) as f64 * 1000.0;
    let pose = match st.mood {
        1 => MoodPose {
            body_x: jitter(mt, 7.0, px2r(1.9464)),
            gaze_x: jitter(mt + 37.0, 10.0, px2r(6.0)),
            gaze_y: jitter(mt + 91.0, 11.0, px2r(3.0)),
            esc: 1.22,
            breathe: 0.0057,
            ..MoodPose::calm()
        },
        2 => MoodPose {
            body_x: jitter(mt + 53.0, 6.0, px2r(1.0467)),
            gaze_y: 0.30,   // 怒目：目光下压
            // 脸色涨红会打破项目的「全中性灰阶」铁律，所以**默认不涨红**，
            // 只用眼睛放大 + 目光下压表达怒意。要涨红改这里即可（#E4574A）。
            esc: 1.18,
            breathe: 0.0032,
            ..MoodPose::calm()
        },
        3 => MoodPose {
            // 发呆 04：左眼望左下、右眼望右上，各想各的，再叠一层极慢的游移
            gaze_x: px2r(3.0) * ((mt / 5614.0 * tau).sin()) as f32,
            eye_l: (px2r(-4.0), px2r(2.0), 1.0),
            eye_r: (px2r(5.0), px2r(-1.0), 1.0),
            tilt: (-3.0f32).to_radians(),
            breathe: 0.0103,
            ..MoodPose::calm()
        },
        4 => MoodPose {
            // 疑惑 11：左眼抬高放大、右眼放低收小往一边瞟，头微倾 + 轻晃
            eye_l: (0.0, px2r(-5.0), 1.087),
            eye_r: (px2r(3.0), px2r(3.0), 0.881),
            tilt: (-6.0f32).to_radians()
                + (1.74f32 * ((mt / 3392.0 * tau).sin()) as f32).to_radians(),
            breathe: 0.0078,
            ..MoodPose::calm()
        },
        5 => MoodPose {
            // 忙碌 32：目光小幅循环往复（lookX amp 6 / lookY amp 5，两轴不同频）
            gaze_x: px2r(6.0) * ((mt / 1495.0 * tau).sin()) as f32,
            gaze_y: px2r(5.0) * ((mt / 1058.0 * tau + 4.578).sin()) as f32,
            breathe: 0.0078,
            ..MoodPose::calm()
        },
        _ => MoodPose::calm(),
    };
    let m_bx = pose.body_x * mk;
    let esc = 1.0 + (pose.esc - 1.0) * mk;
    let br_amp = pose.breathe;
    let br = (ft / 3.2 * std::f32::consts::TAU).sin() * br_amp;
    // 取消按钮出现时头像缩 12% 让位（与按钮同步淡入，见 3b / rm_btn_center）
    let fr = r * (1.0 + br) * (1.0 - RM_BALL_SHRINK * st.cancel_a.clamp(0.0, 1.0));

    // 注视 + 常驻微漂移（永不完全静止）
    let gx = ((st.gaze[0] + 0.05 * ((0.42 * ft).sin() + 0.35 * (1.0 * ft).sin()))
        + pose.gaze_x * 0.55 * mk)
        .clamp(-1.25, 1.25);
    let gy = ((st.gaze[1] + 0.04 * (0.58 * ft).sin()) + pose.gaze_y * 0.55 * mk)
        .clamp(-1.25, 1.25);
    // 整球随注视轻微位移（参考实现里 body 也有 x/y 偏移）
    let bcx = cx + gx * r * 0.030;
    let bcy = cy + gy * r * 0.030;

    // ① 球体：扁平实心（无渐变、无描边）+ 向下偏移的柔和投影
    flat_circle(buf, bcx + m_bx * fr, bcy, fr, pal.block, 0.0, pal.block, 0.0,
        2.6 * K, 0.16, 9.0 * K, 1.0);

    // ② 眼睛：药丸，贴着球面滑动
    //    参考的定位模型：x = R_ORB·sin(经度)，透视 = cos(经度)/cos(基准经度)
    const R_ORB: f32 = 0.92;    // 眼环轨道半径（相对球半径）
    const EYE_DX: f32 = 0.266;  // 眼睛基准横向偏移（相对球半径）
    const EYE_W: f32 = 0.156;   // 眼睛半宽
    const EYE_H: f32 = 0.276;   // 眼睛半高（全睁）
    const EYE_DY: f32 = -0.055; // 基准纵向偏移（略高于球心）
    let lon0 = (EYE_DX / R_ORB).clamp(-1.0, 1.0).asin();
    let turn = gx * 0.30;       // 眼球绕球心转过的弧度
    for side in [-1.0f32, 1.0f32] {
        // 发呆/疑惑需要两只眼「各动各的」：左眼 side=-1，右眼 side=+1
        let (edx, edy, escl) = if side < 0.0 { pose.eye_l } else { pose.eye_r };
        let lon = side * lon0 + turn;
        let persp = (lon.cos() / lon0.cos()).clamp(0.55, 1.50);
        let ex = bcx + m_bx * fr + R_ORB * lon.sin() * fr + edx * fr * mk;
        let ey = bcy + (EYE_DY + gy * 0.20) * fr + edy * fr * mk;
        let escl = 1.0 + (escl - 1.0) * mk;
        let ehw = EYE_W * fr * persp * esc * escl;
        let ehh = EYE_H * fr * open * esc * escl;
        if ehh <= 0.7 {
            continue;
        }
        // 圆角＝半宽 → 胶囊；倾斜角跟随注视方向 + 情绪的「头微倾」
        rot_rrect(buf, ex, ey, ehw, ehh, ehw.min(ehh), gx * 0.30 + pose.tilt * mk,
            pal.paper, 1.0);
    }
}

// ═══════════════════════════════════════════════════════════════════
//  液态玻璃（材质 = 玻璃；用户 2026-10-17 定的两件事：**淡**、**只做轮盘**）
// ═══════════════════════════════════════════════════════════════════
//
// 浏览器一条 `backdrop-filter` 就搞定的事，桌面程序得自己来：
//   ① 菜单弹出**之前**抓一次屏幕（此时窗口已 SW_HIDE）→
//   ② 缩到 1/4、三次盒式模糊、双线性放大 →
//   ③ 按**盘的形状**合成（盘外保持透明，ULW 才不会露出一块方形毛玻璃）。
//
// 参考实现（D:/工作/IDEA/chrome_starter）里最有价值的三条，这里都照做：
//   · 玻璃底色是**深色中性色**，不是白色 —— 拉高浓度 = 更深更实，不是越拉越白；
//   · 模糊**低**（不是糊成一片），靠 saturate 把背景颜色提鲜；
//   · "有厚度"的观感几乎全在**边缘 inset 高光**上（上 0.55 / 左右 0.18 / 下 0.13）。
//
// ⚠️ 背景是**快照**：菜单弹出后桌面变了，玻璃里不会跟着变（macOS 也这样）。
// ⚠️ 受保护窗口（DRM 视频 / 部分银行客户端）抓出来是黑块 → `snap_glass` 判失败，
//    自动退回纸面（`glass_bg` 空 = 非玻璃），绝不出现"一块黑玻璃"。

/// 玻璃浓度（**更透**版，用户 2026-10-17 二轮："太浓了，要再透一些"）。
/// 深色主题 tint 深灰、浅色主题 tint 近白。
/// ⚠️ 这俩是「玻璃有多实」的唯一旋钮：调小 = 更透（背景更清楚），调大 = 更像磨砂。
///    透下来的"颜色感"由 `GLASS_SAT` 补，不靠 tint 堆。
const GLASS_TINT_A_D: f32 = 0.08;
const GLASS_TINT_A_L: f32 = 0.05;
/// 背景提鲜倍数：tint 越淡，越需要它把背景的颜色"举"起来，否则盘面发闷。
const GLASS_SAT: f32 = 2.0;
/// 抓屏后缩到 1/2（**不是** 1/4：1/4 光降采样就已经糊掉一半细节了）
const GLASS_DN: usize = 2;
/// 1/2 尺度上的盒式模糊半径（三次 → 放大后约 3~4px，字还认得出来）
const GLASS_BR: usize = 1;
/// 外环胶囊的浓度（**比盘实**）：胶囊里装着文字，透出亮背景时白字会看不清。
/// 盘是"容器"可以近乎隐形，胶囊是"内容块"，必须保证可读 ——
/// 这也是 iOS 玻璃的分层规矩（vibrancy 靠一层更实的底）。
const GLASS_CHIP_A_D: f32 = 0.62;
const GLASS_CHIP_A_L: f32 = 0.66;
/// 胶囊的边缘高光强度（0..1）。胶囊只有 30px 厚，**满强度的高光会把整块
/// 62% 深灰洗成浅灰**（`GLASS_RIM=8` 的渐变带几乎盖满整个胶囊）——
/// 这才是"胶囊看着还是透的、字看不清"的真凶，光调 tint 治不好。
const GLASS_CHIP_RIM: f32 = 0.30;
/// 模糊层在最终颜色里的占比，剩下的是**原图**。
/// 这一条就是「液态玻璃」和「毛玻璃」的分界线：磨砂 = 100% 模糊，
/// 玻璃 = "软化的原图" —— 背景的字、颗粒、边缘都该还认得出来。
const GLASS_SHARP: f32 = 0.70;
/// 边缘高光的厚度（逻辑 px）—— 太厚就变"发光圈"，玻璃的边是很利的一道光
const GLASS_RIM: f32 = 6.0;
/// 边缘折射的最大外拉距离（px）：靠近盘沿时采样点往外偏这么多。
/// 这是"液态玻璃"和"毛玻璃"的分界线 —— 只有模糊 = 磨砂，有折射才像一块玻璃。
const GLASS_REFRACT: f32 = 13.0;
/// 「自擦」模式（WDA 设不上）下盘面的清晰度权重。
/// 那种模式下盘里的"原图"层是拿外圈均色填出来的假背景，留 30% 只会把分箱的
/// 扇形条纹看得清清楚楚 → 几乎全用模糊层，读成磨砂玻璃。
const GLASS_SHARP_MASK: f32 = 0.97;

/// `st.glass_bg` 每被换掉一次就 +1 —— 玻璃盘面的色层缓存拿它当失效键。
static GLASS_BG_REV: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// 写 `st.glass_bg` 的**唯一**入口：换了背景快照就必须让盘缓存作废，
/// 否则滚动时会一直显示上一帧的玻璃内容。
fn set_glass_bg(st: &mut State, v: Vec<u8>) {
    st.glass_bg = v;
    GLASS_BG_REV.fetch_add(1, Ordering::Relaxed);
}

// ───────────────────── 玻璃降级诊断 ─────────────────────
//
// 玻璃有好几条「静默退回纸面」的路：抓屏失败、抓出来全黑（DRM/受保护窗口）、
// 系统设不上 WDA_EXCLUDEFROMCAPTURE。以前一个字都不写，用户只能报「我这台不实时」，
// 日志里查不到任何线索。现在每个原因**一次运行只记一行**（实时线程 30fps，
// 去重是必须的），并且带上坐标和屏幕尺寸 —— 公司电脑发回来的日志能自己把话说完。

const GL_WDA: u32 = 1 << 0;     // WDA_EXCLUDEFROMCAPTURE 设不上 → 改走「自遮罩」实时
const GL_LIVE_ON: u32 = 1 << 1; // 实时线程跑起来了（记一条，证明这条路是通的）
const GL_SNAP_BLACK: u32 = 1 << 2;
const GL_SNAP_CAP: u32 = 1 << 3;  // 呼出时那次抓屏失败（GetDC / BitBlt）
const GL_LIVE_BLACK: u32 = 1 << 4;
const GL_LIVE_CAP: u32 = 1 << 5;
const GL_OFFSCREEN: u32 = 1 << 6; // 抓取矩形整个落在虚拟屏幕之外

static GLASS_LOGGED: AtomicU32 = AtomicU32::new(0);

/// 同一原因只写一行；`msg` 是闭包，被去重挡掉时连字符串都不拼。
fn glass_note(bit: u32, msg: impl FnOnce() -> String) {
    if GLASS_LOGGED.fetch_or(bit, Ordering::Relaxed) & bit != 0 {
        return;
    }
    diag_line(&format!("{}\\radial_log.txt", log_dir()), &msg());
}

/// 抓屏坐标 + 屏幕范围（一眼看出是不是「副屏 / 负坐标 / 越界」这类问题）。
fn glass_where(x: i32, y: i32) -> String {
    let (vx, vy, vw, vh) = virtual_screen();
    format!(
        "at=({},{}) size={}x{} virtual=({},{},{}x{})",
        x, y, CANVAS, CANVAS, vx, vy, vw, vh
    )
}

fn virtual_screen() -> (i32, i32, i32, i32) {
    unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        )
    }
}

/// 抓取矩形是否整个落在虚拟屏幕外（只压掉一半不算 —— 剩下还有半张桌面，
/// `mostly_black` 会放过，视觉上也就是盘沿一部分糊掉）。
fn glass_offscreen(x: i32, y: i32) -> bool {
    let (vx, vy, vw, vh) = virtual_screen();
    x + CANVAS <= vx || y + CANVAS <= vy || x >= vx + vw || y >= vy + vh
}

/// WDA 设不上时的兜底：把抓到的那一帧里「轮盘自己」抹掉。
///
/// 做法是**角度扩散**，不是径向外推 —— 外推实测会把壁纸拉成一个风车（盘心是奇点，
/// 深色区域被拉成扇形黑块），比不擦还扎眼。这里：
/// 1. 贴着遮罩外沿取一圈干净桌面，按角度分箱求平均色（96 箱，循环平滑一遍）；
/// 2. 盘内每个像素 = 全局均值 → 该角度均值 的径向过渡，越靠盘沿越接近真实周边；
/// 3. 于是盘读起来是「背景的柔和延续」，再过一道 `blur_glass` 就是磨砂玻璃。
/// 不如 WDA（看不见背景的细节），但玻璃是活的、没有自己的残影，也不会露馅成一块白。
fn glass_scrub_self(raw: &mut [u8], rm: f32) {
    const NB: usize = 96;
    /// 取样的环带宽度：贴近遮罩才像「背景的延续」，取远了颜色就跑偏
    const BAND: f32 = 40.0;
    let w = CANVAS as usize;
    let hi_edge = CC - 3.0;                 // 画布内切圆：再往外就取不到干净像素
    let lo = (rm + GL_MASK_PAD).min(hi_edge);
    let hi = (lo + BAND).min(hi_edge);
    if hi - lo < 6.0 {
        return;                             // 遮罩顶到画布边了，没什么可取 → 留着上一帧
    }
    let mut acc = [[0f32; 3]; NB];
    let mut cnt = [0f32; NB];
    let mut gsum = [0f32; 3];
    let mut gn = 0f32;
    let r_out = hi + 1.0;
    let y0 = (CC - r_out).max(0.0) as usize;
    let y1 = ((CC + r_out).min(CANVAS as f32 - 1.0)) as usize;
    for py in y0..=y1 {
        let dy = py as f32 + 0.5 - CC;
        let sx = (r_out * r_out - dy * dy).sqrt().max(0.0);
        let xa = (CC - sx).max(0.0) as usize;
        let xb = ((CC + sx).min(CANVAS as f32 - 1.0)) as usize;
        for px in xa..=xb {
            let dx = px as f32 + 0.5 - CC;
            let d = (dx * dx + dy * dy).sqrt();
            if d < lo || d > hi {
                continue;
            }
            let o = (py * w + px) * 4;
            let b = ang_bin(dx, dy);
            for c in 0..3 {
                let v = raw[o + c] as f32;
                acc[b][c] += v;
                gsum[c] += v;
            }
            cnt[b] += 1.0;
            gn += 1.0;
        }
    }
    if gn < 1.0 {
        return;
    }
    let gm = [gsum[0] / gn, gsum[1] / gn, gsum[2] / gn];
    // 每箱均值（空箱 = 那一向被画布边切掉了）→ 退回全局均值
    let mut prof = [[0f32; 3]; NB];
    for i in 0..NB {
        if cnt[i] > 0.5 {
            for c in 0..3 {
                prof[i][c] = acc[i][c] / cnt[i];
            }
        } else {
            prof[i] = gm;
        }
    }
    // 循环 3 抽头平滑：分箱之间不留硬边
    let mut sm = prof;
    for i in 0..NB {
        let (a, b) = (prof[(i + NB - 1) % NB], prof[(i + 1) % NB]);
        for c in 0..3 {
            sm[i][c] = (a[c] + prof[i][c] * 2.0 + b[c]) * 0.25;
        }
    }
    // 填盘内
    let y0 = (CC - rm).max(0.0) as usize;
    let y1 = ((CC + rm).min(CANVAS as f32 - 1.0)) as usize;
    for py in y0..=y1 {
        let dy = py as f32 + 0.5 - CC;
        let sx = (rm * rm - dy * dy).sqrt().max(0.0);
        let xa = (CC - sx).max(0.0) as usize;
        let xb = ((CC + sx).min(CANVAS as f32 - 1.0)) as usize;
        for px in xa..=xb {
            let dx = px as f32 + 0.5 - CC;
            let d = (dx * dx + dy * dy).sqrt();
            let o = (py * w + px) * 4;
            // 越靠盘心越接近全局均值（幂次 >1 让中心那块"糊"得更开，奇点不显形）
            let u = d / rm;
            let t = u * u * u.sqrt();
            let (i0, i1, fr) = ang_lerp(dx, dy);
            let (p0, p1) = (sm[i0], sm[i1]);
            for c in 0..3 {
                let v = gm[c] * (1.0 - t) + (p0[c] * (1.0 - fr) + p1[c] * fr) * t;
                raw[o + c] = v.clamp(0.0, 255.0) as u8;
            }
        }
    }
}

/// 方向 → 角度箱（0..NB）。约定与 `glass_px` 无关，只要自洽即可。
fn ang_bin(dx: f32, dy: f32) -> usize {
    const NB: usize = 96;
    let th = dy.atan2(dx);                       // -π..π
    let b = (th + std::f32::consts::PI) * (NB as f32 / (2.0 * std::f32::consts::PI));
    (b as usize) % NB
}

/// 方向 → 相邻两箱 + 插值系数（避免 96 箱的分段感）
fn ang_lerp(dx: f32, dy: f32) -> (usize, usize, f32) {
    const NB: usize = 96;
    let th = dy.atan2(dx);
    let pos = (th + std::f32::consts::PI) * (NB as f32 / (2.0 * std::f32::consts::PI));
    let f = pos.floor();
    let i0 = (f as usize) % NB;
    (i0, (i0 + 1) % NB, pos - f)
}

/// 抓桌面一块区域（`CANVAS²`，屏幕坐标）→ 模糊 → `st.glass_bg`。
///
/// ⚠️ 调用时机**必须**是「窗口已经 `SW_HIDE`、且 `DwmFlush()` 已等过一帧」——
/// 否则抓到自己上一次的残影，玻璃里就套着一圈自己的盘。
/// 失败（GDI 报错 / 抓出来 почти全黑 = 受保护窗口）→ 留空，调用方退回纸面。
unsafe fn snap_glass(st: &mut State, x: i32, y: i32) {
    set_glass_bg(st, Vec::new());
    if !st.glass {
        return;
    }
    let n = (CANVAS * CANVAS * 4) as usize;
    // 直接用 `make_dib`：它建的是 32bpp 顶朝下的 DIB，`bits` 就是像素，
    // 比 `CreateCompatibleBitmap` + `GetDIBits` 少一次拷贝、也躲开"选进 DC 时不能 GetDIBits"的坑。
    let (bits, mdc, bmp) = make_dib(CANVAS, CANVAS);
    if bits.is_null() || mdc.is_invalid() || bmp.is_invalid() {
        return;
    }
    let sdc = GetDC(None);
    let mut ok = false;
    if !sdc.is_invalid() {
        // CAPTUREBLT：带上分层窗口（否则某些叠层 / 播放器会抓成空洞）
        if BitBlt(mdc, 0, 0, CANVAS, CANVAS, sdc, x, y, SRCCOPY | CAPTUREBLT).is_ok() {
            ok = true;
        }
        let _ = ReleaseDC(None, sdc);
    }
    let mut raw = Vec::new();
    if ok {
        raw.extend_from_slice(std::slice::from_raw_parts(bits, n));
    }
    let _ = DeleteObject(HGDIOBJ(bmp.0));
    let _ = DeleteDC(mdc);
    if !ok {
        glass_note(GL_SNAP_CAP, || {
            format!("GLASS reason=capfail snap（呼出时那次抓屏就失败 → 第一帧是纸面）{}", glass_where(x, y))
        });
        return;
    }
    if glass_offscreen(x, y) {
        glass_note(GL_OFFSCREEN, || {
            format!("GLASS reason=offscreen（抓取矩形整个在虚拟屏幕外）{}", glass_where(x, y))
        });
    }
    if mostly_black(&raw) {
        glass_note(GL_SNAP_BLACK, || {
            format!("GLASS reason=black snap（抓出来全黑 = DRM/受保护窗口 → 退回纸面）{}", glass_where(x, y))
        });
        return;
    }
    let t0 = std::time::Instant::now();
    set_glass_bg(st, build_glass(&raw));
    // 只在需要时看：抓屏 + 模糊一共多久（菜单是"按下就出"，这段延迟用户能感觉到）
    if std::env::var_os("RL_GLASSDBG").is_some() {
        eprintln!("[glass] snap {}x{} at ({},{}) blur+blit = {:?}", CANVAS, CANVAS, x, y, t0.elapsed());
    }
}

/// 全黑（平均亮度 ≈ 0）→ 判失败。受保护窗口（DRM 视频 / 银行客户端）就这样，
/// 与其显示一块黑玻璃，不如老老实实回到纸面 / 上一帧。
fn mostly_black(raw: &[u8]) -> bool {
    let mut sum = 0u64;
    let mut cnt = 0u64;
    for i in (0..raw.len().saturating_sub(3)).step_by(4 * 61) {
        sum += (raw[i] as u64 + raw[i + 1] as u64 + raw[i + 2] as u64) / 3;
        cnt += 1;
    }
    cnt == 0 || sum / cnt < 4
}

/// 一块 Vec 存两层：原图在前、模糊图在后（`glass_px` 要同时采样两层）
fn build_glass(raw: &[u8]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(raw.len() * 2);
    buf.extend_from_slice(raw);
    buf.extend_from_slice(&blur_glass(raw));
    buf
}

/// 调试用：把抓到的 BGRA 写成 PPM（`RL_GLASSDUMP=<前缀>` 时才调）。
fn dump_ppm(path: &str, w: usize, h: usize, bgra: &[u8]) {
    let mut out = Vec::with_capacity(w * h * 3 + 32);
    out.extend_from_slice(format!("P6\n{} {}\n255\n", w, h).as_bytes());
    for px in bgra.chunks_exact(4) {
        out.extend_from_slice(&[px[2], px[1], px[0]]);
    }
    let _ = std::fs::write(path, out);
}

// ───────────────────── 实时玻璃 ─────────────────────
//
// Windows 没有给「自绘的圆形窗口」任何实时背景 API：DWM 的亚克力/云母只对矩形
// 窗口生效，`DwmEnableBlurBehindWindow` 在 Win8+ 已经不模糊了。所以只能在菜单
// 显示期间，隔一会儿把桌面那一块重抓一次。
//
// 难点是「窗口正显示着」：直接抓会把自己拍进去（实测 Win11 上不带 CAPTUREBLT
// 也照样拍到分层窗口）。解法是 `WDA_EXCLUDEFROMCAPTURE` —— 把轮盘窗口标记成
// **不参与抓屏**，GDI 的 BitBlt 就拍不到它了（带 CAPTUREBLT 也拍不到）。
// ⚠️ 代价：轮盘打开时**录屏 / 截图里也看不见它**。要录宣传片就把 ini 的
//    `glass_live=0`（录完再打开）。
//
// ⚠️⚠️ 抓屏必须放**后台线程**：屏幕 BitBlt 每次都会等到 DWM 下一次合成
//    （≈16ms 阻塞，真正 CPU 只有 0.5ms @475²）。挂在 UI 的 16ms 帧定时器上
//    会把菜单拖到 30fps 以下。

/// 重抓间隔（ms）。30fps 足够让玻璃里的视频/滚动"活着"；再快只会多等几次合成。
const GLASS_LIVE_MS: u64 = 33;

static GLASS_LIVE_ON: AtomicBool = AtomicBool::new(false);
static GLASS_LIVE_X: AtomicI32 = AtomicI32::new(0);
static GLASS_LIVE_Y: AtomicI32 = AtomicI32::new(0);
/// WDA 设不上（老系统 / 远程桌面 / 受管控的机器）→ 每帧自己把盘那一块从抓到的画面里
/// 擦掉（`glass_scrub_self`）。以前是直接放弃实时，玻璃就整个不动了。
static GLASS_SELF_MASK: AtomicBool = AtomicBool::new(false);
/// 要擦掉的半径（**×10 定点**，画布坐标）。UI 线程每帧按「这一帧实际画到哪」更新；
/// 0 = 还没画过，用 `GL_MASK_R_FALLBACK` 兜底。
static GLASS_MASK_R: AtomicU32 = AtomicU32::new(0);
/// 遮罩半径之外还要再让出这么多，才当作「干净的桌面」来取样 ——
/// 遮罩本身已经把盘/外环的投影算进去了（`GL_MASK_SHAD`）。
const GL_MASK_PAD: f32 = 6.0;
/// 盘和外环都带一圈柔和投影（下移 6K + 模糊 18K，注释里实测最远 r+30）。
/// 遮罩半径 = 实际画到哪 + 这一圈投影，否则擦完还留一圈自己的影子。
const GL_MASK_SHAD: f32 = 26.0;
/// UI 还没报过半径时的兜底 = 最坏绘制范围（盘 + 外环 + hover 抬升）。
const GL_MASK_R_FALLBACK: f32 = DISC_R_MAX + 3.0 * K + RING_T + 2.0 * K + GL_MASK_SHAD;
/// UI 线程取走了多少帧（只给 `RL_GLASSDBG` 的日志用）
static GLASS_LIVE_TAKEN: AtomicU32 = AtomicU32::new(0);
/// **外部窗口正在被拖向环**（窗口捕获武装中）→ 暂停抓屏、冻结盘面色层。
/// 理由同「滚动中故意不换帧」（见 `on_timer`）：这时候背景本来就没人看，
/// 而每 33ms 一次的全屏 `BitBlt(CAPTUREBLT)` 会和 DWM 的合成抢带宽 ——
/// 用户正拖着浏览器窗口，那一边最需要的就是合成不被抢（2026-10-26「拖浏览器巨卡」）。
static GLASS_FREEZE: AtomicBool = AtomicBool::new(false);

/// 抓屏内容的粗签名：每 16px 采一个点（≈30×30）。
/// 桌面没变 → 签名相同 → 不 blur、不入槽 → UI 不 bump `GLASS_BG_REV` →
/// 省掉每帧固定 7ms 的盘内重采（空闲时的主要税）。
fn glass_sig(raw: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let step = 16usize;
    let w = CANVAS as usize;
    let mut y = step / 2;
    while y < w {
        let mut x = step / 2;
        while x < w {
            let o = (y * w + x) * 4;
            if o + 3 < raw.len() {
                let p = u32::from_le_bytes([raw[o], raw[o + 1], raw[o + 2], raw[o + 3]]);
                h = (h ^ p as u64).wrapping_mul(0x0000_0100_0000_01b3);
            }
            x += step;
        }
        y += step;
    }
    h
}

/// 最新一帧（UI 线程 `take` 走，直接换进 `glass_bg`，不拷贝）
static GLASS_LIVE_SLOT: std::sync::Mutex<Option<Vec<u8>>> = std::sync::Mutex::new(None);
/// 菜单关着时把线程**挂起**（不是 12ms 轮询）：空闲时 0 唤醒、0 CPU，
/// 也不妨碍系统进低功耗状态。
static GLASS_LIVE_PARK: std::sync::Mutex<()> = std::sync::Mutex::new(());
static GLASS_LIVE_WAKE: std::sync::Condvar = std::sync::Condvar::new();

/// 后台抓屏线程。菜单关着时挂在条件变量上，不占 CPU、不占显存。
fn glass_live_spawn() {
    std::thread::spawn(|| unsafe {
        let n = (CANVAS * CANVAS * 4) as usize;
        let dbg = std::env::var_os("RL_GLASSDBG").is_some();
        let dump = std::env::var("RL_GLASSDUMP").ok();
        let mut dump_i = 0u32;
        let mut dib: Option<(*mut u8, HDC, HBITMAP)> = None;
        let mut on_since: Option<Instant> = None;
        let mut sig_last: Option<u64> = None;
        let (mut stat_n, mut stat_blit_us, mut stat_blur_us) = (0u32, 0u64, 0u64);
        loop {
            if !GLASS_LIVE_ON.load(Ordering::Relaxed) {
                if let Some((_, mdc, bmp)) = dib.take() {
                    let _ = DeleteObject(HGDIOBJ(bmp.0));
                    let _ = DeleteDC(mdc);
                }
                on_since = None;
                // 挂起。锁里复查一次：`glass_live_start` 也是在锁里置位的，不会漏唤醒。
                let guard = GLASS_LIVE_PARK.lock().unwrap_or_else(|e| e.into_inner());
                if !GLASS_LIVE_ON.load(Ordering::Relaxed) {
                    let _ = GLASS_LIVE_WAKE.wait_timeout(guard, std::time::Duration::from_millis(500));
                }
                continue;
            }
            if on_since.is_none() {
                on_since = Some(Instant::now());
                dump_i = 0;
                sig_last = None;
                if let Ok(mut s) = GLASS_LIVE_SLOT.lock() {
                    *s = None;
                }
            }
            if dib.is_none() {
                let (bits, mdc, bmp) = make_dib(CANVAS, CANVAS);
                if bits.is_null() || mdc.is_invalid() || bmp.is_invalid() {
                    GLASS_LIVE_ON.store(false, Ordering::Relaxed);
                    std::thread::sleep(std::time::Duration::from_millis(200));
                    continue;
                }
                dib = Some((bits, mdc, bmp));
            }
            // 有人正把窗口拖向环 → 这一拍不抓屏（把 DWM 合成让给被拖的窗口），
            // 也不推新帧 → 盘面色层冻在最后一帧。见 `GLASS_FREEZE`。
            if GLASS_FREEZE.load(Ordering::Relaxed) {
                sig_last = None;
                std::thread::sleep(std::time::Duration::from_millis(GLASS_LIVE_MS));
                continue;
            }
            let t_tick = Instant::now();
            // WDA 生效要过一帧合成：刚打开的头 40ms 不抓，免得把轮盘自己拍进去
            if on_since.map(|t| t.elapsed().as_millis() as u64 >= 40).unwrap_or(false) {
                let (bits, mdc, _) = dib.unwrap();
                let x = GLASS_LIVE_X.load(Ordering::Relaxed);
                let y = GLASS_LIVE_Y.load(Ordering::Relaxed);
                let sdc = GetDC(None);
                if !sdc.is_invalid() {
                    let t0 = Instant::now();
                    if BitBlt(mdc, 0, 0, CANVAS, CANVAS, sdc, x, y, SRCCOPY | CAPTUREBLT).is_ok() {
                        // ⚠️ 这一段包含「等 DWM 下一次合成」（≈16ms），不是 CPU 耗时
                        let t1 = Instant::now();
                        let raw = std::slice::from_raw_parts_mut(bits, n);
                        if GLASS_SELF_MASK.load(Ordering::Relaxed) {
                            // WDA 没生效 → 把「自己」从这一帧里擦掉，否则玻璃里套一圈盘
                            let rm = match GLASS_MASK_R.load(Ordering::Relaxed) {
                                0 => GL_MASK_R_FALLBACK,
                                t => t as f32 / 10.0,
                            };
                            glass_scrub_self(raw, rm);
                        }
                        // 抓失败（受保护窗口全黑）→ 不发，UI 留着上一帧
                        // 内容没变（粗签名相同）→ 也不发：省掉 blur，更省掉 UI 侧
                        // 每次换帧固定 7ms 的盘内重采 —— 空闲时最大的那笔税。
                        if !mostly_black(raw) && {
                            let sig = glass_sig(raw);
                            let changed = sig_last != Some(sig);
                            sig_last = Some(sig);
                            changed
                        } {
                            let buf = build_glass(raw);
                            let t2 = Instant::now();
                            if let Ok(mut s) = GLASS_LIVE_SLOT.lock() {
                                *s = Some(buf);
                            }
                            dump_i += 1;
                            if let Some(p) = &dump {
                                if matches!(dump_i, 1 | 13 | 25 | 37) {
                                    dump_ppm(&format!("{}-{:02}.ppm", p, dump_i),
                                             CANVAS as usize, CANVAS as usize, raw);
                                }
                            }
                            stat_n += 1;
                            stat_blit_us += t1.duration_since(t0).as_micros() as u64;
                            stat_blur_us += t2.duration_since(t1).as_micros() as u64;
                            if dbg && stat_n >= 30 {
                                eprintln!("[glass] live blit {}us blur {}us / 30 frames, ui took {}",
                                          stat_blit_us / 30, stat_blur_us / 30,
                                          GLASS_LIVE_TAKEN.load(Ordering::Relaxed));
                                stat_n = 0;
                                stat_blit_us = 0;
                                stat_blur_us = 0;
                            }
                        } else {
                            glass_note(GL_LIVE_BLACK, || {
                                format!("GLASS reason=black live（抓出来全黑 = DRM/受保护窗口，玻璃退回上一帧）{}",
                                         glass_where(x, y))
                            });
                        }
                    } else {
                        glass_note(GL_LIVE_CAP, || {
                            format!("GLASS reason=capfail live（BitBlt 抓屏失败，玻璃不实时）{}",
                                    glass_where(x, y))
                        });
                    }
                    let _ = ReleaseDC(None, sdc);
                } else {
                    glass_note(GL_LIVE_CAP, || "GLASS reason=capfail live（GetDC(NULL) 拿不到屏幕 DC）".to_string());
                }
            }
            // 补足到 33ms 一拍。BitBlt 本身就阻塞了 ~16ms，别在这上面再加 33
            let el = t_tick.elapsed().as_millis() as u64;
            if el < GLASS_LIVE_MS {
                std::thread::sleep(std::time::Duration::from_millis(GLASS_LIVE_MS - el));
            }
        }
    });
}

unsafe fn glass_live_start(st: &State, hwnd: HWND) {
    if !st.glass || !st.glass_live || hwnd.is_invalid() {
        return;
    }
    // 轮盘窗口不参与抓屏 —— 实时抓背景时就不会把自己拍进去。
    // ⚠️ 老系统（< Win10 2004）、远程桌面、被 DLP 管控的机器设不上这个属性。
    //    以前设失败就整个关掉实时（用户看到的「玻璃不动」大半是这个），现在退一步：
    //    照常抓，每帧把「自己画的那一圈」用外圈均色填掉（`glass_scrub_self`）。
    //    不如 WDA 干净（看不见背景细节），但玻璃至少是活的。
    let wda_ok = SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE).is_ok()
        && std::env::var_os("RL_GLASS_NOWDA").is_none();
    GLASS_SELF_MASK.store(!wda_ok, Ordering::Relaxed);
    if wda_ok {
        glass_note(GL_LIVE_ON, || "GLASS live=on（WDA 排除自己，抓屏干净）".to_string());
    } else {
        glass_note(GL_WDA, || {
            "GLASS reason=wda（SetWindowDisplayAffinity 设不上 → 每帧自擦盘区域，玻璃仍然实时）"
                .to_string()
        });
    }
    GLASS_LIVE_X.store(st.win_x, Ordering::Relaxed);
    GLASS_LIVE_Y.store(st.win_y, Ordering::Relaxed);
    // 上一轮的半径别带到这一轮（第一帧没画过就用兜底值）
    GLASS_MASK_R.store(0, Ordering::Relaxed);
    // 别把上一轮（可能已关菜单）的残留帧喂给这一次
    if let Ok(mut s) = GLASS_LIVE_SLOT.lock() {
        *s = None;
    }
    {
        // 置位必须在 park 锁里：后台线程是「拿锁 → 复查标志 → 挂起」的顺序
        let _g = GLASS_LIVE_PARK.lock().unwrap_or_else(|e| e.into_inner());
        GLASS_LIVE_ON.store(true, Ordering::SeqCst);
    }
    GLASS_LIVE_WAKE.notify_all();
}

unsafe fn glass_live_stop(hwnd: HWND) {
    GLASS_LIVE_ON.store(false, Ordering::SeqCst);
    if let Ok(mut s) = GLASS_LIVE_SLOT.lock() {
        *s = None;
    }
    // 撤掉「排除抓屏」：不然用户之后的录屏/截图里，轮盘这个窗口的位置一直是个洞
    if !hwnd.is_invalid() {
        let _ = SetWindowDisplayAffinity(hwnd, WDA_NONE);
    }
}

/// 抓屏 → 缩 1/4 → 三次盒式模糊 → 双线性放大回 `CANVAS²`。
///
/// 输出是**非预乘** BGRA（与 `flat_*` 的输入约定一致），直接喂 `glass_px`。
fn blur_glass(src: &[u8]) -> Vec<u8> {
    let cw = CANVAS as usize;
    let dw = cw / GLASS_DN;
    let mut small: Vec<[f32; 3]> = vec![[0.0; 3]; dw * dw];
    // ① 降采样（盒式均值）
    for dy in 0..dw {
        for dx in 0..dw {
            let mut acc = [0f32; 3];
            for y in 0..GLASS_DN {
                let row = (dy * GLASS_DN + y) * cw;
                for x in 0..GLASS_DN {
                    let o = (row + dx * GLASS_DN + x) * 4;
                    acc[0] += src[o + 2] as f32;
                    acc[1] += src[o + 1] as f32;
                    acc[2] += src[o] as f32;
                }
            }
            let k = (GLASS_DN * GLASS_DN) as f32;
            small[dy * dw + dx] = [acc[0] / k, acc[1] / k, acc[2] / k];
        }
    }
    // ② 三次盒式（横一遍 + 纵一遍 = 一次），边界 clamp
    let r = GLASS_BR as isize;
    let mut tmp: Vec<[f32; 3]> = vec![[0.0; 3]; dw * dw];
    for _ in 0..3 {
        for y in 0..dw {
            for x in 0..dw {
                let mut acc = [0f32; 3];
                for k in -r..=r {
                    let sx = (x as isize + k).clamp(0, dw as isize - 1) as usize;
                    let c = small[y * dw + sx];
                    acc[0] += c[0];
                    acc[1] += c[1];
                    acc[2] += c[2];
                }
                let n = (2 * r + 1) as f32;
                tmp[y * dw + x] = [acc[0] / n, acc[1] / n, acc[2] / n];
            }
        }
        for y in 0..dw {
            for x in 0..dw {
                let mut acc = [0f32; 3];
                for k in -r..=r {
                    let sy = (y as isize + k).clamp(0, dw as isize - 1) as usize;
                    let c = tmp[sy * dw + x];
                    acc[0] += c[0];
                    acc[1] += c[1];
                    acc[2] += c[2];
                }
                let n = (2 * r + 1) as f32;
                small[y * dw + x] = [acc[0] / n, acc[1] / n, acc[2] / n];
            }
        }
    }
    // ③ 双线性放大
    let mut out = vec![0u8; cw * cw * 4];
    for y in 0..cw {
        let fy = ((y as f32 + 0.5) / GLASS_DN as f32 - 0.5).max(0.0);
        let y0 = (fy.floor() as usize).min(dw - 1);
        let y1 = (y0 + 1).min(dw - 1);
        let ty = (fy - y0 as f32).clamp(0.0, 1.0);
        for x in 0..cw {
            let fx = ((x as f32 + 0.5) / GLASS_DN as f32 - 0.5).max(0.0);
            let x0 = (fx.floor() as usize).min(dw - 1);
            let x1 = (x0 + 1).min(dw - 1);
            let tx = (fx - x0 as f32).clamp(0.0, 1.0);
            let a0 = small[y0 * dw + x0];
            let a1 = small[y0 * dw + x1];
            let b0 = small[y1 * dw + x0];
            let b1 = small[y1 * dw + x1];
            let o = (y * cw + x) * 4;
            // ⚠️ 内部数组是 **RGB**（降采样时把 src 的 BGR 重排成 RGB 更好读），
            //    写回 buf 必须还原成 **BGRA** —— 这里第一次就写错了，
            //    盘面变一片灰（R/B 互换 + saturate 把灰放大），排查时靠 eprintln 采样值定位。
            for c in 0..3 {
                let top = a0[c] + (a1[c] - a0[c]) * tx;
                let bot = b0[c] + (b1[c] - b0[c]) * tx;
                let v = (top + (bot - top) * ty).clamp(0.0, 255.0);
                let dst = match c {
                    0 => 2, // R → o+2
                    1 => 1, // G
                    _ => 0, // B → o+0
                };
                out[o + dst] = v as u8;
            }
            out[o + 3] = 255;
        }
    }
    out
}

/// 一个玻璃像素：模糊底图 → 提鲜/提亮 → 叠 tint → 叠边缘高光 + 斜向光泽。
///
/// `(x, y)` 是**画布坐标**（像素中心），`(cx, cy, r)` 是**盘的几何**（中心与半径）。
fn glass_px(bg: &[u8], x: f32, y: f32, cx: f32, cy: f32, r: f32, dark: bool, ta: f32,
            rim_k: f32, sharp: f32) -> (f32, f32, f32) {
    let dx0 = x + 0.5 - cx;
    let dy0 = y + 0.5 - cy;
    let d0 = (dx0 * dx0 + dy0 * dy0).sqrt().max(0.001);
    let e0 = r - d0;
    // ⓪ **折射**（液态玻璃的灵魂，只有模糊是不够的）：
    //    越靠边，采样点沿径向往外拉得越远 —— 边缘于是"吸进"盘外的景物，
    //    看起来像一块透镜把背景压弯了。`GLASS_REFRACT` 是最大外拉距离。
    let edge_k = (1.0 - (e0 / (GLASS_RIM * 3.2)).clamp(0.0, 1.0)).powi(2);
    let pull = edge_k * GLASS_REFRACT;
    let sx = x + (dx0 / d0) * pull;
    let sy = y + (dy0 / d0) * pull;
    let ix = sx.clamp(0.0, CANVAS as f32 - 1.0) as usize;
    let iy = sy.clamp(0.0, CANVAS as f32 - 1.0) as usize;
    let idx = iy * CANVAS as usize + ix;
    let n = (CANVAS * CANVAS * 4) as usize;
    let mb = idx * 4;          // 模糊层（缓冲后半）
    let mr = mb + n;           // 原图（缓冲前半）—— 顺序：raw 在前、blur 在后
    let mut c = [bg[mr + 2] as f32, bg[mr + 1] as f32, bg[mr] as f32];
    // 混入模糊层：**这一步决定了它是"玻璃"还是"磨砂"**。
    // 全模糊 = 磨砂；这里只盖 `sharp`（默认 70%），剩下 30% 原图的细节 ——
    // 背景的文字/颗粒/色块边界都还认得出来，边缘又是柔的。
    let inv = 1.0 - sharp;
    c[0] = c[0] * inv + bg[mb + 2] as f32 * sharp;
    c[1] = c[1] * inv + bg[mb + 1] as f32 * sharp;
    c[2] = c[2] * inv + bg[mb] as f32 * sharp;
    // ① 饱和度 + 提亮 4%：玻璃"透出来但更艳"，糊成灰就成磨砂了
    let lum = 0.299 * c[0] + 0.587 * c[1] + 0.114 * c[2];
    for ch in c.iter_mut() {
        *ch = (lum + (*ch - lum) * GLASS_SAT) * 1.04;
    }
    // ② tint：玻璃本身那层"色"（深色主题 → 深灰；浅色 → 近白）。
    //    `ta` 由调用方给：盘用 `GLASS_TINT_A_*`（很淡），胶囊用 `GLASS_CHIP_A_*`（更实）。
    let (tr, tg, tb) = if dark {
        (22.0, 22.0, 28.0)
    } else {
        (250.0, 250.0, 252.0)
    };
    c[0] = c[0] * (1.0 - ta) + tr * ta;
    c[1] = c[1] * (1.0 - ta) + tg * ta;
    c[2] = c[2] * (1.0 - ta) + tb * ta;
    // 玻璃自身的一点"抬升"（+2%）：没有它，浅色桌面上盘面和背景糊成一片
    for ch in c.iter_mut() {
        *ch += (255.0 - *ch) * 0.02;
    }

    let (dx, dy, d) = (dx0, dy0, d0);
    // ③ 边缘 inset 高光：上 0.55 / 左右 0.18 / 下 0.13 —— "玻璃有厚度"几乎全靠这层
    let e = e0;
    if e < GLASS_RIM && e > -2.0 {
        let (nx, ny) = (dx / d, dy / d);
        let k = (0.72 * (-ny).max(0.0) + 0.27 * nx.abs() + 0.17 * ny.max(0.0))
            * (1.0 - (e.max(0.0) / GLASS_RIM).clamp(0.0, 1.0)).powi(2);
        for ch in c.iter_mut() {
            *ch += (255.0 - *ch) * k * rim_k;
        }
    }
    // ③b 厚度暗带：**已去掉**（用户 2026-10-17：「轮盘还分层了」）。
    //     原意是让边缘"有厚度"，但在浅色桌面上它是一圈 13px 宽的灰环，
    //     叠上白描边和高光就成了三层同心圆 —— 看着像盘里套了个盘。
    //     玻璃的边只应该是**一道细高光**，厚度感交给折射去表达。
    // ④ 一道 126° 斜向光泽（极淡）：让盘面看起来像有液体在流
    let ux = 126f32.to_radians().cos();
    let uy = 126f32.to_radians().sin();
    let p = ((dx * ux + dy * uy) / (r * 2.0) + 0.5).clamp(0.0, 1.0);
    let band = (p * std::f32::consts::PI).sin().max(0.0).powi(3) * 0.07;
    for ch in c.iter_mut() {
        *ch += (255.0 - *ch) * band;
    }
    (c[0].clamp(0.0, 255.0), c[1].clamp(0.0, 255.0), c[2].clamp(0.0, 255.0))
}

/// 玻璃盘面的**色层缓存**：只装盘内部（`bbox` 内整像素 BGRA，A=255；其余像素 A=0 表示「不属于这层」）。
struct GlassDiscLayer {
    /// 生成它时的 `glass_bg` 修订号（换快照就作废）
    rev: u64,
    /// 生成它时的盘面半径（`f32::to_bits`）—— 开合动画里每帧都变，自然每帧重建
    r: u32,
    dark: bool,
    /// 生成它时是否走「自擦」模式（WDA 设不上）—— 那一档清晰度不同，缓存不能共用
    mask: bool,
    x0: usize,
    y0: usize,
    w: usize,
    h: usize,
    px: Vec<u8>,
}

thread_local! {
    /// 只有 UI 线程画图 → 单槽就够（菜单同时只有一个盘面）
    static GLASS_DISC_CACHE: std::cell::RefCell<Option<GlassDiscLayer>> =
        const { std::cell::RefCell::new(None) };
}

/// 收起菜单 / 关掉玻璃选项时释放那块 bbox
fn glass_disc_drop() {
    GLASS_DISC_CACHE.with(|c| {
        *c.borrow_mut() = None;
    });
}

/// 把盘面换成玻璃。**必须在 `flat_circle` 之后、任何图标之前调用** ——
/// 那时盘内只有盘面（和一点描边），覆盖掉正好；再往后图标就画在玻璃上了。
///
/// ⚠️ 这一层是**滚动能不能跟手**的关键：`glass_px` 每像素要做两次跨 3.6MB 的
/// 随机采样，整个盘 ≈17 万像素 ≈7ms —— 而它的输出只跟 (快照, 半径, 深浅色) 有关，
/// **滚轮偏移动的是图标，盘面一个像素都不变**。所以这里把它缓存成一层，
/// 命中时只做一次顺序 blit（~0.3ms）。
unsafe fn glass_disc(st: &State, buf: &mut [u8], r: f32, dark: bool) {
    let need = (CANVAS * CANVAS * 4) as usize * 2;
    if st.glass_bg.len() < need {
        return;
    }
    let w = CANVAS as usize;
    let ext = r + 2.0;
    let y0 = (CC - ext).max(0.0) as usize;
    let y1 = ((CC + ext).min(CANVAS as f32 - 1.0)) as usize;
    let x0 = (CC - ext).max(0.0) as usize;
    let x1 = ((CC + ext).min(CANVAS as f32 - 1.0)) as usize;
    let lw = x1 - x0 + 1;
    let lh = y1 - y0 + 1;
    let ta = if dark { GLASS_TINT_A_D } else { GLASS_TINT_A_L };
    // WDA 设不上时，盘里的「原图」层是拿外圈均色填出来的（不是真桌面）。
    // 那一层留 30% 只会把分箱的扇形条纹看得清清楚楚 → 干脆基本全用模糊层，读成磨砂玻璃。
    let mask = GLASS_SELF_MASK.load(Ordering::Relaxed);
    let sharp = if mask { GLASS_SHARP_MASK } else { GLASS_SHARP };

    // 缓存只装**盘内部**（`cov >= 0.999`，即整像素替换）。盘沿那道抗锯齿带只有
    // 不到 1px 宽、约 2k 像素（≈0.1ms），却依赖浮点覆盖率 —— 量化成 8 位会把整圈
    // 盘沿改成 ±1，所以它每帧现算，跟缓存无关。
    let rev = GLASS_BG_REV.load(Ordering::Relaxed);
    let hit = GLASS_DISC_CACHE.with(|c| {
        let b = c.borrow();
        matches!(b.as_ref(), Some(l)
            if l.rev == rev && l.r == r.to_bits() && l.dark == dark && l.mask == mask
                && l.px.len() == lw * lh * 4)
    });
    if !hit {
        let mut px = vec![0u8; lw * lh * 4];
        for (row, py) in (y0..=y1).enumerate() {
            let fy = py as f32 + 0.5;
            let dy = fy - CC;
            for (col, pxi) in (x0..=x1).enumerate() {
                let fx = pxi as f32 + 0.5;
                let dx = fx - CC;
                if cov1((dx * dx + dy * dy).sqrt() - r) < 0.999 {
                    continue;
                }
                let (cr, cg, cb) = glass_px(&st.glass_bg, fx, fy, CC, CC, r, dark, ta, 1.0, sharp);
                let o = (row * lw + col) * 4;
                px[o] = cb as u8;
                px[o + 1] = cg as u8;
                px[o + 2] = cr as u8;
                px[o + 3] = 255; // 内部：整像素替换，不是叠加（盘下压着的是纸色，混进来会发灰）
            }
        }
        GLASS_DISC_CACHE.with(|c| {
            *c.borrow_mut() = Some(GlassDiscLayer { rev, r: r.to_bits(), dark, mask, x0, y0, w: lw, h: lh, px });
        });
    }

    GLASS_DISC_CACHE.with(|c| {
        let b = c.borrow();
        let l = match b.as_ref() {
            Some(l) => l,
            None => return,
        };
        for row in 0..l.h {
            let base = row * l.w * 4;
            for col in 0..l.w {
                let i = base + col * 4;
                if l.px[i + 3] == 0 {
                    continue;
                }
                let o = ((l.y0 + row) * w + l.x0 + col) * 4;
                buf[o..o + 4].copy_from_slice(&l.px[i..i + 4]);
            }
        }
    });

    // 盘沿的亚像素带（`cov` 落在 0.002~0.999 之间，不到 1px 宽 ≈2k 像素）每帧现算：
    // 覆盖率必须是浮点，量化进缓存会把整圈盘沿改成 ±1。
    // 这里刻意扫整个 bbox —— 每像素只有一次 `sqrt` + 比较（~0.4ms），
    // 真正贵的 `glass_px` 只会在那道带上触发。
    for py in y0..=y1 {
        let fy = py as f32 + 0.5;
        let dy = fy - CC;
        for pxi in x0..=x1 {
            let fx = pxi as f32 + 0.5;
            let dx = fx - CC;
            let cov = cov1((dx * dx + dy * dy).sqrt() - r);
            if cov <= 0.002 || cov >= 0.999 {
                continue; // 盘外，或已由缓存整像素替换
            }
            let (cr, cg, cb) = glass_px(&st.glass_bg, fx, fy, CC, CC, r, dark, ta, 1.0, sharp);
            blend_over(buf, (py * w + pxi) * 4, cr, cg, cb, cov);
        }
    }
}

unsafe fn draw_scene(st: &mut State, buf: &mut [u8]) {
    let w = CANVAS as usize;
    let now = now_s() as f64;
    buf.fill(0);
    // 有幽灵时按 `n+1` 排布：幽灵占一个真实槽位、整环让位（用户 2026-10-12 要求）
    let n_ap = st.apps.len();
    // 虚线教学槽位已取消（用户 2026-10-18：它常驻、且位置与真实图标重叠）。
    // 现在「拖进来会多一格」只在**真正拖动时**由 `drop_ghost` 幽灵磁贴体现。
    let n = n_ap + if st.drop_ghost.is_some() { 1 } else { 0 };
    if n == 0 {
        return;
    }
    let (r_icon, icon_draw) = ring_layout(n);
    let disc_r0 = disc_r(n);        // 盘面半径（随条目数自适应）
    let band_r0 = band_in(n);       // 环带内沿

    let pal = pal_of(st.glass_liquid);
    // 精致化的关键：描边一律用「hairline」，不用纯 ink 粗描边
    let hair = mixc(pal.paper, pal.ink, 0.16);

    if st.dump_bg && !st.bg_bits.is_null() {
        let src = std::slice::from_raw_parts(st.bg_bits, w * CANVAS as usize * 4);
        buf.copy_from_slice(src);
    }

    let pop_k = if st.entering || now - st.ent0 < 0.32 {
        clamp01(((now - st.ent0) / 0.32) as f32)
    } else {
        1.0
    };
    let pop = lerp(0.62, 1.0, ease_out(pop_k));
    let disc_r = disc_r0 * pop;

    // ── 1. 盘面：白纸 + 1.5px 细描边 + 向下偏移的贴身柔和投影 ──
    // 投影刻意收紧（dy 6 / blur 18 → 最远只到 r+30），避免在深色桌面上
    // 显出一圈「半透明色带」。真正的高级感来自干净的边界，不是大范围光晕。
    // 盘沿：纸面是墨灰细线；玻璃是**白边**（玻璃边缘就是一道反光，不是墨线）
    let (rim_col, rim_a) = if st.glass {
        ((250u8, 250u8, 252u8), 0.42)
    } else {
        (mixc(pal.paper, pal.ink, 0.40), 1.0)
    };
    flat_circle(
        buf,
        CC,
        CC,
        disc_r,
        pal.paper,
        1.7 * K,
        rim_col,
        rim_a,
        6.0 * K,
        0.11,
        18.0 * K,
        1.0,
    );
    // 液态玻璃：把盘面换成"模糊桌面 + tint"。
    // ⚠️ 位置很关键 —— 必须在 `flat_circle` 之后、**任何图标之前**：
    //    早了没盘可盖，晚了会把图标/头像一起盖掉。
    if st.glass {
        glass_disc(&*st, buf, disc_r, st.glass_liquid);
    }
    // WDA 设不上时抓屏线程要「自擦」的半径 —— 只有这一帧的绘制范围说了算：
    // 外环展开时比盘面大一整圈，只按 disc_r 擦就会在玻璃里留下一圈自己的梯形。
    if GLASS_SELF_MASK.load(Ordering::Relaxed) {
        let drawn = if st.active_app.is_some() {
            ring_radii(n).1 + 2.0 * K
        } else {
            disc_r
        };
        GLASS_MASK_R.store(((drawn + GL_MASK_SHAD) * 10.0) as u32, Ordering::Relaxed);
    }

    // 拖放感知指示：drop 模式下盘沿亮一圈绿环（证明 OLE 已找到我们）
    if st.drop_mode {
        let rim_r = disc_r0 * pop;
        for py in 0..CANVAS as usize {
            let dy = py as f32 + 0.5 - CC;
            if dy.abs() > rim_r + 2.0 {
                continue;
            }
            let sx = (rim_r * rim_r - dy * dy).sqrt().max(0.0);
            for px in (CC - sx - 2.0).max(0.0) as usize..((CC + sx + 2.0).min(CANVAS as f32 - 0.5) as usize + 1) {
                let dx = px as f32 + 0.5 - CC;
                let d = circle_sdf(dx, dy, rim_r);
                if d.abs() <= 1.6 {
                    let a = (1.0 - d.abs() / 1.6) * 0.55;
                    let o = (py * w + px) * 4;
                    if buf[o + 3] > 0 {
                        blend_over(buf, o, 60.0, 200.0, 90.0, a);
                    }
                }
            }
        }
    }

    // 极淡垂直渐变：上纯白 → 下 4% 灰，给出「纸质实体」的厚度。
    // ⚠️ 玻璃下**必须跳过**：这层是不透明的纸色，会把整块玻璃冲成一块灰板
    //    （第一版就是这样，盘面 191 灰、一点背景都透不出来）。玻璃的"厚度"
    //    由 `glass_px` 里的边缘高光表达，不需要纸面渐变。
    if !st.glass {
        disc_gradient(
            buf,
            CC,
            CC,
            disc_r - 1.0,
            pal.paper,
            mixc(pal.paper, pal.ink, 0.055),
        );
    }
    // 图标环带：一条中性灰带，图标坐在带上。
    // **不能省** —— 记事本那种「白纸图标」在纯白盘面上会整片消失，
    // 参考图（TheRing）用同一条带子解决对比度；带子同时给图标划出统一的轨道。
    // 玻璃下改成**极淡**的一层墨（浅色压暗 / 深色提亮 ≈6%）：轨道还在，
    // 但不再把背景糊死。
    if st.glass {
        // 玻璃下**不画环带**（用户 2026-10-17：「轮盘还分层了」）——
        // 那是一条以头像球为圆心的暗环，在近乎透明的盘面上就是一圈同心圆，
        // 看着像"盘里还有一层盘"。图标本身有圆角底板，够对比度了。
    } else {
        ring_band(
            buf,
            CC,
            CC,
            band_r0 * pop,
            (r_icon + icon_draw * 0.5 + BAND_PAD) * pop,
            mixc(pal.paper, pal.ink, BAND_DARK),
            1.0,
        );
    }

    // ── 2. 中心头像：GrokBot 式表情球 ──
    //    几何/动效全部在 `draw_hub_face` 里（与「关于」页的 logo 共用）。
    draw_hub_face(&*st, buf, CC, CC, HUB_R * pop, &pal, now);

    // ── 3. 应用：真实图标直接立在盘面上；选中态 = 图标背后的柔和高亮方 ──
    let n_sel = n;
    // 只画**真实应用**（0..n_ap）；幽灵在第 n_ap 号槽，由 3a 段单独画
    for i in 0..n_ap {
        let ap = {
            let t0 = st.ent0 + 0.05 + i as f64 * 0.022;
            let k = clamp01(((now - t0) / 0.28) as f32);
            ease_out_back(k)
        };
        if ap <= 0.01 {
            continue;
        }
        // ⚠️ 拖动重排时**不再**靠「点亮目标磁贴」做反馈：图标已经让位开缝，
        //    而 `drag_over_app` 是**插入索引**、不是某个图标的显示槽位，
        //    拿它去点亮会亮错那一格（正是用户 2026-10-14 拍到的「光标压在某个应用上」）。
        //    反馈改由「缝 + 插入光标」承担（见下面「3-插入光标」段）。
        // ⚠️ 拖动中**一律不给高亮**（用户 2026-10-15：「拖动时相邻图标不应该出现白色框线」）。
        //    `hover_now` 是粘的、拖动时光标压在谁身上谁就 `app_hot=1` → 白框；
        //    `active_app` 又是故意粘的 → 就算鼠标不在也亮一根。这里两条一起压掉。
        let dragging_any = st.drag_app.is_some();
        let hot_k = if dragging_any { 0.0 } else { st.app_hot[i] };
        let dim = st.app_anim[i];
        // 正在被拖出的那个：跟着光标走，并且随拖出距离淡出
        let dragging = st.drag_app == Some(i);
        let fade = if dragging { st.drag_fade } else { 0.0 };
        // **显示槽位**（连续值）→ 极角、可见度、内沉全部由它派生，绘制与命中共用同一套。
        // 拖动重排时走平滑值（图标是滑开的，不是瞬间跳到新槽位）；被拖的那个跟光标走，
        // 永远不参与淡出 —— 它必须一直画在光标底下。
        let s_disp = if dragging || !st.app_slot_s[i].is_finite() {
            slot_of(st, i)
        } else {
            st.app_slot_s[i]
        };
        let appear = if dragging { 1.0 } else { slot_appear(s_disp, n) };
        if appear <= 0.01 {
            // 完全滚出可见窗口 → 一格都不画（顺带省掉这一格的逐像素循环）。
            // ⚠️ 下标升序 = 槽位升序，所以「淡出的那一格」一定先画、「淡入的那一格」后画，
            //    交叉处新图标压在上面 —— 不用额外排序。
            continue;
        }
        // 淡出同时内沉 + 缩小（见 `SCROLL_SINK`）
        let sink = SCROLL_SINK * (1.0 - appear);
        let am = (1.0 - 0.38 * dim) * ap * (1.0 - fade) * appear;
        // ⚠️ 只要**正在被拖**就跟光标走（不再要求 `fade > 0.01`）：
        //    改了 `want_fade` 之后盘内拖动时 fade 恒为 0，若还挂着这个条件，
        //    图标会赖在自己槽位上不动（「拖不动」）。淡出只影响 `am` 透明度。
        let (cx, cy) = if dragging {
            (st.drag_x, st.drag_y)
        } else {
            let (tx, ty) = polar(r_icon * pop - sink, slot_angle(s_disp, n));
            // 松手落位：**只有被拖的这个**从光标处滑进槽位。
            // 其余图标在拖动期间已经按预览排序滑到位了，这里绝不能再动一次
            // —— 用户 2026-10-14 报的「松手后别的图标也重新移动一下」。
            if st.land_app == i {
                let k = clamp01(((now - st.land_t0) / LAND_SECS) as f32);
                let e = ease_out(k);
                (lerp(st.land_from.0, tx, e), lerp(st.land_from.1, ty, e))
            } else {
                (tx, ty)
            }
        };
        let scale = (0.30 + 0.70 * ap) * (1.0 + 0.10 * hot_k) * (1.0 - sink / icon_draw * 0.55);

        // 每格一块**统一的圆角方形底板** —— 「整齐」的来源就在这里：
        // 无论图标自带什么轮廓（圆形标 / 白纸 / 全出血方块），最后都收敛成同一个圆角方块。
        // 白底图标（记事本那页白纸）靠它显形；hover/选中只改描边粗细与投影，
        // 不再另画一层「托底」—— 托底会被底板和图标整个盖住。
        //
        // **底板边长 = icon_draw（锁死），图标边长 = icon_draw / TILE_PAD_K**。
        // 于是底板露出一圈均匀白边 —— 那一圈就是「内边距」，
        // 让图标之间/图标与盘沿之间有呼吸感，同时轮廓仍然是统一的那个圆角方块。
        let sel = if dragging_any {
            0.0
        } else {
            hot_k.max(if st.active_app == Some(i) { 0.30 } else { 0.0 })
        };
        let icon_side = icon_draw * scale / TILE_PAD_K;
        {
            let side = icon_draw * scale;
            let half = side * 0.5;
            let rad = side * TILE_RAD_K;
            flat_round_rect(
                buf,
                cx,
                cy,
                half,
                half,
                rad,
                pal.paper,
                (1.4 + 1.6 * sel) * K,
                mixc(mixc(pal.paper, pal.ink, 0.34), pal.ink, sel),
                1.0,
                (3.0 + 2.0 * sel) * K,
                0.20 + 0.10 * sel,
                1.5 * K,
                am,
            );
        }

        // 「点了、正在起」→ 图标淡下去、中心三个跳动的点。
        // ⚠️ `LOADING_HUB` 这个哨兵与 `commit_chip == usize::MAX`（内环提交）是同一套约定。
        let hub_loading = st.loading == Some((i, LOADING_HUB));
        let icon_a = if hub_loading { am * 0.42 } else { am };
        if st.apps[i].target == "@act:start" {
            // 开始菜单：四格方块（和 Windows 开始按钮一个意思，但用我们的墨色画，
            // 不描微软的品牌色）
            let sz = icon_side * 0.24;
            let g = icon_side * 0.10;
            for k in 0..4 {
                let dx = if k % 2 == 0 { -1.0 } else { 1.0 };
                let dy = if k < 2 { -1.0 } else { 1.0 };
                let px = cx + dx * (sz * 0.5 + g * 0.5);
                let py = cy + dy * (sz * 0.5 + g * 0.5);
                flat_round_rect(
                    buf, px, py, sz * 0.5, sz * 0.5, sz * 0.22,
                    pal.ink, 0.0, pal.ink, 0.0, 0.0, 0.0, 0.0, icon_a,
                );
            }
        } else if let Some(tex) = st.apps[i].icon.as_ref() {
            draw_bgra_icon(buf, cx, cy, icon_side, tex, icon_a);
        } else if let Some(ch) = st.apps[i].name.chars().next() {
            // 没有真实图标：淡色圆角方 + 同色字形（不用实色块）
            let half = icon_side * 0.5 * 0.86;
            let acc = GRADS[st.apps[i].grad].0;
            flat_round_rect(
                buf, cx, cy, half, half, half * 0.5, mixc(pal.paper, acc, 0.12), 0.0, hair, 0.0,
                0.0, 0.0, 0.0, icon_a,
            );
            let cht = ch.to_uppercase().to_string();
            // 字色用 `ink`：原来用渐变主色，深色主题下「深色字压在深色磁贴上」——
            // 工具屏（"切"/"锁"）和无图标应用都会糊成一团（用户 2026-10-17 截图）。
            draw_text(st, buf, cx as i32, cy as i32, &cht, 20, 700, pal.ink.0, pal.ink.1, pal.ink.2,
                (255.0 * icon_a) as u8, 0, 0);
        }
        if hub_loading {
            draw_wait_dots(buf, cx, cy, icon_draw * 0.5, pal.ink, pal.paper, am.max(0.85));
        }

        if st.commit_pulse > 0.0 && st.commit_app == i && st.commit_chip == usize::MAX {
            let half = icon_draw * 0.5;
            flat_round_rect(buf, cx, cy, half, half, TILE_R, (255, 255, 255), 0.0, (0, 0, 0),
                0.0, 0.0, 0.0, 0.0, 0.42 * st.commit_pulse);
        }
        // 运行中指示点：内环应用有同名窗口在跑 → 图标正下方一个小圆点。
        // ⚠️ 项目禁彩色（piik 灰阶），用 `ink` 实点 + 底下垫 `paper` 圈，
        //    避免压在亮背景玻璃上糊掉（和页点的思路一致）。
        if i < MAX_APPS && st.app_running[i] {
            let off = icon_draw * scale * 0.5 + 5.0 * K;
            let rr = 2.5 * K;
            flat_circle(buf, cx, cy + off, rr + 1.3 * K, pal.paper, 0.0, pal.paper, 0.0,
                0.0, 0.0, 0.0, 0.9);
            flat_circle(buf, cx, cy + off, rr, pal.ink, 0.0, pal.ink, 0.0,
                0.0, 0.0, 0.0, 1.0);
        }
    }
    let _ = n_sel;

    // ── 3-移除区：拖着东西时，整圈虚线 = 「出了这圈松手就删」 ──
    // 用户 2026-10-14 定：移除区做成**整圈虚线**（不是只画盘外一段）。
    // 半径就是判定用的 `remove_r(n)`（胶囊环外沿 +20，离图标外沿有 80px 余量），
    // 正好落在胶囊环(→165)与阈值(185)之间那条空带里，不压胶囊。
    // 项目没有虚线原语 —— 用 `rot_rrect` 沿**切向**摆一圈小圆角条最省
    // （`polar` 走角度：切向单位向量 = (cos a, sin a) → rot = a 的弧度，
    //   不是插入光标那个 a−π/2，别混）。
    // 用户 2026-10-15：**先隐藏**（不是删掉）—— 判定逻辑、旋转、标签全部保留，
    // 想开回来把 `RM_ZONE_SHOW` 改 true 即可。
    if RM_ZONE_SHOW && (st.drag_app.is_some() || st.drag_chip.is_some()) {
        let rr = remove_r(n_ap);
        let col = mixc(pal.paper, pal.ink, 0.42);
        // ⚠️ `a1/a2` 同时加同一个相位 → 虚线图案整体旋转（而不是每段各自乱转）
        let sp = st.rm_spin;
        // ⚠️⚠️ 段数**只能是 24**（间距 15°）：原来 48 段（间距 7.5°）的旋转对称性太强 ——
        //    转 22° = 3×7.5−0.5，图案几乎不变，动态上也只是一片" shimmer"（闪烁噪声），
        //    根本读不出"在转"。段数少一半，一秒才移 6 格，肉眼能跟住。
        dashed_arc(buf, rr, sp, 360.0 + sp, RM_ZONE_DASHES, col, 0.85 + 0.15 * st.rm_spin_k,
            0.9 + 0.5 * st.rm_spin_k);
        // 底部小标签：这圈虚线是个新东西，不解释一句没人知道它什么意思。
        // ⚠️ 字号用 22（`font_bub_s`，真实 12px）—— 环形那批 px 码是查表键，
        //    传 11 拿到的是 7.3px 的 `font_label`，太小。
        let (lx, ly) = polar(rr + 18.0, 180.0);
        draw_text(st, buf, lx as i32, ly as i32, "出了这圈 = 松手移除",
            22, 400, col.0, col.1, col.2, 200, 0, 0);
    }

    // ── 3-插入光标：拖着内环图标时，在**让位开缝的那一格**画一根不闪烁的插入光标 ──
    // 主流重排序（iOS 主屏 / 任务栏 / Trello）的手感是：**图标滑开让出空位，
    // 插入线落在空位里** —— 表示「松手就插到这个缝里」，而不是「指着某个已有条目」。
    // 用户 2026-10-14 拍着截图说得很明确：光标该在**两个图标之间**。
    //
    // 所以这里必须满足三点：
    //   ① 图标已经按 `drag_preview_slot` 让位（见 on_timer 的极角平滑）→ 这一格是空的；
    //   ② 角度取**被拖那一项自己的平滑极角**（它的目标槽位就是缝的位置），
    //      走平滑值才能跟着图标一起滑过去，不会「图标在动、线不动」；
    //   ③ 径向跨满整个图标环 —— 读作「这两格之间的分隔线」，而不是一个点。
    // ⚠️ 内环**故意不画插入光标**（用户 2026-10-15：「内环可以不要光标」）。
    //    内环靠「图标滑开让位 + 空出来的那一格」本身就够读出落点，再加一根线是噪音。
    //    外环（胶囊）不行 —— 胶囊是一串等宽段，不画线就分不清要插到哪两段之间。
    if let Some((ai, dj)) = st.drag_chip {
        // ── 外环同款：光标画在胶囊让位开出来的**缝**上，跨满整个胶囊环 ──
        // 角度取**被拖那一段自己的平滑段角中点** —— 走平滑值才能跟着让位一起滑。
        // ⚠️⚠️ 门控**不能挂 `drag_fade > 0.15`**：`want_fade` 改成「只有拖出盘面才淡」
        //    之后，盘内拖动时 `drag_fade` 恒为 0 → 加上这个条件光标**永远不出现**
        //    （用户 2026-10-15 报「外环之前有光标，现在没了」）。改用「段角已平滑」
        //    作就绪信号 —— 它在拖动开始那一帧就会从 NAN 变成有限值。
        if ai < st.apps.len()
            && dj < MAX_ITEMS
            && st.drag_over_chip.is_some()
            && st.chip_seg_a[dj].0.is_finite()
        {
            let (b1, b2) = st.chip_seg_a[dj];
            let a = (b1 + b2) * 0.5;
            let (rin, rout) = ring_radii(n);
            let (x0, y0) = polar(rin, a);
            let (x1, y1) = polar(rout, a);
            let mid = ((x0 + x1) * 0.5, (y0 + y1) * 0.5);
            let len = ((x1 - x0) * (x1 - x0) + (y1 - y0) * (y1 - y0)).sqrt();
            let th = 2.0;
            rot_rrect(buf, mid.0, mid.1, len * 0.5, th, th,
                a.to_radians() - std::f32::consts::FRAC_PI_2, pal.ink, 1.0);
        }
    }

    // ── 3a. 幽灵磁贴：拖入内容"认不出宿主 / 宿主不在内环"→ 在空位上浮一个**半透明磁贴**。
    //     丢到它上面才真正新建内环。⚠️ 不参与 `ring_layout(n)`，只是叠上去，已有磁贴不动。──
    if let Some(g) = &st.drop_ghost {
        // 满环时幽灵站在**多出来的那一格**（`apps.len()`）→ 不滚出来就看不见。
        // 可见度与真实磁贴走同一份 `slot_appear`，`on_timer` 会在拖动时自动滚到它那一页。
        let ga = slot_appear(slot_of(st, n_ap), n);
        if ga > 0.01 {
            let (gx, gy) = ghost_pos(st);
            let half = icon_draw * 0.5;
            let rad = icon_draw * TILE_RAD_K;
            // 半透明底 + **墨色描边**：原来描边太浅（34% 灰 × 55% α），真机上几乎看不见，
            // 用户根本找不到落点 → 拖进来"没反应"。描边必须够实。
            flat_round_rect(
                buf, gx, gy, half, half, rad,
                mixc(pal.paper, pal.ink, 0.06),
                1.8 * K,
                pal.ink,
                0.85,
                0.0, 0.0, 0.0,
                0.62 * ga,
            );
            // 优先画**真图标**（一眼就知道要加的是哪个应用）；实在没有才退回首字母 ——
            // 只画首字母（Edge → "M"）用户根本看不懂「这是啥」。
            if let Some(tex) = &g.icon {
                let icon_side = icon_draw * 0.86;
                draw_bgra_icon(buf, gx, gy, icon_side, tex, 0.72 * ga);
            } else if let Some(ch) = g.name.chars().next() {
                let cht = ch.to_uppercase().to_string();
                let acc = mixc(pal.paper, pal.ink, 0.62);
                draw_text(st, buf, gx as i32, gy as i32, &cht, 20, 700,
                    acc.0, acc.1, acc.2, (255.0 * 0.62 * ga) as u8, 0, 0);
            }
        }
    }

    // （教学虚线槽位已移除：它常驻且与图标重叠；落点指引改由拖动时的 `drop_ghost` 承担。）

    // ── 3a-刻度：超过一屏才出现的一条**底部细弧**（6 点方向 = 盘上最空的那一带）──
    //    轨道 = 整环可滚的范围，滑块 = 当前这一页占的份数，位置 = 当前偏移。
    //    不写数字、不放箭头：这一条只负责表达「还有别的页」，其余交给气泡说明。
    {
        let mx = ring_off_max(n);
        if mx > 0.0 {
            let r = disc_r - 11.0;
            let half = 34.0; // 轨道半跨（度），以正下方 180° 为中心
            // 滑块长度 ∝ 一页 / 总数（至少留一点，24 个应用时也不至于看不见）
            let th = (half * 2.0 * (ring_vis(n) as f32 / n as f32)).max(10.0);
            let t = (st.ring_off / mx).clamp(0.0, 1.0);
            // 屏幕上的「左端」是 214°（7 点），「右端」是 146°（5 点）→
            // 第 1 页靠左、最后一页靠右，读法和横向滚动条一致。
            let c = lerp(180.0 + half - th * 0.5, 180.0 - half + th * 0.5, t);
            let w = 1.5 * K;
            flat_arc(buf, r, 180.0 - half, 180.0 + half, w,
                mixc(pal.paper, pal.ink, 0.16), 0.9);
            flat_arc(buf, r, c - th * 0.5, c + th * 0.5, w,
                mixc(pal.paper, pal.ink, 0.52), 1.0);
        }
    }

    // ── 3b. 取消按钮（拖出去松手后出现，边框走 5 秒倒计时）──
    // 画法抄自 my-pet `objects/dialogs/dynamic_island.py::_paint_event_countdown`：
    // 把底边「点亮」为高亮色，高亮 = 剩余，随倒计时从起点回退。
    // 那边是手绘抖动边框所以线宽要 2.6px 才盖得住；我们是硬边，1.8px 就够。
    // ⚠️ 画在应用图标**之后**：z 序压过图标和它的投影，物理上不该有重叠，
    //    但万一有（数值将来被改坏），也绝不允许图标把按钮盖住。
    if st.cancel_a > 0.01 {
        let (bx, by) = rm_btn_center(st.cancel_a);
        let a = st.cancel_a;
        let hw = RM_BTN_W * 0.5 * a;
        let hh = RM_BTN_H * 0.5 * a;
        let frac = match rm_soonest(st) {
            Some(next) => ((next - now) / REMOVE_GRACE).clamp(0.0, 1.0) as f32,
            None => 0.0,
        };
        flat_round_rect(buf, bx, by + 2.5 * a, hw, hh, RM_BTN_R * a,
            mixc(pal.paper, pal.ink, 0.16), 0.0, pal.ink, 0.0, 0.0, 0.0, 0.0, a);
        flat_round_rect(buf, bx, by, hw, hh, RM_BTN_R * a, pal.paper,
            1.6 * K, mixc(pal.paper, pal.ink, 0.34), 1.0, 0.0, 0.0, 0.0, a);
        // 倒计时：底边直线段（避开两角圆角）画一条墨色圆头线，长度 ∝ 剩余
        let x0 = bx - hw + RM_BTN_R * a;
        let x1 = bx + hw - RM_BTN_R * a;
        let span = (x1 - x0) * frac;
        if span >= 3.0 {
            let y = by + hh - 0.8;
            flat_round_rect(buf, x0 + span * 0.5, y, span * 0.5, 0.9, 0.9,
                pal.ink, 0.0, pal.ink, 0.0, 0.0, 0.0, 0.0, a);
        }
        if a > 0.55 {
            draw_text(st, buf, bx as i32, by as i32, "取消", 13, 700,
                pal.ink.0, pal.ink.1, pal.ink.2, (255.0 * a) as u8, 0, 0);
        }
    }

    // ── 4. 内容段（外环：梯形环）──
    if let Some(ai) = st.active_app {
        if ai < n {
            let (segs, _rots, _rads) = chip_layout(st, ai);
            // 拖动重排中：换成「预览布局」—— 被拖那一段空出来（=缝），其它胶囊顺移让位
            let preview = match (st.drag_chip, st.drag_over_chip) {
                (Some((dai, dj)), Some(ins)) if dai == ai => chip_preview_segs(st, ai, dj, ins),
                _ => Vec::new(),
            };
            let m = st.apps[ai].items.len();
            let (ring_in, ring_out) = ring_radii(n);
            for j in 0..m {
                let appear = {
                    let t0 = st.switch0 + j as f64 * 0.026;
                    clamp01(((now - t0) / 0.20) as f32)
                };
                let anim = (st.chip_anim[j] * ease_out_back(appear)).clamp(0.0, 1.0);
                if anim <= 0.01 {
                    continue;
                }
                // 正被拖的那一段：让位之后它的槽位就是**缝**，不画（松手才落回去）
                if !preview.is_empty() && st.drag_chip == Some((ai, j)) {
                    continue;
                }
                // ⚠️ 不再用 `drag_over_chip` 点亮：它是**插入索引**、不是某一段的显示槽位，
                //    让位之后拿它点亮会亮错那一格（内环同款坑，用户 2026-10-14 拍到过）。
                let hot_k = st.chip_hot[j];
                // 段角走**平滑值**（拖动让位时是滑开的，不是瞬间跳）
                let (a1, a2) = if st.chip_seg_a[j].0.is_finite() {
                    st.chip_seg_a[j]
                } else if !preview.is_empty() {
                    preview.get(j).copied().unwrap_or(segs[j])
                } else {
                    segs[j]
                };
                let seg = ((a1 + a2) * 0.5, a1, a2);
                // 正被拖出的那一段：原地淡出（与应用图标同款）
                let fade = if st.drag_chip == Some((ai, j)) {
                    st.drag_fade
                } else {
                    0.0
                };
                let am = anim * (1.0 - fade);
                let editing = st.editing == Some((ai, j));

                // 段本体（白底 + `mix(ink 34%)` 描边 + `0 3K` 硬投影）
                // + 段内内容（图标在左、名称在右，沿切线横排）—— 颜色与内环完全一致
                draw_ring_sector(
                    buf, &pal, ring_in, ring_out, seg,
                    if editing { 0.0 } else { hot_k }, am,
                    if editing { 1 } else { 0 },
                    glass_arg(st, disc_r0),
                );
                let it = &st.apps[ai].items[j];
                // 这一段正是后台查询中 → 把名字换成 loading 动画
                let loading = st.loading == Some((ai, j));
                // 改名态：底是 `pal.ink`（亮色=近黑 / 暗色=近白），文字必须取**对比色**
                // `pal.paper`，不能写死白色 —— 暗色下 ink 本身就是浅色，白字=白底看不见
                // （用户 2026-10-12 报「深色模式双击编辑看不清」）。
                let fg = if editing { pal.paper } else { pal.ink };
                // 窗口条目的图标 = 所属应用的图标（它自己没有图标）：
                // 一排胶囊同一个图标，正好读成"这些都是这个应用开的"。
                let chip_icon = it.icon.as_ref();
                draw_ring_content(
                    st, buf, &pal, seg, ring_in, ring_out,
                    chip_icon,
                    if editing {
                        st.edit_buf.as_str()
                    } else {
                        it.name.as_str()
                    },
                    fg,
                    hot_k, editing, am, loading,
                );

                // ⚠️ 窗口条目的「小窗框」标记**已删**（用户 2026-10-15：「外环胶囊外面有个
                //    白色线段」）。它原来画在 `ring_out + 8*K` —— 胶囊外缘**之外** 5px，
                //    悬空飘着，深色主题下 `pal.ink` 近白，读起来就是一根莫名其妙的白线段，
                //    完全不像"这是个窗口"。已关闭的窗口条目也不再灰显（用户 2026-10-17：
                //    「不需要置灰」）—— 点它本来就会重开，没必要示弱。

                if st.commit_pulse > 0.0 && st.commit_chip == j {
                    // 提交成功的一次白色闪光
                    draw_ring_sector(buf, &pal, ring_in, ring_out, seg, 0.0,
                                     0.38 * st.commit_pulse, 2, None);
                }
            }

            // 拖入窗口/网页（认得出宿主）→ 末尾那一格是**幽灵胶囊**：
            // 虚线轮廓 + ＋ + 将要新增的名字。松手落点 = 这里（追加到外环末尾）。
            if let Some(gs) = chip_ghost_seg(st, ai) {
                let gname = drop_ghost_chip_name(st, ai, m).unwrap_or_default();
                draw_ghost_chip(st, buf, &pal, gs, &gname, st.chip_ghost_a);
            }
        }
    }

    // ── 5. 头像气泡：最后画，压在所有内容之上 ──
    draw_bubble(st, buf, &pal);
}

// ───────────────────── 图标位图（面积采样缩放） ─────────────────────

/// 图标纹理 = **mip 链**（逐级 ×1/1.4 的精确面积平均副本）。
///
/// 为什么要 mip：内环图标只画 ~26px、外环 ~19px，而纹理是 73px —— 一次 **2.8:1**
/// 的缩小，无论采样多好都会发虚（实测边缘能量比理想值低 ~25%，肉眼看就是「不清晰」）。
/// 做法是加载时就用**精确面积平均**生成几级副本，绘制时取「≥ 目标边长的最小一级」，
/// 剩下的只有 1.0~1.4:1 的双线性 —— 等效一次高质量缩小（实测边缘能量 +25%）。
#[derive(Clone, Default)]
struct IconTex {
    /// levels[0] 最大，边长递减
    levels: Vec<(usize, Vec<u8>)>,
}

impl IconTex {
    fn from_base(px: usize, data: Vec<u8>) -> IconTex {
        // ⚠️ 内存优化（2026-10-12）：不再无条件把基础纹理当作 levels[0] 存下来。
        // 基础纹理只用来**逐级降采样出 mip**，生成完即可丢 —— 绘制时取不到它
        // （见 `MIP_CAP`）。只有它本身 ≤ MIP_CAP（小图回退路径，如 32px）才必须留。
        let base = data;
        let mut levels: Vec<(usize, Vec<u8>)> = Vec::new();
        let mut cur = px;
        // ⚠️ 步长决定最终质量：`pick` 返回「≥ size 的最小一级」，所以纹理/绘制比
        // `k = tex_px / size` 恒落在 [1, 1.06]。
        //
        // ✂️ 只生成 ≤ MIP_CAP 的级：更大的级永远不会被 `pick` 命中，生成+存着纯浪费。
        //
        // 原来是 **1.4** → k ∈ [1, 1.4]，而**双线性在缩小 27% 时会漏掉高频**
        // （三角核比足迹窄），于是边缘出现台阶状混叠 ——
        // 用户 2026-10-09 报「内环 edge/墨刀、外环桌面/下载锯齿感很明显」就是这个。
        // 1.06 让 k 几乎等于 1，重采样退化成「取最近的、几乎 1:1 的那一级」。
        while cur > 12 {
            let next = ((cur as f32 / 1.06).round() as usize).max(6);
            if next >= cur {
                break;
            }
            // 一律从**基础纹理**降采样（逐级链式降采样会累积误差）
            if next <= MIP_CAP {
                levels.push((next, downsample_box(&base, px, next)));
            }
            cur = next;
        }
        // 基础纹理本身就是 ≤ MIP_CAP 的小图 → 它就是最大级，必须留着（否则 pick 无级可挑）
        if px <= MIP_CAP {
            levels.insert(0, (px, base));
        }
        // px > MIP_CAP：`base` 在此随函数结束被丢弃
        IconTex { levels }
    }

    /// 取「≥ size 的最小一级」；都比 size 小就退回最大的一级。
    fn pick(&self, size: f32) -> (usize, &[u8]) {
        // ⚠️⚠️ `levels` 是**从大到小**排的，而这里要的是「≥ size 里**最小**的那一级」。
        // 顺着从头找会**永远命中第一项 = 基础 73px 纹理** → 整条 mip 链形同虚设，
        // 绘制退化成一次 2.5:1（外环 3.3:1）的大缩小，而采样窗口又被 clamp 在 1.2 纹素
        // （只覆盖所需足迹的一半）→ **严重锯齿**。
        // 用户 2026-10-08 报的原话：「超强的锯齿感，无论外环还是内环」——就是这个。
        // 必须**倒着**找第一个满足的。
        for (n, d) in self.levels.iter().rev() {
            if *n as f32 >= size {
                return (*n, d);
            }
        }
        match self.levels.first() {
            Some((n, d)) => (*n, d),
            None => (0, &[]),
        }
    }
}

/// 精确面积平均（**预乘空间**）把方形纹理缩到 dw×dw。
/// 预乘累加是必须的：透明区 RGB=0，直接平均 RGB 会把边缘拉黑出脏边。
fn downsample_box(src: &[u8], sw: usize, dw: usize) -> Vec<u8> {
    let mut out = vec![0u8; dw * dw * 4];
    if sw == 0 || dw == 0 || src.len() < sw * sw * 4 {
        return out;
    }
    let scale = sw as f32 / dw as f32;
    for dy in 0..dw {
        let y0 = dy as f32 * scale;
        let y1 = y0 + scale;
        let iy0 = y0.floor() as usize;
        let iy1 = (y1.ceil() as usize).min(sw);
        for dx in 0..dw {
            let x0 = dx as f32 * scale;
            let x1 = x0 + scale;
            let ix0 = x0.floor() as usize;
            let ix1 = (x1.ceil() as usize).min(sw);
            let mut acc = [0.0f32; 4];
            let mut tot = 0.0f32;
            for sy in iy0..iy1 {
                let wy = (y1.min(sy as f32 + 1.0) - y0.max(sy as f32)).max(0.0);
                if wy <= 0.0 {
                    continue;
                }
                for sx in ix0..ix1 {
                    let wx = (x1.min(sx as f32 + 1.0) - x0.max(sx as f32)).max(0.0);
                    if wx <= 0.0 {
                        continue;
                    }
                    let so = (sy * sw + sx) * 4;
                    let a = src[so + 3] as f32 / 255.0;
                    let ww = wx * wy;
                    acc[0] += src[so + 2] as f32 * a * ww; // R（缓冲是 BGRA）
                    acc[1] += src[so + 1] as f32 * a * ww;
                    acc[2] += src[so] as f32 * a * ww;
                    acc[3] += a * ww;
                    tot += ww;
                }
            }
            let o = (dy * dw + dx) * 4;
            if tot > 1e-6 && acc[3] > 1e-6 {
                out[o] = (acc[2] / acc[3]).clamp(0.0, 255.0) as u8;
                out[o + 1] = (acc[1] / acc[3]).clamp(0.0, 255.0) as u8;
                out[o + 2] = (acc[0] / acc[3]).clamp(0.0, 255.0) as u8;
                out[o + 3] = ((acc[3] / tot) * 255.0).clamp(0.0, 255.0) as u8;
            }
        }
    }
    out
}

fn draw_bgra_icon(buf: &mut [u8], cx: f32, cy: f32, size: f32, tex: &IconTex, alpha: f32) {
    let (tex_px, data) = tex.pick(size);
    if alpha <= 0.01 || size <= 0.5 || tex_px == 0 || data.len() < tex_px * tex_px * 4 {
        return;
    }
    let n = tex_px as f32;
    let half = size * 0.5;
    let ext = half.ceil() as i32 + 1;
    // 每个目标像素覆盖多少源纹素。mip 步长 1.06 → 恒在 [1, 1.06]，
    // 所以下面的足迹面积平均最多跨 2×2 个纹素。
    let k = n / size;
    // 亚像素：中心不再取整（原来 `cx as i32` 是**截断**，最多错 1px，
    // 边缘的 AA 会和底板对不齐、看起来发毛）。
    let bx = cx.floor() as i32;
    let by = cy.floor() as i32;
    let fx0 = cx - bx as f32;                       // ∈ [0,1)
    let fy0 = cy - by as f32;
    // 足迹半宽（源纹素单位）
    let hw = k * 0.5;
    for iy in -ext..=ext {
        let py = by + iy;
        if py < 0 || py >= CANVAS {
            continue;
        }
        for ix in -ext..=ext {
            let px = bx + ix;
            if px < 0 || px >= CANVAS {
                continue;
            }
            // 目标像素中心 → 源坐标（像素中心对像素中心的约定：
            // 目标中心 -cx 时恰好落在源纹理中心）
            let sx = (ix as f32 + 0.5 - fx0) * k + (n - 1.0) * 0.5;
            let sy = (iy as f32 + 0.5 - fy0) * k + (n - 1.0) * 0.5;
            // 足迹 = k×k，做**面积平均**。
            // 这就是与缩小比匹配的抗混叠滤波 —— 2×2 抽头 + 双线性做不到这点，
            // 双线性的三角核只覆盖 2 纹素宽，而足迹需要 k 纹素宽。
            let x0f = sx - hw;
            let x1f = sx + hw;
            let y0f = sy - hw;
            let y1f = sy + hw;
            let ix0 = x0f.floor().max(0.0) as usize;
            let ix1 = (x1f.ceil() as usize).min(tex_px);
            let iy0 = y0f.floor().max(0.0) as usize;
            let iy1 = (y1f.ceil() as usize).min(tex_px);
            let mut acc = [0.0f32; 4];
            for yy in iy0..iy1 {
                let wy = (yy as f32 + 1.0).min(y1f) - (yy as f32).max(y0f);
                if wy <= 0.0 {
                    continue;
                }
                for xx in ix0..ix1 {
                    let wx = (xx as f32 + 1.0).min(x1f) - (xx as f32).max(x0f);
                    if wx <= 0.0 {
                        continue;
                    }
                    // ⚠️ 步长必须是**实际纹理尺寸** `tex_px`，不能写 `ICON_PX` ——
                    // `pe_png_icon` / 32px 回退会返回比 ICON_PX 小的纹理，
                    // 用常量步长会读串行、图标直接花掉。
                    let so = (yy * tex_px + xx) * 4;
                    let a = data[so + 3] as f32 / 255.0;
                    let ww = wx * wy;
                    acc[0] += data[so + 2] as f32 * a * ww; // R
                    acc[1] += data[so + 1] as f32 * a * ww; // G
                    acc[2] += data[so] as f32 * a * ww;     // B
                    acc[3] += a * ww;
                }
            }
            // 分母用**理论足迹面积** k*k（而不是实际覆盖和）：
            // 足迹越出纹理边界的部分按透明处理，边缘 alpha 才会正确衰减，
            // 不会被「重归一化」抬高成硬边。
            let wsum = k * k;
            if wsum <= 0.0 || acc[3] <= 1e-5 {
                continue;
            }
            let a = (acc[3] / wsum) * alpha;
            if a <= 0.004 {
                continue;
            }
            let r = acc[0] / acc[3];
            let g = acc[1] / acc[3];
            let b = acc[2] / acc[3];
            let o = (py as usize * CANVAS as usize + px as usize) * 4;
            blend_over(buf, o, r, g, b, a);
        }
    }
}

/// 旋转版图标绘制 —— 梯形段的内容沿切线排布，图标要跟着弧一起转。
///
/// 采样方式与 `draw_bgra_icon` **完全一致**（预乘空间累加 + 双线性 + 关于中心对称的
/// 超采样），只是把目标像素先**反旋**到图标局部坐标再取样。
/// 落在图标正方形外的子样本直接丢弃（不参与面积平均）—— 否则会去 clamp 纹理边缘，
/// 把边上的像素拖出一条脏边。
#[allow(clippy::too_many_arguments)]
fn draw_bgra_icon_rot(buf: &mut [u8], cx: f32, cy: f32, size: f32,
                      tex: &IconTex, rot: f32, alpha: f32) {
    let (tex_px, data) = tex.pick(size);
    if alpha <= 0.01 || size <= 0.5 || tex_px == 0 || data.len() < tex_px * tex_px * 4 {
        return;
    }
    let n = tex_px as f32;
    let half = size * 0.5;
    // 旋转后的外接框要按 √2 放大
    let ext = (half * 1.4143).ceil() as i32 + 1;
    let (sn, cs) = rot.sin_cos();
    let k = n / size;                                  // 每个目标像素覆盖多少源纹素
    // 亚像素（与非旋转版一致）。`i`/`j` 是相对 `bx`/`by` 的整数偏移，
    // 目标像素中心相对 (bx,by) 的局部坐标 = i + 0.5 - f0。
    let bx = cx.floor() as i32;
    let by = cy.floor() as i32;
    let fx0 = cx - bx as f32;
    let fy0 = cy - by as f32;
    // ⚠️ 旋转版没法直接按足迹做面积平均（足迹在纹理空间是个**旋转的**方块）。
    // 近似做法：在**局部空间**做 N×N 亚样本的 box 积分，每个亚样本反旋到纹理空间
    // 再双线性取样。k ≤ 1.06 → N=3 足够（采样点间距 1/3 局部像素）。
    const N: i32 = 3;
    let w_ = CANVAS as usize;
    for j in -ext..=ext {
        let py = by + j;
        if py < 0 || py >= CANVAS {
            continue;
        }
        for i in -ext..=ext {
            let px = bx + i;
            if px < 0 || px >= CANVAS {
                continue;
            }
            let mut acc = [0.0f32; 4];
            let mut hit = 0.0f32;
            for ty in 0..N {
                for tx in 0..N {
                    let lx = i as f32 + (tx as f32 + 0.5) / N as f32 - fx0;
                    let ly = j as f32 + (ty as f32 + 0.5) / N as f32 - fy0;
                    // 反旋到「纹理」坐标
                    let ux = lx * cs + ly * sn;
                    let uy = -lx * sn + ly * cs;
                    // ⚠️⚠️ 越界判据**必须用旋转后的 (ux, uy)** —— 那才是纹理的两个轴。
                    //
                    // 之前判的是旋转前的 (lx, ly)：那个正方形绕中心转 45° 后，角点会伸到
                    // 纹理外 √2 倍，于是 `sxf/syf` 被 clamp 到纹理边缘。对「内容撑满纹理」
                    // 的图标（Edge / 设置 / chrome…）边缘像素是不透明的，后果两条：
                    //   ① 圆角被边缘像素填满 → 图标看起来是**直角**方
                    //   ② 方块旋转后没覆盖到的 4 个角也被涂上颜色 → **四个方向凸起**
                    // 修法：落在纹理外就**直接丢这一份子样本**，绝不 clamp（clamp = 把内容
                    // 往外抹）。丢完之后画出来的轮廓正好是「随弧转的圆角方块」。
                    if ux.abs() > half || uy.abs() > half {
                        continue;
                    }
                    let sxf = (ux * k + (n - 1.0) * 0.5).clamp(0.0, n - 1.001);
                    let syf = (uy * k + (n - 1.0) * 0.5).clamp(0.0, n - 1.001);
                    let x0 = sxf.floor();
                    let y0 = syf.floor();
                    let (xi, yi) = (x0 as usize, y0 as usize);
                    let (fxr, fyr) = (sxf - x0, syf - y0);
                    let xi1 = (xi + 1).min(tex_px - 1);
                    let yi1 = (yi + 1).min(tex_px - 1);
                    for (yy2, wy) in [(yi, 1.0 - fyr), (yi1, fyr)] {
                        for (xx2, wx) in [(xi, 1.0 - fxr), (xi1, fxr)] {
                            let so = (yy2 * tex_px + xx2) * 4;
                            let a = data[so + 3] as f32 / 255.0;
                            let ww = wx * wy;
                            acc[0] += data[so + 2] as f32 * a * ww;
                            acc[1] += data[so + 1] as f32 * a * ww;
                            acc[2] += data[so] as f32 * a * ww;
                            acc[3] += a * ww;
                            hit += ww;
                        }
                    }
                }
            }
            // 分母 = N*N（理论亚样本数）→ 落在圆角外的子样本按透明计，
            // 边缘 alpha 正确衰减（不能拿 `hit` 当分母，那会把边"补满"成硬直角）。
            let _ = hit;
            let denom = (N * N) as f32;
            if acc[3] <= 1e-5 {
                continue;
            }
            let a = (acc[3] / denom) * alpha;
            if a <= 0.004 {
                continue;
            }
            let r = acc[0] / acc[3];
            let g = acc[1] / acc[3];
            let b = acc[2] / acc[3];
            let o = (py as usize * w_ + px as usize) * 4;
            blend_over(buf, o, r, g, b, a);
        }
    }
}

// ───────────────────── 文字（遮罩法：白字黑底 → 亮度作覆盖率） ─────────────────────

unsafe fn draw_text(
    st: &State,
    buf: &mut [u8],
    cx: i32,
    cy: i32,
    text: &str,
    px: i32,
    _weight: i32,
    cr: u8,
    cg: u8,
    cb: u8,
    alpha: u8,
    max_w: i32, // 0=居中不限宽；>0=左对齐省略号
    _align: i32,
) {
    if st.txt_dc.0.is_null() || text.is_empty() {
        return;
    }
    let dc = HDC(st.txt_dc.0);
    let font = match px {
        20 => st.font_glyph,
        0..=10 => st.font_hint,
        11 => st.font_label,
        12 => st.font_cnt,
        13 => st.font_row,
        14 => st.font_head,
        21 => st.font_bub,
        22 => st.font_bub_s,
        _ => st.font_name,
    };
    let _ = SelectObject(dc, HGDIOBJ(font.0));
    let _ = SetTextColor(dc, COLORREF(0x00FF_FFFF)); // 白字
    let _ = SetBkColor(dc, COLORREF(0)); // 黑底
    let _ = SetBkMode(dc, TRANSPARENT); // 只画字，不画背景
    // 只清**上一次**用过的宽度（每次清满 448×42 ≈ 1.9 万像素纯属浪费：
    // 一个 chip 要画两次，5 个 chip 每帧就是 19 万次无效写）
    let _ = PatBlt(dc, 0, 0, CANVAS, 140, BLACKNESS); // 全幅清除（与 draw_text_rot 共用 DC）
    let mut chars: Vec<u16> = text.encode_utf16().collect();
    let mut rect = RECT {
        left: 0,
        top: 0,
        right: if max_w > 0 { max_w } else { CANVAS - 8 },
        bottom: 40,
    };
    let fmt_base = DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX;
    if max_w > 0 {
        let _ = DrawTextW(dc, &mut chars, &mut rect, fmt_base | DT_LEFT | DT_END_ELLIPSIS);
    } else {
        let _ = DrawTextW(dc, &mut chars, &mut rect, fmt_base | DT_CALCRECT);
        let tw = rect.right - rect.left;
        // ⚠️ DT_CALCRECT 把 rect.bottom 缩成了文字行高 —— 拿这个矩形直接重画，
        // DT_VCENTER 就在**行高内**居中，字整体上浮 ~11px（曾把「取消」顶出
        // 胶囊上沿）。重画前必须恢复 40px 的满高居中矩形（draw_text_rot 同款）。
        rect.top = 0;
        rect.bottom = 40;
        rect.right = rect.left + tw;
        let _ = DrawTextW(dc, &mut chars, &mut rect, fmt_base | DT_CENTER);
    }
    TXT_W.store((rect.right - rect.left).max(0), Ordering::Relaxed);
    let x0 = if max_w > 0 { cx } else { cx - (rect.right - rect.left) / 2 };
    let y0 = cy - 20;

    let af = alpha as f32 / 255.0;
    for yy in 0..40i32 {
        let py = y0 + yy;
        if py < 0 || py >= CANVAS {
            continue;
        }
        for xx in 0..CANVAS {
            let px_x = x0 + xx;
            if px_x < 0 || px_x >= CANVAS {
                continue;
            }
            let o = (yy as usize * CANVAS as usize + xx as usize) * 4;
            let t = st.txt_bits.add(o);
            let lum = ((*t.add(2)) as u32 + (*t.add(1)) as u32 + (*t.add(0)) as u32) / 3;
            if lum == 0 {
                continue;
            }
            let a = (lum as f32 / 255.0) * af;
            let bo = (py as usize * CANVAS as usize + px_x as usize) * 4;
            if buf[bo + 3] == 0 {
                continue;
            }
            blend_over(buf, bo, cr as f32, cg as f32, cb as f32, a);
        }
    }
}

/// 旋转文字：先把字形画进遮罩（GDI），再按 rot **反向旋转采样**写进画布。
///
/// 贴弧胶囊里的标签必须跟胶囊一起转 —— 否则文字横向躺在倾斜的胶囊里会溢出来。
/// 采样用双线性，保证旋转后边缘不出现硬锯齿。
#[allow(clippy::too_many_arguments)]
unsafe fn draw_text_rot(
    st: &State,
    buf: &mut [u8],
    cx: f32,
    cy: f32,
    text: &str,
    px: i32,
    rot: f32,
    cr: u8,
    cg: u8,
    cb: u8,
    alpha: u8,
) {
    if st.txt_dc.0.is_null() || text.is_empty() || alpha == 0 {
        return;
    }
    let dc = HDC(st.txt_dc.0);
    let font = match px {
        20 => st.font_glyph,
        0..=10 => st.font_hint,
        11 => st.font_label,
        12 => st.font_cnt,
        13 => st.font_row,
        14 => st.font_head,
        21 => st.font_bub,
        22 => st.font_bub_s,
        _ => st.font_name,
    };
    let _ = SelectObject(dc, HGDIOBJ(font.0));
    let _ = SetTextColor(dc, COLORREF(0x00FF_FFFF));
    let _ = SetBkColor(dc, COLORREF(0));
    let _ = SetBkMode(dc, TRANSPARENT);
    // 3 倍超采样：GDI 按 3 倍字号出遮罩，旋转采样时 1 个目标像素覆盖
    // 3x3 个高分辨率纹素 —— 双线性落在超采样遮罩上等效面积平均，
    // 旋转后的文字边缘恢复 GDI 原生锐度（1x 遮罩旋转 = 重采样发虚的根因）。
    const Z: f32 = 3.0;
    let font3 = match px {
        20 => st.font_glyph3,
        0..=10 => st.font_hint3,
        11 => st.font_label3,
        12 => st.font_cnt3,
        13 => st.font_row3,
        14 => st.font_head3,
        _ => st.font_name3,
    };
    let _ = SelectObject(dc, HGDIOBJ(font3.0));
    let _ = PatBlt(dc, 0, 0, CANVAS, 140, BLACKNESS); // 全幅清除（与 draw_text 共用 DC）
    let mut chars: Vec<u16> = text.encode_utf16().collect();
    let mut rect = RECT { left: 0, top: 0, right: CANVAS - 8, bottom: 132 };
    let fmt = DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX;
    let _ = DrawTextW(dc, &mut chars, &mut rect, fmt | DT_CALCRECT);
    let tw3 = (rect.right - rect.left).max(1) as f32;
    let tw = tw3 / Z; // 1x 逻辑宽度（用于布局/外接圆）
    let mut rect2 = RECT { left: 0, top: 0, right: tw3 as i32, bottom: 132 };
    let _ = DrawTextW(dc, &mut chars, &mut rect2, fmt | DT_CENTER);
    TXT_W.store((tw3 / 3.0) as i32, Ordering::Relaxed);

    // 一次性诊断：每次 draw_text_rot 的遮罩单独落盘
    if std::env::var_os("RL_MASKDBG").is_some() {
        static MC: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let k = MC.fetch_add(1, Ordering::Relaxed);
        let n = (CANVAS * 140 * 4) as usize;
        let mb = std::slice::from_raw_parts(st.txt_bits, n);
        let _ = save_bmp(&format!("{}\u{5c}mask_{}.bmp", std::env::var("RL_MASKDBG").unwrap(), k), mb, CANVAS, 140);
    }

    let (sn, cs) = rot.sin_cos();
    let af = alpha as f32 / 255.0;
    // 目标像素的包围盒 = 旋转后的矩形外接圆
    let ext = (((tw * tw + 1600.0).sqrt()) * 0.5 + 4.0).ceil() as i32;
    let cxi = cx.round() as i32;
    let cyi = cy.round() as i32;
    let w = CANVAS as usize;
    for py in (cyi - ext).max(0)..=(cyi + ext).min(CANVAS - 1) {
        for pxx in (cxi - ext).max(0)..=(cxi + ext).min(CANVAS - 1) {
            let dx = pxx as f32 + 0.5 - cx;
            let dy = py as f32 + 0.5 - cy;
            let lx = dx * cs + dy * sn;
            let ly = -dx * sn + dy * cs;
            let mx = lx * Z + tw3 * 0.5;
            let my = ly * Z + 66.0;
            if mx < 0.0 || my < 0.0 || mx >= tw3 || my >= 132.0 {
                continue;
            }
            // 双线性采样遮罩亮度
            let x0 = mx.floor() as i32;
            let y0 = my.floor() as i32;
            let fx = mx - x0 as f32;
            let fy = my - y0 as f32;
            let mut acc = 0.0f32;
            for (ox, oy, wgt) in [
                (0i32, 0i32, (1.0 - fx) * (1.0 - fy)),
                (1, 0, fx * (1.0 - fy)),
                (0, 1, (1.0 - fx) * fy),
                (1, 1, fx * fy),
            ] {
                if wgt <= 0.0 {
                    continue;
                }
                let sx = x0 + ox;
                let sy = y0 + oy;
                if sx < 0 || sy < 0 || sx >= CANVAS || sy >= 140 {
                    continue;
                }
                let o = (sy as usize * CANVAS as usize + sx as usize) * 4;
                let t = st.txt_bits.add(o);
                let lum = ((*t.add(2)) as u32 + (*t.add(1)) as u32 + (*t.add(0)) as u32) as f32
                    / 3.0;
                acc += (lum / 255.0) * wgt;
            }
            if acc <= 0.004 {
                continue;
            }
            let bo = (py as usize * w + pxx as usize) * 4;
            if buf[bo + 3] == 0 {
                continue;
            }
            blend_over(buf, bo, cr as f32, cg as f32, cb as f32, acc * af);
        }
    }
}

// ───────────────────── 双击胶囊改名 ─────────────────────
//
// 交互链：双击外环胶囊 → 就地编辑（不弹任何浮层）→ Enter 落盘 / Esc 取消 /
// 空提交时**删掉这个条目**（和设置页的 ✕ 一致，但有二次确认的机会：Esc 随时可撤）。
//
// 为什么不用右键菜单：菜单要多一套命中/焦点/键盘可达性，而改名是唯一高频操作，
// 双击两下直达更短。代价是**不能只靠键盘**（键盘用户走设置页的「编辑」）。
//
// ⚠️ 中文：不用自己管 IME。窗口是**正常有焦点的 HWND**，系统会发
// `WM_IME_SETCONTEXT` / `WM_IME_COMPOSITION`，合成结果最终以 `WM_CHAR` /
// `WM_IME_CHAR` 形式进来 —— 照着追加就行。但**要主动把 IME 的候选窗挪到光标下面**
// （`ImmSetCompositionWindow`），否则中文候选框会飘到屏幕中央。

/// 双击的时间窗（毫秒）。取系统值但夹一下：太短会误触发，太长会跟「两次单击
/// 分别执行」冲突（单击是执行应用/关闭菜单，这里必须区别对待）。
const DBL_MS: f64 = 380.0;
/// 双击允许的最大位移（px）。手指抖一下就不算了。
const DBL_SLOP: i32 = 6;
/// 第一次单击后**等多久**才真的启动应用（毫秒）。必须略大于 `DBL_MS`。
/// 没有这个延迟，第一击就会启动应用 —— 双击等于「先打开应用再改名」，
/// 这正是 2026-10-07 用户报的问题（「双击后依然是单击的效果，会打开胶囊对应的应用」）。
const DBL_WAIT_MS: u32 = (DBL_MS as u32) + 70;

/// 点击胶囊的结果（决定调用方下一步做什么）。
#[derive(PartialEq)]
enum ChipClick {
    /// 命中胶囊，且这是**双击的第二击** → 进改名，**不要执行应用**
    Double,
    /// 命中胶囊，但这是**第一击** → 排一个延迟执行；返回时**不要**执行应用
    Defer,
}

/// 在 (ai, j) 这个胶囊上处理一次点击。
///
/// ⚠️ **第一击绝不能直接执行应用** —— 单击和双击在事件上无法区分，
/// 只能「都延后执行」，等第二击来了就取消执行、改为改名。
/// 返回 `None` = 没命中胶囊（或已在改名态），调用方走原来的单击逻辑。
fn chip_click(st: &mut State, ai: usize, j: usize, sx: i32, sy: i32) -> Option<ChipClick> {
    let now = now_s();
    let is_dbl = (now - st.edit_last_click) * 1000.0 <= DBL_MS
        && (sx - st.edit_last_pos.0).abs() <= DBL_SLOP
        && (sy - st.edit_last_pos.1).abs() <= DBL_SLOP;
    if is_dbl {
        st.edit_last_click = -99.0;
        if st.editing == Some((ai, j)) {
            // 已经在编辑 → 这次是「选中原文本」（第一次按键会替换它）
            st.edit_started = -1.0;
        } else {
            st.editing = Some((ai, j));
            st.edit_buf = st.apps[ai].items[j].name.clone();
            st.edit_started = 0.0;
        }
        st.edit_caret = now;
        EDIT_ARMED.store(true, Ordering::SeqCst);
        return Some(ChipClick::Double);
    }
    st.edit_last_click = now;
    st.edit_last_pos = (sx, sy);
    // 正在编辑别处时点别处 = 放弃（不改内容）
    if st.editing.is_some() {
        st.editing = None;
        st.edit_buf.clear();
        EDIT_ARMED.store(false, Ordering::SeqCst);
        return None;
    }
    Some(ChipClick::Defer)
}

/// 排一个「延迟单击」：先不执行，等 `DBL_WAIT_MS` 看有没有第二击。
/// 用 `SetTimer` 而不是 `std::thread::sleep` —— 后者会卡住 UI 线程，
/// 那正是注释里写过的「钩子装在 UI 线程会被渲染帧阻塞」同款错误。
fn arm_delayed_click(st: &mut State, hwnd: HWND, ai: usize, j: usize) {
    clear_delayed_click(st, hwnd);
    st.pending_click = Some((ai, j));
    unsafe { let _ = SetTimer(hwnd, TIMER_CLICK, DBL_WAIT_MS, None); }
}

/// 取消待执行的单击（第二击来了 → 改为改名；或菜单收起 → 一律作废）。
fn clear_delayed_click(st: &mut State, hwnd: HWND) {
    if st.pending_click.take().is_some() {
        unsafe { let _ = KillTimer(hwnd, TIMER_CLICK); }
        // 第一击时就已经把 loading 亮起来了（见点击处理），这里一并撤掉
        cancel_loading(st);
    }
}

/// 提交 / 取消编辑。落盘走 `save_settings`（它会先备份成 .bak）。
unsafe fn edit_finish(st: &mut State, commit: bool) {
    EDIT_ARMED.store(false, Ordering::SeqCst);
    let Some((ai, j)) = st.editing.take() else {
        return;
    };
    let buf = std::mem::take(&mut st.edit_buf);
    if !commit {
        return;
    }
    let newname = buf.trim();
    if ai >= st.apps.len() || j >= st.apps[ai].items.len() {
        return;
    }
    if newname.is_empty() {
        // 空提交 = 删除这个条目（与设置页 ✕ 同义）
        st.apps[ai].items.remove(j);
        println!("[rename] 空提交 → 删除条目 (app={} j={})", ai, j);
    } else if newname != st.apps[ai].items[j].name {
        let old = st.apps[ai].items[j].name.clone();
        st.apps[ai].items[j].name = newname.to_string();
        // 颜色是「按名字自动」的，改名后要失效重算（除非用户显式设过）
        if st.apps[ai].items[j].color.is_none() {
            st.apps[ai].items[j].color = None;
        }
        println!("[rename] {} -> {}", old, newname);
    } else {
        return;
    }
    save_settings(st);
}

/// 把一条字符追加进编辑缓冲（过滤控制字符）。
fn edit_push(st: &mut State, ch: char) {
    // 第一次输入时若处于「选中原文本」状态 → 丢弃旧文本
    if st.edit_started < 0.0 {
        st.edit_buf.clear();
        st.edit_started = 0.0;
    }
    if st.edit_buf.chars().count() < 24 && !ch.is_control() {
        st.edit_buf.push(ch);
    }
    st.edit_caret = now_s();
}

// ───────────────────── 拖放识别与入库 ─────────────────────

/// 画布坐标 → 命中 ai 应用的外环胶囊（返回 chip 序号）
fn hit_chip_at(st: &State, ai: usize, px: f32, py: f32) -> Option<usize> {
    // 与绘制共用同一套几何：**极坐标判定**（半径落在环带内、角度落在该段内）。
    // 梯形段是「两条直的径向边 + 内外弧」，极坐标判定与它天然吻合，
    // 比反旋到局部判矩形更准。⚠️ 只能有一份判定实现，否则形状一改就「画在这、点到那」。
    if ai >= st.apps.len() {
        return None;
    }
    let m = st.apps[ai].items.len();
    if m == 0 {
        return None;
    }
    let (segs, _rots, _rads) = chip_layout(st, ai);
    let (r_in, r_out) = ring_radii(st.apps.len());
    let dx = px - CC;
    let dy = py - CC;
    let r = (dx * dx + dy * dy).sqrt();
    let tol = 6.0;                       // 命中容差：比图形略松一点，好点
    if r < r_in - tol || r > r_out + tol {
        return None;
    }
    let th = dx.atan2(-dy).to_degrees(); // 与 `polar` 同一约定：0 = 正上、顺时针
    for (j, (a1, a2)) in segs.iter().enumerate() {
        // 角度落在该段内（首尾各给 4° 容差，好点）
        let mut d = th - (a1 + a2) * 0.5;
        d = ((d + 180.0).rem_euclid(360.0)) - 180.0;
        if d.abs() <= (a2 - a1) * 0.5 + 4.0 {
            return Some(j);
        }
    }
    None
}

/// **有效槽位数**：有幽灵时按 `n+1` 排布 —— 幽灵占一个**真实槽位**，其它图标整体让位。
/// （用户 2026-10-12：幽灵必须是"真实占位"，不能硬塞在两个图标中间。）
fn eff_apps(st: &State) -> usize {
    st.apps.len() + if st.drop_ghost.is_some() { 1 } else { 0 }
}

/// 画布坐标 → 最近的应用磁贴（拖放落点识别）
fn nearest_app(st: &State, px: f32, py: f32, slack: f32) -> Option<usize> {
    let n = eff_apps(st);
    if n == 0 {
        return None;
    }
    let dx = px - CC;
    let dy = py - CC;
    let (r_icon, icon_sz) = ring_layout(n);
    let hit_half = icon_sz * 0.5 + 7.0;
    if (dx * dx + dy * dy).sqrt() > r_icon + hit_half + slack + 4.0 {
        return None;
    }
    let deg = dx.atan2(-dy).to_degrees().rem_euclid(360.0);
    let i = app_at_deg(st, deg, n)?;
    // 中心距也要按**显示槽位**算（滚动 / 淡出内沉后图标并不在自己的下标角度上）
    let (ix, iy) = polar(r_icon - SCROLL_SINK * (1.0 - slot_appear(slot_of(st, i), n)),
        slot_angle(slot_of(st, i), n));
    let d = ((px - ix) * (px - ix) + (py - iy) * (py - iy)).sqrt();
    if d <= hit_half + slack {
        Some(i)
    } else {
        None
    }
}

/// 拖拽落点（内环）＝按**角度吸附**到最近的应用槽位。
/// 与 `nearest_app` 的关键区别：**不要求光标贴住某个图标** —— 落在两个图标之间的
/// **空隙**里也能命中（用户 2026-10-09：拖到两个图标中间「没反应、插不进去」，
/// 就是因为旧判定要求光标到图标中心 ≤ 图标半宽+7，缝隙处到两侧都太远 → 落空 → 当作放弃）。
/// 径向只要求在图标环带附近：太靠内（盘心）/ 太靠外 → 放弃。
/// 返回值 ∈ `0..=st.apps.len()`：`== st.apps.len()` 表示**幽灵槽**（要新建的那个）。
fn drop_slot_app(st: &State, px: f32, py: f32) -> Option<usize> {
    let n = eff_apps(st);
    if n == 0 {
        return None;
    }
    let dx = px - CC;
    let dy = py - CC;
    let r = (dx * dx + dy * dy).sqrt();
    let (r_icon, icon_sz) = ring_layout(n);
    if (r - r_icon).abs() > icon_sz * 0.5 + 18.0 {
        return None;
    }
    let deg = dx.atan2(-dy).to_degrees().rem_euclid(360.0);
    slot_at_deg(st, deg, n)
}

/// 拖拽落点（外环）＝按**角度吸附**到最近的外环段（允许落在段间的缝里）。
fn drop_slot_chip(st: &State, ai: usize, px: f32, py: f32) -> Option<usize> {
    if ai >= st.apps.len() || st.apps[ai].items.is_empty() {
        return None;
    }
    let (segs, _rots, _rads) = chip_layout(st, ai);
    let (r_in, r_out) = ring_radii(st.apps.len());
    let dx = px - CC;
    let dy = py - CC;
    let r = (dx * dx + dy * dy).sqrt();
    let tol = 10.0;
    if r < r_in - tol || r > r_out + tol {
        return None;
    }
    // 段中心角最接近光标的那个段（缝里也归到最近的一段）
    let th = dx.atan2(-dy).to_degrees();
    let mut best: Option<usize> = None;
    let mut best_d = f32::MAX;
    for (j, (a1, a2)) in segs.iter().enumerate() {
        let mut d = th - (a1 + a2) * 0.5;
        d = ((d + 180.0).rem_euclid(360.0)) - 180.0;
        if d.abs() < best_d {
            best_d = d.abs();
            best = Some(j);
        }
    }
    best
}

/// 拖放落点 → 归属应用：图标磁贴 > 外环胶囊 > 盘面以内（当前亮着的应用）
fn drop_force_app(st: &State, sx: i32, sy: i32) -> Option<usize> {
    let px = sx - st.win_x;
    let py = sy - st.win_y;
    if let Some(ai) = nearest_app(st, px as f32, py as f32, 8.0) {
        return Some(ai);
    }
    // 落在某个应用的外环胶囊上 → 归那个应用（按当前位置算，不是「当前亮着的」）
    for ai in 0..st.apps.len().min(MAX_APPS) {
        if hit_chip_at(st, ai, px as f32, py as f32).is_some() {
            return Some(ai);
        }
    }
    // ⚠️ 这里**不能**再回退到 `st.active_app`。
    // 它是粘性的：悬停过 A 之后光标挪到中心头像，active_app 仍是 A，
    // 于是「把东西拖到中间的球上松手」会被记进 A 的外环（用户实测踩到）。
    // 位置不在任何目标上就是不认 —— 交给 process_drop 的自动识别分支。
    None
}

/// 路径 → DropItem（文件夹/exe/lnk/普通文件）
fn drop_item_from_path(path: &str) -> DropItem {
    let is_dir = std::path::Path::new(path).is_dir();
    let stem = std::path::Path::new(path)
        .file_stem()
        .or_else(|| std::path::Path::new(path).file_name())
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| path.to_string());
    DropItem {
        name: clamp_name(&stem),
        target: path.to_string(),
        is_url: false,
        is_dir,
    }
}

/// 剪贴板兜底（环上按 V）：CF_HDROP → CF_UNICODETEXT（像链接才收）
unsafe fn clipboard_drop_items() -> Vec<DropItem> {
    let mut out: Vec<DropItem> = Vec::new();
    if OpenClipboard(HWND(ptr::null_mut())).is_err() {
        return out;
    }
    if let Ok(h) = GetClipboardData(15) {
        // CF_HDROP
        let hg = HGLOBAL(h.0);
        let n = DragQueryFileW(HDROP(hg.0), u32::MAX, None);
        for i in 0..n {
            let mut buf = [0u16; 520];
            let len = DragQueryFileW(HDROP(hg.0), i, Some(&mut buf)) as usize;
            let path = String::from_utf16_lossy(&buf[..len]);
            if !path.is_empty() {
                out.push(drop_item_from_path(&path));
            }
        }
    }
    if out.is_empty() {
        if let Ok(h) = GetClipboardData(13) {
            // CF_UNICODETEXT
            let hg = HGLOBAL(h.0);
            let p = GlobalLock(hg);
            if !p.is_null() {
                let ws = p as *const u16;
                let mut len = 0usize;
                while *ws.add(len) != 0 && len < 2048 {
                    len += 1;
                }
                let text = String::from_utf16_lossy(std::slice::from_raw_parts(ws, len));
                let _ = GlobalUnlock(hg);
                let text = text.trim().to_string();
                if text.starts_with("http://") || text.starts_with("https://") {
                    out.push(DropItem {
                        name: url_host_name(&text),
                        target: text,
                        is_url: true,
                        is_dir: false,
                    });
                }
            }
        }
    }
    let _ = CloseClipboard();
    out
}

/// 拖动预览：被拖的那一项从原槽位抽出、插到 `ins`，返回**应用 i 在预览排序里的槽位**。
///
/// 为什么需要它：主流重排序（iOS 主屏 / 任务栏 / Trello）在拖动时**图标会让位开缝**，
/// 光标（插入线）落在缝里 —— 而不是「指着某个已有条目」。用户 2026-10-14 明确要求。
/// 于是 `drag_over_app` 这个「插入索引 t」必须翻译成每个图标的**显示槽位**：
/// 原槽位 `ai` 空出来（那就是缝），`ai` 之后、`t` 之前的整体前移一格。
fn drag_preview_slot(ai: usize, ins: usize, i: usize) -> usize {
    if i == ai {
        return ins;
    }
    let p = if i > ai { i - 1 } else { i };
    if p < ins {
        p
    } else {
        p + 1
    }
}

/// 当前**真正聚焦**的应用：光标在内环某个图标上、或在该应用的外环胶囊上、
/// 或拖放正悬在某个落点上。
/// ⚠️ 绝对不能直接拿 `active_app` 当焦点 —— 它是**粘性**的（为了「移向外环不丢选中」故意不清空）。
/// 光标挪到中心头像或盘面空白后它依然非空，于是其它应用的压暗遮罩会一直挂着不散：
/// 用户看到的就是「hover 取消了，但别的应用还蒙着一层灰」。
fn focus_app(st: &State) -> Option<usize> {
    if let Some(i) = st.drop_target_app {
        return Some(i);
    }
    if let Some(i) = st.hover_now {
        return Some(i);
    }
    if st.hover_chip.is_some() {
        return st.active_app;
    }
    None
}

/// 环上按 V：把剪贴板里的文件/链接加入环（拖放的兜底通道）
unsafe fn paste_add(st: &mut State) {
    let items = clipboard_drop_items();
    if items.is_empty() {
        return;
    }
    let force = focus_app(st); // 真的悬停在哪就加进谁；没悬停则自动识别
    let n = process_drop(st, &items, force);
    if n > 0 {
        st.commit_pulse = 1.0;
        // 与拖放入环同款：消化新成员时短暂「忙碌」
        set_mood(st, 5);
        st.mood_hold = now_s() + 1.5;
    }
    println!("[paste] 入环 {} 项", n);
}

/// 载荷 → **宿主应用 exe**（权威版，取代旧启发式）：
///   目录 → `explorer.exe`；网址 → 系统默认浏览器；exe/lnk → 自身；其它文件 → 按扩展名关联。
/// 认不出返回 None（→ 幽灵=载荷自身，直接成为顶层应用）。
unsafe fn resolve_host_exe(it: &DropItem) -> Option<String> {
    if it.is_dir {
        return Some("explorer.exe".to_string());
    }
    if it.is_url {
        return default_browser_exe();
    }
    if exe_name_of(&it.target).is_some() {
        // .exe / .lnk → 自身的实际目标
        return resolve_target_exe(&it.target).or_else(|| Some(it.target.clone()));
    }
    // 普通文件 → 按扩展名查默认关联程序（如 .zip 装了 7-Zip 就是它；没装 → None）
    let ext = std::path::Path::new(&it.target)
        .extension()
        .map(|s| s.to_string_lossy().to_ascii_lowercase())?;
    if ext.is_empty() {
        return None;
    }
    assoc_exe(&format!(".{}", ext))
}

/// 宿主 exe → 内环里对应应用的索引（按可执行文件名比较，忽略路径）。
/// 只有"认得出来"才高亮那个内环，引导用户往上丢。
fn host_app_index(st: &State, host_exe: &str) -> Option<usize> {
    let want = exe_name_of(host_exe)?;
    st.apps
        .iter()
        .position(|a| exe_name_of(&a.target).as_deref() == Some(want.as_str()))
}

/// target 打开的是不是一个文件夹（`shell:` 文件夹命名空间，或真实目录路径）。
fn target_opens_folder(target: &str) -> bool {
    let t = target.trim();
    if t.starts_with("shell:") {
        // shell: 文件夹命名空间（UsersFilesFolder / Downloads / Personal…）。
        // ⚠️ 不走 `shell_ns_to_path`：它没收录 UsersFilesFolder（默认 hub 就是它）。
        return true;
    }
    if t.contains("://") || t.starts_with('@') {
        return false; // URL / 内置动作（@act:start）不是文件夹
    }
    std::path::Path::new(t).is_dir()
}

/// 内环里的「文件夹 hub」索引 —— 拖进来的文件夹应归它，而不是另开一个 explorer 应用。
/// 判据：target 是 `shell:` 文件夹命名空间（默认「文件夹」=shell:UsersFilesFolder），
/// 或它已聚合了文件夹条目（像默认 hub 那样挂了下载/文档）。
/// ⚠️ 刻意**不**把「裸的顶层文件夹应用」（target 就是一个目录、没有条目）算作 hub，
///    否则用户拖一个新文件夹会被塞进某个不相干目录的外环里。
fn folder_hub_index(st: &State) -> Option<usize> {
    st.apps.iter().position(|a| {
        a.target.trim().starts_with("shell:")
            || a.items.iter().any(|it| target_opens_folder(&it.target))
    })
}

/// 是不是资源管理器（explorer.exe）——它承载的是**文件夹窗口**，
/// 拖这种窗口进环应归「文件夹」hub，而不是另开一个 explorer 内环。
fn is_explorer_exe(exe: &str) -> bool {
    exe_name_of(exe).as_deref() == Some("explorer.exe")
}

/// 当前拖拽会话「已算过预览的载荷签名」。同一会话里 DragOver 每 3px 就跑一次
/// `compute_drop_preview`，而 .lnk 载荷每次都要 `resolve_host_exe`(COM 解析快捷方式)
/// + `decode_icon`(~76ms) + `host_app_index`(逐 app 再解析 .lnk) —— 一次拖拽上百次
/// 会把 UI 拖死（用户 2026-10-18：「拖动添加特别卡、添加不上」）。载荷在同一次拖拽里
/// 不变 → 命中就直接复用上次结果。DragEnter / 离开 / 落下时失效。
static PREVIEW_KEY: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

fn preview_key(items: &[DropItem]) -> String {
    match items.first() {
        None => String::new(),
        Some(it) => format!(
            "{}|{}|{}|{}",
            items.len(),
            it.target,
            it.is_dir as u8,
            it.name
        ),
    }
}

/// 拖入时算「建议目标 / 幽灵」：认得出宿主内环 → 高亮它；认不出 → 出幽灵。
/// 结果存进 `st.drop_suggest` / `st.drop_ghost`，供渲染与落点判定用。
fn compute_drop_preview(st: &mut State, items: &[DropItem]) {
    let key = preview_key(items);
    {
        let mut c = PREVIEW_KEY.lock().unwrap_or_else(|e| e.into_inner());
        if !key.is_empty() && c.as_deref() == Some(key.as_str()) {
            return; // 同一会话、载荷未变 → 复用上次预览
        }
        *c = Some(key);
    }
    compute_drop_preview_body(st, items);
}

/// 拖拽会话边界（进入/离开/落下）→ 失效预览缓存。
fn drop_preview_invalidate() {
    *PREVIEW_KEY.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

fn compute_drop_preview_body(st: &mut State, items: &[DropItem]) {
    st.drop_suggest = None;
    st.drop_ghost = None;
    let Some(it) = items.first() else {
        return;
    };
    // 拖**文件夹**：优先归到已有的「文件夹」hub（默认 target=shell:UsersFilesFolder）
    // → 高亮它，丢上去就成为它外环里的一个文件夹条目。
    // ⚠️ 之前这里落到通用 host 分支：`resolve_host_exe` 把目录当 explorer.exe，
    //    而 `host_app_index` 只按 exe 名比 → `shell:` hub 永远匹配不上
    //    → 另开一个 explorer 内环（用户 2026-10-18 报「跟默认文件夹不是一个内环图标」）。
    if it.is_dir {
        if let Some(ai) = folder_hub_index(st) {
            st.drop_suggest = Some(ai);
            return;
        }
        // 没有文件夹 hub → 幽灵 = 这个文件夹本身（新建顶层应用打开它）
        let icon = unsafe { decode_icon(&it.target) }.map(|(px, d)| IconTex::from_base(px, d));
        if st.apps.len() < MAX_APPS {
            st.drop_ghost = Some(GhostApp {
                name: it.name.clone(),
                target: it.target.clone(),
                put_items: false,
                icon,
            });
        }
        return;
    }
    // 载荷本身就是应用（exe/lnk）→ 也浮一个**图标占位符**（与底下「认不出宿主」同款幽灵）：
    // 拖动中就能看见「会落在哪」，松手 `create_from_ghost` 把它变实体（2026-10-18：
    // 原 early-return 无任何占位符，用户拖 app 进环时看不到落点）。已在内环则不出（落下报「已在环上」）。
    if exe_name_of(&it.target).is_some() {
        if let Some(exe_s) = unsafe { resolve_host_exe(it) } {
            let dup = host_app_index(st, &exe_s).is_some();
            if st.apps.len() < MAX_APPS && !dup {
                let name = exe_name_of(&exe_s)
                    .map(|b| clamp_name(b.trim_end_matches(".exe")))
                    .unwrap_or_else(|| it.name.clone());
                let icon =
                    unsafe { decode_icon(&exe_s) }.map(|(px, d)| IconTex::from_base(px, d));
                st.drop_ghost = Some(GhostApp {
                    name,
                    target: it.target.clone(),
                    put_items: false,
                    icon,
                });
            }
        } else if st.apps.len() < MAX_APPS {
            // 解析不出宿主（罕见）→ 直接以载荷自身为应用
            let icon =
                unsafe { decode_icon(&it.target) }.map(|(px, d)| IconTex::from_base(px, d));
            st.drop_ghost = Some(GhostApp {
                name: it.name.clone(),
                target: it.target.clone(),
                put_items: false,
                icon,
            });
        }
        return;
    }
    let host_dbg = unsafe { resolve_host_exe(it) };
    drag_log(&format!(
        "preview tgt={} host={:?} hostix={:?}",
        it.target,
        host_dbg,
        host_dbg.as_deref().and_then(|h| host_app_index(st, h)),
    ));
    match host_dbg {
        Some(exe) => {
            if let Some(ai) = host_app_index(st, &exe) {
                // 认得宿主 → 高亮那个内环，引导用户往上丢
                st.drop_suggest = Some(ai);
            } else if st.apps.len() < MAX_APPS {
                // 宿主不在内环 → 幽灵=新建这个宿主应用，载荷进它的外环
                let name = exe_name_of(&exe)
                    .map(|b| clamp_name(b.trim_end_matches(".exe")))
                    .unwrap_or_else(|| it.name.clone());
                let icon = unsafe { decode_icon(&exe) }.map(|(px, d)| IconTex::from_base(px, d));
                st.drop_ghost = Some(GhostApp { name, target: exe, put_items: true, icon });
            }
        }
        None => {
            // 认不出宿主 → 幽灵=载荷自身，直接成为顶层应用
            if st.apps.len() < MAX_APPS {
                let icon =
                    unsafe { decode_icon(&it.target) }.map(|(px, d)| IconTex::from_base(px, d));
                st.drop_ghost = Some(GhostApp {
                    name: it.name.clone(),
                    target: it.target.clone(),
                    put_items: false,
                    icon,
                });
            }
        }
    }
}

/// 幽灵磁贴的位置（画布坐标）：**它就是"多出来的那个槽位"** ——
/// 整个环按 `n+1` 排布，幽灵站在索引 `n` 的位置，其它图标各自让位到新角度上。
fn ghost_pos(st: &State) -> (f32, f32) {
    let n_ap = st.apps.len();
    let n = n_ap + 1; // 画幽灵时必然按 n+1 排
    let (r_icon, _sz) = ring_layout(n);
    let s = slot_of(st, n_ap);
    polar(r_icon - SCROLL_SINK * (1.0 - slot_appear(s, n)), slot_angle(s, n))
}


/// 丢在幽灵上 = 建这个内环：
///   `put_items` → 先建宿主应用，再把载荷放进它的外环；
///   否则 → 载荷自身成为顶层应用（逐个去重 + 上限）。
fn create_from_ghost(st: &mut State, items: &[DropItem], g: &GhostApp) -> usize {
    if g.put_items {
        ini_insert_app(&st.ini_path, &g.name, &g.target);
        unsafe { load_ini(st) };
        match host_app_index(st, &g.target) {
            Some(ai) => process_drop(st, items, Some(ai)),
            None => 0,
        }
    } else {
        let mut added = 0;
        for it in items {
            if st.apps.iter().any(|a| a.target.eq_ignore_ascii_case(&it.target)) {
                continue; // 去重
            }
            if st.apps.len() >= MAX_APPS {
                break;
            }
            if ini_insert_app(&st.ini_path, &it.name, &it.target) {
                added += 1;
            }
        }
        if added > 0 {
            unsafe { load_ini(st) };
        }
        added
    }
}

/// 入环：URL → 浏览器外环（推荐）或落点应用；文件夹 → 资源管理器外环；
/// exe/lnk/文件 → 新增进常用。落到哪个图标上 = 强制加进谁。
/// 返回新增条数。入库即回写 ini 并全量热重载。
fn process_drop(st: &mut State, items: &[DropItem], force_app: Option<usize>) -> usize {
    let mut added = 0usize;
    // 内环**数据**上限撞到了（一屏装不下的部分靠滚轮看，但总数不能再加）
    let mut full = false;
    for it in items {
        drag_log(&format!(
            "item [{}] target={} force={:?}",
            it.name, it.target, force_app
        ));
        let dest: Option<usize> = if let Some(ai) = force_app {
            if ai < st.apps.len() { Some(ai) } else { None }
        } else if it.is_url {
            // 推荐：目标本身是 URL 或已有 URL 条目的应用（= 浏览器）
            st.apps
                .iter()
                .position(|a| a.target.contains("://") || a.items.iter().any(|x| x.target.contains("://")))
        } else if it.is_dir {
            // 推荐：已有的文件夹 hub（target=shell:UsersFilesFolder 的默认「文件夹」）
            // → 归它外环；没有 hub 才退到资源管理器（target 或已有条目是目录）
            folder_hub_index(st).or_else(|| {
                st.apps.iter().position(|a| {
                    a.target.eq_ignore_ascii_case("explorer.exe")
                        || a.items.iter().any(|x| std::path::Path::new(&x.target).is_dir())
                })
            })
        } else {
            None
        };

        match dest {
            Some(ai) => {
                let app = &st.apps[ai];
                if app.items.iter().any(|x| x.target.eq_ignore_ascii_case(&it.target)) {
                    drag_log(&format!("  skip dup -> [{}]", app.name));
                    continue; // 已存在
                }
                if app.items.len() >= MAX_ITEMS {
                    drag_log(&format!(
                        "  skip FULL ({}/{} items) -> [{}]",
                        app.items.len(),
                        MAX_ITEMS,
                        app.name
                    ));
                    continue; // 外环满了
                }
                let app_name = app.name.clone();
                if ini_insert_item(&st.ini_path, &app_name, &it.name, &it.target) {
                    added += 1;
                }
            }
            None => {
                if st.apps.iter().any(|a| a.target.eq_ignore_ascii_case(&it.target)) {
                    drag_log("  skip dup (top-level)");
                    continue;
                }
                if st.apps.len() >= MAX_APPS {
                    drag_log("  skip FULL apps");
                    full = true;
                    continue;
                }
                if ini_insert_app(&st.ini_path, &it.name, &it.target) {
                    added += 1;
                }
            }
        }
    }
    drag_log(&format!("  added={}", added));
    if added > 0 {
        // ini 已更新，全量热重载（重新解析 + 重新解码图标）
        unsafe { load_ini(st) };
        // onboarding：只要真加进去了，① 「拖入」就算学过；
        // 落点命中某个已有应用（进它的外环）→ ②「拖外环条目」也算学过
        // （不管是不是 force 的：命中即说明用户已经会往环里丢了）。
        onb_done_drag(st);
    }
    if full {
        // 撞上限必须说一声 —— 以前只是 `drag_log`，用户的观感是「拖进去没反应」
        say(st, "内环装不下了", &format!("最多 {} 个 · 先移除再加", MAX_APPS), 6.0);
    }
    added
}

/// 出厂默认（读不到任务栏时的兜底）—— 只用**系统自带且一定存在**的东西。
// 出厂默认已改为固定清单（见 `default_apps`），不再需要任务栏兜底。

/// 出厂默认应用：**发挥径向菜单优势，不放任务栏的复制品**。
///
/// 只放「高频 + 触达成本高（需导航/输入/多步）+ 用户不会主动钉常驻」的东西：
/// 文件夹（聚合下载/文档等 → 外环）、浏览器（一圈点开常用 AI / 新标签），开始菜单兜底入口。
/// 「文件夹」本体 = 打开用户文件夹（下载/文档的父级，`shell:` 协议 → 每次新开一个窗口，
/// 不走 `find_window_of_exe` 去聚焦已有的资源管理器进程）；外环才是下载/文档这些具体位置。
/// URL 与 `shell:` 都交给 `ShellExecuteW` 处理（默认浏览器 / 资源管理器自动接管），
/// 无需自行定位浏览器 exe。
fn default_apps() -> Vec<(String, String)> {
    vec![
        ("文件夹".to_string(), "shell:UsersFilesFolder".to_string()),
        ("浏览器".to_string(), "https://www.bing.com".to_string()),
        ("开始菜单".to_string(), "@act:start".to_string()),
    ]
}

/// 当前 `[apps]` 是否已被用户改过（≠ 出厂默认）。
///
/// ⚠️ 基准必须和 `default_ini()` 用**同一份** `default_apps()` ——
/// 早先这里比的是硬编码的 8 个 target，默认值一改成任务栏固定项，就永远判"改过"，
/// 新用户会直接跳过「拖进来添加」那步引导。
fn apps_customized(st: &State) -> bool {
    if st.apps.is_empty() {
        return false;
    }
    let base = default_apps();
    if st.apps.len() != base.len() {
        return true;
    }
    base.iter()
        .any(|(_, t)| !st.apps.iter().any(|a| a.target.eq_ignore_ascii_case(t)))
}

/// onboarding ① 完成：用户**真的**把东西拖进环里了。
/// ⚠️ 置位条件是"动作本身"，**不是**"气泡播过" —— 所以槽位是 self-clearing 的，
///    不需要任何「已读」状态机（用户 2026-10-14 的设计核心）。
fn onb_done_drag(st: &mut State) {
    if !st.onb_drag {
        st.onb_drag = true;
        unsafe { save_settings(st) };
    }
}

/// onboarding 气泡：**每次呼出挑一条该说的**（`show_menu` 里调）。
///
/// ⚠️ ① 不在这里置位 —— 它的完成条件是"用户**真的**拖入成功"（`onb_done_drag`），
///    所以在他拖之前，每次呼出都会再说一遍（和虚线槽位一直挂着是同一件事的两面：
///    槽位负责"看得见"，气泡负责"说得清"）。
/// ⚠️ ③④ 没有对应的动作可等（等用户去拖 = 永远等不到），所以播过即完成并落盘。
fn onb_say(st: &mut State) {
    if !st.onb_drag && !st.onb_customized {
        say(st, "拖文件或窗口到这里", "应用 · 文件夹 · 网址都行", ONB_SECS);
    } else if !st.onb_move {
        say(st, "松手换位", "拖到盘外 = 移除", ONB_SECS);
        st.onb_move = true;
        unsafe { save_settings(st) };
    } else if st.onb_move && !st.onb_edit {
        say(st, "双击可改名", "Enter 保存 · Esc 取消", ONB_SECS);
        st.onb_edit = true;
        unsafe { save_settings(st) };
    } else if st.apps.len() > RING_SLOTS && !st.onb_scroll {
        // 只有**真的超过一屏**才提这条（否则提了也没东西可滚）
        say(st, "超过一屏了", "滚轮可以连续滚", ONB_SECS);
        st.onb_scroll = true;
        unsafe { save_settings(st) };
    }
}

/// 后台检查发现新版本 → 下一次呼出轮盘时用头像气泡提醒**一次**。
///
/// 「每个版本最多提醒一次」靠 ini 的 `update_bub_ver` 记账：**说出口那一刻就落盘**，
/// 所以哪怕这次没点、之后重启了，同一个版本也不会再提醒第二遍。
/// ⚠️ 只在 `WM_APP_SHOW` 真实呼出后调（排在 `show_menu` 里的 `onb_say` 之后，
///    教学气泡占着气泡位时不抢话）；放进 `show_menu` 会被启动「亮个相」那声 `say` 顶掉。
fn update_say(st: &mut State) {
    let v = st.update_avail_ver.clone();
    if v.is_empty() || v == st.update_bub_ver || st.bub_until > now_s() {
        return;
    }
    // 只报版本号，不带「去哪更新」那行小字（用户 2026-10-27：一行就够，别啰嗦）
    say(st, &format!("发现新版本 v{v}"), "", UPD_BUB_SECS);
    st.update_bub_ver = v.clone();
    unsafe { save_settings(st) };
    // 一行日志 = 这条提醒**真的说出口了**（回归靠它数次数：每个版本只能有一条）
    println!("[upd] 头像气泡提醒 · 新版本 v{v}");
}


/// 把 `name=target` 插入 [app.<app>] 段（无段则建段）。成功返回 true。
fn ini_insert_item(path: &str, app: &str, key: &str, value: &str) -> bool {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let header = format!("[app.{}]", app);
    let line = format!("{}={}", key, value);
    let mut out = String::new();
    if let Some(pos) = text.find(&header) {
        // 段尾 = 下一个段头或 EOF
        let after = &text[pos + header.len()..];
        let next_sec = after
            .find("\n[")
            .map(|i| pos + header.len() + i + 1)
            .unwrap_or(text.len());
        out.push_str(&text[..next_sec]);
        // 规范上一行结尾
        if !out.ends_with('\n') {
            out.push_str("\r\n");
        }
        out.push_str(&line);
        out.push_str("\r\n");
        out.push_str(&text[next_sec..]);
    } else {
        out.push_str(text.trim_end());
        if !out.is_empty() {
            out.push_str("\r\n");
        }
        out.push_str(&header);
        out.push_str("\r\n");
        out.push_str(&line);
        out.push_str("\r\n");
    }
    std::fs::write(path, out).is_ok()
}

/// 把 `name=target` 追加进 [apps] 段。成功返回 true。
fn ini_insert_app(path: &str, key: &str, value: &str) -> bool {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let line = format!("{}={}", key, value);
    let mut out = String::new();
    if let Some(pos) = text.find("[apps]") {
        let after = &text[pos + 6..];
        let next_sec = after
            .find("\n[")
            .map(|i| pos + 6 + i + 1)
            .unwrap_or(text.len());
        out.push_str(&text[..next_sec]);
        if !out.ends_with('\n') {
            out.push_str("\r\n");
        }
        out.push_str(&line);
        out.push_str("\r\n");
        out.push_str(&text[next_sec..]);
    } else {
        out.push_str("[apps]\r\n");
        out.push_str(&line);
        out.push_str("\r\n");
        out.push_str(text.trim_start());
    }
    std::fs::write(path, out).is_ok()
}

// ─────────────── 窗口捕获（拖窗口进环） ───────────────
// 拖一个窗口的标题栏丢进环 → 它成为「宿主应用」的**一条外环条目**（kind=1）。
// 点开：窗口还活着 → 聚焦；已关 → 用捕获到的命令行重开。条目落盘（存 relaunch）。
type NtQip = unsafe extern "system" fn(*mut c_void, u32, *mut c_void, u32, *mut usize) -> i32;

/// 被拖动中的窗口：`GetGUIThreadInfo().hwndMoveSize`（需 AttachThreadInput 到前台线程才准）。
unsafe fn moving_window() -> Option<HWND> {
    use windows::Win32::UI::WindowsAndMessaging::{GetGUIThreadInfo, GUITHREADINFO};
    use windows::Win32::System::Threading::AttachThreadInput;
    let fg = GetForegroundWindow();
    if fg.0.is_null() {
        return None;
    }
    let fg_tid = GetWindowThreadProcessId(fg, None);
    let my_tid = GetCurrentThreadId();
    let attached = fg_tid != 0
        && fg_tid != my_tid
        && AttachThreadInput(my_tid, fg_tid, true).as_bool();
    let mut gi: GUITHREADINFO = std::mem::zeroed();
    gi.cbSize = std::mem::size_of::<GUITHREADINFO>() as u32;
    let ok = GetGUIThreadInfo(0, &mut gi).is_ok();
    if attached {
        let _ = AttachThreadInput(my_tid, fg_tid, false);
    }
    if ok && !gi.hwndMoveSize.0.is_null() {
        Some(gi.hwndMoveSize)
    } else {
        None
    }
}

/// 「松手」那一瞬的光标（`pack_pt`；0 = 没有记录）。见 [`moving_window_throttled`]。
static MOVE_RELEASE_PT: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);

/// 光标当前位置：优先用钩子写进原子量的值；钩子没装（停止监听）或还没收到过
/// 任何一次移动（原子量还是 0）→ 直接问系统。`WM_APP_SHOW` / `WM_APP_HOVER`
/// 早就是这么兜底的，窗口捕获的落点判定也必须一致 —— 否则原子量停在 (0,0)，
/// 「拖进环」永远判成没落盘。
unsafe fn cursor_now() -> (i32, i32) {
    let (x, y) = (
        CURSOR_X.load(Ordering::Relaxed),
        CURSOR_Y.load(Ordering::Relaxed),
    );
    if x == 0 && y == 0 {
        let mut p = POINT::default();
        let _ = GetCursorPos(&mut p);
        (p.x, p.y)
    } else {
        (x, y)
    }
}

/// `moving_window()` 的节流 + 去抖版。三条硬规则（2026-10-26 修「呼出轮盘后拖浏览器巨卡」）：
///  * **一律 ≤15Hz**（原来「没在拖时每帧都查」）：`moving_window` 要 `AttachThreadInput`
///    挂到**前台线程**（拖浏览器时就是浏览器的 UI 线程）再取 `hwndMoveSize`。挂/摘本身
///    要打扰那个线程，60fps 反复做 = 每帧给浏览器插一刀 —— 用户报的「拖窗口掉帧」正是它。
///  * **左键没按下 = 不可能在拖窗口** → 一次都不查（零 attach）。
///    拖标题栏全程按着左键，这是免费且绝对可靠的前置门。
///  * **连续 2 次 None 才算松手**：单帧 `hwndMoveSize` 为空常常只是被拖线程 momentarily
///    忙（弹菜单、切进程），以前一 None 就判定松手 → 拖到半路预览消失、落点误判。
///    第一次 None 时**快照当时光标**当松手点（`release_point`）：去抖多等的这 ~130ms
///    里手可能已经移开，用快照才等于用户真实的落点。
fn moving_window_throttled() -> Option<HWND> {
    use std::sync::atomic::{AtomicIsize, AtomicU32};
    static AT: AtomicU32 = AtomicU32::new(0);
    static RES: AtomicIsize = AtomicIsize::new(0);
    static MISS: AtomicU32 = AtomicU32::new(0);
    /// ≈15Hz
    const PERIOD: u32 = 66;
    /// 连续这么多次 None 才承认松手
    const KEEP: u32 = 2;

    let now = unsafe { windows::Win32::System::SystemInformation::GetTickCount() };
    let prev = RES.load(Ordering::Relaxed);
    if now.wrapping_sub(AT.load(Ordering::Relaxed)) < PERIOD {
        // 未到复查间隔：沿用上次的结论
        return if prev != 0 { Some(HWND(prev as *mut c_void)) } else { None };
    }
    if prev == 0 {
        let ldown = unsafe {
            windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState(0x01) as u16 & 0x8000 != 0
        };
        if !ldown {
            // 没人按着左键 → 连 attach 都省了，只推进时间戳
            AT.store(now, Ordering::Relaxed);
            return None;
        }
    }
    AT.store(now, Ordering::Relaxed);
    let r = unsafe { moving_window() };
    match r {
        Some(h) => {
            MISS.store(0, Ordering::Relaxed);
            MOVE_RELEASE_PT.store(0, Ordering::Relaxed);
            RES.store(h.0 as isize, Ordering::Relaxed);
            Some(h)
        }
        None => {
            let was_dragging = prev != 0;
            if was_dragging {
                let m = MISS.fetch_add(1, Ordering::Relaxed) + 1;
                if m < KEEP {
                    // 还在去抖窗口内：对外仍然算「正在拖」
                    return Some(HWND(prev as *mut c_void));
                }
                MISS.store(0, Ordering::Relaxed);
                // 松手点快照：落点判定用它，不用去抖之后的当前光标
                let (rx, ry) = unsafe { cursor_now() };
                MOVE_RELEASE_PT.store(pack_pt(rx, ry), Ordering::Relaxed);
            }
            RES.store(0, Ordering::Relaxed);
            None
        }
    }
}

/// 取走（并清空）最近一次「松手」时的光标位置。None = 没有记录（调用方用当前光标）。
fn release_point() -> Option<(i32, i32)> {
    let v = MOVE_RELEASE_PT.swap(0, Ordering::Relaxed);
    if v == 0 {
        None
    } else {
        Some(unpack_pt(v))
    }
}

/// 窗口标题（GetWindowTextW）
unsafe fn window_title(hwnd: HWND) -> String {
    let mut buf = [0u16; 260];
    let n = GetWindowTextW(hwnd, &mut buf);
    if n > 0 {
        String::from_utf16_lossy(&buf[..n as usize])
    } else {
        String::new()
    }
}

/// 窗口类名（`CabinetWClass` / `Chrome_WidgetWin_1` 那些）
unsafe fn window_class(hwnd: HWND) -> String {
    let mut buf = [0u16; 260];
    let n = GetClassNameW(hwnd, &mut buf);
    if n > 0 {
        String::from_utf16_lossy(&buf[..n as usize])
    } else {
        String::new()
    }
}

/// 窗口所属进程 exe（小写全路径）
unsafe fn window_exe(hwnd: HWND) -> Option<String> {
    let mut pid = 0u32;
    GetWindowThreadProcessId(hwnd, Some(&mut pid));
    if pid == 0 {
        return None;
    }
    process_image(pid)
}

/// 进程命令行（重开用）。走 `NtQueryInformationProcess` 的
/// `ProcessCommandLineInformation`(60) —— 一次返回 UNICODE_STRING，无需读 PEB。
/// ntdll 没有稳定 Rust 绑定 → 动态取地址。
unsafe fn process_command_line(pid: u32) -> Option<String> {
    use windows::Win32::System::Threading::{
        OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_VM_READ,
    };
    use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
    use windows::core::PCSTR;
    let h = OpenProcess(
        PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_VM_READ,
        false,
        pid,
    )
    .ok()?;
    let ntdll = match GetModuleHandleW(PCWSTR(wide("ntdll").as_ptr())) {
        Ok(h) => h,
        Err(_) => return None,
    };
    let res = (|| -> Option<String> {
        let name = b"NtQueryInformationProcess\0".to_vec();
        let f = match GetProcAddress(ntdll, PCSTR(name.as_ptr())) {
            Some(f) => f,
            None => return None,
        };
        let ntqip: NtQip = std::mem::transmute(f);
        const PROCESS_CMDLINE: u32 = 60;
        let mut buf = [0u8; 8192];
        let mut ret: usize = 0;
        // 返回 UNICODE_STRING（Length/MaximumLength/Buffer），命令行数据紧跟结构体之后。
        let status = ntqip(
            h.0 as *mut c_void,
            PROCESS_CMDLINE,
            buf.as_mut_ptr() as *mut c_void,
            buf.len() as u32,
            &mut ret,
        );
        if status != 0 || ret < 8 {
            return None;
        }
        let len = u16::from_ne_bytes([buf[0], buf[1]]) as usize;
        if len < 2 {
            return None;
        }
        let buf_ptr = usize::from_ne_bytes([
            buf[8], buf[9], buf[10], buf[11], buf[12], buf[13], buf[14], buf[15],
        ]) as *const u16;
        let base = buf.as_ptr() as usize;
        let end = base + buf.len();
        // Buffer 必须落在我们给的输出缓冲内才安全可读（ProcessCommandLineInformation 保证如此）
        if (buf_ptr as usize) < base || (buf_ptr as usize) + len > end {
            return None;
        }
        let s = std::slice::from_raw_parts(buf_ptr, len / 2);
        Some(String::from_utf16_lossy(s))
    })();
    let _ = CloseHandle(h);
    res
}

/// 用命令行重开窗口（CreateProcess；失败退化为 ShellExecute 单文件）
unsafe fn spawn_cmd(cmd: &str) -> bool {
    use windows::Win32::System::Threading::{
        CreateProcessW, PROCESS_INFORMATION, STARTUPINFOW, PROCESS_CREATION_FLAGS,
    };
    let mut cmd16: Vec<u16> = wide(cmd);
    let mut pi = PROCESS_INFORMATION::default();
    let mut si = STARTUPINFOW::default();
    si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
    match CreateProcessW(
        PCWSTR::null(),
        PWSTR(cmd16.as_mut_ptr()),
        None,
        None,
        false,
        PROCESS_CREATION_FLAGS(0),
        None,
        PCWSTR::null(),
        &si,
        &mut pi,
    ) {
        Ok(_) => {
            let _ = CloseHandle(pi.hProcess);
            let _ = CloseHandle(pi.hThread);
            true
        }
        Err(_) => {
            // ⚠️ 退化时**不能把整串塞给 lpFile**：带参数 + 引号的命令行会被整体当成
            //    文件名解析失败（静默不开窗）。必须拆成「程序」+「参数」两个槽位。
            let (file, args) = split_cmd(cmd);
            let f = wide(&file);
            let a = wide(&args);
            let _ = ShellExecuteW(
                HWND(ptr::null_mut()),
                PCWSTR::null(),
                PCWSTR(f.as_ptr()),
                if args.is_empty() {
                    PCWSTR::null()
                } else {
                    PCWSTR(a.as_ptr())
                },
                PCWSTR::null(),
                SW_SHOWNORMAL,
            );
            false
        }
    }
}

/// `程序 参数…` → (程序, 参数)。程序带空格时命令行里是加引号的，要按引号切。
fn split_cmd(cmd: &str) -> (String, String) {
    let t = cmd.trim();
    if let Some(s) = t.strip_prefix('"') {
        return match s.find('"') {
            Some(i) => (s[..i].to_string(), s[i + 1..].trim().to_string()),
            None => (t.trim_matches('"').to_string(), String::new()),
        };
    }
    match t.find(' ') {
        Some(i) => (t[..i].to_string(), t[i + 1..].trim().to_string()),
        None => (t.to_string(), String::new()),
    }
}

/// 按「exe + 标题」找一个还活着的活动窗口（restamp / 去重校验用）
unsafe fn find_window_for_capture(exe: &str, title: &str) -> Option<HWND> {
    struct WinCap {
        want_exe: String,
        want_title: String,
        found: Option<HWND>,
    }
    extern "system" fn cb(hwnd: HWND, lp: LPARAM) -> BOOL {
        unsafe {
            let s = &mut *(lp.0 as *mut WinCap);
            if s.found.is_some() {
                return BOOL(0);
            }
            if IsWindowVisible(hwnd).as_bool() {
                let mut pid = 0u32;
                GetWindowThreadProcessId(hwnd, Some(&mut pid));
                if let Some(pe) = process_image(pid) {
                    if pe == s.want_exe {
                        let t = window_title(hwnd).to_ascii_lowercase();
                        if t.contains(s.want_title.as_str()) {
                            s.found = Some(hwnd);
                            return BOOL(0);
                        }
                    }
                }
            }
            BOOL(1)
        }
    }
    let mut s = WinCap {
        want_exe: exe.to_ascii_lowercase(),
        want_title: title.to_ascii_lowercase(),
        found: None,
    };
    let _ = EnumWindows(Some(cb), LPARAM(&mut s as *mut _ as isize));
    s.found
}

/// 重启后给窗口条目重新匹配 live_hwnd（按 exe+标题）；匹配不到（已关）留 0。
unsafe fn restamp_window_hwnds(st: &mut State) {
    for a in st.apps.iter_mut() {
        for it in a.items.iter_mut() {
            if it.kind == 1 {
                it.live_hwnd = find_window_for_capture(&it.target, &it.name)
                    .map(|h| h.0 as isize)
                    .unwrap_or(0);
            }
        }
    }
}

/// 确保宿主应用存在（内环有 → 返回索引；没有 → 内存+ini 新建一个），返回索引。
fn ensure_host_app(st: &mut State, exe: &str) -> usize {
    // Explorer 文件夹窗口 → 优先归「文件夹」hub，而不是另开一个 explorer 内环。
    // ⚠️ 排在 `host_app_index` 之前：已有的 explorer 应用（历史 bug 建的）也要让位，
    //    否则新拖进来的文件夹窗口还会被塞回那个重复的 explorer 内环。
    if is_explorer_exe(exe) {
        if let Some(ai) = folder_hub_index(st) {
            return ai;
        }
    }
    if let Some(ai) = host_app_index(st, exe) {
        return ai;
    }
    let name = clamp_name(
        exe_name_of(exe)
            .map(|b| b.trim_end_matches(".exe").to_string())
            .unwrap_or_else(|| exe.to_string())
            .as_str(),
    );
    let app_icon = unsafe { decode_icon(exe) }.map(|(px, d)| IconTex::from_base(px, d));
    st.apps.push(AppEntry {
        name: name.clone(),
        target: exe.to_string(),
        icon: app_icon,
        items: Vec::new(),
        grad: grad_for(&name),
    });
    ini_insert_app(&st.ini_path, &name, exe);
    st.apps.len() - 1
}

/// 往 [app.X] 段追加一条窗口条目（`target|@win|<relaunch>`）
fn ini_insert_window_item(path: &str, app: &str, key: &str, target: &str, relaunch: &str) -> bool {
    let value = format!("{}|@win|{}", target, relaunch);
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let header = format!("[app.{}]", app);
    let line = format!("{}={}", key, value);
    let mut out = String::new();
    if let Some(pos) = text.find(&header) {
        let after = &text[pos + header.len()..];
        let next_sec = after
            .find("\n[")
            .map(|i| pos + header.len() + i + 1)
            .unwrap_or(text.len());
        out.push_str(&text[..next_sec]);
        if !out.ends_with('\n') {
            out.push_str("\r\n");
        }
        out.push_str(&line);
        out.push_str("\r\n");
        out.push_str(&text[next_sec..]);
    } else {
        out.push_str(text.trim_end());
        if !out.is_empty() {
            out.push_str("\r\n");
        }
        out.push_str(&header);
        out.push_str("\r\n");
        out.push_str(&line);
        out.push_str("\r\n");
    }
    std::fs::write(path, out).is_ok()
}

/// 把被拖动的窗口捕获成一条外环条目（落盘）。返回新增条数。
unsafe fn capture_window(
    st: &mut State,
    hwnd: HWND,
    exe: &str,
    title: &str,
    relaunch: &str,
) -> usize {
    let ai = ensure_host_app(st, exe);
    let item = ItemEntry {
        name: clamp_name(title),
        target: exe.to_string(),
        icon: decode_icon(exe).map(|(px, d)| IconTex::from_base(px, d)),
        color: None,
        kind: 1,
        relaunch: relaunch.to_string(),
        live_hwnd: hwnd.0 as isize,
    };
    // 去重：同 exe + 同标题已存在 → 只刷新 live_hwnd/relaunch，不加重复
    if let Some(pos) = st
        .apps
        .get(ai)
        .and_then(|a| {
            a.items
                .iter()
                .position(|x| x.kind == 1 && x.target.eq_ignore_ascii_case(exe) && x.name == item.name)
        })
    {
        let old = st.apps[ai].items[pos].relaunch.clone();
        st.apps[ai].items[pos].live_hwnd = item.live_hwnd;
        // ⚠️ **不降级覆盖**：这次 UIA 没探到（relaunch 退化成裸 exe）时，别把上次
        //    好不容易存下来的 `explorer.exe "D:\..."` 冲掉 —— 冲掉就等于这条废了。
        if relaunch_score(&item.relaunch) > relaunch_score(&old) {
            st.apps[ai].items[pos].relaunch = item.relaunch.clone();
            let sec = st.apps[ai].name.clone();
            ini_set_item(
                &st.ini_path,
                &sec,
                &item.name,
                &format!("{}|@win|{}", exe, item.relaunch),
            );
        }
        println!("[wincap] 刷新已有窗口条目 · {}", title);
        wlog(&format!(
            "refresh name={} old={} new={}",
            item.name, old, item.relaunch
        ));
        return 0;
    }
    if st.apps[ai].items.len() >= MAX_ITEMS {
        println!("[wincap] 外环已满，跳过 · {}", title);
        return 0;
    }
    st.apps[ai].items.push(item);
    ini_insert_window_item(
        &st.ini_path,
        &st.apps[ai].name,
        &st.apps[ai].items.last().unwrap().name,
        exe,
        relaunch,
    );
    println!("[wincap] 捕获窗口入环 · exe={} title={} relaunch={}", exe, title, relaunch);
    wlog(&format!(
        "add app={} name={} exe={} relaunch={}",
        st.apps[ai].name,
        st.apps[ai].items.last().unwrap().name,
        exe,
        relaunch
    ));
    // 拖窗口进来 = "拖入过"（窗口条目落进的是某个应用的外环 kind=1）
    onb_done_drag(st);
    1
}

/// 清理"重开命令行"：不能用来重开的（COM 工厂串 / 空）退回裸 exe。
///
/// explorer 的**窗口进程**命令行是 `C:\WINDOWS\explorer.exe /factory,{guid} -Embedding`
/// （它是 COM 外壳服务进程），照着这条 spawn 出来的进程不会开任何窗口 ——
/// 用户看到的就是"点了没反应 / 打不开"。宁可退回裸 exe（至少开出一个资源管理器）。
fn sanitize_relaunch(cmd: &str, exe: &str) -> String {
    let low = cmd.to_ascii_lowercase();
    if cmd.trim().is_empty()
        || low.contains("/factory")
        || low.contains("-embedding")
        || low.contains("-servername")
    {
        exe.to_string()
    } else {
        cmd.to_string()
    }
}

/// relaunch 的"含金量"：0 = 根本不能用来重开，越大越具体（带参数的 > 裸 exe）。
fn relaunch_score(s: &str) -> usize {
    let low = s.to_ascii_lowercase();
    if s.trim().is_empty() || low.contains("/factory") || low.contains("-embedding") {
        return 0;
    }
    s.split_whitespace().count()
}

/// 窗口捕获 / 点击的落盘诊断（LocalAppData\PiPie\logs\wincap.log）。
/// 这条链路跨进程 + 跨 UIA，只看 ini 看不出"为什么存成了这样"，留个脚印。
fn wlog(line: &str) {
    use std::io::Write;
    let dir = log_dir();
    let _ = std::fs::create_dir_all(&dir);
    let mut p = std::path::PathBuf::from(dir);
    p.push("wincap.log");
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(p) {
        let _ = writeln!(f, "[{:.2}] {}", now_s(), line);
    }
}

/// 把 `[app.X]` 段里 `key=` 那行**整行替换**（段里没有就追加到段尾）。
/// 用于"刷新已有窗口条目的 relaunch"—— 只改内存的话，下次 `save_settings`
/// 会用内存里的旧值把 ini 覆盖回去。
fn ini_set_item(path: &str, app: &str, key: &str, value: &str) -> bool {
    let Ok(text) = std::fs::read_to_string(path) else {
        return false;
    };
    let header = format!("[app.{}]", app);
    let Some(pos) = text.find(&header) else {
        return false;
    };
    let start = pos + header.len();
    let end = text[start..]
        .find("\n[")
        .map(|i| start + i + 1)
        .unwrap_or(text.len());
    let mut out = String::new();
    out.push_str(&text[..start]);
    let mut replaced = false;
    for line in text[start..end].lines() {
        let t = line.trim();
        if !replaced && !t.is_empty() && !t.starts_with('[') {
            if let Some(rest) = t.strip_prefix(key) {
                if rest.trim_start().starts_with('=') {
                    out.push_str(&format!("{}={}\r\n", key, value));
                    replaced = true;
                    continue;
                }
            }
        }
        out.push_str(line);
        out.push_str("\r\n");
    }
    if !replaced {
        out.push_str(&format!("{}={}\r\n", key, value));
    }
    out.push_str(&text[end..]);
    std::fs::write(path, out).is_ok()
}

/// 探测到一个正在被拖动的窗口 → 收集信息 + 显示 ghost/suggest 预览
unsafe fn arm_window_capture(st: &mut State, hw: HWND) {
    let Some(exe) = window_exe(hw) else {
        return;
    };
    let title = window_title(hw);
    let mut pid = 0u32;
    GetWindowThreadProcessId(hw, Some(&mut pid));
    // ⚠️⚠️ **`exe` 是全路径**（`c:\windows\explorer.exe`），不是文件名！
    //    判进程一律先取文件名再比（曾经拿全路径和 `"explorer.exe"` 比，恒为 false
    //    → UIA 探测一次都没跑过，落盘的 explorer 条目点了什么都不开 ——
    //    用户 2026-10-16「我窗口拖进来的文件夹直接打不开了」）。
    let base = exe
        .rsplit(['\\', '/'])
        .next()
        .unwrap_or(&exe)
        .to_ascii_lowercase();
    let is_explorer = base == "explorer.exe";
    let is_browser = is_browser_exe(&base);

    // ── ① 先把预览摆出来（纯内存，<1ms）──
    // 这一步**绝不能等 UIA**：预览（ghost / suggest）是用户判断"拖进去会落在哪"的
    // 唯一反馈，晚了就等于没有（用户 2026-10-15：拖进去不展示内环/外环）。
    st.window_capture = Some(WindowCapture {
        hwnd: hw.0 as isize,
        exe: exe.clone(),
        title: title.clone(),
        // relaunch 先占位（进程命令行）；explorer/浏览器要等**落盘那一次**才由
        // 后台 UIA 探测补上（`wincap_probe_start` → 回填条目，见 ②）。
        // ⚠️ 占位值要过 `sanitize_relaunch`：explorer 窗口进程的命令行是 COM 工厂串
        //    （`… /factory,{guid} -Embedding`），拿它"重开"点了什么都不开。
        relaunch: sanitize_relaunch(&process_command_line(pid).unwrap_or_else(|| exe.clone()), &exe),
        test: false,
    });
    st.drop_suggest = None;
    st.drop_ghost = None;
    // Explorer 文件夹窗口 → 预览就高亮「文件夹」hub（别再浮 explorer 幽灵）。
    let hub = if is_explorer_exe(&exe) { folder_hub_index(st) } else { None };
    match hub.or_else(|| host_app_index(st, &exe)) {
        Some(ai) => st.drop_suggest = Some(ai),
        None if st.apps.len() < MAX_APPS => {
        let name = clamp_name(
            exe_name_of(&exe)
                .map(|b| b.trim_end_matches(".exe").to_string())
                .unwrap_or_else(|| exe.to_string())
                .as_str(),
        );
        // ⚠️ 走缓存：`decode_icon` ~76ms，而这里是 `WM_MOUSEMOVE` 热路径。
            let icon = {
                let key = exe.to_ascii_lowercase();
                let mut c = WC_ICON_CACHE.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(pos) = c.iter().position(|(k, _)| *k == key) {
                    c[pos].1.clone()
                } else {
                    let v = decode_icon(&exe).map(|(px, d)| IconTex::from_base(px, d));
                    if c.len() >= 16 {
                        c.remove(0);
                    }
                    c.push((key, v.clone()));
                    v
                }
            };
            st.drop_ghost = Some(GhostApp {
                name,
                target: exe.clone(),
                put_items: true,
                icon,
            });
        }
        _ => {}
    }
    set_mood(st, 5);

    // ── ② explorer / 浏览器：这里**不探测**路径·网址 ──
    //
    // 原来在这里同步跑 UIA（注释里写「实测只要 30~120ms」），代价是：
    //  * 拖动过程中每次武装都触发一次 `FindAll(Descendants, 全部元素)`，
    //    浏览器为了回答它要**现搭整棵无障碍树** —— 而用户绝大多数拖动
    //    只是挪窗口、根本不收进环。实测拖 Edge 时「非常非常卡、掉帧」。
    //  * 探测失败时缓存里存的是空串，`Some(v) if !v.is_empty()` 判不命中
    //    → **每次移动都重探**（实测 msedge 每次 arm 40~90ms，全部白跑）。
    // 现在：预览照旧（纯内存，<1ms），**只有松手且落在盘内、这条真要存了**
    // 才问一次（`wincap_probe_start` 后台线程 → `WM_APP_WINPROBE` 回填条目）。
    println!(
        "[wincap] 检测到拖动窗口 · exe={} title={} relaunch={}",
        exe,
        title,
        st.window_capture.as_ref().map(|w| w.relaunch.clone()).unwrap_or_default()
    );
    wlog(&format!(
        "arm hw={:#x} exe={} title={} explorer={} browser={} relaunch={}",
        hw.0 as isize,
        exe,
        title,
        is_explorer,
        is_browser,
        st.window_capture.as_ref().map(|w| w.relaunch.clone()).unwrap_or_default()
    ));
}

/// `on_timer` 每帧调用：探测/落盘窗口捕获。返回是否处于捕获中（用于保持持续重绘）。
unsafe fn poll_window_capture(st: &mut State) -> bool {
    // 武装中 = 有人在拖窗口 → 实时玻璃暂停抓屏、盘面色层冻结（`GLASS_FREEZE`）。
    // 每帧按状态重设，不留任何「忘了清」的分支。
    GLASS_FREEZE.store(st.window_capture.is_some(), Ordering::Relaxed);
    if st.window_capture.is_some() {
        // 测试武装态（WM_APP_WINARM）：poll 不得自动清理/落盘，专等 WM_APP_WINDROP 处理
        if st.window_capture.as_ref().unwrap().test {
            return true;
        }
        if moving_window_throttled().is_some() {
            return true; // 还在拖
        }
        // 松手 → 光标在盘面内才落盘。
        // ⚠️ 用**松手那一刻**的光标（`release_point`）而不是当前光标：去抖要多等
        //    ~130ms 才承认松手，这期间手早就可能移开了 —— 用当前光标会把
        //    「在环里松的手」判成没落盘，或反过来误吞一次快速甩开。
        let wc = st.window_capture.take().unwrap();
        let (rx, ry) = release_point().unwrap_or_else(|| unsafe { cursor_now() });
        let over = in_disc(st, rx, ry);
        if over {
            capture_window(
                st,
                HWND(wc.hwnd as *mut c_void),
                &wc.exe,
                &wc.title,
                &wc.relaunch,
            );
            st.commit_pulse = 1.0;
            // 落盘成功 = 这条要存了，**这时**才去 UIA 问路径/网址（后台线程，不卡 UI）
            wincap_probe_start(st, HWND(wc.hwnd as *mut c_void), &wc.exe);
        }
        st.drop_suggest = None;
        st.drop_ghost = None;
        st.drop_mode = false;
        set_mood(st, 0);
        over
    } else if MENU_OPEN.load(Ordering::Relaxed) {
        if let Some(hw) = moving_window_throttled() {
            let mh = MAIN_HWND.load(Ordering::Relaxed);
            let rh = RING_HWND.load(Ordering::Relaxed);
            if hw.0 as isize != mh && hw.0 as isize != rh {
                arm_window_capture(st, hw);
                return true;
            }
        }
        false
    } else {
        false
    }
}

// ─────────────── 窗口捕获的「路径 / 网址」后台探测 ───────────────

/// 后台探测的回信信箱：`(hwnd, 探到的 relaunch)`（None = 没探到）。
static WINPROBE_TX: std::sync::Mutex<Vec<(isize, Option<String>)>> = std::sync::Mutex::new(Vec::new());
/// 正在探测中的窗口（同一个 hwnd 不叠第二个线程）
static WINPROBE_BUSY: std::sync::Mutex<Vec<isize>> = std::sync::Mutex::new(Vec::new());
/// 同一扇窗探测失败后，多久之内不再重试
const WINPROBE_FAIL_MS: u32 = 60_000;

/// 浏览器内核 exe（`exe` 可以是全路径，也可以已经只剩文件名）
fn is_browser_exe(exe: &str) -> bool {
    let base = exe
        .rsplit(['\\', '/'])
        .next()
        .unwrap_or(exe)
        .to_ascii_lowercase();
    matches!(
        base.as_str(),
        "msedge.exe" | "chrome.exe" | "brave.exe" | "vivaldi.exe" | "firefox.exe"
    )
}

/// 这个「内环应用」的条目是不是**网址**：应用本身是浏览器，或者应用直接指向一个 URL。
/// 决定设置页那颗入口是「＋ 网址」（内联输入框）还是「＋ 添加条目…」（系统文件框）。
fn app_accepts_url(a: &AppEntry) -> bool {
    is_url(&a.target) || is_browser_exe(&a.target)
}

/// 这条 exe 需不需要问 UIA（只有 explorer 文件夹窗口和浏览器窗口要）
fn wincap_needs_probe(exe: &str) -> bool {
    let base = exe
        .rsplit(['\\', '/'])
        .next()
        .unwrap_or(exe)
        .to_ascii_lowercase();
    base == "explorer.exe" || is_browser_exe(&base)
}

/// **落盘之后**才发起：后台线程问 UIA「这扇窗现在的路径 / 网址是什么」，
/// 回信走 `WM_APP_WINPROBE` → [`wincap_apply_probe`] 回填条目和 INI。
///
/// 为什么搬后台：`explorer_window_folder` / `browser_window_url` 实测 15~3300ms
/// （冷启动更慢），同步跑在 UI 线程上 = 松手那一刻整个菜单钉住；更要命的是它以前
/// 挂在拖动路径上，**只是挪个窗口不收进环**也要撞一次，浏览器还得现搭无障碍树
/// —— 用户 2026-10-26 报的「呼出轮盘后拖浏览器非常非常卡」。
///
/// 为什么现在敢搬后台：只在**这条真要存了**的时候跑一次，hwnd 就是刚存的那扇窗，
/// 回填按 `live_hwnd` 定位，不存在以前「探测回来时对不上 hwnd」的竞态。
/// 回信晚了 = 条目先留着占位值（点开至少能起进程），回来再升级；
/// 探不到就记进 `wincap_failed`（带时间戳），60 秒内不再撞第二次。
unsafe fn wincap_probe_start(st: &mut State, hw: HWND, exe: &str) {
    if !wincap_needs_probe(exe) {
        return;
    }
    let h = hw.0 as isize;
    let now = windows::Win32::System::SystemInformation::GetTickCount();
    if let Some(pos) = st.wincap_failed.iter().position(|(x, _)| *x == h) {
        if now.wrapping_sub(st.wincap_failed[pos].1) < WINPROBE_FAIL_MS {
            wlog(&format!("probe-skip hw={:#x} ({}ms 前刚失败过)", h, now.wrapping_sub(st.wincap_failed[pos].1)));
            return;
        }
        st.wincap_failed.remove(pos);
    }
    {
        let Ok(mut b) = WINPROBE_BUSY.lock() else { return };
        if b.contains(&h) {
            return;
        }
        b.push(h);
    }
    let exe_owned = exe.to_string();
    let is_explorer = exe
        .rsplit(['\\', '/'])
        .next()
        .unwrap_or(&exe)
        .eq_ignore_ascii_case("explorer.exe");
    // 诊断开关：`RL_WINCAP_SYNC=1` → 就在 UI 线程**同步**探一次。
    // 用来分清「读不到路径」到底是后台线程的问题，还是这扇窗本来就读不出来
    // （伪窗口 / 对方无障碍没开）。沙箱实测两种跑法结果一致 → 不是线程的问题。
    if std::env::var_os("RL_WINCAP_SYNC").is_some() {
        let got = wincap_probe_exe(h, &exe_owned, is_explorer);
        wlog(&format!("probe-sync hw={h:#x} got={got:?}"));
        if let Ok(mut t) = WINPROBE_TX.lock() {
            t.push((h, got));
        }
        if let Ok(mut b) = WINPROBE_BUSY.lock() {
            b.retain(|x| *x != h);
        }
        wincap_apply_probe(st);
        return;
    }
    std::thread::spawn(move || unsafe {
        let got = wincap_probe_exe(h, &exe_owned, is_explorer);
        // ⚠️ 套间模型不是这里的关键：实测本线程先 `CoInitializeEx(MULTITHREADED)`
        //    （把后面那句 APARTMENTTHREADED 挡成 RPC_E_CHANGED_MODE）和根本不先初始化
        //    （= 走 STA）**结果一样**。跨进程 UIA 贵的是等提供方建树，不是我们的套间。
        //    不注册 UIA 事件，所以这条线程没有消息循环也没关系。
        wlog(&format!("probe-done hw={:#x} got={:?}", h, got));
        if let Ok(mut t) = WINPROBE_TX.lock() {
            t.push((h, got));
        }
        if let Ok(mut b) = WINPROBE_BUSY.lock() {
            b.retain(|x| *x != h);
        }
        post(WM_APP_WINPROBE, 0, 0);
    });
}

/// 探测本体：explorer 走面包屑，浏览器走地址栏。同步调用，谁调谁等。
unsafe fn wincap_probe_exe(h: isize, exe: &str, is_explorer: bool) -> Option<String> {
    let hw = HWND(h as *mut c_void);
    if is_explorer {
        explorer_window_folder(hw).map(|p| format!("{} \"{}\"", exe, p))
    } else {
        browser_window_url(hw)
    }
}

/// 回填：把后台探到的路径 / 网址写进那条窗口条目（内存 + INI）。
/// 只在 `live_hwnd` 对得上、且**比现有值更像能重开**（`relaunch_score`）时才覆盖 ——
/// 不降级覆盖（同 `capture_window`）。窗口已经关了 / 条目已被删 → 直接丢弃。
unsafe fn wincap_apply_probe(st: &mut State) {
    let got = match WINPROBE_TX.lock() {
        Ok(mut t) if !t.is_empty() => std::mem::take(&mut *t),
        _ => return,
    };
    let now = windows::Win32::System::SystemInformation::GetTickCount();
    for (h, v) in got {
        let Some(v) = v else {
            st.wincap_failed.retain(|(x, _)| *x != h);
            st.wincap_failed.push((h, now));
            continue;
        };
        let hit = st.apps.iter().position(|a| {
            a.items
                .iter()
                .any(|x| x.kind == 1 && x.live_hwnd == h && relaunch_score(&v) > relaunch_score(&x.relaunch))
        });
        let Some(ai) = hit else { continue };
        let ii = st.apps[ai]
            .items
            .iter()
            .position(|x| x.kind == 1 && x.live_hwnd == h)
            .unwrap();
        let exe = st.apps[ai].items[ii].target.clone();
        let sec = st.apps[ai].name.clone();
        let name = st.apps[ai].items[ii].name.clone();
        st.apps[ai].items[ii].relaunch = v.clone();
        ini_set_item(
            &st.ini_path,
            &sec,
            &name,
            &format!("{}|@win|{}", exe, v),
        );
        println!("[wincap] 回填重开命令行 · {} → {}", name, v);
        wlog(&format!("probe-patch hw={:#x} app={} name={} relaunch={}", h, sec, name, v));
    }
}

// ─────────────── 「已打开 → 直接展示」 ───────────────
// 启动前先看这个程序是不是**已经在跑**：是就把它的窗口还原 + 置前，不再重复启动。
// （用户 2026-10-09：再点一次 idea 没反应，应该把已开的窗口弹到最前面。）

/// 进程 PID → 映像文件全路径（小写；失败返回 None）。
unsafe fn process_image(pid: u32) -> Option<String> {
    use windows::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
        PROCESS_QUERY_LIMITED_INFORMATION,
    };
    let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
    let mut buf = [0u16; 1024];
    let mut n = buf.len() as u32;
    let r = QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut n);
    let _ = CloseHandle(h);
    r.ok()?;
    Some(String::from_utf16_lossy(&buf[..n as usize]).to_ascii_lowercase())
}

/// 目标 → 「用来找已开窗口的 exe 路径」。
/// `.lnk` 解析出快捷方式目标；`.exe` 原样；URL / `shell:` / 文件夹 → None（没有可聚焦的进程）。
unsafe fn resolve_target_exe(target: &str) -> Option<String> {
    use windows::Win32::System::Com::{
        CoCreateInstance, IPersistFile, CLSCTX_INPROC_SERVER, STGM_READ,
    };
    use windows::Win32::UI::Shell::{IShellLinkW, ShellLink, SLGP_UNCPRIORITY};
    use windows_core::Interface;
    let t = target.trim();
    let low = t.to_ascii_lowercase();
    if low.ends_with(".exe") {
        return Some(t.to_string());
    }
    if !low.ends_with(".lnk") {
        return None;
    }
    let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER).ok()?;
    let pf: IPersistFile = link.cast().ok()?;
    let w: Vec<u16> = t.encode_utf16().chain(std::iter::once(0)).collect();
    pf.Load(PCWSTR(w.as_ptr()), STGM_READ).ok()?;
    let mut buf = [0u16; 1024];
    link.GetPath(&mut buf, ptr::null_mut(), SLGP_UNCPRIORITY.0 as u32).ok()?;
    let n = buf.iter().position(|&c| c == 0).unwrap_or(0);
    if n == 0 {
        None
    } else {
        Some(String::from_utf16_lossy(&buf[..n]))
    }
}

/// 从一个路径拿到**进程映像文件名**（小写，只认 `.exe`）。
/// `.lnk` 先解析出目标再取文件名 —— 否则加进名单的会是快捷方式自己的名字。
fn exe_name_of(path: &str) -> Option<String> {
    let t = path.trim();
    if t.is_empty() {
        return None;
    }
    let real = unsafe { resolve_target_exe(t) }.unwrap_or_else(|| t.to_string());
    let base = real.rsplit(['\\', '/']).next().unwrap_or(&real).to_ascii_lowercase();
    if base.ends_with(".exe") && base.len() > 4 {
        Some(base)
    } else {
        None
    }
}

struct WinSearch<'a> {
    want: &'a str,  // 小写
    bare: bool,     // want 不含路径分隔符 → 只比文件名
    found: Option<HWND>,
}

/// 找「同一个程序的已开顶层窗口」。
unsafe fn find_window_of_exe(exe: &str) -> Option<HWND> {
    unsafe extern "system" fn cb(hwnd: HWND, lp: LPARAM) -> BOOL {
        let s = &mut *(lp.0 as *mut WinSearch);
        if s.found.is_some() {
            return BOOL(0);
        }
        if !IsWindowVisible(hwnd).as_bool() || GetWindowTextLengthW(hwnd) == 0 {
            return BOOL(1);
        }
        // 桌面 / 任务栏这些外壳窗口也是 explorer.exe 的，别当成「应用窗口」
        let mut cls = [0u16; 64];
        let n = GetClassNameW(hwnd, &mut cls).max(0) as usize;
        let c = String::from_utf16_lossy(&cls[..n.min(cls.len())]);
        if matches!(
            c.as_str(),
            "Progman"
                | "WorkerW"
                | "Shell_TrayWnd"
                | "Shell_SecondaryTrayWnd"
                | "Windows.UI.Core.CoreWindow"
                | "RLHost"
                | "RLRing"
        ) {
            return BOOL(1);
        }
        // 工具窗口 / 无主窗口的浮窗：跳过
        let ex = WINDOW_EX_STYLE(GetWindowLongW(hwnd, GWL_EXSTYLE) as u32);
        if ex.0 & WS_EX_TOOLWINDOW.0 != 0 {
            return BOOL(1);
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid == 0 || pid == std::process::id() {
            return BOOL(1);
        }
        if let Some(p) = process_image(pid) {
            let hit = if s.bare {
                p.rsplit(['\\', '/']).next().unwrap_or(&p) == s.want
            } else {
                p == s.want
            };
            if hit {
                s.found = Some(hwnd);
                return BOOL(0);
            }
        }
        BOOL(1)
    }
    let want = exe.trim().to_ascii_lowercase();
    let bare = !want.contains('\\') && !want.contains('/');
    let mut s = WinSearch { want: want.as_str(), bare, found: None };
    let _ = EnumWindows(Some(cb), LPARAM(&mut s as *mut _ as isize));
    s.found
}

/// 把窗口还原 + 置前。
/// ⚠️ `SW_RESTORE` **只在最小化时**调 —— 对最大化的窗口调它会把窗口缩回来。
/// ⚠️ 我们的窗口是 `WS_EX_NOACTIVATE`，从不成为前台进程 → 必须用 `AttachThreadInput`
/// 把本线程挂到**当前前台线程**的输入队列上，才能绕过前台锁让 `SetForegroundWindow` 生效。
unsafe fn focus_window(hwnd: HWND) {
    // ⚠️ `AttachThreadInput` 在 windows crate 里归在 `System::Threading`（不是 UI 模块）
    use windows::Win32::System::Threading::AttachThreadInput;
    let fg = GetForegroundWindow();
    let fg_tid = GetWindowThreadProcessId(fg, None);
    let my_tid = GetCurrentThreadId();
    let attached =
        fg_tid != 0 && fg_tid != my_tid && AttachThreadInput(my_tid, fg_tid, true).as_bool();
    if IsIconic(hwnd).as_bool() {
        let _ = ShowWindow(hwnd, SW_RESTORE);
    }
    let _ = BringWindowToTop(hwnd);
    let _ = SetForegroundWindow(hwnd);
    if attached {
        let _ = AttachThreadInput(my_tid, fg_tid, false);
    }
}

// ─────────────── 网址：已打开就切到那个标签 ───────────────
// ⚠️ Windows 没有任何 API 能读「某个标签页的 URL」—— UI Automation 里标签页**只暴露标题**。
//    所以做法只能是：打开网址前先抓这个网址的 `<title>`，再拿标题去比对浏览器所有标签，
//    命中就切过去；抓不到/对不上就照常新开（**只会漏判，不会切错**）。

fn is_url(t: &str) -> bool {
    let l = t.trim().to_ascii_lowercase();
    l.starts_with("http://") || l.starts_with("https://") || l.starts_with("file:///")
}

/// 标题规范化：小写 + 压掉连续空白。
/// 浏览器标签标题会带「 - 已固定」「 - 选项卡组 X 的一部分」这类后缀，靠前缀/包含匹配兜住。
fn norm_title(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

/// 抽第一个 `<title>`。
/// ⚠️ `to_ascii_lowercase()` 只改 ASCII 字母、**不改变字节长度** → 可以用同一套下标切片。
fn extract_title(html: &str) -> Option<String> {
    let low = html.to_ascii_lowercase();
    let mut from = 0usize;
    while let Some(pos) = low[from..].find("<title") {
        let i = from + pos;
        let gt = html[i..].find('>')?;
        let start = i + gt + 1;
        let end = low[start..].find("</title")?;
        // 只压空白、**保留原始大小写**：这个标题现在有两个用处 ——
        // 比对标签页（`select_tab_by_title` 自己会 `norm_title` 转小写）和
        // 当设置页条目名（「example domain」这种全小写很难看）。
        let t = start + end;
        let raw = html[start..t].split_whitespace().collect::<Vec<_>>().join(" ");
        if !raw.is_empty() {
            return Some(raw);
        }
        from = t + 8;
    }
    None
}

/// 取网页标题：`file:///` 直接读本地文件；http(s) 走 WinINet（短超时，失败就放弃）。
unsafe fn fetch_page_title(url: &str) -> Option<String> {
    use windows::Win32::Networking::WinInet::*;
    let u = url.trim();
    if u.to_ascii_lowercase().starts_with("file:///") {
        let p = u[8..].replace('/', "\\");
        return extract_title(&String::from_utf8_lossy(&std::fs::read(p).ok()?));
    }
    let agent = wide("PiPie/1.0");
    let h = InternetOpenW(
        PCWSTR(agent.as_ptr()),
        INTERNET_OPEN_TYPE_PRECONFIG.0,
        PCWSTR::null(),
        PCWSTR::null(),
        0,
    );
    if h.is_null() {
        return None;
    }
    let ms: u32 = 1200;
    let pms = &ms as *const u32 as *const c_void;
    for opt in [
        INTERNET_OPTION_CONNECT_TIMEOUT,
        INTERNET_OPTION_SEND_TIMEOUT,
        INTERNET_OPTION_RECEIVE_TIMEOUT,
    ] {
        let _ = InternetSetOptionW(Some(h as *const c_void), opt, Some(pms), 4);
    }
    let uw = wide(u);
    let uh = InternetOpenUrlW(
        h as *const c_void,
        PCWSTR(uw.as_ptr()),
        None,
        INTERNET_FLAG_RELOAD,
        0,
    );
    let mut html = Vec::<u8>::new();
    if !uh.is_null() {
        let mut buf = [0u8; 8192];
        loop {
            let mut read = 0u32;
            let ok = InternetReadFile(
                uh as *const c_void,
                buf.as_mut_ptr() as *mut c_void,
                buf.len() as u32,
                &mut read,
            )
            .is_ok();
            if !ok || read == 0 {
                break;
            }
            html.extend_from_slice(&buf[..read as usize]);
            if html.len() > 262_144 {
                break;
            }
        }
        let _ = InternetCloseHandle(uh as *const c_void);
    }
    let _ = InternetCloseHandle(h as *const c_void);
    extract_title(&String::from_utf8_lossy(&html))
}

/// 标题匹配打分：精确 3 > 前缀 2 > 包含 1。
/// ⚠️ 「包含」要求 ≥6 个字符，否则「首页」这种泛词会把一堆标签乱配。
fn title_score(tab: &str, page: &str) -> i32 {
    if tab.is_empty() || page.is_empty() {
        return 0;
    }
    if tab == page {
        return 3;
    }
    if tab.starts_with(page) {
        return 2;
    }
    if page.chars().count() >= 6 && tab.contains(page) {
        return 1;
    }
    0
}

/// 收集某窗口类的可见顶层窗口（Chromium 系浏览器都是 `Chrome_WidgetWin_1`）。
unsafe fn windows_of_class(cls: &str) -> Vec<HWND> {
    struct Scan<'a> {
        cls: &'a str,
        out: Vec<HWND>,
    }
    unsafe extern "system" fn cb(hwnd: HWND, lp: LPARAM) -> BOOL {
        let s = &mut *(lp.0 as *mut Scan);
        if !IsWindowVisible(hwnd).as_bool() {
            return BOOL(1);
        }
        let mut b = [0u16; 64];
        let n = GetClassNameW(hwnd, &mut b).max(0) as usize;
        if String::from_utf16_lossy(&b[..n.min(b.len())]).eq_ignore_ascii_case(s.cls) {
            s.out.push(hwnd);
        }
        BOOL(1)
    }
    let mut s = Scan { cls, out: Vec::new() };
    let _ = EnumWindows(Some(cb), LPARAM(&mut s as *mut _ as isize));
    s.out
}

/// 把「虚拟磁盘」样式（`本地磁盘 (C:)` / `新加卷 (D:)`）还原成 `C:\` / `D:\`。
///
/// Windows 的面包屑第一段（以及导航窗格）用的是**显示名**而不是盘符路径，
/// 而且带全角/半角括号与空格。这里按「括号里取盘符」的规律还原。
fn drive_root_from_label(label: &str) -> Option<String> {
    let open = label.find('(')?;
    let close = label[open + 1..].find(')')? + open + 1;
    let inner = label[open + 1..close].trim();
    // 只要是单字母盘符（C / D / E）就认；其它内容（"个人 (OneDrive)"）不认。
    // ⚠️ 括号里通常是 `D:` —— **带冒号**。之前用 `it.next().is_some()` 一刀切，
    //    结果把 `D:` 也判否了，路径拼出来是 `新加卷 (D:)\工作\…`（open 端没走通）。
    let inner = inner.trim_end_matches(':').trim();
    let mut it = inner.chars();
    let c = it.next()?;
    if !c.is_ascii_alphabetic() {
        return None;
    }
    if it.next().is_some() {
        return None;
    }
    // ⚠️ **不要带尾斜杠**：调用方是 `crumbs.join("\\")`，带了就会拼成
    //    `D:\\工作\…`（双反斜杠，Explorer 能认但 ini 里很难看）。
    Some(format!("{}:", c.to_ascii_uppercase()))
}

/// explorer 窗口的**当前目录**（完整路径，无尾斜杠）。
///
/// ## 为什么必须走 UI Automation
///
/// explorer 是 **COM 服务器模式启动**的：进程命令行是
/// `explorer.exe /factory,{75dff2b7-…} -Embedding` —— **路径根本不在命令行里**。
/// 所以 `process_command_line(pid)` 只能拿到光秃秃的 `Explorer.EXE`，重开时会打开
/// 「上次位置」而不是用户拖的那个文件夹。
///
/// 原本打算用 `SHGetDesktopFolder` → `SHBindToObject` → `IShellView::GetCurrentFolder`，
/// 但 **windows 0.58 根本没生成该方法的绑定**（`IShellView_Vtbl` 在 `GetItemObject`
/// 处就截断了），手调 vtable 会段错误。
///
/// UI Automation 两条路（实测结构见下）：
/// - **面包屑链**（首选）：`cls = FileExplorerExtensions.BreadcrumbBarItemControl`
///   的元素**按顺序**列出了路径的每一段，例如
///   `此电脑` → `新加卷 (D:)` → `工作` → `IDEA` → `PiPie`。
/// - **地址栏 Edit**（备选）：`ct = Edit(50004)` / `cls = TextBox` / `name = 地址栏`，
///   `ValuePattern.CurrentValue` 直接给全路径 —— 但**折叠状态下是空的**，
///   用户没点过地址栏就拿不到。
unsafe fn explorer_window_folder(hwnd: HWND) -> Option<String> {
    if let Some(v) = explorer_window_folder_self(hwnd) {
        return Some(v);
    }
    // ⚠️⚠️ **伪窗口兜底 —— 这是本函数存在的主要理由**：
    //
    //    一个 explorer 文件夹窗口在窗口列表里有**好几个 hwnd**：`CabinetWClass`
    //    主窗口（UIA 能看到 110~131 个元素）+ 若干**伪窗口**（UIA 只看得到 7 个）。
    //    实测 15 个同名窗口里**只有 1 个**能读到面包屑 —— 用户拖标题栏时
    //    `moving_window()` 给哪个完全不确定，给到伪窗口就读不到路径，
    //    表现就是「存下来的又是光秃秃的 Explorer.EXE」。
    //
    //    `GetAncestor`/`GetParent` 靠不住（windows 0.58 里前者返回 `Result`，
    //    实测也找不到真窗口）。**可靠做法是按进程 + 窗口标题在候选里找**：
    //    同进程、同类名、同标题的那批窗口里逐个试 UIA，第一个能读出面包屑的
    //    就是它。窗口很少（通常 <5 个），试错成本可忽略。
    // ⚠️⚠️ **不要按 pid 过滤** —— explorer 是**多进程**的：实测同一个文件夹窗口
    //    在窗口列表里有 2 个 hwnd，分别属于 pid 3632 和 pid **3908**。
    //    按 pid 找就等于「只在自己进程里找」，而真窗口（UIA 能读到面包屑的那个）
    //    恰恰**在另一个进程**里 —— 实测按 pid 过滤的 scan 9ms 就返回 None。
    //    所以只按「类名 + 文件夹名」匹配，不看进程。
    let cls = window_class(hwnd);
    let title = window_title(hwnd);
    // 标题去掉「 - 文件资源管理器」后缀 = 文件夹名，用它做同组判定
    let leaf = title
        .rsplit_once(" - ")
        .map(|(a, _)| a.to_string())
        .unwrap_or_else(|| title.clone());

    struct Scan {
        cls: String,
        leaf: String,
        want: isize,
        found: Option<String>,
        done: bool,
        /// 同类名、同文件夹的候选数（诊断：分不清「扫描没找到候选」和
        /// 「找到候选但 UIA 读不出面包屑」时看这个数）
        cands: u32,
        /// 其中 `IsWindowVisible=false` 的有几个
        hidden: u32,
    }
    extern "system" fn cb(h: HWND, lp: LPARAM) -> BOOL {
        unsafe {
            let s = &mut *(lp.0 as *mut Scan);
            if s.done || h.0 as isize == s.want {
                return BOOL(1);
            }
            if window_class(h) != s.cls {
                return BOOL(1);
            }
            let t = window_title(h);
            let lf = t
                .rsplit_once(" - ")
                .map(|(a, _)| a.to_string())
                .unwrap_or_else(|| t.clone());
            if lf != s.leaf {
                return BOOL(1);
            }
            s.cands += 1;
            // ⚠️ 可见性判定放在**类名+标题匹配之后**，而且只记不筛：
            //    以前它排在最前面，`IsWindowVisible=false` 的那些（Win11 的 XAML 宿主
            //    窗口 / 刚建好还没走完可见性转换的窗口）连名都不报，
            //    等于「能读到面包屑的那个候选」可能被无声滤掉。
            //    候选本来就只有一两个，多试一个隐藏窗口的代价 = 一次 UIA 读。
            if !IsWindowVisible(h).as_bool() {
                s.hidden += 1;
            }
            if let Some(v) = explorer_window_folder_self(h) {
                s.found = Some(v);
                s.done = true;
                return BOOL(0);
            }
            BOOL(1)
        }
    }
    let mut sc = Scan {
        cls: cls.clone(),
        leaf: leaf.clone(),
        want: hwnd.0 as isize,
        found: None,
        done: false,
        cands: 0,
        hidden: 0,
    };
    let _ = EnumWindows(Some(cb), LPARAM(&mut sc as *mut _ as isize));
    // 一行说清：给到的 hwnd 是什么类名/组、扫到几个同类候选、成了没有。
    // 「存下来的又是光秃秃的 Explorer.EXE」时先看这行 —— 候选 0 个是匹配问题，
    // 候选 >0 还没路径就是对方那棵 UIA 树读不出面包屑。
    wlog(&format!(
        "crumb-scan want={:#x} cls={:?} leaf={:?} 候选 {} 个(隐藏 {}) → {}",
        hwnd.0 as isize,
        cls,
        leaf,
        sc.cands,
        sc.hidden,
        if sc.found.is_some() { "读到路径" } else { "没有" }
    ));
    sc.found
}

/// `explorer_window_folder` 的真身（只看给定的这一个 hwnd）。
unsafe fn explorer_window_folder_self(hwnd: HWND) -> Option<String> {
    use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};
    use windows::Win32::UI::Accessibility::{CUIAutomation, TreeScope_Descendants};

    // ⚠️ COM 必须在本线程初始化过。主程序启动时已 `CoInitializeEx` 过整个 UI 线程，
    //    而后台探测线程自己也会初始化 —— 但**别的调用方**（诊断探针、将来的代码）
    //    可能来自未初始化的线程，CoCreateInstance 会静默失败（实测返回 0ms + None）。
    let _ = windows::Win32::System::Com::CoInitializeEx(
        None,
        windows::Win32::System::Com::COINIT_APARTMENTTHREADED,
    );
    let hwi = hwnd.0 as isize;
    let ui: windows::Win32::UI::Accessibility::IUIAutomation =
        match CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER) {
            Ok(v) => v,
            Err(e) => {
                wlog(&format!("uia-crumb hw={hwi:#x} CoCreateInstance 失败 {e:?}"));
                return None;
            }
        };
    // ⚠️ 这里**不能**学 `browser_window_url` 用属性窄查询：实测按
    //    `ClassName = FileExplorerExtensions.BreadcrumbBarItemControl` 过滤，
    //    DirectUI（explorer 的工具栏）那批 provider **一个元素都不返回**（沙箱实测
    //    `命中元素 0 个`，同窗口用 TrueCondition 能读到面包屑）。
    //    explorer 的树本来就小（实测 110~131 个元素），全量走一遍代价可忽略 ——
    //    真正贵的是 Chromium 那种几千节点的树。
    let cond = ui.CreateTrueCondition().ok()?;
    let root = match ui.ElementFromHandle(hwnd) {
        Ok(v) => v,
        Err(e) => {
            wlog(&format!("uia-crumb hw={hwi:#x} ElementFromHandle 失败 {e:?}"));
            return None;
        }
    };
    let arr = match root.FindAll(TreeScope_Descendants, &cond) {
        Ok(v) => v,
        Err(e) => {
            wlog(&format!("uia-crumb hw={hwi:#x} FindAll 失败 {e:?}"));
            return None;
        }
    };

    let mut crumbs: Vec<String> = Vec::new();
    let mut edit_val = String::new();
    let mut crumb_hits = 0u32;
    for i in 0..arr.Length().unwrap_or(0) {
        let Ok(e) = arr.GetElement(i) else { continue };
        let cls = e.CurrentClassName().map(|b| b.to_string()).unwrap_or_default();

        // ① 面包屑（路径的权威来源）
        if cls == "FileExplorerExtensions.BreadcrumbBarItemControl" {
            crumb_hits += 1;
            let name = e.CurrentName().map(|b| b.to_string()).unwrap_or_default();
            if name.trim().is_empty() {
                continue;
            }
            // 第一段「此电脑」不是路径的一部分，跳过
            if name == "此电脑" || name == "This PC" {
                crumbs.clear();
                continue;
            }
            match drive_root_from_label(&name) {
                Some(root) => {
                    // 盘符段：把之前的替换掉（面包屑是逐段追加的）
                    crumbs.clear();
                    crumbs.push(root);
                }
                None => crumbs.push(name),
            }
            continue;
        }

        // ② 地址栏编辑框（面包屑拿不到时的兜底）
        if edit_val.is_empty()
            && e.CurrentControlType().map(|c| c.0).unwrap_or(-1) == UIA_EDIT_CT
            && (cls == "TextBox" || e.CurrentName().map(|b| b.to_string()).unwrap_or_default().contains("地址"))
        {
            if let Ok(p) = e
                .GetCurrentPatternAs::<windows::Win32::UI::Accessibility::IUIAutomationValuePattern>(
                    windows::Win32::UI::Accessibility::UIA_ValuePatternId,
                )
            {
                if let Ok(v) = p.CurrentValue() {
                    let t = v.to_string();
                    if looks_like_path(&t) {
                        edit_val = t;
                    }
                }
            }
        }
    }

    if !crumbs.is_empty() {
        return Some(crumbs.join("\\"));
    }
    if !edit_val.is_empty() {
        return Some(edit_val);
    }
    // `元素` = 整棵树的节点数，`面包屑` = 其中属于面包屑的个数。
    // 伪窗口（UIA 只看得到 7 个节点、面包屑 0 个）就是靠这行认出来的。
    wlog(&format!(
        "uia-crumb hw={hwi:#x} 元素 {} 个 / 面包屑 {crumb_hits} 个 → 读不到路径",
        arr.Length().unwrap_or(0)
    ));
    None
}

/// UIA 的「编辑框」控件类型值。
///
/// ⚠️⚠️ **是 50004，不是 5004** —— 这是我踩过的最蠢的坑：凭印象写成 5004，
///    于是 `ct != 5004` 恒为 true、地址栏永远被 `continue` 掉。
///    症状极隐蔽：诊断里能打印出 `ct=50004`，但比较结果却是 `false`，
///    让人怀疑是「UIA 属性查询不实时」之类的玄学。
///    **规则：UIA 控件类型一律用 crate 的命名常量，别写裸数字。**
const UIA_EDIT_CT: i32 = 50004;

/// `*const u16`（UTF-16，以 NUL 结尾）→ `String`。空指针返回空串。
fn bstr_to_string(p: *const u16) -> String {
    if p.is_null() {
        return String::new();
    }
    let mut n = 0usize;
    unsafe {
        while *p.add(n) != 0 {
            n += 1;
            if n > 4096 {
                break;
            }
        }
        String::from_utf16_lossy(std::slice::from_raw_parts(p, n))
    }
}

/// 字符串长得像一个文件系统路径（而不是「此电脑」这种占位名）
fn looks_like_path(s: &str) -> bool {
    let t = s.trim();
    if t.len() < 3 {
        return false;
    }
    // `X:\...` 或 `\\server\share`
    (t.len() >= 3 && t.as_bytes()[1] == b':' && (t.as_bytes()[2] == b'\\' || t.as_bytes()[2] == b'/'))
        || t.starts_with("\\\\")
}

/// 浏览器窗口**当前标签页的网址**（拖标题栏时用它当 relaunch）。
///
/// Chromium 系（Edge / Chrome）的地址栏是 `ct = Edit(50004)`、
/// `cls = OmniboxViewViews`、`name = "地址和搜索栏"`，`ValuePattern.CurrentValue`
/// 直接给完整 URL（实测能拿到 `https://cn.bing.com/images/search?q=…`）。
///
/// ⚠️ 必须走**后台线程**（UIA 首次 `ElementFromHandle` 实测 15~3300ms，
///    冷启动更慢）—— 调用点是 `wincap_probe_start`（只在松手落盘那一次）。
unsafe fn browser_window_url(hwnd: HWND) -> Option<String> {
    use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};
    use windows::Win32::UI::Accessibility::{CUIAutomation, TreeScope_Descendants};

    // 同上：COM 未初始化时 CoCreateInstance 静默失败
    let _ = windows::Win32::System::Com::CoInitializeEx(
        None,
        windows::Win32::System::Com::COINIT_APARTMENTTHREADED,
    );
    let ui: windows::Win32::UI::Accessibility::IUIAutomation =
        CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER).ok()?;
    // ⚠️ **窄查询**：TrueCondition 会让 UIA 把整棵子树的每个元素都物化并缓存，
    //    Chromium 为了回答它要现搭整棵无障碍树（卡的是浏览器那一头）。
    //    地址栏一定是 Edit，按 ControlType 筛掉 99% 的元素。
    let cond = ui
        .CreatePropertyCondition(
            windows::Win32::UI::Accessibility::UIA_ControlTypePropertyId,
            &windows_core::VARIANT::from(UIA_EDIT_CT),
        )
        .ok()?;
    let root = ui.ElementFromHandle(hwnd).ok()?;
    let arr = root.FindAll(TreeScope_Descendants, &cond).ok()?;
    let hwi = hwnd.0 as isize;
    let mut edits = 0i32;
    let mut omni = 0i32;
    for i in 0..arr.Length().unwrap_or(0) {
        let Ok(e) = arr.GetElement(i) else { continue };
        edits += 1;
        let cls = e.CurrentClassName().map(|b| b.to_string()).unwrap_or_default();
        let name = e.CurrentName().map(|b| b.to_string()).unwrap_or_default();
        // 地址栏的特征：cls 是 OmniboxViewViews，或名字里带「地址」/「搜索栏」
        let is_omni = cls == "OmniboxViewViews" || cls.contains("Omnibox");
        let is_named =
            name.contains("地址") || name.contains("Address") || name.contains("搜索栏");
        if !is_omni && !is_named {
            continue;
        }
        omni += 1;
        // 三条取值途径，按可靠度排序：
        // ① `ValuePattern`（最正规，但部分版本的 Chromium 只在聚焦时给）
        // ② `UIA_ValueValuePropertyId`（属性直读，绕过 pattern）
        // ③ `LegacyIAccessible` 的 Value（最老但最兼容的一批控件实现它）
        let mut url = String::new();
        if let Ok(p) = e
            .GetCurrentPatternAs::<windows::Win32::UI::Accessibility::IUIAutomationValuePattern>(
                windows::Win32::UI::Accessibility::UIA_ValuePatternId,
            )
        {
            if let Ok(v) = p.CurrentValue() {
                url = v.to_string();
            }
        }
        if url.trim().is_empty() {
            // ⚠️ 这个绑定**直接返回 VARIANT**（不是出参），`bstrVal` 在 union 里
            //    还要再进一层 `Anonymous`。
            if let Ok(var) = e.GetCurrentPropertyValue(
                windows::Win32::UI::Accessibility::UIA_ValueValuePropertyId,
            ) {
                // ⚠️ union 要进**三层** `Anonymous` 才到 `bstrVal`，它本身是
                //    `*const u16`（不是 BSTR）→ 得自己数长度再 from_utf16_lossy。
                url = bstr_to_string(unsafe { var.as_raw().Anonymous.Anonymous.Anonymous.bstrVal });
            }
        }
        if url.trim().is_empty() {
            if let Ok(l) = e.GetCurrentPatternAs::<
                windows::Win32::UI::Accessibility::IUIAutomationLegacyIAccessiblePattern,
            >(windows::Win32::UI::Accessibility::UIA_LegacyIAccessiblePatternId) {
                if let Ok(v) = l.CurrentValue() {
                    url = v.to_string();
                }
            }
        }
        let t = url.trim();
        if t.starts_with("http://") || t.starts_with("https://") || t.starts_with("file://") {
            return Some(t.to_string());
        }
    }
    wlog(&format!(
        "uia-omni hw={hwi:#x} Edit {edits} 个 / 地址栏 {omni} 个，三条途径都取不到 URL"
    ));
    None
}

/// 在浏览器里找标题命中的标签页 → 选中它（= 切换过去）。返回该浏览器窗口。
unsafe fn select_tab_by_title(page_title: &str) -> Option<HWND> {
    use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};
    use windows::Win32::UI::Accessibility::{
        CUIAutomation, IUIAutomation, IUIAutomationSelectionItemPattern, TreeScope_Descendants,
        UIA_SelectionItemPatternId, UIA_TabItemControlTypeId,
    };
    let p = norm_title(page_title);
    if p.chars().count() < 2 {
        return None;
    }
    let ui: IUIAutomation = CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER).ok()?;
    let cond = ui.CreateTrueCondition().ok()?;
    let mut best: Option<(i32, HWND, IUIAutomationSelectionItemPattern)> = None;
    for hw in windows_of_class("Chrome_WidgetWin_1") {
        let Ok(root) = ui.ElementFromHandle(hw) else {
            continue;
        };
        let Ok(arr) = root.FindAll(TreeScope_Descendants, &cond) else {
            continue;
        };
        for i in 0..arr.Length().unwrap_or(0) {
            let Ok(e) = arr.GetElement(i) else { continue };
            // 只认标签页元素
            if e.CurrentControlType().map(|c| c.0).unwrap_or(-1) != UIA_TabItemControlTypeId.0 {
                continue;
            }
            let Ok(name) = e.CurrentName() else { continue };
            let sc = title_score(&norm_title(&name.to_string()), &p);
            if sc == 0 {
                continue;
            }
            if best.as_ref().map(|(b, _, _)| sc > *b).unwrap_or(true) {
                if let Ok(pat) = e
                    .GetCurrentPatternAs::<IUIAutomationSelectionItemPattern>(UIA_SelectionItemPatternId)
                {
                    best = Some((sc, hw, pat));
                }
            }
        }
    }
    let (_, hw, pat) = best?;
    if !pat.CurrentIsSelected().map(|b| b.as_bool()).unwrap_or(false) {
        let _ = pat.Select();
    }
    Some(hw)
}

/// 网址已经开着 → 切到那个标签并置前。返回 false = 没开（调用方照常打开）。
/// ⚠️ 慢（联网 + UI Automation 共 0.5~2 秒）→ **只能从后台线程调**（见 `switch_bg`）。
unsafe fn switch_to_open_tab(url: &str) -> bool {
    // 没有 Chromium 系浏览器在跑就别联网抓标题了（省掉最贵的那一步）
    if windows_of_class("Chrome_WidgetWin_1").is_empty() {
        return false;
    }
    // ① 先试标题缓存（不联网，最快）
    if let Some(t) = cache_get(url) {
        if let Some(hw) = select_tab_by_title(&t) {
            focus_window(hw);
            return true;
        }
    }
    // ② 缓存没中（或标题变了）→ 联网重抓一次再试
    let Some(t) = fetch_page_title(url) else {
        return false;
    };
    cache_put(url, &t);
    match select_tab_by_title(&t) {
        Some(hw) => {
            focus_window(hw);
            true
        }
        None => false,
    }
}

/// **后台**执行上面那件事，完成后 post `WM_APP_LOADED` 让 UI 线程收尾。
/// ⚠️ 必须有自己的 `CoInitializeEx`（UIA 的 `CUIAutomation` 是 COM 对象；
/// 拿 UI 线程的 COM 公寓跨线程用会直接失败）。
fn switch_bg(url: String) {
    std::thread::spawn(move || {
        let hit = unsafe {
            let _ = windows::Win32::System::Com::CoInitializeEx(
                None,
                windows::Win32::System::Com::COINIT_APARTMENTTHREADED,
            );
            switch_to_open_tab(&url)
        };
        LOAD_HIT.store(hit, Ordering::SeqCst);
        unsafe { post(WM_APP_LOADED, 0, 0) };
    });
}

// ─────────────── 例外名单：当前前台程序「该不该放行」 ───────────────

/// 前台程序的**映像文件名**（小写）；拿不到（含「就是我们自己」）返回空串。
unsafe fn fg_proc_name() -> String {
    let fg = GetForegroundWindow();
    if fg.0.is_null() {
        return String::new();
    }
    let mut pid = 0u32;
    GetWindowThreadProcessId(fg, Some(&mut pid));
    if pid == 0 || pid == std::process::id() {
        return String::new();
    }
    match process_image(pid) {
        Some(p) => p.rsplit(['\\', '/']).next().unwrap_or(&p).to_string(),
        None => String::new(),
    }
}

/// Windows 外壳窗口 = **桌面本身**（`Progman` / 桌面幻灯片 `WorkerW`）和任务栏。
///
/// ⚠️ 为什么例外名单判定必须把它们摘出去（issue #1，2026-10-27 定位）：
/// 桌面窗口的矩形**正好铺满整个显示器**（实测 1920×1080 的机器上
/// `Progman` = `(0,0,1920,1080)`），于是「前台窗口矩形盖满显示器」这条全屏判据
/// 对桌面恒为真 → `EXEMPT` → 呼出键被原样放行 → **盘子再也调不出来**。
/// 现场就是用户报的：「浏览器里中键呼出，点右下角“显示桌面”，之后中键无效，
/// 非得进一次设置界面才恢复」—— 进设置界面之所以有效，是因为前台换成了
/// 我们自己的窗口（`fg_proc_name` 对本进程返回空串 → 不例外）。
unsafe fn is_shell_wnd(hwnd: HWND) -> bool {
    if hwnd.is_invalid() {
        return false;
    }
    matches!(
        window_class(hwnd).as_str(),
        "Progman" | "WorkerW" | "Shell_TrayWnd" | "Shell_SecondaryTrayWnd"
    )
}

/// 窗口矩形是否**完全盖住**它所在的整个显示器（= 真全屏 / 无边框窗口化游戏）
unsafe fn window_covers_monitor(hwnd: HWND) -> bool {
    use windows::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST,
    };
    let mut r = RECT::default();
    if GetWindowRect(hwnd, &mut r).is_err() {
        return false;
    }
    let mut mi = MONITORINFO::default();
    mi.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
    if !GetMonitorInfoW(MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST), &mut mi).as_bool() {
        return false;
    }
    let m = mi.rcMonitor;
    r.left <= m.left && r.top <= m.top && r.right >= m.right && r.bottom >= m.bottom
}

/// 前台窗口是不是「全屏」（游戏 / 视频 / 远程桌面…）
unsafe fn fg_fullscreen() -> bool {
    let fg = GetForegroundWindow();
    if fg.is_invalid() || !IsWindowVisible(fg).as_bool() || is_shell_wnd(fg) {
        return false;
    }
    window_covers_monitor(fg)
}

/// 刷新 `EXEMPT`。⚠️ 只能由 **UI 线程**定时调（见该静态量的注释）。
unsafe fn update_exempt(st: &State) {
    let ex = if st.exc_fs && fg_fullscreen() {
        true
    } else {
        let n = fg_proc_name();
        if n.is_empty() {
            false
        } else if st.exc_mode == 1 {
            // 白名单模式：**不在**白名单里的才放行。
            // ⚠️ 空表时必须判成「不例外」，否则启动器永远不工作。
            // ⚠️ 桌面/任务栏也不算「应用」：白名单下前台是桌面时必须照样能呼出，
            //    否则「回到桌面」就把启动器钉死了（和 issue #1 同一个坑，另一条路）。
            //    黑名单**不**走这个后门 —— 用户把 `explorer.exe` 拉黑是有意的。
            let w = st.exc_w.iter().any(|e| e == &n);
            !st.exc_w.is_empty() && !w && !is_shell_wnd(GetForegroundWindow())
        } else {
            // 黑名单模式：在名单里的放行
            st.exc.iter().any(|e| e == &n)
        }
    };
    EXEMPT.store(ex, Ordering::Relaxed);
}

// ───────────────────── 执行 ─────────────────────

fn launch(target: &str) {
    unsafe {
        // 网址已经开着 → 切到那个标签并置前
        if is_url(target) && switch_to_open_tab(target) {
            println!("[*] 切到已打开网页 · {target}");
            return;
        }
        // 目标程序**已经在跑** → 只把它的窗口弹到最前，不再重复启动
        if let Some(exe) = resolve_target_exe(target) {
            if let Some(hw) = find_window_of_exe(&exe) {
                focus_window(hw);
                println!("[*] 聚焦已开窗口 · {target}");
                return;
            }
        }
        let p = wide(target);
        let _ = ShellExecuteW(
            HWND(ptr::null_mut()),
            PCWSTR::null(),
            PCWSTR(p.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        );
    }
}

/// 挂上 loading（任何"慢操作"开始时调）。
///
/// ⚠️⚠️ **必须把帧定时器踢起来**：菜单空闲时它是停着的（省 CPU），不踢的话
/// loading 只是躺在内存里 —— 环停在旧帧上，用户什么动画都看不到
/// （当年网址 loading 第一版就是这么白做的）。
fn start_loading(st: &mut State, ai: usize, ci: usize, kind: u8) {
    st.loading = Some((ai, ci));
    st.loading_t0 = now_s();
    st.loading_kind = kind;
    set_mood(st, 5); // 忙碌：目光小幅循环往复
    unsafe {
        let mh = MAIN_HWND.load(Ordering::Relaxed);
        if mh != 0 {
            let _ = SetTimer(HWND(mh as *mut c_void), TIMER_FRAME, 16, None);
        }
        frame(st, 255);
    }
}

/// loading 收尾：清状态、收起菜单、头像回常态。
/// 网址那条路由 `WM_APP_LOADED` 自己收（它还要切标签），**不要**在这里抢。
fn finish_loading(st: &mut State) {
    if st.loading.take().is_some() {
        set_mood(st, 0);
        unsafe { hide_menu(st) };
    }
}

/// 撤掉 loading 的**显示**，但**不碰菜单**。
/// 用于「延迟单击」被取消（第二击改名 / 菜单收起）—— 那时菜单该留着，
/// 只有那个"我以为你要开"的三点动画该消失。
fn cancel_loading(st: &mut State) {
    if st.loading.take().is_some() {
        set_mood(st, 0);
    }
}

/// 窗口条目的**动作**部分（不碰 State，可在任意线程跑）。
/// 返回值只用于日志：`focus` / `respawn` / `fallback`。
unsafe fn activate_window_action(it: &ItemEntry) -> &'static str {
    let hw = HWND(it.live_hwnd as *mut c_void);
    let alive = it.live_hwnd != 0
        && IsWindow(hw).as_bool()
        && IsWindowVisible(hw).as_bool()
        && window_matches_item(hw, &it.target, &it.name);
    if alive {
        focus_window(hw);
        "focus"
    } else if relaunch_score(&it.relaunch) > 0 {
        spawn_cmd(&it.relaunch);
        "respawn"
    } else {
        launch(&it.target);
        "fallback"
    }
}

/// 启动 / 聚焦这类**会阻塞**的动作一律丢后台跑，跑完 post `WM_APP_LAUNCHED` 收尾。
///
/// ⚠️ 为什么必须异步：`resolve_target_exe`（.lnk 要 COM）、`find_window_of_exe`
///    （EnumWindows 扫全屏窗口）、`spawn_cmd`（CreateProcess）任一都可能上百毫秒，
///    同步跑在 UI 线程上 = 整圈冻住 —— 而 loading 动画**就在这条线程上画**，
///    自相矛盾：越慢越看不到动画（用户 2026-10-16：「点外环没有加载动效」）。
fn run_action_async(kind: u8, target: String, hw: isize, relaunch: String, name: String) {
    std::thread::spawn(move || {
        unsafe {
            let _ = windows::Win32::System::Com::CoInitializeEx(
                None,
                windows::Win32::System::Com::COINIT_APARTMENTTHREADED,
            );
            if kind == LOAD_KIND_WINDOW {
                let it = ItemEntry {
                    name,
                    target,
                    icon: None,
                    color: None,
                    kind: 1,
                    relaunch,
                    live_hwnd: hw,
                };
                let r = activate_window_action(&it);
                wlog(&format!("click {} name={} hw={:#x}", r, it.name, it.live_hwnd));
            } else {
                launch(&target);
            }
            post(WM_APP_LAUNCHED, 0, 0);
        }
    });
}

/// 三个跳动的点：**"在等"的通用画法**（外环段内、内环图标位置共用）。
/// 相位在 `0..(2n-2)` 之间**来回**跳（0,1,2,1,0,…）—— 比单向循环更像"在处理"。
unsafe fn draw_wait_dots(buf: &mut [u8], cx: f32, cy: f32, half_w: f32, col: (u8, u8, u8),
                  paper: (u8, u8, u8), alpha: f32) {
    let n = 3usize;
    let step = half_w * 0.52;
    let span = (n * 2 - 2).max(1);
    let ph = ((now_s() * 3.2) as usize) % span;
    let hi = if ph < n { ph } else { span - ph };
    for k in 0..n {
        let on = k == hi;
        let c = if on { col } else { mixc(paper, col, 0.42) };
        let r = if on { 4.0 * K } else { 2.4 * K };
        flat_circle(buf, cx + (k as f32 - 1.0) * step, cy, r, c, 0.0, c, 0.0, 0.0, 0.0, 0.0, alpha);
    }
}

/// 呼出时算一次：每个内环应用"是否正在运行" + 它最前面的窗口 hwnd。
///
/// 匹配靠现成的 `exe_name_of`（.lnk 解析出 exe 文件名）+ `find_window_of_exe`
/// （EnumWindows 找同名前台窗口）—— 和「拖窗口进环 / 切到那个窗口」用同一套，行为一致。
/// 文件夹 / URL / `@act:` 这类没有"进程"的条目永远判不中（它们本来就不是窗口）。
unsafe fn refresh_running(st: &mut State) {
    for i in 0..MAX_APPS {
        st.app_running[i] = false;
        st.app_hwnd[i] = 0;
    }
    let n = st.apps.len().min(MAX_APPS);
    for i in 0..n {
        let t = &st.apps[i].target;
        if t.starts_with("@act:") {
            continue; // 内置动作（开始菜单等）没有"正在运行"的概念
        }
        if let Some(exe) = exe_name_of(t) {
            if let Some(hw) = find_window_of_exe(&exe) {
                st.app_running[i] = true;
                st.app_hwnd[i] = hw.0 as isize;
            }
        }
    }
}

/// 拉起系统开始菜单。没有可靠的 CLSID 能"打开开始菜单"（`shell:::{...}` 那套是搜索/个性化设置），
/// 唯一稳的办法就是**注入一次 Win 键**。
/// ⚠️ 这里是 UI 线程处理点击，不是钩子回调 —— 钩子回调里绝不能注入输入（会变僵尸壳），
/// 普通路径可以。
unsafe fn open_start_menu() {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        SendInput, INPUT, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, VK_LWIN,
    };
    // ⚠️ Win 键是**扩展键**：不带 `KEYEVENTF_EXTENDEDKEY` + 扫描码 `0x5B` 时，
    //    注入的事件会被当成普通按键，系统不会弹开始菜单（实测点了没反应）。
    use windows::Win32::UI::Input::KeyboardAndMouse::KEYEVENTF_EXTENDEDKEY;
    let mut inp = [INPUT::default(); 2];
    inp[0].r#type = INPUT_KEYBOARD;
    inp[0].Anonymous.ki = KEYBDINPUT {
        wVk: VK_LWIN,
        wScan: 0x5B,
        dwFlags: KEYEVENTF_EXTENDEDKEY,
        time: 0,
        dwExtraInfo: 0,
    };
    inp[1].r#type = INPUT_KEYBOARD;
    inp[1].Anonymous.ki = KEYBDINPUT {
        wVk: VK_LWIN,
        wScan: 0x5B,
        dwFlags: KEYEVENTF_EXTENDEDKEY | KEYEVENTF_KEYUP,
        time: 0,
        dwExtraInfo: 0,
    };
    let n = SendInput(&inp, std::mem::size_of::<INPUT>() as i32);
    if std::env::var_os("RL_STARTDBG").is_some() {
        eprintln!("[start] SendInput -> {}", n);
    }
}

fn commit(st: &mut State) {
    // 正在改名时 commit 应该是「提交」，不是「启动」——
    // 正常路径 Enter 已在 WM_APP_KEY 里被 edit 分支截走，这里是兜底
    // （比如鼠标点别的路径绕过了钩子）。
    if st.editing.is_some() {
        unsafe {
            edit_finish(st, true);
        }
        return;
    }
    unlock(st);
    // ── 内置动作（目前只有「开始菜单」）──
    // target 是 `@act:start` 这种前缀，根本不是 exe —— 必须在走 launch 之前分派，
    // 否则会去 `ShellExecuteW` 一个 "@act:start"。
    if let Some(ai) = st.active_app {
        let t = st.apps.get(ai).map(|a| a.target.clone()).unwrap_or_default();
        if t == "@act:start" {
            unsafe {
                hide_menu(st);
                open_start_menu();
            }
            return;
        }
    }
    let action = match (st.active_app, st.hover_chip) {
        (Some(ai), Some(ci)) => st.apps.get(ai).and_then(|a| a.items.get(ci)).map(|it| (ai, Some(ci), it.name.clone(), it.target.clone())),
        (Some(ai), None) => st.apps.get(ai).map(|a| (ai, None, a.name.clone(), a.target.clone())),
        _ => None,
    };

    if let Some((ai, ci, _, _)) = action.clone() {
        st.commit_app = ai;
        st.commit_chip = ci.unwrap_or(usize::MAX);
        st.commit_pulse = 1.0;
    }

    // 窗口条目（kind==1）：点开 = 聚焦活着的活动窗口，否则用命令行重开。
    // ⚠️ 必须**后台 + loading**：找活窗口要 EnumWindows、重开要 CreateProcess，
    //    同步做就是「点了没反应」（菜单挂在那儿等，慢的时候整圈冻住）。
    if let (Some(ai), Some(ci)) = (st.active_app, st.hover_chip) {
        if let Some(it) = st.apps.get(ai).and_then(|a| a.items.get(ci)) {
            if it.kind == 1 {
                let (target, relaunch, name, hw) =
                    (it.target.clone(), it.relaunch.clone(), it.name.clone(), it.live_hwnd);
                start_loading(st, ai, ci, LOAD_KIND_WINDOW);
                run_action_async(LOAD_KIND_WINDOW, target, hw, relaunch, name);
                return;
            }
        }
    }

    // ── 网址 + 浏览器在跑 → **异步**查「是不是已经开着」──
    // ⚠️⚠️ 绝不能同步做：抓标题（联网）+ UI Automation 扫标签要 1~2.5 秒，
    // 同步跑会把 UI 线程连同整个环一起冻住（用户 2026-10-09 报「卡住两三秒」）。
    // 这里先挂上 loading 状态并**保持菜单可见**（宠物切「忙碌」、该段显示跳动的点），
    // 查完由 `WM_APP_LOADED` 收尾。
    if let Some((ai, ci, label, target)) = action.clone() {
        if is_url(&target) && unsafe { !windows_of_class("Chrome_WidgetWin_1").is_empty() } {
            if let Some(cix) = ci {
                start_loading(st, ai, cix, LOAD_KIND_URL);
                st.loading_url = target.clone();
                println!("[*] 查询是否已打开 · {label}");
                switch_bg(target);
                return;
            }
        }
    }

    if let Some((ai, ci, label, target)) = action {
        // 普通条目（内环应用本体 / 外环快捷方式、文件夹）：启动同样是"慢操作" ——
        // `.lnk` 解析要 COM、命中已开窗口要 EnumWindows，慢起来就是「点了没反应」。
        // 内环用 `LOADING_HUB` 哨兵 → 图标位置显示跳动的点（外环则是胶囊文字位置）。
        start_loading(st, ai, ci.unwrap_or(LOADING_HUB), LOAD_KIND_LAUNCH);
        println!("[*] 启动 · {label}");
        run_action_async(LOAD_KIND_LAUNCH, target, 0, String::new(), String::new());
        return;
    }

    unsafe {
        hide_menu(st);
    }
}

/// 这个 hwnd **还是不是**那条条目记的窗口？
///
/// ⚠️ `IsWindow` 远远不够：HWND 会被系统**复用**（窗口关掉后同一个值可能给别的窗口），
///    也可能指向捕获时误拿的伪窗口（explorer 一个文件夹窗口有多个 hwnd）。
///    只判 IsWindow 就会「聚焦一个不相干的窗口」——用户眼里就是点了没反应。
unsafe fn window_matches_item(hw: HWND, target: &str, name: &str) -> bool {
    match window_exe(hw) {
        Some(e) if e.eq_ignore_ascii_case(target) => {}
        _ => return false,
    }
    let t = window_title(hw).to_ascii_lowercase();
    let n = name.to_ascii_lowercase();
    if t.is_empty() || n.is_empty() {
        return true; // 有一边没标题就不苛求（总比直接判死好）
    }
    // 条目名是**截断过**的窗口标题（`clamp_name` 20 字），两边互相包含都算对
    t.contains(&n) || n.contains(&t)
}


// ───────────────────── 圆环窗口 ─────────────────────

// ═══════════════════════════════════════════════════════════════════
//  窗口缩略图（悬停外环胶囊 → 实时小窗，像 Windows 任务栏）
// ═══════════════════════════════════════════════════════════════════
//
// **为什么用 DWM 缩略图而不是 PrintWindow 截图**：
//   · 缩略图由 DWM 直接合成 → 视频/动画是**活的**，CPU 几乎为零；
//   · 截图只有一帧，而且最小化 / 被遮挡的窗口经常抓成黑块。
//
// **为什么必须是独立窗口**：`DwmRegisterThumbnail` 的目标不能是 ULW 分层窗口
// （RLRing 就是），DWM 不会在它上面合成缩略图。所以另开一个**普通**顶层窗口：
// `WS_EX_TOOLWINDOW`（不进任务栏）+ `TOPMOST` + `NOACTIVATE`（不抢焦点）+
// `TRANSPARENT`（鼠标穿透，绝不会挡住点胶囊）。

static THUMB_ID: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);

unsafe extern "system" fn thumb_proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    match msg {
        WM_ERASEBKGND => LRESULT(1),
        WM_PAINT => {
            // 只在"缩略图不可用"时画字（最小化窗口 DWM 给的是黑块）
            let note = THUMB_NOTE.lock().ok().and_then(|g| g.clone());
            if let Some(t) = note {
                let mut ps = PAINTSTRUCT::default();
                let hdc = BeginPaint(hwnd, &mut ps);
                let mut r = RECT::default();
                let _ = GetClientRect(hwnd, &mut r);
                let _ = SetBkMode(hdc, TRANSPARENT);
                let _ = SetTextColor(hdc, COLORREF(0x00A8A8B2));
                let font = GetStockObject(DEFAULT_GUI_FONT);
                let old = SelectObject(hdc, font);
                let mut wt: Vec<u16> = wide(&t);
                let mut rr = r;
                let n = wt.len().saturating_sub(1);
                DrawTextW(hdc, &mut wt[..n], &mut rr, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
                let _ = SelectObject(hdc, old);
                let _ = EndPaint(hwnd, &ps);
                return LRESULT(0);
            }
            let _ = DefWindowProcW(hwnd, msg, w, l);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, w, l),
    }
}

/// 懒创建缩略图窗口（第一次要显示时才建）。返回 invalid = 失败。
unsafe fn thumb_ensure() -> HWND {
    let h = THUMB_HWND.load(Ordering::Relaxed);
    if h != 0 {
        return HWND(h as *mut c_void);
    }
    let hinst = HINSTANCE(GetModuleHandleW(None).unwrap_or_default().0);
    let cls: Vec<u16> = wide("RLThumb");
    // 深色卡片底：Windows 的任务栏预览就是不透明卡片，这里跟 PiPie 的灰调一致
    let brush = CreateSolidBrush(COLORREF(0x00202024));
    let _ = RegisterClassW(&WNDCLASSW {
        lpfnWndProc: Some(thumb_proc),
        hInstance: hinst,
        lpszClassName: PCWSTR(cls.as_ptr()),
        hbrBackground: brush,
        ..Default::default()
    });
    let hw = CreateWindowExW(
        WINDOW_EX_STYLE(
            WS_EX_TOOLWINDOW.0 | WS_EX_TOPMOST.0 | WS_EX_NOACTIVATE.0 | WS_EX_TRANSPARENT.0,
        ),
        PCWSTR(cls.as_ptr()),
        PCWSTR::null(),
        WS_POPUP,
        0,
        0,
        THUMB_W,
        THUMB_H,
        HWND(ptr::null_mut()),
        HMENU(ptr::null_mut()),
        hinst,
        None,
    )
    .unwrap_or(HWND(ptr::null_mut()));
    if hw.is_invalid() {
        return hw;
    }
    // Win11 圆角（Win10 上会失败，忽略 —— 那台机器就是直角卡片）
    let round: u32 = 2; // DWMWCP_ROUND
    let _ = windows::Win32::Graphics::Dwm::DwmSetWindowAttribute(
        hw,
        windows::Win32::Graphics::Dwm::DWMWINDOWATTRIBUTE(33),
        &round as *const u32 as *const core::ffi::c_void,
        4,
    );
    THUMB_HWND.store(hw.0 as isize, Ordering::Relaxed);
    hw
}

/// 只注销缩略图源（换目标时用，随后立刻重新注册）
unsafe fn thumb_detach() {
    let old = THUMB_ID.load(Ordering::Relaxed);
    if old != 0 {
        let _ = windows::Win32::Graphics::Dwm::DwmUnregisterThumbnail(old);
        THUMB_ID.store(0, Ordering::Relaxed);
    }
    THUMB_SRC.store(0, Ordering::Relaxed);
}

/// 收起预览（离开胶囊 / 关菜单 / 不是窗口条目）
unsafe fn thumb_hide() {
    thumb_detach();
    THUMB_KEY.store(usize::MAX, Ordering::Relaxed);
    if let Ok(mut t) = THUMB_SINCE.lock() {
        *t = 0.0;
    }
    THUMB_POS.store(i64::MIN, Ordering::Relaxed);
    if let Ok(mut g) = THUMB_NOTE.lock() {
        *g = None;
    }
    let h = THUMB_HWND.load(Ordering::Relaxed);
    if h != 0 {
        let _ = ShowWindow(HWND(h as *mut c_void), SW_HIDE);
    }
}

/// 把源窗口的实时缩略图挂到宿主窗口上，并摆到 `(x, y)`。
unsafe fn thumb_apply(src_hwnd: isize, at: (i32, i32)) {
    let dst = thumb_ensure();
    if dst.is_invalid() || src_hwnd == 0 {
        return;
    }
    let src = HWND(src_hwnd as *mut c_void);
    if !IsWindow(src).as_bool() {
        thumb_hide();
        return;
    }
    // ⚠️ **先把目标窗口显示出来，再注册缩略图**。
    //    源窗口最小化时 DWM 给的是「最小化那一刻的快照」，但目标窗口还藏着的时候
    //    注册经常拿不到东西（出来一块纯黑）——顺序反了就是"最小化窗口没预览"。
    {
        let packed = ((at.0 as i64) << 32) | (at.1 as i64 & 0xffff_ffff);
        let was_hidden = !IsWindowVisible(dst).as_bool();
        if THUMB_POS.swap(packed, Ordering::Relaxed) != packed || was_hidden {
            let _ = SetWindowPos(
                dst,
                HWND_TOPMOST,
                at.0,
                at.1,
                THUMB_W,
                THUMB_H,
                SWP_NOACTIVATE | SWP_SHOWWINDOW,
            );
        }
    }
    if THUMB_NOTE.lock().map(|g| g.is_some()).unwrap_or(false) {
        if let Ok(mut g) = THUMB_NOTE.lock() {
            *g = None;
        }
        let _ = InvalidateRect(dst, None, true);
    }
    if THUMB_SRC.load(Ordering::Relaxed) != src_hwnd {
        thumb_detach();
        match windows::Win32::Graphics::Dwm::DwmRegisterThumbnail(dst, src) {
            Ok(id) => {
                THUMB_ID.store(id, Ordering::Relaxed);
                THUMB_SRC.store(src_hwnd, Ordering::Relaxed);
            }
            Err(_) => return,
        }
    }
    let id = THUMB_ID.load(Ordering::Relaxed);
    if id == 0 {
        return;
    }
    use windows::Win32::Graphics::Dwm::{
        DwmUpdateThumbnailProperties, DWM_THUMBNAIL_PROPERTIES, DWM_TNP_OPACITY,
        DWM_TNP_RECTDESTINATION, DWM_TNP_SOURCECLIENTAREAONLY, DWM_TNP_VISIBLE,
    };
    let pad = 8;
    let mut pr = DWM_THUMBNAIL_PROPERTIES::default();
    pr.dwFlags = DWM_TNP_RECTDESTINATION
        | DWM_TNP_VISIBLE
        | DWM_TNP_OPACITY
        | DWM_TNP_SOURCECLIENTAREAONLY;
    pr.rcDestination = RECT {
        left: pad,
        top: pad,
        right: THUMB_W - pad,
        bottom: THUMB_H - pad,
    };
    pr.opacity = 255;
    pr.fVisible = BOOL(1);
    pr.fSourceClientAreaOnly = BOOL(1);
    let _ = DwmUpdateThumbnailProperties(id, &pr);

}

/// 每帧调一次：决定"该不该弹、弹在哪"。
///
/// 只在**悬停外环里一个窗口条目**（`kind==1` 且有 `live_hwnd`）时弹；
/// 稳定 `THUMB_DELAY` 秒才出现（掠过不闪）。
unsafe fn thumb_tick(st: &mut State) {
    if !MENU_OPEN.load(Ordering::Relaxed) || st.hiding {
        thumb_hide();
        return;
    }
    // ① 悬停外环里一个窗口条目（kind==1 且有 live_hwnd）→ 预览那个窗口
    let want_chip = match (st.active_app, st.hover_chip) {
        (Some(ai), Some(ci)) => st
            .apps
            .get(ai)
            .and_then(|a| a.items.get(ci))
            .filter(|it| it.kind == 1 && it.live_hwnd != 0)
            .map(|it| (ai, ci, it.live_hwnd)),
        _ => None,
    };
    // ② 悬停内环一个**正在运行**的应用 → 预览它最前面的窗口（合并后默认屏的功能）
    // ⚠️ **有外环的应用不弹内环预览**（用户 2026-10-17 定）：它的窗口已经一条条
    //    摊在外环上了，内环再弹一张「最前面的窗口」既重复、又会把外环胶囊压住。
    let want_app = if want_chip.is_none() {
        if let Some(ai) = st.hover_now {
            let has_ring = st.apps.get(ai).map(|a| !a.items.is_empty()).unwrap_or(true);
            if !has_ring && ai < MAX_APPS && st.app_running[ai] && st.app_hwnd[ai] != 0 {
                Some((ai, st.app_hwnd[ai]))
            } else {
                None
            }
        } else {
            None
        }
    } else {
        None
    };

    // ── 决定 key / 窗口 hwnd / 预览位置 ──
    let (key, src, pos): (usize, isize, (f32, f32)) = if let Some((ai, ci, hw)) = want_chip {
        let (segs, _rots, rads) = chip_layout(st, ai);
        if ci >= segs.len() {
            thumb_hide();
            return;
        }
        let mid = (segs[ci].0 + segs[ci].1) * 0.5;
        let (cxp, cyp) = polar(rads[ci], mid);
        let dx = cxp - CC;
        let dy = cyp - CC;
        let len = (dx * dx + dy * dy).sqrt().max(1.0);
        let gap = 54.0;
        let half_diag = ((THUMB_W * THUMB_W + THUMB_H * THUMB_H) as f32).sqrt() * 0.5;
        let px = st.win_x as f32 + cxp + dx / len * (half_diag * 0.55 + gap) - THUMB_W as f32 * 0.5;
        let py = st.win_y as f32 + cyp + dy / len * (half_diag * 0.55 + gap) - THUMB_H as f32 * 0.5;
        (ai * 1000 + ci, hw, (px, py))
    } else if let Some((ai, hw)) = want_app {
        // 与绘制同源：有幽灵时整环按 `n+1` 排，预览也必须跟着幽灵那一格走
        let n = eff_apps(st);
        let (r_icon, _icon_sz) = ring_layout(n);
        let s = if st.app_slot_s[ai].is_finite() {
            st.app_slot_s[ai]
        } else {
            slot_of(st, ai)
        };
        let (cxp, cyp) = polar(r_icon, slot_angle(s, n));
        let dx = cxp - CC;
        let dy = cyp - CC;
        let len = (dx * dx + dy * dy).sqrt().max(1.0);
        let gap = 42.0; // 内环图标 → 预览边
        let half_diag = ((THUMB_W * THUMB_W + THUMB_H * THUMB_H) as f32).sqrt() * 0.5;
        let px = st.win_x as f32 + cxp + dx / len * (half_diag * 0.55 + gap) - THUMB_W as f32 * 0.5;
        let py = st.win_y as f32 + cyp + dy / len * (half_diag * 0.55 + gap) - THUMB_H as f32 * 0.5;
        (200000 + ai, hw, (px, py))
    } else {
        thumb_hide();
        return;
    };

    let now = now_s();
    if THUMB_KEY.load(Ordering::Relaxed) != key {
        THUMB_KEY.store(key, Ordering::Relaxed);
        if let Ok(mut t) = THUMB_SINCE.lock() {
            *t = now;
        }
        // 换了目标 → 先摘掉旧的（避免旧画面闪一下）
        if THUMB_SRC.load(Ordering::Relaxed) != 0 {
            thumb_detach();
        }
        return;
    }
    let since = THUMB_SINCE.lock().map(|t| *t).unwrap_or(0.0);
    if since <= 0.0 || now - since < THUMB_DELAY {
        return;
    }
    // 钳到工作区，别飘出屏幕
    let mut mi = MONITORINFO::default();
    mi.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
    let mon = MonitorFromWindow(
        HWND(RING_HWND.load(Ordering::Relaxed) as *mut c_void),
        MONITOR_DEFAULTTONEAREST,
    );
    let (mut px, mut py) = pos;
    if GetMonitorInfoW(mon, &mut mi).as_bool() {
        let w = mi.rcWork;
        px = px.clamp(w.left as f32, (w.right - THUMB_W) as f32);
        py = py.clamp(w.top as f32, (w.bottom - THUMB_H) as f32);
    }
    thumb_apply(src, (px as i32, py as i32));
}

unsafe extern "system" fn ring_proc(hwnd: HWND, msg: u32, _w: WPARAM, _l: LPARAM) -> LRESULT {
    match msg {
        WM_SETCURSOR => {
            // 窗口接收鼠标后必须自设光标：拖放期间系统可能留下忙碌指针，
            // 类光标为空时 Windows 不会自动改回，于是"一直转圈"
            unsafe {
                let _ = SetCursor(LoadCursorW(None, IDC_ARROW).unwrap_or(HCURSOR(ptr::null_mut())));
            }
            LRESULT(1)
        }
        WM_NCHITTEST => {
            // 菜单开着 = 接收鼠标（拖放定位/命中需要）；不透明像素之外
            // 的纯透明区域由分层窗口机制自动穿透
            if MENU_OPEN.load(Ordering::Relaxed) {
                LRESULT(HTCLIENT as isize)
            } else {
                LRESULT(HTTRANSPARENT as isize)
            }
        }
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        WM_ERASEBKGND => LRESULT(1),
        _ => DefWindowProcW(hwnd, msg, _w, _l),
    }
}

// ───────────────────── 离屏调试导出 ─────────────────────

unsafe fn save_bmp(path: &str, bgra: &[u8], w: i32, h: i32) -> bool {
    use std::io::Write;
    let mut out: Vec<u8> = Vec::with_capacity(bgra.len() + 1078);
    // 每行按 4 字节对齐填充 —— 必须按**行宽**算，不能按总像素数算，
    // 否则宽度不是 4 的倍数时文件会短一截（BMP 损坏）。
    let row = w as u32 * 3;
    let pad = (4 - row % 4) % 4;
    let stride = row + pad;
    let data_len = stride * h as u32;
    out.extend_from_slice(b"BM");
    out.extend_from_slice(&(54u32 + data_len).to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&54u32.to_le_bytes());
    out.extend_from_slice(&40u32.to_le_bytes());
    out.extend_from_slice(&w.to_le_bytes());
    out.extend_from_slice(&h.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&24u16.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&data_len.to_le_bytes());
    out.extend_from_slice(&[0u8; 16]);
    for y in (0..h as usize).rev() {
        for x in 0..w as usize {
            let o = (y * w as usize + x) * 4;
            out.push(bgra[o]); // B
            out.push(bgra[o + 1]); // G
            out.push(bgra[o + 2]); // R
        }
        for _ in 0..pad {
            out.push(0);
        }
    }
    std::fs::File::create(path).and_then(|mut f| f.write_all(&out)).is_ok()
}

/// 裸 BGRA（top-down，无文件头）落盘 —— 只给离屏分析用。
/// `save_bmp` 写的是 24bpp，**alpha 会被整条丢掉**，做图标对照稿时不够用
/// （透明底图标在对照稿里会变成白底图块，看不出真实轮廓）。
fn save_rgba(path: &str, bgra: &[u8]) -> bool {
    use std::io::Write;
    std::fs::File::create(path)
        .and_then(|mut f| f.write_all(bgra))
        .is_ok()
}

unsafe fn synth_bg(st: &mut State, gray: bool) {    // 彩色壁纸（macOS Sonoma 风格），便于离屏校验玻璃染色
    let n = (CANVAS * CANVAS) as usize;
    let buf = std::slice::from_raw_parts_mut(st.bg_bits, n * 4);
    for y in 0..CANVAS as usize {
        for x in 0..CANVAS as usize {
            let u = x as f32 / CANVAS as f32;
            let v = y as f32 / CANVAS as f32;
            // 蓝紫底 + 三个光斑
            let mut r = 0.42 + 0.20 * (1.0 - v) + 0.10 * u;
            let mut g = 0.46 + 0.16 * (1.0 - u) + 0.06 * v;
            let mut b = 0.92 - 0.10 * v + 0.06 * u;
            let blob = |cx: f32, cy: f32, rad: f32, amp: f32, ch: usize, c: &mut [f32; 3]| {
                let d = ((u - cx).powi(2) + (v - cy).powi(2)).sqrt();
                let k = (1.0 - (d / rad).clamp(0.0, 1.0)).powi(2) * amp;
                c[ch] += k;
            };
            let mut c = [r, g, b];
            blob(0.18, 0.12, 0.55, 0.42, 0, &mut c);
            blob(0.18, 0.12, 0.55, 0.40, 1, &mut c);
            blob(0.18, 0.12, 0.55, 0.36, 2, &mut c);
            blob(0.86, 0.20, 0.50, 0.20, 0, &mut c);
            blob(0.86, 0.20, 0.50, 0.24, 1, &mut c);
            blob(0.86, 0.20, 0.50, 0.46, 2, &mut c);
            blob(0.70, 0.92, 0.52, 0.40, 0, &mut c);
            blob(0.70, 0.92, 0.52, 0.24, 1, &mut c);
            blob(0.70, 0.92, 0.52, 0.05, 2, &mut c);
            r = c[0].clamp(0.0, 1.0);
            g = c[1].clamp(0.0, 1.0);
            b = c[2].clamp(0.0, 1.0);
            let o = (y * CANVAS as usize + x) * 4;
            if gray {
                // 近白灰桌面：盘面必须仍然是一块亮玻璃，不能跟着塌成中灰
                let l = 0.299 * r + 0.587 * g + 0.114 * b;
                let v = 0.62 + 0.30 * l;
                buf[o] = (v * 255.0) as u8;
                buf[o + 1] = (v * 255.0) as u8;
                buf[o + 2] = (v * 255.0) as u8;
            } else {
                buf[o] = (b * 255.0) as u8;
                buf[o + 1] = (g * 255.0) as u8;
                buf[o + 2] = (r * 255.0) as u8;
            }
            buf[o + 3] = 255;
        }
    }
}

unsafe fn draw_to_buf(st: &mut State) -> Vec<u8> {
    let n = (CANVAS * CANVAS * 4) as usize;
    let buf = std::slice::from_raw_parts_mut(st.dbi_bits, n);
    draw_scene(st, buf);
    std::slice::from_raw_parts(st.dbi_bits, n).to_vec()
}

unsafe fn dump_all(dir: &str) {
    let mut st = State::new();
    st.dump_bg = true;
    st.ini_path = format!("{}\\settings.ini", dir);
    if !std::path::Path::new(&st.ini_path).exists() {
        let _ = std::fs::write(&st.ini_path, default_ini());
    }
    load_ini(&mut st);
    init_surfaces(&mut st);
    init_fonts(&mut st);
    synth_bg(&mut st, false);
    st.ent0 = -10.0; // 动画已收敛
    st.switch0 = -10.0;

    // 1) 静置态（无悬停）
    st.entering = false;
    st.active_app = None;
    st.hover_chip = None;
    eprintln!("[cp4] drawing disc");
    let buf = draw_to_buf(&mut st);
    eprintln!("[cp5] disc drawn");
    save_bmp(&format!("{}\\dump_disc.bmp", dir), &buf, CANVAS, CANVAS);

    // 1b) 滚动：应用超过一屏（`RING_SLOTS`）时再多出三张 ——
    //     第 1 页 / 中间整页 / 交叉淡化最坏情况（偏移 = 1/4 格，两端同时半透明）/ 最后一页。
    //     用于离线核对「让位的那格有没有压到中心头像」「底部刻度走位对不对」。
    if st.apps.len() > RING_SLOTS {
        let mx = ring_off_max(st.apps.len());
        for (tag, off) in [
            ("p1", 0.0f32),
            ("mid", (mx * 0.5).round().max(0.5)),
            ("xfade", mx * 0.5 + 0.25),
            ("last", mx),
        ] {
            st.ring_off = off;
            st.ring_to = off;
            st.app_slot_s = [f32::NAN; MAX_APPS];
            let buf = draw_to_buf(&mut st);
            save_bmp(&format!("{}\\dump_scroll_{}.bmp", dir, tag), &buf, CANVAS, CANVAS);
        }
        st.ring_off = 0.0;
        st.ring_to = 0.0;
    }

    // 2) 悬停态（第一个有内容的应用展开 chips + 选中环 + 压暗其他）
    if let Some(hi) = st
        .apps
        .iter()
        .enumerate()
        .max_by_key(|(_, a)| a.items.len())
        .filter(|(_, a)| !a.items.is_empty())
        .map(|(i, _)| i)
    {
        eprintln!("[cp6] hover app {} items {}", hi, st.apps[hi].items.len());
        st.active_app = Some(hi);
        st.hover_chip = Some(0);
        for (i, v) in st.app_anim.iter_mut().enumerate() {
            *v = if i == hi { 0.0 } else { 1.0 };
        }
        for v in st.chip_anim.iter_mut() {
            *v = 1.0;
        }
        for (i, v) in st.app_hot.iter_mut().enumerate() {
            *v = if i == hi { 1.0 } else { 0.0 };
        }
        for (j, v) in st.chip_hot.iter_mut().enumerate() {
            *v = if j == 0 { 1.0 } else { 0.0 };
        }
        let buf = draw_to_buf(&mut st);
        save_bmp(&format!("{}\\dump_hover.bmp", dir), &buf, CANVAS, CANVAS);

        // 2b) 改名编辑态：同一个 chip 换成墨底 + 输入缓冲（核对对比度与光标）
        let real_name = st.apps[hi].items[0].name.clone();
        st.editing = Some((hi, 0));
        st.edit_buf = format!("{}·改", real_name);
        st.edit_caret = now_s(); // 让光标落在「亮」的相位上
        let buf = draw_to_buf(&mut st);
        let edit_path = [dir, "\\dump_edit.bmp"].concat();
        save_bmp(&edit_path, &buf, CANVAS, CANVAS);
        st.editing = None;
        st.edit_buf.clear();

        // 2c) 气泡：hover 内环 → 应用名（宽度按文字量收缩，这条只有 2 个字）
        st.bub_a = 1.0;
        st.hover_now = Some(hi);
        let buf = draw_to_buf(&mut st);
        save_bmp(&format!("{}\\dump_bub_name.bmp", dir), &buf, CANVAS, CANVAS);
        // 2d) 气泡：状态消息（主行 + 副行，最长的一档 → 看会不会顶到上限 150）
        st.hover_now = None;
        say(&mut st, "已添加：Visual Studio Code", "拖进来就能用，右键可改", 99.0);
        let buf = draw_to_buf(&mut st);
        save_bmp(&format!("{}\\dump_bub_add.bmp", dir), &buf, CANVAS, CANVAS);
        // 2d-2) 气泡：发现新版本（用户 2026-10-27 要头像提醒）—— 文案必须和 `update_say` 一致
        say(&mut st, "发现新版本 v1.0.7", "", 99.0);
        let buf = draw_to_buf(&mut st);
        save_bmp(&format!("{}\\dump_bub_update.bmp", dir), &buf, CANVAS, CANVAS);
        // 2e) 气泡：短消息（验证「按文字量收缩」而不是一律撑满）
        say(&mut st, "已移除：终端", "", 99.0);
        let buf = draw_to_buf(&mut st);
        save_bmp(&format!("{}\\dump_bub_short.bmp", dir), &buf, CANVAS, CANVAS);
        // 2f) 深色主题下的气泡 —— 用户用的是深色，必须确认对比度没反
        //     （块色/字色在深浅下是同一套 token，搞反了就是白底白字）
        st.glass_liquid = true;
        say(&mut st, "已添加：Chrome", "拖进来就能用，右键可改", 99.0);
        let buf = draw_to_buf(&mut st);
        save_bmp(&format!("{}\\dump_bub_dark.bmp", dir), &buf, CANVAS, CANVAS);
        st.glass_liquid = false;
        st.bub_until = 0.0;
        st.bub_a = 0.0;

        // 2g) 拖拽插入光标：拖着 0 号图标、悬在「插到 2 号位」的缝上
        if st.apps.len() >= 3 {
            let nap = st.apps.len();
            st.drag_app = Some(0);
            st.drag_over_app = Some(2);
            st.drag_fade = 1.0;
            st.drag_x = CC + 96.0;
            st.drag_y = CC - 66.0;
            // ⚠️ 离线 dump 不走 `on_timer`，让位动画不会自己跑 —— 手动把**槽位**
            //    落到「预览排序」的终态，否则图标还停在原位、缝没开。
            for i in 0..nap {
                st.app_slot_s[i] = drag_preview_slot(0, 2, i) as f32 - st.ring_off;
            }
            let buf = draw_to_buf(&mut st);
            save_bmp(&format!("{}\\dump_drag_caret.bmp", dir), &buf, CANVAS, CANVAS);
            st.drag_app = None;
            st.drag_over_app = None;
            st.drag_fade = 0.0;
            st.drag_x = 0.0;
            st.drag_y = 0.0;
            st.app_slot_s = [f32::NAN; MAX_APPS];
        }

        // 2h) 外环插入光标：拖着 0 号胶囊、插到 2 号位的缝里
        if st.apps[hi].items.len() >= 3 {
            let m = st.apps[hi].items.len();
            st.active_app = Some(hi);
            st.drag_chip = Some((hi, 0));
            st.drag_over_chip = Some(2);
            // ⚠️ 盘内拖动 → `drag_fade = 0`（改了 `want_fade` 之后就是这样）。
            //    这正是光标门控失效的那个场景，出图必须按真实状态设。
            st.drag_fade = 0.0;
            st.chip_seg_app = hi;
            // 同样：离线 dump 不走 on_timer，手动落到预览终态
            let pv = chip_preview_segs(&st, hi, 0, 2);
            for k in 0..m.min(MAX_ITEMS) {
                st.chip_seg_a[k] = pv.get(k).copied().unwrap_or((f32::NAN, f32::NAN));
            }
            let buf = draw_to_buf(&mut st);
            save_bmp(&format!("{}\\dump_drag_chip_caret.bmp", dir), &buf, CANVAS, CANVAS);
            println!("[dump] dump_drag_chip_caret 外环插入光标（缝中，drag_fade=0）");
            st.drag_chip = None;
            st.drag_over_chip = None;
            st.drag_fade = 0.0;
            st.chip_seg_a = [(f32::NAN, f32::NAN); MAX_ITEMS];
            st.chip_seg_app = usize::MAX;
        }

        // ── 2h2) 拖入窗口（**认得出宿主**）：外环末尾的幽灵胶囊 ──
        // 这是内环 `drop_ghost` 磁贴的外环对称物：虚线轮廓 + ＋ + 将要新增的名字，
        // 已有胶囊整体让位一格。松手落点 = 这一格（追加到外环末尾）。
        {
            st.drag_chip = None;
            st.drag_over_chip = None;
            st.active_app = Some(hi);
            st.drop_suggest = Some(hi);
            st.window_capture = Some(WindowCapture {
                hwnd: 0,
                exe: "c:\\windows\\explorer.exe".to_string(),
                title: "PiPie - 文件资源管理器".to_string(),
                relaunch: String::new(),
                test: true,
            });
            st.chip_ghost_a = 1.0;
            st.chip_seg_app = hi;
            // 离线 dump 不走 on_timer → 手动把段角落到「含幽灵」的终态
            let segs = chip_layout(&st, hi).0;
            for k in 0..st.apps[hi].items.len().min(MAX_ITEMS) {
                st.chip_seg_a[k] = segs.get(k).copied().unwrap_or((f32::NAN, f32::NAN));
            }
            let buf = draw_to_buf(&mut st);
            save_bmp(&format!("{}\\dump_drop_chip.bmp", dir), &buf, CANVAS, CANVAS);
            println!("[dump] dump_drop_chip 拖入窗口 → 外环末尾幽灵胶囊（虚线 + ＋ + 名字）");
            st.drop_suggest = None;
            st.window_capture = None;
            st.chip_ghost_a = 0.0;
            st.chip_seg_a = [(f32::NAN, f32::NAN); MAX_ITEMS];
            st.chip_seg_app = usize::MAX;
        }

        // ── 2h3) loading：内环应用本体（图标淡出 + 三个跳动的点）──
        {
            st.active_app = Some(hi);
            st.loading = Some((hi, LOADING_HUB));
            st.mood = 5;
            let buf = draw_to_buf(&mut st);
            save_bmp(&format!("{}\\dump_loading_hub.bmp", dir), &buf, CANVAS, CANVAS);
            println!("[dump] dump_loading_hub 内环 loading（图标淡出 + 跳动的点）");
            st.loading = None;
            st.mood = 0;
        }

        // ── 2h4) loading：外环某一格（名字位置换成跳动的点）──
        {
            st.active_app = Some(hi);
            st.loading = Some((hi, 0));
            st.mood = 5;
            let buf = draw_to_buf(&mut st);
            save_bmp(&format!("{}\\dump_loading_chip.bmp", dir), &buf, CANVAS, CANVAS);
            println!("[dump] dump_loading_chip 外环 loading（胶囊内跳动的点）");
            st.loading = None;
            st.mood = 0;
        }
    }

    // ── 2i) onboarding ①：教学虚线槽位 + 气泡（还没拖入过）──
    {
        st.active_app = None;
        st.drag_app = None;
        st.drag_chip = None;
        st.drop_ghost = None;
        st.window_capture = None;
        st.onb_drag = false;
        st.bub_main = "拖文件或窗口到这里".to_string();
        st.bub_sub = "应用 · 文件夹 · 网址都行".to_string();
        st.bub_until = now_s() + 999.0;
        st.bub_a = 1.0;
        let buf = draw_to_buf(&mut st);
        save_bmp(&format!("{}\\dump_onb1.bmp", dir), &buf, CANVAS, CANVAS);
        println!("[dump] dump_onb1 教学槽位（内环末位虚线格）");
        st.bub_until = 0.0;
        st.bub_a = 0.0;
    }

    // ── 2j) onboarding ③：拖动中 —— 被拖图标**仍可见** + 整圈虚线移除区 ──
    {
        st.onb_drag = true;
        st.active_app = None;
        if st.apps.len() >= 3 {
            st.drag_app = Some(0);
            st.drag_over_app = Some(0);
            // ⚠️ 盘内拖动：改过 `want_fade` 之后这里必须是 0（图标可见），
            //    否则说明淡出逻辑又退回「拖起即隐身」了。
            st.drag_fade = 0.0;
            st.drag_x = CC + 96.0;
            st.drag_y = CC - 66.0;
            st.bub_main = "松手换位".to_string();
            st.bub_sub = "拖到盘外 = 移除".to_string();
            st.bub_until = now_s() + 999.0;
            st.bub_a = 1.0;
            let buf = draw_to_buf(&mut st);
            save_bmp(&format!("{}\\dump_onb3.bmp", dir), &buf, CANVAS, CANVAS);
            println!("[dump] dump_onb3 拖动中（图标可见 · 无插入光标 · 相邻图标无白框 · 移除圈已隐藏）");
            st.drag_app = None;
            st.drag_over_app = None;
            st.drag_fade = 0.0;
            st.bub_until = 0.0;
            st.bub_a = 0.0;
        }
    }

    // ── 2j2) 同上，但光标已**越过移除线** → 虚线环转起来（`rm_spin` 相位）──
    {
        st.onb_drag = true;
        st.active_app = None;
        if st.apps.len() >= 3 {
            st.drag_app = Some(0);
            st.drag_over_app = None;
            st.drag_fade = 1.0; // 已出界 → 被拖的淡掉
            let rr = remove_r(st.apps.len());
            // ⚠️ `drag_x/y` 是**画布坐标**：放到圈外（正下方 rr+14）
            st.drag_x = CC;
            st.drag_y = CC + rr + 14.0;
            // ⚠️ 相位取**半个间距**（15°/2 = 7.5°）：转整格图案不变，出图核对不出来。
            st.rm_spin = 7.5;
            st.rm_spin_k = 1.0;
            st.bub_main = "松手就移除".to_string();
            st.bub_sub = String::new();
            st.bub_until = now_s() + 999.0;
            st.bub_a = 1.0;
            let buf = draw_to_buf(&mut st);
            save_bmp(&format!("{}\\dump_rm_armed.bmp", dir), &buf, CANVAS, CANVAS);
            println!("[dump] dump_rm_armed 越过移除线（虚线旋转到相位 7.5° = 半个间距）");
            st.drag_app = None;
            st.drag_over_app = None;
            st.drag_fade = 0.0;
            st.rm_spin = 0.0;
            st.rm_spin_k = 0.0;
            st.bub_until = 0.0;
            st.bub_a = 0.0;
        }
    }

    // ── 3) 灰底变体（模拟浅灰桌面，检查盘面是否仍然明亮通透）
    synth_bg(&mut st, true);
    st.active_app = Some(0);
    st.hover_chip = None;
    for v in st.chip_anim.iter_mut() {
        *v = 1.0;
    }
    let buf = draw_to_buf(&mut st);
    save_bmp(&format!("{}\\dump_disc_gray.bmp", dir), &buf, CANVAS, CANVAS);
    synth_bg(&mut st, false);

    // 3.5) 侧面方位：选「有子项且方位角最靠近 90°」的应用，
    //      验证「贴弧胶囊」在 3 点方向依然可读（旋转最容易在这一档崩）
    {
        let ns = st.apps.len();
        let mut best: Option<usize> = None;
        let mut bd = f32::MAX;
        for i in 0..ns {
            if st.apps[i].items.is_empty() {
                continue;
            }
            let d = (slot_angle(slot_of(&st, i), ns) - 90.0).abs();
            if d < bd {
                bd = d;
                best = Some(i);
            }
        }
        if let Some(b) = best {
            st.active_app = Some(b);
            st.hover_chip = None;
            for v in st.chip_anim.iter_mut() {
                *v = 1.0;
            }
            for (i, v) in st.app_hot.iter_mut().enumerate() {
                *v = if i == b { 1.0 } else { 0.0 };
            }
            let buf = draw_to_buf(&mut st);
            save_bmp(&format!("{}\\dump_side.bmp", dir), &buf, CANVAS, CANVAS);
            st.app_hot = [0.0; MAX_APPS];
            st.chip_anim = [0.0; MAX_ITEMS];
        }
    }

    // 3.75) 液态玻璃（用户 2026-10-17：「淡」+「只做轮盘」）
    //       离屏没有真桌面 → 用 `synth_bg` 的彩色壁纸当后台，走**与真机同一条**
    //       `blur_glass` + `glass_disc`。明/暗两个主题各一张（tint 不同）。
    {
        let n = (CANVAS * CANVAS * 4) as usize;
        let raw = std::slice::from_raw_parts(st.bg_bits, n).to_vec();
        {
            let mut buf = Vec::with_capacity(raw.len() * 2);
            buf.extend_from_slice(&raw);
            buf.extend_from_slice(&blur_glass(&raw));
            set_glass_bg(&mut st, buf);
        }
        st.glass = true;
        // 挑一个**真的有外环条目**的应用显示外环（玻璃段也要一起看）
        // 优先挑**带窗口条目**的应用：那份图能同时看到玻璃胶囊 + 已关闭的灰显
        let ghi = st
            .apps
            .iter()
            .position(|a| a.items.iter().any(|it| it.kind == 1))
            .or_else(|| st.apps.iter().position(|a| !a.items.is_empty()))
            .unwrap_or(0);
        for (tag, dark) in [("light", false), ("dark", true)] {
            st.glass_liquid = dark;
            st.active_app = Some(ghi);
            st.hover_chip = None;
            st.app_hot = [0.0; MAX_APPS];
            st.chip_anim = [1.0; MAX_ITEMS];
            st.chip_hot = [0.0; MAX_ITEMS];
            // 离线 dump 不走 on_timer → 手动把段角落到终态（否则胶囊缩在 0 宽）
            st.chip_seg_app = ghi;
            let segs = chip_layout(&st, ghi).0;
            for k in 0..st.apps[ghi].items.len().min(MAX_ITEMS) {
                st.chip_seg_a[k] = segs.get(k).copied().unwrap_or((f32::NAN, f32::NAN));
            }
            let buf = draw_to_buf(&mut st);
            save_bmp(&format!("{}\\dump_glass_{}.bmp", dir, tag), &buf, CANVAS, CANVAS);
            println!("[dump] dump_glass_{} 液态玻璃盘面", tag);
        }
        // 浅灰桌面（近白）：最容易暴露"分层"——盘面近白时，任何一层灰带都会显形。
        // 用户 2026-10-17 报「轮盘还分层了」就是在白色背景上看到的。
        synth_bg(&mut st, true);
        {
            let raw2 = std::slice::from_raw_parts(st.bg_bits, n).to_vec();
            let mut buf = Vec::with_capacity(raw2.len() * 2);
            buf.extend_from_slice(&raw2);
            buf.extend_from_slice(&blur_glass(&raw2));
            set_glass_bg(&mut st, buf);
        }
        st.glass_liquid = false;
        st.glass = true;
        st.active_app = Some(ghi);
        st.chip_seg_app = ghi;
        let segs = chip_layout(&st, ghi).0;
        for k in 0..st.apps[ghi].items.len().min(MAX_ITEMS) {
            st.chip_seg_a[k] = segs.get(k).copied().unwrap_or((f32::NAN, f32::NAN));
        }
        let buf = draw_to_buf(&mut st);
        save_bmp(&format!("{}\\dump_glass_gray.bmp", dir), &buf, CANVAS, CANVAS);
        println!("[dump] dump_glass_gray 液态玻璃（浅灰桌面，查分层）");

        st.glass = false;
        set_glass_bg(&mut st, Vec::new());
        st.glass_liquid = false;
        synth_bg(&mut st, false);
    }

    // 3.8) 深色盘面对照图（托盘可切换；深色下盘面/图标/胶囊的对比度要单独看）
    st.glass_liquid = true;
    st.active_app = None;
    st.hover_chip = None;
    st.app_hot = [0.0; MAX_APPS];
    let buf = draw_to_buf(&mut st);
    save_bmp(&format!("{}\\dump_dark.bmp", dir), &buf, CANVAS, CANVAS);
    st.glass_liquid = false;

    // 3.9) 深色 + **改名态**：编辑段是「墨底（暗色下=近白）+ 对比色文字」，
    //      文字必须取 pal.paper，暗色下才看得清（用户 2026-10-12 报「双击编辑看不清」）。
    let saved_theme = st.theme;
    st.theme = 1;
    apply_theme(&mut st);
    if let Some(hi) = st
        .apps
        .iter()
        .enumerate()
        .max_by_key(|(_, a)| a.items.len())
        .filter(|(_, a)| !a.items.is_empty())
        .map(|(i, _)| i)
    {
        st.active_app = Some(hi);
        for v in st.chip_anim.iter_mut() {
            *v = 1.0;
        }
        for (j, v) in st.chip_hot.iter_mut().enumerate() {
            *v = if j == 0 { 1.0 } else { 0.0 };
        }
        st.editing = Some((hi, 0));
        st.edit_buf = "长按右键·改".to_string();
        let buf = draw_to_buf(&mut st);
        save_bmp(&format!("{}\\dump_dark_edit.bmp", dir), &buf, CANVAS, CANVAS);
        st.editing = None;
    }
    st.theme = saved_theme;
    apply_theme(&mut st);

    // 3.10) 拖放引导：**幽灵磁贴**（认不出宿主→要新建的内环）+ 建议内环高亮（beacon）
    if let Some(hi) = st
        .apps
        .iter()
        .enumerate()
        .max_by_key(|(_, a)| a.items.len())
        .filter(|(_, a)| !a.items.is_empty())
        .map(|(i, _)| i)
    {
        st.active_app = Some(hi);
        for v in st.chip_anim.iter_mut() {
            *v = 1.0;
        }
        st.drop_suggest = Some(0); // 认得宿主 → 点亮第 0 个内环（引导往上丢）
        st.app_hot[0] = 1.0;
        // 用**默认浏览器**当样例：它一定存在，能展示"幽灵带真图标"的效果
        let gexe = unsafe { default_browser_exe() }
            .unwrap_or_else(|| "C:\\Windows\\explorer.exe".to_string());
        st.drop_ghost = Some(GhostApp {
            name: "browser".to_string(),
            target: gexe.clone(),
            put_items: true,
            icon: unsafe { decode_icon(&gexe) }.map(|(px, d)| IconTex::from_base(px, d)),
        });
        let buf = draw_to_buf(&mut st);
        save_bmp(&format!("{}\\dump_drop_ghost.bmp", dir), &buf, CANVAS, CANVAS);
        st.drop_suggest = None;
        st.drop_ghost = None;
        st.app_hot = [0.0; MAX_APPS];
    }

    // 4) chip 命中自检：绘制用的中心点必须能命中自己（保证 hover/点击不失效）
    st.win_x = 0;
    st.win_y = 0;
    let mut pass = 0usize;
    let mut fail = 0usize;
    if let Some(hi) = st
        .apps
        .iter()
        .enumerate()
        .max_by_key(|(_, a)| a.items.len())
        .filter(|(_, a)| !a.items.is_empty())
        .map(|(i, _)| i)
    {
        st.active_app = Some(hi);
        let m = st.apps[hi].items.len();
        for j in 0..m {
            // 重新取布局（每次命中后 hover_chip 会被 reset，不影响 layout）
            let (segs, _rots, rads) = chip_layout(&st, hi);
            let (cx, cy) = polar(rads[j], (segs[j].0 + segs[j].1) * 0.5);
            update_hover(&mut st, cx as i32, cy as i32);
            if st.hover_chip == Some(j) {
                pass += 1;
            } else {
                fail += 1;
                eprintln!(
                    "[hit] FAIL chip {} ({}) center=({:.0},{:.0}) -> got {:?}",
                    j, st.apps[hi].items[j].name, cx, cy, st.hover_chip
                );
            }
        }
    }
    // 调试：导出文字遮罩 DC 内容（3x 超采样遮罩诊断）
    {
        let n = (CANVAS * 140 * 4) as usize;
        let buf = std::slice::from_raw_parts(st.txt_bits, n);
        save_bmp(&format!("{}\u{5c}dump_mask.bmp", dir), buf, CANVAS, 140);
    }
    println!("[*] 已导出 dump_disc/dump_hover/dump_disc_gray.bmp 到 {dir}");
    println!("[*] chip 命中自检：PASS={pass} FAIL={fail}");
}

// ───────────────────── 退出清理 ─────────────────────

/// 结束本项目的其它实例。
///
/// ⚠️ 两个必须注意的地方（都踩过）：
/// ① **不能只按进程名匹配**。上一轮为了绕开「文件被占用」把 exe 改名成
///    `PiPie.old.exe` / `PiPie.prev.exe`，测试实例又在 `_dragtest/` 下 ——
///    按名字匹配会把它们**全部漏掉**（`--quit` 什么都没杀就是这么来的）。
///    现在按**映像文件的完整路径**判断（文件名以 `PiPie` / `radialmenu` / `radial-launcher` 开头）。
/// ② **进程表里可能有「已退出但对象未回收」的僵尸**（异常终止 / 会话切换残留）。
///    它们 `GetExitCodeProcess` 返回的不是 `STILL_ACTIVE`，`taskkill` 会报
///    「没有此任务的实例在运行」。对这种 PID 调 `TerminateProcess` 纯属浪费，
///    直接跳过。
unsafe fn kill_other_instances() {
    use windows::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };
    use windows::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, TerminateProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        PROCESS_TERMINATE,
    };

    let me = std::process::id();
    // ① 有宿主窗口的实例 = 活实例 → 发 WM_CLOSE 走 quit_app 优雅退出。
    //    TerminateProcess 会把钩子线程连人带枪活埋，钩子仍挂在输入链上
    //    （僵尸壳毒化全系统鼠标）。硬杀只留给不响应的壳。
    let host_cls: Vec<u16> = wide("RLHost");
    if let Ok(hw) = FindWindowW(PCWSTR(host_cls.as_ptr()), PCWSTR::null()) {
        let mut pid = 0u32;
        GetWindowThreadProcessId(hw, Some(&mut pid));
        if pid != 0 && pid != me {
            let mut res = 0;
            let _ = SendMessageTimeoutW(
                hw, WM_CLOSE, WPARAM(0), LPARAM(0), SMTO_BLOCK, 3000, Some(&mut res),
            );
            // 等它彻底退场（quit_app 里摘钩子 + 等钩子线程，一般 <1s）
            let t0 = GetTickCount();
            loop {
                let alive = match OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) {
                    Ok(hq) => {
                        let mut code = 0u32;
                        let _ = GetExitCodeProcess(hq, &mut code);
                        let _ = CloseHandle(hq);
                        code as i32 == STILL_ACTIVE.0
                    }
                    Err(_) => break, // 进程没了
                };
                if !alive || GetTickCount().wrapping_sub(t0) > 4000 {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        }
    }

    let Ok(snap_h) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
        return;
    };
    let mut entry = PROCESSENTRY32W::default();
    entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
    let mut killed = 0u32;

    if Process32FirstW(snap_h, &mut entry).is_ok() {
        loop {
            let pid = entry.th32ProcessID;
            let name = {
                let buf = &entry.szExeFile;
                let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
                String::from_utf16_lossy(&buf[..n])
            };
            let base = name.rsplit(['\\', '/']).next().unwrap_or(&name).to_ascii_lowercase();
            let ours = base.starts_with("pipie")
                || base.starts_with("pigo")
                || base.starts_with("radialmenu")
                || base.starts_with("radial-launcher");
            if pid != 0 && pid != me && ours {
                // 先开一个「只查询」的句柄判断是不是僵尸
                if let Ok(hq) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) {
                    let mut code = 0u32;
                    let ok = GetExitCodeProcess(hq, &mut code).is_ok();
                    let _ = CloseHandle(hq);
                    if !ok || code as i32 == STILL_ACTIVE.0 {
                        if let Ok(h) = OpenProcess(PROCESS_TERMINATE, false, pid) {
                            if TerminateProcess(h, 0).is_ok() {
                                killed += 1;
                            }
                            let _ = CloseHandle(h);
                        }
                    }
                }
            }
            if Process32NextW(snap_h, &mut entry).is_err() {
                break;
            }
        }
    }
    let _ = CloseHandle(snap_h);
    if killed > 0 {
        println!("[*] 已结束 {killed} 个残留实例");
    }
}

unsafe impl Send for State {}

// ═══════════════════════════════════════════════════════════════════
//  以下为「重建段」：一次补丁锚点跨度过大，误删了这一整块业务代码，
//  按本会话记录逐段恢复。函数顺序在 Rust 里无关紧要，统一追加在文件末尾。
// ═══════════════════════════════════════════════════════════════════

// ───────────────────── 公共小工具 ─────────────────────

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 可执行文件所在目录（配置、导出文件都放这里）
fn exe_dir() -> String {
    let mut buf = [0u16; 1024];
    let n = unsafe { GetModuleFileNameW(None, &mut buf) } as usize;
    let p = String::from_utf16_lossy(&buf[..n.min(buf.len())]);
    match p.rfind('\\') {
        Some(i) => p[..i].to_string(),
        None => ".".into(),
    }
}

/// 配置目录：`%APPDATA%\PiPie`（**Roaming**）。
///
/// 为什么不放 exe 同目录（用户 2026-10-16 提的）：exe 一挪、一升级、换个目录解压，
/// 配置就"丢"了 —— 用户得重新配一遍。放 `%APPDATA%` 是这个平台的标准做法：
/// 与 exe 位置解耦，换用户账户/换机器（域漫游）还能跟着走。
///
/// 环境变量 `PIPIE_CONFIG_DIR` 可覆盖 —— 自动化测试用它指向沙箱，
/// **否则测试会污染用户的真实配置**（多实例共用一个 ini）。
/// 旧名 `PIGO_CONFIG_DIR` 仍作为兜底，避免既有测试脚本失效。
fn config_dir() -> String {
    for k in ["PIPIE_CONFIG_DIR", "PIGO_CONFIG_DIR"] {
        if let Ok(d) = std::env::var(k) {
            let d = d.trim().to_string();
            if !d.is_empty() {
                return d;
            }
        }
    }
    // ⚠️ 兼容自动化测试：测试实例都带 `RL_INSTANCE`（互斥体后缀）且以 `cwd=沙箱`
    //    启动、ini 就放沙箱里 —— 现有 20+ 个测试脚本全部依赖这个约定，
    //    不认它就得逐个去改。要**显式**指定请用 `PIPIE_CONFIG_DIR`。
    if std::env::var("RL_INSTANCE").map(|v| !v.is_empty()).unwrap_or(false) {
        return std::env::current_dir()
            .map(|d| d.to_string_lossy().to_string())
            .unwrap_or_else(|_| exe_dir());
    }
    match std::env::var("APPDATA") {
        Ok(a) if !a.trim().is_empty() => format!("{}\\PiPie", a.trim().trim_end_matches('\\')),
        _ => exe_dir(), // 极端兜底（APPDATA 都没了）
    }
}

/// 日志目录：`%LOCALAPPDATA%\PiPie\logs`。
/// 日志**不漫游** —— 体积大、只对本机有意义（Roaming 会被域同步拖走）。
/// 设了 `PIPIE_CONFIG_DIR` 时日志也留在沙箱里，测试不往真实目录拉屎。
fn log_dir() -> String {
    let overridden = ["PIPIE_CONFIG_DIR", "PIGO_CONFIG_DIR"].iter().any(|k| {
        std::env::var(k)
            .map(|d| !d.trim().is_empty())
            .unwrap_or(false)
    });
    if overridden {
        return config_dir();
    }
    match std::env::var("LOCALAPPDATA") {
        Ok(a) if !a.trim().is_empty() => {
            format!("{}\\PiPie\\logs", a.trim().trim_end_matches('\\'))
        }
        _ => config_dir(),
    }
}

/// 统一决定 ini 路径（并做一次旧配置迁移）。
/// ⚠️ 别在别处手写 `format!("{}\\settings.ini", …)` —— 路径规则只能有一处。
fn init_config_path(st: &mut State) {
    let dir = config_dir();
    let _ = std::fs::create_dir_all(&dir);
    migrate_legacy_config(&dir);
    st.ini_path = format!("{}\\settings.ini", dir);
}

/// 把老版本留在 **exe 同目录** 的配置搬一次（改配置位置后的平滑升级）。
///
/// 只在「目标还没有 ini」且「exe 旁边确实有旧 ini」时动手；旧文件**改名**成
/// `settings.ini.migrated` 而不是删掉 —— 用户的配置不能悄悄消失（留着还能手动回滚）。
fn migrate_legacy_config(dst_dir: &str) {
    let dst = format!("{}\\settings.ini", dst_dir);
    if std::path::Path::new(&dst).exists() {
        return;
    }
    // 改名（pigo → PiPie）后配置目录由 `%APPDATA%\pigo` 换成 `%APPDATA%\PiPie`。
    // 只在「新目录还没有 ini」时复制一次；旧文件**保留**，用户可手动回滚/对比。
    if let Ok(a) = std::env::var("APPDATA") {
        let a = a.trim().trim_end_matches('\\');
        if !a.is_empty() {
            let old = format!("{}\\pigo\\settings.ini", a);
            if !old.eq_ignore_ascii_case(&dst)
                && std::path::Path::new(&old).exists()
                && std::fs::copy(&old, &dst).is_ok()
            {
                let old_bak = format!("{}\\pigo\\settings.ini.bak", a);
                if std::path::Path::new(&old_bak).exists() {
                    let _ = std::fs::copy(&old_bak, format!("{}.bak", dst));
                }
                println!("[*] 旧版配置（pigo）已复制到 {}", dst);
                return;
            }
        }
    }
    let src_dir = exe_dir();
    if src_dir.eq_ignore_ascii_case(dst_dir) {
        return; // 沙箱/测试：本来就同一个目录
    }
    let src = format!("{}\\settings.ini", src_dir);
    if !std::path::Path::new(&src).exists() {
        return;
    }
    if std::fs::copy(&src, &dst).is_err() {
        return;
    }
    let src_bak = format!("{}.bak", src);
    if std::path::Path::new(&src_bak).exists() {
        let _ = std::fs::copy(&src_bak, format!("{}.bak", dst));
    }
    let _ = std::fs::rename(&src, format!("{}\\settings.ini.migrated", src_dir));
    println!("[*] 旧配置已迁移到 {}", dst);
}

/// 宽字符串 → String（在第一个 NUL 处截断）
fn path_str_of(wt: &[u16]) -> String {
    let end = wt.iter().position(|&c| c == 0).unwrap_or(wt.len());
    String::from_utf16_lossy(&wt[..end])
}

// ───────────────────── 配色语义映射 ─────────────────────

/// 按名字关键词挑配色 —— 避免「记事本=灰、终端=绿」这种按下标轮转的任意感
fn grad_for(name: &str) -> usize {
    let n = name.to_lowercase();
    let has = |k: &str| n.contains(k);
    // 站点 / 品牌专色（放在通用分组之前，保证外环 chip 有辨识度）
    if has("github") {
        return 2;
    }
    if has("哔哩") || has("bilibili") || has("b站") {
        return 9;
    }
    if has("豆包") || has("doubao") {
        return 4;
    }
    if has("知乎") || has("zhihu") {
        return 10;
    }
    if has("youtube") || has("视频") {
        return 6;
    }
    if has("gmail") {
        return 6;
    }
    if has("浏览器") || has("browser") || has("edge") || has("chrome") || has("firefox")
        || has("网页") || has("百度")
    {
        return 0;
    }
    if has("资源管理") || has("文件夹") || has("explorer") || has("folder") || has("目录") {
        return 1;
    }
    if has("终端") || has("cmd") || has("powershell") || has("wsl") || has("console")
        || has("控制台") || has("shell") || has("term")
    {
        return 2;
    }
    if has("设置") || has("setting") || has("配置") || has("preference") || has("系统") {
        return 3;
    }
    if has("计算器") || has("calc") || has("工具") || has("tool") {
        return 4;
    }
    if has("画图") || has("paint") || has("绘图") || has("截图") || has("photo") {
        return 5;
    }
    if has("邮件") || has("mail") || has("outlook") || has("账单") || has("订阅") {
        return 6;
    }
    if has("微信") || has("qq") || has("chat") || has("message") || has("消息")
        || has("聊天") || has("扫码") || has("收付款")
    {
        return 7;
    }
    if has("音乐") || has("music") || has("网易云") || has("spotify") || has("网易") {
        return 9;
    }
    if has("下载") || has("download") || has("云") || has("drive") {
        return 10;
    }
    if has("记事本") || has("notepad") || has("笔记") || has("note") || has("文档")
        || has("文本") || has("说明")
    {
        return 11;
    }
    if has("任务") || has("task") || has("监控") || has("性能") {
        return 12;
    }
    if has("桌面") || has("desktop") {
        return 5;
    }
    8
}

// ───────────────────── 配置（settings.ini） ─────────────────────

/// 出厂配置。`[apps]` 段由 `default_apps()` 生成（浏览器 / 下载 / 文档 / 开始菜单）。
fn default_ini() -> String {
    let mut s = String::from("[apps]\r\n");
    for (n, t) in default_apps() {
        s.push_str(&format!("{}={}\r\n", n, t));
    }
    // 文件夹外环：下载/文档（段名须与内环「文件夹」一致）
    s.push_str("\r\n[app.文件夹]\r\n");
    s.push_str("下载=shell:Downloads\r\n");
    s.push_str("文档=shell:Personal\r\n");
    // 浏览器外环：一圈点开常用 AI / 新标签（段名须与内环「浏览器」一致）
    s.push_str("\r\n[app.浏览器]\r\n");
    s.push_str("DeepSeek=https://chat.deepseek.com\r\n");
    s.push_str("豆包=https://www.doubao.com\r\n");
    s.push_str("新标签页=about:blank\r\n");
    s
}

/// 从 INI 读设置开关。dark= 现在是 0 亮 / 1 暗 / 2 跟随系统（旧值 0/1 完全兼容）。
unsafe fn read_persistent(st: &mut State) {
    if let Ok(txt) = std::fs::read_to_string(&st.ini_path) {
        for line in txt.lines() {
            let l = line.trim();
            let get = |p: &str| l.strip_prefix(p).map(|v| v.trim().to_string());
            if let Some(v) = get("dark=") {
                st.theme = v.parse::<i32>().unwrap_or(if v != "0" { 1 } else { 0 }).clamp(0, 2);
            } else if let Some(v) = get("persistent=") {
                st.persistent = v != "0";
            } else if let Some(v) = get("listening=") {
                st.listening = v != "0";
            } else if let Some(v) = get("key_middle=") {
                st.key_middle = v != "0";
            } else if let Some(v) = get("hover_name=") {
                st.hover_name = v != "0";
            } else if let Some(v) = get("glass=") {
                // 缺省 = 关（纸面）—— 玻璃要看桌面，不是所有人的壁纸都撑得住
                st.glass = v != "0";
            } else if let Some(v) = get("glass_live=") {
                // 缺省 = 开。关掉才能把「轮盘」录进宣传片（见 `glass_live_start`）
                st.glass_live = v != "0";
            } else if let Some(v) = get("key_side=") {
                st.key_side = v != "0";
            } else if let Some(v) = get("key_rhold=") {
                st.key_rhold = v != "0";
            } else if let Some(v) = get("trig_custom_kind=") {
                st.trig_custom_kind = v.parse::<u8>().unwrap_or(0);
            } else if let Some(v) = get("trig_custom_btn=") {
                st.trig_custom_btn = v.parse::<u8>().unwrap_or(0);
            } else if let Some(v) = get("trig_custom_vk=") {
                st.trig_custom_vk = v.parse::<u32>().unwrap_or(0);
            } else if let Some(v) = get("trig_custom_mods=") {
                st.trig_custom_mods = v.parse::<u8>().unwrap_or(0);
            } else if let Some(v) = get("autostart=") {
                // ⚠️ 缺省值 = 开（`State::new` 里已经是 true）—— 这里只在**显式写了 0** 时才关，
                // 保证「老配置 / 全新配置」都默认勾选开机自启。
                st.autostart = v != "0";
            } else if let Some(v) = get("onb_drag=") {
                st.onb_drag = v != "0";
            } else if let Some(v) = get("onb_move=") {
                st.onb_move = v != "0";
            } else if let Some(v) = get("onb_edit=") {
                st.onb_edit = v != "0";
            } else if let Some(v) = get("onb_scroll=") {
                st.onb_scroll = v != "0";
            } else if let Some(v) = get("guide_seen=") {
                st.guide_seen = v != "0";
            } else if let Some(v) = get("update_auto=") {
                st.update_auto = v != "0";
            } else if let Some(v) = get("update_last_check=") {
                st.update_last_check = v.parse::<f64>().unwrap_or(0.0);
            } else if let Some(v) = get("update_avail_ver=") {
                st.update_avail_ver = v;
            } else if let Some(v) = get("update_bub_ver=") {
                st.update_bub_ver = v;
            }
        }
    }
    apply_theme(st);
    apply_key_opts(st);
}

/// 重新解码全部应用图标（增删应用后调用）
unsafe fn load_icons(st: &mut State) {
    // 诊断开关：跳过全部图标解码，用于**测量图标到底占多少内存**（含堆留存）。
    if std::env::var_os("RL_NOICONS").is_some() {
        for a in st.apps.iter_mut() {
            a.icon = None;
            for it in a.items.iter_mut() {
                it.icon = None;
            }
        }
        return;
    }
    // ⚠️ 试过「按 target 去重（只解码一次）」，**实测反而更占内存**（15.69 vs 14.91）：
    // 去重缓存要把全部纹理同时 hold 住直到本函数结束（~1.1MB 被堆留存），
    // 而本配置只重复 2 次解码 —— 省的还没缓存本身多。已放弃，别再捡回来。
    for a in st.apps.iter_mut() {
        a.grad = grad_for(&a.name);
        // `decode_icon` 返回**基础纹理**（ICON_PX），这里包成 mip 链：
        // 绘制时才能取到接近目标边长的那一级，避免一次 2.8:1 的大缩小。
        a.icon = decode_icon(&a.target).map(|(px, d)| IconTex::from_base(px, d));
        // 外环条目也要真图标（与内环同一套管线）。URL / ms-* / shell:* / 文件夹
        // `decode_icon` 内部都已经处理；失败时画首字母兜底。
        for it in a.items.iter_mut() {
            it.icon = decode_icon(&it.target).map(|(px, d)| IconTex::from_base(px, d));
        }
    }
}

/// 往 settings.ini 的 [设置] 段里写一个布尔开关（已存在则替换）。
/// ⚠️ 已被 save_settings 取代（设置页成为唯一入口），保留仅作历史参考。
#[allow(dead_code)]
unsafe fn write_ini_flag(path: &str, key: &str, val: bool) {
    let Ok(txt) = std::fs::read_to_string(path) else {
        return;
    };
    let line = format!("{}={}", key, if val { 1 } else { 0 });
    let mut out: Vec<String> = Vec::new();
    let mut replaced = false;
    for l in txt.lines() {
        let t = l.trim();
        if t.starts_with(&format!("{}=", key)) {
            if !replaced {
                out.push(line.clone());
                replaced = true;
            }
            continue;
        }
        out.push(l.to_string());
    }
    if !replaced {
        // 插到 [设置] 段首；没有该段就追加到文件末尾
        match out.iter().position(|l| l.trim() == "[设置]") {
            Some(i) => out.insert(i + 1, line),
            None => {
                out.push(String::new());
                out.push("[设置]".into());
                out.push(line);
            }
        }
    }
    let _ = std::fs::write(path, out.join("\r\n"));
}

unsafe fn load_ini(st: &mut State) {
    st.apps.clear();
    st.exc.clear();
    st.exc_w.clear();
    if std::path::Path::new(&st.ini_path).exists() {
        if let Ok(text) = std::fs::read_to_string(&st.ini_path) {
            // 0=无 1=[apps] 2=[app.X] 3=[例外]
            let mut mode = 0usize;
            let mut cur: Option<usize> = None;
            for line in text.lines() {
                let line = line.trim();
                if line.starts_with('[') {
                    let sec = line.trim_start_matches('[').trim_end_matches(']').trim();
                    if sec.eq_ignore_ascii_case("apps") {
                        mode = 1;
                    } else if let Some(name) = sec.strip_prefix("app.") {
                        mode = 2;
                        let name = name.trim();
                        cur = st.apps.iter().position(|a| a.name == name);
                    } else if sec.eq_ignore_ascii_case("例外") {
                        mode = 3;
                    } else {
                        mode = 0;
                    }
                    continue;
                }
                if mode == 0 || line.is_empty() || line.starts_with(';') || line.starts_with('#') {
                    continue;
                }
                if let Some((k, v)) = line.split_once('=') {
                    let (k, v) = (k.trim(), v.trim());
                    if k.is_empty() || v.is_empty() {
                        continue;
                    }
                    match mode {
                        1 => {
                            if st.apps.len() < MAX_APPS {
                                st.apps.push(AppEntry {
                                    name: k.to_string(),
                                    target: v.to_string(),
                                    icon: None,
                                    items: Vec::new(),
                                    grad: grad_for(k),
                                });
                            }
                        }
                        2 => {
                            if let Some(ai) = cur {
                            if st.apps[ai].items.len() < MAX_ITEMS {
                                // 持久化编码（`load_ini` 与 `save_settings` 约定）：
                                //   `target` | `target|N`（配色）
                                //   | `target|N|@win|<relaunch>`（窗口条目，kind=1）
                                // 顺序：先摘 `|@win|` 后缀拿到 kind/relaunch，
                                // 剩下的再按 `target|N` 解出配色。
                                let mut raw = v.to_string();
                                let mut kind: u8 = 0;
                                let mut relaunch = String::new();
                                if let Some(idx) = raw.find("|@win|") {
                                    kind = 1;
                                    relaunch = raw[idx + 6..].to_string();
                                    raw.truncate(idx);
                                }
                                let (target, color) = match raw.rsplit_once('|') {
                                    Some((t, s)) if !t.is_empty() => {
                                        match s.parse::<usize>() {
                                            Ok(n) if n < GRADS.len() => {
                                                (t.to_string(), Some(n))
                                            }
                                            _ => (raw.clone(), None),
                                        }
                                    }
                                    _ => (raw.clone(), None),
                                };
                                st.apps[ai].items.push(ItemEntry {
                                    icon: None,
                                    name: k.to_string(),
                                    target,
                                    color,
                                    kind,
                                    relaunch,
                                    live_hwnd: 0,
                                });
                            }
                            }
                        }
                        3 => {
                            // `mode=` 决定二选一生效的是黑名单还是白名单（0=black / 1=white）；
                            // 名单条目用 `b=`（黑）/ `w=`（白）前缀，两张卡片可并存于配置。
                            if k.eq_ignore_ascii_case("mode") {
                                st.exc_mode = if v.eq_ignore_ascii_case("white") { 1 } else { 0 };
                            } else if k.eq_ignore_ascii_case("fullscreen") {
                                st.exc_fs = v == "1";
                            } else {
                                // 兼容旧格式（曾短暂用过）`<进程名>=1` —— 那种情况下 key 才是进程名。
                                let (is_w, raw) = if k.eq_ignore_ascii_case("b") {
                                    (false, v)
                                } else if k.eq_ignore_ascii_case("w") {
                                    (true, v)
                                } else {
                                    (false, k)
                                };
                                let n = raw.to_ascii_lowercase();
                                let bucket = if is_w { &mut st.exc_w } else { &mut st.exc };
                                if n.ends_with(".exe") && !bucket.iter().any(|e| e == &n) {
                                    bucket.push(n);
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    } else {
        let _ = std::fs::write(&st.ini_path, default_ini());
        load_ini(st); // 写完默认配置后走统一解析
        return;
    }
    if st.apps.len() > MAX_APPS {
        st.apps.truncate(MAX_APPS);
    }
    // 每次重载都重算「用户是否自己配过环」：他可能刚拖进来一个新应用，
    // 也可能刚删掉了默认项 —— 两个方向都该立刻停止"拖进来添加"的引导。
    st.onb_customized = apps_customized(st);
    load_icons(st);
    // 窗口条目（kind==1）的 live_hwnd 不持久化，重启后这里按「exe + 标题」重新
    // 匹配一个还活着的活动窗口，没匹配到（已关）就保持 0（渲染灰显、点击重开）。
    restamp_window_hwnds(st);
}

// ───────────────────── 图标提取 ─────────────────────
/// 图标内容的 alpha 外接框。图标位图四周常带透明留白（实测 `cmd` 的内容只有
/// 82×68、`mspaint` 82×62，其余 82×82）—— 不裁掉的话「光学尺寸」各不一样，
/// 在环上看起来就是有的满格、有的偏小，整体显得不整齐。
fn alpha_bbox(bits: &[u8], bw: usize, bh: usize) -> (usize, usize, usize, usize) {
    const THR: u8 = 8;
    let (mut x0, mut y0, mut x1, mut y1) = (bw, bh, 0usize, 0usize);
    for y in 0..bh {
        for x in 0..bw {
            if bits[(y * bw + x) * 4 + 3] > THR {
                if x < x0 { x0 = x; }
                if y < y0 { y0 = y; }
                if x + 1 > x1 { x1 = x + 1; }
                if y + 1 > y1 { y1 = y + 1; }
            }
        }
    }
    if x1 <= x0 || y1 <= y0 {
        (0, 0, bw, bh)
    } else {
        (x0, y0, x1, y1)
    }
}

/// box-filter 把图标缩放进 tw×tw（BGRA）。tw 必须 **≤ 内容尺寸**（放大造不出细节）。
///
/// 两步：① 按 alpha 外接框裁掉透明留白；② 等比缩放并**居中**放进 tw×tw。
fn normalize_to(bits: &[u8], bw: usize, bh: usize, tw: usize) -> Vec<u8> {
    let mut norm = vec![0u8; tw * tw * 4];
    if bw == 0 || bh == 0 {
        return norm;
    }
    // 面积平均（box filter），不是双线性。
    // 双线性在下采样时只取 4 个纹素 = 点采样，会丢细节 + 产生锯齿，
    // 看上去就是「糊」。这里对每个目标像素覆盖的源矩形做加权平均。
    //
    // 关键：在**预乘空间**累加。图标透明区域 RGB=0，若直接平均 RGB，
    // 边缘会被拉黑出现脏边；先乘 alpha 平均后再除回去才对。
    let (cx0, cy0, cx1, cy1) = alpha_bbox(bits, bw, bh);
    let cw = (cx1 - cx0) as f32;
    let ch = (cy1 - cy0) as f32;
    // ⚠️⚠️ 用 **cover**（短边撑满 tw），**不是 contain**（长边撑满、短边留白）。
    //
    // 为什么必须 cover：③ 的圆角裁切作用在**整个 tw×tw 框**上。用 contain 时，内容不是
    // 正方的图标（cmd 254×210、文件夹 224×176、elasticsearch 256×192…）会在上下（或左右）
    // 留出透明带，圆角的那段弧正好落在透明带上 —— **图标自己的直角就原样露出来了**，
    // 于是环上混着直角图标和圆角图标（用户 2026-10-08 报「有的是直角有的是圆角」）。
    // 改成 cover 后每一格的轮廓都是同一个圆角方块，只有长边溢出的一点点被裁掉
    // （Windows 图标长宽比都在 1.4 以内 → 每边裁 ≤12%，且都是图标自身的留白）。
    //
    // 极端长宽比（>1.6，实际没遇到过）时退回 contain，免得硬裁把内容裁没。
    let aspect = cw.max(ch) / cw.min(ch).max(1.0);
    let cover = aspect <= 1.6;
    let k = if cover {
        tw as f32 / cw.min(ch).max(1.0)
    } else {
        tw as f32 / cw.max(ch).max(1.0)
    };
    let ox = (tw as f32 - cw * k) * 0.5;
    let oy = (tw as f32 - ch * k) * 0.5;
    // cover 时源范围会越出内容外框 → **夹到外框**（不是夹到纹理边，更不能放任取到透明区），
    // 内容才会一直铺到格子边缘；否则最外一两圈会取样到透明 → 边缘留一圈白、轮廓又变小。
    let (lo_x, hi_x, lo_y, hi_y) = if cover {
        (cx0 as f32, cx1 as f32, cy0 as f32, cy1 as f32)
    } else {
        (0.0, bw as f32, 0.0, bh as f32)
    };
    for ny in 0..tw {
        let y0f = cy0 as f32 + (ny as f32 - oy) / k;
        let y1f = cy0 as f32 + ((ny + 1) as f32 - oy) / k;
        let iy0 = y0f.floor().max(lo_y) as usize;
        let iy1 = (y1f.ceil()).max(iy0 as f32).min(hi_y) as usize;
        for nx in 0..tw {
            let x0f = cx0 as f32 + (nx as f32 - ox) / k;
            let x1f = cx0 as f32 + ((nx + 1) as f32 - ox) / k;
            let ix0 = x0f.floor().max(lo_x) as usize;
            let ix1 = (x1f.ceil()).max(ix0 as f32).min(hi_x) as usize;
            let mut acc = [0.0f32; 4]; // 预乘后的 R,G,B,A
            let mut wsum = 0.0f32;
            for yy in iy0..iy1 {
                let wy = (yy as f32 + 1.0).min(y1f) - (yy as f32).max(y0f);
                if wy <= 0.0 {
                    continue;
                }
                for xx in ix0..ix1 {
                    let wx = (xx as f32 + 1.0).min(x1f) - (xx as f32).max(x0f);
                    if wx <= 0.0 {
                        continue;
                    }
                    let wt = wx * wy;
                    let so = (yy * bw + xx) * 4;
                    let a = bits[so + 3] as f32 / 255.0;
                    acc[0] += bits[so + 2] as f32 * a * wt; // R
                    acc[1] += bits[so + 1] as f32 * a * wt; // G
                    acc[2] += bits[so] as f32 * a * wt;     // B
                    acc[3] += a * wt;
                    wsum += wt;
                }
            }
            let o = (ny * tw + nx) * 4;
            if wsum > 0.0 && acc[3] > 1e-5 {
                norm[o] = (acc[2] / acc[3]).clamp(0.0, 255.0) as u8; // B
                norm[o + 1] = (acc[1] / acc[3]).clamp(0.0, 255.0) as u8; // G
                norm[o + 2] = (acc[0] / acc[3]).clamp(0.0, 255.0) as u8; // R
                norm[o + 3] = ((acc[3] / wsum) * 255.0).clamp(0.0, 255.0) as u8;
            }
        }
    }
    // ③ 统一裁成**圆角方块**：图标自带轮廓千奇百怪 —— 圆形标、白纸、**直角方块**
    //    （设置 / 计算器就是 100% 不透明的直角方块，角落是尖的）。
    //    不裁的话环上会混着尖角方块和圆角，完全不是一个语言；
    //    裁完每格都是同一个圆角方形，这才是参考图那种「整齐」。
    // 圆角半径用 TILE_RAD_K × 边长 × TILE_PAD_K。
    // 为什么要再乘 PAD：纹理被画到「底板 ÷ PAD」的尺寸上，而底板圆角是
    // TILE_RAD_K × 底板边长。只按自身边长取半径的话，缩放后图标圆角会比
    // 底板小 PAD 倍 → 两者对不齐、边上露白缝。乘上后正好齐平。
    {
        // cover：裁切框 = 整个 tw×tw（内容已经铺满它，跟底板圆角天然对齐）。
        // 非 cover（极端长宽比）：裁切框贴着**内容外框**、半径按短边取 ——
        // 这样内容自己的直角一定会被倒掉，绝不会出现「方角图标」。
        let (fw, fh) = if cover { (tw as f32, tw as f32) } else { (cw * k, ch * k) };
        let half = tw as f32 * 0.5;
        let half_w = fw * 0.5;
        let half_h = fh * 0.5;
        let rad = fw.min(fh) * TILE_RAD_K * TILE_TEX_PAD_K;
        for ny in 0..tw {
            for nx in 0..tw {
                let o = (ny * tw + nx) * 4;
                if norm[o + 3] == 0 {
                    continue;
                }
                // cov1 要「内部为负」，而 sdf_rounded_rect 是「内部为正」→ 取负
                let cov = cov1(-sdf_rounded_rect(
                    nx as f32 + 0.5 - half,
                    ny as f32 + 0.5 - half,
                    half_w,
                    half_h,
                    rad,
                ));
                if cov < 0.999 {
                    norm[o + 3] = (norm[o + 3] as f32 * cov) as u8;
                }
            }
        }
    }
    norm
}

/// 高分辨率图标：走 shell 的 IShellItemImageFactory（Explorer 同款管线）。
/// `PrivateExtractIconsW` 对 explorer/notepad/cmd/calc/taskmgr 只给 32×32，
/// 放大画到 45px 必然发虚 —— 这是图标模糊的根因。
unsafe fn shell_icon_hires(exe_path: &str) -> Option<Vec<u8>> {
    use windows::Win32::UI::Shell::{
        IShellItemImageFactory, SHCreateItemFromParsingName, SIIGBF_BIGGERSIZEOK, SIIGBF_ICONONLY,
    };
    let dbg = std::env::var_os("RL_ICONDBG").is_some();
    let wt = wide(exe_path);
    let factory: IShellItemImageFactory =
        match SHCreateItemFromParsingName(PCWSTR(wt.as_ptr()), None) {
            Ok(f) => f,
            Err(e) => {
                if dbg {
                    eprintln!("[icon] hires parse FAIL {:?} <- {}", e.code(), exe_path);
                }
                return None;
            }
        };
    let hbmp = factory
        .GetImage(SIZE { cx: 256, cy: 256 }, SIIGBF_ICONONLY | SIIGBF_BIGGERSIZEOK)
        .ok()?;
    if hbmp.0.is_null() {
        return None;
    }
    let (iw, ih) = (256i32, 256i32);
    let mut bmi = BITMAPINFO::default();
    bmi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
    bmi.bmiHeader.biWidth = iw;
    bmi.bmiHeader.biHeight = -ih;
    bmi.bmiHeader.biPlanes = 1;
    bmi.bmiHeader.biBitCount = 32;
    // ⚠️ 复用全局暂存区（见 ICON_SCRATCH）：不再每个图标申请一块 256KB。
    let need = (iw * ih * 4) as usize;
    let mut scratch = match ICON_SCRATCH.lock() {
        Ok(g) => g,
        Err(e) => e.into_inner(),
    };
    if scratch.len() < need {
        scratch.resize(need, 0);
    }
    let bits = &mut scratch[..need];
    let hdc = GetDC(None);
    let got = GetDIBits(
        hdc,
        hbmp,
        0,
        ih as u32,
        Some(bits.as_mut_ptr() as *mut c_void),
        &mut bmi,
        DIB_RGB_COLORS,
    );
    let _ = ReleaseDC(None, hdc);
    let _ = DeleteObject(HGDIOBJ(hbmp.0));
    if got == 0 {
        return None;
    }
    let opaque = bits.chunks_exact(4).filter(|c| c[3] == 0).count();
    if dbg {
        eprintln!(
            "[icon] hires bmp get={} opaquePx={}/{} <- {}",
            got,
            opaque,
            256 * 256,
            exe_path
        );
    }
    if !bits.chunks_exact(4).any(|c| c[3] != 0) {
        return None;
    }
    if dbg {
        eprintln!("[icon] hires 256x256  <- {}", exe_path);
    }
    // 一次性诊断：把 shell 给的**原始位图**和**归一化后的纹理**分别落盘，
    // 用来判断「图标糊」到底来自源图还是我方缩放。
    if let Some(dir) = std::env::var_os("RL_ICON_DUMP") {
        let dir = dir.to_string_lossy().to_string();
        let tag: String = exe_path
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .collect();
        let _ = save_bmp(&format!("{}\\raw_{}.bmp", dir, tag), bits, iw, ih);
        let _ = save_rgba(&format!("{}\\raw_{}.rgba", dir, tag), bits);
        let n = normalize_to(bits, iw as usize, ih as usize, ICON_PX);
        let _ = save_bmp(
            &format!("{}\\norm_{}.bmp", dir, tag),
            &n,
            ICON_PX as i32,
            ICON_PX as i32,
        );
        let _ = save_rgba(&format!("{}\\norm_{}.rgba", dir, tag), &n);
        return Some(n);
    }
    Some(normalize_to(bits, iw as usize, ih as usize, ICON_PX))
}


/// 从 exe 资源里提取最大的内嵌 PNG 图标（Vista+ 的 256px 图标都是 PNG），
/// 用 WIC 解码 —— 完全绕开 shell 的图标缓存/放大充数，源即所见图。
/// 返回 (边长px, BGRA 直通 alpha)
unsafe fn pe_png_icon(path: &str) -> Option<(usize, Vec<u8>)> {
    use windows::Win32::Graphics::Imaging::{
        CLSID_WICImagingFactory, IWICImagingFactory, WICDecodeMetadataCacheOnDemand,
    };
    use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};

    let d = std::fs::read(path).ok()?;
    if d.len() < 0x100 || &d[..2] != b"MZ" {
        return None;
    }
    let e = u32::from_le_bytes(d[0x3c..0x40].try_into().ok()?) as usize;
    if d.get(e..e + 4) != Some(b"PE\x00\x00") {
        return None;
    }
    let coff = e + 4;
    let nsec = u16::from_le_bytes(d[coff + 2..coff + 4].try_into().ok()?) as usize;
    let opt_size = u16::from_le_bytes(d[coff + 16..coff + 18].try_into().ok()?) as usize;
    let magic = u16::from_le_bytes(d[coff + 20..coff + 22].try_into().ok()?) as u16;
    let dd_off = coff + 20 + if magic == 0x20b { 112 } else { 96 };
    if dd_off + 24 > d.len() {
        return None;
    }
    let res_rva = u32::from_le_bytes(d[dd_off + 16..dd_off + 20].try_into().ok()?) as usize;
    if res_rva == 0 {
        return None;
    }
    let sec_off = coff + 20 + opt_size;
    let mut secs: Vec<(usize, usize, usize)> = Vec::new();
    for i in 0..nsec {
        let s = sec_off + i * 40;
        if s + 40 > d.len() {
            break;
        }
        let va = u32::from_le_bytes(d[s + 12..s + 16].try_into().ok()?) as usize;
        let sz = u32::from_le_bytes(d[s + 8..s + 12].try_into().ok()?) as usize;
        let raw = u32::from_le_bytes(d[s + 20..s + 24].try_into().ok()?) as usize;
        secs.push((va, sz, raw));
    }
    let rva2off = |rva: usize| -> Option<usize> {
        for (va, sz, raw) in &secs {
            if rva >= *va && rva < va + (*sz).max(1) {
                return Some(raw + (rva - *va));
            }
        }
        None
    };
    let r = rva2off(res_rva)?;
    // 目录：8 项 16 字节头 + 每项 8 字节（NameOrId, OffsetToData|0x80000000=子目录）
    let dir_entries = |off: usize| -> Option<Vec<(u32, u32)>> {
        if off + 16 > d.len() {
            return None;
        }
        let named = u16::from_le_bytes(d[off + 12..off + 14].try_into().ok()?) as usize;
        let ided = u16::from_le_bytes(d[off + 14..off + 16].try_into().ok()?) as usize;
        let mut v = Vec::new();
        for i in 0..named + ided {
            let e2 = off + 16 + i * 8;
            if e2 + 8 > d.len() {
                return None;
            }
            let name = u32::from_le_bytes(d[e2..e2 + 4].try_into().ok()?);
            let dvo = u32::from_le_bytes(d[e2 + 4..e2 + 8].try_into().ok()?);
            v.push((name, dvo));
        }
        Some(v)
    };
    // RT_ICON = 3：类型 → ID → 语言 → 数据
    let mut best: Option<(usize, usize, usize)> = None; // (png_len, off, 数据项里的估算尺寸)
    for (typ, dvo) in dir_entries(r)? {
        if typ != 3 || dvo & 0x8000_0000 == 0 {
            continue;
        }
        let l1 = rva2off(res_rva + (dvo & 0x7fff_ffff) as usize)?;
        for (_, dvo2) in dir_entries(l1)? {
            if dvo2 & 0x8000_0000 == 0 {
                continue;
            }
            let l2 = rva2off(res_rva + (dvo2 & 0x7fff_ffff) as usize)?;
            for (_, dvo3) in dir_entries(l2)? {
                if dvo3 & 0x8000_0000 != 0 {
                    continue;
                }
                let de = rva2off(res_rva + dvo3 as usize)?;
                if de + 8 > d.len() {
                    continue;
                }
                let drva = u32::from_le_bytes(d[de..de + 4].try_into().ok()?) as usize;
                let dsize = u32::from_le_bytes(d[de + 4..de + 8].try_into().ok()?) as usize;
                let doff = rva2off(drva)?;
                if doff + 8 > d.len() {
                    continue;
                }
                // PNG magic: 89 50 4E 47 0D 0A 1A 0A
                if d[doff] == 0x89 && d[doff + 1] == 0x50 && d[doff + 2] == 0x4E && d[doff + 3] == 0x47 {
                    if best.map(|(l, _, _)| dsize > l).unwrap_or(true) {
                        best = Some((dsize, doff, 0));
                    }
                }
            }
        }
    }
    let (_, off, _) = best?;
    let png = d.get(off..off + best.unwrap().0)?.to_vec();

    // WIC 解码 PNG → BGRA 直通 alpha
    let factory: IWICImagingFactory =
        CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER).ok()?;
    let stream = factory.CreateStream().ok()?;
    if stream.Write(png.as_ptr() as *const c_void, png.len() as u32, None).is_err() {
        return None;
    }
    let decoder = factory
        .CreateDecoderFromStream(&stream, std::ptr::null(), WICDecodeMetadataCacheOnDemand)
        .ok()?;
    let frame = decoder.GetFrame(0).ok()?;
    let mut w = 0u32;
    let mut h = 0u32;
    frame.GetSize(&mut w, &mut h).ok()?;
    if w == 0 || h == 0 {
        return None;
    }
    // ⚠️ 内存优化（2026-10-12）：全分辨率解码缓冲最大 256*256*4 = 256KB。
    // 原来每个图标申请一块、归一化后即弃 —— 但 **Windows 堆不主动归还 OS**，
    // 实测 20 个图标就白占约 5MB 常驻。这里复用全局同一块（见 `ICON_SCRATCH`），
    // 并且**在函数内就归一化**，让 256KB 的大缓冲**不泄漏到调用方**。
    let need = (w * h * 4) as usize;
    let mut scratch = match ICON_SCRATCH.lock() {
        Ok(g) => g,
        Err(e) => e.into_inner(),
    };
    if scratch.len() < need {
        scratch.resize(need, 0);
    }
    let buf = &mut scratch[..need];
    frame.CopyPixels(std::ptr::null(), w * 4, buf).ok()?;
    let tw = (w as usize).min(ICON_PX); // 只缩小不放大
    Some((tw, normalize_to(buf, w as usize, h as usize, tw)))
}

/// SHGetFileInfoW / PrivateExtractIconsW 提取 → AND 掩码重建透明度 → 归一化
unsafe fn extract_icon_bits(wt: &[u16]) -> Option<Vec<u8>> {
    let path_str = path_str_of(wt);
    let hicon = if std::path::Path::new(&path_str).exists() {
        let mut h = [HICON(ptr::null_mut())];
        let mut id = 0u32;
        let mut fname = [0u16; 260];
        let cp = wt.len().min(259);
        fname[..cp].copy_from_slice(&wt[..cp]);
        let n = PrivateExtractIconsW(
            &fname,
            0,
            256,
            256,
            Some(&mut h),
            Some(&mut id as *mut u32),
            0,
        );
        if n == 0 || h[0].0.is_null() {
            return None;
        }
        h[0]
    } else {
        let mut sfi = SHFILEINFOW::default();
        let r = SHGetFileInfoW(
            PCWSTR(wt.as_ptr()),
            FILE_ATTRIBUTE_NORMAL,
            Some(&mut sfi),
            std::mem::size_of::<SHFILEINFOW>() as u32,
            SHGFI_ICON | SHGFI_LARGEICON | SHGFI_USEFILEATTRIBUTES,
        );
        if r == 0 {
            return None;
        }
        sfi.hIcon
    };
    let mut ii = ICONINFO::default();
    if GetIconInfo(hicon, &mut ii).is_err() {
        let _ = DestroyIcon(hicon);
        return None;
    }
    let mut out: Option<Vec<u8>> = None;
    if !ii.hbmColor.0.is_null() {
        let (mut bw, mut bh) = (32i32, 32i32);
        let mut bm = BITMAP::default();
        if GetObjectW(
            ii.hbmColor,
            std::mem::size_of::<BITMAP>() as i32,
            Some(&mut bm as *mut _ as *mut c_void),
        ) > 0
        {
            bw = bm.bmWidth;
            bh = bm.bmHeight.abs();
        }
        if std::env::var_os("RL_ICONDBG").is_some() {
            eprintln!("[icon] native {}x{}  <- {}", bw, bh, path_str);
        }
        if bw > 0 && bw <= 256 && bh > 0 && bh <= 256 {
            let mut bmi = BITMAPINFO::default();
            bmi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
            bmi.bmiHeader.biWidth = bw;
            bmi.bmiHeader.biHeight = -bh;
            bmi.bmiHeader.biPlanes = 1;
            bmi.bmiHeader.biBitCount = 32;
            let n_px = (bw * bh) as usize;
            let mut bits = vec![0u8; n_px * 4];
            let hdc = GetDC(None);
            let got = GetDIBits(
                hdc,
                ii.hbmColor,
                0,
                bh as u32,
                Some(bits.as_mut_ptr() as *mut c_void),
                &mut bmi,
                DIB_RGB_COLORS,
            );
            if got > 0 {
                let mut mbmi = BITMAPINFO::default();
                mbmi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
                mbmi.bmiHeader.biWidth = bw;
                mbmi.bmiHeader.biHeight = -(bh * 2);
                mbmi.bmiHeader.biPlanes = 1;
                mbmi.bmiHeader.biBitCount = 1;
                let mask_row = ((bw + 31) / 32 * 4) as usize;
                let mut mbits = vec![0u8; mask_row * bh as usize * 2];
                let mask_ok = GetDIBits(
                    hdc,
                    ii.hbmMask,
                    0,
                    (bh * 2) as u32,
                    Some(mbits.as_mut_ptr() as *mut c_void),
                    &mut mbmi,
                    DIB_RGB_COLORS,
                ) > 0;
                let coverage = bits.iter().skip(3).step_by(4).filter(|&&a| a > 0).count();
                if coverage < n_px * 3 / 10 && mask_ok {
                    for y in 0..bh as usize {
                        for x in 0..bw as usize {
                            let i = y * bw as usize + x;
                            if bits[i * 4 + 3] == 0 {
                                let transparent =
                                    mbits[y * mask_row + x / 8] & (0x80 >> (x % 8)) != 0;
                                bits[i * 4 + 3] = if transparent { 0 } else { 255 };
                            }
                        }
                    }
                }
                if bits.chunks_exact(4).any(|c| c[3] != 0) {
                    out = Some(normalize_to(&bits, bw as usize, bh as usize, 32));
                }
            }
            let _ = ReleaseDC(None, hdc);
        }
    }
    let _ = DeleteObject(ii.hbmColor);
    let _ = DeleteObject(ii.hbmMask);
    let _ = DestroyIcon(hicon);
    out
}

/// 提取通用文档图标（无图标 exe 的回退）作为基准，进程内只算一次
unsafe fn generic_exe_icon() -> &'static Option<Vec<u8>> {
    static G: std::sync::OnceLock<Option<Vec<u8>>> = std::sync::OnceLock::new();
    G.get_or_init(|| extract_icon_bits(&wide("@@@generic@@@.zzz")))
}

unsafe fn default_browser_exe() -> Option<String> {
    let mut buf = [0u16; 520];
    let mut len = buf.len() as u32;
    let proto = wide("http");
    let hr = AssocQueryStringW(
        ASSOCF_NONE,
        ASSOCSTR_EXECUTABLE,
        PCWSTR(proto.as_ptr()),
        PCWSTR::null(),
        windows::core::PWSTR(buf.as_mut_ptr()),
        &mut len,
    );
    if hr.is_err() || len < 2 {
        return None;
    }
    Some(String::from_utf16_lossy(&buf[..(len - 1) as usize]))
}

/// 通用：按「关联名」（协议如 `http` / 扩展名如 `.zip`）查默认程序的 exe 全路径。
/// 与 `default_browser_exe` 同一个 API，只是不写死协议。
unsafe fn assoc_exe(assoc: &str) -> Option<String> {
    let mut buf = [0u16; 520];
    let mut len = buf.len() as u32;
    let a = wide(assoc);
    let hr = AssocQueryStringW(
        ASSOCF_NONE,
        ASSOCSTR_EXECUTABLE,
        PCWSTR(a.as_ptr()),
        PCWSTR::null(),
        windows::core::PWSTR(buf.as_mut_ptr()),
        &mut len,
    );
    if hr.is_err() || len < 2 {
        return None;
    }
    let exe = String::from_utf16_lossy(&buf[..(len - 1) as usize]);
    // ⚠️⚠️ 未关联的扩展名，Windows 会**兜底**成 `OpenWith.exe`（"你要用什么打开？"）。
    // 那不是真正的宿主 —— 必须当成"认不出"，否则会凭空建一个 OpenWith 应用，
    // 后续同类文件还会被误路由到它（用户 2026-10-12 实测：拖 zip/未知文件出怪东西）。
    let base = exe.rsplit(['\\', '/']).next().unwrap_or("").to_ascii_lowercase();
    if base.is_empty() || base == "openwith.exe" {
        return None;
    }
    Some(exe)
}

unsafe fn default_browser_icon() -> Option<(usize, Vec<u8>)> {
    let exe = default_browser_exe()?;
    decode_icon(&exe)
}

/// `shell:Downloads` / `shell:Desktop` / `shell:Personal`… → 真实文件夹路径。
/// 认不出来（或路径不存在）就返回 `None`，调用方退回命名空间解析。
fn shell_ns_to_path(target: &str) -> Option<String> {
    let home = std::env::var("USERPROFILE").ok()?;
    let sub = match target.get(6..)?.trim().to_ascii_lowercase().as_str() {
        "desktop" => "Desktop",
        "personal" | "mydocuments" | "documents" => "Documents",
        "downloads" => "Downloads",
        "pictures" | "mypictures" => "Pictures",
        "music" | "mymusic" => "Music",
        "videos" | "myvideos" => "Videos",
        "favorites" => "Favorites",
        _ => return None,
    };
    let p = format!("{}{}{}", home.trim_end_matches('\\'), '\\', sub);
    if std::path::Path::new(&p).exists() {
        Some(p)
    } else {
        None
    }
}

unsafe fn decode_icon(target: &str) -> Option<(usize, Vec<u8>)> {
    // ── 1) URL / URI / shell 命名空间 ──
    // 关键：绝不能把 URL 本体交给 shell —— SIIGBF_ICONONLY 对网站返回的是
    // 16/32px 的 favicon，被放大到 256 就是用户看到的「糊」。网站书签一律
    // 用默认浏览器 exe 的内嵌 256px PNG（PE 直读），统一且锐利。
    if target.contains("://") || target.starts_with("ms-") || target.starts_with("shell:") {
        if target.starts_with("http") {
            if let Some(exe) = default_browser_exe() {
                if let Some(v) = pe_png_icon(&exe) {
                    return Some(v);
                }
                if let Some(v) = shell_icon_hires(&exe) {
                    return Some((ICON_PX, v));
                }
            }
            return default_browser_icon();
        }
        if target.starts_with("shell:") {
            // `shell:Downloads` / `shell:Desktop` / `shell:Personal`…（之前这里
            // `return None` → 只剩首字母兜底）。
            //
            // ⚠️ **优先换成真实文件夹路径再取图标**：直接用命名空间解析，
            // `shell:Desktop` 会返回一张**全出血的渐变方块**（不是文件夹形状，
            // 小尺寸下就是一块蓝方块），而真实路径给的是正常的文件夹图标；
            // `shell:Downloads` / `shell:Personal` 两者一致，所以统一走路径。
            if let Some(p) = shell_ns_to_path(target) {
                if let Some(v) = shell_icon_hires(&p) {
                    return Some((ICON_PX, v));
                }
            }
            if let Some(v) = shell_icon_hires(target) {
                return Some((ICON_PX, v));
            }
        }
        if target.starts_with("ms-settings") {
            for p in [
                "C:\\Windows\\ImmersiveControlPanel\\SystemSettings.exe",
                "C:\\Windows\\System32\\SystemSettings.exe",
            ] {
                if let Some(v) = pe_png_icon(p) {
                    return Some(v);
                }
                if let Some(v) = shell_icon_hires(p) {
                    return Some((ICON_PX, v));
                }
            }
        }
        return None;
    }

    // ── 2) 解析真实文件路径：SearchPathW → System32 → Windows 目录 ──
    let mut full: Vec<u16> = Vec::new();
    if !target.contains('\\') {
        let name = wide(target);
        let mut buf = [0u16; 520];
        let mut part = windows::core::PWSTR::null();
        let n = windows::Win32::Storage::FileSystem::SearchPathW(
            None,
            PCWSTR(name.as_ptr()),
            None,
            Some(&mut buf),
            Some(&mut part),
        );
        if n > 0 && (n as usize) <= buf.len() {
            full = buf[..n as usize].to_vec();
        }
        // 商店应用别名（WindowsApps\mspaint.exe）是 reparse 点，但 shell 能把它
        // 解析成真实 App 图标 —— 先交给 shell，不要直接放弃
        if !full.is_empty() {
            let resolved = path_str_of(&full);
            if resolved.contains("WindowsApps") {
                if let Some(v) = shell_icon_hires(&resolved) {
                    return Some((ICON_PX, v));
                }
                return None;
            }
        }
        if full.is_empty() {
            for dir in ["C:\\Windows\\System32\\", "C:\\Windows\\"] {
                let cand = format!("{dir}{target}");
                if std::path::Path::new(&cand).exists() {
                    full = wide(&cand);
                    break;
                }
            }
        }
        if full.is_empty() {
            return None;
        }
    } else if !std::path::Path::new(target).exists() {
        return None;
    }
    let wt = if full.is_empty() { wide(target) } else { full };
    let path = path_str_of(&wt);

    // ── 3a) 最优先：exe 内嵌的 PNG 大图标（现代应用都有 256px），WIC 直解码
    //    shell 管线对部分路径会走图标缓存甚至把 32px 放大充数，均不可依赖
    // `pe_png_icon` 内部已归一化到 `min(原尺寸, ICON_PX)`（见其内存优化注释）
    if let Some(v) = pe_png_icon(&path) {
        return Some(v);
    }

    // ── 3b) shell 高分辨率（连 reparse 存根也能解析成真实图标）──
    if let Some(v) = shell_icon_hires(&path) {
        return Some((ICON_PX, v));
    }

    // ── 4) 到这一步还拿不到，且是 reparse 存根 → 只能提出假图标，放弃 ──
    {
        use windows::Win32::Storage::FileSystem::{
            GetFileAttributesW, FILE_ATTRIBUTE_REPARSE_POINT,
        };
        let attrs = GetFileAttributesW(PCWSTR(wt.as_ptr()));
        if attrs != u32::MAX && attrs & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 {
            return None;
        }
    }

    // ── 5) 32px 回退 ──
    let bits = extract_icon_bits(&wt)?;
    let g = generic_exe_icon().as_ref();
    if g == Some(&bits) {
        return None;
    }
    // ⚠️ 这里以前直接返回 32px 原始位图 —— **绕过了 normalize_to**，于是这条路径的图标
    // 既没裁掉透明留白、也没倒圆角，画出来就是「直角方块 + 可能偏心」。
    // 现在也过一遍（tw=32 不放大，只做「按 alpha 外接框裁切 + 圆角方块」）。
    Some((32, normalize_to(&bits, 32, 32, 32)))
}

// ═══════════════════════════════════════════════════════════════════
//  重建段 B：布局 / 钩子 / 交互 / 生命周期 / 窗口过程 / main
// ═══════════════════════════════════════════════════════════════════

// ───────────────────── GDI 资源 ─────────────────────

unsafe fn make_dib(w: i32, h: i32) -> (*mut u8, HDC, HBITMAP) {
    let sdc = GetDC(None);
    let mut bmi = BITMAPINFO::default();
    bmi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
    bmi.bmiHeader.biWidth = w;
    bmi.bmiHeader.biHeight = -h;
    bmi.bmiHeader.biPlanes = 1;
    bmi.bmiHeader.biBitCount = 32;
    bmi.bmiHeader.biCompression = BI_RGB.0;
    let mut bits: *mut c_void = ptr::null_mut();
    let bmp = match CreateDIBSection(
        sdc,
        &bmi as *const _ as *const BITMAPINFO,
        DIB_RGB_COLORS,
        &mut bits,
        None,
        0,
    ) {
        Ok(b) => b,
        Err(_) => {
            let _ = ReleaseDC(None, sdc);
            return (ptr::null_mut(), HDC(ptr::null_mut()), HBITMAP(ptr::null_mut()));
        }
    };
    let memdc = CreateCompatibleDC(sdc);
    let _ = ReleaseDC(None, sdc);
    let _ = SelectObject(memdc, HGDIOBJ(bmp.0));
    std::ptr::write_bytes(bits as *mut u8, 0, (w * h * 4) as usize);
    (bits as *mut u8, memdc, bmp)
}

unsafe fn init_surfaces(st: &mut State) {
    // 扁平化后不再抓屏模糊 —— 背景 DIB 仅 --dump 的合成壁纸会用到，
    // 常规运行时省下一块画布尺寸的缓冲
    if st.dump_bg {
        let (b, d, m) = make_dib(CANVAS, CANVAS);
        st.bg_bits = b;
        st.bg_dc = d;
        st.bg_bmp = m;
    }
    let (b2, d2, m2) = make_dib(CANVAS, CANVAS);
    st.dbi_bits = b2;
    st.dbi_dc = d2;
    st.dbi_bmp = m2;
    let (b3, d3, m3) = make_dib(CANVAS, 140);
    st.txt_bits = b3;
    st.txt_dc = d3;
    st.txt_bmp = m3;
}

unsafe fn init_fonts(st: &mut State) {
    let face: Vec<u16> = wide("Microsoft YaHei UI");
    st.font_name = CreateFontW(-(15.0 * K).round() as i32, 0, 0, 0, 600, 0, 0, 0, 134, 0, 0, 2, 0, PCWSTR(face.as_ptr()));
    st.font_label = CreateFontW(-(11.0 * K).round() as i32, 0, 0, 0, 500, 0, 0, 0, 134, 0, 0, 2, 0, PCWSTR(face.as_ptr()));
    st.font_row = CreateFontW(-(13.0 * K * KC).round() as i32, 0, 0, 0, 600, 0, 0, 0, 134, 0, 0, 2, 0, PCWSTR(face.as_ptr()));
    st.font_hint = CreateFontW(-(10.0 * K).round() as i32, 0, 0, 0, 400, 0, 0, 0, 134, 0, 0, 2, 0, PCWSTR(face.as_ptr()));
    st.font_head = CreateFontW(-(16.0 * K * KC).round() as i32, 0, 0, 0, 700, 0, 0, 0, 134, 0, 0, 2, 0, PCWSTR(face.as_ptr()));
    st.font_cnt = CreateFontW(-(12.0 * K).round() as i32, 0, 0, 0, 500, 0, 0, 0, 134, 0, 0, 2, 0, PCWSTR(face.as_ptr()));
    st.font_glyph = CreateFontW(-(22.0 * K).round() as i32, 0, 0, 0, 700, 0, 0, 0, 134, 0, 0, 2, 0, PCWSTR(face.as_ptr()));
    // 气泡：**绝对像素**，不乘 K（理由见 State 里的注释）
    st.font_bub = CreateFontW(-14, 0, 0, 0, 500, 0, 0, 0, 134, 0, 0, 2, 0, PCWSTR(face.as_ptr()));
    st.font_bub_s = CreateFontW(-12, 0, 0, 0, 400, 0, 0, 0, 134, 0, 0, 2, 0, PCWSTR(face.as_ptr()));
    // 3 倍超采样字体：与上面各字号一一对应 ×3（draw_text_rot 旋转文字防糊）
    st.font_name3 = CreateFontW(-(45.0 * K).round() as i32, 0, 0, 0, 600, 0, 0, 0, 134, 0, 0, 2, 0, PCWSTR(face.as_ptr()));
    st.font_label3 = CreateFontW(-(33.0 * K).round() as i32, 0, 0, 0, 500, 0, 0, 0, 134, 0, 0, 2, 0, PCWSTR(face.as_ptr()));
    st.font_row3 = CreateFontW(-(39.0 * K * KC).round() as i32, 0, 0, 0, 600, 0, 0, 0, 134, 0, 0, 2, 0, PCWSTR(face.as_ptr()));
    st.font_hint3 = CreateFontW(-(30.0 * K).round() as i32, 0, 0, 0, 400, 0, 0, 0, 134, 0, 0, 2, 0, PCWSTR(face.as_ptr()));
    st.font_head3 = CreateFontW(-(48.0 * K * KC).round() as i32, 0, 0, 0, 700, 0, 0, 0, 134, 0, 0, 2, 0, PCWSTR(face.as_ptr()));
    st.font_cnt3 = CreateFontW(-(36.0 * K).round() as i32, 0, 0, 0, 500, 0, 0, 0, 134, 0, 0, 2, 0, PCWSTR(face.as_ptr()));
    st.font_glyph3 = CreateFontW(-(66.0 * K).round() as i32, 0, 0, 0, 700, 0, 0, 0, 134, 0, 0, 2, 0, PCWSTR(face.as_ptr()));
    // ⚠️ 设置页那5 号 SS× 字体**不在这里建** —— 只有设置页在用，而 GDI 每
    //    HFONT 各持一份字形缓存（yahei 18.8MB 的文件，只驻留用到的字形）。
    //    改成 `ensure_setup_fonts` 按需创建、`setup_free` 关窗时删。
}

// ───────────────────── 径向布局 ─────────────────────

/// 一屏几个槽：≤`RING_SLOTS` 个应用时就是应用数本身（均分一整圈，和老行为一模一样）；
/// 超过则恒为 `RING_SLOTS` —— 多出来的应用靠滚轮连续滚动看到（`State::ring_off`）。
/// **所有几何函数都先过这一道**，所以 `ring_layout(n)`/`disc_r(n)` 对 n>12 不再继续缩图标。
#[inline]
fn ring_vis(n: usize) -> usize {
    n.min(RING_SLOTS).max(1)
}

/// 一格槽位的角跨（度）
#[inline]
fn ring_step(n: usize) -> f32 {
    360.0 / ring_vis(n) as f32
}

/// 滚动偏移的取值上限。按**布局格数**算（`eff_apps`）—— 幽灵也占一格，
/// 所以「满环 + 拖进来一个新应用」正好能滚出一格把幽灵磁贴露出来。
#[inline]
fn ring_off_max(n: usize) -> f32 {
    n.saturating_sub(RING_SLOTS) as f32
}

/// 槽位下标 `i`（含幽灵槽 `apps.len()`）的**连续槽位** = 下标 − 滚动偏移。
/// 小数是常态：滚动动画里偏移本身就是小数。
#[inline]
fn slot_of(st: &State, i: usize) -> f32 {
    i as f32 - st.ring_off
}

/// 连续槽位 → 方位角（0 = 正上方，顺时针；与 `polar` 同一约定）。
///
/// ⚠️ **不做 360° 取模**：槽位 `vis` 和槽位 `0` 是同一个物理位置，但只有落在
/// 可见窗口里的那一个该画出来 —— 而 `slot_appear` 正是靠**未取模**的槽位判断这一点。
#[inline]
fn slot_angle(s: f32, n: usize) -> f32 {
    s * ring_step(n)
}

/// 连续槽位 → 缝隙处的可见度（1 = 完全可见，0 = 已滚出去 / 还没滚进来）。
///
/// 可见窗口 = `[-0.5, vis-0.5)`，**两端落在同一个楔形里**（第 `vis` 格与第 1 格之间，
/// 约 11:30 位置）：出去的那个淡出、进来的那个淡入，在这一带交叉。
/// 淡出带宽 = 0.5 槽 = 「缝隙中心 → 最近一格」的距离，于是**静止时（偏移为整数）
/// 每一格的 alpha 恒为 1** —— 画面和没这功能时完全一致，只有滚动过程中才有淡出。
fn slot_appear(s: f32, n: usize) -> f32 {
    let v = ring_vis(n) as f32;
    let k = clamp01((s + 0.5) * 2.0).min(clamp01((v - 0.5 - s) * 2.0));
    // smoothstep：线性 alpha 在快速滚动时会看出「一闪一闪」
    k * k * (3.0 - 2.0 * k)
}

/// 极角（度，0=上、顺时针）→ 这一格**当前显示**的槽位下标（`0..n`，`n` = `eff_apps`，含幽灵槽）。
///
/// ⚠️ 命中判定**只许**走这里（`update_hover` / `nearest_app` / `drop_slot_app` 共用），
/// 而且它和绘制共用同一份 `slot_of` + `slot_appear` —— 否则滚动偏移一变就「画在这、点到那」。
/// 可见度 <0.5 的格子直接跳过：不允许点着一块几乎看不见的图标。
fn slot_at_deg(st: &State, deg: f32, n: usize) -> Option<usize> {
    let per = ring_step(n);
    let mut best: Option<usize> = None;
    let mut bd = f32::MAX;
    for i in 0..n {
        let s = slot_of(st, i);
        if slot_appear(s, n) < 0.5 {
            continue;
        }
        let mut d = slot_angle(s, n) - deg;
        d = (d + 180.0).rem_euclid(360.0) - 180.0;
        if d.abs() < bd {
            bd = d.abs();
            best = Some(i);
        }
    }
    // 只认「离最近一格不超过半格」的（楔形尽头那两个都在淡出，已由上面的 0.5 门卡掉）
    if bd <= per * 0.5 + 2.0 {
        best
    } else {
        None
    }
}

/// 极角 → 应用下标（`0..apps.len()`）。幽灵槽不是「应用」，不参与 hover。
fn app_at_deg(st: &State, deg: f32, n: usize) -> Option<usize> {
    slot_at_deg(st, deg, n).filter(|&i| i < st.apps.len())
}

/// 选中项滚出可见窗口 → 撤掉选中（外环跟着收）。
/// `active_app` 是**故意粘性**的，但粘的是「哪个应用」，不是「它那一格还看得见」——
/// 让它锚在一块已经淡出的图标上，外环就会在盘面上凭空浮出来。
fn scroll_prune(st: &mut State) {
    if let Some(ai) = st.active_app {
        let n = eff_apps(st);
        if ai >= n || slot_appear(slot_of(st, ai), n) < 0.5 {
            st.active_app = None;
            st.hover_chip = None;
        }
    }
}

/// 把滚动**目标**收到「槽位 `i` 落在可见窗口里」所需的最小幅度 ——
/// 只做必要的滚动，不居中。用在两处：拖进来的幽灵磁贴、键盘 `nav_app` 换选中。
/// ⚠️ 每帧都复查（`on_timer` 里调）：应用数和幽灵随时会变，算一次不够。
fn ring_reveal(st: &mut State, i: usize) {
    let n = eff_apps(st);
    let mx = ring_off_max(n);
    if mx <= 0.0 {
        st.ring_to = 0.0;
        return;
    }
    // 可见窗口 = [off, off+vis) → `i` 可见 ⇔ off ∈ [i-(vis-1), i]
    let v = ring_vis(n) as f32;
    let f = i as f32;
    let lo = (f - v + 1.0).max(0.0);
    let hi = f.min(mx);
    st.ring_to = st.ring_to.clamp(lo, hi);
}

/// 滚轮转 `d` 格（`d > 0` = 往后一页的方向）：只改**目标**偏移，动画归 `on_timer`。
/// 返回 `false` = 没得滚（没超过一屏，或已经到头）。
fn ring_scroll(st: &mut State, d: f32) -> bool {
    let mx = ring_off_max(eff_apps(st));
    if mx <= 0.0 {
        return false;
    }
    let to = (st.ring_to + d).clamp(0.0, mx);
    if (to - st.ring_to).abs() < 1e-3 {
        return false; // 顶到边界了
    }
    st.ring_to = to;
    // 滚轮也是「输入」：光标压在轮盘中心不动时，`note_input` 不会刷新心跳，
    // 一直滚就会在 12s 后被闲置自动收起打断（用户 2026-10-25 报的卡顿感之一）。
    st.last_input = now_s();
    st.hit_app = None;
    st.hover_now = None;
    scroll_prune(st);
    true
}

/// 方位角（0=上，顺时针）→ 画布坐标
fn polar(r: f32, deg: f32) -> (f32, f32) {
    let a = deg.to_radians();
    (CC + r * a.sin(), CC - r * a.cos())
}

/// 估算文字宽度（CJK 按全宽，其余按半宽）
fn est_text_w(text: &str, cjk_w: f32, lat_w: f32) -> f32 {
    text.chars().map(|c| if c as u32 > 0x2E7F { cjk_w } else { lat_w }).sum()
}

/// 内环自适应布局：返回 (图标环半径, 图标绘制边长)。
///
/// 规则（和 `chip_layout` 同一套思路 —— 布局由「内容尺寸」反推，不由常量钉死）：
/// 1. 先按**最大图标**求半径：相邻两格的弦长必须容得下「方块切向投影 + 目标间隙」
///    `2·r·sin(π/n) ≥ icon·TANG + icon·GAP_K`；
/// 2. 半径超过 `DISC_R_MAX` 就封顶（菜单不能无限大），此时**反过来缩图标**保间隙 ——
///    宁肯图标小一点，也不要挤成一片。
///
/// 旧实现的目标只是「刚好不相撞」（`icon·1.25 + 6`），所以 12 个应用时
/// 间隙只有 0.28×图标，视觉上就是「贴在一起」。
fn ring_layout(n: usize) -> (f32, f32) {
    let hs = (std::f32::consts::PI / ring_vis(n) as f32).sin();
    let mut icon = ICON_DRAW;
    let mut r = icon * (TANG + GAP_K) / (2.0 * hs);
    let r_cap = DISC_R_MAX - icon * 0.5 - BAND_PAD;
    if r > r_cap {
        r = r_cap;
        icon = 2.0 * r * hs / (TANG + GAP_K);
    }
    icon = icon.clamp(ICON_MIN, ICON_DRAW);
    // 条目很少时别把环缩到盘心（要给中心头像留位置）
    let r = r.max(R_ICON_MIN).max(HUB_R + icon * 0.5 + 10.0);
    (r, icon)
}

/// 盘面半径：图标环 + 图标半宽 + 环带留白。**绘制、命中、chip 环、压暗范围必须都走这里**，
/// 否则「画在哪」和「点到哪」又会分家。
fn disc_r(n: usize) -> f32 {
    let (r, icon) = ring_layout(n);
    r + icon * 0.5 + BAND_PAD
}

/// 环带内沿（中心留白区的半径）。
/// **压到头像球底下**（`HUB_R - 5`）—— 头像周围不留白圈：球是后画的，会盖住这一圈，
/// 于是视觉上「灰色环带 → 球」直接相接。留 5px 余量是为了呼吸动画
/// （球半径会缩 ~2.4%，不留余量会在球最小时露出一道白边）。
/// 环带内沿半径 —— 压到头像球底下（球是后画的会盖住这一圈）。
/// 留 5px 是因为呼吸动画会让球半径缩 ~2.4%。
fn band_in(n: usize) -> f32 {
    let (r, icon) = ring_layout(n);
    (HUB_R - 5.0).min(r - icon * 0.5 - 6.0).max(HUB_R * 0.5)
}

// ───────────────────── 外环：梯形环（圆角环形扇区）─────────────────────
//
// 形态模仿 StarPie 的「经典紧凑扇区」：环带切成 m 段，每段 = 两条**直的径向边**
// + 内弧 + 外弧，四角大圆角，段间留一条角度缝。
//
// 颜色**与内环完全一致**（§2「统一圆角方块」的那套）：白底 + `mix(ink 34%)` 描边
// + `0 3K` 硬投影。整个界面里唯一有颜色的东西是**真实图标**，内环也是这个规矩。
//
// **排布 = 「甲」方案（用户 2026-10-08 拍板）**：段仍然**贴身**铺在对应内环图标
// 的方位角两侧（`chip_layout` 的贴身扇面），内容**沿切线横排**（图标在左、名称在右）
// 并跟着弧转 —— 也就是「壳从胶囊换成梯形段，里面一律没动」，改动最小。

/// 外环厚度 = **与原来的胶囊同高**（`CHIP_H`）。
/// 「甲」方案要的是「保持现有排布 + 只换外壳形状」，所以厚度不变。
const RING_T: f32 = CHIP_H;
/// 四角圆角半径 = 厚度 × 这个比例（薄环用更大的比例，跟参考图一致）
const RING_CR_K: f32 = 0.30;
/// hover 时沿**径向向外**抬起的距离系数（段与段内内容必须用同一个，否则会脱开）
const RING_LIFT_K: f32 = 2.0;
/// 段里**名称的宽度上限**（px）。段宽正比于名字宽度 —— 不封顶的话一个 18 字符的
/// URL（`newtab.claisly.com`）会让单段吃掉 50°+ 的环，整圈被它一个人占满。
/// 超过就截断加省略号。`chip_half_w` 与段内绘制**必须用同一个上限**，
/// 否则「段宽 / 文字宽 / 命中框」三者会对不上。
const RING_NAME_MAX_W: f32 = 90.0;
/// 段内名称字号（传给 `draw_text_rot`：13 → `font_row`，13*K*KC ≈ 11.5px）
const RING_NAME_PX: i32 = 13;

/// 外环的内外半径（跟着自适应的盘面半径走）
fn ring_radii(n: usize) -> (f32, f32) {
    let r_in = disc_r(n) + 3.0 * K;
    (r_in, r_in + RING_T)
}

/// 圆角环形扇区的 SDF（**内部为负** —— 与项目里所有 `flat_*` 的约定一致）。
///
/// 做法：把像素换算到「(径向 u, 切向 v)」局部坐标，其中 **v 用中间半径的弧长刻度**
/// （`v = Δθ · r_mid`）—— 这样等 v 线正好是**真正的径向直线**（梯形要的就是直边）；
/// 若写成 `v = Δθ · r` 会得到微微弯曲的边。然后当圆角矩形求 SDF 即可。
fn sdf_sector(px: f32, py: f32, cx: f32, cy: f32,
              r_mid: f32, hu: f32, hv: f32, cr: f32, mid_rad: f32) -> f32 {
    let dx = px - cx;
    let dy = py - cy;
    let r = (dx * dx + dy * dy).sqrt();
    // 角度约定与 `polar` 一致：0 = 正上、顺时针
    let th = dx.atan2(-dy);
    let tau = std::f32::consts::TAU;
    let mut d = th - mid_rad;
    d -= tau * (d / tau).round();
    // ⚠️ `sdf_rounded_rect` 是「**内部为正**」的约定，而 `cov1()` 按「内部为负」用
    // （本项目所有 `flat_*` 都取内部为负）。这里必须取负号 ——
    // 忘了取负的后果不是「画小一点」，而是**填充整个包围盒、把梯形挖成洞**
    // （2026-10-08 真机调试时踩到：段变成一片白方框 + 中间透出壁纸）。
    -sdf_rounded_rect(r - r_mid, d * r_mid, hu, hv, cr)
}

/// 沿一条弧画虚线 —— 项目没有虚线原语，用 `rot_rrect` 沿**切向**摆一排小圆角条。/// `a1/a2` 是**角度**（与 `polar` 同约定：0 = 正上、顺时针）。
/// ⚠️ 切向单位向量 = (cos a, sin a) → `rot = a` 的弧度；
///    插入光标那个是**径向**（`a − π/2`），别混（2026-10-14 栽过一次）。
unsafe fn dashed_arc(buf: &mut [u8], r: f32, a1: f32, a2: f32, n: usize,
                     col: (u8, u8, u8), alpha: f32, hw_t: f32) {
    if n == 0 || a2 <= a1 {
        return;
    }
    let slot_deg = (a2 - a1) / n as f32;
    let half_len = (slot_deg.to_radians() * r) * 0.28;
    for k in 0..n {
        let a = a1 + slot_deg * (k as f32 + 0.5);
        let (x, y) = polar(r, a);
        rot_rrect(buf, x, y, half_len, hw_t, hw_t, a.to_radians(), col, alpha);
    }
}

/// 一段**连续**的细圆弧（描边，不是扇形填充）。`a1/a2` = 角度，约定同 `polar`（0=上、顺时针）。
///
/// 直接复用梯形环那套 SDF（`sdf_sector`）：把径向半厚收到一两像素、圆角取满，
/// 两端自然成圆头 —— 不用为刻度另写一份几何。
unsafe fn flat_arc(buf: &mut [u8], r: f32, a1: f32, a2: f32, hw: f32,
                   col: (u8, u8, u8), alpha: f32) {
    if a2 <= a1 || alpha <= 0.002 {
        return;
    }
    let mid_rad = ((a1 + a2) * 0.5).to_radians();
    // 切向半长用**中间半径的弧长**刻度（与 `sdf_sector` 的约定一致）
    let hv = ((a2 - a1) * 0.5).to_radians() * r;
    // 包围盒：沿弧采样，内外两个半径各取一次（稳，不依赖解析极值）
    let mut minx = f32::MAX;
    let mut maxx = f32::MIN;
    let mut miny = f32::MAX;
    let mut maxy = f32::MIN;
    let steps = ((a2 - a1) / 3.0).ceil().max(2.0) as usize;
    for k in 0..=steps {
        let a = a1 + (a2 - a1) * k as f32 / steps as f32;
        for rr in [r - hw, r + hw] {
            let (x, y) = polar(rr, a);
            minx = minx.min(x);
            maxx = maxx.max(x);
            miny = miny.min(y);
            maxy = maxy.max(y);
        }
    }
    let pad = hw + 3.0;
    let x0 = (minx - pad).floor().max(0.0) as i32;
    let y0 = (miny - pad).floor().max(0.0) as i32;
    let x1 = (maxx + pad).ceil().min(CANVAS as f32 - 1.0) as i32;
    let y1 = (maxy + pad).ceil().min(CANVAS as f32 - 1.0) as i32;
    let w = CANVAS as usize;
    for y in y0..=y1 {
        let fy = y as f32 + 0.5;
        for x in x0..=x1 {
            let fx = x as f32 + 0.5;
            // 极坐标原点**只能是画布中心**（同梯形环那条铁律）
            let c = cov1(sdf_sector(fx, fy, CC, CC, r, hw, hv, hw, mid_rad)) * alpha;
            if c > 0.002 {
                let o = (y as usize * w + x as usize) * 4;
                blend_over(buf, o, col.0 as f32, col.1 as f32, col.2 as f32, c);
            }
        }
    }
}


/// 玻璃参数：关着、或快照还没抓好 → `None`（走纸面）。
fn glass_arg<'a>(st: &'a State, disc: f32) -> Option<(&'a [u8], bool, f32)> {
    if !st.glass || st.glass_bg.len() < (CANVAS * CANVAS * 4) as usize * 2 {
        None
    } else {
        Some((st.glass_bg.as_slice(), st.glass_liquid, disc))
    }
}

/// 画一段梯形环（含硬投影、填充、描边）。
#[allow(clippy::too_many_arguments)]
fn draw_ring_sector(buf: &mut [u8], pal: &Pal, r_in: f32, r_out: f32,
                    seg: (f32, f32, f32), hot: f32, alpha: f32, mode: u8,
                    glass: Option<(&[u8], bool, f32)>) {
    if alpha <= 0.01 {
        return;
    }
    let (mid_deg, a1, a2) = seg;
    let hu = (r_out - r_in) * 0.5;
    let r0 = (r_in + r_out) * 0.5;                 // 基准中位半径（不含 hover 抬升）
    let alpha_span = (a2 - a1).to_radians();
    let hv = alpha_span * 0.5 * r0;
    let cr = (RING_T * RING_CR_K).min(hu * 0.98).min(hv * 0.98);
    let mid_rad = mid_deg.to_radians();

    // mode：0 = 正常段（白底 + 描边 + 硬投影）
    //       1 = 改名中的段（墨底，无描边无投影）
    //       2 = 提交成功的一次白色闪光
    // hover：**加粗描边 + 沿径向外抬**（内环的选中态也是「加粗描边 + 抬升」，不另画托底）。
    // 抬升直接用「半径 +lift」表达 —— SDF 本来就在极坐标里，不用做屏幕位移。
    let plain = mode != 0;
    // ⚠️ 描边宽度/填充/投影**必须与内环底板用同一套公式**（用户 2026-10-08：
    // 「梯形颜色跟内环要一致」）。底板是 `(1.4 + 1.6*sel)*K` / `pal.paper` /
    // `(3.0+2.0*sel)*K` 偏移 + `0.20+0.10*sel` 强度 —— 这里以前是 2.0/1.1、
    // 填充还掺了 2% 墨、投影只有 0.13，所以两圈看着不是一种材料。
    let edge_w = if plain { 0.0 } else { (1.4 + 1.6 * hot) * K };
    let lift = 2.0 * K * hot;
    let r_mid = r0 + lift;
    // ⚠️ 极坐标原点**只能是画布中心**。之前这里收了一个 `cx, cy` 参数、调用方传的是
    // 「段中心」→ 半径算成了相对段中心的距离，整段被画成一坨巨形（真机一眼就看出）。
    let (cx, cy) = (CC, CC);

    // 包围盒：沿弧采样 + 取内外两个半径（稳、不依赖解析极值）
    let mut minx = f32::MAX;
    let mut maxx = f32::MIN;
    let mut miny = f32::MAX;
    let mut maxy = f32::MIN;
    let steps = ((a2 - a1).abs() / 3.0).ceil().max(2.0) as usize;
    for k in 0..=steps {
        let a = a1 + (a2 - a1) * k as f32 / steps as f32;
        for rr in [r_in + lift, r_out + lift] {
            let (x, y) = polar(rr, a);
            minx = minx.min(x);
            maxx = maxx.max(x);
            miny = miny.min(y);
            maxy = maxy.max(y);
        }
    }
    let pad = edge_w + 3.0 * K + 3.0;
    let x0 = (minx - pad).floor().max(0.0) as i32;
    let y0 = (miny - pad).floor().max(0.0) as i32;
    let x1 = (maxx + pad).ceil().min(CANVAS as f32 - 1.0) as i32;
    let y1 = (maxy + pad).ceil().min(CANVAS as f32 - 1.0) as i32;

    // 与内环底板同一套硬投影：偏移 `(3.0+2.0*sel)*K`、强度 `0.20+0.10*sel`
    let sh_dy = if plain { 0.0 } else { (3.0 + 2.0 * hot) * K };
    let sh_a = if plain { 0.0 } else { 0.20 + 0.10 * hot };
    // 填充一律 `pal.paper`（纯白）—— 内环底板也是纯白，不能掺墨变灰
    let fill = match mode {
        1 => pal.block,
        _ => pal.paper,
    };
    let edge = mixc(mixc(pal.paper, pal.ink, 0.34), pal.ink, hot);
    let w = CANVAS as usize;
    for y in y0..=y1 {
        let fy = y as f32 + 0.5;
        for x in x0..=x1 {
            let fx = x as f32 + 0.5;
            let s = sdf_sector(fx, fy, cx, cy, r_mid, hu, hv, cr, mid_rad);
            let o = (y as usize * w + x as usize) * 4;
            let ss = sdf_sector(fx, fy - sh_dy, cx, cy, r_mid, hu, hv, cr, mid_rad);
            let a_sh = cov1(ss) * sh_a * alpha;
            if a_sh > 0.002 {
                blend_over(buf, o, pal.ink.0 as f32, pal.ink.1 as f32, pal.ink.2 as f32, a_sh);
            }
            let a_f = cov1(s) * alpha;
            if a_f > 0.002 {
                // 玻璃段：填充色来自**模糊桌面 + tint**（`glass_px` 里盘的边缘高光
                // 在这条半径上不成立 —— 段在盘外，`e = disc_r - d` 是负的，正好自动跳过）。
                // 于是胶囊看起来和盘是同一块材料，而不是"玻璃盘上贴了白纸板"。
                match glass {
                    Some((bg, dark, dr)) if mode == 0 && bg.len() >= (CANVAS * CANVAS * 4) as usize * 2 => {
                        let ta = if dark { GLASS_CHIP_A_D } else { GLASS_CHIP_A_L };
                        let (gr, gg, gb) =
                            glass_px(bg, fx, fy, CC, CC, dr, dark, ta, GLASS_CHIP_RIM, GLASS_SHARP);
                        blend_over(buf, o, gr, gg, gb, a_f);
                    }
                    _ => blend_over(buf, o, fill.0 as f32, fill.1 as f32, fill.2 as f32, a_f),
                }
            }
            let a_e = cov1(s.abs() - edge_w * 0.5) * alpha;
            if a_e > 0.002 {
                blend_over(buf, o, edge.0 as f32, edge.1 as f32, edge.2 as f32, a_e);
            }
        }
    }
}

/// 段内文字放不下就截断成 `名字…`。
/// ⚠️ 放不下时**至少要画出首字** —— 绝不让整条名字凭空消失（「66」这么丢过一次）。
fn fit_chip_label(name: &str, avail: f32, cjk: f32, lat: f32) -> String {
    let mut label = name.to_string();
    if est_text_w(&label, cjk, lat) > avail {
        let mut acc = String::new();
        for ch in name.chars() {
            let mut t = acc.clone();
            t.push(ch);
            if est_text_w(&format!("{}…", t), cjk, lat) > avail {
                break;
            }
            acc.push(ch);
        }
        if acc.is_empty() {
            acc = name.chars().take(1).collect();
        } else {
            acc.push('…');
        }
        label = acc;
    }
    label
}

/// 「即将新增」的**幽灵胶囊**：虚线轮廓 + ＋ + 名字。
///
/// 视觉语言刻意与「内环教学槽位」「拖到盘外移除」的整圈虚线保持一致 ——
/// 用户已经学过「虚线 = 这里会多一个 / 这里会没掉」，不用再教第二遍。
/// 与内环 `drop_ghost`（新应用的半透明磁贴）是同语义的内外两环版本。
unsafe fn draw_ghost_chip(st: &State, buf: &mut [u8], pal: &Pal, seg: (f32, f32), name: &str, alpha: f32) {
    if alpha <= 0.02 {
        return;
    }
    let (r_in, r_out) = ring_radii(st.apps.len());
    let (a1, a2) = seg;
    let mid = (a1 + a2) * 0.5;
    let col = mixc(pal.paper, pal.ink, 0.50);
    let lt = 1.5 * K;
    // ① 内外弧虚线
    let dashes = (((a2 - a1) / 7.0).round() as usize).clamp(3, 7);
    dashed_arc(buf, r_in, a1, a2, dashes, col, 0.95 * alpha, lt);
    dashed_arc(buf, r_out, a1, a2, dashes, col, 0.95 * alpha, lt);
    // ② 两条径向直边虚线（沿半径方向摆小条）
    let nd = 4usize;
    let dr = (r_out - r_in) / nd as f32;
    for side in [a1, a2] {
        for k in 0..nd {
            let t = r_in + dr * (k as f32 + 0.5);
            let (x, y) = polar(t, side);
            rot_rrect(buf, x, y, dr * 0.26, lt * 0.5, lt * 0.5, side.to_radians(), col, 0.95 * alpha);
        }
    }
    // ③ 段内：＋（占图标位）+ 名字（沿弧排，与真胶囊同一套排版与截断）
    let r_mid = (r_in + r_out) * 0.5;
    let half = ((a2 - a1).to_radians() * 0.5 * r_mid).max(6.0);
    let place = |lx: f32| -> (f32, f32, f32) {
        let ang = mid + (lx / r_mid).to_degrees();
        let (x, y) = polar(r_mid, ang);
        (x, y, norm_rot(ang))
    };
    let (ppx, ppy, pang) = place(-half + CHIP_PAD_L + CHIP_ICON * 0.5);
    let arm = CHIP_ICON * 0.36;
    rot_rrect(buf, ppx, ppy, arm, lt, lt, pang.to_radians(), col, 0.95 * alpha);
    rot_rrect(buf, ppx, ppy, lt, arm, lt, pang.to_radians(), col, 0.95 * alpha);

    let (cjk, lat) = (13.0 * K * KC, 7.0 * K * KC);
    let avail = (2.0 * half - CHIP_PAD_L - CHIP_ICON - CHIP_GAP - CHIP_PAD_R + 1.0).max(10.0);
    let label = fit_chip_label(name, avail, cjk, lat);
    if !label.is_empty() {
        let tw = est_text_w(&label, cjk, lat);
        let (tx, ty, tang) = place(-half + CHIP_PAD_L + CHIP_ICON + CHIP_GAP + tw * 0.5);
        draw_text_rot(st, buf, tx, ty, &label, RING_NAME_PX, tang.to_radians(),
                      col.0, col.1, col.2, (255.0 * alpha).clamp(0.0, 255.0) as u8);
    }
}

/// 段内内容：**沿弧线排**（图标在左 + 名称在右），每个元素**各自**贴在弧上、
/// 各自按该处的切线旋转 —— 这就是「甲」的「图标在左、名称在右、跟着弧转」。
///
/// ⚠️ 但**不能**把它们摆在一条**共用的切线**上（第一版就是这么写的）：切线只是弧
/// 在一点处的近似，离切点越远偏得越多（偏差 ≈ `d² / 2r`）。30px 的环带根本装不下，
/// 表现就是**图标被甩到段外面、文字只有中间一截露出来**（2026-10-08 真机踩到）。
/// 改成「每个元素用自己的弧长位置 + 自己那处的切线角」之后，偏差只剩
/// 「半个元素尺度 + 弦高」，10px 以内，稳稳待在环带里。
///
/// `fg` = 文字颜色（普通态 `ink` / 改名态白）；`caret` = 画不画编辑光标。
#[allow(clippy::too_many_arguments)]
unsafe fn draw_ring_content(st: &State, buf: &mut [u8], pal: &Pal,
                            seg: (f32, f32, f32), r_in: f32, r_out: f32,
                            icon: Option<&IconTex>, name: &str,
                            fg: (u8, u8, u8), hot: f32, caret: bool, alpha: f32,
                            loading: bool) {
    let (mid_deg, a1, a2) = seg;
    let a8 = (255.0 * alpha).clamp(0.0, 255.0) as u8;
    if a8 == 0 {
        return;
    }
    // hover 抬升必须与 `draw_ring_sector` 用同一个公式，否则内容会和段脱开
    let r_mid = (r_in + r_out) * 0.5;
    let rc = r_mid + RING_LIFT_K * K * hot;
    // 局部坐标（lx = 沿弧的**弧长**、ly = 径向）→ 画布坐标 + 该处的切线角（度）。
    // ⚠️ 不要退回「切线近似」，理由见函数头注释。
    let place = |lx: f32, ly: f32| -> (f32, f32, f32) {
        let ang = mid_deg + (lx / rc).to_degrees();
        let (x, y) = polar(rc + ly, ang);
        (x, y, norm_rot(ang))
    };
    let half = ((a2 - a1).to_radians() * 0.5 * r_mid).max(6.0);
    let (cjk, lat) = (13.0 * K * KC, 7.0 * K * KC);
    // `+1.0` 是余量：段宽本来就是按同一个 `est_text_w` 算出来的，不给余量会出现
    // 「刚好相等 → 判成放不下 → 截成空串」的浮点毛刺（名称「66」就这样消失过）。
    let avail = (2.0 * half - CHIP_PAD_L - CHIP_ICON - CHIP_GAP - CHIP_PAD_R + 1.0).max(10.0);
    let label = fit_chip_label(name, avail, cjk, lat);

    // 左端图标（各自贴弧、按该处切线转；与内环同一套纹理管线）
    let (ix, iy, iang) = place(-half + CHIP_PAD_L + CHIP_ICON * 0.5, 0.0);
    if let Some(tex) = icon {
        draw_bgra_icon_rot(buf, ix, iy, CHIP_ICON * (1.0 + 0.06 * hot),
                           tex, iang.to_radians(), alpha);
    } else if let Some(ch) = name.chars().next() {
        // 拿不到图标（少数 shell: 命名空间）→ **中性**小底板 + 首字母。
        // 全界面唯一允许有颜色的东西是真实图标，所以这里不放彩色。
        let h2 = CHIP_ICON * 0.5;
        rot_rrect(buf, ix, iy, h2, h2, 4.0 * K, iang.to_radians(),
                  mixc(pal.paper, pal.ink, 0.14), alpha);
        let cht = ch.to_uppercase().to_string();
        draw_text_rot(st, buf, ix, iy, &cht, 13, iang.to_radians(),
                      pal.ink.0, pal.ink.1, pal.ink.2, a8);
    }

    // ── 加载中：把名字换成**三个跳动的点** ──
    // 「在等」必须看得出来在动，否则用户只会觉得「点了没反应 / 卡住」。
    if loading {
        let n = 3usize;
        let step = 7.0 * K * KC;
        // 相位在 0..(2n-2) 之间来回（0,1,2,1,0,1,…）= 来回跳，比单向循环更像「在处理」
        let span = (n * 2 - 2).max(1);
        let ph = ((now_s() * 3.2) as usize) % span;
        let hi = if ph < n { ph } else { span - ph };
        // 把整组点**居中**在文字可用区里（名字变短了，别挤在图标旁边）
        let group = step * (n as f32 - 1.0);
        let base = -half + CHIP_PAD_L + CHIP_ICON + CHIP_GAP + (avail - group).max(0.0) * 0.5;
        for k in 0..n {
            let (dx, dy, _ang) = place(base + k as f32 * step, 0.0);
            let on = k == hi;
            let col = if on { fg } else { mixc(pal.paper, fg, 0.42) };
            let r = if on { 3.4 * K } else { 2.1 * K };
            flat_circle(buf, dx, dy, r, col, 0.0, col, 0.0, 0.0, 0.0, 0.0, alpha);
        }
        return; // 加载中不画名字、也不画光标
    }

    if !label.is_empty() {
        let textw = est_text_w(&label, cjk, lat);
        let (tx, ty, tang) = place(-half + CHIP_PAD_L + CHIP_ICON + CHIP_GAP + textw * 0.5, 0.0);
        // ⚠️⚠️ `draw_text_rot` 收的是**弧度**（内部直接 `sin_cos()`）。传「度」=
        // 旋转矩阵整个是错的 → 文字被采样成一团乱码（2026-10-08 踩到，
        // 现场看起来就是「文字坏了」；遮罩本身是好的，落盘验证过）。
        draw_text_rot(st, buf, tx, ty, &label, RING_NAME_PX, tang.to_radians(),
                      fg.0, fg.1, fg.2, a8);
    }

    // 编辑光标：0.5s 周期方波闪烁，画在文本末尾。
    // ⚠️⚠️ 相位必须按**当前时间**算。原来用的是 `st.edit_caret` —— 那是「进入编辑的时刻」，
    // 是个常量，`(常量 * 2).fract()` 只会得到一个固定值 → 光标要么常亮要么常灭，
    // **永远不会变**。所以即使帧一直在重绘，看起来也完全不闪
    // （用户 2026-10-09 报「不知道这是编辑模式了」）。
    if caret {
        let blink = (now_s() * 2.0).fract() < 0.5;
        if blink {
            let textw = est_text_w(&label, cjk, lat);
            let cw = est_text_w("0", cjk, lat).max(4.0);
            let (bx, by, tang) = place(
                -half + CHIP_PAD_L + CHIP_ICON + CHIP_GAP + textw + 1.5 + cw * 0.5,
                0.0,
            );
            // 高度按**文字实际高度**取（原来是 7K ≈ 9.2px，比文字 11.5px 矮一截，
            // 看着像个小点）
            let chh = (RING_NAME_PX as f32 * K * KC) * 0.5;
            rot_rrect(buf, bx, by, cw * 0.5, chh, 1.0, tang.to_radians(), fg, alpha);
        }
    }
}



// ───────────────────── 拖出移除 ─────────────────────

/// 取消按钮：出现时**头像缩 12% 让位**（`cancel_a` 驱动，与按钮同步淡入），
/// 按钮贴着缩小后的球底悬挂 —— 球底 38.3 → 按钮占 41.3..59.3 → 图标砖内沿
/// ≥66.9，下方净空 ~7.6px（连图标投影都够不到），任何 n 都不遮挡。
/// 旧版固定位置两度压到图标（`HUB_R+24` 高30 → `HUB_R+11` 高18 都嫌挤），
/// 用户拍板：与其硬挤，不如让头像缩一下。
const RM_BTN_W: f32 = 62.0;
const RM_BTN_H: f32 = 18.0;
/// 按钮中心相对盘心的偏移 = 缩小的球底(HUB_R*0.88) + 12（cancel_a=1 时）
const RM_BTN_TUCK: f32 = 12.0;
/// 头像让位缩放幅度
const RM_BALL_SHRINK: f32 = 0.12;
const RM_BTN_R: f32 = RM_BTN_H * 0.5;

/// 取消按钮中心（画布坐标）。btn_a = cancel_a（0→1 淡入进度），
/// 按钮边淡入边跟着缩小的球上移
fn rm_btn_center(btn_a: f32) -> (f32, f32) {
    let a = btn_a.clamp(0.0, 1.0);
    (CC, CC + HUB_R * (1.0 - RM_BALL_SHRINK * a) + RM_BTN_TUCK * a)
}

/// 点是否落在取消按钮热区（比视觉尺寸大一圈，指针好点中）
fn hit_rm_btn(st: &State, x: f32, y: f32) -> bool {
    if st.cancel_a < 0.5 {
        return false;
    }
    let (cx, cy) = rm_btn_center(st.cancel_a);
    let hw = RM_BTN_W * 0.5 + 4.0;
    let hh = RM_BTN_H * 0.5 + 4.0;
    (x - cx).abs() <= hw && (y - cy).abs() <= hh
}

/// 屏幕坐标是否落在「请求移除」区 —— **胶囊环外沿再往外 `REMOVE_PULL`**。
///
/// ⚠️ 别退回用 `in_disc`（盘面边缘）：图标环外沿到盘边只有 28px，
/// 正常换位往外一甩就误判成移除（用户 2026-10-14 报）。
/// ⚠️ 上限压在画布内：大 n 时胶囊环本身就贴到画布边了，不能要求「拖出画布」。
/// 把「被拖物当前所在的画布坐标」换成**屏幕坐标**。
///
/// ⚠️⚠️ `st.drag_x/y` 存的是**画布坐标**（赋值处就是 `x - st.win_x`），
/// 而 `in_disc` / `in_window` / `remove_zone` 收的是**屏幕坐标**（内部自己减 `win_x`）。
/// 混用会整整差一个窗口偏移（几百 px）→ 判定完全错（第一次写 `want_fade` 就栽在这）。
fn drag_screen(st: &State) -> (i32, i32) {
    (st.drag_x as i32 + st.win_x, st.drag_y as i32 + st.win_y)
}

/// 「拖出移除」阈值的**半径** = 胶囊环外沿 + `REMOVE_PULL`（再压在画布内）。
/// 抽出来是为了让「判定」和「画整圈虚线」用的是同一个数 —— 否则画出来的圈
/// 和真正的移除边界对不上（看着还在圈里、松手却被删了）。
fn remove_r(n: usize) -> f32 {
    (ring_radii(n).1 + REMOVE_PULL).min(CANVAS as f32 * 0.5 - 4.0)
}

fn remove_zone(st: &State, sx: i32, sy: i32) -> bool {
    let n = st.apps.len();
    if n == 0 {
        return false;
    }
    let lim = remove_r(n);
    let dx = (sx - st.win_x) as f32 - CC;
    let dy = (sy - st.win_y) as f32 - CC;
    dx * dx + dy * dy > lim * lim
}

/// 屏幕坐标是否还在盘面内
fn in_disc(st: &State, sx: i32, sy: i32) -> bool {
    let px = (sx - st.win_x) as f32;
    let py = (sy - st.win_y) as f32;
    let dx = px - CC;
    let dy = py - CC;
    dx * dx + dy * dy <= disc_r(st.apps.len()) * disc_r(st.apps.len())
}

/// 屏幕坐标是否还在菜单窗口内（外环胶囊本来就悬在盘沿之外，
/// 它的「拖出」判定用窗口边界：拖出窗口才算动真格）
fn in_window(st: &State, sx: i32, sy: i32) -> bool {
    sx >= st.win_x && sx < st.win_x + CANVAS && sy >= st.win_y && sy < st.win_y + CANVAS
}

/// 输入心跳：光标动了/点了就刷新 last_input，并唤醒发呆中的头像
fn note_input(st: &mut State, x: i32, y: i32) {
    if x != st.cur_x || y != st.cur_y {
        st.cur_x = x;
        st.cur_y = y;
        st.last_input = now_s();
        if st.mood == 3 {
            set_mood(st, 0);
        }
    }
}

/// 切换头像情绪（带过渡）
fn set_mood(st: &mut State, m: u8) {
    if st.mood == m {
        return;
    }
    st.mood_prev = st.mood;
    st.mood = m;
    st.mood_k = 0.0;
    st.mood_t = now_s();
}

/// 倒计时到点：待移除的内容**早已从盘面摘下**，这里只负责落盘。
/// 与设置页的删除同一套持久化：应用删除备份→写→重载图标；胶囊删除只写。
unsafe fn commit_expired(st: &mut State, now: f64) {
    if st.pending_rms.is_empty() {
        return;
    }
    let mut removed_any = false;
    let mut removed_app = false;
    let mut last_name = String::new();
    let mut i = 0;
    while i < st.pending_rms.len() {
        if st.pending_rms[i].deadline() > now {
            i += 1;
            continue;
        }
        let p = st.pending_rms.remove(i);
        removed_any = true;
        match p {
            PendingRm::App { entry, .. } => {
                println!("[remove] 已移除应用：{}", entry.name);
                last_name = entry.name.clone();
                removed_app = true;
            }
            PendingRm::Chip { app, entry, .. } => {
                let owner = st.apps.get(app).map(|a| a.name.as_str()).unwrap_or("?");
                println!("[remove] 已移除胶囊：{}（{}）", entry.name, owner);
                last_name = entry.name.clone();
            }
        }
    }
    // 落盘只做一次（`load_icons` 是重活，删多个应用时别重载多次）
    if removed_any {
        save_settings(st);
        if removed_app {
            load_icons(st);
        }
    }
    // ⚠️ 真删掉那一刻以前只有一行 `println!` —— 用户唯一的反馈是「东西不见了」，
    //    不确定是自己删的还是出 bug 了。这里必须明确报出来。
    if !last_name.is_empty() {
        say(st, &format!("已移除：{}", last_name), "", 1.6);
    }
}

/// 取消一次待移除：把摘下的内容原位放回（倒计时到点走 commit_remove，不走这里）
/// 取消一次待移除：只作用于**最后加入**的那个（用户 2026-10-09 明确要求），
/// 把它原位放回。其余待删的继续按各自的倒计时走。
fn cancel_remove(st: &mut State) {
    if let Some(p) = st.pending_rms.pop() {
        match p {
            PendingRm::App { pos, entry, .. } => {
                let pos = pos.min(st.apps.len());
                st.apps.insert(pos, entry);
                println!("[remove] 已取消，放回：{}", st.apps[pos].name);
                let nm = st.apps[pos].name.clone();
                say(st, &format!("已放回：{}", nm), "", 1.4);
            }
            PendingRm::Chip { app, pos, entry, .. } => {
                if let Some(a) = st.apps.get_mut(app) {
                    let pos = pos.min(a.items.len());
                    a.items.insert(pos, entry);
                    println!("[remove] 已取消，放回胶囊：{}", a.items[pos].name);
                }
            }
        }
    }
    // 还有别的待删 → 保持「生气」（等着继续确认）
    set_mood(st, if st.pending_rms.is_empty() { 0 } else { 2 });
}

/// 菜单收起（关掉轮盘）= 撤销窗口结束 → 待移除的内容**立即落盘**，不放回。
///
/// ⚠️ 旧行为是「收起 = 放回」：拖走应用后必须干等约 5 秒倒计时走完才算真删掉，
/// 期间一关轮盘删除就作废、应用又冒出来（用户 2026-10-12 反馈：不合理）。
/// 撤销只认**显式点「取消」按钮**这一个动作；关掉轮盘 = 确认删除。
unsafe fn commit_all_pending(st: &mut State) {
    if st.pending_rms.is_empty() {
        return;
    }
    // 传一个「未来」的 now → 所有待删都视为已到期，一次性落盘
    commit_expired(st, f64::MAX);
}

/// 拖动中：抓的是「按下那一刻」命中的目标（应用图标优先，其次外环胶囊），
/// 不是当前光标下的。盘外 → 紧张；抓着东西在盘内晃 → 疑惑。
fn drag_step(st: &mut State, x: i32, y: i32) {
    note_input(st, x, y);
    if st.drag_app.is_none() && st.drag_chip.is_none() {
        let px = (LB_DOWN_X.load(Ordering::Relaxed) as i32 - st.win_x) as f32;
        let py = (LB_DOWN_Y.load(Ordering::Relaxed) as i32 - st.win_y) as f32;
        st.drag_app = nearest_app(st, px, py, 0.0);
        if st.drag_app.is_none() {
            // 胶囊只可能属于当前亮着的应用（别的应用的 chip 根本没画出来）
            if let Some(ai) = st.active_app {
                if ai < st.apps.len() {
                    st.drag_chip = hit_chip_at(st, ai, px, py).map(|j| (ai, j));
                }
            }
        }
    }
    st.drag_x = (x - st.win_x) as f32;
    st.drag_y = (y - st.win_y) as f32;
    // ── 插入目标：光标落在**另一个**图标/胶囊上就记下来（松手时插到该槽位）──
    // 每帧重算，所以离开就自动清掉。
    st.drag_over_app = None;
    st.drag_over_chip = None;
    if let Some(ai) = st.drag_app {
        // ⚠️ 落点用**角度吸附**（`drop_slot_app`），不是「贴住某个图标」（`nearest_app`）：
        // 用户要把图标拖到**两个图标中间**插进去（2026-10-09），贴图标判定会落空。
        if let Some(t) = drop_slot_app(st, st.drag_x, st.drag_y) {
            if t != ai {
                st.drag_over_app = Some(t);
            }
        }
    } else if let Some((ai, j)) = st.drag_chip {
        // 同理：段间的缝里也算落点，归到最近的一段。
        if let Some(t) = drop_slot_chip(st, ai, st.drag_x, st.drag_y) {
            if t != j {
                st.drag_over_chip = Some(t);
            }
        }
    }
    let grabbed = st.drag_app.is_some() || st.drag_chip.is_some();
    // ⚠️ 用 `remove_zone`（胶囊环外 20px），不是 `in_disc`（盘面边缘）——
    //    见 `remove_zone` 的注释：后者会和正常换位抢判定。
    let outside = remove_zone(st, x, y);
    set_mood(st, if !outside {
        if grabbed { 4 } else { 0 }
    } else {
        1
    });
    // 拖到盘外 → 预告「松手就移除」。
    // 旧行为只有表情变紧张 —— 用户不知道松手会发生什么，只能靠试。
    if grabbed && outside {
        let nm = st
            .drag_app
            .and_then(|ai| st.apps.get(ai).map(|a| a.name.clone()))
            .or_else(|| {
                st.drag_chip.and_then(|(ai, j)| {
                    st.apps.get(ai).and_then(|a| a.items.get(j)).map(|i| i.name.clone())
                })
            });
        if let Some(n) = nm {
            say(st, &format!("松手就移除：{}", n), "", 0.0);
        }
    } else if st.bub_until.is_infinite() {
        hush(st); // 拖回盘内 = 不删了，预告收掉
    }
}

/// 拖动松手：
///   应用图标 —— 盘外 = 请求移除：**立即摘下**（盘面上马上消失）+ 生气 +
///              取消按钮 + 5 秒倒计时；到点落盘，取消/收起菜单都放回。
///   外环胶囊 —— 拖出**窗口** = 同上（胶囊本来就悬在盘沿外，所以边界取窗口）。
///   盘内/窗内松手 = 放弃，图标回槽位。
fn drag_end(st: &mut State, x: i32, y: i32) {
    note_input(st, x, y);
    if let Some(ai) = st.drag_app {
        // ⚠️ 这里**不再检查**有没有待确认的移除 —— 待删是可以排队的，
        // 拖走一个之后要能立刻拖下一个（用户 2026-10-09 要求）。
        if ai < st.apps.len() && remove_zone(st, x, y) {
            let entry = st.apps.remove(ai);
            // 槽位收缩：被摘槽位之后的索引整体左移，别让选中态指错应用
            match st.active_app {
                Some(a) if a > ai => st.active_app = Some(a - 1),
                Some(a) if a == ai => st.active_app = None,
                _ => {}
            }
            let name = entry.name.clone();
            st.pending_rms.push(PendingRm::App {
                pos: ai,
                entry,
                deadline: now_s() + REMOVE_GRACE,
            });
            set_mood(st, 2);
            println!("[remove] {} 秒后移除：{}（点「取消」放回）", REMOVE_GRACE as u32, name);
            say(st, &format!("已摘下：{}", name), "点「取消」可以放回", REMOVE_GRACE);
        } else if let Some(t) = st.drag_over_app.filter(|&t| t != ai && t < st.apps.len()) {
            // ── 盘内落在**另一个应用图标**上 = 插入排序（不是两两交换）──
            // 拖起的那一个从原槽位抽出，插入到目标槽位；两者之间的整体顺移一格。
            // `remove(ai)` + `insert(t, ..)` 恰好等于「拖起项占用目标槽位、其余顺移」：
            //   [A,B,C] 拖 A 到 C 槽位 → [B,C,A]（A 落在 index 2，C 左移一格）。
            // （旧实现是 swap → [C,B,A]，中间的不动，用户 2026-10-09 明确要求改成插入。）
            let entry = st.apps.remove(ai);
            let name = entry.name.clone();
            st.apps.insert(t, entry);
            // ⚠️⚠️ `app_slot_s` 是**按数组下标**存的「当前显示极角」，必须跟着
            //     `remove + insert` **同样重排一次**。忘了重排 → 下标与角度错位，
            //    松手后每个槽位都要从「别人的角度」滑到「自己的角度」= 图标集体多动一下
            //    （用户 2026-10-14 报：松手前它们明明已经就位了）。
            let sa = st.app_slot_s[ai];
            if ai < t {
                for k in ai..t {
                    st.app_slot_s[k] = st.app_slot_s[k + 1];
                }
            } else {
                let mut k = ai;
                while k > t {
                    st.app_slot_s[k] = st.app_slot_s[k - 1];
                    k -= 1;
                }
            }
            st.app_slot_s[t] = sa;
            // 松手后**只有被拖的那个需要动**：从光标处滑进槽位。
            // 其余图标在拖动期间已经滑到位（预览排序），这里必须原地不动。
            st.land_app = t;
            st.land_t0 = now_s();
            st.land_from = (st.drag_x, st.drag_y);
            // 选中态跟着**被拖动的那一个**走（应用身份不变，只是换了槽位）：
            //   拖动项原在 ai、现落 t；中间那批整体向 ai 方向平移一格。
            match st.active_app {
                Some(a) if a == ai => st.active_app = Some(t),
                Some(a) if ai < a && a <= t => st.active_app = Some(a - 1),
                Some(a) if t <= a && a < ai => st.active_app = Some(a + 1),
                _ => {}
            }
            unsafe { save_settings(st) };
            println!("[reorder] 应用插入：{} → 槽位 {}", name, t);
            set_mood(st, if !st.pending_rms.is_empty() { 2 } else { 0 });
        } else {
            // 盘内松手 = 放弃：图标从光标处滑回**原来的**槽位（不然是硬跳回去）
            hush(st);
            if ai < st.apps.len() {
                st.land_app = ai;
                st.land_t0 = now_s();
                st.land_from = (st.drag_x, st.drag_y);
            }
            set_mood(st, if !st.pending_rms.is_empty() { 2 } else { 0 });
        }
    } else if let Some((ai, j)) = st.drag_chip {
        if ai < st.apps.len() && !in_window(st, x, y) {
            let entry = st.apps[ai].items.remove(j);
            match st.hover_chip {
                Some(h) if h > j => st.hover_chip = Some(h - 1),
                Some(h) if h == j => st.hover_chip = None,
                _ => {}
            }
            let name = entry.name.clone();
            st.pending_rms.push(PendingRm::Chip {
                app: ai,
                pos: j,
                entry,
                deadline: now_s() + REMOVE_GRACE,
            });
            set_mood(st, 2);
            println!("[remove] {} 秒后移除胶囊：{}（点「取消」放回）", REMOVE_GRACE as u32, name);
            say(st, &format!("已摘下：{}", name), "点「取消」可以放回", REMOVE_GRACE);
        } else if let Some(t) = st.drag_over_chip
            .filter(|&t| t != j && t < st.apps.get(ai).map(|a| a.items.len()).unwrap_or(0))
        {
            // ── 外环内**插入排序**（只重排同一个应用的条目，不是两两交换）──
            let entry = st.apps[ai].items.remove(j);
            let name = entry.name.clone();
            st.apps[ai].items.insert(t, entry);
            // ⚠️⚠️ `chip_seg_a` 是**按条目下标**存的「当前段角」，必须跟着
            //     `remove + insert` **同样重排一次** —— 忘了就会下标/角度错位，
            //     松手后每一段都要从「别人的角度」滑到「自己的角度」= 胶囊集体多动一下
            //     （内环 `app_slot_s` 同款坑，用户 2026-10-14 报过）。
            if j < MAX_ITEMS && t < MAX_ITEMS {
                let ca = st.chip_seg_a[j];
                if j < t {
                    for k in j..t {
                        st.chip_seg_a[k] = st.chip_seg_a[k + 1];
                    }
                } else {
                    let mut k = j;
                    while k > t {
                        st.chip_seg_a[k] = st.chip_seg_a[k - 1];
                        k -= 1;
                    }
                }
                st.chip_seg_a[t] = ca;
            }
            // 悬停跟随被拖动的那个条目（身份不变，只换槽位）
            match st.hover_chip {
                Some(h) if h == j => st.hover_chip = Some(t),
                Some(h) if j < h && h <= t => st.hover_chip = Some(h - 1),
                Some(h) if t <= h && h < j => st.hover_chip = Some(h + 1),
                _ => {}
            }
            unsafe { save_settings(st) };
            println!("[reorder] 条目插入：{} → 槽位 {}", name, t);
            set_mood(st, if !st.pending_rms.is_empty() { 2 } else { 0 });
        } else {
            set_mood(st, if !st.pending_rms.is_empty() { 2 } else { 0 });
        }
    }
    // 空手拖动松手（按在空白处划了一圈）：回常态；有待确认移除则回「生气」
    if st.drag_app.is_none() && st.drag_chip.is_none() {
        set_mood(st, if !st.pending_rms.is_empty() { 2 } else { 0 });
    }
    st.drag_app = None;
    st.drag_chip = None;
    st.drag_over_app = None;
    st.drag_over_chip = None;
}

/// 三个不同频率正弦叠加成一个伪随机抖动。amp 的单位是**球半径的倍数**。
fn jitter(t_ms: f64, speed: f64, amp: f32) -> f32 {
    let s = (t_ms / 1000.0 * speed) as f32;
    ((s * 3.1).sin() + (s * 5.7 + 0.85).sin() + (s * 9.3 + 1.52).sin()) / 3.0 * amp
}

/// 段半宽（命中判定与绘制共用同一公式）。
/// ⚠️ 名称宽度封顶 `RING_NAME_MAX_W`；`draw_ring_content` 里截断用的也是它。
fn chip_half_w(name: &str) -> f32 {
    (CHIP_PAD_L + CHIP_ICON + CHIP_GAP
        + est_text_w(name, 13.0 * K * KC, 7.0 * K * KC).min(RING_NAME_MAX_W)
        + CHIP_PAD_R)
        / 2.0
}

/// 把角度归一到 (-90°, 90°] —— 只为「文字永远从左向右读」。
/// 下半圈翻转 180°，否则段里的字会是倒的。
fn norm_rot(a: f32) -> f32 {
    let aa = ((a % 360.0) + 360.0) % 360.0;
    if aa <= 90.0 || aa >= 270.0 {
        if aa > 180.0 {
            aa - 360.0
        } else {
            aa
        }
    } else {
        aa - 180.0
    }
}

/// 外环段布局：返回 (**每段的角度范围 (a1, a2)**, 每段的旋转角(度), 环半径)。
///
/// **永远是贴身扇面**：以所属内环应用的方位角 `a0` 为中心，各段按**默认间距**
/// (`CHIP_MIN_GAP`) 向两侧铺开，每段角跨 ∝ 该条目名字宽度。
/// 用户 2026-10-17 定稿：「外环不应该是均匀分布的，而是以默认间距在对应内环附近分布」——
/// 旧实现在跨度 > 240° 时会退回「整圈均布 + 首段锚定 a0」，结果一堆胶囊散布一整圈、
/// 跟它所属的图标完全脱钩（截图里就是这样）。现在只有两级退让：
///   ① 默认间距铺开，跨度 ≤ `CHIP_FAN_MAX`；② 铺不下就**先压缝**（贴身优先，缝最小 `CHIP_GAP_MIN`）；
///   ③ 压到最小缝还铺不下（条目多 + 名字长，物理上绕不完）→ 才退回整圈均布。
///
/// ⚠️ 半径**只能**用环带的中位半径 —— 段就画在那条带上；用别的半径算角宽会让段
/// 与内容对不上（宽了相邻重叠、窄了留大缝）。
/// ⚠️ 绘制与命中判定**只能**调这一个函数。
/// 外环某一格显示的名字（改名中 → 用输入缓冲）。
/// 抽出来是因为**预览排序**也要按名字重算段宽，得和正式布局走同一条路。
fn chip_name_of(st: &State, ai: usize, j: usize) -> String {
    match st.editing {
        Some((eai, ej)) if eai == ai && ej == j => st.edit_buf.clone(),
        _ => st
            .apps
            .get(ai)
            .and_then(|a| a.items.get(j))
            .map(|i| i.name.clone())
            .unwrap_or_default(),
    }
}

/// 段布局核心：给定**已排好序的名字**，算每段的角范围。
/// 抽出来是为了让「拖动让位预览」复用同一套几何（角跨 ∝ 名字宽度、段间留缝）。
fn chip_segs_of(st: &State, ai: usize, names: &[String]) -> Vec<(f32, f32)> {
    let n = st.apps.len();
    let m = names.len();
    if n == 0 || m == 0 {
        return Vec::new();
    }
    let halfs: Vec<f32> = names.iter().map(|s| chip_half_w(s)).collect();
    let sum: f32 = halfs.iter().map(|h| h * 2.0).sum();

    let (r_in, r_out) = ring_radii(n);
    let r = (r_in + r_out) * 0.5;
    let a0 = slot_angle(slot_of(st, ai), n);
    let deg_per_px = 180.0 / (std::f32::consts::PI * r);
    let gap_deg = CHIP_MIN_GAP * deg_per_px;
    let wdeg: Vec<f32> = halfs.iter().map(|h| 2.0 * h * deg_per_px).collect();
    let width_deg = sum * deg_per_px;
    let span0 = width_deg + gap_deg * (m.saturating_sub(1)) as f32;

    let mut segs: Vec<(f32, f32)> = Vec::with_capacity(m);
    // ① 默认间距就铺得下 → 直接用默认间距；② 铺不下 → **先压缝**（贴身优先，缝别低于下限）
    let gap_use = if span0 <= CHIP_FAN_MAX {
        gap_deg
    } else if m > 1 {
        let fit = (CHIP_FAN_MAX - width_deg) / (m - 1) as f32;
        if fit >= CHIP_GAP_MIN * deg_per_px { fit } else { -1.0 }
    } else {
        -1.0
    };
    if gap_use >= 0.0 {
        // 贴身扇面：整体以 a0 为中心左右对称
        let span = width_deg + gap_use * (m.saturating_sub(1)) as f32;
        let mut acc = -span * 0.5;
        for j in 0..m {
            segs.push((a0 + acc, a0 + acc + wdeg[j]));
            acc += wdeg[j] + gap_use;
        }
    } else {
        // ③ 压到最小缝还铺不下（条目多 + 名字长，一整圈都绕不完）→ 整圈均布兜底
        //    间隙数 = m（闭合环首尾之间也要算一个），第 1 段锚定在 a0
        let gap = ((std::f32::consts::TAU * r - sum) / m as f32).max(CHIP_MIN_GAP);
        let gd = gap * deg_per_px;
        let mut acc = 0.0f32;
        for j in 0..m {
            segs.push((a0 + acc, a0 + acc + wdeg[j]));
            acc += wdeg[j] + gd;
        }
    }
    segs
}

/// 外环**预览布局**：条目按「预览排序」重排后再排段 —— 于是被拖那一段的
/// 原槽位空出来（=缝），其它胶囊整体顺移让位。
///
/// 返回**按条目下标**索引的段（`out[j]` = 条目 j 该画在哪一格）。
/// 与内环的 `drag_preview_slot` 是同一套语义，只是这里的「槽宽」不等宽。
fn chip_preview_segs(st: &State, ai: usize, dj: usize, ins: usize) -> Vec<(f32, f32)> {
    let m = st.apps.get(ai).map(|a| a.items.len()).unwrap_or(0);
    if m == 0 || dj >= m {
        return Vec::new();
    }
    // order[slot] = 条目下标
    let mut order: Vec<usize> = (0..m).filter(|&j| j != dj).collect();
    order.insert(ins.min(order.len()), dj);
    let names: Vec<String> = order.iter().map(|&j| chip_name_of(st, ai, j)).collect();
    let segs = chip_segs_of(st, ai, &names);
    let mut out = vec![(0.0f32, 0.0f32); m];
    for (slot, &j) in order.iter().enumerate() {
        if slot < segs.len() {
            out[j] = segs[slot];
        }
    }
    out
}

/// 拖进来的东西**认得出宿主**（拖 explorer 窗口 / 浏览器标签）→ 外环末尾要新增一条。
/// 这里给出那条的**名字**：必须与落盘时 `capture_window` 用的一致
/// （都是 `clamp_name(window_title)`），否则预览名和真加进去的名字对不上。
///
/// 认不出宿主（新应用）那条路走的是内环 `drop_ghost` 幽灵磁贴 —— 两者是同一个语义的
/// 内外两环版本，**缺了外环这半边，用户就完全不知道松手会不会成功**（2026-10-16 用户报）。
fn drop_ghost_chip_name(st: &State, ai: usize, m: usize) -> Option<String> {
    if st.drop_suggest != Some(ai) || m >= MAX_ITEMS {
        return None;
    }
    let w = st.window_capture.as_ref()?;
    let n = clamp_name(&w.title);
    if n.is_empty() {
        Some(clamp_name(&w.exe))
    } else {
        Some(n)
    }
}

/// 外环排版用的名字列表（含幽灵名 —— 它**参与排版**，所以已有胶囊会整体让位）。
fn chip_layout_names(st: &State, ai: usize) -> (Vec<String>, Option<String>) {
    let m = st.apps.get(ai).map(|a| a.items.len()).unwrap_or(0);
    let mut names: Vec<String> = (0..m).map(|j| chip_name_of(st, ai, j)).collect();
    let ghost = drop_ghost_chip_name(st, ai, m);
    if let Some(g) = &ghost {
        names.push(g.clone());
    }
    (names, ghost)
}

/// 幽灵段（「即将新增」那一格）的角范围。没有幽灵就是 `None`。
fn chip_ghost_seg(st: &State, ai: usize) -> Option<(f32, f32)> {
    let m = st.apps.get(ai).map(|a| a.items.len()).unwrap_or(0);
    let (names, ghost) = chip_layout_names(st, ai);
    ghost?;
    chip_segs_of(st, ai, &names).get(m).copied()
}

fn chip_layout(st: &State, ai: usize) -> (Vec<(f32, f32)>, Vec<f32>, Vec<f32>) {
    let n = st.apps.len();
    if n == 0 || ai >= n {
        return (Vec::new(), Vec::new(), Vec::new());
    }
    let m = st.apps[ai].items.len();
    if m == 0 {
        return (Vec::new(), Vec::new(), Vec::new());
    }
    // ⚠️ 名字列表里**带幽灵**（若有）→ 真实胶囊自动让位一格。
    //    这就是「拖窗口进来时外环腾出位置」的全部实现，命中测试/平滑段角共用同一份布局。
    let (names, _ghost) = chip_layout_names(st, ai);
    let segs: Vec<(f32, f32)> = chip_segs_of(st, ai, &names).into_iter().take(m).collect();
    let rots: Vec<f32> = segs.iter().map(|(a1, a2)| norm_rot((a1 + a2) * 0.5)).collect();
    let (r_in, r_out) = ring_radii(n);
    (segs, rots, vec![(r_in + r_out) * 0.5; m])
}

// ───────────────────── 窗口状态 ─────────────────────

unsafe fn state_of(hwnd: HWND) -> &'static mut State {
    &mut *(GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut State)
}

unsafe fn post(m: u32, w: usize, l: usize) {
    let h = MAIN_HWND.load(Ordering::Relaxed);
    if h != 0 {
        // PostMessage 失败 = 窗口已销毁，或 UI 线程消息队列塞满。
        // **队列塞满正是「UI 线程被卡住」的直接证据** —— UI 线程卡住时
        // 所有 post 过来的消息都不处理，菜单就再也不响应任何操作。
        //
        // 节流：用 exchange 而不是「失败才置位」—— 每次失败都记会瞬间刷爆日志
        // （队列满时每毫秒都在失败）。只在「从成功切到失败」时记第一条。
        if PostMessageW(HWND(h as *mut c_void), m, WPARAM(w), LPARAM(l as isize)).is_err() {
            if !POST_DROP_LAST.swap(true, Ordering::Relaxed) {
                diag_push(DK_POST_DROP, m);
            }
        } else {
            POST_DROP_LAST.store(false, Ordering::Relaxed);
        }
    }
}

// ───────────────────── OLE 放置目标 ─────────────────────

#[windows_core::implement(IDropTarget)]
struct RingDropTarget;

impl IDropTarget_Impl for RingDropTarget_Impl {
    fn DragEnter(
        &self,
        pdataobj: Option<&IDataObject>,
        _grfkeystate: MODIFIERKEYS_FLAGS,
        pt: &POINTL,
        pdweffect: *mut DROPEFFECT,
    ) -> windows_core::Result<()> {
        DRAG_ACTIVE.store(true, Ordering::SeqCst);
        DRAG_SEEN.store(true, Ordering::SeqCst);
        DROP_ARM_AT.store(unsafe { GetTickCount() }, Ordering::SeqCst);
        drag_log("DragEnter");
        // 预取载荷：拖拽**过程中**就要能识别宿主、给「高亮 / 幽灵」提示。
        // 只在 Drop 时才取的话，全程都没有反馈，用户不知道能丢到哪。
        if let Some(obj) = pdataobj {
            let items = unsafe { extract_drop_items(obj) };
            if let Ok(mut q) = DROP_PAYLOAD.lock() {
                *q = items;
            }
        }
        DROP_PT.store(pack_pt(pt.x, pt.y), Ordering::SeqCst);
        unsafe {
            *pdweffect = DROPEFFECT(1 | 4); // COPY | LINK
        }
        unsafe { post(WM_APP_DRAGEVENT, 0, 0); }
        Ok(())
    }

    fn DragOver(
        &self,
        _grfkeystate: MODIFIERKEYS_FLAGS,
        pt: &POINTL,
        pdweffect: *mut DROPEFFECT,
    ) -> windows_core::Result<()> {
        let packed = pack_pt(pt.x, pt.y);
        let prev = DROP_PT.load(Ordering::SeqCst);
        DROP_PT.store(packed, Ordering::SeqCst);
        // 无条件刷新（不受下面 3px 节流影响）：系统在光标不动时仍会周期性回调 DragOver，
        // 用它保持「拖放会话仍活着」的时间戳新鲜，超时兜底才不会误判。
        DROP_ARM_AT.store(unsafe { GetTickCount() }, Ordering::SeqCst);
        unsafe {
            *pdweffect = DROPEFFECT(1 | 4);
        }
        // 节流：位移超过 3px 才通知（否则拖动期间消息风暴）
        let (px, py) = unpack_pt(packed);
        let (qx, qy) = unpack_pt(prev);
        if (px - qx).abs() > 3 || (py - qy).abs() > 3 {
            unsafe { post(WM_APP_DRAGEVENT, 1, 0); }
        }
        Ok(())
    }

    fn DragLeave(&self) -> windows_core::Result<()> {
        DRAG_ACTIVE.store(false, Ordering::SeqCst);
        DROP_ARM_AT.store(unsafe { GetTickCount() }, Ordering::SeqCst);
        drag_log("DragLeave");
        unsafe { post(WM_APP_DRAGEVENT, 2, 0); }
        Ok(())
    }

    fn Drop(
        &self,
        pdataobj: Option<&IDataObject>,
        _grfkeystate: MODIFIERKEYS_FLAGS,
        pt: &POINTL,
        pdweffect: *mut DROPEFFECT,
    ) -> windows_core::Result<()> {
        DROP_PT.store(pack_pt(pt.x, pt.y), Ordering::SeqCst);
        drag_log("Drop");
        unsafe {
            *pdweffect = DROPEFFECT(1);
        }
        if let Some(obj) = pdataobj {
            let items = unsafe { extract_drop_items(obj) };
            if let Ok(mut q) = DROP_PAYLOAD.lock() {
                *q = items;
            }
        }
        unsafe { post(WM_APP_DRAGEVENT, 3, 0); }
        Ok(())
    }
}

/// 从 IDataObject 提取可入环的条目：文件/文件夹/lnk（CF_HDROP）优先，
/// 其次 URL（UniformResourceLocatorW），最后纯文本（像 URL 才收）。
unsafe fn extract_drop_items(obj: &IDataObject) -> Vec<DropItem> {
    let mut out: Vec<DropItem> = Vec::new();
    let fmt_hdrop = FORMATETC {
        cfFormat: 15, // CF_HDROP
        dwAspect: DVASPECT_CONTENT.0 as u32,
        lindex: -1,
        tymed: TYMED_HGLOBAL.0 as u32,
        ptd: ptr::null_mut(),
    };
    let cf_url = RegisterClipboardFormatW(PCWSTR(wide("UniformResourceLocatorW").as_ptr()));
    let fmt_url = FORMATETC {
        cfFormat: cf_url as u16,
        dwAspect: DVASPECT_CONTENT.0 as u32,
        lindex: -1,
        tymed: TYMED_HGLOBAL.0 as u32,
        ptd: ptr::null_mut(),
    };
    let fmt_text = FORMATETC {
        cfFormat: 13, // CF_UNICODETEXT
        dwAspect: DVASPECT_CONTENT.0 as u32,
        lindex: -1,
        tymed: TYMED_HGLOBAL.0 as u32,
        ptd: ptr::null_mut(),
    };

    if obj.GetData(&fmt_hdrop).is_ok() {
        let mut stg = obj.GetData(&fmt_hdrop).unwrap();
        let hg = stg.u.hGlobal;
        if !hg.0.is_null() {
            let n = DragQueryFileW(HDROP(hg.0), u32::MAX, None);
            for i in 0..n {
                let mut buf = [0u16; 520];
                let len = DragQueryFileW(HDROP(hg.0), i, Some(&mut buf)) as usize;
                let path = String::from_utf16_lossy(&buf[..len]);
                if path.is_empty() {
                    continue;
                }
                let is_dir = std::path::Path::new(&path).is_dir();
                let stem = std::path::Path::new(&path)
                    .file_stem()
                    .or_else(|| std::path::Path::new(&path).file_name())
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_else(|| path.clone());
                out.push(DropItem {
                    name: clamp_name(&stem),
                    target: path,
                    is_url: false,
                    is_dir,
                });
            }
        }
        ReleaseStgMedium(&mut stg);
        if !out.is_empty() {
            return out;
        }
    }

    if obj.GetData(&fmt_url).is_ok() {
        let mut stg = obj.GetData(&fmt_url).unwrap();
        let hg = stg.u.hGlobal;
        if !hg.0.is_null() {
            let p = GlobalLock(HGLOBAL(hg.0));
            if !p.is_null() {
                let mut len = 0usize;
                let ws = p as *const u16;
                while *ws.add(len) != 0 && len < 2048 {
                    len += 1;
                }
                let url = String::from_utf16_lossy(std::slice::from_raw_parts(ws, len));
                let _ = GlobalUnlock(HGLOBAL(hg.0));
                let url = url.trim().to_string();
                if !url.is_empty() {
                    let name = url_host_name(&url);
                    out.push(DropItem { name, target: url, is_url: true, is_dir: false });
                }
            }
        }
        ReleaseStgMedium(&mut stg);
        if !out.is_empty() {
            return out;
        }
    }

    if obj.GetData(&fmt_text).is_ok() {
        let mut stg = obj.GetData(&fmt_text).unwrap();
        let hg = stg.u.hGlobal;
        if !hg.0.is_null() {
            let p = GlobalLock(HGLOBAL(hg.0));
            if !p.is_null() {
                let mut len = 0usize;
                let ws = p as *const u16;
                while *ws.add(len) != 0 && len < 2048 {
                    len += 1;
                }
                let text = String::from_utf16_lossy(std::slice::from_raw_parts(ws, len));
                let _ = GlobalUnlock(HGLOBAL(hg.0));
                let text = text.trim().to_string();
                // 只有看起来像链接的纯文本才收（拖动网页文字不该误入环）
                if text.len() < 2048
                    && (text.starts_with("http://")
                        || text.starts_with("https://")
                        || text.starts_with("www."))
                {
                    let name = url_host_name(&text);
                    out.push(DropItem {
                        name,
                        target: if text.starts_with("www.") {
                            format!("https://{}", text)
                        } else {
                            text
                        },
                        is_url: true,
                        is_dir: false,
                    });
                }
            }
        }
        ReleaseStgMedium(&mut stg);
    }
    out
}

fn clamp_name(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .filter(|c| *c != '=' && *c != '\n' && *c != '\r' && *c != '[' && *c != ']')
        .collect();
    cleaned.trim().chars().take(20).collect()
}

fn url_host_name(url: &str) -> String {
    let rest = url
        .split("://")
        .nth(1)
        .unwrap_or(url)
        .split('/')
        .next()
        .unwrap_or(url);
    let host = rest.trim_start_matches("www.");
    let name = if host.is_empty() { url } else { host };
    clamp_name(name)
}

// ───────────────────── 钩子 ─────────────────────

/// 低级鼠标钩子的事件结构（MSLLHOOKSTRUCT）
#[repr(C)]
struct MsLlHook {
    pt: POINT,
    mouse_data: u32,
    flags: u32,
    time: u32,
    dw_extra_info: usize,
}

unsafe extern "system" fn mouse_hook(code: i32, w: WPARAM, l: LPARAM) -> LRESULT {
    // 计时包装：本回调变慢 = 用户鼠标全系统卡顿的直接证据。
    // 只多两次 GetTickCount（纳秒级），事件本身照常处理。
    //
    // ⚠️ 静默阈值从 30s 降到 **5s**：用户报「鼠标失灵」时要能定位**精确起止时刻**。
    // 30s 太粗 —— 461 秒的空档只记一条，看不出「什么时候开始不对劲」。
    // 5s 一条，一个小时最多 720 条，看门狗有 20000 行预算，绰绰有余。
    let t0 = GetTickCount();
    let last = LAST_HOOK_TICK.swap(t0, Ordering::Relaxed);
    let gap = t0.wrapping_sub(last);
    if gap > 5_000 {
        diag_push(DK_MOUSE_IDLE, gap.min(0x00ff_ffff));
    }
    if gap > 30_000 {
        diag_push(DK_HOOK_GAP, gap.min(0x00ff_ffff));
    }
    let r = mouse_hook_impl(code, w, l);
    let dt = GetTickCount().wrapping_sub(t0);
    if dt > 30 {
        diag_push(DK_HOOK_SLOW, dt.min(0x00ff_ffff));
    }
    r
}

/// 右键长按（呼出键之一）状态机。
///
/// 返回 `Some(r)` = 这次事件由我们消费，调用方直接 `return r`；
/// 返回 `None`   = 不归我们管，调用方继续往下（等价于 `CallNextHookEx`）。
///
/// **为什么抽成独立函数**：这段逻辑的 bug 无法用「注入全局鼠标事件」来测 ——
/// 测试进程注入输入 + 我们装着全局钩子，两边会互相卡死（试过，整个测试挂死）。
/// 抽出来之后，自动化可以走 `WM_APP_RHOLDSIM` 直接喂事件码来验，安全且确定。
///
/// **必须守住的不变式**：吞掉的 DOWN 必须由配对的 UP 收尾；
/// **没吞过 DOWN 的 UP 一律放行**。违反它的后果（2026-10-08 用户报的现场）：
/// 应用收到 RIGHTDOWN、永远收不到 RIGHTUP → 它以为右键一直按着
/// → 「关掉菜单后右键点哪儿都不生效」。
unsafe fn rhold_step(m: u32) -> Option<LRESULT> {
    if !OPT_RHOLD.load(Ordering::Relaxed) {
        return None;
    }
    if m == WM_RBUTTONDOWN {
        let menu_open = MENU_OPEN.load(Ordering::Relaxed);
        R_DOWN_AT.store(GetTickCount(), Ordering::Relaxed);
        R_DOWN_INMENU.store(menu_open, Ordering::Relaxed);
        R_DOWN_SEEN.fetch_add(1, Ordering::Relaxed);
        if menu_open {
            // 菜单开着时右键 = 收起。这一对 DOWN/UP 都由我们吞掉（**成对**，不留孤儿）。
            if !DRAG_ACTIVE.load(Ordering::Relaxed) {
                post(WM_APP_CANCEL, 0, 0);
            }
            return Some(LRESULT(1));
        }
        // 菜单没开：**放行**。为什么不像以前那样吞掉——见 UP 分支的长注释。
        return None;
    }
    if m == WM_RBUTTONUP {
        let t0 = R_DOWN_AT.swap(0, Ordering::Relaxed);
        if t0 == 0 {
            // 没有配对的 DOWN → 一律放行（吞掉会让应用永久以为右键按着）
            return None;
        }
        let was_in_menu = R_DOWN_INMENU.load(Ordering::Relaxed);
        let held = GetTickCount().wrapping_sub(t0) >= RHOLD_MS;
        if held && !MENU_OPEN.load(Ordering::Relaxed) && !EXEMPT.load(Ordering::Relaxed) {
            // 长按 = 呼出菜单。
            // 先记下当前前台窗口：这次放行的 UP 会让它弹出自己的右键菜单，
            // 我们要在 UI 线程里补一次 Esc 把它关掉（见 WM_APP_DISMISS_FG）。
            let fg = GetForegroundWindow();
            FG_HWND.store(fg.0 as isize, Ordering::Relaxed);
            let mut p = POINT::default();
            let _ = GetCursorPos(&mut p);
            CURSOR_X.store(p.x, Ordering::Relaxed);
            CURSOR_Y.store(p.y, Ordering::Relaxed);
            post(WM_APP_SHOW, 0, 0);
            post(WM_APP_DISMISS_FG, 0, 0);
        }
        // ── 为什么长按呼出**不再吞 DOWN / 也不再补发** ──
        //
        // 老设计：吞掉 DOWN，短按时在回调里 `mouse_event` 补发一次。
        // 实测三连炸（2026-10-08，全部可复现）：
        //   ① 在**钩子回调里**注入 → 钩子线程自锁，回调耗时 1000ms，鼠标卡 5~6 秒
        //   ② 改到 **UI 线程**注入 → UI 线程卡死在 `mouse_event` 里永不返回
        //      （证据：补发请求已发出但 inject_done 一直 0，之后所有消息超时 4 秒）
        //   ③ 再改到 **独立线程**注入 → 注入线程卡在第二个 mouse_event，
        //      等于给应用留了个「有 DOWN 没 UP」的孤儿事件
        // 结论：**本进程装着全局鼠标钩子时，任何鼠标事件注入都不可靠。**
        // 所以改成：右键**全程放行**（DOWN、UP 都不吞），短按就是一次完全正常的
        // 右键；长按除了呼出我们的菜单，还会让应用弹一下它自己的菜单 —— 我们在
        // UI 线程补一个 Esc 把它关掉。**没有注入 = 不可能再卡死系统。**
        // 代价只是长按那一瞬应用菜单可能闪一下（失败也是「闪一下」，不会挂）。
        return if was_in_menu { Some(LRESULT(1)) } else { None };
    }
    None
}

unsafe extern "system" fn mouse_hook_impl(code: i32, w: WPARAM, l: LPARAM) -> LRESULT {
    if code < 0 {
        return CallNextHookEx(None, code, w, l);
    }
    // 只放行「我们自己补发的」事件（靠 dwExtraInfo 打标）。
    // ⚠️ 不能一刀切拦掉所有 injected 事件 —— 触摸板、远程桌面、自动化测试
    // 发的也是 injected，一刀切会让它们全部失效。
    if (*(l.0 as *const MsLlHook)).dw_extra_info == RL_MAGIC {
        // 诊断：数一下「我们自己注入的事件」到底有没有带着标记回来。
        // 如果这个数一直是 0，说明 dwExtraInfo 标记没生效 —— 那时我们注入的
        // 右键会被自己当成真实右键吞掉 → 补发 → 再注入 → **无限注入风暴**，
        // 表现为输入队列被灌满、UI 线程卡死（2026-10-08 排查用）。
        MAGIC_SEEN.fetch_add(1, Ordering::Relaxed);
        return CallNextHookEx(None, code, w, l);
    }

    // ── 录制态：抓下一次输入。所有鼠标键均不收录（中/侧键已有独立开关、右键=长按右键专属、
    //    左键=全局点击会破坏正常操作）；仅键盘组合键可录为自定义触发。
    if CAPTURE_TRIGGER.load(Ordering::Relaxed) {
        let btn = match w.0 as u32 {
            WM_LBUTTONDOWN => 1,
            WM_RBUTTONDOWN => 2,
            WM_MBUTTONDOWN => 3,
            WM_XBUTTONDOWN => {
                let xb = ((w.0 >> 16) & 0xffff) as u16;
                if xb == 0x0001 { 4 } else { 5 }
            }
            _ => 0,
        };
        if btn != 0 {
            // 所有鼠标键都不作为「自定义触发」录制：中/侧键已有上方独立开关、
            // 右键=长按右键专属、左键会破坏正常左键操作。显式反馈「不可用作触发键」，
            // 但 **⚠️ 绝不吞掉这次点击**：吞掉的话点击永远到不了设置窗，
            // 录制中的「取消录制」按钮就是个摆设（用户 2026-10-13 报
            // 「一旦开始录制就必须录制完」正是这个原因）。放行后：
            //   · 点「取消录制」→ 正常退出录制
            //   · 点到别处 → 照常传给下面的程序（不打断用户）
            CAP_BAD.store(btn, Ordering::SeqCst);
            post_trigbad();
            return CallNextHookEx(None, code, w, l);
        }
    }

    let m = w.0 as u32;

    // ── 右键长按（呼出键之一）：**必须在状态分派之前处理** ──
    //
    // 它是**跨状态**的：按下时菜单可能没开、松开时菜单可能已经被这次按下关掉了。
    // 之前把 down 记在 `!MENU_OPEN` 分支、up 也在同一分支读，于是出现致命组合：
    //   ① 菜单开着时按右键 → MENU_OPEN 分支只清 R_DOWN_AT，**DOWN 透传给应用**
    //   ② 菜单立刻关掉（MENU_OPEN=false）
    //   ③ 松开 → 走 !MENU_OPEN 分支 → t0==0 → 什么都不做，但 `return LRESULT(1)` 吞掉
    //   ⇒ 应用收到 DOWN 收不到 UP → 以为右键一直按着 → 「右键点哪儿都不生效」
    // 现在整段逻辑收在 `rhold_step()` 里，状态无关、且有明确不变式。
    if let Some(r) = rhold_step(m) {
        return r;
    }

    if !MENU_OPEN.load(Ordering::Relaxed) {
        // 呼出键：中键 / 侧键，两个开关各自独立（可都开）
        // 例外名单命中（或全屏）→ 呼出键**直接放行**，连键盘钩子都不用装
        // 鼠标键 → 按钮编号；自定义鼠标触发按录得的键号匹配
        let btn = match m {
            WM_LBUTTONDOWN => 1,
            WM_RBUTTONDOWN => 2,
            WM_MBUTTONDOWN => 3,
            WM_XBUTTONDOWN => {
                let xb = ((w.0 >> 16) & 0xffff) as u16;
                if xb == 0x0001 { 4 } else { 5 }
            }
            _ => 0,
        };
        let fire = !EXEMPT.load(Ordering::Relaxed)
            && btn != 0
            && !CAPTURE_TRIGGER.load(Ordering::Relaxed)
            && (match btn {
                3 => OPT_MIDDLE.load(Ordering::Relaxed),
                4 | 5 => OPT_SIDE.load(Ordering::Relaxed),
                _ => false,
            } || OPT_CUSTOM_MOUSE.load(Ordering::Relaxed) == btn);
        if fire {
            let mut p = POINT::default();
            let _ = GetCursorPos(&mut p);
            CURSOR_X.store(p.x, Ordering::Relaxed);
            CURSOR_Y.store(p.y, Ordering::Relaxed);
            post(WM_APP_SHOW, 0, 0);
        }
        return CallNextHookEx(None, code, w, l);
    }
    match m {
    WM_MOUSEMOVE => {
        let mut p = POINT::default();
        let _ = GetCursorPos(&mut p);
        CURSOR_X.store(p.x, Ordering::Relaxed);
        CURSOR_Y.store(p.y, Ordering::Relaxed);
        // 按住左键在动：越过阈值算「拖」；**之后每次移动都通知** ——
        // 被拖图标跟手、紧张/疑惑表情的实时切换都靠连续的 drag_step。
        // 消息本身去重（队列里只留一条），坐标以原子量为准。
        if LB_DOWN.load(Ordering::Relaxed) {
            if !LB_MOVED.load(Ordering::Relaxed) {
                let dx = (p.x - LB_DOWN_X.load(Ordering::Relaxed) as i32) as f32;
                let dy = (p.y - LB_DOWN_Y.load(Ordering::Relaxed) as i32) as f32;
                if (dx * dx + dy * dy).sqrt() > DRAG_THRESH {
                    LB_MOVED.store(true, Ordering::Relaxed);
                }
            }
            if LB_MOVED.load(Ordering::Relaxed) && !DRAG_POSTED.swap(true, Ordering::Relaxed) {
                post(WM_APP_DRAGSTEP, 0, 0);
            }
        }
        // 去重：队列里只保留一条 hover 通知，坐标以原子量为准
        if !HOVER_POSTED.swap(true, Ordering::Relaxed) {
            post(WM_APP_HOVER, 0, 0);
        }
    }
        WM_MBUTTONUP | WM_XBUTTONUP => {
            // 松开触发键时左键仍按着 = 用户正拖着东西 → 进入拖放等待态，
            // 菜单保持显示等拖放落环，绝不在这时执行/取消
            let left_held =
                windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState(0x01) as u16 & 0x8000 != 0;
            if left_held || DRAG_ACTIVE.load(Ordering::Relaxed) {
                post(WM_APP_DRAGEVENT, 0, 0);
            } else {
                post(WM_APP_COMMIT, 0, 0);
            }
        }
        WM_MOUSEWHEEL => {
            // 菜单不抢焦点，收不到真正的 `WM_MOUSEWHEEL` → 只能在这里截。
            // 开着菜单时**吞掉**消息，否则滚轮会同时滚动下面的窗口。
            if MENU_OPEN.load(Ordering::Relaxed) && !EXEMPT.load(Ordering::Relaxed) {
                // ⚠️⚠️ 滚轮增量在 **`MSLLHOOKSTRUCT.mouseData` 的高 16 位**，
                // 不在 `wParam` 里 —— 低级钩子的 wParam 只带消息 ID（实测恒为 0x20A，
                // 高 16 位永远是 0）。第一版照窗口消息的写法取 `w.0 >> 16`，
                // 于是 `d` 恒为 0、滚多少都没反应（用户 2026-10-17 报「滚轮后没反应」）。
                //
                // 钩子线程只做「取增量 + PostMessage」，绝不在这里碰 State（铁律）。
                // 一格 = 120 = 转一个图标；细粒度滚轮（触摸板）自然给出小数 → 连续滚。
                let d = ((*(l.0 as *const MsLlHook)).mouse_data >> 16) as u16 as i16;
                if d != 0 {
                    // 传**原始增量**（有符号），换算/夹取全在 UI 线程
                    post(WM_APP_WHEEL, (d as u16) as usize, 0);
                }
                return LRESULT(1);
            }
        }
        WM_LBUTTONDOWN => {
            if DRAG_ACTIVE.load(Ordering::Relaxed) {
                // OLE 拖放中的左键是拖放内部事件，忽略
            } else {
                // 只记录按下，**不立刻执行** —— 否则「按住拖出去」会先把应用启动了。
                let mut p = POINT::default();
                let _ = GetCursorPos(&mut p);
                LB_DOWN_X.store(p.x as isize, Ordering::Relaxed);
                LB_DOWN_Y.store(p.y as isize, Ordering::Relaxed);
                LB_MOVED.store(false, Ordering::Relaxed);
                LB_DOWN.store(true, Ordering::Relaxed);
            }
        }
        WM_LBUTTONUP => {
            let moved = LB_MOVED.swap(false, Ordering::Relaxed);
            LB_DOWN.store(false, Ordering::Relaxed);
            if moved {
                // 拖动松手：盘内=放弃、盘外=请求移除（由 UI 线程按光标位置判定）
                post(WM_APP_DRAGEND, 0, 0);
            } else if DRAG_ACTIVE.load(Ordering::Relaxed) {
                // OLE 拖放内部事件，忽略
            } else if DROP_MODE_ARMED.load(Ordering::Relaxed) {
                // 拖放等待态下在外面松手（没落环）= 收起菜单
                post(WM_APP_CANCEL, 0, 0);
            } else if LOCKED.load(Ordering::Relaxed) {
                post(WM_APP_CLICK, 0, 0);
            } else {
                post(WM_APP_CANCEL, 0, 0);
            }
        }
        WM_RBUTTONDOWN | WM_RBUTTONUP => {
            // 走到这里 = `OPT_RHOLD` 没开（开着的话上面已经 return 了）。
            // 此时右键长按呼出是关闭的，右键应当**完全透传**给应用 ——
            // 不能吞、也不能改 R_DOWN_AT（那个量只属于长按逻辑）。
            // 菜单开着时右键仍要能收起菜单。
            if m == WM_RBUTTONDOWN && !DRAG_ACTIVE.load(Ordering::Relaxed) {
                post(WM_APP_CANCEL, 0, 0);
            }
        }
        _ => {}
    }
    CallNextHookEx(None, code, w, l)
}

/// 低级键盘钩子的事件结构
#[repr(C)]
struct KbdLlHook {
    vk_code: u32,
    scan_code: u32,
    flags: u32,
    time: u32,
    dw_extra_info: usize,
}

/// 低级键盘钩子：菜单打开时截获 Esc / 回车 / 方向键 / Tab
unsafe extern "system" fn key_hook(code: i32, w: WPARAM, l: LPARAM) -> LRESULT {
    let t0 = GetTickCount();
    let r = key_hook_impl(code, w, l);
    let dt = GetTickCount().wrapping_sub(t0);
    if dt > 30 {
        diag_push(DK_KEY_SLOW, dt.min(0x00ff_ffff));
    }
    r
}

unsafe extern "system" fn key_hook_impl(code: i32, w: WPARAM, l: LPARAM) -> LRESULT {
    if code < 0 {
        return CallNextHookEx(None, code, w, l);
    }
    let m = w.0 as u32;
    let kb = &*(l.0 as *const KbdLlHook);
    // ── 自定义键盘触发：菜单关也生效（opt-in 常驻钩子，仅用户显式设键盘组合才挂）──
    if !MENU_OPEN.load(Ordering::Relaxed)
        && !EXEMPT.load(Ordering::Relaxed)
        && OPT_CUSTOM_VK.load(Ordering::Relaxed) != 0
        // ⚠️ 录制期间绝不许呼出：否则按下组合键的第一个键（如 Ctrl）就会把旧触发弹出来
        && !CAPTURE_TRIGGER.load(Ordering::Relaxed)
    {
        if m == 0x0100 || m == 0x0104 {
            let vk = kb.vk_code;
            if !is_modifier(vk) {
                let mods = current_mods();
                if vk == OPT_CUSTOM_VK.load(Ordering::Relaxed)
                    && mods == OPT_CUSTOM_MODS.load(Ordering::Relaxed)
                {
                    let mut p = POINT::default();
                    let _ = GetCursorPos(&mut p);
                    CURSOR_X.store(p.x, Ordering::Relaxed);
                    CURSOR_Y.store(p.y, Ordering::Relaxed);
                    post(WM_APP_SHOW, 0, 0);
                    return LRESULT(1); // 吞掉组合键，不传给前台程序
                }
            }
        }
    }
    // ── 录制态：抓下一个键盘组合（含 Ctrl/Alt/Shift/Win）──
    // 修饰键不立即成键，只实时累积到 CAP_MODS_LIVE；非修饰键才是组合键的「主键」。
    // 这样 Ctrl+W 一定录成「W + Ctrl 修饰位」，不会因 GetAsyncKeyState 时序漏掉修饰键。
    if CAPTURE_TRIGGER.load(Ordering::Relaxed) {
        if m == 0x0100 || m == 0x0101 || m == 0x0104 || m == 0x0105 {
            let vk = kb.vk_code;
            let up = (kb.flags & 0x80) != 0; // LLKHF_UP：键正被松开
            if is_modifier(vk) {
                let bit = match vk {
                    0x11 | 0xA2 | 0xA3 => 1,         // Ctrl（通用 / 左 / 右）
                    0x12 | 0xA4 | 0xA5 => 2,         // Alt（通用 / 左 / 右）
                    0x10 | 0xA0 | 0xA1 => 4,         // Shift（通用 / 左 / 右）
                    0x5B | 0x5C => 8,                // Win（左 / 右）
                    _ => 0,
                };
                if bit != 0 {
                    if up { CAP_MODS_LIVE.fetch_and(!bit, Ordering::SeqCst); }
                    else { CAP_MODS_LIVE.fetch_or(bit, Ordering::SeqCst); }
                }
                // 修饰键本身始终透传给前台程序（不吞、不触发）
                return CallNextHookEx(None, code, w, l);
            }
            if !up {
                // 主键 keydown = 组合键落成
                CAP_KIND.store(2, Ordering::SeqCst);
                CAP_VK.store(vk, Ordering::SeqCst);
                CAP_MODS.store(CAP_MODS_LIVE.load(Ordering::SeqCst), Ordering::SeqCst);
                CAPTURE_TRIGGER.store(false, Ordering::SeqCst);
                post_trigcap();
                return LRESULT(1); // 吞掉主键，不传给前台程序
            }
            // 主键 keyup：已捕获，无视
            return CallNextHookEx(None, code, w, l);
        }
    }
    if code >= 0 && MENU_OPEN.load(Ordering::Relaxed) {
        // 改名模式下先拦编辑键 —— Esc/Enter/Backspace 语义全变了
        if m == 0x0100 && EDIT_ARMED.load(Ordering::Relaxed) {
            match kb.vk_code {
                0x1B => {
                    post(WM_APP_KEY, KA_EDIT_CANCEL, 0);
                    return LRESULT(1);
                }
                0x0D => {
                    post(WM_APP_KEY, KA_EDIT_COMMIT, 0);
                    return LRESULT(1);
                }
                0x08 => {
                    post(WM_APP_KEY, KA_EDIT_BACK, 0);
                    return LRESULT(1);
                }
                0x2E => {
                    post(WM_APP_KEY, KA_EDIT_DEL, 0);
                    return LRESULT(1);
                }
                _ => {}
            }
        }
        if m == 0x0100 || m == 0x0104 {
            let vk = kb.vk_code;
            let mut act = 0usize;
            match vk {
                0x1B => act = KA_CLOSE,       // Esc
                0x0D => act = KA_CONFIRM,     // Enter
                0x25 | 0x26 => act = KA_PREV, // ← ↑
                0x27 | 0x28 => act = KA_NEXT, // → ↓
                0x56 => act = KA_PASTE, // V：剪贴板内容入环
                0x09 => {
                    // Tab：Shift 反向
                    let shift = windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState(0x10)
                        as u16
                        & 0x8000
                        != 0;
                    act = if shift { KA_PREV } else { KA_NEXT };
                }
                _ => {}
            }
            if act != 0 {
                post(WM_APP_KEY, act, 0);
                return LRESULT(1); // 吞掉，不透传给前台程序
            }
            // 改名模式：其余可打印字符追加进缓冲
            if m == 0x0100 && EDIT_ARMED.load(Ordering::Relaxed) {
                if let Some(c) = key_to_char(kb.vk_code, kb.scan_code, kb.flags) {
                    if !c.is_control() {
                        post(WM_APP_CHAR, c as usize, 0);
                        return LRESULT(1);
                    }
                }
            }
        }
    }
    CallNextHookEx(None, code, w, l)
}

/// 当前按下的修饰键（用于录制 / 匹配键盘组合）：1 Ctrl 2 Alt 4 Shift 8 Win
unsafe fn current_mods() -> u8 {
    use windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
    let mut m = 0u8;
    if (GetAsyncKeyState(0x11) as u16 & 0x8000) != 0 { m |= 1; }
    if (GetAsyncKeyState(0x12) as u16 & 0x8000) != 0 { m |= 2; }
    if (GetAsyncKeyState(0x10) as u16 & 0x8000) != 0 { m |= 4; }
    if (GetAsyncKeyState(0x5B) as u16 & 0x8000) != 0 || (GetAsyncKeyState(0x5C) as u16 & 0x8000) != 0 {
        m |= 8;
    }
    m
}

fn is_modifier(vk: u32) -> bool {
    // ⚠️ 必须覆盖**左右专用**虚拟键码：真实输入 / keybd_event / SendInput
    // 送来的 Ctrl/Alt/Shift 经常是 VK_LCONTROL(0xA2)/VK_RCONTROL(0xA3) 这类
    // 专用码，而不是通用码 0x11。旧实现只认通用码 → 左 Ctrl 被当成「主键」录成
    // vk=0xA2/mods=0（即「Ctrl+W」变成裸「LCtrl」），正是用户报的「组合键显示不对」。
    matches!(vk, 0x10 | 0x11 | 0x12 | 0x5B | 0x5C
        | 0xA0 | 0xA1 | 0xA2 | 0xA3 | 0xA4 | 0xA5)
}

unsafe fn post_trigcap() {
    post(WM_APP_TRIGCAP, 0, 0);
}

unsafe fn post_trigbad() {
    post(WM_APP_TRIGBAD, 0, 0);
}

/// 键盘钩子事件 → 一个字符。走 `ToUnicodeEx` 用当前键盘布局转换，
/// 这样非英文布局（德语 ä、西语 ñ）也能正确输入。
/// ⚠️ 必须带 KEYEVENTF_UNICODE 之外的常规路径：ToUnicodeEx 需要 scan code
/// 与键盘状态，而低级别钩子的 flags 里已经带了扩展键位标记，要原样传。
unsafe fn key_to_char(vk: u32, scan: u32, flags: u32) -> Option<char> {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        GetKeyboardState, ToUnicodeEx, KEYEVENTF_EXTENDEDKEY,
    };
    let mut ks = [0u8; 256];
    if GetKeyboardState(&mut ks).is_err() {
        return None;
    }
    let mut buf = [0u16; 4];
    // dwhkl 传 HKL(None) = 用当前活动布局。非英文布局（德语 ä / 西语 ñ）靠这条。
    let n = ToUnicodeEx(vk, scan, &ks, &mut buf, flags, None);
    if n > 0 {
        return char::from_u32(buf[0] as u32);
    }
    // 扩展键（小键盘 Enter/方向键等）ToUnicodeEx 会失败
    if flags & (KEYEVENTF_EXTENDEDKEY.0 as u32) != 0 {
        return None;
    }
    if (0x30..=0x5A).contains(&vk) {
        return char::from_u32(vk);
    }
    None
}

/// 把键盘钩子同步到 `KEY_HOOK_WANT`。**只能在钩子线程里调** ——
/// 低级钩子必须由「安装它的那条线程」装卸，而那条线程要有消息循环。
unsafe fn sync_key_hook() {
    let want = KEY_HOOK_WANT.load(Ordering::Relaxed);
    let have = KEY_HOOK_LOCAL.load(Ordering::Relaxed) != 0;
    if want && !have {
        let hinst = HINSTANCE(GetModuleHandleW(None).unwrap_or(HMODULE(ptr::null_mut())).0);
        match SetWindowsHookExW(WH_KEYBOARD_LL, Some(key_hook), hinst, 0) {
            Ok(h) => {
                KEY_HOOK_HANDLE.store(h.0 as isize, Ordering::SeqCst);
                KEY_HOOK_LOCAL.store(h.0 as isize, Ordering::Relaxed);
            }
            Err(_) => eprintln!("[!] 键盘钩子安装失败"),
        }
    } else if !want && have {
        let kh = KEY_HOOK_LOCAL.swap(0, Ordering::Relaxed);
        if kh != 0 {
            let _ = UnhookWindowsHookEx(HHOOK(kh as *mut c_void));
        }
        KEY_HOOK_HANDLE.store(0, Ordering::SeqCst);
    }
}

/// 请求（或撤销）键盘钩子。任意线程可调；真正的装卸发生在钩子线程里。
fn need_key_hook(on: bool) {
    if KEY_HOOK_WANT.swap(on, Ordering::SeqCst) == on {
        return; // 状态没变，别白叫醒一次
    }
    let tid = HOOK_TID.load(Ordering::Relaxed);
    if tid != 0 {
        unsafe {
            let _ = PostThreadMessageW(tid, WM_APP_HOOK_SYNC, WPARAM(0), LPARAM(0));
        }
    }
}

unsafe fn stop_hook() {
    // 顺手把「要键盘钩子」的意愿清零 —— 否则下次 set_running(true) 时
    // 钩子线程会立刻把键盘钩子装回去（包括菜单其实没开的情况）。
    KEY_HOOK_WANT.store(false, Ordering::SeqCst);
    let h = HOOK_HANDLE.swap(0, Ordering::SeqCst);
    if h != 0 {
        let _ = UnhookWindowsHookEx(HHOOK(h as *mut c_void));
    }
    let k = KEY_HOOK_HANDLE.swap(0, Ordering::SeqCst);
    if k != 0 {
        let _ = UnhookWindowsHookEx(HHOOK(k as *mut c_void));
    }
    let tid = HOOK_TID.swap(0, Ordering::SeqCst);
    if tid != 0 {
        let _ = PostThreadMessageW(tid, WM_QUIT, WPARAM(0), LPARAM(0));
    }
}

/// 优雅退出：**先摘钩子、等钩子线程真正退场，然后才结束进程**。
/// 直接 `std::process::exit(0)` 会把钩子线程连人带枪活埋 —— win32k 里的
/// 钩子没来得及卸载，僵尸壳的钩子仍留在系统输入链上：每个鼠标事件都
/// 等一个死线程（全系统鼠标卡顿），活钩子递进死钩子也永不返回（触发键
/// 全灭）。曾把整条输入链毒化到必须重启系统。退出纪律没有商量余地。
unsafe fn quit_app() -> ! {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{OpenThread, THREAD_ALL_ACCESS, WaitForSingleObject};
    let h = HOOK_HANDLE.swap(0, Ordering::SeqCst);
    if h != 0 {
        let _ = UnhookWindowsHookEx(HHOOK(h as *mut c_void));
    }
    let k = KEY_HOOK_HANDLE.swap(0, Ordering::SeqCst);
    if k != 0 {
        let _ = UnhookWindowsHookEx(HHOOK(k as *mut c_void));
    }
    let tid = HOOK_TID.load(Ordering::SeqCst);
    if tid != 0 {
        // WM_QUIT → 钩子线程的 GetMessage 返回 0 → 循环结束 → 线程退场
        let _ = PostThreadMessageW(tid, WM_QUIT, WPARAM(0), LPARAM(0));
        if let Ok(th) = OpenThread(THREAD_ALL_ACCESS, false, tid) {
            // 最多等 2 秒；超时也照退 —— 钩子已摘，剩下的壳无害
            let _ = WaitForSingleObject(th, 2000);
            let _ = CloseHandle(th);
        }
    }
    std::process::exit(0);
}

// ───────────────────── 外环胶囊右键菜单（改名 / 换色） ─────────────────────

/// 弹出右键菜单 / 改名对话框期间置 true：TrackPopupMenu 和模态对话框都是
/// 消息循环，队列里堆积的真实输入（点击/右键）会在其中被分发 —— 不挡住
/// 的话，一点杂散点击就会关菜单或误触 commit。
static CTX_OPEN: AtomicBool = AtomicBool::new(false);
/// 改名模式是否激活。键盘钩子在**别的线程**上跑，必须靠这个原子量才知道
/// 该不该把字符吞掉（读 st.editing 会跨线程，是 UB）。
static EDIT_ARMED: AtomicBool = AtomicBool::new(false);
// ── 右键菜单 / 原生输入框：**已按用户要求删除** ──
// 2026-10-07：用户明确「去掉换色去掉菜单，双击直接就在胶囊里改名」。
// 原 chip_ctx_menu()（右键 -> 改名/换色 TrackPopupMenu）+ input_dialog() + dlg_proc()
// 全部移除，改名走 editing（就地编辑胶囊，见 edit_push / edit_finish）。
// 保留 CTX_OPEN 是因为 WM_APP_CANCEL 还要用它判断「菜单开着时别误收起」。

// ───────────────────── 托盘 / 启停 ─────────────────────

unsafe fn add_tray_icon(hwnd: HWND) {
    let mut ni = NOTIFYICONDATAW::default();
    ni.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    ni.hWnd = hwnd;
    ni.uID = 1;
    ni.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
    ni.uCallbackMessage = WM_APP_TRAY;
    ni.hIcon = app_icon();
    let tip: Vec<u16> = "PiPie（Ctrl+Alt+R 开关监听）".encode_utf16().collect();
    for (i, c) in tip.iter().enumerate().take(127) {
        ni.szTip[i] = *c;
    }
    let _ = Shell_NotifyIconW(NIM_ADD, &ni);
}

/// 程序图标 = **exe 里那张资源图标**（中心头像球：黑球 + 两只白色药丸眼）。
///
/// 资源由 `build.rs` 调 Windows SDK 的 `rc.exe` 把 `app.rc`（`1 ICON "app.ico"`）
/// 编成 `.res` 再交给链接器；`.ico` 本身由 `make_icon.py` 按中心头像的**真实几何**生成。
///
/// ⚠️ 以前这里用 `IDI_APPLICATION`（系统默认程序图标），跟本程序毫无关系。
/// ⚠️ 尺寸必须用 `SM_CXSMICON`（高 DPI 下是 20/24px），**不要写死 16** ——
///    写死的话系统要把 16px 再拉大一次，托盘上会明显发虚。
/// ⚠️ 万一资源没编进去（本机没装 Windows SDK），退回系统默认图标而不是给个空 HICON，
///    否则托盘会出现一个**看不见的占位**。
unsafe fn app_icon() -> HICON {
    let hinst = HINSTANCE(GetModuleHandleW(None).unwrap_or(HMODULE(ptr::null_mut())).0);
    let s = GetSystemMetrics(SM_CXSMICON).max(16);
    // ID = 1，与 `app.rc` 里的 `1 ICON "app.ico"` 对应（MAKEINTRESOURCE 语义）
    // `LR_SHARED`：系统缓存这一个句柄，我们不必（也不该）去 DestroyIcon。
    LoadImageW(hinst, PCWSTR(1 as *const u16), IMAGE_ICON, s, s,
               LR_DEFAULTCOLOR | LR_SHARED)
        .map(|h| HICON(h.0))
        .unwrap_or_else(|_| {
            LoadIconW(None, IDI_APPLICATION).unwrap_or(HICON(ptr::null_mut()))
        })
}

unsafe fn remove_tray_icon(hwnd: HWND) {
    let mut ni = NOTIFYICONDATAW::default();
    ni.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    ni.hWnd = hwnd;
    ni.uID = 1;
    let _ = Shell_NotifyIconW(NIM_DELETE, &ni);
}

unsafe fn set_running(st: &mut State, on: bool) {
    // `listening`（用户意图，`save_settings` 写它）与 `running`（实际钩子状态）保持同步：
    // 否则任何在钩子安装前发生的保存都会把 listening 写成 0。
    st.listening = on;
    if st.running == on {
        return;
    }
    if on {
        // 上一轮的钩子如果还挂着（托盘「停止监听」失败、或上次退出没走
        // quit_app 的路径），**必须先摘干净**再装新的 —— 否则系统输入链上
        // 会出现两条钩子，其中一条可能挂在死线程上，永远返回不了。
        if HOOK_HANDLE.load(Ordering::Relaxed) != 0 || KEY_HOOK_HANDLE.load(Ordering::Relaxed) != 0 {
            stop_hook();
            // 等旧线程真的退场（最多 1.5s）。不等的话新旧会短暂并存。
            let old = HOOK_TID.swap(0, Ordering::SeqCst);
            if old != 0 {
                use windows::Win32::Foundation::CloseHandle;
                use windows::Win32::System::Threading::{
                    OpenThread, WaitForSingleObject, THREAD_ALL_ACCESS,
                };
                if let Ok(th) = OpenThread(THREAD_ALL_ACCESS, false, old) {
                    let _ = WaitForSingleObject(th, 1500);
                    let _ = CloseHandle(th);
                }
            }
        }
        // 钩子装在专用线程：回调即时返回，不受 UI 线程渲染繁忙影响
        // （低级钩子回调在安装线程的消息循环里执行，装在 UI 线程上会被
        //   渲染帧阻塞，超时后被 Windows 静默移除 → 松开事件丢失 → 菜单卡死）
        std::thread::spawn(|| unsafe {
            let hinst = HINSTANCE(GetModuleHandleW(None).unwrap_or(HMODULE(ptr::null_mut())).0);
            match SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook), hinst, 0) {
                Ok(h) => {
                    HOOK_HANDLE.store(h.0 as isize, Ordering::SeqCst);
                    MOUSE_HOOK_LOCAL.store(h.0 as isize, Ordering::Relaxed);
                }
                Err(_) => {
                    eprintln!("[!] 鼠标钩子安装失败");
                    return;
                }
            }
            // ⚠️ 键盘钩子**不在这里装** —— 它只在菜单开着时才需要，见
            // `KEY_HOOK_WANT` 的注释。由循环顶部的 `sync_key_hook()` 按需装卸。
            HOOK_TID.store(GetCurrentThreadId(), Ordering::SeqCst);
            diag_push(DK_HOOK_ON, 0);
            // ⚠️⚠️ 这个循环是**整个项目最要命的一行**。
            //
            // 旧写法 `while GetMessageW(...).0 > 0 {}` 把三件事混成一件：
            //   > 0  正常收到消息 → 继续
            //   == 0 收到 WM_QUIT → 正常退出
            //   == -1 **出错**     → 旧写法也退出 ←←← 灾难
            // 而 `SetWindowsHookExW` 注册的钩子**不随线程退出自动摘除**。
            // 一旦这里因 -1 退出，内核里就留着一个「挂在死线程上的钩子」：
            //   · 系统仍把每个鼠标事件投进那条链 → 事件卡住，无人处理
            //   · 表现 = 只有已经在等这个事件的窗口能点，任务栏/其它窗口全点不动
            //   · 中键也被同一条死链吃掉 → 径向菜单根本弹不出来（菜单不显示！）
            // `quit_app` 的注释里记过这个现象（「每个鼠标事件都等一个死线程」），
            // 但只防住了「主动退出」这一条路，漏了「循环自己因错误退出」。
            let mut msg = MSG::default();
            loop {
                // 每次醒来先把键盘钩子同步到「该不该挂着」的状态。
                // （`need_key_hook()` 会用 `PostThreadMessageW` 把我们叫醒。）
                sync_key_hook();
                let r = GetMessageW(&mut msg, None, 0, 0).0;
                if r > 0 {
                    continue;
                }
                if r == 0 {
                    break; // WM_QUIT：stop_hook 主动叫停，正常
                }
                // -1 = 出错。**必须先摘钩子再退出**，否则留下毒化输入链的死钩子。
                diag_line(
                    &format!("{}\\radial_log.txt", log_dir()),
                    "HOOK_LOOP_ERR GetMessage 返回 -1，先摘钩子再退出",
                );
                break;
            }
            // 兜底：只要走到了这里（无论正常还是异常退场），确保钩子被摘掉。
            // 重复 UnhookWindowsHookEx 是安全的（第二次返回 0）。
            let mh = MOUSE_HOOK_LOCAL.load(Ordering::Relaxed);
            if mh != 0 {
                let _ = UnhookWindowsHookEx(HHOOK(mh as *mut c_void));
                MOUSE_HOOK_LOCAL.store(0, Ordering::Relaxed);
            }
            let kh = KEY_HOOK_LOCAL.load(Ordering::Relaxed);
            if kh != 0 {
                let _ = UnhookWindowsHookEx(HHOOK(kh as *mut c_void));
                KEY_HOOK_LOCAL.store(0, Ordering::Relaxed);
            }
        });
    } else {
        stop_hook();
        if MENU_OPEN.load(Ordering::Relaxed) {
            MENU_OPEN.store(false, Ordering::SeqCst);
            need_key_hook(false);
            let rh = RING_HWND.load(Ordering::Relaxed);
            if rh != 0 {
                let _ = ShowWindow(HWND(rh as *mut c_void), SW_HIDE);
            }
            let mh = MAIN_HWND.load(Ordering::Relaxed);
            if mh != 0 {
                let _ = KillTimer(HWND(mh as *mut c_void), TIMER_FRAME);
            }
        }
    }
    st.running = on;
    ST_RUNNING_FLAG.store(on, Ordering::SeqCst);
    println!("[*] 监听已{}", if on { "开启" } else { "停止" });
}

// ───────────────────── 悬停命中 ─────────────────────

fn update_hover(st: &mut State, sx: i32, sy: i32) {
    if st.hiding {
        return;
    }
    let px = sx - st.win_x;
    let py = sy - st.win_y;
    let dx = px as f32 - CC;
    let dy = py as f32 - CC;
    let n_ap = st.apps.len();
    // 命中/绘制都必须按**含幽灵的格数**算：有幽灵时整环按 `n+1` 排布、
    // 每格角度也随之变化（`update_hover` 以前用 `apps.len()` 排布，幽灵一出现
    // 图标就「画在这、点到那」）。
    let n = eff_apps(st);

    // 内环：距离最近的图标中心 < SEL+2 → 锁定为当前应用
    // 锁定后移向盘面任意位置（含外环 chips）都不会丢失当前应用
    st.hit_app = None;
    st.hover_now = None;
    if n > 0 {
        let (r_icon, icon_sz) = ring_layout(n);
        let hit_half = icon_sz * 0.5 + 7.0;
        let rr = (dx * dx + dy * dy).sqrt();
        let mut hit: Option<usize> = None;
        if rr <= r_icon + hit_half + 4.0 * K {
            let deg = dx.atan2(-dy).to_degrees().rem_euclid(360.0);
            // 只认真正显示出来的那一格（`app_at_deg` 与绘制共用同一份槽位换算）
            if let Some(i) = app_at_deg(st, deg, n) {
                let s = slot_of(st, i);
                let (ix, iy) = polar(
                    r_icon - SCROLL_SINK * (1.0 - slot_appear(s, n)),
                    slot_angle(s, n),
                );
                let ddx = px as f32 - ix;
                let ddy = py as f32 - iy;
                if (ddx * ddx + ddy * ddy).sqrt() <= hit_half {
                    hit = Some(i);
                }
            }
        }
        st.hit_app = hit;
        st.hover_now = hit; // 高亮严格跟随光标：移出即恢复
        if let Some(i) = hit {
            if st.active_app != Some(i) {
                st.active_app = Some(i);
                st.hover_chip = None;
                st.switch0 = now_s();
            }
        }
    }

    // 外环段：**直接调 hit_chip_at**。
    // 这里曾经内联复制了一份「反旋到局部坐标判盒子」的判定 —— 形状一改
    // （胶囊 → 梯形环）就会「画在这、点到那」。命中判定**只能有一份实现**。
    st.hover_chip = None;
    if let Some(ai) = st.active_app {
        if ai < n_ap {
            st.hover_chip = hit_chip_at(st, ai, px as f32, py as f32);
        }
    }
}

/// 眨眼开合度：在关键帧队列里插值。
/// 1.0 = 正常，0.05 = 闭合，>1 = 睁大过冲（参考实现是 1.08）。
fn blink_open(q: &[(f64, f32)], now: f64) -> f32 {
    if q.is_empty() {
        return 1.0;
    }
    if now <= q[0].0 {
        return q[0].1;
    }
    for i in 1..q.len() {
        if now <= q[i].0 {
            let (t0, v0) = q[i - 1];
            let (t1, v1) = q[i];
            let f = clamp01(((now - t0) / (t1 - t0).max(1e-6)) as f32);
            return lerp(v0, v1, f);
        }
    }
    q[q.len() - 1].1
}

/// 头像状态推进：注视平滑 + 自动眨眼。
///
/// 注视用**帧率无关的指数平滑** `k = 1 - exp(-5.66*dt)`（与 my-pet 一致）——
/// 用固定系数的话高帧率下会追得过快、低帧率下会拖沓。
fn face_tick(st: &mut State, now: f64) {
    let dt = if st.face_prev <= 0.0 {
        0.016
    } else {
        (now - st.face_prev).clamp(0.0, 0.1)
    };
    st.face_prev = now;

    // 注视目标 = 鼠标相对盘心的方向，归一化到 -1..1
    let bx = st.win_x as f32 + CC;
    let by = st.win_y as f32 + CC;
    let cx = CURSOR_X.load(Ordering::Relaxed) as f32;
    let cy = CURSOR_Y.load(Ordering::Relaxed) as f32;
    let rng = (disc_r(st.apps.len()) * 1.6).max(1.0);
    let tx = ((cx - bx) / rng).clamp(-1.0, 1.0);
    let ty = ((cy - by) / rng).clamp(-1.0, 1.0);
    let k = 1.0 - (-5.66 * dt as f32).exp();
    st.gaze[0] += (tx - st.gaze[0]) * k;
    st.gaze[1] += (ty - st.gaze[1]) * k;

    // 自动眨眼：预备(睁大) → 闭 → 过冲睁大 → 回落
    if st.blink_next <= 0.0 {
        st.blink_next = now + 1.0;
    }
    if now >= st.blink_next {
        st.blink_q.clear();
        st.blink_q.push((now, 0.05));
        st.blink_q.push((now + 0.070, 0.05));
        st.blink_q.push((now + 0.150, 1.08));
        st.blink_q.push((now + 0.300, 1.00));
        // 14% 概率连眨两下（参考实现的行为）
        if (((now * 977.0).fract().abs() * 1000.0) as u32) % 100 < 14 {
            st.blink_q.push((now + 0.370, 0.05));
            st.blink_q.push((now + 0.480, 1.00));
        }
        let rnd = (((now * 131.0).fract().abs() * 1000.0) as u32) % 2200;
        st.blink_next = now + 2.4 + rnd as f64 / 1000.0;
    }
    while st.blink_q.len() > 1 && st.blink_q[1].0 <= now {
        st.blink_q.remove(0);
    }
}

fn nav_app(st: &mut State, dir: i32) {
    let n = st.apps.len();
    if n == 0 {
        return;
    }
    let cur = match st.active_app {
        Some(i) => i as i32,
        None => {
            if dir > 0 {
                -1
            } else {
                0
            }
        }
    };
    let next = ((cur + dir) % n as i32 + n as i32) % n as i32;
    let next = next as usize;
    if st.active_app != Some(next) {
        st.active_app = Some(next);
        st.hover_chip = None;
        st.switch0 = now_s();
        // 键盘翻页翻到了**另一页**的应用 → 顺手把那半页滚出来，否则选中看不见
        ring_reveal(st, next);
    }
}

fn nav_chip(st: &mut State, dir: i32) {
    let Some(ai) = st.active_app else {
        nav_app(st, dir);
        return;
    };
    let m = st.apps.get(ai).map(|a| a.items.len()).unwrap_or(0);
    if m == 0 {
        nav_app(st, dir);
        return;
    }
    let cur = st.hover_chip.map(|c| c as i32).unwrap_or(-1);
    let next = ((cur + dir) % m as i32 + m as i32) % m as i32;
    st.hover_chip = Some(next as usize);
}

fn unlock(st: &mut State) {
    st.locked = false;
    LOCKED.store(false, Ordering::SeqCst);
}

// ───────────────────── 帧驱动 ─────────────────────

#[allow(unused_assignments)] // 末尾的 moving=true 已覆盖前面所有分支
unsafe fn on_timer(st: &mut State) {
    if !MENU_OPEN.load(Ordering::Relaxed) {
        return;
    }
    // 悬停外环窗口条目 → 实时小窗预览（DWM 缩略图）
    thumb_tick(st);
    // loading 超时兜底：后台线程万一卡死（被杀软拦、COM 挂住），菜单不能永远挂着。
    // 网址那条路要联网，合法地慢 → 只兜「启动 / 窗口」这两类。
    if st.loading.is_some() && st.loading_kind != LOAD_KIND_URL && now_s() - st.loading_t0 > 3.0 {
        finish_loading(st);
    }

    // 把 UI 私有状态推给看门狗（跨线程不能直接读 st，只能靠镜像）
    SNAP_HIDING.store(st.hiding, Ordering::Relaxed);
    SNAP_ENTERING.store(st.entering, Ordering::Relaxed);
    SNAP_PENDING_RM.store(!st.pending_rms.is_empty(), Ordering::Relaxed);
    SNAP_FOCUS.store(focus_app(st).unwrap_or(usize::MAX), Ordering::Relaxed);
    SNAP_HOVER_CHIP.store(st.hover_chip.unwrap_or(usize::MAX), Ordering::Relaxed);
    SNAP_HIT_APP.store(st.hit_app.unwrap_or(usize::MAX), Ordering::Relaxed);
    let now = now_s();
    let mut alpha = 255u8;

    // ── 拖放等待态超时兜底 ──
    // `DROP_MODE_ARMED` 只在 DragLeave→物理抬手（走钩子的左键分支）或 Drop 时才清。
    // 但只要拖放会话「异常结束」——比如拖进来的文件在环上绕一圈又拖走、源进程直接收场，
    // 我们既收不到 Drop、也没等到配对的物理抬手——这个旗标就会**永久卡住**。
    // 后果：之后每次左键都被当成「拖放残留」而只关菜单、**不启动应用**（用户 2026-10-12
    // 16:48~17:00 实测：反复呼出、点内环/外环都没反应，快照里 DROP_ARMED 一直挂着）。
    // 这里按「距上次 OLE 事件」超时自动解除，保证最多卡 DROP_ARM_TTL_MS。
    if DROP_MODE_ARMED.load(Ordering::Relaxed) {
        let age = GetTickCount().wrapping_sub(DROP_ARM_AT.load(Ordering::Relaxed));
        if age > DROP_ARM_TTL_MS {
            DROP_MODE_ARMED.store(false, Ordering::SeqCst);
            DRAG_ACTIVE.store(false, Ordering::SeqCst);
            st.drop_mode = false;
            st.drop_target_app = None;
            println!("[drop] 拖放等待态超时解除（{}ms 无 OLE 事件）", age);
        }
    }

    // ── 闲置自动收起（安全阀）──
    // 菜单开着却没有关闭路径时，它会一直悬着：此后每个鼠标事件都要先穿过
    // 我们的钩子判断一遍，用户感觉像「鼠标被什么吸住了」。
    // 超过 IDLE_CLOSE_AFTER 没有任何输入就自己收起。
    // 常驻模式（persistent）**不自动收** —— 那是用户明确要的常驻行为。
    if !st.hiding && !st.persistent && now - st.last_input >= IDLE_CLOSE_AFTER {
        // 倒计时移除 / OLE 拖放进行中 / 窗口捕获中都不动：那几种状态下自动收起会丢东西
        if st.pending_rms.is_empty()
            && !DRAG_ACTIVE.load(Ordering::Relaxed)
            && st.window_capture.is_none()
        {
            hide_menu(st);
        }
    }

    if st.hiding {
        let k = clamp01(((now - st.hide0) / 0.16) as f32);
        alpha = (255.0 * (1.0 - k)) as u8;
        if k >= 1.0 {
            close_menu_now(st);
            return;
        }
    }
    let entering_anim = st.entering;
    if st.entering {
        let k = clamp01(((now - st.ent0) / 0.32) as f32);
        if k >= 1.0 {
            st.entering = false;
        }
    }

    let mut moving = false;
    // ── 内环连续滚动：每帧把 `ring_off` 追向 `ring_to`（指数跟随，≈130ms 收敛）──
    // ⚠️ 这是滚动**唯一的动画源**：绘制 / 命中 / 外环锚点全都现读 `ring_off`，
    //    所以偏移一到位画面就静止 —— 别再给磁贴叠第二层「跟随」，那会变成双重拖影。
    {
        let d = st.ring_to - st.ring_off;
        if d.abs() > 0.002 {
            st.ring_off += d * RING_SCROLL_K;
            // 滚动中：高亮必须跟着撤 —— 光标一动不动地停在旧位置上，
            // 但那一格下面的图标已经换成另一个应用了（`hover_now` 是每帧重算的粘态）。
            st.hit_app = None;
            st.hover_now = None;
            scroll_prune(st);
            moving = true;
        } else if st.ring_off != st.ring_to {
            st.ring_off = st.ring_to;
            moving = true;
            // 刚停稳：按**当前光标**重判一次命中。不重判的话，用户滚完什么都不动，
            // 高亮 / 外环还赖在上一轮的那一格上（粘性状态当位置 —— 这坑栽过四次）。
            let mut p = POINT::default();
            let _ = GetCursorPos(&mut p);
            update_hover(st, p.x, p.y);
        }
    }
    // 拖进来的幽灵磁贴站在**多出来的那一格**（满环时它在下一页）→ 自动滚过去露出它，
    // 否则用户拖进来什么都看不见，只会以为「拖了没反应」。
    if st.drop_ghost.is_some() {
        ring_reveal(st, st.apps.len());
    }
    // ── 窗口捕获：轮询「是否有窗口正被拖动」（hwndMoveSize），松手落盘 ──
    if poll_window_capture(st) {
        moving = true; // 捕获中保持持续重绘（预览 + 头像忙碌）
    }
    // ── 拖出移除：情绪过渡 / 拖动淡出 / 取消按钮出现 / 倒计时到点 ──
    {
        // 情绪过渡（帧率无关，≈120ms 到位）
        if st.mood_k < 1.0 {
            st.mood_k = (st.mood_k + (now - st.face_prev).max(0.0) as f32 * 8.0).min(1.0);
            moving = true;
        }
        // 拖动中的图标/胶囊：**拖出盘面（胶囊是拖出窗口）才淡掉** —— 注释本来就是
        // 这么写的，但代码没判 in_disc/，只要拖起来就 =1.0。
        // ⚠️ 后果：图标一抓起来就整个消失，「抓手感」没了 —— 而拖拽换位 / 移除
        //    全靠"我手里抓着它"这个感知（用户 2026-10-14 定的草稿里，拖动那一屏
        //    被拖图标是**可见**的，还带投影）。所以改成只有真的出界才淡。
        let dragging = st.drag_app.is_some() || st.drag_chip.is_some();
        let (dsx, dsy) = drag_screen(st);
        let outside = if st.drag_chip.is_some() {
            !in_window(st, dsx, dsy)
        } else {
            !in_disc(st, dsx, dsy)
        };
        let want_fade = if dragging && outside { 1.0 } else { 0.0 };
        if (st.drag_fade - want_fade).abs() > 0.004 {
            st.drag_fade += (want_fade - st.drag_fade) * 0.25;
            moving = true;
        } else {
            st.drag_fade = want_fade;
        }
        // 移除区虚线：**越过阈值就转起来**（用户 2026-10-14 定）。
        // 为什么用旋转而不是变色：本项目是纯黑白灰，没有"危险色"可用；
        // 而"静态的一圈线"和"转起来的线"在余光里差别极大 —— 这是"已上膛"的信号。
        // ⚠️ `on_timer` 没有 `dt`（它是 16ms 固定帧的定时器，动画全用固定系数，
        //    见上面的 `drag_fade` 用 0.25）—— 这里跟着同一套约定，别去借 `setup_tick` 的 `k`。
        let armed = dragging && remove_zone(st, dsx, dsy);
        let want_spin = if armed { 1.0 } else { 0.0 };
        if (st.rm_spin_k - want_spin).abs() > 0.004 {
            st.rm_spin_k += (want_spin - st.rm_spin_k) * 0.25;
            moving = true;
        } else {
            st.rm_spin_k = want_spin;
        }
        if st.rm_spin_k > 0.002 {
            st.rm_spin = (st.rm_spin + RM_SPIN_STEP * st.rm_spin_k) % 360.0;
            moving = true;
        }
        // 取消按钮出现
        let want_btn = if st.pending_rms.is_empty() { 0.0 } else { 1.0 };
        if (st.cancel_a - want_btn).abs() > 0.004 {
            st.cancel_a += (want_btn - st.cancel_a) * 0.30;
            moving = true;
        } else {
            st.cancel_a = want_btn;
        }
        // 倒计时到点 → 落盘。队列里可能一次到点多个（各自有 deadline）。
        if !st.pending_rms.is_empty() {
            let next = rm_soonest(st).unwrap_or(f64::INFINITY);
            if now >= next {
                commit_expired(st, now);
                if st.pending_rms.is_empty() {
                    st.cancel_a = 0.0;
                    set_mood(st, 0);
                } else {
                    moving = true;
                }
            } else {
                moving = true; // 倒计时期间必须持续重绘（边框在走）
            }
        }
        // 短暂情绪（如拖放悬停的「忙碌」）到点回落；待确认移除期间则回到生气
        if st.mood_hold > 0.0 && now >= st.mood_hold {
            st.mood_hold = 0.0;
            if st.mood == 5 {
                set_mood(st, if !st.pending_rms.is_empty() { 2 } else { 0 });
            }
        }
        // 发呆：菜单开着、无拖动、无待确认、无拖放，且一段时间没有任何输入
        if st.mood == 0
            && st.mood_hold == 0.0
            && st.drag_app.is_none()
            && st.drag_chip.is_none()
            && st.pending_rms.is_empty()
            && !st.drop_mode
            && now - st.last_input >= IDLE_MOOD_AFTER
        {
            set_mood(st, 3);
        }
    }
    // ── 改名编辑：光标要闪 → **必须持续重绘** ──
    // ⚠️⚠️ 这是「进了编辑态却看不到光标闪」的根因：`editing` 从来没参与过 `moving`
    // 的判断，于是编辑态只画一帧就静止，光标相位固定在 `edit_caret` 那一刻 ——
    // 落在「灭」的半周期上就**完全看不见**（用户 2026-10-09 报「不知道这是编辑模式」）。
    if st.editing.is_some() {
        moving = true;
    }
    // ── 后台查「网址是否已开」期间：宠物「忙碌」表情 + 该段跳动的点都要动 ──
    if st.loading.is_some() {
        moving = true;
    }
    // 例外名单：每 250ms 刷一次「当前前台程序该不该放行」。
    // 钩子回调只做一次 `EXEMPT` 原子读，重活全在这里（UI 线程）。
    if now - st.exc_t >= 0.25 {
        st.exc_t = now;
        update_exempt(st);
    }

    // 悬停压暗渐变。焦点必须来自**真实命中**（见 focus_app）：
    // 用粘性的 active_app 会让「移到中心头像后」其它应用依旧蒙着灰。
    let focus = focus_app(st);
    for i in 0..st.apps.len().min(MAX_APPS) {
        let target = if focus.is_some() && focus != Some(i) { 1.0 } else { 0.0 };
        let d = target - st.app_anim[i];
        if d.abs() > 0.004 {
            st.app_anim[i] += d * 0.28;
            moving = true;
        } else {
            st.app_anim[i] = target;
        }
    }
    // chip 出现渐变
    for v in st.chip_anim.iter_mut() {
        let target = if st.active_app.is_some() { 1.0 } else { 0.0 };
        let d = target - *v;
        if d.abs() > 0.004 {
            *v += d * 0.26;
            moving = true;
        } else {
            *v = target;
        }
    }
    // 内环磁贴 hover/选中动效：高亮跟随光标（移出即灭）；拖放入环时落到谁身上谁亮
    for i in 0..st.apps.len().min(MAX_APPS) {
        // `drop_suggest` = 拖入内容"认得出来的宿主内环" → 也点亮它（引导用户往上丢）
        let target = if st.hover_now == Some(i)
            || st.drop_target_app == Some(i)
            || st.drop_suggest == Some(i)
        {
            1.0
        } else {
            0.0
        };
        let d = target - st.app_hot[i];
        if d.abs() > 0.003 {
            st.app_hot[i] += d * 0.30;
            moving = true;
        } else {
            st.app_hot[i] = target;
        }
    }
    // ── 拖动重排：图标让位开缝（**槽位**平滑滑到「预览槽位」）──
    // 主流重排序是**图标滑开让出空位**，插入线落在空位里；原来的实现在原地闪一下，
    // 光标只能画在某个已有图标上 —— 用户 2026-10-14 明确否掉。
    {
        let n = eff_apps(st);
        if n > 0 {
            for i in 0..st.apps.len().min(MAX_APPS) {
                // 目标 = **槽位**（不是极角）：槽位不会绕 360° 回绕，直接线性追赶就行，
                // 而且淡出判定也认槽位（见 `slot_appear`）。
                let tgt = match (st.drag_app, st.drag_over_app) {
                    (Some(ai), Some(ins)) if st.drop_ghost.is_none() => {
                        drag_preview_slot(ai, ins, i) as f32 - st.ring_off
                    }
                    _ => slot_of(st, i),
                };
                let cur = st.app_slot_s[i];
                if !cur.is_finite() {
                    st.app_slot_s[i] = tgt; // 首帧直接落位
                } else {
                    let d = tgt - cur;
                    if d.abs() > 0.01 {
                        st.app_slot_s[i] = cur + d * 0.30;
                        moving = true;
                    } else {
                        st.app_slot_s[i] = tgt;
                    }
                }
            }
        }
    }
    // 拖窗口/网页进来时：外环**锁定**在宿主应用上。
    // 这样幽灵胶囊一定看得见，而且与松手后的落点（`ensure_host_app` 按 exe 匹配宿主）
    // 永远一致 —— 光标在别处乱晃也不会把外环切走、让用户以为「没成功」。
    if let Some(ai) = st.drop_suggest {
        if ai < st.apps.len() {
            st.active_app = Some(ai);
        }
    }

    // ── 外环重排：胶囊让位开缝（段角平滑滑到「预览布局」）──
    // 与内环同一套语义：被拖那一段的槽位空出来（=缝），插入光标落在缝里。
    if let Some(ai) = st.active_app {
        if ai < st.apps.len() {
            // 换了应用 → 条目下标没有可比性，整组重置
            if st.chip_seg_app != ai {
                st.chip_seg_app = ai;
                st.chip_seg_a = [(f32::NAN, f32::NAN); MAX_ITEMS];
            }
            let m = st.apps[ai].items.len().min(MAX_ITEMS);
            // 幽灵胶囊淡入/淡出（拖动期间出现，松手或移开就消失）
            let gwant = if chip_ghost_seg(st, ai).is_some() { 1.0 } else { 0.0 };
            st.chip_ghost_a += (gwant - st.chip_ghost_a) * 0.30;
            if (gwant - st.chip_ghost_a).abs() > 0.01 {
                moving = true;
            }
            let parked = chip_layout(st, ai).0;
            let preview = match (st.drag_chip, st.drag_over_chip) {
                (Some((dai, dj)), Some(ins)) if dai == ai => chip_preview_segs(st, ai, dj, ins),
                _ => Vec::new(),
            };
            for j in 0..m {
                let tgt = if preview.is_empty() {
                    parked.get(j).copied().unwrap_or((0.0, 0.0))
                } else {
                    preview.get(j).copied().unwrap_or((0.0, 0.0))
                };
                let (c1, c2) = st.chip_seg_a[j];
                if !c1.is_finite() || !c2.is_finite() {
                    st.chip_seg_a[j] = tgt; // 首帧直接落位
                } else {
                    // 两端各走各自的最短弧（a2 恒 = a1 + 段宽，所以宽度自动保持一致）
                    let d1 = (tgt.0 - c1 + 540.0) % 360.0 - 180.0;
                    let d2 = (tgt.1 - c2 + 540.0) % 360.0 - 180.0;
                    if d1.abs() > 0.04 || d2.abs() > 0.04 {
                        st.chip_seg_a[j] = (c1 + d1 * 0.30, c2 + d2 * 0.30);
                        moving = true;
                    } else {
                        st.chip_seg_a[j] = tgt;
                    }
                }
            }
        }
    }
    // 外环 chip hover 动效
    for j in 0..MAX_ITEMS {
        let target = if st.hover_chip == Some(j) { 1.0 } else { 0.0 };
        let d = target - st.chip_hot[j];
        if d.abs() > 0.003 {
            st.chip_hot[j] += d * 0.34;
            moving = true;
        } else {
            st.chip_hot[j] = target;
        }
    }
    if st.commit_pulse > 0.0 {
        st.commit_pulse -= 0.016 / 0.22;
        if st.commit_pulse < 0.0 {
            st.commit_pulse = 0.0;
        }
        moving = true;
    }

    // 头像一直在呼吸/眨眼/漂移 —— 菜单打开期间需要持续重绘。
    //
    // 这里**必须每帧都跑**：注视（眼睛跟着鼠标转）是高频动作，
    // 之前节流到 30fps 会让整幅菜单实际只有 30fps —— 表现就是「转动时掉帧」。
    // face_tick 本身只有几次浮点运算，代价可忽略；真正的开销在 draw_scene，
    // 那部分靠下面「投影早退」优化。
    // 拖放进行中暂停表情球连续动画：OLE 的 DragOver 是跨进程等待回复的
    // 调用，线程必须随时空闲应答 —— 60fps 连续重绘会让回复排队，
    // 系统因此挂出忙碌指针。只有状态变化才出帧。
    if !DRAG_ACTIVE.load(Ordering::Relaxed) && !DROP_MODE_ARMED.load(Ordering::Relaxed) {
        st.face_n = st.face_n.wrapping_add(1);
        face_tick(st, now);
        moving = true;
    }

    // 松手落位动画：只让「被拖的那一个」动，到点就结束
    if st.land_app != usize::MAX {
        if now - st.land_t0 >= LAND_SECS {
            st.land_app = usize::MAX;
        } else {
            moving = true;
        }
    }

    // ── 头像气泡：淡入淡出 + 状态消息到期自动消失 ──
    {
        let want = if bub_text(st).is_some() { 1.0 } else { 0.0 };
        if (st.bub_a - want).abs() > 0.004 {
            st.bub_a += (want - st.bub_a) * 0.30;
            moving = true;
        } else {
            st.bub_a = want;
        }
        if st.bub_until > 0.0 && now >= st.bub_until {
            st.bub_until = 0.0;
        }
    }
    // ── 延迟收菜单：让气泡显示完再收 ──
    // 原来拖入完成 0.16s 就把菜单收了，用户只看到「一闪」—— 反馈等于没有。
    if st.hide_at > 0.0 && now >= st.hide_at {
        st.hide_at = 0.0;
        hide_menu(st);
    }

    // 实时玻璃：后台线程抓到的新帧换进来（Vec 直接 move，没有拷贝）。
    // 抓失败时槽里是空的 → 留着上一帧，不会闪回旧桌面。
    //
    // ⚠️ **滚动中故意不换**：换了 `glass_bg` 就等于换了玻璃的内容，
    //    盘面色层缓存（见 `glass_disc`）必须重采一遍 —— 那是 7ms，正好落在
    //    每一格滚轮动画上，用户看到的就是「一卡一卡」。背景在滚动这 ~200ms 里
    //    冻结一下没人看得出来（动的本来就是图标），停下来立刻恢复实时。
    // 有人正把窗口拖向环时同样冻结（`GLASS_FREEZE`）：换帧 = 盘面色层重采 7ms，
    // 而这 7ms 正和用户在拖的那个窗口抢 CPU/GPU，此时背景根本没人看。
    let scrolling = (st.ring_off - st.ring_to).abs() > 0.002;
    if st.glass && st.glass_live && !scrolling && !GLASS_FREEZE.load(Ordering::Relaxed) {
        if let Ok(mut s) = GLASS_LIVE_SLOT.lock() {
            if let Some(buf) = s.take() {
                // ⚠️ 不要在这里 `moving = true`：OLE 拖放期间是故意停连续重绘的
                // （线程要随时应答 DragOver，见上面的说明）。下一帧自然用新图。
                set_glass_bg(st, buf);
                GLASS_LIVE_TAKEN.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    let animating = st.hiding || entering_anim || moving;
    if animating {
        let ft0 = GetTickCount();
        frame(st, alpha);
        let fdt = GetTickCount().wrapping_sub(ft0);
        if fdt > 100 {
            diag_push(DK_UI_SLOW, fdt.min(0x00ff_ffff));
        }
    }
}

/// ULW 的 `AC_SRC_ALPHA` 要求缓冲区是**预乘 alpha**。
///
/// `blend_over` 走的是标准 source-over，产出的是**非预乘** RGBA。
/// 直接提交时系统按预乘解释，等于把每个半透明像素的原色**全亮度**叠上去 ——
/// 表现就是：本该是淡淡一层投影的地方，变成一整块实色
/// （用户看到的「环外面还有个半透明蓝色的环形块」，颜色正好等于 SHADOW）。
///
/// 注意：离屏 dump 不走这里 —— 它是把 alpha 正确合成到不透明背景上，
/// 所以 dump 一直是对的，只有真机窗口错了。这也是这个 bug 藏这么久的原因。
unsafe fn premultiply(buf: &mut [u8]) {
    for px in buf.chunks_exact_mut(4) {
        let a = px[3] as u32;
        if a == 255 {
            continue;
        }
        if a == 0 {
            px[0] = 0;
            px[1] = 0;
            px[2] = 0;
            continue;
        }
        px[0] = ((px[0] as u32 * a + 127) / 255) as u8;
        px[1] = ((px[1] as u32 * a + 127) / 255) as u8;
        px[2] = ((px[2] as u32 * a + 127) / 255) as u8;
    }
}

unsafe fn frame(st: &mut State, alpha: u8) {
    if st.dbi_bits.is_null() {
        return;
    }
    let n = (CANVAS * CANVAS * 4) as usize;
    let buf = std::slice::from_raw_parts_mut(st.dbi_bits, n);
    draw_scene(st, buf);
    premultiply(buf);

    let rh = RING_HWND.load(Ordering::Relaxed);
    if rh == 0 {
        return;
    }
    let dst = POINT { x: st.win_x, y: st.win_y };
    let src = POINT::default();
    let size = SIZE { cx: CANVAS, cy: CANVAS };
    let blend = BLENDFUNCTION {
        BlendOp: 0u8,
        BlendFlags: 0,
        SourceConstantAlpha: alpha,
        AlphaFormat: 1u8,
    };
    let _ = UpdateLayeredWindow(
        HWND(rh as *mut c_void),
        HDC(ptr::null_mut()),
        Some(&dst),
        Some(&size),
        st.dbi_dc,
        Some(&src),
        COLORREF(0),
        Some(&blend),
        ULW_ALPHA,
    );
}

// ───────────────────── 菜单生命周期 ─────────────────────

unsafe fn show_menu(st: &mut State, sx: i32, sy: i32) {
    let rh = RING_HWND.load(Ordering::Relaxed);
    if rh == 0 {
        return;
    }
    let hwnd = HWND(rh as *mut c_void);

    // 画布钳位到虚拟屏幕
    let vx = GetSystemMetrics(SM_XVIRTUALSCREEN);
    let vy = GetSystemMetrics(SM_YVIRTUALSCREEN);
    let vw = GetSystemMetrics(SM_CXVIRTUALSCREEN);
    let vh = GetSystemMetrics(SM_CYVIRTUALSCREEN);
    st.win_x = (sx - CANVAS / 2).clamp(vx, vx + vw - CANVAS);
    st.win_y = (sy - CANVAS / 2).clamp(vy, vy + vh - CANVAS);

    // 呼出时刷新「哪些应用正在运行」+ 每个应用最前面的窗口（圆点 + 悬停预览的数据源）
    refresh_running(st);

    st.active_app = None;
    st.hover_chip = None;
    st.app_anim = [0.0; MAX_APPS];
    st.app_hot = [0.0; MAX_APPS];
    // 槽位重新落位（NAN = 首帧直接赋值，不做「从旧槽位滑过来」的动画）
    st.app_slot_s = [f32::NAN; MAX_APPS];
    // ⚠️ 每次呼出都从第 1 页开始 —— 滚动偏移是**位置**，不是粘性状态。
    //    留着上次的偏移，用户每次弹出都看到「随机那一页」，而且首帧的图标
    //    位置与命中判定会错开一格（`update_hover` 用的是偏移后的槽位）。
    st.ring_off = 0.0;
    st.ring_to = 0.0;
    // 换次呼出 → UIA 探测结果作废（用户可能换了窗口/换了目录）
    st.wincap_failed.clear();
    st.land_app = usize::MAX;
    st.chip_anim = [0.0; MAX_ITEMS];
    st.chip_hot = [0.0; MAX_ITEMS];
    st.chip_seg_a = [(f32::NAN, f32::NAN); MAX_ITEMS];
    st.chip_seg_app = usize::MAX;
    st.commit_pulse = 0.0;
    st.commit_app = usize::MAX;
    st.commit_chip = usize::MAX;
    st.bub_main.clear();
    st.bub_sub.clear();
    st.bub_until = 0.0;
    onb_say(st);
    st.bub_a = 0.0;
    st.hide_at = 0.0;
    st.hiding = false;
    st.entering = true;
    st.ent0 = now_s();
    st.face0 = now_s();
    st.face_prev = 0.0;
    st.gaze = [0.0, 0.0];
    st.last_input = now_s();
    st.cur_x = sx;
    st.cur_y = sy;
    st.mood_hold = 0.0;
    diag_push(DK_MENU_OPEN, 0);
    st.blink_q.clear();
    st.blink_next = 0.0;
    st.switch0 = now_s();
    st.locked = false;
    LOCKED.store(false, Ordering::SeqCst);

    MENU_OPEN.store(true, Ordering::SeqCst);
    // 菜单开了 → 现在才需要键盘钩子（Esc/回车/方向键/改名输入）
    need_key_hook(true);
    let _ = ShowWindow(hwnd, SW_HIDE);
    // 液态玻璃：**现在**抓背景（窗口已隐藏，抓到的才是真桌面）。
    // ⚠️ `DwmFlush()` 不能省：`ShowWindow(SW_HIDE)` 只是改窗口状态，
    //    真正从屏幕上撤下去要等 DWM 下一次合成 —— 不等就会抓到自己的残影，
    //    玻璃里套着一圈盘（这种 bug 很难从截图上看出来）。
    if st.glass {
        let _ = windows::Win32::Graphics::Dwm::DwmFlush();
        snap_glass(st, st.win_x, st.win_y);
    } else if !st.glass_bg.is_empty() {
        set_glass_bg(st, Vec::new());
    }
    let _ = SetWindowPos(hwnd, HWND_TOPMOST, st.win_x, st.win_y, CANVAS, CANVAS, SWP_NOACTIVATE);
    frame(st, 255);
    let _ = SetWindowPos(
        hwnd,
        HWND_TOPMOST,
        st.win_x,
        st.win_y,
        CANVAS,
        CANVAS,
        SWP_NOACTIVATE | SWP_SHOWWINDOW,
    );
    // 窗口已经显示 → 从这一刻起后台线程每 33ms 重抓一次背景（首帧快照仍是上面那张）
    glass_live_start(st, hwnd);
    let mh = MAIN_HWND.load(Ordering::Relaxed);
    if mh != 0 {
        let _ = SetTimer(HWND(mh as *mut c_void), TIMER_FRAME, 16, None);
    }
}

unsafe fn hide_menu(st: &mut State) {
    if st.hiding {
        return;
    }
    st.hiding = true;
    st.hide0 = now_s();
    let mh = MAIN_HWND.load(Ordering::Relaxed);
    if mh != 0 {
        let _ = SetTimer(HWND(mh as *mut c_void), TIMER_FRAME, 16, None);
    }
}

/// 裁剪工作集：把映射进来的字体/DLL 共享页还给系统。
/// `-1, -1` = 缩到「当前最小需求」，即尽可能多地丢弃干净页。
/// ⚠️ **只在完全空闲时调**（菜单已收、没在动画、没在拖拽）—— 正在画的时候裁剪
///    会把当前帧要用的页也丢掉，反而卡顿。
unsafe fn trim_working_set() {
    // 顺带把 GDI 的临时对象也清一下（字体/画笔缓存），这个 API 无副作用。
    let _ = SetProcessWorkingSetSize(GetCurrentProcess(), usize::MAX, usize::MAX);
}

unsafe fn close_menu_now(st: &mut State) {
    MENU_OPEN.store(false, Ordering::SeqCst);
    // 实时玻璃：先停后台抓屏，再撤掉「排除抓屏」—— 用户之后的录屏/截图里
    // 不能因为我们开过菜单就少了这个窗口
    let rh = RING_HWND.load(Ordering::Relaxed);
    glass_live_stop(HWND(rh as *mut c_void));
    // 玻璃快照只服务于"这次呼出"，收起就还回去（900KB）。下次呼出重新抓 —— 3.5ms。
    // 常驻模式下菜单一直开着，自然一直留着。
    if !st.glass_bg.is_empty() {
        set_glass_bg(st, Vec::new());
    }
    // 盘面色层缓存（bbox 大小）同理还回去 —— 菜单不在屏幕上时不该占着
    glass_disc_drop();
    // 收起后开始计时：闲置 `TRIM_AFTER` 秒就把工作集还回去。
    st.trim_at = now_s() + TRIM_AFTER;
    // 菜单关了 → 若没有键盘组合自定义触发，立即摘掉键盘钩子（打字路径上不留钩子）
    if st.trig_custom_kind != 2 {
        need_key_hook(false);
    }
    LOCKED.store(false, Ordering::SeqCst);
    st.locked = false;
    st.hiding = false;
    // 菜单收起 → 预览必须立刻消失（它是独立窗口，不会跟着菜单一起淡出）
    thumb_hide();
    // 菜单收起 = 待执行的延迟单击作废。否则它到期后会对着
    // 「菜单已经没了」的状态去 commit —— 凭空启动一个应用。
    //
    // ⚠️ `KillTimer` 的窗口必须和 `SetTimer` 的一致：`SetTimer` 是在
    // **MAIN_HWND** 上装的（`WM_APP_CLICK` 被 post 到 MAIN_HWND，所以那里
    // 的 `hwnd` 就是它），早先在 RING_HWND 上 Kill → 根本没杀掉，
    // 留下一个每 450ms 响一次的重复定时器。
    let mh = MAIN_HWND.load(Ordering::Relaxed);
    if mh != 0 {
        clear_delayed_click(st, HWND(mh as *mut c_void));
    } else {
        st.pending_click = None;
    }
    st.edit_last_click = -99.0;
    // ⚠️ 这里**不要**清 R_DOWN_AT！清了会造出反向 bug：
    // 右键用来关菜单时 down 已被吞、up 到达时 t0==0 → 被放行 → 应用收到一个
    // **孤立 RIGHTUP**（没有配对的 DOWN）→ 桌面会凭空弹出右键菜单。
    // R_DOWN_AT 的收尾交给配对的 UP（见 mouse_hook_impl 的不变式）。
    // 菜单收起 = 撤销窗口结束 → 待移除的内容**立即落盘**（不再放回）。
    // ⚠️ 旧行为是「收起 = 放回」，导致拖走应用后必须干等倒计时走完才算删掉，
    //    期间一关轮盘删除就作废 —— 用户明确反馈不合理（2026-10-12）。
    //    撤销只认显式点「取消」按钮；关掉轮盘 = 确认删除。
    commit_all_pending(st);
    st.drag_app = None;
    st.drag_chip = None;
    st.drag_over_app = None;
    st.drag_over_chip = None;
    st.drag_fade = 0.0;
    st.land_app = usize::MAX;
    // ⚠️ 滚动偏移也随菜单归零（粘性状态复位铁律）：`show_menu` 里同样会清一次，
    //    但常驻模式（`locked`）不重走 `show_menu`，漏在这里就会带着上一页的偏移继续用。
    st.ring_off = 0.0;
    st.ring_to = 0.0;
    st.app_slot_s = [f32::NAN; MAX_APPS];
    // 窗口捕获随菜单一起清：没落盘的拖动作废（用户松手在环外 = 放弃）
    st.window_capture = None;
    // 拖放等待态随菜单一起清：残留的 DROP_MODE_ARMED 会让下次呼出后**所有左键都只关菜单、
    // 不启动应用**（用户 2026-10-12 实测）。
    st.drop_mode = false;
    st.drop_target_app = None;
    st.drop_suggest = None;
    st.drop_ghost = None;
    DROP_MODE_ARMED.store(false, Ordering::SeqCst);
    DRAG_ACTIVE.store(false, Ordering::SeqCst);
    // 后台「网址是否已开」查询的 loading 一并作废（Esc 取消后不该再冒出来动作）
    st.loading = None;
    st.mood_hold = 0.0;
    // 气泡随菜单一起收：延迟收菜单的排期作废，别在关掉之后又去 hide 一次
    st.bub_until = 0.0;
    st.bub_a = 0.0;
    st.hide_at = 0.0;
    // 改名态必须一起清：否则 EDIT_ARMED 残留，键盘钩子会一直吞字符
    st.editing = None;
    st.edit_buf.clear();
    EDIT_ARMED.store(false, Ordering::SeqCst);
    set_mood(st, 0);
    diag_push(DK_MENU_CLOSE, 0);
    let mh = MAIN_HWND.load(Ordering::Relaxed);
    if mh != 0 {
        let _ = KillTimer(HWND(mh as *mut c_void), TIMER_FRAME);
    }
    let rh = RING_HWND.load(Ordering::Relaxed);
    if rh != 0 {
        let _ = ShowWindow(HWND(rh as *mut c_void), SW_HIDE);
    }
}

// ───────────────────── 托盘宿主窗口过程 ─────────────────────

fn mif(on: bool) -> MENU_ITEM_FLAGS {
    MENU_ITEM_FLAGS(MF_STRING.0 | if on { MF_CHECKED.0 } else { MF_UNCHECKED.0 })
}

unsafe extern "system" fn host_proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    match msg {
        WM_HOTKEY => {
            if w.0 as i32 == HOTKEY_TOGGLE {
                let st = state_of(hwnd);
                let on = !st.running;
                let st2 = state_of(hwnd);
                set_running(st2, on);
            }
            LRESULT(0)
        }
        WM_APP_TRIGCAP | WM_APP_TRIGSIM => {
            let st = state_of(hwnd);
            let sim = msg == WM_APP_TRIGSIM;
            if sim {
                // 仅测试：模拟捕获到键盘组合 Ctrl+W（自定义触发现在只接受键盘组合）
                CAP_KIND.store(2, Ordering::SeqCst);
                CAP_VK.store(87, Ordering::SeqCst);  // 0x57 = W
                CAP_MODS.store(1, Ordering::SeqCst);  // 1 = Ctrl
                CAP_BTN.store(0, Ordering::SeqCst);
            }
            let kind = CAP_KIND.load(Ordering::SeqCst);
            if kind == 2 {
                st.trig_custom_kind = 2;
                st.trig_custom_vk = CAP_VK.load(Ordering::SeqCst);
                st.trig_custom_mods = CAP_MODS.load(Ordering::SeqCst);
                need_key_hook(true); // 键盘组合触发需常驻键盘钩子
            } else if kind == 1 {
                st.trig_custom_kind = 1;
                st.trig_custom_btn = CAP_BTN.load(Ordering::SeqCst);
                need_key_hook(false); // 鼠标触发不需要键盘钩子
            }
            st.trig_capturing = false;
            st.trig_capture_bad = 0;
            CAP_BAD.store(0, Ordering::SeqCst);
            CAPTURE_TRIGGER.store(false, Ordering::SeqCst);
            apply_key_opts(st);
            save_settings(st);
            // ⚠️ 必须踢帧定时器：否则新录的触发键要等下一次鼠标事件才显示出来
            //    （「录完等一会才显示」）
            kick_setup_frame(st);
            LRESULT(0)
        }
        WM_APP_TRIGBAD => {
            let st = state_of(hwnd);
            if st.trig_capturing {
                st.trig_capture_bad = CAP_BAD.load(Ordering::SeqCst);
                // ⚠️ 这里原来只 `InvalidateRect` —— 但本窗口 **WM_PAINT 是空实现**，
                //    根本没有重绘。必须踢帧定时器才会真的刷新。
                kick_setup_frame(st);
            }
            LRESULT(0)
        }
        WM_APP_SHOW => {
            let st = state_of(hwnd);
            if !MENU_OPEN.load(Ordering::Relaxed) {
                let mut x = CURSOR_X.load(Ordering::Relaxed);
                let mut y = CURSOR_Y.load(Ordering::Relaxed);
                if x == 0 && y == 0 {
                    let mut p = POINT::default();
                    let _ = GetCursorPos(&mut p);
                    x = p.x;
                    y = p.y;
                }
                show_menu(st, x, y);
                // 有新版本 → 借这一次**真实呼出**提醒一次（每个版本最多一次）。
                // ⚠️ 不放进 `show_menu`：启动那次「亮个相」也走 `show_menu`，
                //    它紧接着的 `say(REVEAL_MSG)` 会把提醒顶掉，可账已经记了 —— 白丢一次。
                update_say(st);
            }
            LRESULT(0)
        }
        WM_APP_HOVER => {
            HOVER_POSTED.store(false, Ordering::Relaxed);
            let st = state_of(hwnd);
            if MENU_OPEN.load(Ordering::Relaxed) {
                let mut x = CURSOR_X.load(Ordering::Relaxed);
                let mut y = CURSOR_Y.load(Ordering::Relaxed);
                if x == 0 && y == 0 {
                    let mut p = POINT::default();
                    let _ = GetCursorPos(&mut p);
                    x = p.x;
                    y = p.y;
                }
                note_input(st, x, y);
                // 拖着外环胶囊时冻结 hover：光标一挪 active_app 就可能换人，
                // chip 布局跟着洗牌，抓在手里的那颗就不知道飞哪去了
                if st.drag_chip.is_none() {
                    update_hover(st, x, y);
                }
            }
            LRESULT(0)
        }
        WM_APP_COMMIT => {
            if CTX_OPEN.load(Ordering::Relaxed) {
                return LRESULT(0);
            }
            let st = state_of(hwnd);
            if MENU_OPEN.load(Ordering::Relaxed) {
                if st.persistent {
                    // 常驻模式：松开触发键后保持显示，改由点击/回车确认
                    st.locked = true;
                    LOCKED.store(true, Ordering::SeqCst);
                    println!("[*] 已锁定 · 点击或回车确认，Esc 关闭");
                } else {
                    // 非常驻模式：松开即执行 —— 但必须真的停在某个目标上。
                    // 旧逻辑无条件 commit，导致「刚呼出就松手」也会启动上次悬停的应用。
                    let x = CURSOR_X.load(Ordering::Relaxed);
                    let y = CURSOR_Y.load(Ordering::Relaxed);
                    update_hover(st, x, y);
                    if st.hover_chip.is_some() || st.hit_app.is_some() {
                        commit(st);
                    } else {
                        unlock(st);
                        close_menu_now(st);
                    }
                }
            }
            LRESULT(0)
        }
        WM_APP_DROPSIM => {
            // 仅测试：把合成载荷塞进 DROP_PT / DROP_PAYLOAD，之后测试自己发
            // WM_APP_DRAGEVENT(1 预览 / 3 提交) —— 与真实 OLE 拖放走同一条代码路径。
            let x = (w.0 & 0xFFFF) as i32;
            let y = ((w.0 >> 16) & 0xFFFF) as i32;
            DROP_PT.store(pack_pt(x, y), Ordering::SeqCst);
            let it = match l.0 {
                0 => DropItem { name: "示例站点".to_string(), target: "https://example.com/".to_string(), is_url: true, is_dir: false },
                1 => DropItem { name: "Windows".to_string(), target: "C:\\Windows".to_string(), is_url: false, is_dir: true },
                2 => DropItem { name: "probe".to_string(), target: "C:\\__rl_probe__.zzz".to_string(), is_url: false, is_dir: false },
                3 => DropItem { name: "probe2".to_string(), target: "C:\\__rl_probe2__.qqq".to_string(), is_url: false, is_dir: false },
                _ => DropItem { name: "Explorer".to_string(), target: "C:\\Windows\\explorer.exe".to_string(), is_url: false, is_dir: false },
            };
            if let Ok(mut q) = DROP_PAYLOAD.lock() {
                *q = vec![it];
            }
            LRESULT(0)
        }
        WM_APP_DRAGEVENT => {
            let st = state_of(hwnd);
            if !MENU_OPEN.load(Ordering::Relaxed) {
                return LRESULT(0);
            }
            let (sx, sy) = unpack_pt(DROP_PT.load(Ordering::Relaxed));
            let px = (sx - st.win_x) as f32;
            let py = (sy - st.win_y) as f32;
            match w.0 {
                // 进入 / 移动：算「建议目标 / 幽灵」，并高亮光标下的落点
                0 | 1 => {
                    st.drop_mode = true;
                    DROP_MODE_ARMED.store(true, Ordering::SeqCst);
                    // 同步刷新「最后一次拖放活动」时间戳，与 OLE 回调保持一致
                    // （这样任何来源的 arming 都会重新计时，超时兜底不会误判为陈旧）
                    DROP_ARM_AT.store(GetTickCount(), Ordering::SeqCst);
                    // 新拖拽会话（进入）→ 失效预览缓存，保证首帧重算；后续 DragOver 复用
                    if w.0 == 0 {
                        drop_preview_invalidate();
                    }
                    let items = DROP_PAYLOAD.lock().map(|q| q.clone()).unwrap_or_default();
                    compute_drop_preview(st, &items);
                    st.drop_target_app = drop_force_app(st, sx, sy);
                    // 文件悬停在环上 = 收下前的「忙碌」；drag_step 在拖放开始时
                    // 可能设过「紧张」，进环就放松下来
                    set_mood(st, 0);
                }
                // 离开环区域
                2 => {
                    st.drop_target_app = None;
                    st.drop_suggest = None;
                    st.drop_ghost = None;
                    drop_preview_invalidate();
                    set_mood(st, 0);
                }
                // 落下：**按落点归属**（不再自动挑目标）
                _ => {
                    drop_preview_invalidate();
                    let items = DROP_PAYLOAD
                        .lock()
                        .map(|mut q| std::mem::take(&mut *q))
                        .unwrap_or_default();
                    DRAG_ACTIVE.store(false, Ordering::SeqCst);
                    DROP_MODE_ARMED.store(false, Ordering::SeqCst);
                    st.drop_mode = false;
                    st.drop_target_app = None;
                    // ⚠️ 必须在 take 幽灵**之前**算落点：幽灵在场时按 n+1 排布，
                    //    幽灵槽的索引 = apps.len()。take 之后再算就退回 n 排布、永远命中不到。
                    let n_ap = st.apps.len();
                    let over_tile = drop_slot_app(st, px, py);
                    let ghost = st.drop_ghost.take();
                    let suggest = st.drop_suggest.take();
                    // 落点优先级：**正好压在幽灵槽上 → 建幽灵**；压在某个已有内环上 → 进它外环；
                    // 其余任何位置（含盘心头像/空白）→ 按识别结果落地（有幽灵就建、否则归宿主）。
                    // ⚠️ 曾经要求"必须精确丢到幽灵/高亮磁贴上"，结果用户丢到头像上 = 无操作，
                    //    体验等于坏掉（用户 2026-10-12 实测）。
                    drag_log(&format!(
                        "drop tgt={} px={} py={} n_ap={} over_tile={:?} ghost={} suggest={:?}",
                        items.first().map(|i| i.target.clone()).unwrap_or_default(),
                        px as i32, py as i32, n_ap, over_tile, ghost.is_some(), suggest
                    ));
                    let n = if items.is_empty() {
                        0
                    } else if let Some(g) = ghost {
                        match over_tile {
                            Some(idx) if idx == n_ap => create_from_ghost(st, &items, &g), // 命中幽灵槽
                            Some(ai) => process_drop(st, &items, Some(ai)), // 压在别的内环上
                            None => create_from_ghost(st, &items, &g),      // 盘心/空白 → 仍按幽灵建
                        }
                    } else if let Some(sai) = suggest {
                        // 认得宿主：落在别的内环上 → 归它；否则直接归**宿主应用**
                        match over_tile {
                            Some(ai) if ai < n_ap => process_drop(st, &items, Some(ai)),
                            _ => process_drop(st, &items, Some(sai)),
                        }
                    } else {
                        // 载荷本身是应用（exe/lnk）→ 经典「拖进来加个内环」；已在环里则去重跳过
                        let is_app = items
                            .first()
                            .map(|it| exe_name_of(&it.target).is_some())
                            .unwrap_or(false);
                        let dup = items
                            .first()
                            .and_then(|it| unsafe { resolve_host_exe(it) })
                            .and_then(|exe| host_app_index(st, &exe))
                            .is_some();
                        if is_app && !dup {
                            process_drop(st, &items, None)
                        } else {
                            0
                        }
                    };
                    if n > 0 {
                        drag_log(&format!("Drop processed, added={}", n));
                        println!("[drop] 入环 {} 项", n);
                        st.commit_pulse = 1.0;
                        // 「忙碌」：低头消化刚进环的新成员（到点自动回落）
                        set_mood(st, 5);
                        st.mood_hold = now_s() + 1.5;
                        say(st, &format!("已添加：{}", drop_name(&items)), "拖进来就能用，右键可改", 1.8);
                    } else {
                        // ⚠️ 这里以前只有一行 `println!` —— 用户拖重复 / 落点没命中时
                        //    界面上零反馈，表现就是「拖进去了但不知道成没成」。
                        //    现在分两种原因报出来，别让用户猜。
                        println!("[drop] 落点不在目标上 / 已存在 → 无操作");
                        let nm = drop_name(&items);
                        let dup = items
                            .first()
                            .and_then(|it| unsafe { resolve_host_exe(it) })
                            .and_then(|exe| host_app_index(st, &exe))
                            .is_some();
                        if dup {
                            say(st, &format!("已经在环上了：{}", nm), "", 1.7);
                        } else {
                            say(st, &format!("没接住：{}", nm), "再拖一次试试", 1.7);
                        }
                    }
                    // 常驻模式：保持打开，让用户亲眼看到新成员出现；
                    // 否则**等气泡显示完再收** —— 原来这里立刻 `hide_menu`，
                    // 0.16s 淡出把气泡一起带走，等于没反馈。
                    if !(st.persistent && st.locked) {
                        st.hide_at = now_s() + 2.0;
                    }
                }
            }
            LRESULT(0)
        }
        WM_APP_WINARM => {
            // 仅测试：用给定 hwnd 武装窗口捕获（l = hwnd isize）
            let st = state_of(hwnd);
            if MENU_OPEN.load(Ordering::Relaxed) {
                let h = HWND(l.0 as isize as *mut c_void);
                unsafe {
                    arm_window_capture(st, h);
                    if st.window_capture.is_some() {
                        st.window_capture.as_mut().unwrap().test = true;
                    }
                }
            }
            LRESULT(0)
        }
        WM_APP_WINDROP => {
            // 仅测试：模拟「松手」—— 光标在盘面内就把捕获落盘
            let st = state_of(hwnd);
            if MENU_OPEN.load(Ordering::Relaxed) && st.window_capture.is_some() {
                unsafe {
                    let wc = st.window_capture.take().unwrap();
                    let (cx, cy) = cursor_now();
                    let over = in_disc(st, cx, cy);
                    if over {
                        capture_window(
                            st,
                            HWND(wc.hwnd as *mut c_void),
                            &wc.exe,
                            &wc.title,
                            &wc.relaunch,
                        );
                        wincap_probe_start(st, HWND(wc.hwnd as *mut c_void), &wc.exe);
                        st.commit_pulse = 1.0;
                        set_mood(st, 5);
                        st.mood_hold = now_s() + 1.5;
                    }
                    st.drop_suggest = None;
                    st.drop_ghost = None;
                    st.drop_mode = false;
                    set_mood(st, 0);
                    println!("[wincap-test] drop over={} exe={}", over, wc.exe);
                }
            }
            LRESULT(0)
        }
        WM_APP_WINPROBE => {
            // 后台 UIA 探到了文件夹路径 / 网址 → 回填刚落盘的那条窗口条目
            let st = state_of(hwnd);
            unsafe { wincap_apply_probe(st) };
            LRESULT(0)
        }
        WM_APP_CHAR => {
            let st = state_of(hwnd);
            if st.editing.is_some() {
                if let Some(c) = char::from_u32(w.0 as u32) {
                    edit_push(st, c);
                    frame(st, 255);
                }
            }
            LRESULT(0)
        }
        // 仅自动化测试：双击改名要真鼠标 + 中文 IME，自动化跑不了。
        // 这条诊断消息和真实消息**调用同一批函数**，只是把输入摆出来。
        //
        // ⚠️ phase 0 会**自己**把 active_app 设成条目最多的那个应用 ——
        // 正常流程靠中键呼出菜单 + hover 选中，那条路要真鼠标钩子，
        // 在有僵尸实例的环境里会被它们的钩子吞掉，测不到真正的逻辑。
        WM_APP_EDITSIM => {
            let st = state_of(hwnd);
            // l 的高位（>= 1000）指定要选哪个应用下标
            let want_ai = if l.0 >= 1000 { Some((l.0 - 1000) as usize) } else { None };
            let ai = match want_ai {
                Some(a) if a < st.apps.len() => a,
                _ => st
                    .apps
                    .iter()
                    .enumerate()
                    .max_by_key(|(_, a)| a.items.len())
                    .map(|(i, _)| i)
                    .unwrap_or(0),
            };
            if w.0 == 0 {
                st.active_app = Some(ai);
                st.hover_chip = Some(0);
            }
            // phase 9 = **只设状态**，不依赖该应用有没有外环条目。
            // 原来它藏在 `items 非空` 分支里 → 工具屏（items 恒空）根本测不了。
            if w.0 == 9 {
                st.editing = None;
                st.edit_buf.clear();
                EDIT_ARMED.store(false, Ordering::SeqCst);
                st.active_app = Some(ai);
                let has_items = st.apps.get(ai).map(|a| !a.items.is_empty()).unwrap_or(false);
                let jj = if l.0 < 1000 { l.0 as usize } else { 0 };
                st.hover_chip = if has_items { Some(jj) } else { None };
                if has_items {
                    FORCE_CHIP.store(jj as i64, Ordering::SeqCst);
                }
                return LRESULT(0);
            }
            if st.apps.get(ai).map(|a| !a.items.is_empty()).unwrap_or(false) {
                let j = if l.0 < 1000 { l.0 as usize } else { 0 };
                match w.0 {
                    0 => {
                        let now = now_s();
                        st.edit_last_click = now;
                        st.edit_last_pos =
                            (CURSOR_X.load(Ordering::Relaxed), CURSOR_Y.load(Ordering::Relaxed));
                        chip_click(st, ai, j, st.edit_last_pos.0, st.edit_last_pos.1);
                        println!("[editsim] 进入编辑 app={} chip={} buf={:?}",
                            ai, j, st.edit_buf);
                    }
                    1 => {
                        if let Some(c) = char::from_u32(l.0 as u32) {
                            edit_push(st, c);
                        }
                    }
                    2 => {
                        st.edit_buf.pop();
                        st.edit_caret = now_s();
                    }
                    3 => {
                        unsafe { edit_finish(st, true) };
                    }
                    4 => {
                        unsafe { edit_finish(st, false) };
                    }
                    5 => {
                        st.edit_buf.clear();
                        unsafe { edit_finish(st, true) };
                    }
                    // 仅测试：**只设** active_app / hover_chip，不进编辑态。
                    // 自动化要触发「单击胶囊」走真 `chip_click` 路径，但真光标会被
                    // 环境里的僵尸钩子吞掉、`update_hover()` 算不出 hover_chip。
                    // phase 0 也不行 —— 它会顺手 chip_click() 进编辑态，把后续全带偏。
                    9 => {
                        st.editing = None;
                        st.edit_buf.clear();
                        EDIT_ARMED.store(false, Ordering::SeqCst);
                        st.active_app = Some(ai);
                        st.hover_chip = Some(j);
                        FORCE_CHIP.store(j as i64, Ordering::SeqCst);
                    }
                    _ => {}
                }
            }
            LRESULT(0)
        }
        WM_APP_KEY => {
            let st = state_of(hwnd);
            if MENU_OPEN.load(Ordering::Relaxed) {
                // 改名模式优先：编辑键的语义与全局快捷键不同
                if st.editing.is_some() {
                    match w.0 {
                        KA_EDIT_COMMIT => {
                            edit_finish(st, true);
                            frame(st, 255);
                        }
                        KA_EDIT_CANCEL => {
                            edit_finish(st, false);
                            frame(st, 255);
                        }
                        KA_EDIT_BACK => {
                            st.edit_buf.pop();
                            if st.edit_started < 0.0 {
                                st.edit_started = 0.0;
                            }
                            st.edit_caret = now_s();
                            frame(st, 255);
                        }
                        KA_EDIT_DEL => {
                            // 光标在末尾，Delete 等价于 Backspace（单行编辑）
                            st.edit_buf.pop();
                            st.edit_caret = now_s();
                            frame(st, 255);
                        }
                        _ => {}
                    }
                    return LRESULT(0);
                } else {
                    match w.0 {
                        KA_CLOSE => {
                            unlock(st);
                            close_menu_now(st);
                        }
                        KA_CONFIRM => commit(st),
                        KA_PASTE => unsafe { paste_add(st) },
                        KA_PREV => {
                            if st.hover_chip.is_some() {
                                nav_chip(st, -1)
                            } else {
                                nav_app(st, -1)
                            }
                        }
                        KA_NEXT => {
                            if st.hover_chip.is_some() {
                                nav_chip(st, 1)
                            } else {
                                nav_app(st, 1)
                            }
                        }
                        _ => {}
                    }
                }
            }
            LRESULT(0)
        }
        WM_APP_CLICK => {
            if CTX_OPEN.load(Ordering::Relaxed) {
                return LRESULT(0);
            }
            let st = state_of(hwnd);
            if MENU_OPEN.load(Ordering::Relaxed) {
                let x = CURSOR_X.load(Ordering::Relaxed);
                let y = CURSOR_Y.load(Ordering::Relaxed);
                note_input(st, x, y);
                update_hover(st, x, y);
                // 仅测试：`FORCE_CHIP` >= 0 时用它顶替 hover 判定。
                // `update_hover()` 依赖**真鼠标**写的 CURSOR_X/Y，自动化环境里
                // 僵尸钩子会吞掉鼠标事件 → hover 算不出来 → 走不到 chip_click。
                // 这个旁路只在本进程被诊断消息驱动时生效，正常路径永远走 hover。
                if FORCE_CHIP.load(Ordering::Relaxed) >= 0 {
                    if let Some(ai) = st.active_app {
                        st.hover_chip = Some(FORCE_CHIP.load(Ordering::Relaxed) as usize);
                        st.hit_app = Some(ai);
                    }
                }
                // 取消按钮优先：它在倒计时期间要能点
                if hit_rm_btn(st, (x - st.win_x) as f32, (y - st.win_y) as f32) {
                    cancel_remove(st);
                    frame(st, 255);
                } else if let Some(ai) = st.active_app {
                    // 胶囊点击：先判双击（改名），否则**延迟**这一击再决定执行与否。
                    if let Some(j) = st.hover_chip {
                        match chip_click(st, ai, j, x, y) {
                            Some(ChipClick::Double) => {
                                // 第二击 → 取消待执行的一击，进改名
                                clear_delayed_click(st, hwnd);
                                frame(st, 255);
                                return LRESULT(0);
                            }
                            Some(ChipClick::Defer) => {
                                // 第一击 → 排延迟执行，**这一击不执行**。
                                // ⚠️⚠️ 但**视觉反馈必须立刻给**：双击窗口有 ~570ms
                                //    （`DBL_WAIT_MS`），等它过期才亮 loading，用户读到的
                                //    就是"点了没反应"（2026-10-16 用户：「loading 出得太慢，
                                //    应该一点立刻出动画」）。执行时机不变 —— 见
                                //    `chip_click` 头的注释：早执行 = 双击的第一击就把应用打开了。
                                arm_delayed_click(st, hwnd, ai, j);
                                start_loading(st, ai, j, LOAD_KIND_LAUNCH);
                                frame(st, 255);
                                return LRESULT(0);
                            }
                            None => {} // 已在改名态，点别处放弃编辑 → 继续往下
                        }
                    }
                    if st.hover_chip.is_some() || st.hit_app.is_some() {
                        // 只有真正点在「外环 chip」或「内环应用图标」上才执行。
                        // 旧逻辑用 st.active_app.is_some() 判定，但 active_app 是粘性的
                        // （移向外环时不丢），结果点屏幕任意位置都会启动当前应用。
                        commit(st);
                    } else {
                        unlock(st);
                        close_menu_now(st);
                    }
                } else {
                    unlock(st);
                    close_menu_now(st);
                }
            }
            LRESULT(0)
        }
        WM_APP_DRAGSTEP => {
            DRAG_POSTED.store(false, Ordering::Relaxed);
            let st = state_of(hwnd);
            if MENU_OPEN.load(Ordering::Relaxed) && !DRAG_ACTIVE.load(Ordering::Relaxed) {
                drag_step(
                    st,
                    CURSOR_X.load(Ordering::Relaxed),
                    CURSOR_Y.load(Ordering::Relaxed),
                );
            }
            LRESULT(0)
        }
        WM_APP_DRAGEND => {
            if CTX_OPEN.load(Ordering::Relaxed) {
                return LRESULT(0);
            }
            let st = state_of(hwnd);
            if MENU_OPEN.load(Ordering::Relaxed) {
                drag_end(
                    st,
                    CURSOR_X.load(Ordering::Relaxed),
                    CURSOR_Y.load(Ordering::Relaxed),
                );
            }
            LRESULT(0)
        }
        // 仅测试：跑一次与钩子同一条的例外判定并报告
        WM_APP_EXEMSIM => {
            let st = state_of(hwnd);
            unsafe {
                update_exempt(st);
            }
            let ex = EXEMPT.load(Ordering::Relaxed);
            let (fg, fs) = unsafe { (fg_proc_name(), fg_fullscreen()) };
            // `progman_*` = **不用真的把桌面切到前台**也能验这条判据（issue #1 回归）：
            // 桌面矩形盖满整屏（geo=1）正是它被误判成全屏的原因，`is_shell_wnd` 是摘掉它的那一刀。
            // 期望值：`progman_geo=1 progman_shell=1 progman_fs=0`。
            let pm = match unsafe { FindWindowW(PCWSTR(wide("Progman").as_ptr()), PCWSTR::null()) }
            {
                Ok(h) => h,
                Err(_) => HWND(ptr::null_mut()),
            };
            let (geo, shell) = unsafe { (window_covers_monitor(pm), is_shell_wnd(pm)) };
            println!(
                "[exemsim] fg={:?} fullscreen={} black={} white={} exempt={} progman_geo={} progman_shell={} progman_fs={} mode={}",
                fg,
                fs as u8,
                st.exc.len(),
                st.exc_w.len(),
                ex as u8,
                geo as u8,
                shell as u8,
                (geo && !shell) as u8,
                st.exc_mode
            );
            LRESULT(0)
        }
        // 后台「这个网址是不是已经开着」查完了（`switch_bg` 线程发的）。
        // 后台把「启动 / 聚焦窗口」做完了 → 收尾（清 loading、收起菜单）。
        // 与 `WM_APP_LOADED`（网址查询，收尾还要切标签）刻意分开。
        WM_APP_LAUNCHED => {
            let st = state_of(hwnd);
            finish_loading(st);
            LRESULT(0)
        }
        WM_APP_UPDATE_DONE => {
            // 后台查完版本 → 取结果刷关于页（异步改 State 后必须 kick_setup_frame）
            let st = state_of(hwnd);
            let got = UPDATE_RESULT.lock().ok().and_then(|mut q| q.take());
            if let Some((s, latest, msg)) = got {
                st.update_st = s;
                st.update_latest = latest;
                st.update_msg = msg;
                // s=3「有新版本」→ 把版本号记到 ini：提醒要等**下一次呼出**才发生，
                // 中间进程可能已经重启过，不落盘这条提醒就会丢。
                if s == 3 && st.update_avail_ver != st.update_latest {
                    st.update_avail_ver = st.update_latest.clone();
                    unsafe { save_settings(st) };
                }
            }
            unsafe { kick_setup_frame(st) };
            LRESULT(0)
        }
        WM_APP_LOADED => {
            let st = state_of(hwnd);
            // `loading` 为空 = 用户中途按了 Esc / 菜单已收起 → 整件事作废
            if let Some(_) = st.loading.take() {
                let url = std::mem::take(&mut st.loading_url);
                let hit = LOAD_HIT.load(Ordering::SeqCst);
                set_mood(st, 0);
                unsafe {
                    hide_menu(st);
                }
                if hit {
                    println!("[*] 已切到打开着的网页 · {url}");
                } else {
                    // 没开着 → 照常打开
                    launch(&url);
                    println!("[*] 启动 · {url}");
                }
                unsafe {
                    frame(st, 255);
                }
            }
            LRESULT(0)
        }
        // 仅自动化测试：拖放/拖出这套状态机没法用合成鼠标事件驱动
        // （低级钩子只认真实事件），所以留一条诊断消息直接走同一条代码路径。
        // w = (y<<16)|x 屏幕坐标；l = 0 拖动中 / 1 松手 / 2 只挪光标。
        WM_APP_DRAGSIM => {
            let st = state_of(hwnd);
            let x = (w.0 & 0xFFFF) as i32;
            let y = ((w.0 >> 16) & 0xFFFF) as i32;
            CURSOR_X.store(x, Ordering::Relaxed);
            CURSOR_Y.store(y, Ordering::Relaxed);
            LB_DOWN_X.store(x as isize, Ordering::Relaxed);
            LB_DOWN_Y.store(y as isize, Ordering::Relaxed);
            if MENU_OPEN.load(Ordering::Relaxed) {
                match l.0 {
                    0 => drag_step(st, x, y),
                    1 => drag_end(st, x, y),
                    _ => {} // 2 = 只挪光标（供测试点取消按钮）
                }
            }
            LRESULT(0)
        }
        WM_APP_RHOLDSIM => {
            // 仅测试：驱动右键长按状态机（见常量处的说明）。
            let ev = match w.0 {
                0 => Some(WM_RBUTTONDOWN),
                1 => Some(WM_RBUTTONUP),
                _ => None,
            };
            let mut consumed = 0usize;
            if let Some(m) = ev {
                if unsafe { rhold_step(m) }.is_some() {
                    consumed = 1;
                }
            }
            let st = (if R_DOWN_AT.load(Ordering::Relaxed) != 0 { 1usize } else { 0 }) << 1
                | (if R_DOWN_INMENU.load(Ordering::Relaxed) { 1 } else { 0 }) << 2
                | (if MENU_OPEN.load(Ordering::Relaxed) { 1 } else { 0 }) << 5
                | ((REINJECT_POSTED.load(Ordering::Relaxed) as usize) & 0xff) << 8
                | ((R_DOWN_SEEN.load(Ordering::Relaxed) as usize) & 0xff) << 16
                | ((MAGIC_SEEN.load(Ordering::Relaxed) as usize) & 0x7f) << 24
                | ((INJECT_DONE.load(Ordering::Relaxed) as usize) & 0x7f) << 31;
            LRESULT((consumed | st) as isize)
        }
WM_APP_DISMISS_FG => {
            // 长按右键呼出我们菜单时，被「放行」的那个 UP 会让前台程序弹出它自己
            // 的右键菜单 —— 这里补一次 Esc 把它关掉，免得我们的菜单背后挂着别人的菜单。
            //
            // ⚠️ 用 `PostMessage(WM_KEYDOWN/WM_KEYUP)` 这种**窗口消息**，
            // 不要用 `keybd_event`/`SendInput` 那种**输入注入**：
            // 本进程装着全局钩子时注入不可靠（可能永久卡住调用线程）。
            // 窗口消息走的是应用自己的消息队列，安全、异步、失败也无害。
            //
            // 也**不要**在钩子回调里做 —— 那里只许原子操作 + PostMessage。
            let fg = FG_HWND.load(Ordering::Relaxed);
            if fg != 0 {
                let h = HWND(fg as *mut c_void);
                unsafe {
                    let _ = PostMessageW(h, WM_KEYDOWN, WPARAM(0x1B), LPARAM(0));
                    let _ = PostMessageW(h, WM_KEYUP, WPARAM(0x1B), LPARAM(0));
                }
            }
            LRESULT(0)
        }
        WM_APP_CANCEL => {
            let st = state_of(hwnd);
            if MENU_OPEN.load(Ordering::Relaxed) && !CTX_OPEN.load(Ordering::Relaxed) {
                // 右键 / 点空白 / Esc = **一律收起菜单**。
                // 曾经这里会「右键命中胶囊 → 呼出改名/换色菜单」——
                // 那是用户明确否掉的方案（2026-10-07：「去掉换色去掉菜单，
                // 双击直接就在胶囊里改名」）。改名只有一条路：`WM_APP_CLICK`
                // 里 `chip_click()` 判定双击 → 进入 `editing` 就地编辑。
                unlock(st);
                close_menu_now(st);
            }
            LRESULT(0)
        }
        WM_APP_THUMBSIM => {
            let hw = w.0 as isize;
            unsafe { thumb_apply(hw, (160, 160)) };
            LRESULT(0)
        }
        WM_APP_WHEEL => {
            // 滚轮一格：内环**连续滚动**（只在一屏装不下时才生效）。
            // `w` = 原始增量（钩子把 `mouseData` 高 16 位当 u16 传过来，这里还原成 i16）。
            let st = state_of(hwnd);
            let d = (w.0 as u16) as i16;
            // 一格 = `WHEEL_DELTA`(120) = 正好转一个图标；触摸板的小增量自然变成连续滚。
            ring_scroll(st, -(d as f32 / 120.0));
            LRESULT(0)
        }
        WM_APP_SCROLLSIM => {
            // 仅测试：把偏移**直接**设成 `w / 100`（不动画），
            // 用来回归「滚到第 N 页之后命中和绘制还对不对」。
            let st = state_of(hwnd);
            let mx = ring_off_max(eff_apps(st));
            let to = ((w.0 as f32) / 100.0).clamp(0.0, mx);
            st.ring_off = to;
            st.ring_to = to;
            st.hit_app = None;
            st.hover_now = None;
            scroll_prune(st);
            LRESULT(0)
        }
        WM_APP_HITTEST => {
            // 仅测试：走**真实**命中路径（含滚动偏移、淡出内沉），回报 `hit_app + 1`。
            let st = state_of(hwnd);
            update_hover(st, w.0 as i32, l.0 as i32);
            LRESULT(st.hit_app.map(|i| i as isize + 1).unwrap_or(0))
        }
        WM_APP_APPPOS => {
            let st = state_of(hwnd);
            let i = w.0;
            let n = eff_apps(st);
            if n == 0 || i >= n {
                return LRESULT(0);
            }
            // 回报**显示位置**（含滚动偏移与淡出内沉），并且淡到看不见的格子返回 0 ——
            // 测试据此判断「这一格不在当前页上」，别去点一个隐形的图标。
            let s = slot_of(st, i);
            let a = slot_appear(s, n);
            if a < 0.5 {
                return LRESULT(0);
            }
            let (r, _icon) = ring_layout(n);
            let (dx, dy) = polar(r - SCROLL_SINK * (1.0 - a), slot_angle(s, n));
            let sx = (st.win_x + dx as i32).clamp(0, 65535);
            let sy = (st.win_y + dy as i32).clamp(0, 65535);
            return LRESULT(((sy as isize) << 16) | (sx as isize));
        }
        WM_APP_CHIPPOS => {
            // 仅测试：回报**第 `w` 号**胶囊中心的屏幕坐标。
            // 单击/拖拽路径都靠真实光标命中，自动化必须知道胶囊在哪；
            // 外环是极坐标形状（扇形段），坐标算不准就点不到/拖不中 —— 所以由程序侧给出。
            // 下标越界时返回 0（测试据此判断「没有这个段」）。
            let st = state_of(hwnd);
            let want = w.0;
            let mut packed: usize = 0;
            if let Some(ai) = st.active_app.filter(|&a| a < st.apps.len()) {
                let (segs, _rots, rads) = chip_layout(st, ai);
                if want < segs.len() {
                    let s = segs[want];
                    let (cx, cy) = polar(rads[want], (s.0 + s.1) * 0.5);
                    let sx = (cx as i32 + st.win_x).clamp(0, 65535);
                    let sy = (cy as i32 + st.win_y).clamp(0, 65535);
                    packed = ((sy as usize) << 16) | (sx as usize);
                }
            }
            LRESULT(packed as isize)
        }
        WM_APP_DUMP => {
            let st = state_of(hwnd);
            println!(
                "[dump] open={} active={:?} chip={:?} loading={:?} mood={} win=({},{})",
                MENU_OPEN.load(Ordering::Relaxed),
                st.active_app,
                st.hover_chip,
                st.loading,
                st.mood,
                st.win_x,
                st.win_y
            );
            if !st.dbi_bits.is_null() {
                let n = (CANVAS * CANVAS * 4) as usize;
                let buf = std::slice::from_raw_parts(st.dbi_bits, n);
                let path = format!("{}\\live_dump.bmp", exe_dir());
                let ok = save_bmp(&path, buf, CANVAS, CANVAS);
                println!("[dump] {} {}", if ok { "OK" } else { "FAIL" }, path);
            }
            LRESULT(0)
        }
        WM_APP_DEBUGPOS => {
            // 自动化测试：w = x, l = y
            let st = state_of(hwnd);
            if MENU_OPEN.load(Ordering::Relaxed) {
                CURSOR_X.store(w.0 as i32, Ordering::Relaxed);
                CURSOR_Y.store(l.0 as i32, Ordering::Relaxed);
                update_hover(st, w.0 as i32, l.0 as i32);
            }
            LRESULT(0)
        }
        WM_APP_DROPTEST => {
            let st = state_of(hwnd);
            let hit = drop_force_app(st, w.0 as i32, l.0 as i32);
            println!("[droptest] ({}, {}) -> {}", w.0 as i32, l.0 as i32,
                match hit { Some(i) => i.to_string(), None => "None".into() });
            LRESULT(0)
        }
        WM_TIMER => {
            if w.0 as usize == TIMER_FRAME {
                let st = state_of(hwnd);
                on_timer(st);
            } else if w.0 as usize == TIMER_CLICK {
                // 「延迟单击」到点 = 确认没有第二击 → 真的执行
                let st = state_of(hwnd);
                if let Some((ai, j)) = st.pending_click.take() {
                    let _ = KillTimer(hwnd, TIMER_CLICK);
                    // 单击的落点在**按下那一刻**就定了（存在 pending_click 里），这里只该
                    // 复核「菜单还在不在 / 有没有被第二击改成改名」。
                    //
                    // ⚠️⚠️ **不要**再用 hover 复核落点：网址要联网查 1~2 秒，用户点完自然会把
                    // 鼠标移开，hover_chip 变 None → 单击被悄悄丢弃，表现就是
                    // 「点完鼠标移出梯形，既不 loading 也不跳转」（用户 2026-10-09 报）。
                    if MENU_OPEN.load(Ordering::Relaxed) && st.editing.is_none() {
                        st.active_app = Some(ai);
                        st.hover_chip = Some(j);
                        commit(st);
                    }
                }
            } else if w.0 as usize == TIMER_EXEMPT {
                // 「例外名单」常驻刷新：每 250ms 重算一次前台进程 → EXEMPT，
                // 与渲染帧定时器**无关**，菜单关掉 / 静止（帧定时器已 Kill）也照刷。
                // 否则 EXEMPT 冻结在旧值，表现正是用户 2026-10-11 报的
                // 「黑名单里能开一次轮盘、之后切到任何应用都打不开」。
                let st = state_of(hwnd);
                update_exempt(st);
                // 空闲工作集裁剪也挂这里 —— ⚠️ **不能挂 `on_timer`**：`TIMER_FRAME`
                // 在菜单收起时被 `KillTimer` 了（静止不重绘就不跑），裁剪永远不会触发。
                // `TIMER_EXEMPT` 只设一次永不杀，菜单关着也在走。
                //
                // 判据要齐：`MENU_OPEN` 管可见性、`hiding` 管淡出动画、`locked` 管
                // 常驻锁定、`bub_a` 管气泡淡入、`DRAG_ACTIVE` 管 OLE 拖放 ——
                // 少一条就会在正画的时候裁剪、当前帧的页被丢回去 → 反而卡顿。
                if st.trim_at > 0.0 && now_s() >= st.trim_at {
                    st.trim_at = 0.0;
                    if !MENU_OPEN.load(Ordering::Relaxed)
                        && !st.hiding
                        && !st.locked
                        && st.bub_a <= 0.01
                        && !DRAG_ACTIVE.load(Ordering::Relaxed)
                        && st.window_capture.is_none()
                        && st.pending_click.is_none()
                    {
                        trim_working_set();
                    }
                }
                // 在线升级的**周期**检查也挂这条定时器：常驻进程可能十天不重启，
                // 只在启动时查一次就永远等不到发版（用户 2026-10-27 要「可以每天检查」）。
                // ⚠️ 三道闸缺一不可：`update_st == 1` = 已经有一发在跑（别叠）；
                //    `MENU_OPEN`/`hiding` = 正呼出 / 正淡出时绝不插队
                //    （`start_update_check` 要在 UI 线程同步写一次 ini）。
                if st.update_auto
                    && st.update_st != 1
                    && !MENU_OPEN.load(Ordering::Relaxed)
                    && !st.hiding
                    && unix_now() - st.update_last_check >= update_min_interval()
                {
                    start_update_check(st, false);
                }
            }
            LRESULT(0)
        }
        WM_APP_TRAY => {
            let lp = l.0 as u32;
            if lp == WM_RBUTTONUP || lp == WM_LBUTTONUP {
                let st = state_of(hwnd);
                let menu = CreatePopupMenu().unwrap_or(HMENU(ptr::null_mut()));
                let t0: Vec<u16> = wide("设置…");
                let _ = AppendMenuW(menu, mif(false), MENU_SETTINGS as usize, PCWSTR(t0.as_ptr()));
                let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
                let t1: Vec<u16> = wide(if st.running { "停止监听" } else { "开启监听" });
                let _ = AppendMenuW(menu, mif(false), MENU_TOGGLE as usize, PCWSTR(t1.as_ptr()));
                let t2: Vec<u16> = wide("常驻模式（松开后保持）");
                let _ = AppendMenuW(menu, mif(st.persistent), MENU_PERSIST as usize, PCWSTR(t2.as_ptr()));
                let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
                let t7: Vec<u16> = wide("退出");
                let _ = AppendMenuW(menu, mif(false), MENU_QUIT as usize, PCWSTR(t7.as_ptr()));

                let mut p = POINT::default();
                let _ = GetCursorPos(&mut p);
                let _ = SetForegroundWindow(hwnd);
                let cmd = TrackPopupMenu(
                    menu,
                    TPM_RETURNCMD | TPM_RIGHTBUTTON,
                    p.x,
                    p.y,
                    0,
                    hwnd,
                    None,
                );
                let _ = DestroyMenu(menu);
                let _ = PostMessageW(hwnd, WM_NULL, WPARAM(0), LPARAM(0));

                let st = state_of(hwnd);
                match cmd.0 as u16 {
                    MENU_SETTINGS => open_settings(st),
                    MENU_TOGGLE => {
                        let on = !st.running;
                        set_running(st, on);
                        save_settings(st);
                    }
                    MENU_PERSIST => {
                        st.persistent = !st.persistent;
                        save_settings(st);
                        println!("[*] 常驻模式：{}", if st.persistent { "开" } else { "关" });
                    }
                    MENU_QUIT => unsafe { quit_app() },
                    _ => {}
                }
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            remove_tray_icon(hwnd);
            let _ = UnregisterHotKey(hwnd, HOTKEY_TOGGLE);
            unsafe { quit_app() };
        }
        _ => DefWindowProcW(hwnd, msg, w, l),
    }
}

// ═════════════════════ 设置窗口（GDI 自绘 · 扁平 piik） ═════════════════════
//
// 为什么全部自绘、不用子控件：整机视觉是黑白灰扁平语言，主题化的原生控件
// （按钮/滚动条/复选框）会立刻把风格破掉。这里所有控件都用 GDI RoundRect +
// DrawTextW 画在**窗口 DC** 上 —— 窗口 DC 上 GDI 是完全可靠的，不存在
// 32bpp 内存 DIB 那套 Pie/AngleArc/alpha 的坑。
// 命中判定与绘制共用同一套 rect 计算函数（和 chip_layout 同一个道理）。

const S_W: i32 = 780;
/// ⚠️ 2026-10-14：交互页新增第 5 个开关「显示应用名称」→ 开关区多占 44px。
/// 卡片底固定在 `S_H - 14`，若不抬窗口高度，黑白名单卡片会从 154px 缩到 110px ——
/// 而它要放「头 42 + 3 行 ×26 = 120」+ 一行「…还有 N 项」，**第 3 行会被切掉**。
/// 开关行距 44 是「统一间距 ACT_GAP」派生出来的铁律，不能压；所以抬窗口。
const S_H: i32 = 670;
const S_NAV_W: i32 = 172;
const S_TITLE_H: i32 = 52;
const S_X0: i32 = S_NAV_W + 26;      // 内容区左边界
const S_X1: i32 = S_W - 26;          // 内容区右边界
// ⚠️ 列表顶与「页标题底 + ACT_GAP」对齐 —— 四页共用同一套纵向节奏（见 `ACT_*`），
//    否则切页时标题会上下跳几像素。`S_LIST_TOP` 一旦改，`test_settings.py` 里的
//    镜像常量必须同步（那边有硬编码）。
const S_LIST_TOP: i32 = ACT_TOP_Y + ACT_LABEL_H + ACT_GAP;
const S_LIST_BOT: i32 = S_H - 78;

// ── 渲染管线：整页先在 SS 倍大的 DIB 上用 GDI 画，再 SS×SS 盒式降采样 ──
//
// 为什么必须这样：**GDI 的 RoundRect / Ellipse 完全没有抗锯齿**，直接画到
// 窗口上就是楼梯（chip 圆角、开关胶囊、导航胶囊全是一格一格的）；
// 文字还会走 ClearType 出彩色边（放大看「鼠标中键」边上全是橙蓝锯齿）。
// 先在 3× 画再平均，等于给整页做了一次 AA —— 彩色边也在平均里被抹成灰阶，
// 和环那边「2× 采样 + SDF 覆盖率」是同一个思路。
// 超采样倍数。⚠️ **直接决定设置页那块画布的大小**（`S_WW*SS × S_HH*SS × 4`）：
//SS=3 → 20.2 MB，SS=2 → 9.0 MB。降采样是 N×N 盒式平均，2×2 对设置页那些
// 实心圆角矩形已经够平滑（圆弧都是 SDF 画的，边缘质量不靠超采样撑）。
// 想再省可以到SS=1（不超采样），但那时圆角会看出锯齿 —— 别为了省内存牺牲观感。
const SS: i32 = 2;
/// 卡片外留一圈画投影；窗口 = 卡片 + 四周各 S_M
const S_M: i32 = 22;
const S_WW: i32 = S_W + S_M * 2;
const S_HH: i32 = S_H + S_M * 2;
const CARD_RAD: i32 = 18;
/// ⚠️ windows-rs 0.58 里 `WM_MOUSELEAVE` 在 `Win32::UI::Controls`，**不在** WindowsAndMessaging。
/// 直接写 `WM_MOUSELEAVE` 编译不报错 —— match 里的未知标识符会**静默变成匹配一切的绑定**，
/// 于是这条臂把 WM_NCCREATE 也吃掉，而那时 USERDATA 还没设 → state_of() 空指针 → 进程崩在回调里。
/// 所以这里显式给个自带名字的常量，别依赖导入。
const WM_MOUSELEAVE_MSG: u32 = 0x02A3;

/// 逻辑坐标（卡片左上为 0,0）→ hi DIB 的设备坐标
#[inline]
fn sxv(v: i32) -> i32 {
    (v + S_M) * SS
}

/// 呼出键开关。钩子跑在**另一个线程**上，读不到 State，所以用原子量。
static OPT_MIDDLE: AtomicBool = AtomicBool::new(true);
static OPT_SIDE: AtomicBool = AtomicBool::new(true);
static OPT_RHOLD: AtomicBool = AtomicBool::new(false);
// ── 自定义触发（钩子线程只读这些原子量，不跨线程读 `st`）──
static OPT_CUSTOM_MOUSE: AtomicU8 = AtomicU8::new(0); // 0 无 / 1..5 鼠标键
static OPT_CUSTOM_VK: AtomicU32 = AtomicU32::new(0);   // 0 无 / 键盘 vk
static OPT_CUSTOM_MODS: AtomicU8 = AtomicU8::new(0);   // 修饰键掩码
// 录制态：钩子抓到下一次输入后写入下面几个，再 post WM_APP_TRIGCAP
static CAPTURE_TRIGGER: AtomicBool = AtomicBool::new(false);
static CAP_KIND: AtomicU8 = AtomicU8::new(0);
static CAP_BTN: AtomicU8 = AtomicU8::new(0);
static CAP_VK: AtomicU32 = AtomicU32::new(0);
static CAP_MODS: AtomicU8 = AtomicU8::new(0);
/// 录制键盘组合时，**实时累积**的修饰键掩码：由钩子收到的每个修饰键
/// 按下/松开事件维护，比 `GetAsyncKeyState` 更可靠（不受同批次输入的时序影响，
/// 不会把 Ctrl+W 录成单独的 W）。仅在 CAPTURE_TRIGGER 期间有意义。
static CAP_MODS_LIVE: AtomicU8 = AtomicU8::new(0);
/// 录制态内按下的「无效键」：1 左键 / 2 右键（右键=长按右键专属，左键=全局点击会破坏正常操作）。
/// 仅用于 UI 显式反馈「不可用作触发键」，不落盘、不结束录制态。
static CAP_BAD: AtomicU8 = AtomicU8::new(0);
/// 右键按下的 tick（长按判定），0 = 当前没按着。
/// **非 0 = 我们吞掉了这次 RBUTTONDOWN，欠应用一个交代** ——
/// 这个「欠账」必须由配对的 RBUTTONUP 收尾（补发 or 呼出菜单）。
static R_DOWN_AT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
/// 吞掉那次 RBUTTONDOWN 时，菜单是否已经开着。
/// 用途：松开时判断这次右键是「用户在关菜单」还是「用户想点桌面」——
/// 前者不补发（否则右键会透传到桌面弹出资源管理器菜单），后者才补发。
static R_DOWN_INMENU: AtomicBool = AtomicBool::new(false);
/// 诊断计数：一共发出过几次「补发右键」请求。自动化据此验证配对是否成立。
static REINJECT_POSTED: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
/// 诊断计数：一共吞掉过几次 RBUTTONDOWN。
static R_DOWN_SEEN: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
/// 长按右键那一刻的前台窗口 —— 稍后要给它补一次 Esc 关掉它的右键菜单。
static FG_HWND: AtomicIsize = AtomicIsize::new(0);
/// 诊断：钩子见到多少个「带 RL_MAGIC 标记」的事件（应为补发次数×2）。
static MAGIC_SEEN: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
/// 诊断：补发请求实际执行完的次数。
static INJECT_DONE: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
static SET_HWND: AtomicIsize = AtomicIsize::new(0);
/// 长按右键的判定门槛（毫秒）
const RHOLD_MS: u32 = 380;
/// 给「我们自己补发的合成鼠标事件」打的标记（写进 dwExtraInfo），
/// 钩子里据此放行，避免补发事件被自己再吞一次形成死循环
const RL_MAGIC: usize = 0x524C_4D31; // 'RLM1'

/// 设置页控件标识 —— 绘制与命中共用同一个枚举，杜绝「画在这、点在那」
#[derive(Clone, Copy, PartialEq)]
enum Ctl {
    None,
    Close,
    Nav(usize),
    Theme(usize),
    /// 外观页「材质」：0 纸面 / 1 液态玻璃
    Mat(usize),
    Key(usize),
    TrigCapture,         // 交互页「呼出方式」下的「自定义触发」行：点击进入录制态（再点取消）
    TrigClear,           // 自定义触发已设置时的「✕」清除按钮
    TrigCancel,          // **录制中**的「取消录制」按钮（否则一进录制就只能录完）
    Sw(usize),
    AppHead(usize),
    AppDel(usize),
    ItemDel(usize, usize),
    AddApp,
    /// 删除确认小卡上的两颗按钮（自绘模态；点 ✕ 不再直接删 —— 用户 2026-10-27）
    ConfYes,
    ConfNo,
    /// 条目行末尾的入口胶囊：`0` = 选文件，`1` = 选文件夹。
    /// ⚠️ 必须是**两颗** —— 原生对话框里「选文件」和「选文件夹」是两种模式
    ///    （`FOS_PICKFOLDERS` 一开，文件就变灰不可选），没有「两个都能选」的现代对话框。
    ///    用户 2026-10-27：「文件夹应该是能添加目录的而不是只能添加文件」。
    AddItem(usize, u8),
    /// 交互页行为区的「开机自启」（状态 = `st.autostart`，同步到注册表）
    Autostart,
    /// 标题栏「查看引导」入口（重播聚光灯引导）
    GuideEntry,
    // ── 交互页：黑白名单（二选一生效）──
    ExcCard(u8),          // 0 黑名单 / 1 白名单 —— 点对应卡片的标题（卡片头）切到该名单为当前生效方
    ExcDel(u8, usize),    // 删**当前生效**卡片里的第 i 条
    ExcPlus(u8),          // 当前生效卡片的「＋」= 选一个 exe 加进去
    // ── 交互页：名字右边的「?」→ 悬停出 tooltip ──
    Tip(u8),              // 1 常驻 2 监听 3 全屏放行 4 黑名单 5 白名单
    /// 关于页「检查更新」按钮
    UpdateCheck,
}

// ══ 交互页几何（绘制与命中共用，别写第二份）══
//
// ⚠️⚠️ **整页纵向节奏只在这里定义一次**，全部由 `ACT_GAP` 派生 ——
//     不要再往各段硬塞 magic number。之前就是各写各的：
//     「呼出方式」标题下面空 40px、「开机自启」下面只空 5px，一眼就不齐
//     （用户 2026-10-13：「呼出方式下面间距大了，开机自启下面间距小了」）。
//
// 自上而下：呼出方式标题 → 胶囊行 → 自定义触发提示行 →（空 ACT_GAP）→
//           行为标题 → 4 个开关 →（空 ACT_GAP）→ 黑白名单标题 → 两张卡片
//
// ⚠️ 这页比别的页挤（4 个开关 + 两张卡片），所以开关行另起一套 `act_sw_*`，
//    不能复用 `s_sw_row`（那是 62 间距的）。
/// 外观页：「材质」段（第二行）
const LOOK_MAT_LABEL_Y: i32 = ACT_TOP_Y + ACT_LABEL_H + ACT_GAP + ACT_CHIPS_H + ACT_GAP;
const LOOK_MAT_Y: i32 = LOOK_MAT_LABEL_Y + ACT_LABEL_H + ACT_GAP;
const ACT_GAP: i32 = 16;       // **统一间距**：段标题↔内容、内容↔下一段标题 都用它
const ACT_LABEL_H: i32 = 22;   // 段标题行高

const ACT_TOP_Y: i32 = S_TITLE_H + 14;                       // 呼出方式 标题顶
const ACT_CHIPS_Y: i32 = ACT_TOP_Y + ACT_LABEL_H + ACT_GAP;  // 胶囊行顶
const ACT_CHIPS_H: i32 = 38;
/// 自定义触发的提示行 —— 与胶囊行**同属一块**，所以只隔 4px
const TRIG_ROW_Y: i32 = ACT_CHIPS_Y + ACT_CHIPS_H + 4;
const ACT_TRIG_H: i32 = 30;
const ACT_SEC2_Y: i32 = TRIG_ROW_Y + ACT_TRIG_H + ACT_GAP;   // 行为 标题顶
const ACT_SW_Y0: i32 = ACT_SEC2_Y + ACT_LABEL_H + ACT_GAP + 8; // 开关 y（行矩形 = y-8..y+34）
const ACT_SW_DY: i32 = 44;     // 行距 44 - 内容高 28 ≈ 视觉空隙 16，与 ACT_GAP 一致
/// 开关里最后一行的底边（5 行：常驻 / 监听 / 全屏放行 / 开机自启 / 显示应用名称）
const ACT_SW_LAST_BOT: i32 = ACT_SW_Y0 + 4 * ACT_SW_DY + 34;

fn act_sw_y(i: usize) -> i32 {
    ACT_SW_Y0 + i as i32 * ACT_SW_DY
}

fn act_sw_rect(i: usize) -> RECT {
    s_sw_rect_at(act_sw_y(i))
}

fn act_sw_row(i: usize) -> RECT {
    let y = act_sw_y(i);
    RECT { left: S_X0, top: y - 8, right: S_X1, bottom: y + 34 }
}

fn trig_row_rect() -> RECT {
    RECT { left: S_X0, top: TRIG_ROW_Y, right: S_X1, bottom: TRIG_ROW_Y + ACT_TRIG_H }
}
fn trig_clear_rect() -> RECT {
    RECT { left: S_X1 - 30, top: TRIG_ROW_Y + 6, right: S_X1 - 8, bottom: TRIG_ROW_Y + 28 }
}

/// 录制中的「取消录制」按钮（详情行右端）。
/// ⚠️ 必须有这个出口：录制的抓手是「下一次输入」，按 Esc 反而会被当成触发键录走 ——
///    没有它就「一旦开始录制就必须录完」（用户 2026-10-13 报）。
fn trig_cancel_rect() -> RECT {
    RECT { left: S_X1 - 88, top: TRIG_ROW_Y + 5, right: S_X1 - 8, bottom: TRIG_ROW_Y + 29 }
}

/// 自定义触发芯片上的文案（呼出方式区第 4 个按钮）：
/// 未设置=「自定义」；录制中=「录制中…」；已设置=「✓ <绑定>」。
fn trig_chip_label(st: &State) -> String {
    if st.trig_capturing {
        "录制中…".to_string()
    } else if st.trig_custom_kind == 1 {
        format!("✓ {}", mouse_name(st.trig_custom_btn))
    } else if st.trig_custom_kind == 2 {
        format!("✓ {}", combo_str(st.trig_custom_mods, st.trig_custom_vk))
    } else {
        "自定义".to_string()
    }
}

fn mouse_name(btn: u8) -> &'static str {
    match btn {
        1 => "鼠标左键", 2 => "鼠标右键", 3 => "鼠标中键",
        4 => "鼠标侧键X1", 5 => "鼠标侧键X2", _ => "鼠标键",
    }
}

/// 键盘组合键的可读名（如 "Ctrl+W" / "Ctrl+Alt+K"）
fn combo_str(mods: u8, vk: u32) -> String {
    let mut s = String::new();
    if mods & 1 != 0 { s.push_str("Ctrl+"); }
    if mods & 2 != 0 { s.push_str("Alt+"); }
    if mods & 4 != 0 { s.push_str("Shift+"); }
    if mods & 8 != 0 { s.push_str("Win+"); }
    s.push_str(&vk_name(vk));
    s
}

/// 虚拟键码 → 可读名（覆盖常见键；未知回退「键0xXX」）
fn vk_name(vk: u32) -> String {
    match vk {
        0x08 => "Backspace".into(),
        0x09 => "Tab".into(),
        0x0D => "Enter".into(),
        0x1B => "Esc".into(),
        0x20 => "Space".into(),
        0x21 => "PgUp".into(),
        0x22 => "PgDn".into(),
        0x23 => "Home".into(),
        0x24 => "End".into(),
        0x25 => "←".into(),
        0x26 => "↑".into(),
        0x27 => "→".into(),
        0x28 => "↓".into(),
        0x30..=0x39 => format!("{}", (vk as u8 - 0x30) as char),
        0x41..=0x5A => format!("{}", vk as u8 as char),
        0x70..=0x7B => format!("F{}", vk - 0x6F),
        0x5B | 0x5C => "Win".into(),
        _ => format!("键0x{:02X}", vk),
    }
}

/// 行为区「?」的位置（固定 x，长度不同的标签也能对齐）
fn act_tip_rect(i: usize) -> RECT {
    let y = act_sw_y(i);
    RECT { left: S_X0 + 132, top: y + 2, right: S_X0 + 150, bottom: y + 20 }
}

// ⚠️ 全部由 `ACT_*` 的节奏派生（别再写死）：标题顶 = 上一段底 + ACT_GAP；
//    卡片顶 = 标题底 + ACT_GAP。卡片底贴到页面下沿内 14px。
const EXC_CARDS_TITLE_Y: i32 = ACT_SW_LAST_BOT + ACT_GAP;
const EXC_CARD_TOP: i32 = EXC_CARDS_TITLE_Y + ACT_LABEL_H + ACT_GAP;
const EXC_CARD_BOT: i32 = S_H - 14;
const EXC_CARD_GAP: i32 = 20;

/// 卡片头（标题区，约顶部 42px）：点它 = 切到该名单为当前生效方（二选一）。
fn exc_head_rect(c: usize) -> RECT {
    let r = exc_card_rect(c);
    RECT { left: r.left, top: r.top, right: r.right, bottom: r.top + 42 }
}

fn exc_card_rect(c: usize) -> RECT {
    let w = (S_X1 - S_X0 - EXC_CARD_GAP) / 2;
    let l = S_X0 + c as i32 * (w + EXC_CARD_GAP);
    RECT { left: l, top: EXC_CARD_TOP, right: l + w, bottom: EXC_CARD_BOT }
}

fn exc_tip_rect(c: usize) -> RECT {
    let r = exc_card_rect(c);
    RECT { left: r.left + 64, top: r.top + 13, right: r.left + 82, bottom: r.top + 31 }
}

fn exc_row_rect(c: usize, i: usize) -> RECT {
    let r = exc_card_rect(c);
    let t = r.top + 42 + i as i32 * 26;
    RECT { left: r.left + 8, top: t, right: r.right - 8, bottom: t + 24 }
}

fn exc_del_rect(c: usize, i: usize) -> RECT {
    let rr = exc_row_rect(c, i);
    RECT { left: rr.right - 20, top: rr.top, right: rr.right, bottom: rr.bottom }
}

/// 落点 → 卡片（0 黑 / 1 白）；不在任何卡片里 → None。
/// 绘制/命中/拖放的落点判定**共用这一份**。
fn card_at(px: i32, py: i32) -> Option<usize> {
    (0..2usize).find(|&c| in_rect(exc_card_rect(c), px, py))
}

fn exc_plus_rect(c: usize) -> RECT {
    let r = exc_card_rect(c);
    RECT { left: r.left + 8, top: r.bottom - 34, right: r.right - 8, bottom: r.bottom - 8 }
}

/// 卡片里最多画几行（放不下的只提示数量）
const EXC_ROWS: usize = 3;

// ── tooltip 文案（每个「?」一句；自己控制换行，项目里没有自动折行）──
fn tip_lines(id: u8) -> &'static [&'static str] {
    match id {
        1 => &["松开呼出键后菜单不消失。", "左键点选或回车执行，Esc / 点空白处关闭。"],
        2 => &["关掉后完全不挂钩子，呼出键失效。", "（省电；或临时让给别的软件）"],
        3 => &["前台程序占满整个屏幕时不拦截鼠标。", "游戏 / 视频 / 远程桌面常需要。"],
        4 => &["选「黑名单」模式时：列表里的程序呼出键不生效。", "拖 exe 进来添加；点卡片标题切换当前生效的是哪张。"],
        5 => &["选「白名单」模式时：只有列表里的程序呼出键才生效。", "拖 exe 进来添加；点卡片标题切换当前生效的是哪张。"],
        6 => &["鼠标悬停内环图标时，用头像旁的气泡显示它的应用名。", "关掉就只看图标（操作结果提示不受影响）。"],
        _ => &[],
    }
}

fn tip_anchor(id: u8) -> (i32, i32) {
    match id {
        1..=3 => {
            let r = act_tip_rect(id as usize - 1);
            (r.left, r.bottom + 6)
        }
        4 => {
            let r = exc_tip_rect(0);
            (r.left, r.bottom + 6)
        }
        5 => {
            let r = exc_tip_rect(1);
            (r.left, r.bottom + 6)
        }
        6 => {
            let r = act_tip_rect(4);
            (r.left, r.bottom + 6)
        }
        _ => (0, 0),
    }
}

// ── 应用页的行（随展开状态/滚动量变化，绘制与命中共用） ──
struct SRow {
    kind: u8, // 0 应用 1 条目 2 「＋ 添加条目」
    ai: usize,
    ii: usize,
    y: i32,
    h: i32,
}

fn s_rows(st: &State) -> (Vec<SRow>, i32) {
    let mut v = Vec::new();
    let mut y = 0i32;
    for (ai, a) in st.apps.iter().enumerate() {
        v.push(SRow { kind: 0, ai, ii: 0, y, h: 42 });
        y += 42;
        if st.set_expanded == ai {
            for ii in 0..a.items.len() {
                v.push(SRow { kind: 1, ai, ii, y, h: 32 });
                y += 32;
            }
            v.push(SRow { kind: 2, ai, ii: 0, y, h: 32 });
            y += 32;
        }
        y += 6;
    }
    (v, y)
}

fn s_scroll_max(st: &State) -> i32 {
    let (_, total) = s_rows(st);
    (total - (S_LIST_BOT - S_LIST_TOP)).max(0)
}

fn s_nav_rect(i: usize) -> RECT {
    let t = S_TITLE_H + 18 + i as i32 * 44;
    RECT { left: 14, top: t, right: S_NAV_W - 14, bottom: t + 40 }
}

fn s_row_rect(y: i32, h: i32) -> RECT {
    RECT { left: S_X0, top: y, right: S_X1, bottom: y + h - 6 }
}

/// 条目行、「＋ 网址」小胶囊、内联输入框的左边界 —— 三者必须同一列
const S_ADD_X: i32 = S_X0 + 26;
/// 小胶囊/输入框的高度（比 32 的行高矮一截，上下各留 4px 才不像一整行）
const S_PILL_H: i32 = 24;

/// 条目行末尾有哪几颗入口胶囊 —— 浏览器只要「＋ 网址」，其它应用「文件 / 文件夹」都要。
/// 顺序即 `Ctl::AddItem` 的 mode，绘制与命中都按它走。
fn add_pill_labels(web: bool) -> &'static [&'static str] {
    if web {
        &["＋ 网址"]
    } else {
        &["＋ 文件", "＋ 文件夹"]
    }
}

/// 第 `m` 颗小胶囊的矩形：从 `S_ADD_X` 起按文字宽度依次排开，颗间留 8px。
/// 绘制和命中都走这里（量宽要个 DC，命中那边传 `st.txt_dc`），
/// 否则 hover 高亮会圈在画出来的框外面。
unsafe fn s_add_pill(dc: HDC, st: &State, ry: i32, web: bool, m: usize) -> RECT {
    let ls = add_pill_labels(web);
    let mut x = S_ADD_X;
    for k in 0..m.min(ls.len()) {
        x += s_text_w(dc, ls[k], st.sf_small) + 20 + 8;
    }
    let w = s_text_w(dc, ls.get(m).copied().unwrap_or(""), st.sf_small) + 20;
    RECT { left: x, top: ry + 4, right: x + w, bottom: ry + 4 + S_PILL_H }
}

/// 内联输入框：点中小胶囊之后就位，宽度放开到整个内容区（网址比按钮长得多）
fn s_add_input(ry: i32) -> RECT {
    RECT { left: S_ADD_X, top: ry + 4, right: S_X1, bottom: ry + 4 + S_PILL_H }
}

// ── 删除确认小卡（自绘模态，画在内容区正中）──

const S_CONF_W: i32 = 340;
const S_CONF_H: i32 = 116;

/// 卡片本体：在**内容区**（侧栏右边那块）水平居中、垂直偏上 40%
fn s_conf_rect() -> RECT {
    let l = S_NAV_W + ((S_W - S_NAV_W - S_CONF_W) / 2).max(8);
    let t = (S_H * 2 / 5 - S_CONF_H / 2).max(S_TITLE_H + 12);
    RECT { left: l, top: t, right: l + S_CONF_W, bottom: t + S_CONF_H }
}

/// 卡开着 = 模态：整块画布压暗一档，让卡从页面里「浮」出来。
/// GDI 没有半透明填充，但分层窗口的像素就是我们自己的 DIB —— 直接按位乘，
/// 和聚光灯引导用的是同一招（见 `guide_draw`）。
/// ⚠️ 必须在**降采样之前**调用（否则圆角和文字的抗锯齿会被这趟遍历毁掉）；
///    卡本体是在这之后才画的不透明实心，所以整块压暗不会伤到它。
unsafe fn conf_dim(st: &State) {
    if st.s_hi_bits.is_null() {
        return;
    }
    let mut lut = [0u8; 256];
    for v in 0..256usize {
        lut[v] = (v * 45 / 100) as u8;
    }
    let w = (S_WW * SS) as usize;
    let h = (S_HH * SS) as usize;
    let bits = st.s_hi_bits;
    for y in 0..h {
        let row = y * w;
        for x in 0..w {
            let o = (row + x) * 4;
            // BGRA：只压 RGB，alpha 不动（窗口圆角外的透明边界得留着）
            *bits.add(o) = lut[*bits.add(o) as usize];
            *bits.add(o + 1) = lut[*bits.add(o + 1) as usize];
            *bits.add(o + 2) = lut[*bits.add(o + 2) as usize];
        }
    }
}

/// 卡里那颗按钮（`0` = 右边那颗「取消」，`1` = 左边那颗「删除」；`i` 越大越往左）
fn s_conf_btn(r: RECT, i: usize) -> RECT {
    let bw = 88;
    let right = r.right - 16 - (i as i32) * (bw + 8);
    RECT { left: right - bw, top: r.bottom - 16 - S_PILL_H, right, bottom: r.bottom - 16 }
}

/// 待删对象的一句话（卡片标题下那行小字）
fn s_conf_text(st: &State) -> (String, String) {
    let idx = |i: usize, n: usize| i < n;
    match st.set_confirm {
        Ctl::AppDel(ai) => {
            if idx(ai, st.apps.len()) {
                let a = &st.apps[ai];
                (
                    format!("删除应用「{}」？", a.name),
                    if a.items.is_empty() {
                        "它不会出现在内环了。".to_string()
                    } else {
                        format!("连同外环 {} 条条目一起删除。", a.items.len())
                    },
                )
            } else {
                ("删除应用？".to_string(), String::new())
            }
        }
        Ctl::ItemDel(ai, ii) => {
            let name = if idx(ai, st.apps.len()) && idx(ii, st.apps[ai].items.len()) {
                Some(st.apps[ai].items[ii].name.clone())
            } else {
                None
            };
            (
                match name {
                    Some(n) => format!("删除条目「{}」？", n),
                    None => "删除条目？".to_string(),
                },
                "只删这一条，应用还在。".to_string(),
            )
        }
        Ctl::ExcDel(c, i) => {
            let bucket = if c == 0 { &st.exc } else { &st.exc_w };
            let name = bucket.get(i).cloned().unwrap_or_default();
            (
                format!("从{}单里移除「{}」？", if c == 0 { "黑" } else { "白" }, name),
                "这个程序以后按上面的模式处理。".to_string(),
            )
        }
        _ => (String::new(), String::new()),
    }
}

/// 内容区里从左到右排一排等宽控件
fn s_strip(i: usize, top: i32, w: i32, gap: i32, h: i32) -> RECT {
    let l = S_X0 + i as i32 * (w + gap);
    RECT { left: l, top, right: l + w, bottom: top + h }
}

/// 指定 y 的开关（各页自成一套布局，序号不通用）
fn s_sw_rect_at(y: i32) -> RECT {
    RECT { left: S_X1 - 46, top: y, right: S_X1, bottom: y + 26 }
}

// ── GDI 小工具 ──

fn rgb(c: (u8, u8, u8)) -> COLORREF {
    COLORREF((c.0 as u32) | ((c.1 as u32) << 8) | ((c.2 as u32) << 16))
}

unsafe fn g_rrect(dc: HDC, r: RECT, rad: i32, fill: (u8, u8, u8), edge: Option<(u8, u8, u8)>, ew: i32) {
    if r.right <= r.left || r.bottom <= r.top {
        return;
    }
    let br = CreateSolidBrush(rgb(fill));
    let pen = match edge {
        Some(e) => CreatePen(PS_SOLID, ew * SS, rgb(e)),
        None => CreatePen(PS_NULL, 0, rgb(fill)),
    };
    let ob = SelectObject(dc, HGDIOBJ(br.0));
    let op = SelectObject(dc, HGDIOBJ(pen.0));
    let _ = RoundRect(
        dc,
        sxv(r.left),
        sxv(r.top),
        sxv(r.right),
        sxv(r.bottom),
        rad * 2 * SS,
        rad * 2 * SS,
    );
    let _ = SelectObject(dc, ob);
    let _ = SelectObject(dc, op);
    let _ = DeleteObject(HGDIOBJ(br.0));
    let _ = DeleteObject(HGDIOBJ(pen.0));
}

/// 只描边不填充（卡片外框：画在内容之上，不能盖住内容）
unsafe fn g_rrect_stroke(dc: HDC, r: RECT, rad: i32, edge: (u8, u8, u8), ew: i32) {
    let pen = CreatePen(PS_SOLID, ew * SS, rgb(edge));
    let nb = GetStockObject(NULL_BRUSH);
    let op = SelectObject(dc, HGDIOBJ(pen.0));
    let ob = SelectObject(dc, nb);
    let _ = RoundRect(
        dc,
        sxv(r.left),
        sxv(r.top),
        sxv(r.right),
        sxv(r.bottom),
        rad * 2 * SS,
        rad * 2 * SS,
    );
    let _ = SelectObject(dc, op);
    let _ = SelectObject(dc, ob);
    let _ = DeleteObject(HGDIOBJ(pen.0));
}

unsafe fn g_fill(dc: HDC, r: RECT, c: (u8, u8, u8)) {
    let br = CreateSolidBrush(rgb(c));
    let rr = RECT { left: sxv(r.left), top: sxv(r.top), right: sxv(r.right), bottom: sxv(r.bottom) };
    let _ = FillRect(dc, &rr, br);
    let _ = DeleteObject(HGDIOBJ(br.0));
}

/// 硬阴影：向下偏移 3px 的同形圆角块（无模糊半径）
unsafe fn g_shadow(dc: HDC, r: RECT, rad: i32, pal: &Pal) {
    let sh = mixc(pal.paper, pal.ink, 0.13);
    let sr = RECT { left: r.left, top: r.top + 3, right: r.right, bottom: r.bottom + 3 };
    g_rrect(dc, sr, rad, sh, None, 0);
}

unsafe fn g_text(
    dc: HDC,
    text: &str,
    r: RECT,
    color: (u8, u8, u8),
    font: HFONT,
    center: bool,
) {
    let _ = SelectObject(dc, HGDIOBJ(font.0));
    let _ = SetBkMode(dc, TRANSPARENT);
    let _ = SetTextColor(dc, rgb(color));
    let mut t: Vec<u16> = text.encode_utf16().collect();
    let mut rr = RECT { left: sxv(r.left), top: sxv(r.top), right: sxv(r.right), bottom: sxv(r.bottom) };
    let fmt = DT_SINGLELINE
        | DT_VCENTER
        | DT_NOPREFIX
        | if center { DT_CENTER } else { DT_LEFT | DT_END_ELLIPSIS };
    let _ = DrawTextW(dc, &mut t, &mut rr, fmt);
}

/// 分段控件 / 多选胶囊：选中 = 墨底白字。
/// `hv` = hover 强度 0..1 —— **必须在这里参与填充色的计算**：
/// 早先在调用处「先铺一层 hover 底色、再画胶囊」，胶囊的不透明填充会把它整个盖掉，
/// 表现就是「hover 完全没反应」。
unsafe fn g_chip(
    dc: HDC,
    pal: &Pal,
    r: RECT,
    on: bool,
    label: &str,
    font: HFONT,
    hv: f32,
) {
    if on {
        if hv > 0.02 {
            g_shadow(dc, r, 11, pal); // 选中态 hover：浮起来一点
        }
        g_rrect(dc, r, 11, pal.block, Some(pal.block), 2);
        g_text(dc, label, r, pal.paper, font, true);
    } else {
        let fill = mixc(pal.paper, pal.ink, 0.07 * hv);
        let edge = mixc(pal.edge, pal.ink, 0.45 * hv);
        g_rrect(dc, r, 11, fill, Some(edge), 2);
        g_text(dc, label, r, pal.ink, font, true);
    }
}

/// 整页淡入用的调色板：把每个颜色朝卡片色插值。
/// 这样「切页淡入」只要换一个 pal 就行，不用改动每一处绘制。
fn fade_pal(p: &Pal, a: f32) -> Pal {
    let a = a.clamp(0.0, 1.0);
    let f = |c: (u8, u8, u8)| mixc(p.paper, c, a);
    Pal {
        wall: f(p.wall),
        wall2: f(p.wall2),
        paper: p.paper,
        ink: f(p.ink),
        block: f(p.block),
        action: f(p.action),
        edge: f(p.edge),
    }
}

unsafe fn g_switch(dc: HDC, pal: &Pal, r: RECT, anim: f32) {
    let rad = (r.bottom - r.top) / 2;
    let a = anim.clamp(0.0, 1.0);
    let fill = mixc(pal.wall2, pal.block, a);
    let edge = mixc(pal.edge, pal.block, a);
    g_rrect(dc, r, rad, fill, Some(edge), 2);
    let d = (r.bottom - r.top) - 8;
    // 圆钮位置随 anim 滑动（不是瞬间跳）
    let x_on = (r.right - 4 - d) as f32;
    let x_off = (r.left + 4) as f32;
    let kx = (x_off + (x_on - x_off) * a).round() as i32;
    let kr = RECT { left: kx, top: r.top + 4, right: kx + d, bottom: r.top + 4 + d };
    g_rrect(dc, kr, d / 2, pal.paper, None, 0);
}

// ── 绘制 ──

const S_PAGES: [&str; 4] = ["外观", "交互", "应用", "关于"];

/// 控件当前的 hover 强度 0..1：新 hover 淡入、旧 hover 淡出。
/// 用「渐变状态量」而不是布尔 —— 否则 hover 是硬切，没有过渡感。
fn hov(st: &State, c: Ctl) -> f32 {
    if st.set_hover == c {
        st.set_hov_a
    } else if st.set_hover_prev == c {
        1.0 - st.set_hov_a
    } else {
        0.0
    }
}

/// 开关的动画值：0 = 关、1 = 开（中间态用于滑动过渡）
/// 交互页开关的动画量：0 常驻 / 1 监听 → 直接对应；2 全屏放行(游戏模式) → `sw_a[3]`
fn act_sw_val(st: &State, i: usize) -> f32 {
    match i {
        0 | 1 => st.sw_a[i],
        2 => st.sw_a[3], // 全屏放行
        4 => st.sw_a[4], // 显示应用名称
        _ => st.sw_a[3],
    }
}

// ═══════════════ 设置页「聚光灯引导」（coach marks）═══════════════
//
// 首次打开设置页自动播 4 步；标题栏「查看引导」可重播。
// ⚠️ 引导期间**只读**：只有卡片上的三个按钮响应点击，点其它地方一律吃掉 ——
//    「讲明白」而不是「教操作」，也避免点了之后后面几步的锚点位移。

/// 第 i 步高亮的目标区域（卡片逻辑坐标）。**与 `s_paint_act` 的版面一一对应** ——
/// 版面一改这里必须同步，否则就「画在这、亮在那」。
fn guide_target_rect(i: usize) -> RECT {
    match i {
        // ① 呼出方式块：4 个胶囊 + 下面的说明行（整块一起高亮）
        0 => RECT {
            left: S_X0 - 8,
            top: ACT_CHIPS_Y - 8,
            right: S_X1 + 8,
            bottom: TRIG_ROW_Y + ACT_TRIG_H + 4,
        },
        // ② 常驻模式行（行为区第 0 行）
        1 => {
            let r = act_sw_row(0);
            RECT { left: r.left - 8, top: r.top, right: r.right + 8, bottom: r.bottom }
        }
        // ③ 全屏放行(游戏模式) 行（行为区第 2 行）
        2 => {
            let r = act_sw_row(2);
            RECT { left: r.left - 8, top: r.top, right: r.right + 8, bottom: r.bottom }
        }
        // ④ 黑白名单块（标题 + 两张卡）。
        //    ⚠️ 上边界只到标题上方一点点：`act_sw_row(3)`（开机自启）的矩形下沿(436)
        //    和本块标题(428)是**交叠**的，抠太靠上会把上一行的开关切一半。
        _ => RECT {
            left: S_X0 - 8,
            top: EXC_CARDS_TITLE_Y - 4,
            right: S_X1 + 8,
            bottom: EXC_CARD_BOT + 10,
        },
    }
}

/// 引导卡（标题, 正文行）。正文**自己控制换行** —— 项目里没有自动折行。
/// 每行 ≤ 20 个全角字（卡片内宽 268px / 13px ≈ 20 字）。
fn guide_text(i: usize) -> (&'static str, &'static [&'static str]) {
    match i {
        0 => (
            "自定义按键",
            &[
                "中键、侧键、长按右键都能用，也可以点",
                "这里录一个自己的组合键（比如 Ctrl+W），",
                "录完按下就呼出。",
            ],
        ),
        1 => (
            "常驻模式",
            &["打开后轮盘一直挂在屏幕上，点一下图标", "就直接打开，不用按住不放。"],
        ),
        2 => (
            "全屏放行(游戏模式)",
            &[
                "看视频、打游戏全屏时自动放行，呼出键",
                "不再误触打断。想反过来拦住全屏，就",
                "把它关掉。",
            ],
        ),
        _ => (
            "黑白名单",
            &[
                "两张卡片二选一生效，点标题切换。",
                "黑名单 = 这些程序里不呼出；",
                "白名单 = 只有这些程序里才呼出。",
            ],
        ),
    }
}

const GUIDE_W: i32 = 300;   // 卡片宽
const GUIDE_GAP: i32 = 46;  // 卡片与目标之间的空隙（箭头就画在这一段里）
const GUIDE_PAD: i32 = 16;

/// 引导卡矩形。**自动避让**：下方放不下就翻到目标上方，箭头反向。
fn guide_card_rect(i: usize) -> RECT {
    let t = guide_target_rect(i);
    let lines = guide_text(i).1.len() as i32;
    // 高 = 上边距 + 步骤行 + 标题 + 正文 n 行 + 按钮 + 下边距
    let h = GUIDE_PAD + 18 + 10 + 22 + 7 + lines * 21 + 14 + 30 + GUIDE_PAD;
    let cx = (t.left + t.right) / 2;
    let left = (cx - GUIDE_W / 2).clamp(12, S_W - 12 - GUIDE_W);
    let below = t.bottom + GUIDE_GAP + h <= S_H - 12;
    let top = if below { t.bottom + GUIDE_GAP } else { t.top - GUIDE_GAP - h };
    RECT { left, top, right: left + GUIDE_W, bottom: top + h }
}

/// 引导卡按钮（0=跳过 1=上一步 2=下一步/完成）的逻辑矩形
fn guide_btn_rect(i: usize, b: usize) -> RECT {
    let c = guide_card_rect(i);
    let by = c.bottom - GUIDE_PAD - 30;
    let r76 = |off: i32| RECT {
        left: c.right - GUIDE_PAD - off - 76,
        top: by,
        right: c.right - GUIDE_PAD - off,
        bottom: by + 30,
    };
    match b {
        2 => r76(0),
        1 => r76(84),
        _ => RECT { left: c.left + GUIDE_PAD, top: by, right: c.left + GUIDE_PAD + 70, bottom: by + 30 },
    }
}

/// 标题栏「查看引导」入口 —— 跳过的人也能随时找回来（避免「跳过即永久错过」）
fn s_guide_entry_rect() -> RECT {
    // ⚠️ 必须避开右侧的版本号（`v3 · N 个应用` 居中在 S_W-300..S_W-62 → 实测文字 559..639）；
    // 原来放在 S_W-266..S_W-158（居中 568）会和它**叠字**。
    RECT { left: S_W - 340, top: 14, right: S_W - 232, bottom: 38 }
}

/// 引导期间命中测试：返回 0=跳过 1=上一步 2=下一步；**-1 = 吃掉（不做任何事）**
fn guide_hit(st: &State, x: i32, y: i32) -> i32 {
    if st.guide_step < 0 {
        return -1;
    }
    let i = st.guide_step as usize;
    if !in_rect(guide_card_rect(i), x, y) {
        return -1;
    }
    for b in 0..3usize {
        if in_rect(guide_btn_rect(i, b), x, y) {
            if b == 1 && i == 0 {
                return -1; // 第一步没有「上一步」
            }
            return b as i32;
        }
    }
    -1
}

/// 圆角描边（无填充）—— 引导挖孔的白环
unsafe fn g_ring(dc: HDC, r: RECT, rad: i32, c: (u8, u8, u8), ew: i32) {
    let br = GetStockObject(NULL_BRUSH);
    let pen = CreatePen(PS_SOLID, ew * SS, rgb(c));
    let ob = SelectObject(dc, br);
    let op = SelectObject(dc, pen);
    let _ = RoundRect(dc, sxv(r.left), sxv(r.top), sxv(r.right), sxv(r.bottom), rad * 2 * SS, rad * 2 * SS);
    let _ = SelectObject(dc, ob);
    let _ = SelectObject(dc, op);
    let _ = DeleteObject(HGDIOBJ(pen.0));
}

/// 实心圆（进度点）
unsafe fn g_dot(dc: HDC, cx: i32, cy: i32, rad: i32, c: (u8, u8, u8)) {
    let br = CreateSolidBrush(rgb(c));
    let pen = CreatePen(PS_NULL, 0, rgb(c));
    let ob = SelectObject(dc, HGDIOBJ(br.0));
    let op = SelectObject(dc, HGDIOBJ(pen.0));
    let _ = Ellipse(dc, sxv(cx - rad), sxv(cy - rad), sxv(cx + rad), sxv(cy + rad));
    let _ = SelectObject(dc, ob);
    let _ = SelectObject(dc, op);
    let _ = DeleteObject(HGDIOBJ(br.0));
    let _ = DeleteObject(HGDIOBJ(pen.0));
}

/// 直线段（箭头头用）
unsafe fn g_line(dc: HDC, x0: i32, y0: i32, x1: i32, y1: i32, wpx: i32, c: (u8, u8, u8)) {
    let pen = CreatePen(PS_SOLID, wpx * SS, rgb(c));
    let op = SelectObject(dc, HGDIOBJ(pen.0));
    let _ = MoveToEx(dc, sxv(x0), sxv(y0), None);
    let _ = LineTo(dc, sxv(x1), sxv(y1));
    let _ = SelectObject(dc, op);
    let _ = DeleteObject(HGDIOBJ(pen.0));
}

/// 画引导层：**先压暗挖孔外的像素**，再画白环 / 卡片 / 虚线箭头。
/// ⚠️ 必须在**降采样之前**调用（卡片圆角与文字靠 SS 超采样才有抗锯齿）。
unsafe fn guide_draw(st: &mut State) {
    if st.guide_step < 0 || st.s_hi_bits.is_null() {
        return;
    }
    let i = st.guide_step as usize;
    let t = guide_target_rect(i);
    let c = guide_card_rect(i);
    let pal = pal_of(st.glass_liquid);

    // ── 1) 压暗：等效覆盖一层 rgba(纯黑, 55%) → `v * 0.45`。
    //      ⚠️ 用**纯黑**而不是 `rgba(ink, .58)`：后者对已经接近黑的像素会「**提亮**」
    //      （深色主题下 nav #0F0F11 反而从 15 变 22），明暗关系就反了。
    //      纯黑合成在亮/暗两套主题下都单调变暗，这才是遮罩该有的行为。
    //      256 项查表，避免逐像素乘除。
    let mut lut = [0u8; 256];
    for v in 0..256usize {
        lut[v] = (v * 45 / 100) as u8;
    }
    let w = (S_WW * SS) as usize;
    let h = (S_HH * SS) as usize;
    // 挖孔在设备坐标下的范围（sxv = (v + S_M) * SS）
    let hx0 = sxv(t.left).clamp(0, S_WW * SS) as usize;
    let hy0 = sxv(t.top).clamp(0, S_HH * SS) as usize;
    let hx1 = sxv(t.right).clamp(0, S_WW * SS) as usize;
    let hy1 = sxv(t.bottom).clamp(0, S_HH * SS) as usize;
    let bits = st.s_hi_bits;
    // 诊断点：逻辑 (760,600) 卡片内右下空白，必是底纸
    let dbg_o = (((600 + S_M) * SS) as usize * w + ((760 + S_M) * SS) as usize) * 4;
    let dbg = std::env::var_os("RL_GUIDEDBG").is_some();
    if dbg {
        eprintln!(
            "[guide] dim 前 (760,600) BGR = {:?}",
            (*bits.add(dbg_o), *bits.add(dbg_o + 1), *bits.add(dbg_o + 2))
        );
    }
    for y in 0..h {
        let band = y >= hy0 && y < hy1;
        let row = y * w;
        for x in 0..w {
            if band && x >= hx0 && x < hx1 {
                continue; // 挖孔内保持原样
            }
            let o = (row + x) * 4;
            // BGRA：只压 RGB，alpha 不动
            *bits.add(o) = lut[*bits.add(o) as usize];
            *bits.add(o + 1) = lut[*bits.add(o + 1) as usize];
            *bits.add(o + 2) = lut[*bits.add(o + 2) as usize];
        }
    }

    if dbg {
        eprintln!(
            "[guide] dim 后 (760,600) BGR = {:?}",
            (*bits.add(dbg_o), *bits.add(dbg_o + 1), *bits.add(dbg_o + 2))
        );
    }
    let dc = st.s_hi_dc;
    let muted = mixc(pal.ink, pal.paper, 0.45);
    let body = mixc(pal.ink, pal.paper, 0.28);

    // ── 2) 挖孔白环 ──
    g_ring(dc, t, 10, (255, 255, 255), 2);

    // ── 3) 引导卡 ──
    g_shadow(dc, c, 12, &pal);
    g_rrect(dc, c, 12, pal.paper, Some(mixc(pal.paper, pal.ink, 0.14)), 1);

    let (title, lines) = guide_text(i);
    // 步骤 + 进度点
    g_text(
        dc,
        &format!("第 {} 步 / 共 4 步", i + 1),
        RECT { left: c.left + GUIDE_PAD, top: c.top + GUIDE_PAD, right: c.right - 80, bottom: c.top + GUIDE_PAD + 18 },
        muted,
        st.sf_small,
        false,
    );
    for d in 0..4usize {
        let dcx = c.right - GUIDE_PAD - 3 - (3 - d as i32) * 11;
        let dcy = c.top + GUIDE_PAD + 9;
        g_dot(dc, dcx, dcy, 3, if d == i { pal.block } else { mixc(pal.paper, pal.ink, 0.17) });
    }
    // 标题
    g_text(
        dc,
        title,
        RECT { left: c.left + GUIDE_PAD, top: c.top + GUIDE_PAD + 28, right: c.right - GUIDE_PAD, bottom: c.top + GUIDE_PAD + 50 },
        pal.ink,
        st.sf_bold,
        false,
    );
    // 正文
    for (k, line) in lines.iter().enumerate() {
        let ly = c.top + GUIDE_PAD + 57 + k as i32 * 21;
        g_text(
            dc,
            line,
            RECT { left: c.left + GUIDE_PAD, top: ly, right: c.right - GUIDE_PAD, bottom: ly + 20 },
            body,
            st.sf_body,
            false,
        );
    }
    // 按钮：跳过 / 上一步 / 下一步（末步＝完成）
    let by = c.bottom - GUIDE_PAD - 30;
    g_text(
        dc,
        "跳过引导",
        RECT { left: c.left + GUIDE_PAD, top: by, right: c.left + GUIDE_PAD + 70, bottom: by + 30 },
        muted,
        st.sf_small,
        false,
    );
    if i > 0 {
        let r = guide_btn_rect(i, 1);
        g_rrect(dc, r, 8, pal.paper, Some(mixc(pal.paper, pal.ink, 0.14)), 1);
        g_text(dc, "上一步", RECT { left: r.left + 8, top: r.top, right: r.right - 8, bottom: r.bottom }, body, st.sf_small, true);
    }
    let rn = guide_btn_rect(i, 2);
    g_rrect(dc, rn, 8, pal.block, None, 0);
    let last = i == 3;
    g_text(
        dc,
        if last { "完成" } else { "下一步" },
        RECT { left: rn.left + 8, top: rn.top, right: rn.right - 8, bottom: rn.bottom },
        pal.paper,
        st.sf_small,
        true,
    );

    // ── 4) 虚线箭头：卡片 → 挖孔（画在卡片之后再盖一次也无妨，线段本身不重叠）──
    let ax = (t.left + t.right) / 2;
    let below = c.top > t.bottom;
    let (ay0, ay1) = if below { (c.top - 2, t.bottom + 8) } else { (c.bottom + 2, t.top - 8) };
    let (lo, hi) = (ay0.min(ay1), ay0.max(ay1));
    let mut y = lo;
    while y < hi {
        let seg = (y + 5).min(hi) - y;
        if seg > 1 {
            g_rrect(dc, RECT { left: ax - 1, top: y, right: ax + 1, bottom: y + seg }, 1, pal.ink, None, 0);
        }
        y += 8;
    }
    // 箭头头（朝挖孔那一端）
    let (hx, hy) = if below { (ax, t.bottom + 1) } else { (ax, t.top - 1) };
    let dh = if below { -6 } else { 6 };
    g_line(dc, hx - 5, hy + dh, hx, hy, 2, pal.ink);
    g_line(dc, hx + 5, hy + dh, hx, hy, 2, pal.ink);
}

/// 推进/结束引导。`d`：+1 下一步，-1 上一步，0 结束。
unsafe fn guide_advance(st: &mut State, d: i32) {
    if st.guide_step < 0 {
        return;
    }
    if d == 0 {
        st.guide_step = -1;
    } else {
        let n = st.guide_step + d;
        if n < 0 {
            st.guide_step = 0;
        } else if n > 3 {
            st.guide_step = -1; // 末步「完成」= 关掉
        } else {
            st.guide_step = n;
        }
    }
}

/// 开始（或重播）引导。
///
/// ⚠️⚠️ 必须**先切到交互页**（第 1 页）：4 步高亮的全是交互页的控件，
/// 停在外观页时挖孔会「亮在一片不相干的内容上」。用户 2026-10-13 报
/// 「我刚进设置页是外观啊，应该是进交互才引导」就是这个。
unsafe fn guide_start(st: &mut State) {
    if st.set_page != 1 {
        st.set_page = 1;
        st.set_page_a = 0.0; // 与点导航一致：带切页淡入
        st.set_hover = Ctl::None;
        st.set_hover_prev = Ctl::None;
        st.tip = 0;
    }
    st.guide_step = 0;
    // ⚠️⚠️ 必须**踢帧定时器** —— `InvalidateRect` 对这个分层窗口是空操作。
    //
    // 切页淡入靠 `setup_tick` 把 `set_page_a` 从 0 收敛到 1；而**引导进行中
    // `s_hit` 恒返回 `None`**（引导是只读的）→ 鼠标移动永远不改 hover →
    // `WM_MOUSEMOVE` 里那两个 `setup_kick` 永远不会被调到 → 帧定时器不启动
    // → `set_page_a` 卡在 0 → **整页被 `fade_pal` 淡成纸色，内容全不可见**，
    // 只剩引导层看得见。
    //
    // 用户 2026-10-14 报的「打开设置后明明在外观 tab 却出现了引导」正是这个：
    // 页面其实**已经切到交互页了**，只是整页看不见，所以被当成还停在外观。
    kick_setup_frame(st);
}

/// 把一页画进 hi 画布（逻辑坐标；缩放与超采样由 g_* 负责）
unsafe fn settings_draw(st: &mut State, dc: HDC) {
    let mut pal = pal_of(st.glass_liquid);
    // 切页淡入：整块调色板朝卡片色插值，不用改每一处绘制
    if st.set_page_a < 0.999 {
        pal = fade_pal(&pal, st.set_page_a);
    }
    let full = RECT { left: 0, top: 0, right: S_W, bottom: S_H };
    g_fill(dc, full, pal.paper);

    // 侧栏
    let nav_bg = RECT { left: 0, top: S_TITLE_H, right: S_NAV_W, bottom: S_H };
    g_fill(dc, nav_bg, pal.wall2);
    g_fill(dc, RECT { left: S_NAV_W - 2, top: S_TITLE_H, right: S_NAV_W, bottom: S_H },
        mixc(pal.paper, pal.ink, 0.14));

    // 标题栏
    g_text(dc, "设置", RECT { left: 22, top: 0, right: 220, bottom: S_TITLE_H }, pal.ink, st.sf_title, false);
    let ver = format!("v3 · {} 个应用", st.apps.len());
    g_text(dc, &ver, RECT { left: S_W - 300, top: 0, right: S_W - 62, bottom: S_TITLE_H },
        mixc(pal.paper, pal.ink, 0.55), st.sf_small, true);
    // 标题栏「查看引导」：跳过首启引导的人随时能找回来
    {
        let er = s_guide_entry_rect();
        let ehv = hov(st, Ctl::GuideEntry);
        let col = mixc(pal.paper, pal.ink, 0.45 - 0.30 * ehv);
        g_text(dc, "查看引导", er, col, st.sf_small, true);
        g_fill(
            dc,
            RECT { left: er.left + 14, top: er.bottom - 3, right: er.right - 14, bottom: er.bottom - 2 },
            mixc(pal.paper, pal.ink, 0.28 - 0.20 * ehv),
        );
    }
    g_fill(dc, RECT { left: 0, top: S_TITLE_H - 2, right: S_W, bottom: S_TITLE_H },
        mixc(pal.paper, pal.ink, 0.14));
    // 关闭按钮
    let cr = RECT { left: S_W - 46, top: 12, right: S_W - 16, bottom: 42 };
    let chv = hov(st, Ctl::Close);
    if chv > 0.01 {
        g_rrect(dc, cr, 9, mixc(pal.paper, pal.ink, 0.08 * chv), None, 0);
    }
    g_text(dc, "×", cr, mixc(pal.ink, pal.paper, 0.0), st.sf_bold, true);

    // 侧栏导航
    for i in 0..S_PAGES.len() {
        let r = s_nav_rect(i);
        // 文字要**内缩**：pill 从 left=14 起，文字贴着它的左边缘会显得「顶到边」
        let tr = RECT { left: r.left + 15, top: r.top, right: r.right - 8, bottom: r.bottom };
        if st.set_page == i {
            g_rrect(dc, r, 10, pal.block, None, 0);
            g_text(dc, S_PAGES[i], tr, pal.paper, st.sf_bold, false);
        } else {
            let hv = hov(st, Ctl::Nav(i));
            if hv > 0.01 {
                g_rrect(dc, r, 10, mixc(pal.wall2, pal.ink, 0.09 * hv), None, 0);
            }
            g_text(dc, S_PAGES[i], tr, pal.ink, st.sf_nav, false);
        }
    }

    match st.set_page {
        0 => s_paint_look(st, dc, &pal),
        1 => s_paint_act(st, dc, &pal),
        2 => s_paint_apps(st, dc, &pal),
        _ => s_paint_adv(st, dc, &pal),
    }

    // ── 「?」的 tooltip 画在最后（要盖在内容上面）──
    if st.tip != 0 {
        let lines = tip_lines(st.tip);
        if !lines.is_empty() {
            let (ax, ay) = tip_anchor(st.tip);
            // ⚠️ 宽度**按文案量出来**，别写死 —— 写死 384 会拖出一条超宽的条
            //    （用户 2026-10-11：「明显展示的 tooltip 过宽了」）。
            //    这是 12px 字体（`sf_small`）：中文≈12px、西文≈6.5px，左右各留 12 padding。
            let tw = lines
                .iter()
                .map(|l| est_text_w(l, 12.0, 6.5))
                .fold(0.0f32, f32::max)
                .ceil() as i32;
            let w = (tw + 24).clamp(120, S_W - S_M * 2 - 24);
            let h = 16 + lines.len() as i32 * 20;
            let x = (ax - 14).min(S_W - 10 - w).max(8);
            let mut y = ay;
            if y + h > S_H - 10 {
                y = (ay - h - 34).max(8); // 下面放不下就翻到上面
            }
            let bx = RECT { left: x, top: y, right: x + w, bottom: y + h };
            g_rrect(dc, bx, 10, mixc(pal.block, pal.paper, 0.04), None, 0);
            for (i, l) in lines.iter().enumerate() {
                let t = y + 8 + i as i32 * 20;
                g_text(dc, l,
                    RECT { left: x + 12, top: t, right: x + w - 12, bottom: t + 18 },
                    pal.paper, st.sf_small, false);
            }
        }
    }

    // ── 删除确认小卡：画在最后（要盖住列表和 tooltip）──
    if st.set_confirm != Ctl::None {
        let r = s_conf_rect();
        let (t1, t2) = s_conf_text(st);
        conf_dim(st);
        g_shadow(dc, r, 14, &pal);
        g_rrect(dc, r, 14, mixc(pal.paper, pal.ink, 0.04), Some(pal.edge), 2);
        g_text(
            dc,
            &t1,
            RECT { left: r.left + 18, top: r.top + 16, right: r.right - 18, bottom: r.top + 40 },
            pal.ink,
            st.sf_body,
            false,
        );
        if !t2.is_empty() {
            g_text(
                dc,
                &t2,
                RECT { left: r.left + 18, top: r.top + 44, right: r.right - 18, bottom: r.top + 64 },
                mixc(pal.paper, pal.ink, 0.5),
                st.sf_small,
                false,
            );
        }
        for (i, label) in ["取消", "删除"].iter().enumerate() {
            let br = s_conf_btn(r, i);
            let ctl = if i == 0 { Ctl::ConfNo } else { Ctl::ConfYes };
            let hv = hov(st, ctl);
            if i == 1 {
                // 危险动作：实底红，hover 再深一档
                let base = mixc((198, 74, 74), pal.ink, 0.12 * hv);
                g_rrect(dc, br, S_PILL_H / 2, base, None, 0);
                g_text(dc, label, br, mixc(pal.paper, (255, 255, 255), 0.6 + 0.4 * hv),
                    st.sf_small, true);
            } else {
                g_rrect(dc, br, S_PILL_H / 2, mixc(pal.paper, pal.ink, 0.06 * hv),
                    Some(mixc(pal.edge, pal.ink, 0.25 + 0.5 * hv)), 2);
                g_text(dc, label, br, mixc(pal.paper, pal.ink, 0.6 + 0.3 * hv), st.sf_small, true);
            }
        }
    }

    // 卡片外框**最后画**：它是描边不填充，既不会被内容盖住，也不会盖住内容
    // （卡片外那圈靠 alpha 裁掉，所以这里的直角矩形也不用倒角）
    g_rrect_stroke(dc, full, CARD_RAD, pal.ink, 2);
}

unsafe fn s_paint_look(st: &mut State, dc: HDC, pal: &Pal) {
    let y = ACT_TOP_Y;
    g_text(dc, "配色", RECT { left: S_X0, top: y, right: S_X1, bottom: y + ACT_LABEL_H },
        pal.ink, st.sf_bold, false);
    let labels = ["亮色", "暗色", "跟随系统"];
    for i in 0..3 {
        let r = s_strip(i, ACT_CHIPS_Y, 108, 10, ACT_CHIPS_H);
        let on = st.theme == i as i32;
        g_chip(dc, pal, r, on, labels[i], st.sf_body, hov(st, Ctl::Theme(i)));
    }

    // ── 材质：纸面 / 液态玻璃（只作用于**轮盘**，设置页永远纸面）──
    g_text(dc, "材质", RECT { left: S_X0, top: LOOK_MAT_LABEL_Y, right: S_X1, bottom: LOOK_MAT_LABEL_Y + ACT_LABEL_H },
        pal.ink, st.sf_bold, false);
    let mats = ["纸面", "液态玻璃"];
    for i in 0..2 {
        let r = s_strip(i, LOOK_MAT_Y, 108, 10, ACT_CHIPS_H);
        let on = st.glass == (i == 1);
        g_chip(dc, pal, r, on, mats[i], st.sf_body, hov(st, Ctl::Mat(i)));
    }
    // 一句说明（玻璃的真实限制：实时刷新靠「把自己排除出抓屏」，录屏里就看不见轮盘）
    let hint = if st.glass_live {
        "玻璃实时透出桌面；打开时轮盘不会被录屏/截图拍到"
    } else {
        "玻璃会透出呼出时的桌面；抓屏不可用（受保护窗口）会自动回到纸面"
    };
    let hy = LOOK_MAT_Y + ACT_CHIPS_H + 14;
    g_text(dc, hint, RECT { left: S_X0, top: hy, right: S_X1, bottom: hy + 20 },
        mixc(pal.paper, pal.ink, 0.42), st.sf_small, false);
}

unsafe fn s_paint_act(st: &mut State, dc: HDC, pal: &Pal) {
    // ⚠️ 纵向位置一律取 `ACT_*` 常量（见其定义处的节奏说明），别在这里写死数字
    let y = ACT_TOP_Y;
    g_text(dc, "呼出方式", RECT { left: S_X0, top: y, right: S_X1, bottom: y + ACT_LABEL_H },
        pal.ink, st.sf_bold, false);
    let labels = ["鼠标中键", "鼠标侧键", "长按右键", "自定义"];
    let on_flags = [
        st.key_middle,
        st.key_side,
        st.key_rhold,
        st.trig_custom_kind != 0 || st.trig_capturing,
    ];
    for i in 0..4 {
        let r = s_strip(i, ACT_CHIPS_Y, 118, 10, ACT_CHIPS_H);
        let (on, lbl) = if i == 3 {
            (on_flags[3], trig_chip_label(st))
        } else {
            let on = on_flags[i];
            let lbl = if on { format!("✓ {}", labels[i]) } else { labels[i].to_string() };
            (on, lbl)
        };
        g_chip(dc, pal, r, on, &lbl, st.sf_body,
            hov(st, if i == 3 { Ctl::TrigCapture } else { Ctl::Key(i) }));
    }

    // ── 自定义触发：详情行（录制提示 / 已启用可重录 / 未设置提示）+ 清除按钮 ──
    {
        let rr = trig_row_rect();
        let detail = if st.trig_capturing {
            if st.trig_capture_bad == 1 {
                "✗ 鼠标左键不可用作触发键（会破坏正常左键操作），请改按键盘组合键".to_string()
            } else if st.trig_capture_bad == 2 {
                "✗ 鼠标右键不可用作触发键（已用于「长按右键」），请改按键盘组合键".to_string()
            } else if st.trig_capture_bad == 3 {
                "✗ 鼠标中键已可在上方单独勾选，无需录制为自定义触发键".to_string()
            } else if st.trig_capture_bad == 4 || st.trig_capture_bad == 5 {
                "✗ 鼠标侧键已可在上方单独勾选，无需录制为自定义触发键".to_string()
            } else {
                "按键盘组合键（如 Ctrl+W）".to_string()
            }
        } else if st.trig_custom_kind != 0 {
            "已启用 · 再次点击可重新录制".to_string()
        } else {
            "点击「自定义」录制一个触发键（如 Ctrl+W）".to_string()
        };
        // 录制中右侧要放「取消录制」按钮 → 正文得相应收窄，别被压住
        g_text(dc, &detail,
            RECT {
                left: S_X0 + 4,
                top: rr.top + 9,
                right: if st.trig_capturing { S_X1 - 96 } else { S_X1 - 40 },
                bottom: rr.top + 29,
            },
            if st.trig_capturing { pal.ink } else { mixc(pal.ink, pal.paper, 0.5) },
            st.sf_small, false);
        if st.trig_capturing {
            // 录制态：右下角给一个**鼠标可点的**出口
            let cr = trig_cancel_rect();
            let hvc = hov(st, Ctl::TrigCancel);
            g_rrect(dc, cr, 7,
                if hvc > 0.01 { mixc(pal.paper, pal.ink, 0.10) } else { mixc(pal.paper, pal.ink, 0.04) },
                Some(mixc(pal.paper, pal.ink, 0.22 + 0.30 * hvc)), 1);
            g_text(dc, "取消录制", cr, mixc(pal.paper, pal.ink, 0.60 - 0.35 * hvc),
                st.sf_small, true);
        } else if st.trig_custom_kind != 0 {
            let xr = trig_clear_rect();
            let hvc = hov(st, Ctl::TrigClear);
            g_rrect(dc, xr, 6,
                if hvc > 0.01 { mixc(pal.paper, pal.ink, 0.08) } else { mixc(pal.paper, pal.ink, 0.03) },
                Some(mixc(pal.paper, pal.ink, 0.22)), 1);
            g_text(dc, "✕", xr, pal.ink, st.sf_small, true);
        }
    }

    // ── 行为：常驻 / 监听 / 全屏放行(游戏模式) / 开机自启 ──
    let y2 = ACT_SEC2_Y;
    g_text(dc, "行为", RECT { left: S_X0, top: y2, right: S_X1, bottom: y2 + ACT_LABEL_H },
        pal.ink, st.sf_bold, false);
    let rows: [&str; 3] = ["常驻模式", "监听", "全屏放行(游戏模式)"];
    for i in 0..3 {
        let rr = act_sw_row(i);
        let hv = hov(st, Ctl::Sw(i));
        if hv > 0.01 {
            g_rrect(dc, rr, 10, mixc(pal.paper, pal.ink, 0.05 * hv), None, 0);
        }
        g_text(dc, rows[i],
            RECT { left: S_X0 + 8, top: rr.top + 12, right: S_X1 - 60, bottom: rr.top + 32 },
            pal.ink, st.sf_body, false);
        g_switch(dc, pal, act_sw_rect(i), act_sw_val(st, i));
        draw_tip_icon(st, dc, pal, act_tip_rect(i), i as u8 + 1);
    }
    // 开机自启（从「高级」页移入交互页；状态直接读注册表，不走 sw_a 动画量）
    {
        let rr = act_sw_row(3);
        let hv = hov(st, Ctl::Autostart);
        if hv > 0.01 {
            g_rrect(dc, rr, 10, mixc(pal.paper, pal.ink, 0.05 * hv), None, 0);
        }
        g_text(dc, "开机自启",
            RECT { left: S_X0 + 8, top: rr.top + 12, right: S_X1 - 60, bottom: rr.top + 32 },
            pal.ink, st.sf_body, false);
        // 与其它开关一样走 `sw_a[2]` 动画量（在 `setup_tick` 里按 `st.autostart` 收敛）
        g_switch(dc, pal, act_sw_rect(3), st.sw_a[2]);
    }
    // 显示应用名称（悬停内环图标 → 头像气泡报名字；门控在 `bub_text` 里）
    {
        let rr = act_sw_row(4);
        let hv = hov(st, Ctl::Sw(4));
        if hv > 0.01 {
            g_rrect(dc, rr, 10, mixc(pal.paper, pal.ink, 0.05 * hv), None, 0);
        }
        g_text(dc, "显示应用名称",
            RECT { left: S_X0 + 8, top: rr.top + 12, right: S_X1 - 60, bottom: rr.top + 32 },
            pal.ink, st.sf_body, false);
        g_switch(dc, pal, act_sw_rect(4), act_sw_val(st, 4));
        draw_tip_icon(st, dc, pal, act_tip_rect(4), 6);
    }

    // ── 黑白名单：两张卡片，**二选一**生效（点卡片标题切换当前生效方）──
    let ty = EXC_CARDS_TITLE_Y;
    g_text(dc, "黑白名单", RECT { left: S_X0, top: ty, right: S_X1 - 8, bottom: ty + ACT_LABEL_H },
        pal.ink, st.sf_bold, false);
    let active_c = st.exc_mode as usize;
    for c in 0..2usize {
        let card = exc_card_rect(c);
        let list: &Vec<String> = if c == 0 { &st.exc } else { &st.exc_w };
        let active = c == active_c;
        let has = !list.is_empty();
        // 当前生效且有内容 → 粗墨描边；生效但空 → 浅边；未启用 → 更浅的灰边（一眼区分）
        let edge = if active && has { pal.ink } else { mixc(pal.paper, pal.ink, 0.18) };
        let head_hv = hov(st, Ctl::ExcCard(c as u8));
        let base = if active { mixc(pal.paper, pal.ink, 0.02) } else { pal.paper };
        g_rrect(dc, card, 14, mixc(base, pal.ink, 0.05 * head_hv),
            Some(edge), if active && has { 2 } else { 1 });
        let nm = if c == 0 { "黑名单" } else { "白名单" };
        let dim = if active { pal.ink } else { mixc(pal.paper, pal.ink, 0.42) };
        g_text(dc, nm,
            RECT { left: card.left + 14, top: card.top + 12, right: card.right - 92, bottom: card.top + 34 },
            dim, st.sf_bold, false);
        draw_tip_icon(st, dc, pal, exc_tip_rect(c), 4 + c as u8);
        if active && has {
            let dot = RECT { left: card.right - 72, top: card.top + 15, right: card.right - 14, bottom: card.top + 31 };
            g_rrect(dc, dot, 8, pal.block, None, 0);
            g_text(dc, "生效中", dot, pal.paper, st.sf_small, true);
        } else if !active {
            let tag = RECT { left: card.right - 72, top: card.top + 15, right: card.right - 14, bottom: card.top + 31 };
            g_rrect(dc, tag, 8, mixc(pal.paper, pal.ink, 0.06), Some(mixc(pal.paper, pal.ink, 0.22)), 1);
            g_text(dc, "未启用", tag, mixc(pal.paper, pal.ink, 0.42), st.sf_small, true);
        }
        if list.is_empty() {
            g_text(dc, "把 exe 拖进来",
                RECT { left: card.left + 14, top: card.top + 46, right: card.right - 10, bottom: card.top + 66 },
                mixc(pal.paper, pal.ink, 0.40), st.sf_small, false);
        }
        for (i, nn) in list.iter().take(EXC_ROWS).enumerate() {
            let rr = exc_row_rect(c, i);
            if active {
                let dh = hov(st, Ctl::ExcDel(c as u8, i));
                if dh > 0.01 {
                    g_rrect(dc, rr, 7, mixc(pal.paper, pal.ink, 0.06 * dh), None, 0);
                }
                g_text(dc, "×", exc_del_rect(c, i),
                    if dh > 0.01 { pal.ink } else { mixc(pal.paper, pal.ink, 0.45) }, st.sf_small, true);
            }
            g_text(dc, nn,
                RECT { left: rr.left + 6, top: rr.top + 3, right: rr.right - 24, bottom: rr.bottom - 1 },
                dim, st.sf_small, false);
        }
        if list.len() > EXC_ROWS {
            let t = card.top + 42 + EXC_ROWS as i32 * 26 - 2;
            g_text(dc, &format!("…还有 {} 项", list.len() - EXC_ROWS),
                RECT { left: card.left + 14, top: t, right: card.right - 10, bottom: t + 18 },
                mixc(pal.paper, pal.ink, 0.45), st.sf_small, false);
        }
        if active {
            let pr = exc_plus_rect(c);
            let ph = hov(st, Ctl::ExcPlus(c as u8));
            g_rrect(dc, pr, 8, mixc(pal.paper, pal.ink, 0.05 + 0.06 * ph), None, 0);
            g_text(dc, "＋ 添加 exe", pr, pal.ink, st.sf_small, true);
        } else {
            g_text(dc, "（切到上方模式才生效）",
                RECT { left: card.left + 14, top: card.bottom - 28, right: card.right - 10, bottom: card.bottom - 8 },
                mixc(pal.paper, pal.ink, 0.40), st.sf_small, false);
        }
    }
}

/// 「?」小圆标：悬停时变深，同时右边/下面弹出 tooltip
unsafe fn draw_tip_icon(st: &State, dc: HDC, pal: &Pal, r: RECT, id: u8) {
    let on = st.tip == id;
    let bg = mixc(pal.paper, pal.ink, if on { 0.22 } else { 0.09 });
    g_rrect(dc, r, (r.bottom - r.top) / 2, bg, None, 0);
    g_text(dc, "?", r, mixc(pal.paper, pal.ink, if on { 0.78 } else { 0.55 }), st.sf_small, true);
}

unsafe fn s_paint_apps(st: &mut State, dc: HDC, pal: &Pal) {
    let y = ACT_TOP_Y;
    g_text(dc, "内环应用",
        RECT { left: S_X0, top: y, right: S_X1, bottom: y + ACT_LABEL_H }, pal.ink, st.sf_bold, false);

    let saved = SaveDC(dc);
    // ⚠️ 裁剪区也要走 sxv 缩放 —— 这个 DC 是 3× 的，混着写会把列表裁到只剩第一行
    let _ = IntersectClipRect(dc, sxv(0), sxv(S_LIST_TOP - 6), sxv(S_W), sxv(S_LIST_BOT));
    let scroll = st.set_scroll;
    let (rows, _) = s_rows(st);
    for row in &rows {
        let ry = S_LIST_TOP - scroll + row.y;
        if ry + row.h < S_LIST_TOP - 6 || ry > S_LIST_BOT {
            continue;
        }
        let head = row.kind == 0;
        let r = if head {
            s_row_rect(ry, row.h)
        } else {
            RECT { left: S_X0 + 26, top: ry, right: S_X1, bottom: ry + row.h - 4 }
        };
        let del = RECT { left: r.right - 38, top: r.top + 8, right: r.right - 6, bottom: r.top + 30 };
        let del_ctl = if head { Ctl::AppDel(row.ai) } else { Ctl::ItemDel(row.ai, row.ii) };

        if row.kind == 2 {
            let web = app_accepts_url(&st.apps[row.ai]);
            let editing = st.set_edit_ai == row.ai;
            let r2 = if editing {
                s_add_input(ry)
            } else {
                s_add_pill(dc, st, ry, web, 0)
            };
            if editing {
                let fg = mixc(pal.paper, pal.ink, 0.85);
                g_rrect(dc, r2, 8, mixc(pal.paper, pal.ink, 0.05),
                    Some(mixc(pal.paper, pal.ink, 0.5)), 2);
                let buf = st.set_edit_buf.clone();
                let tr = RECT { left: r2.left + 10, top: r2.top, right: r2.right - 10, bottom: r2.bottom };
                if buf.is_empty() {
                    g_text(dc, "输入网址，回车添加", tr,
                        mixc(pal.paper, pal.ink, 0.35), st.sf_small, false);
                } else {
                    g_text(dc, &buf, tr, pal.ink, st.sf_small, false);
                }
                // 光标：0.5s 方波闪烁，画在文字末尾（相位必须按当前时间算，见环上同名逻辑）
                if (now_s() * 2.0).fract() < 0.5 {
                    let tw = if buf.is_empty() {
                        0
                    } else {
                        s_text_w(dc, &buf, st.sf_small)
                    };
                    let cx = (tr.left + tw).min(r2.right - 12);
                    g_rrect(dc, RECT { left: cx + 1, top: r2.top + 6, right: cx + 2, bottom: r2.bottom - 6 },
                        0, fg, None, 0);
                }
            } else {
                // 一颗一画（非浏览器有两颗：文件 / 文件夹）
                for (m, label) in add_pill_labels(web).iter().enumerate() {
                    let pr = s_add_pill(dc, st, ry, web, m);
                    let ahv = hov(st, Ctl::AddItem(row.ai, m as u8));
                    g_rrect(dc, pr, S_PILL_H / 2, mixc(pal.paper, pal.ink, 0.06 * ahv),
                        Some(mixc(mixc(pal.paper, pal.ink, 0.14), pal.edge, ahv)), 2);
                    g_text(dc, label, pr,
                        mixc(mixc(pal.paper, pal.ink, 0.55), pal.ink, ahv), st.sf_small, true);
                }
            }
            continue;
        }

        let bg = if head {
            mixc(pal.paper, pal.ink, 0.05 + 0.05 * hov(st, Ctl::AppHead(row.ai)))
        } else {
            pal.paper
        };
        g_rrect(dc, r, 9, bg, Some(mixc(pal.paper, pal.ink, 0.14)), 2);

        let (name, target) = if head {
            (&st.apps[row.ai].name, st.apps[row.ai].target.clone())
        } else {
            (
                &st.apps[row.ai].items[row.ii].name,
                st.apps[row.ai].items[row.ii].target.clone(),
            )
        };
        let tx = r.left + 12;
        let nm_r = RECT { left: tx, top: r.top, right: tx + 170, bottom: r.bottom };
        let tg_r = RECT { left: tx + 176, top: r.top, right: r.right - 52, bottom: r.bottom };
        // 后台还在抓这个网址的 `<title>` → 名字先显示跳动的点（标题回来就换成真名）
        let waiting = !head && st.title_busy.iter().any(|u| u.as_str() == target.as_str());
        if waiting {
            let dots = ".".repeat(((now_s() * 2.0) as usize) % 3 + 1);
            g_text(dc, &format!("正在取标题{dots}"), nm_r,
                mixc(pal.paper, pal.ink, 0.45), st.sf_small, false);
        } else {
            g_text(dc, name, nm_r,
                if head { pal.ink } else { mixc(pal.paper, pal.ink, 0.75) },
                if head { st.sf_body } else { st.sf_small }, false);
        }
        g_text(dc, &target, tg_r, mixc(pal.paper, pal.ink, 0.45), st.sf_small, false);

        let dhv = hov(st, del_ctl);
        if dhv > 0.01 {
            g_rrect(dc, del, 8, mixc(pal.paper, pal.ink, dhv), None, 0);
        }
        g_text(dc, "×", del, mixc(mixc(pal.paper, pal.ink, 0.5), pal.paper, dhv), st.sf_small, true);
    }
    let _ = RestoreDC(dc, saved);

    // 底部：添加入口 + 备份提示（hover 在「描边态」和「实心态」之间渐变）
    let ba = RECT { left: S_X0, top: S_H - 64, right: S_X0 + 190, bottom: S_H - 28 };
    let bhv = hov(st, Ctl::AddApp);
    if bhv > 0.02 {
        g_shadow(dc, ba, 10, pal);
    }
    g_rrect(dc, ba, 10, mixc(pal.paper, pal.ink, bhv), Some(mixc(pal.edge, pal.ink, bhv)), 2);
    g_text(dc, "＋ 添加应用…", ba, mixc(pal.ink, pal.paper, bhv), st.sf_body, true);
    g_text(dc, "每次改动都会先把旧配置备份成 settings.ini.bak",
        RECT { left: S_X0 + 206, top: S_H - 64, right: S_X1, bottom: S_H - 28 },
        mixc(pal.paper, pal.ink, 0.45), st.sf_small, false);
}

/// 把**环中心那个头像**画到设置页的 hi 缓冲上（「关于」页的 logo）。
///
/// 与环上共用 `draw_hub_face` —— 两处长得必须一模一样（用户 2026-10-16 要求）。
/// ⚠️ 三个关键点：
///   ① 必须画在 **hi（SS×）缓冲**上、且在降采样**之前**（抗锯齿靠降采样给）；
///   ② 头像 SDF 按全局 `CANVAS` 定宽 → 先渲染到那块画布，再搬中心一块过来；
///   ③ 在 CANVAS 上按 **SS 倍半径**画 = 目标像素尺寸 → 搬运是 **1:1**，不用缩放。
unsafe fn s_draw_avatar_logo(st: &mut State, cx: i32, cy: i32, r: i32) {
    let n = (CANVAS * CANVAS * 4) as usize;
    if st.hub_buf.len() != n {
        st.hub_buf = vec![0u8; n];
    }
    let pal = pal_of(st.glass_liquid);
    let now = now_s() as f64;
    {
        let mut src = std::mem::take(&mut st.hub_buf);
        for b in src.iter_mut() {
            *b = 0;
        }
        draw_hub_face(&*st, &mut src, CC, CC, (r * SS) as f32, &pal, now);
        st.hub_buf = src;
    }

    let half = (r + 4) * SS; // 含投影余量（设备 px）
    let size = half * 2;
    let hi_w = (S_WW * SS) as usize;
    let hi_h = (S_HH * SS) as usize;
    let cw = CANVAS as usize;
    let dx0 = (cx + S_M) * SS - half;
    let dy0 = (cy + S_M) * SS - half;
    let hub = std::slice::from_raw_parts(st.hub_buf.as_ptr(), n);
    let hi = std::slice::from_raw_parts_mut(st.s_hi_bits, hi_w * hi_h * 4);
    for y in 0..size {
        let ty = dy0 + y;
        if ty < 0 || ty >= hi_h as i32 {
            continue;
        }
        let sy = CC as i32 - half + y;
        if sy < 0 || sy >= CANVAS {
            continue;
        }
        for x in 0..size {
            let tx = dx0 + x;
            if tx < 0 || tx >= hi_w as i32 {
                continue;
            }
            let sx = CC as i32 - half + x;
            if sx < 0 || sx >= CANVAS {
                continue;
            }
            let so = (sy as usize * cw + sx as usize) * 4;
            // 头像缓冲是**预乘 alpha** 的 BGRA：直接 over 到（未预乘的）hi 缓冲
            let a = hub[so + 3] as f32 / 255.0;
            if a <= 0.002 {
                continue;
            }
            let to = (ty as usize * hi_w + tx as usize) * 4;
            for c in 0..3 {
                hi[to + c] = (hub[so + c] as f32 + hi[to + c] as f32 * (1.0 - a))
                    .clamp(0.0, 255.0) as u8;
            }
        }
    }
}

/// 量一段文字在指定字体下的宽度（设置页排版用）。
/// 返回**逻辑像素**（已除去超采样倍率）——「产品名后面紧跟一枚版本胶囊」这种
/// 贴边排版不能靠估中文字宽，估偏一点就是错位。
unsafe fn s_text_w(dc: HDC, text: &str, font: HFONT) -> i32 {
    let op = SelectObject(dc, HGDIOBJ(font.0));
    let mut t: Vec<u16> = text.encode_utf16().collect();
    let mut r = RECT { left: 0, top: 0, right: 0, bottom: 0 };
    let _ = DrawTextW(dc, &mut t, &mut r, DT_SINGLELINE | DT_NOPREFIX | DT_CALCRECT);
    let _ = SelectObject(dc, op);
    ((r.right - r.left) as f32 / SS as f32).ceil() as i32
}

/// 「关于」页的分段：小标题（`sf_bold`）+ 若干正文行（`sf_small`，行距 24）。
/// 节奏与其它页一致（标题底 +`ACT_GAP` 起正文），别在这里写死额外的间距常量。
unsafe fn s_about_section(
    st: &State,
    dc: HDC,
    pal: &Pal,
    cy: &mut i32,
    title: &str,
    lines: &[String],
) {
    g_text(dc, title, RECT { left: S_X0, top: *cy, right: S_X1, bottom: *cy + ACT_LABEL_H },
        pal.ink, st.sf_bold, false);
    *cy += ACT_LABEL_H + ACT_GAP - 2;
    for l in lines.iter() {
        g_text(dc, l, RECT { left: S_X0, top: *cy, right: S_X1, bottom: *cy + 20 },
            mixc(pal.paper, pal.ink, 0.62), st.sf_small, false);
        *cy += 24;
    }
}

/// 「关于」页里「呼出」那行的内容：**只列当前真的启用**的方式。
/// 写死一串按键等于骗人 —— 用户把中键勾掉了，关于页还写着"鼠标中键"就不对。
fn about_hotkeys(st: &State) -> String {
    let mut v: Vec<String> = Vec::new();
    if st.key_middle {
        v.push("鼠标中键".to_string());
    }
    if st.key_side {
        v.push("鼠标侧键".to_string());
    }
    if st.key_rhold {
        v.push("长按右键".to_string());
    }
    if st.trig_custom_kind == 1 {
        v.push(mouse_name(st.trig_custom_btn).to_string());
    } else if st.trig_custom_kind == 2 {
        v.push(combo_str(st.trig_custom_mods, st.trig_custom_vk));
    }
    if v.is_empty() {
        "未设置（去「交互」页勾选一个）".to_string()
    } else {
        v.join(" / ")
    }
}

/// 关于页「检查更新」按钮矩形（绘制与命中共用同一份，别写两处）。
/// 高度取 `ACT_CHIPS_H`（与设置页其它按钮/胶囊统一），单独一行放在标语下方。
fn s_update_btn_rect() -> RECT {
    // 标语顶 = base+32、行高 20；按钮再下移 14px → base+66
    let top = ACT_TOP_Y + ACT_LABEL_H + ACT_GAP + 32 + 34;
    RECT { left: S_X0, top, right: S_X0 + 168, bottom: top + ACT_CHIPS_H }
}

/// 关于页「运行信息」里那行更新状态文案。
fn update_status_text(st: &State) -> String {
    match st.update_st {
        1 => "检查中…".to_string(),
        2 => format!("已是最新（v{}）", cur_version()),
        3 => format!("发现新版本 v{} · 点右上角「更新」", st.update_latest),
        4 => format!("检查失败：{}", st.update_msg),
        5 => format!("新版本 v{} 已下载并校验 · 点右上角「重启并更新」", st.update_latest),
        6 => "下载中…（校验 SHA256 后提示重启）".to_string(),
        _ => format!("未检查（当前 v{}）", cur_version()),
    }
}

unsafe fn s_paint_adv(st: &mut State, dc: HDC, pal: &Pal) {
    let y = ACT_TOP_Y;
    g_text(dc, "关于", RECT { left: S_X0, top: y, right: S_X1, bottom: y + ACT_LABEL_H },
        pal.ink, st.sf_bold, false);

    // ── 产品头：头像 logo + 名字 + 版本胶囊 + 一句话定位 + 细分割线 ──
    //
    // logo = **轮盘中心那个头像**（共用 `draw_hub_face`，不是再画一个相似图形）。
    // ⚠️ 不贴 exe 图标：图标是黑底白点，深色主题下整块看不见；现画取主题色，明暗都成立。
    let mut cy = y + ACT_LABEL_H + ACT_GAP;
    let mc = cy + 14; // logo 竖中心（与 28px 的名字行对齐）
    s_draw_avatar_logo(st, S_X0 + 15, mc, 11);

    let tx = S_X0 + 38;
    let name = "PiPie";
    let nw = s_text_w(dc, name, st.sf_h1);
    g_text(dc, name, RECT { left: tx, top: cy, right: tx + nw + 4, bottom: cy + 28 },
        pal.ink, st.sf_h1, false);
    let ver = format!("v{}", env!("CARGO_PKG_VERSION"));
    let pw = s_text_w(dc, &ver, st.sf_small) + 20;
    let vr = RECT { left: tx + nw + 12, top: cy + 5, right: tx + nw + 12 + pw, bottom: cy + 23 };
    g_rrect(dc, vr, 9, mixc(pal.paper, pal.ink, 0.07), Some(pal.edge), 2);
    g_text(dc, &ver, vr, mixc(pal.paper, pal.ink, 0.55), st.sf_small, true);

    cy += 32;
    g_text(dc, "常驻后台，一键呼出；图标绕成一圈，指到就用",
        RECT { left: tx, top: cy, right: S_X1, bottom: cy + 20 },
        mixc(pal.paper, pal.ink, 0.55), st.sf_small, false);

    // 「检查更新」按钮：整行 chip 高度（和设置页其它按钮统一），单独一行放标语下方
    let ub = s_update_btn_rect();
    let (ulabel, ufill) = match st.update_st {
        1 => ("检查中…".to_string(), mixc(pal.paper, pal.ink, 0.04)),
        2 => ("已是最新".to_string(), mixc(pal.paper, pal.ink, 0.04)),
        3 => ("更新".to_string(), mixc(pal.paper, pal.ink, 0.14)),
        4 => ("重试".to_string(), mixc(pal.paper, pal.ink, 0.04)),
        5 => ("重启并更新".to_string(), mixc(pal.paper, pal.ink, 0.14)),
        6 => (
            format!("下载中 {}%", UPDATE_PROGRESS.load(std::sync::atomic::Ordering::Relaxed)),
            mixc(pal.paper, pal.ink, 0.04),
        ),
        _ => ("检查更新".to_string(), mixc(pal.paper, pal.ink, 0.06)),
    };
    g_rrect(dc, ub, 10, ufill, Some(pal.edge), 2);
    g_text(dc, &ulabel, ub, pal.ink, st.sf_small, true);
    cy = ub.bottom + ACT_GAP;

    g_line(dc, S_X0, cy, S_X1, cy, 1, mixc(pal.paper, pal.ink, 0.13));
    cy += 20;

    // ── 怎么用（呼出方式按当前设置动态生成）──
    let how = [
        format!("呼出：{}", about_hotkeys(st)),
        "指到图标即选中 · 松开或单击执行 · Esc 取消".to_string(),
        "把文件、文件夹、网页或窗口标题栏拖进来即可添加".to_string(),
        "双击图标改名 · 按住拖动换位 · 拖到环外移除".to_string(),
        "黑白名单在「交互」页设置，名单里的应用不会弹出".to_string(),
    ];
    s_about_section(st, dc, pal, &mut cy, "怎么用", &how);
    cy += 8;

    // ── 运行信息（现算，不写死）──
    let sz = std::fs::metadata(exe_path()).map(|m| m.len()).unwrap_or(0);
    let items = st.apps.iter().map(|a| a.items.len()).sum::<usize>();
    let info = [
        format!("单文件 {} KB · 无外部依赖 · 空闲内存约 1 MB", sz / 1024),
        format!("已收录 {} 个应用 · 共 {} 条外环条目", st.apps.len(), items),
        format!("更新：{}", update_status_text(st)),
        format!("配置：{}（每次改动自动备份为 .bak）", st.ini_path),
    ];
    s_about_section(st, dc, pal, &mut cy, "运行信息", &info);
    let _ = cy;

    g_text(dc, "© 2026 PiPie · 配置只保存在本机",
        RECT { left: S_X0, top: S_H - 64, right: S_X1, bottom: S_H - 28 },
        mixc(pal.paper, pal.ink, 0.4), st.sf_small, false);
}

// ── 命中 ──

fn s_hit(st: &State, x: i32, y: i32) -> Ctl {
    // ⚠️ 引导进行中 → **只读**：命中测试一律返回 `None`（卡片上的按钮在鼠标处理里
    // 单独判），**连关闭按钮也不响应** —— 否则「遮罩盖着却还能点」就露馅了。
    if st.guide_step >= 0 {
        return Ctl::None;
    }
    // 删除确认卡开着 = 模态：只认卡上那两颗按钮，卡外一律 `None`
    // （点卡外由 `s_click` 解释成「取消」；这里返回 None 才不会让「取消」到处发亮）
    if st.set_confirm != Ctl::None {
        let r = s_conf_rect();
        if in_rect(s_conf_btn(r, 0), x, y) {
            return Ctl::ConfNo;
        }
        if in_rect(s_conf_btn(r, 1), x, y) {
            return Ctl::ConfYes;
        }
        return Ctl::None;
    }
    if x >= S_W - 50 && y < S_TITLE_H && x >= S_W - 46 - 6 {
        return Ctl::Close;
    }
    if in_rect(s_guide_entry_rect(), x, y) {
        return Ctl::GuideEntry;
    }
    for i in 0..S_PAGES.len() {
        if in_rect(s_nav_rect(i), x, y) {
            return Ctl::Nav(i);
        }
    }
    match st.set_page {
        0 => {
            for i in 0..3 {
                if in_rect(s_strip(i, ACT_CHIPS_Y, 108, 10, ACT_CHIPS_H), x, y) {
                    return Ctl::Theme(i);
                }
            }
            for i in 0..2 {
                if in_rect(s_strip(i, LOOK_MAT_Y, 108, 10, ACT_CHIPS_H), x, y) {
                    return Ctl::Mat(i);
                }
            }
        }
        1 => {
            for i in 0..4 {
                if in_rect(s_strip(i, ACT_CHIPS_Y, 118, 10, ACT_CHIPS_H), x, y) {
                    return if i == 3 { Ctl::TrigCapture } else { Ctl::Key(i) };
                }
            }
            // 录制中的「取消录制」按钮（先于下面的 ✕ 判 —— 两者位置重叠，
            // 但一个只在录制态出现、一个只在非录制态出现，互斥）
            if st.trig_capturing && in_rect(trig_cancel_rect(), x, y) {
                return Ctl::TrigCancel;
            }
            // 自定义触发「✕」清除（已设置时），先于行为开关行判
            if in_rect(trig_clear_rect(), x, y) && st.trig_custom_kind != 0 {
                return Ctl::TrigClear;
            }
            // ⚠️⚠️ 「?」必须**先于开关行**判：它就落在 `act_sw_row` 里面，
            //    原来先判开关行 → 永远返回 `Sw` → 悬停「?」出不了 tooltip
            //    （用户 2026-10-11：「那个问号 hover 后没有展示 tooltip」）。
            for i in [0usize, 1, 2, 4] {
                if in_rect(act_tip_rect(i), x, y) {
                    // 行 0/1/2 → tip 1/2/3；行 4（显示应用名称）→ tip 6
                    return Ctl::Tip(if i == 4 { 6 } else { i as u8 + 1 });
                }
            }
            for i in 0..5 {
                if in_rect(act_sw_row(i), x, y) {
                    return if i == 3 { Ctl::Autostart } else { Ctl::Sw(i) };
                }
            }
            for c in 0..2usize {
                // 卡片头里的「?」优先于卡片头本身
                if in_rect(exc_tip_rect(c), x, y) {
                    return Ctl::Tip(4 + c as u8);
                }
            }
            // 点卡片标题（卡片头）= 切到该名单为当前生效方（二选一，非独立切换器）
            for c in 0..2usize {
                if in_rect(exc_head_rect(c), x, y) {
                    return Ctl::ExcCard(c as u8);
                }
            }
            // 只有**当前生效**的那张卡片能增删（未启用的卡片不响应点击）
            let c = st.exc_mode as usize;
            for i in 0..EXC_ROWS {
                if in_rect(exc_del_rect(c, i), x, y) {
                    return Ctl::ExcDel(c as u8, i);
                }
            }
            if in_rect(exc_plus_rect(c), x, y) {
                return Ctl::ExcPlus(c as u8);
            }
        }
        2 => {
            let ba = RECT { left: S_X0, top: S_H - 64, right: S_X0 + 190, bottom: S_H - 28 };
            if in_rect(ba, x, y) {
                return Ctl::AddApp;
            }
            if y < S_LIST_TOP - 6 || y > S_LIST_BOT {
                return Ctl::None;
            }
            let scroll = st.set_scroll;
            let (rows, _) = s_rows(st);
            for row in &rows {
                let ry = S_LIST_TOP - scroll + row.y;
                if row.kind == 2 {
                    // 只有胶囊本身可点（编辑中是那条输入框），整行不算命中
                    if st.set_edit_ai == row.ai {
                        if in_rect(s_add_input(ry), x, y) {
                            return Ctl::AddItem(row.ai, 0);
                        }
                        continue;
                    }
                    let web = app_accepts_url(&st.apps[row.ai]);
                    for m in 0..add_pill_labels(web).len() {
                        if in_rect(unsafe { s_add_pill(st.txt_dc, st, ry, web, m) }, x, y) {
                            return Ctl::AddItem(row.ai, m as u8);
                        }
                    }
                    continue;
                }
                let r = if row.kind == 0 {
                    s_row_rect(ry, row.h)
                } else {
                    RECT { left: S_X0 + 26, top: ry, right: S_X1, bottom: ry + row.h - 4 }
                };
                if !in_rect(r, x, y) {
                    continue;
                }
                let del = RECT { left: r.right - 38, top: r.top + 8, right: r.right - 6, bottom: r.top + 30 };
                return match row.kind {
                    0 => {
                        if in_rect(del, x, y) {
                            Ctl::AppDel(row.ai)
                        } else {
                            Ctl::AppHead(row.ai)
                        }
                    }
                    _ => {
                        if in_rect(del, x, y) {
                            Ctl::ItemDel(row.ai, row.ii)
                        } else {
                            Ctl::None
                        }
                    }
                };
            }
        }
        3 => {
            // 关于页：「检查更新」按钮
            if in_rect(s_update_btn_rect(), x, y) {
                return Ctl::UpdateCheck;
            }
        }
        _ => {}
    }
    Ctl::None
}

fn in_rect(r: RECT, x: i32, y: i32) -> bool {
    x >= r.left && x < r.right && y >= r.top && y < r.bottom
}

// ── 动作 ──

unsafe fn open_settings(st: &mut State) {
    let h = SET_HWND.load(Ordering::SeqCst);
    if h == 0 {
        return;
    }
    let hwnd = HWND(h as *mut c_void);
    let (sw, sh) = (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN));
    st.set_x = (sw - S_WW) / 2;
    st.set_y = (sh - S_HH) / 2;
    let _ = SetWindowPos(hwnd, None, st.set_x, st.set_y, S_WW, S_HH, SWP_NOZORDER);
    st.set_hover = Ctl::None;
    st.set_hover_prev = Ctl::None;
    // 关窗时没收回的输入框：重开不该停在「打字打到一半」的脏状态
    st.set_edit_ai = usize::MAX;
    st.set_edit_buf.clear();
    // 确认卡同理：卡存的是**下标**，重开时列表可能已经变了（拖放/热重载都会动它）
    st.set_confirm = Ctl::None;
    // 首启引导：**只在没看过时自动播一次**（`guide_seen` 落 ini，跳过了也不再弹）；
    // 想重看就点标题栏的「查看引导」。
    // `guide_start` 内部会切到交互页 —— 4 步高亮的都是交互页的控件。
    if !st.guide_seen {
        st.guide_seen = true;
        guide_start(st);
        save_settings(st);
    }
    setup_present(st);
    let _ = ShowWindow(hwnd, SW_SHOW);
    let _ = SetForegroundWindow(hwnd);
}

/// 关设置窗口时必须取消「录制自定义触发」——
/// 否则窗口一关，**下一次任意鼠标点击会被钩子当成录到的触发吞掉**（用户看不到任何提示），
/// 重开设置还会停在「请按下…」= 不可见的脏状态（本项目视为已损坏）。
unsafe fn cancel_trig_capture() {
    let h = SET_HWND.load(Ordering::SeqCst);
    if h != 0 {
        let st = state_of(HWND(h as *mut c_void));
        st.trig_capturing = false;
        st.trig_capture_bad = 0;
        CAP_MODS_LIVE.store(0, Ordering::SeqCst);
        CAP_BAD.store(0, Ordering::SeqCst);
        // 取消后按「是否已设键盘触发」还原武装状态（不留常驻钩子）
        if st.trig_custom_kind == 2 {
            apply_key_opts(st); // 键盘触发仍在 → 重新挂键盘钩子
        } else {
            OPT_CUSTOM_VK.store(0, Ordering::SeqCst);
            OPT_CUSTOM_MODS.store(0, Ordering::SeqCst);
            need_key_hook(false); // 无键盘触发 → 摘钩子
        }
    }
    CAPTURE_TRIGGER.store(false, Ordering::SeqCst);
}

// ── 应用页「＋ 网址」：内联输入框 + 后台抓标题改名 ──

/// 后台抓 `<title>` 的回信信箱：`(网址, 标题)`（None = 没抓到，条目留着域名当名字）
static TITLE_TX: std::sync::Mutex<Vec<(String, Option<String>)>> = std::sync::Mutex::new(Vec::new());

/// 输入框里的东西 → 可点击的网址。没写协议就补 `https://`；
/// 一眼就不像网址（连点都没有，例如「百度」）就返回空串，**不添加**、留在输入框里让用户改。
fn normalize_url(raw: &str) -> String {
    let s = raw.trim();
    if s.is_empty() {
        return String::new();
    }
    let l = s.to_ascii_lowercase();
    if l.starts_with("http://") || l.starts_with("https://") || l.starts_with("file:///") {
        return s.to_string();
    }
    let host = s.split('/').next().unwrap_or(s);
    if !host.contains('.') && !l.starts_with("localhost") {
        return String::new();
    }
    format!("https://{s}")
}

/// 剪贴板的纯文本（内联输入框按 Ctrl+V 用）。只认 `CF_UNICODETEXT`，最多 2048 字符。
unsafe fn clip_text() -> Option<String> {
    if OpenClipboard(HWND(ptr::null_mut())).is_err() {
        return None;
    }
    let mut out: Option<String> = None;
    if let Ok(h) = GetClipboardData(13) {
        let hg = HGLOBAL(h.0);
        let p = GlobalLock(hg);
        if !p.is_null() {
            let ws = p as *const u16;
            let mut len = 0usize;
            while *ws.add(len) != 0 && len < 2048 {
                len += 1;
            }
            out = Some(String::from_utf16_lossy(std::slice::from_raw_parts(ws, len)));
            let _ = GlobalUnlock(hg);
        }
    }
    let _ = CloseClipboard();
    out.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// 起后台线程抓这个网址的标题（复用「已打开就切到那个标签」那套 `fetch_page_title`
/// + `SWITCH_CACHE`）。⚠️ 1.2 秒超时只是**下限**，慢站能在后台挂好几秒 ——
/// 同步跑在 UI 线程上就是「按了回车整页钉住」，所以一律走线程 + `WM_APP_TITLE` 回信。
unsafe fn title_fetch_start(st: &mut State, url: &str) {
    if let Some(t) = cache_get(url) {
        apply_title(st, url, Some(&t));
        return;
    }
    if st.title_busy.iter().any(|u| u == url) {
        return;
    }
    st.title_busy.push(url.to_string());
    let u = url.to_string();
    std::thread::spawn(move || unsafe {
        // 单次 1.2s 超时经常空手（实测 example.org 连跑 6 次有 2 次没抓到）——
        // 这条线程不阻塞任何人，再给一次机会。
        let mut got = fetch_page_title(&u);
        if got.is_none() {
            got = fetch_page_title(&u);
        }
        let got = got.map(|t| {
            cache_put(&u, &t);
            t
        });
        if let Ok(mut b) = TITLE_TX.lock() {
            b.push((u, got));
        }
        let h = SET_HWND.load(Ordering::SeqCst);
        if h != 0 {
            let _ = PostMessageW(HWND(h as *mut c_void), WM_APP_TITLE, WPARAM(0), LPARAM(0));
        }
    });
}

/// 把抓到的标题写进那条网址条目（按 `target` 找，不按下标 —— 等待期间用户可能
/// 又加/删了条目，下标会漂）。抓不到就留着原来的域名名。
unsafe fn apply_title(st: &mut State, url: &str, title: Option<&str>) {
    st.title_busy.retain(|u| u != url);
    let Some(t) = title else { return };
    let name = clamp_name(t);
    if name.is_empty() {
        return;
    }
    let mut changed = false;
    for a in st.apps.iter_mut() {
        for it in a.items.iter_mut() {
            if it.kind == 0 && it.target == url && it.name != name {
                it.name = name.clone();
                changed = true;
            }
        }
    }
    if changed {
        save_settings(st);
    }
}

/// 收信：把后台抓到的标题一律应用掉（设置窗口可能已经关了 → 消息根本不会进来）
unsafe fn title_apply_pending(st: &mut State) {
    let got = match TITLE_TX.lock() {
        Ok(mut t) if !t.is_empty() => std::mem::take(&mut *t),
        _ => return,
    };
    for (url, title) in got {
        apply_title(st, &url, title.as_deref());
    }
}

/// 打开/保持应用页的内联网址输入框（只有浏览器类应用有这一态）
fn set_edit_begin(st: &mut State, ai: usize) {
    if st.set_edit_ai != ai {
        st.set_edit_ai = ai;
        st.set_edit_buf.clear();
    }
}

/// 退出输入框。`commit` = 按回车的语义：缓冲能规整成网址就加一条，否则**不收**——
/// 留在框里让用户接着改（打错字按回车不该悄悄丢内容）。
/// `commit=false`（Esc / 点到别处）= 直接收回并丢掉缓冲。
unsafe fn set_edit_finish(st: &mut State, commit: bool) {
    let ai = st.set_edit_ai;
    if ai == usize::MAX {
        return;
    }
    if !commit {
        st.set_edit_ai = usize::MAX;
        st.set_edit_buf.clear();
        setup_present(st);
        return;
    }
    let url = normalize_url(&st.set_edit_buf);
    if url.is_empty() {
        setup_present(st);
        return;
    }
    st.set_edit_ai = usize::MAX;
    st.set_edit_buf.clear();
    add_url_item(st, ai, &url);
    setup_present(st);
}

/// 加一条网址条目：名字先用缓存标题或域名顶着，剩下的交给 `title_fetch_start`
unsafe fn add_url_item(st: &mut State, ai: usize, url: &str) {
    let Some(app) = st.apps.get(ai) else { return };
    if app.items.iter().any(|x| x.kind == 0 && x.target == url) {
        return; // 同一个网址不加第二遍
    }
    let base = cache_get(url)
        .map(|t| clamp_name(&t))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| url_host_name(url));
    let name = unique_item_name(st, ai, &base);
    st.apps[ai].items.push(ItemEntry {
        name,
        target: url.to_string(),
        color: None,
        icon: None,
        kind: 0,
        relaunch: String::new(),
        live_hwnd: 0,
    });
    if st.apps[ai].items.len() > MAX_ITEMS {
        st.apps[ai].items.truncate(MAX_ITEMS);
    }
    save_settings(st);
    title_fetch_start(st, url);
}

/// 确认卡上按「删除」之后真正执行的那一步。
///
/// ⚠️ 调用前必须**先把卡收掉**（`st.set_confirm = Ctl::None`）：
/// 删完下标全体左移，卡还挂着旧下标就会指着另一行。
unsafe fn s_do_del(st: &mut State, t: Ctl) {
    match t {
        Ctl::AppDel(ai) => {
            if ai >= st.apps.len() {
                return;
            }
            // 删的是应用本身 → 内联网址输入框（按应用下标定身）必须一起收掉，
            // 不然下标全体左移，输入框会「隐形」挂在另一个应用上。
            st.set_edit_ai = usize::MAX;
            st.set_edit_buf.clear();
            st.apps.remove(ai);
            if st.set_expanded == ai {
                st.set_expanded = usize::MAX;
            }
            st.active_app = None;
            save_settings(st);
            load_icons(st);
        }
        Ctl::ItemDel(ai, ii) => {
            if ai < st.apps.len() && ii < st.apps[ai].items.len() {
                st.apps[ai].items.remove(ii);
                save_settings(st);
            }
        }
        Ctl::ExcDel(c, i) => {
            let bucket = if c == 0 { &mut st.exc } else { &mut st.exc_w };
            if i < bucket.len() {
                bucket.remove(i);
                save_settings(st);
            }
        }
        _ => {}
    }
}

unsafe fn s_click(st: &mut State, hwnd: HWND, x: i32, y: i32) {
    let c = s_hit(st, x, y);
    // ── 删除确认卡开着 = 模态：这一击只用来处理卡，别的控件一律不响应 ──
    if st.set_confirm != Ctl::None {
        if c == Ctl::ConfYes {
            let t = st.set_confirm;
            st.set_confirm = Ctl::None;
            s_do_del(st, t);
        } else {
            st.set_confirm = Ctl::None; // 「取消」和「点卡外」同一个意思
        }
        setup_present(st);
        setup_kick(st, hwnd);
        return;
    }
    // 输入框开着又点了别的地方 → 先收回（缓冲按「放弃」处理，不悄悄加半条网址）。
    // 例外：点的还是**自己那颗胶囊/输入框**，要保持编辑态。
    if st.set_edit_ai != usize::MAX
        && !matches!(c, Ctl::AddItem(i, _) if i == st.set_edit_ai)
    {
        set_edit_finish(st, false);
    }
    match c {
        Ctl::None => {}
        // 确认卡的按钮在上面的模态分支里已经消费完了，走到这里说明不该再有事发生。
        Ctl::ConfYes | Ctl::ConfNo => {}
        Ctl::Close => {
            cancel_trig_capture();
            let _ = ShowWindow(hwnd, SW_HIDE);
            return;
        }
        Ctl::Nav(i) => {
            if st.set_page != i {
                st.set_page = i;
                st.set_page_a = 0.0; // 切页淡入
                st.set_hover = Ctl::None;
                st.set_hover_prev = Ctl::None;
            }
        }
        Ctl::Theme(i) => {
            st.theme = i as i32;
            apply_theme(st);
            apply_key_opts(st);
            save_settings(st);
        }
        Ctl::Mat(i) => {
            let want = i == 1;
            if st.glass != want {
                st.glass = want;
                if !want {
                    // 关掉就把 900KB 的快照还回去
                    set_glass_bg(st, Vec::new());
                    st.glass_bg.shrink_to_fit();
                    glass_disc_drop();
                }
                save_settings(st);
            }
        }
        Ctl::Key(i) => {
            match i {
                0 => st.key_middle = !st.key_middle,
                1 => st.key_side = !st.key_side,
                _ => st.key_rhold = !st.key_rhold,
            }
            apply_key_opts(st);
            save_settings(st);
        }
        Ctl::TrigCapture => {
            if st.trig_capturing {
                // 再点一次芯片 = 取消录制（走同一个取消函数，别写第二份）
                cancel_trig_capture();
            } else {
                // 进入录制态：下一次**键盘组合键**即成为触发键（鼠标键不收录）
                st.trig_capturing = true;
                st.trig_capture_bad = 0;
                CAP_BAD.store(0, Ordering::SeqCst);
                CAPTURE_TRIGGER.store(true, Ordering::SeqCst);
                CAP_MODS_LIVE.store(0, Ordering::SeqCst);
                // 录制期间先摘掉旧触发，避免旧组合键在录制时误呼出
                // （落地由 host_proc 的 apply_key_opts 按新类型重新挂）
                OPT_CUSTOM_VK.store(0, Ordering::SeqCst);
                OPT_CUSTOM_MODS.store(0, Ordering::SeqCst);
                need_key_hook(true); // 临时挂键盘钩子以录制组合键
            }
            setup_kick(st, hwnd); // 状态变了 → 立刻重绘
        }
        Ctl::TrigCancel => {
            // 录制中放弃：还原到「进录制之前」的状态（有键盘触发就重新武装，没有就摘钩子）
            cancel_trig_capture();
            setup_kick(st, hwnd); // 立刻重绘，别等下一次鼠标事件
        }
        Ctl::TrigClear => {
            st.trig_custom_kind = 0;
            st.trig_custom_btn = 0;
            st.trig_custom_vk = 0;
            st.trig_custom_mods = 0;
            st.trig_capturing = false;
            st.trig_capture_bad = 0;
            CAP_BAD.store(0, Ordering::SeqCst);
            CAPTURE_TRIGGER.store(false, Ordering::SeqCst);
            need_key_hook(false); // 摘掉录制用的临时键盘钩子
            apply_key_opts(st);
            save_settings(st);
            setup_kick(st, hwnd);
        }
        Ctl::Sw(i) => match i {
            0 => {
                st.persistent = !st.persistent;
                save_settings(st);
            }
            1 => {
                let on = !st.running;
                set_running(st, on);
                save_settings(st);
            }
            2 => {
                st.exc_fs = !st.exc_fs;
                save_settings(st);
            }
            4 => {
                // 「显示应用名称」：只门控**悬停报名字**，不影响气泡的其它用途
                st.hover_name = !st.hover_name;
                save_settings(st);
            }
            _ => {}
        },
        Ctl::Autostart => {
            // ⚠️ 以 `st.autostart`（ini）为准，不再直读注册表 —— 否则「默认开」这条
            // 规则在设置页里对不上（注册表可能还没被同步过）。
            st.autostart = !st.autostart;
            autostart_set(st.autostart);
            save_settings(st);
        }
        Ctl::GuideEntry => {
            // 重播引导（跳过过的人也能找回来）
            guide_start(st);
            // ⚠️ 一并重置拖入引导的四步 —— 否则 onboarding 只播一次就再也不回来，
            //    想看第二遍只能手工删 ini。
            st.onb_drag = false;
            st.onb_move = false;
            st.onb_edit = false;
            st.onb_scroll = false;
            unsafe { save_settings(st) };
        }
        Ctl::UpdateCheck => match st.update_st {
            3 => start_download(st),  // 发现新版 → 下载+校验
            5 => apply_update(st),    // 已就绪 → 重启并更新
            1 | 6 => {}               // 检查/下载中：忽略
            _ => start_update_check(st, true),
        },
        Ctl::Tip(_) => {} // 「?」只用于悬停出 tooltip，点了不做事
        Ctl::AppHead(ai) => {
            st.set_expanded = if st.set_expanded == ai { usize::MAX } else { ai };
            st.set_scroll = st.set_scroll.min(s_scroll_max(st));
        }
        // 点 ✕ **不直接删** —— 先弹自绘确认卡（用户 2026-10-27），真删在 `s_do_del`
        Ctl::AppDel(ai) => {
            if ai < st.apps.len() {
                st.set_confirm = c;
            }
        }
        Ctl::ItemDel(ai, ii) => {
            if ai < st.apps.len() && ii < st.apps[ai].items.len() {
                st.set_confirm = c;
            }
        }
        Ctl::AddApp => {
            if let Some(p) = pick_file() {
                let name = unique_app_name(st, &name_from_path(&p));
                st.apps.push(AppEntry {
                    name,
                    target: p,
                    icon: None,
                    items: Vec::new(),
                    grad: grad_for(""),
                });
                if st.apps.len() > MAX_APPS {
                    st.apps.truncate(MAX_APPS);
                }
                save_settings(st);
                load_icons(st);
            }
        }
        Ctl::ExcCard(c) => {
            st.exc_mode = c;
            save_settings(st);
        }
        Ctl::ExcDel(cc, i) => {
            let n = if cc == 0 { st.exc.len() } else { st.exc_w.len() };
            if i < n {
                st.set_confirm = c;
            }
        }
        Ctl::ExcPlus(c) => {
            if let Some(p) = pick_file() {
                if let Some(n) = exe_name_of(&p) {
                    let bucket = if c == 0 { &mut st.exc } else { &mut st.exc_w };
                    if !bucket.iter().any(|e| e == &n) {
                        bucket.push(n);
                        save_settings(st);
                    }
                }
            }
        }

        Ctl::AddItem(ai, m) => {
            if ai < st.apps.len() {
                if app_accepts_url(&st.apps[ai]) {
                    // 浏览器（或应用本身就是个网址）→ 条目是网址，就地开内联输入框
                    set_edit_begin(st, ai);
                } else if let Some(p) = if m == 1 { pick_folder() } else { pick_file() } {
                    let name = unique_item_name(st, ai, &name_from_path(&p));
                    st.apps[ai]
                        .items
                        .push(ItemEntry { name, target: p, color: None, icon: None, kind: 0, relaunch: String::new(), live_hwnd: 0 });
                    if st.apps[ai].items.len() > MAX_ITEMS {
                        st.apps[ai].items.truncate(MAX_ITEMS);
                    }
                    save_settings(st);
                }
            }
        }
    }
    // ⚠️ 分层窗口的 WM_PAINT 是空实现，`InvalidateRect` 不会重绘任何东西 ——
    // 之前点完按钮「等一秒才有反应」，就是状态变了但没人重画，直到鼠标一动
    // 触发了 hover 重绘。这里必须显式重渲染。
    setup_present(st);
    setup_kick(st, hwnd);
}

fn name_from_path(p: &str) -> String {
    let stem = std::path::Path::new(p)
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| p.to_string());
    let n = clamp_name(&stem);
    if n.is_empty() {
        "新项目".to_string()
    } else {
        n
    }
}

fn unique_app_name(st: &State, base: &str) -> String {
    let mut n = base.to_string();
    let mut i = 2;
    while st.apps.iter().any(|a| a.name == n) {
        n = format!("{} {}", base, i);
        i += 1;
    }
    n
}

fn unique_item_name(st: &State, ai: usize, base: &str) -> String {
    let mut n = base.to_string();
    let mut i = 2;
    while st.apps[ai].items.iter().any(|it| it.name == n) {
        n = format!("{} {}", base, i);
        i += 1;
    }
    n
}

/// 关窗时把超采样缓冲还回去：hi 那张是 SS² 倍大（3× 时 ~18MB），
/// 只有设置页开着时才需要它，常驻挂着纯属浪费
unsafe fn setup_free(st: &mut State) {
    // 「关于」页 logo 的临时画布：关窗还回去（~900KB，不是小数目）
    st.hub_buf = Vec::new();
    st.hub_buf.shrink_to_fit();
    // 关窗 = 引导结束（下次打开不该停在第 N 步；重看走「查看引导」）
    st.guide_step = -1;
    st.tip = 0;
    // 设置页那 5 号字体（SS× 大号）只有设置页在用 —— GDI 每个 HFONT 各持一份
    // 字形缓存，常驻纯浪费。关窗就删，下次开窗 `ensure_setup_fonts` 重建。
    for f in [
        &mut st.sf_title, &mut st.sf_h1, &mut st.sf_nav, &mut st.sf_body,
        &mut st.sf_small, &mut st.sf_bold,
    ] {
        if !f.0.is_null() {
            // ⚠️ 用 DeleteObject 而不是 DeleteFont —— windows 0.58 的 Gdi feature 集里
            //    没有导出 DeleteFont（编译报 E0425）。HFONT 本质是 GDI 对象，
            //    DeleteObject 删字体完全合法（DeleteFont 只是它的语义包装）。
            let _ = DeleteObject(HGDIOBJ((*f).0));
            *f = HFONT(ptr::null_mut());
        }
    }
    if !st.s_hi_dc.0.is_null() {
        let _ = DeleteDC(st.s_hi_dc);
        let _ = DeleteObject(HGDIOBJ(st.s_hi_bmp.0));
        st.s_hi_dc = HDC(ptr::null_mut());
        st.s_hi_bmp = HBITMAP(ptr::null_mut());
        st.s_hi_bits = ptr::null_mut();
    }
    if !st.s_lo_dc.0.is_null() {
        let _ = DeleteDC(st.s_lo_dc);
        let _ = DeleteObject(HGDIOBJ(st.s_lo_bmp.0));
        st.s_lo_dc = HDC(ptr::null_mut());
        st.s_lo_bmp = HBITMAP(ptr::null_mut());
        st.s_lo_bits = ptr::null_mut();
    }
}

/// 设置页动画节拍
const TIMER_SETUP_FRAME: usize = 2;

/// 启动设置页动画（hover 淡入淡出 / 开关滑动 / 切页淡入）
unsafe fn setup_kick(st: &mut State, hwnd: HWND) {
    st.set_last = Instant::now();
    let _ = SetTimer(hwnd, TIMER_SETUP_FRAME, 16, None);
}

/// 让设置窗**立刻重绘一帧**。
///
/// ⚠️⚠️ 设置窗空闲时帧定时器已被 `setup_tick` 收尾 `KillTimer` 掉，而 `WM_PAINT`
/// 在本窗口是**空实现**（分层窗口自己 ULW 上屏）。所以凡是「钩子线程只改了 State、
/// 由 PostMessage 异步通知」的更新（录制落成 / 录制报错），**必须踢一下定时器**，
/// 否则界面要等到**下一次鼠标事件**（hover 变化）才刷新 ——
/// 表现就是「录完等一会才显示」（用户 2026-10-13 报）。
unsafe fn kick_setup_frame(st: &mut State) {
    let h = SET_HWND.load(Ordering::SeqCst);
    if h != 0 {
        setup_kick(st, HWND(h as *mut c_void));
    }
}

unsafe fn setup_tick(st: &mut State, hwnd: HWND) {
    let now = Instant::now();
    let dt = (now - st.set_last).as_secs_f32().clamp(0.001, 0.10);
    st.set_last = now;
    // 帧率无关的指数收敛；rate 取 26 ≈ 120ms 到位，手感接近 --spring
    let k = 1.0 - (-26.0 * dt).exp();
    let mut busy = false;

    if st.set_hov_a < 1.0 {
        st.set_hov_a += (1.0 - st.set_hov_a) * k;
        if st.set_hov_a > 0.99 {
            st.set_hov_a = 1.0;
        } else {
            busy = true;
        }
    }
    if st.set_page_a < 1.0 {
        st.set_page_a += (1.0 - st.set_page_a) * k;
        if st.set_page_a > 0.99 {
            st.set_page_a = 1.0;
        } else {
            busy = true;
        }
    }
    let mut targets = [
        if st.persistent { 1.0f32 } else { 0.0 },
        if st.running { 1.0 } else { 0.0 },
        st.sw_a[2],
        st.sw_a[3],
        st.sw_a[4],
    ];
    // 开机自启开关现画在**交互页**（第 1 页），高级页的旧位置也留着这条动画
    if st.set_page == 1 || st.set_page == 3 {
        targets[2] = if st.autostart { 1.0 } else { 0.0 };
    }
    if st.set_page == 1 {
        targets[3] = if st.exc_fs { 1.0 } else { 0.0 };
        targets[4] = if st.hover_name { 1.0 } else { 0.0 };
    }
    for i in 0..5 {
        let d = targets[i] - st.sw_a[i];
        if d.abs() > 0.003 {
            st.sw_a[i] += d * k;
            busy = true;
        } else {
            st.sw_a[i] = targets[i];
        }
    }

    // 内联网址输入框：光标要闪；后台还在抓标题：名字那串点要跳 —— 两种都得继续出帧。
    // ⚠️ 但窗口已经藏了（点 × / 按 Esc 关掉）就不能再算编辑态忙：那条状态是「粘性」的，
    //    没人再点一下它就永远没人取消 → 16ms 定时器会一直全速重绘一页看不见的东西。
    //    （重开时 `open_settings` 会把输入框收回干净。）
    if (st.set_edit_ai != usize::MAX && IsWindowVisible(hwnd).as_bool())
        || !st.title_busy.is_empty()
    {
        busy = true;
    }

    if !busy {
        st.set_hover_prev = Ctl::None;
        let _ = KillTimer(hwnd, TIMER_SETUP_FRAME);
    }
    setup_present(st);
}

/// 设置窗口过程
unsafe extern "system" fn setup_proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    // 分层窗口自己渲染上屏，WM_PAINT 无事可做
    if msg == WM_PAINT {
        let mut ps = PAINTSTRUCT::default();
        let _ = BeginPaint(hwnd, &mut ps);
        let _ = EndPaint(hwnd, &ps);
        return LRESULT(0);
    }
    // 鼠标坐标是**窗口**坐标（卡片外还有一圈 S_M 的投影留白），
    // 布局用的是卡片内的逻辑坐标，所以先减掉 S_M
    let lx = || ((l.0 & 0xFFFF) as i16 as i32) - S_M;
    let ly = || (((l.0 >> 16) & 0xFFFF) as i16 as i32) - S_M;
    match msg {
        WM_ERASEBKGND => LRESULT(1),
        WM_NCHITTEST => {
            // ⚠️⚠️ `WM_NCHITTEST` 的 lParam 是**屏幕坐标**（不是客户区坐标！）。
            // 原来直接拿 S_M/S_W 去比 → 只有窗口恰好贴在屏幕左上角时才成立，
            // 所以设置页「一直拖不动」（2026-10-11 用户报）。先换算成窗口坐标再判。
            let mut wr = RECT::default();
            let _ = GetWindowRect(hwnd, &mut wr);
            let x = ((l.0 & 0xFFFF) as i16 as i32) - wr.left;
            let y = (((l.0 >> 16) & 0xFFFF) as i16 as i32) - wr.top;
            // 标题条拖动（排除关闭按钮那一段，**以及「查看引导」入口** ——
            // ⚠️ 入口也落在标题条里，不排除的话点它会被判成「拖动标题栏」，
            // 窗口跟着鼠标跑，于是「点了没反应」（用户 2026-10-13 报）。
            if x >= S_M
                && x < S_M + S_W - 56
                && y >= S_M
                && y < S_M + S_TITLE_H
                && !in_rect(s_guide_entry_rect(), x - S_M, y - S_M)
            {
                return LRESULT(HTCAPTION as isize);
            }
            DefWindowProcW(hwnd, msg, w, l)
        }
        WM_MOUSEMOVE => {
            let st = state_of(hwnd);
            // 注册一次 LEAVE 通知，否则指针移出窗口后 hover 会「粘」在最后一个控件上
            if !st.set_tracking {
                use windows::Win32::UI::Input::KeyboardAndMouse::{
                    TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT,
                };
                st.set_tracking = true;
                let mut tme = TRACKMOUSEEVENT {
                    cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                    dwFlags: TME_LEAVE,
                    hwndTrack: hwnd,
                    dwHoverTime: 0,
                };
                let _ = TrackMouseEvent(&mut tme);
            }
            let c = s_hit(st, lx(), ly());
            // 「?」悬停 → 记下要显示哪条 tooltip（画在 settings_draw 最后）
            let tip = if let Ctl::Tip(id) = c { id } else { 0 };
            if tip != st.tip {
                st.tip = tip;
                setup_kick(st, hwnd);
            }
            if c != st.set_hover {
                // 旧的淡出、新的淡入：靠 set_hov_a 这一个进度量驱动
                st.set_hover_prev = st.set_hover;
                st.set_hover = c;
                st.set_hov_a = 0.0;
                setup_kick(st, hwnd);
            }
            LRESULT(0)
        }
        WM_MOUSELEAVE_MSG => {
            let st = state_of(hwnd);
            st.set_tracking = false;
            // 指针离开窗口 → tooltip 必须收掉（否则会「粘」在屏幕上）
            if st.tip != 0 {
                st.tip = 0;
                setup_kick(st, hwnd);
            }
            if st.set_hover != Ctl::None {
                st.set_hover_prev = st.set_hover;
                st.set_hover = Ctl::None;
                st.set_hov_a = 0.0;
                setup_kick(st, hwnd);
            }
            LRESULT(0)
        }
        // 从资源管理器把文件拖进黑白名单卡片。用 WM_DROPFILES（比再挂一个 OLE
        // 放置目标简单得多，这里不需要拖拽预览）。
        WM_DROPFILES => {
            let hd = HDROP(w.0 as *mut c_void);
            let mut pt = POINT::default();
            let _ = DragQueryPoint(hd, &mut pt);
            let cnt = DragQueryFileW(hd, 0xFFFF_FFFF, None);
            let mut names: Vec<String> = Vec::new();
            for i in 0..cnt {
                let mut buf = [0u16; 1024];
                let len = DragQueryFileW(hd, i, Some(&mut buf));
                if len == 0 {
                    continue;
                }
                let p = String::from_utf16_lossy(&buf[..len as usize]);
                if let Some(nm) = exe_name_of(&p) {
                    names.push(nm);
                }
            }
            let _ = DragFinish(hd);
            println!(
                "[drop] 文件数={} 可用={} 落点=({},{})",
                cnt, names.len(), pt.x, pt.y
            );
            if !names.is_empty() {
                let st = state_of(hwnd);
                // 只进**当前生效**的那张卡片（另一张「未启用」，落上去也不该生效）
                let c = st.exc_mode as usize;
                let mut added = 0;
                for nm in names {
                    let bucket = if c == 0 { &mut st.exc } else { &mut st.exc_w };
                    if !bucket.iter().any(|e| e == &nm) {
                        bucket.push(nm);
                        added += 1;
                    }
                }
                if added > 0 {
                    save_settings(st);
                }
                setup_kick(st, hwnd);
            }
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            let st = state_of(hwnd);
            // 引导进行中：**只**判卡片上的三个按钮，点别处一律吃掉（只读）
            if st.guide_step >= 0 {
                match guide_hit(st, lx(), ly()) {
                    0 => guide_advance(st, 0),  // 跳过 → 结束
                    1 => guide_advance(st, -1), // 上一步
                    2 => guide_advance(st, 1),  // 下一步 / 完成
                    _ => {}
                }
                setup_kick(st, hwnd);
                return LRESULT(0);
            }
            s_click(st, hwnd, lx(), ly());
            LRESULT(0)
        }
        WM_TIMER => {
            let st = state_of(hwnd);
            if w.0 as usize == TIMER_SETUP_FRAME {
                setup_tick(st, hwnd);
            }
            LRESULT(0)
        }
        WM_MOUSEWHEEL => {
            let st = state_of(hwnd);
            if st.set_page == 2 {
                let d = ((w.0 >> 16) & 0xFFFF) as i16 as i32 / 120;
                st.set_scroll = (st.set_scroll - d * 42).clamp(0, s_scroll_max(st));
                setup_present(st);
            }
            LRESULT(0)
        }
        WM_APP_TITLE => {
            // 后台抓 `<title>` 回来了 → 改名 + 把「正在取标题」的点点掉
            let st = state_of(hwnd);
            unsafe { title_apply_pending(st) };
            setup_present(st);
            setup_kick(st, hwnd);
            LRESULT(0)
        }
        WM_CHAR => {
            // 内联网址输入框的唯一入口。IME 合成的中文由 `WM_IME_CHAR` 交给
            // `DefWindowProcW`（默认分支）转成这条 `WM_CHAR`，所以这里只处理一种。
            let st = state_of(hwnd);
            if st.set_edit_ai == usize::MAX {
                return LRESULT(0);
            }
            match w.0 as u32 {
                0x1B => unsafe { set_edit_finish(st, false) }, // Esc：收回输入框（**不是**关窗）
                0x0D => unsafe { set_edit_finish(st, true) },  // 回车：添加
                0x08 => {
                    st.set_edit_buf.pop();
                }
                0x16 => {
                    // Ctrl+V：只贴纯文本，取第一个非空字段（复制「标题 + 链接」时不至于粘进标题）
                    if let Some(t) = unsafe { clip_text() } {
                        for ch in t.split_whitespace().next().unwrap_or("").chars() {
                            if !ch.is_control() && st.set_edit_buf.chars().count() < 200 {
                                st.set_edit_buf.push(ch);
                            }
                        }
                    }
                }
                _ => {
                    if let Some(ch) = char::from_u32(w.0 as u32) {
                        if !ch.is_control() && st.set_edit_buf.chars().count() < 200 {
                            st.set_edit_buf.push(ch);
                        }
                    }
                }
            }
            setup_present(st);
            setup_kick(st, hwnd);
            LRESULT(0)
        }
        WM_KEYDOWN => {
            // 引导进行中：←/→/Enter 翻步，Esc **退出引导**（而不是关设置窗）
            {
                let st = state_of(hwnd);
                if st.guide_step >= 0 {
                    match w.0 as u32 {
                        0x1B => guide_advance(st, 0),
                        0x25 => guide_advance(st, -1),       // ←
                        0x27 | 0x0D => guide_advance(st, 1), // → / Enter
                        _ => {}
                    }
                    setup_kick(st, hwnd);
                    return LRESULT(0);
                }
                // 删除确认卡开着：Esc = 收卡，**不是**关整个设置窗
                if st.set_confirm != Ctl::None {
                    st.set_confirm = Ctl::None;
                    setup_present(st);
                    setup_kick(st, hwnd);
                    return LRESULT(0);
                }
                // 内联网址输入框开着：按键一律走 `WM_CHAR` 那套语义。
                // 少了这道闸，输入框里按 Esc 会**把整个设置窗关掉**。
                if st.set_edit_ai != usize::MAX {
                    return LRESULT(0);
                }
            }
            if w.0 as u32 == 0x1B {
                cancel_trig_capture();
                let _ = ShowWindow(hwnd, SW_HIDE);
                setup_free(state_of(hwnd));
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            cancel_trig_capture();
            let _ = ShowWindow(hwnd, SW_HIDE);
            setup_free(state_of(hwnd));
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, w, l),
    }
}

// ── 持久化 ──

unsafe fn save_settings(st: &State) {
    // 无 git —— 破坏性写之前先留一份可回滚的副本
    let bak = format!("{}.bak", st.ini_path);
    let _ = std::fs::copy(&st.ini_path, &bak);
    let b = |v: bool| if v { 1 } else { 0 };
    let mut out = String::new();
    out.push_str("[设置]\r\n");
    out.push_str(&format!("dark={}\r\n", st.theme));
    out.push_str(&format!("persistent={}\r\n", b(st.persistent)));
    // ⚠️ 写 `listening`（用户意图），不能写 `running`（运行时实际钩子状态）：
    // 启动早期 `running` 还是 false（钩子尚未安装），此时任何 `save_settings`
    // 都会把 listening=0 落盘 → 下次启动就不监听。2026-10-20 自动更新检查踩过。
    out.push_str(&format!("listening={}\r\n", b(st.listening)));
    out.push_str(&format!("key_middle={}\r\n", b(st.key_middle)));
    out.push_str(&format!("key_side={}\r\n", b(st.key_side)));
    out.push_str(&format!("hover_name={}\r\n", b(st.hover_name)));
    out.push_str(&format!("glass={}\r\n", b(st.glass)));
    out.push_str(&format!("glass_live={}\r\n", b(st.glass_live)));
    out.push_str(&format!("key_rhold={}\r\n", b(st.key_rhold)));
    out.push_str(&format!("autostart={}\r\n", b(st.autostart)));
    out.push_str(&format!("guide_seen={}\r\n", b(st.guide_seen)));
    out.push_str(&format!("update_auto={}\r\n", b(st.update_auto)));
    out.push_str(&format!("update_last_check={}\r\n", st.update_last_check as i64));
    out.push_str(&format!("update_avail_ver={}\r\n", st.update_avail_ver));
    out.push_str(&format!("update_bub_ver={}\r\n", st.update_bub_ver));
    out.push_str(&format!("onb_drag={}\r\n", b(st.onb_drag)));
    out.push_str(&format!("onb_move={}\r\n", b(st.onb_move)));
    out.push_str(&format!("onb_edit={}\r\n", b(st.onb_edit)));
    out.push_str(&format!("onb_scroll={}\r\n", b(st.onb_scroll)));
    out.push_str(&format!("trig_custom_kind={}\r\n", st.trig_custom_kind));
    out.push_str(&format!("trig_custom_btn={}\r\n", st.trig_custom_btn));
    out.push_str(&format!("trig_custom_vk={}\r\n", st.trig_custom_vk));
    out.push_str(&format!("trig_custom_mods={}\r\n", st.trig_custom_mods));
    out.push_str("\r\n[apps]\r\n");
    for a in &st.apps {
        out.push_str(&format!("{}={}\r\n", a.name, a.target));
    }
    for a in &st.apps {
        if a.items.is_empty() {
            continue;
        }
        out.push_str(&format!("\r\n[app.{}]\r\n", a.name));
        for it in &a.items {
            match it.kind {
                1 => {
                    // 窗口条目：保留重开命令行。`|@win|` 后缀与配色 `|N` 不冲突
                    // （解析时先摘 `|@win|` 再解 `|N`）。
                    match it.color {
                        Some(c) => out.push_str(&format!(
                            "{}={}|{}|@win|{}\r\n",
                            it.name, it.target, c, it.relaunch
                        )),
                        None => out.push_str(&format!(
                            "{}={}|@win|{}\r\n",
                            it.name, it.target, it.relaunch
                        )),
                    }
                }
                _ => match it.color {
                    Some(c) => out.push_str(&format!("{}={}|{}\r\n", it.name, it.target, c)),
                    None => out.push_str(&format!("{}={}\r\n", it.name, it.target)),
                },
            }
        }
    }
    // 例外名单：mode=black|white 决定二选一生效哪张；fullscreen=0|1；
    // 名单条目 `b=`（黑）/ `w=`（白）两张卡片可并存于配置，但只有 mode 指定的那张真正生效。
    out.push_str("\r\n[例外]\r\n");
    out.push_str(&format!("mode={}\r\n", if st.exc_mode == 1 { "white" } else { "black" }));
    out.push_str(&format!("fullscreen={}\r\n", b(st.exc_fs)));
    for n in &st.exc {
        out.push_str(&format!("b={}\r\n", n));
    }
    for n in &st.exc_w {
        out.push_str(&format!("w={}\r\n", n));
    }
    let _ = std::fs::write(&st.ini_path, out);
}

fn apply_key_opts(st: &State) {
    OPT_MIDDLE.store(st.key_middle, Ordering::SeqCst);
    OPT_SIDE.store(st.key_side, Ordering::SeqCst);
    OPT_RHOLD.store(st.key_rhold, Ordering::SeqCst);
    // 自定义触发只接受键盘组合：所有鼠标键都不再作为自定义触发键
    // （中/侧键已有上方独立开关；左键破坏正常操作；右键=长按右键专属）。
    // 录制端已拒绝，这里再兜底——legacy ini 里残留的 kind=1 也不武装鼠标钩子。
    OPT_CUSTOM_MOUSE.store(0, Ordering::SeqCst);
    OPT_CUSTOM_VK.store(if st.trig_custom_kind == 2 { st.trig_custom_vk } else { 0 }, Ordering::SeqCst);
    OPT_CUSTOM_MODS.store(if st.trig_custom_kind == 2 { st.trig_custom_mods } else { 0 }, Ordering::SeqCst);
    // 键盘组合触发 = opt-in 常驻键盘钩子（用户显式设置才挂，避开「常挂导致打字卡顿」）
    if st.trig_custom_kind == 2 {
        need_key_hook(true);
    }
}

/// 跟随系统：读 Windows 的「应用模式」是否用浅色（0 = 深色）
fn system_is_dark() -> bool {
    use windows::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};
    let sub = wide("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize");
    let val = wide("AppsUseLightTheme");
    let mut v: u32 = 1;
    let mut sz: u32 = 4;
    let r = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            PCWSTR(sub.as_ptr()),
            PCWSTR(val.as_ptr()),
            RRF_RT_REG_DWORD,
            None,
            Some(&mut v as *mut u32 as *mut c_void),
            Some(&mut sz),
        )
    };
    if r.0 == 0 {
        v == 0
    } else {
        false
    }
}

unsafe fn apply_theme(st: &mut State) {
    st.glass_liquid = match st.theme {
        0 => false,
        1 => true,
        _ => system_is_dark(),
    };
}

fn autostart_key() -> (Vec<u16>, Vec<u16>) {
    (
        wide("Software\\Microsoft\\Windows\\CurrentVersion\\Run"),
        wide("PiPie"),
    )
}

/// 注册表里**当前有没有**自启项（只查存在性，值内容不关心）
fn autostart_reg_exists() -> bool {
    use windows::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_SZ};
    let (sub, name) = autostart_key();
    let mut buf = [0u16; 1024];
    let mut sz: u32 = (buf.len() * 2) as u32;
    let r = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            PCWSTR(sub.as_ptr()),
            PCWSTR(name.as_ptr()),
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr() as *mut c_void),
            Some(&mut sz),
        )
    };
    r.0 == 0
}

/// 启动时把注册表对齐到 `want`（ini 为准）。
///
/// ⚠️⚠️ **只在「该有却没有 / 不该有却有」时才动注册表**，绝不无脑覆写。
/// 原因：自启项的值是 `"<当前进程 exe 路径>"`，而沙箱 / 测试跑的是
/// `_tmptest\PiPie.exe` —— 一启动就把**用户真实的自启项改指到沙箱**去
/// （实测踩到：值被改成 `_tmptest`）。用「存在性差集」同步就完全避开了：
/// 正常安装时值已存在且正确 → 一次都不写。
fn autostart_sync(want: bool) {
    autostart_drop_legacy();
    let has = autostart_reg_exists();
    if want && !has {
        autostart_set(true);
    } else if !want && has {
        autostart_set(false);
    }
}

/// 删掉**改名前**留下的自启项（`Run` 下那个 `RadialMenu`）。
/// 不删的话用户会同时有两个自启项，而且旧的那个指向已经不存在的旧 exe 名。
fn autostart_drop_legacy() {
    use windows::Win32::System::Registry::{
        RegCloseKey, RegDeleteValueW, RegOpenKeyExW, HKEY, HKEY_CURRENT_USER, KEY_SET_VALUE,
    };
    let (sub, _) = autostart_key();
    // `RadialMenu` = 更早的名字；`pigo` = 改名为 PiPie 之前那一版。
    // 不删的话用户会同时有两个自启项，而且旧项指向已经不存在的旧 exe 名。
    let legacy_names = [wide("RadialMenu"), wide("pigo")];
    unsafe {
        let mut hk = HKEY::default();
        if RegOpenKeyExW(HKEY_CURRENT_USER, PCWSTR(sub.as_ptr()), 0, KEY_SET_VALUE, &mut hk).is_ok() {
            for legacy in &legacy_names {
                let _ = RegDeleteValueW(hk, PCWSTR(legacy.as_ptr()));
            }
            let _ = RegCloseKey(hk);
        }
    }
}

// ⚠️ 曾经有 `autostart_get()`（直读注册表）。现在 **ini 是唯一真相**
// （`st.autostart`，缺省 = 开），启动时单向同步到注册表 —— 直读注册表会让
// 「默认勾选」在设置页里对不上（注册表可能还没被同步过）。故已删除。

fn autostart_set(on: bool) {
        use windows::Win32::System::Registry::{
            RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegSetValueExW, HKEY,
            HKEY_CURRENT_USER, KEY_SET_VALUE, REG_OPTION_NON_VOLATILE, REG_SZ,
        };
    let (sub, name) = autostart_key();
    unsafe {
        let mut hk = HKEY::default();
        let r = RegCreateKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(sub.as_ptr()),
            0,
            PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE,
            None,
            &mut hk,
            None,
        );
        if r.0 != 0 {
            return;
        }
        if on {
            let d: Vec<u16> = wide(&format!("\"{}\"", exe_path()));
            let bytes: &[u8] = std::slice::from_raw_parts(d.as_ptr() as *const u8, d.len() * 2);
            let _ = RegSetValueExW(hk, PCWSTR(name.as_ptr()), 0, REG_SZ, Some(bytes));
        } else {
            let _ = RegDeleteValueW(hk, PCWSTR(name.as_ptr()));
        }
        let _ = RegCloseKey(hk);
    }
}

fn exe_path() -> String {
    let mut buf = [0u16; 1024];
    let n = unsafe { GetModuleFileNameW(None, &mut buf) } as usize;
    String::from_utf16_lossy(&buf[..n.min(buf.len())])
}

/// 选文件（原生对话框：中文路径输入交给系统，自己不造轮子）
unsafe fn pick_file() -> Option<String> {
    use windows::Win32::UI::Controls::Dialogs::{
        GetOpenFileNameW, OPENFILENAMEW, OFN_DONTADDTORECENT, OFN_FILEMUSTEXIST, OFN_NOCHANGEDIR,
        OFN_PATHMUSTEXIST,
    };
    let filter: Vec<u16> = "程序 / 快捷方式\0*.exe;*.lnk;*.bat;*.cmd\0所有文件\0*.*\0\0"
        .encode_utf16()
        .collect();
    let defext: Vec<u16> = "lnk\0".encode_utf16().collect();
    let mut buf = [0u16; 1024];
    let mut ofn = OPENFILENAMEW::default();
    ofn.lStructSize = std::mem::size_of::<OPENFILENAMEW>() as u32;
    ofn.lpstrFilter = PCWSTR(filter.as_ptr());
    ofn.lpstrFile = windows::core::PWSTR(buf.as_mut_ptr());
    ofn.nMaxFile = buf.len() as u32;
    ofn.lpstrDefExt = PCWSTR(defext.as_ptr());
    ofn.Flags = OFN_FILEMUSTEXIST | OFN_PATHMUSTEXIST | OFN_NOCHANGEDIR | OFN_DONTADDTORECENT;
    if !GetOpenFileNameW(&mut ofn).as_bool() {
        return None;
    }
    let end = buf.iter().position(|c| *c == 0).unwrap_or(buf.len());
    if end == 0 {
        return None;
    }
    Some(String::from_utf16_lossy(&buf[..end]))
}

/// 选**文件夹**（原生「选择文件夹」对话框 = 同一个 `IFileOpenDialog`，只是开了
/// `FOS_PICKFOLDERS`）。
///
/// ⚠️ 为什么不能和「选文件」合成一颗入口：`FOS_PICKFOLDERS` 一开，列表里的**文件会变灰、
/// 选不动**（实测；微软文档：该选项让对话框「在文件夹而非文件之间做选择」）。
/// 唯一能两者通选的 `SHBrowseForFolder` + `BIF_BROWSEINCLUDEFILES` 是老式树控件，
/// 和设置页的观感对不上 —— 所以做成「＋ 文件 / ＋ 文件夹」两颗。
unsafe fn pick_folder() -> Option<String> {
    use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED};
    use windows::Win32::System::Com::CoTaskMemFree;
    use windows::Win32::UI::Shell::{
        FileOpenDialog, IFileOpenDialog, FOS_FORCEFILESYSTEM, FOS_NOCHANGEDIR, FOS_PICKFOLDERS,
        SIGDN_FILESYSPATH,
    };
    // 本线程（UI 线程）启动时已 CoInitializeEx 过，这里再调一次只是幂等的保险
    let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    let dlg: IFileOpenDialog = CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).ok()?;
    // NOCHANGEDIR：不许对话框改我们的当前目录（`pick_file` 同样带这条）
    let _ = dlg.SetOptions(FOS_PICKFOLDERS | FOS_FORCEFILESYSTEM | FOS_NOCHANGEDIR);
    if dlg.Show(None).is_err() {
        return None; // 用户取消
    }
    let item = dlg.GetResult().ok()?;
    let ps = item.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
    let s = ps.to_string().ok()?;
    CoTaskMemFree(Some(ps.0 as *const _));
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

// ── 超采样渲染管线 ──

/// 设置页那5 号 SS× 字体：按需创建（关窗由 `setup_free` 删掉）。
/// ⚠️ 字号全部乘 `SS` —— 改`SS` 时这里自动跟着走。
unsafe fn ensure_setup_fonts(st: &mut State) {
    if !st.sf_title.0.is_null() {
        return;
    }
    let face: Vec<u16> = wide("Microsoft YaHei UI");
    st.sf_title = CreateFontW(-(16 * SS), 0, 0, 0, 700, 0, 0, 0, 134, 0, 0, 2, 0, PCWSTR(face.as_ptr()));
    st.sf_h1 = CreateFontW(-(20 * SS), 0, 0, 0, 700, 0, 0, 0, 134, 0, 0, 2, 0, PCWSTR(face.as_ptr()));
    st.sf_nav = CreateFontW(-(14 * SS), 0, 0, 0, 600, 0, 0, 0, 134, 0, 0, 2, 0, PCWSTR(face.as_ptr()));
    st.sf_body = CreateFontW(-(13 * SS), 0, 0, 0, 600, 0, 0, 0, 134, 0, 0, 2, 0, PCWSTR(face.as_ptr()));
    st.sf_small = CreateFontW(-(12 * SS), 0, 0, 0, 400, 0, 0, 0, 134, 0, 0, 2, 0, PCWSTR(face.as_ptr()));
    st.sf_bold = CreateFontW(-(14 * SS), 0, 0, 0, 700, 0, 0, 0, 134, 0, 0, 2, 0, PCWSTR(face.as_ptr()));
}

unsafe fn ensure_setup_surfaces(st: &mut State) {
    ensure_setup_fonts(st);
    if st.s_hi_dc.0.is_null() {
        let (b, d, m) = make_dib(S_WW * SS, S_HH * SS);
        st.s_hi_bits = b;
        st.s_hi_dc = d;
        st.s_hi_bmp = m;
    }
    if st.s_lo_dc.0.is_null() {
        let (b, d, m) = make_dib(S_WW, S_HH);
        st.s_lo_bits = b;
        st.s_lo_dc = d;
        st.s_lo_bmp = m;
    }
}

/// 把当前页渲染进 lo 缓冲（**未预乘**）：hi 上 SS× 画 → SS×SS 盒式降采样 → 投影 + alpha
unsafe fn setup_render(st: &mut State) {
    let t0 = Instant::now();
    ensure_setup_surfaces(st);
    if st.s_hi_dc.0.is_null() || st.s_lo_dc.0.is_null() || st.s_hi_bits.is_null() {
        return;
    }
    let pal = pal_of(st.glass_liquid);
    let hi_w = (S_WW * SS) as usize;
    let hi_h = (S_HH * SS) as usize;
    let lo_w = S_WW as usize;
    let lo_h = S_HH as usize;

    // 1) hi 画布整体铺卡片色。**不能留黑** —— 降采样时边缘会把黑混进来，
    //    卡片边上就会出现一圈脏灰。
    g_fill(
        st.s_hi_dc,
        RECT { left: -S_M, top: -S_M, right: S_W + S_M, bottom: S_H + S_M },
        pal.paper,
    );
    // 2) 画整页
    settings_draw(st, st.s_hi_dc);
    // 2.5) 聚光灯引导层（压暗挖孔外 + 白环 / 卡片 / 箭头）—— 必须在降采样之前
    guide_draw(st);

    // 3) SS×SS 盒式降采样（整数倍，等价于 SS² 级覆盖率）
    let hi = std::slice::from_raw_parts(st.s_hi_bits, hi_w * hi_h * 4);
    let lo = std::slice::from_raw_parts_mut(st.s_lo_bits, lo_w * lo_h * 4);
    let n = (SS * SS) as u32;
    for y in 0..lo_h {
        for x in 0..lo_w {
            let (mut a0, mut a1, mut a2) = (0u32, 0u32, 0u32);
            for dy in 0..SS as usize {
                let row = ((y * SS as usize + dy) * hi_w + x * SS as usize) * 4;
                for dx in 0..SS as usize {
                    let o = row + dx * 4;
                    a0 += hi[o] as u32;
                    a1 += hi[o + 1] as u32;
                    a2 += hi[o + 2] as u32;
                }
            }
            let o = (y * lo_w + x) * 4;
            lo[o] = (a0 / n) as u8;
            lo[o + 1] = (a1 / n) as u8;
            lo[o + 2] = (a2 / n) as u8;
        }
    }

    // 4) alpha：GDI 不写 alpha 通道，所以形状的透明区在这里**解析式**补上。
    //    卡片 = 圆角矩形（外扩 1px，让 2px 描边的外半边不被切掉）；
    //    卡片之外按「向下偏移 + 收紧模糊」铺一层投影。
    let hw = S_W as f32 / 2.0;
    let hh = S_H as f32 / 2.0;
    let ccx = (S_M + S_W / 2) as f32;
    let ccy = (S_M + S_H / 2) as f32;
    // 投影：向下偏移 + **同时向内收**，否则四周会糊出一圈对称光晕（就是「大黑边」）。
    // 收进去之后：下方只有 4px 实带 + 渐变，左右各剩一点点，上方几乎没有。
    let (sh_dy, sh_blur, sh_a) = (8.0f32, 14.0f32, 0.26f32);
    let shc = [SHADOW.2 as f32, SHADOW.1 as f32, SHADOW.0 as f32]; // 缓冲是 BGRA
    for y in 0..lo_h {
        for x in 0..lo_w {
            let fx = x as f32 + 0.5;
            let fy = y as f32 + 0.5;
            let o = (y * lo_w + x) * 4;
            let ac = cov1(-sdf_rounded_rect(fx - ccx, fy - ccy, hw + 1.0, hh + 1.0, CARD_RAD as f32));
            if ac >= 0.999 {
                lo[o + 3] = 255;
                continue;
            }
            let ds = -sdf_rounded_rect(
                fx - ccx,
                fy - ccy - sh_dy,
                (hw - 6.0).max(2.0),
                (hh - 4.0).max(2.0),
                CARD_RAD as f32,
            );
            let ash = if ds > 0.0 {
                let t = 1.0 - (ds / sh_blur).clamp(0.0, 1.0);
                t * t * sh_a
            } else {
                sh_a
            };
            let a = (ac + (1.0 - ac) * ash).clamp(0.0, 1.0);
            if a < 0.002 {
                lo[o + 3] = 0;
                continue;
            }
            let inv = 1.0 / a;
            for k in 0..3 {
                let c = lo[o + k] as f32;
                lo[o + k] = ((c * ac + shc[k] * (1.0 - ac) * ash) * inv).clamp(0.0, 255.0) as u8;
            }
            lo[o + 3] = (a * 255.0).round().clamp(0.0, 255.0) as u8;
        }
    }

    if std::env::var("RL_SETDBG").is_ok() {
        eprintln!("[setdbg] setup_render {:.1}ms", t0.elapsed().as_secs_f64() * 1000.0);
    }
}

/// 渲染 + 上屏（分层窗口：ULW 要预乘 alpha）
unsafe fn setup_present(st: &mut State) {
    let h = SET_HWND.load(Ordering::SeqCst);
    if h == 0 {
        return;
    }
    setup_render(st);
    if st.s_lo_bits.is_null() {
        return;
    }
    {
        let buf = std::slice::from_raw_parts_mut(st.s_lo_bits, (S_WW * S_HH * 4) as usize);
        premultiply(buf);
    }
    // ⚠️ `pptDst` 必须传 `None`：让分层窗口**保留当前位置**。
    // 之前传 `Some(&POINT{set_x,set_y})` 写死居中值，每帧把窗口拽回屏幕中心 ——
    // 表现正是用户 2026-10-11 报的「拖动设置窗口后立刻弹回中心」。
    // 初始居中由 `open_settings` 的 `SetWindowPos` 负责，拖动由 `WM_NCHITTEST→HTCAPTION`
    // 让系统移动窗口，这里不再覆盖位置。
    let src = POINT::default();
    let size = SIZE { cx: S_WW, cy: S_HH };
    let blend = BLENDFUNCTION {
        BlendOp: 0u8,
        BlendFlags: 0,
        SourceConstantAlpha: 255,
        AlphaFormat: 1u8,
    };
    let _ = UpdateLayeredWindow(
        HWND(h as *mut c_void),
        HDC(ptr::null_mut()),
        None,
        Some(&size),
        st.s_lo_dc,
        Some(&src),
        COLORREF(0),
        Some(&blend),
        ULW_ALPHA,
    );
}

/// 设置页的离屏核对：把每个页面渲染成 BMP（配 bmp2png.py 看图，不用真机点）
/// 把当前 `s_lo_bits` 合成到中灰底上存成 `{dir}\\{name}.bmp`。
/// BMP 不带 alpha，直接存的话圆角外和投影区显示的是原始 RGB，
/// 看不出投影浓淡和透明边界 —— 所以每次出图都要过这一步。
unsafe fn s_dump_save(st: &State, dir: &str, name: &str) {
    let bg = (205u8, 205u8, 208u8);
    let mut px = std::slice::from_raw_parts(st.s_lo_bits, (S_WW * S_HH * 4) as usize).to_vec();
    for p in px.chunks_mut(4) {
        let a = p[3] as f32 / 255.0;
        let ia = 1.0 - a;
        p[0] = (p[0] as f32 * a + bg.2 as f32 * ia).clamp(0.0, 255.0) as u8;
        p[1] = (p[1] as f32 * a + bg.1 as f32 * ia).clamp(0.0, 255.0) as u8;
        p[2] = (p[2] as f32 * a + bg.0 as f32 * ia).clamp(0.0, 255.0) as u8;
        p[3] = 255;
    }
    let ok = save_bmp(&format!("{dir}\\{name}.bmp"), &px, S_WW, S_HH);
    println!("[setup] {name} {}", if ok { "OK" } else { "FAIL" });
}

unsafe fn settings_dump(dir: &str, st: &mut State) {
    // 最后一个「页」不是页：交互页 + 展开的「?」tooltip
    // （tooltip 只在悬停时出现，离屏没法复现 hover，直接置位出图核对）
    for (page, tip, guide) in (0..S_PAGES.len())
        .map(|p| (p, 0u8, -1i32))
        .chain([(1usize, 3u8, -1), (1, 4, -1), (1, 5, -1)])
        // 交互页的 4 个引导步骤也出图（离屏没法点按钮，直接置位核对）
        .chain([(1usize, 0u8, 0i32), (1, 0, 1), (1, 0, 2), (1, 0, 3)])
    {
        st.set_page = page;
        st.tip = tip;
        st.guide_step = guide;
        st.set_hover = Ctl::None;
        st.set_scroll = 0;
        st.set_expanded = if page == 2 { 0 } else { usize::MAX };
        setup_render(st);
        let (dr, dtot) = s_rows(st);
        println!(
            "[setup] page={} apps={} rows={} total={} expanded={}",
            page,
            st.apps.len(),
            dr.len(),
            dtot,
            st.set_expanded
        );
        // 合成到中灰底上再看 —— BMP 不带 alpha，直接存的话圆角外和投影区
        // 显示的是原始 RGB，完全看不出投影浓淡和透明边界
        let bg = (205u8, 205u8, 208u8);
        let mut px = std::slice::from_raw_parts(st.s_lo_bits, (S_WW * S_HH * 4) as usize).to_vec();
        for p in px.chunks_mut(4) {
            let a = p[3] as f32 / 255.0;
            let ia = 1.0 - a;
            p[0] = (p[0] as f32 * a + bg.2 as f32 * ia).clamp(0.0, 255.0) as u8; // B
            p[1] = (p[1] as f32 * a + bg.1 as f32 * ia).clamp(0.0, 255.0) as u8; // G
            p[2] = (p[2] as f32 * a + bg.0 as f32 * ia).clamp(0.0, 255.0) as u8; // R
            p[3] = 255;
        }
        let name = if guide >= 0 {
            format!("setup_guide{}", guide)
        } else if tip == 0 {
            format!("setup_{}", page)
        } else {
            format!("setup_tip{}", tip)
        };
        let p = format!("{}\\{}.bmp", dir, name);
        let ok = save_bmp(&p, &px, S_WW, S_HH);
        println!("[setup] {} page={} tip={} guide={} {}", name, page, tip, guide, if ok { "OK" } else { "FAIL" });
    }

    // 首启场景：**故意先停在外观页**，再走 `guide_start`（首启与「查看引导」共用这条路径），
    // 出图核对「引导真的切到了交互页」而不是停在外观页（用户 2026-10-13 报的那个 bug）。
    {
        st.set_page = 0;
        st.set_page_a = 1.0;
        st.guide_step = -1;
        st.set_hover = Ctl::None;
        guide_start(st);
        st.set_page_a = 1.0; // 出图不等切页淡入
        setup_render(st);
        let bg = (205u8, 205u8, 208u8);
        let mut px = std::slice::from_raw_parts(st.s_lo_bits, (S_WW * S_HH * 4) as usize).to_vec();
        for p in px.chunks_mut(4) {
            let a = p[3] as f32 / 255.0;
            let ia = 1.0 - a;
            p[0] = (p[0] as f32 * a + bg.2 as f32 * ia).clamp(0.0, 255.0) as u8;
            p[1] = (p[1] as f32 * a + bg.1 as f32 * ia).clamp(0.0, 255.0) as u8;
            p[2] = (p[2] as f32 * a + bg.0 as f32 * ia).clamp(0.0, 255.0) as u8;
            p[3] = 255;
        }
        let ok = save_bmp(&format!("{}\\setup_first.bmp", dir), &px, S_WW, S_HH);
        println!(
            "[setup] setup_first page={} guide={} {}（期望 page=1 交互页）",
            st.set_page,
            st.guide_step,
            if ok { "OK" } else { "FAIL" }
        );
    }

    // 录制态：详情行右端应出现「取消录制」按钮（否则一进录制就只能录完）
    {
        st.set_page = 1;
        st.set_page_a = 1.0;
        st.guide_step = -1;
        st.trig_capturing = true;
        st.trig_capture_bad = 0;
        st.set_hover = Ctl::None;
        setup_render(st);
        let bg = (205u8, 205u8, 208u8);
        let mut px = std::slice::from_raw_parts(st.s_lo_bits, (S_WW * S_HH * 4) as usize).to_vec();
        for p in px.chunks_mut(4) {
            let a = p[3] as f32 / 255.0;
            let ia = 1.0 - a;
            p[0] = (p[0] as f32 * a + bg.2 as f32 * ia).clamp(0.0, 255.0) as u8;
            p[1] = (p[1] as f32 * a + bg.1 as f32 * ia).clamp(0.0, 255.0) as u8;
            p[2] = (p[2] as f32 * a + bg.0 as f32 * ia).clamp(0.0, 255.0) as u8;
            p[3] = 255;
        }
        let ok = save_bmp(&format!("{}\\setup_trigcap.bmp", dir), &px, S_WW, S_HH);
        println!("[setup] setup_trigcap capturing=true {}", if ok { "OK" } else { "FAIL" });
        st.trig_capturing = false;
    }

    // 「＋ 网址」内联输入框：浏览器应用展开后那颗小胶囊点开、并且已经打了字。
    // 光标是 0.5s 方波闪的，出两张才有一张带光标（离屏没法按真键盘）；
    // 第三张是空缓冲 → 核对占位文案。
    for tag in ["a", "b", "c"] {
        st.set_page = 2;
        st.set_page_a = 1.0;
        st.guide_step = -1;
        st.tip = 0;
        st.set_scroll = 0;
        st.set_expanded = 0;
        st.set_hover = Ctl::None;
        st.set_edit_ai = 0;
        st.set_edit_buf = if tag == "c" { String::new() } else { "github.com/qoder".to_string() };
        st.title_busy = vec!["https://example.com/".to_string()];
        setup_render(st);
        std::thread::sleep(std::time::Duration::from_millis(280));
        let bg = (205u8, 205u8, 208u8);
        let mut px = std::slice::from_raw_parts(st.s_lo_bits, (S_WW * S_HH * 4) as usize).to_vec();
        for p in px.chunks_mut(4) {
            let a = p[3] as f32 / 255.0;
            let ia = 1.0 - a;
            p[0] = (p[0] as f32 * a + bg.2 as f32 * ia).clamp(0.0, 255.0) as u8;
            p[1] = (p[1] as f32 * a + bg.1 as f32 * ia).clamp(0.0, 255.0) as u8;
            p[2] = (p[2] as f32 * a + bg.0 as f32 * ia).clamp(0.0, 255.0) as u8;
            p[3] = 255;
        }
        let ok = save_bmp(&format!("{}\\setup_addurl_{}.bmp", dir, tag), &px, S_WW, S_HH);
        println!("[setup] setup_addurl_{} {}", tag, if ok { "OK" } else { "FAIL" });
    }
    st.set_edit_ai = usize::MAX;
    st.set_edit_buf.clear();
    st.title_busy.clear();

    // 删除确认卡：应用行 ✕ 点下去之后的样子（页 2 / 应用 0 展开）
    {
        st.set_page = 2;
        st.set_page_a = 1.0;
        st.guide_step = -1;
        st.tip = 0;
        st.set_scroll = 0;
        st.set_expanded = 0;
        st.set_hover = Ctl::ConfYes; // 鼠标停在「删除」上那张
        st.set_confirm = Ctl::AppDel(0);
        setup_render(st);
        std::thread::sleep(std::time::Duration::from_millis(280));
        s_dump_save(st, dir, "setup_confirm");
        st.set_confirm = Ctl::None;
        st.set_hover = Ctl::None;
    }
    // 同一张卡在**交互页**上的样子：那页比应用页挤（开关 + 两张名单卡），
    // 没有遮罩，得确认卡盖下去之后底下的字不会和卡里的字打架。
    {
        st.set_page = 1;
        st.set_expanded = usize::MAX;
        st.set_hover = Ctl::None;
        st.set_confirm = Ctl::ExcDel(0, 0);
        setup_render(st);
        std::thread::sleep(std::time::Duration::from_millis(280));
        s_dump_save(st, dir, "setup_confirm_exc");
        st.set_confirm = Ctl::None;
    }

    st.set_page = 0;
    st.tip = 0;
    st.guide_step = -1;
    st.set_expanded = usize::MAX;
}

// ───────────────────── 在线升级（v1：查版本） ─────────────────────

/// GitHub 最新 release API（Public 仓库可匿名读）。可用 `PIPIE_UPDATE_URL` 覆盖以便本地演练
/// （旧名 `PIGO_UPDATE_URL` 仍作兜底，本地演练脚本不用改）。
const UPDATE_API: &str = "https://api.github.com/repos/lifeFxxker/pipie/releases/latest";
/// 自动检查的最小间隔（秒）：防 GitHub 匿名 API 限流（60 次/时/IP）。
const UPDATE_MIN_INTERVAL: f64 = 6.0 * 3600.0;

/// 实际生效的检查间隔。`PIPIE_UPDATE_INTERVAL`（秒）可覆盖 ——
/// 常驻进程「不重启也能等到发版」靠的是周期定时器，回归总不能真等 6 小时。
fn update_min_interval() -> f64 {
    std::env::var("PIPIE_UPDATE_INTERVAL")
        .ok()
        .and_then(|s| s.trim().parse::<f64>().ok())
        .filter(|v| *v > 0.0)
        .unwrap_or(UPDATE_MIN_INTERVAL)
}

/// 当前版本（Cargo.toml 的 version）。
fn cur_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// 把版本串拆成数字段（`v1.2.3` / `1.2.3-beta` → [1,2,3]；每段取前导数字，缺省 0）。
fn ver_parts(v: &str) -> Vec<u64> {
    v.trim()
        .trim_start_matches('v')
        .split('.')
        .map(|s| {
            s.chars()
                .take_while(|c| c.is_ascii_digit())
                .collect::<String>()
                .parse::<u64>()
                .unwrap_or(0)
        })
        .collect()
}

/// `a > b`（逐段数字比，长度不齐按 0 补）。
fn version_gt(a: &str, b: &str) -> bool {
    let (pa, pb) = (ver_parts(a), ver_parts(b));
    for i in 0..pa.len().max(pb.len()) {
        let x = *pa.get(i).unwrap_or(&0);
        let y = *pb.get(i).unwrap_or(&0);
        if x != y {
            return x > y;
        }
    }
    false
}

/// 极简 JSON 取字符串字段（只用于 release 响应里的 `tag_name` 等**不含转义引号**的字段）。
fn json_str_field(json: &str, key: &str) -> Option<String> {
    let pat = format!("\"{}\"", key);
    let i = json.find(&pat)?;
    let rest = &json[i + pat.len()..];
    let rest = rest.trim_start().strip_prefix(':')?;
    let rest = rest.trim_start().strip_prefix('"')?;
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

/// HTTPS GET（WinINet，走系统代理/TLS）→ (HTTP 状态码, 原始字节)。
/// `on_read(已读字节, 总字节)` 每读一块回调一次（总字节未知时为 0）—— 下载进度用。
fn http_get_raw_cb(
    url: &str,
    mut on_read: Option<&mut dyn FnMut(u64, u64)>,
) -> Result<(u32, Vec<u8>), String> {
    use windows::core::PCWSTR;
    use windows::Win32::Networking::WinInet::{
        HttpQueryInfoW, InternetCloseHandle, InternetOpenUrlW, InternetOpenW, InternetReadFile,
        HTTP_QUERY_CONTENT_LENGTH, HTTP_QUERY_FLAG_NUMBER, HTTP_QUERY_STATUS_CODE,
        INTERNET_FLAG_NO_CACHE_WRITE, INTERNET_FLAG_RELOAD, INTERNET_OPEN_TYPE_PRECONFIG,
    };
    let agent = wide("PiPie-updater");
    let target = wide(url);
    unsafe {
        let h = InternetOpenW(
            PCWSTR(agent.as_ptr()),
            INTERNET_OPEN_TYPE_PRECONFIG.0,
            PCWSTR::null(),
            PCWSTR::null(),
            0,
        );
        if h.is_null() {
            return Err("InternetOpen 失败".into());
        }
        let flags = INTERNET_FLAG_RELOAD | INTERNET_FLAG_NO_CACHE_WRITE;
        let req = InternetOpenUrlW(h, PCWSTR(target.as_ptr()), None, flags, 0);
        if req.is_null() {
            let _ = InternetCloseHandle(h);
            return Err("连接失败（网络/DNS/被墙）".into());
        }
        // 状态码
        let mut status: u32 = 0;
        let mut slen = std::mem::size_of::<u32>() as u32;
        let _ = HttpQueryInfoW(
            req,
            HTTP_QUERY_STATUS_CODE | HTTP_QUERY_FLAG_NUMBER,
            Some(&mut status as *mut _ as *mut core::ffi::c_void),
            &mut slen,
            None,
        );
        // Content-Length（进度分母；可能没有）
        let mut total: u32 = 0;
        let mut tlen = std::mem::size_of::<u32>() as u32;
        let _ = HttpQueryInfoW(
            req,
            HTTP_QUERY_CONTENT_LENGTH | HTTP_QUERY_FLAG_NUMBER,
            Some(&mut total as *mut _ as *mut core::ffi::c_void),
            &mut tlen,
            None,
        );
        // 响应体
        let mut body: Vec<u8> = Vec::new();
        let mut buf = [0u8; 16384];
        loop {
            let mut read: u32 = 0;
            if InternetReadFile(
                req,
                buf.as_mut_ptr() as *mut core::ffi::c_void,
                buf.len() as u32,
                &mut read,
            )
            .is_err()
            {
                break;
            }
            if read == 0 {
                break;
            }
            body.extend_from_slice(&buf[..read as usize]);
            if let Some(cb) = on_read.as_mut() {
                cb(body.len() as u64, total as u64);
            }
        }
        let _ = InternetCloseHandle(req);
        let _ = InternetCloseHandle(h);
        Ok((status, body))
    }
}

fn http_get_raw(url: &str) -> Result<(u32, Vec<u8>), String> {
    http_get_raw_cb(url, None)
}

/// 取文本响应（release API / SHA256SUMS 用）。
fn http_get(url: &str) -> Result<(u32, String), String> {
    http_get_raw(url).map(|(s, b)| (s, String::from_utf8_lossy(&b).into_owned()))
}

enum UpdateOutcome {
    /// 已是最新（带上 latest 便于显示）
    UpToDate { latest: String },
    /// 有新版本（带 asset 下载地址）
    Available {
        latest: String,
        exe_url: String,
        sums_url: String,
    },
    /// 检查失败（原因是给人看的）
    Failed(String),
}

/// 下载进度 0..=100（UI 在 setup tick 里读，避免每块都投消息）。
static UPDATE_PROGRESS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
/// 检查成功时记下的 asset 下载地址（供随后「更新」按钮用）。
static UPDATE_EXE_URL: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());
static UPDATE_SUMS_URL: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());
/// 校验通过的新 exe 临时路径（供「重启并更新」用）。
static UPDATE_NEW_PATH: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());

fn update_api_url() -> String {
    std::env::var("PIPIE_UPDATE_URL")
        .or_else(|_| std::env::var("PIGO_UPDATE_URL"))
        .unwrap_or_else(|_| UPDATE_API.to_string())
}

/// 从 release JSON 的 assets 数组里找名为 `want` 的资产，返回 browser_download_url。
/// ⚠️ 只做朴素的花括号配对（GitHub asset 对象的字符串值里不含 `{}[]`）。
fn find_asset_url(json: &str, want: &str) -> Option<String> {
    let aidx = json.find("\"assets\"")?;
    let arr = aidx + json[aidx..].find('[')?;
    let bytes = json.as_bytes();
    let mut i = arr + 1;
    let mut depth = 0i32;
    let mut obj_start = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'{' => {
                if depth == 0 {
                    obj_start = i;
                }
                depth += 1;
            }
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    let obj = &json[obj_start..=i];
                    if json_str_field(obj, "name").as_deref() == Some(want) {
                        return json_str_field(obj, "browser_download_url");
                    }
                }
            }
            b']' if depth == 0 => break,
            _ => {}
        }
        i += 1;
    }
    None
}

/// 从 SHA256SUMS（`<hex>  <file>` 每行）里取 `file` 的 hash（小写）。`*file` = 二进制模式。
fn parse_sums(text: &str, file: &str) -> Option<String> {
    for line in text.lines() {
        let mut it = line.split_whitespace();
        let h = it.next()?;
        let f = it.next().unwrap_or("").trim_start_matches('*');
        if h.len() == 64 && f.eq_ignore_ascii_case(file) {
            return Some(h.to_ascii_lowercase());
        }
    }
    None
}

/// SHA-256（BCrypt/CNG，不引入哈希 crate）→ 小写 hex。
fn sha256_hex(data: &[u8]) -> Option<String> {
    use windows::core::PCWSTR;
    use windows::Win32::Security::Cryptography::{
        BCryptCloseAlgorithmProvider, BCryptCreateHash, BCryptDestroyHash, BCryptFinishHash,
        BCryptHashData, BCryptOpenAlgorithmProvider, BCRYPT_ALG_HANDLE, BCRYPT_HASH_HANDLE,
        BCRYPT_SHA256_ALGORITHM,
    };
    unsafe {
        let mut alg = BCRYPT_ALG_HANDLE::default();
        if BCryptOpenAlgorithmProvider(
            &mut alg,
            BCRYPT_SHA256_ALGORITHM,
            PCWSTR::null(),
            Default::default(),
        )
        .0
            < 0
        {
            return None;
        }
        let mut h = BCRYPT_HASH_HANDLE::default();
        if BCryptCreateHash(alg, &mut h, None, None, 0).0 < 0 {
            let _ = BCryptCloseAlgorithmProvider(alg, 0);
            return None;
        }
        let mut out = [0u8; 32];
        let ok = BCryptHashData(h, data, 0).0 >= 0 && BCryptFinishHash(h, &mut out, 0).0 >= 0;
        let _ = BCryptDestroyHash(h);
        let _ = BCryptCloseAlgorithmProvider(alg, 0);
        if ok {
            Some(out.iter().map(|b| format!("{:02x}", b)).collect())
        } else {
            None
        }
    }
}

/// 同步检查更新（网络阻塞，**不要**在 UI 线程直接调；UI 走后台线程）。
fn check_update_sync() -> UpdateOutcome {
    match http_get(&update_api_url()) {
        Ok((status, body)) => {
            if status == 403 || status == 429 {
                return UpdateOutcome::Failed(format!("请求受限（HTTP {status}）"));
            }
            if status != 200 {
                return UpdateOutcome::Failed(format!("HTTP {status}"));
            }
            match json_str_field(&body, "tag_name") {
                Some(tag) => {
                    let latest = tag.trim_start_matches('v').to_string();
                    if version_gt(&latest, cur_version()) {
                        match (
                            find_asset_url(&body, "PiPie.exe"),
                            find_asset_url(&body, "SHA256SUMS"),
                        ) {
                            (Some(exe_url), Some(sums_url)) => UpdateOutcome::Available {
                                latest,
                                exe_url,
                                sums_url,
                            },
                            _ => UpdateOutcome::Failed(
                                "release 里缺 PiPie.exe 或 SHA256SUMS asset".into(),
                            ),
                        }
                    } else {
                        UpdateOutcome::UpToDate { latest }
                    }
                }
                None => UpdateOutcome::Failed("响应里没有 tag_name".into()),
            }
        }
        Err(e) => UpdateOutcome::Failed(e),
    }
}

/// unix 秒（用于 `update_last_check`；注意 `now_s()` 是进程内相对时间，不是这个）。
fn unix_now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// 触发一次检查：置「检查中」、起后台线程（联网 + 解析），完成后 post `WM_APP_UPDATE_DONE`。
/// `manual=false` 先睡 3s 避开启动高峰。**绝不在 UI 线程同步联网。**
fn start_update_check(st: &mut State, manual: bool) {
    if st.update_st == 1 {
        return; // 已有检查在跑
    }
    st.update_st = 1;
    st.update_msg.clear();
    st.update_last_check = unix_now();
    unsafe { save_settings(st) };
    std::thread::spawn(move || {
        if !manual {
            std::thread::sleep(std::time::Duration::from_secs(3));
        }
        let (s, latest, msg) = match check_update_sync() {
            UpdateOutcome::UpToDate { latest } => (2u8, latest, "已是最新".to_string()),
            UpdateOutcome::Available {
                latest,
                exe_url,
                sums_url,
            } => {
                if let Ok(mut q) = UPDATE_EXE_URL.lock() {
                    *q = exe_url;
                }
                if let Ok(mut q) = UPDATE_SUMS_URL.lock() {
                    *q = sums_url;
                }
                (3u8, latest, String::new())
            }
            UpdateOutcome::Failed(e) => (4u8, String::new(), e),
        };
        if let Ok(mut q) = UPDATE_RESULT.lock() {
            *q = Some((s, latest, msg));
        }
        unsafe { post(WM_APP_UPDATE_DONE, 0, 0) };
    });
}

/// 下载 + SHA256 校验 → `<exe 同目录>\.pipie-update.new`；完成后 post 状态（5 就绪 / 4 失败）。
/// 下载 exe + 取 SHA256SUMS + 校验 + 写入 `dest`（同步，阻塞）。供后台线程与诊断 CLI 共用。
fn download_and_verify(exe_url: &str, sums_url: &str, dest: &str) -> Result<(), String> {
    let (code, bytes) = http_get_raw_cb(
        exe_url,
        Some(&mut |done, total| {
            let p = if total > 0 {
                ((done * 100 / total.max(1)) as u32).min(100)
            } else {
                0
            };
            UPDATE_PROGRESS.store(p, std::sync::atomic::Ordering::Relaxed);
        }),
    )
    .map_err(|e| format!("下载失败：{e}"))?;
    if code != 200 {
        return Err(format!("下载 HTTP {code}"));
    }
    UPDATE_PROGRESS.store(100, std::sync::atomic::Ordering::Relaxed);
    let (s_code, sums) = http_get(sums_url).map_err(|e| format!("取校验文件失败：{e}"))?;
    if s_code != 200 {
        return Err(format!("校验文件 HTTP {s_code}"));
    }
    let want = parse_sums(&sums, "PiPie.exe").ok_or("SHA256SUMS 里没有 PiPie.exe")?;
    let got = sha256_hex(&bytes).ok_or("SHA256 计算失败")?;
    if got != want {
        return Err(format!("校验不符（期望 {want}，实际 {got}）"));
    }
    std::fs::write(dest, &bytes).map_err(|e| format!("写入失败：{e}"))?;
    Ok(())
}

fn start_download(st: &mut State) {
    if st.update_st == 1 || st.update_st == 6 {
        return; // 正在检查 / 下载
    }
    let exe_url = UPDATE_EXE_URL.lock().map(|s| s.clone()).unwrap_or_default();
    let sums_url = UPDATE_SUMS_URL.lock().map(|s| s.clone()).unwrap_or_default();
    if exe_url.is_empty() || sums_url.is_empty() {
        st.update_st = 4;
        st.update_msg = "缺少下载地址（请先检查更新）".into();
        return;
    }
    st.update_st = 6;
    st.update_msg.clear();
    UPDATE_PROGRESS.store(0, std::sync::atomic::Ordering::Relaxed);
    let new_path = format!("{}\\.pipie-update.new", exe_dir());
    unsafe { save_settings(st) };
    std::thread::spawn(move || {
        let result = download_and_verify(&exe_url, &sums_url, &new_path);
        let (s, msg) = match result {
            Ok(()) => (5u8, String::new()),
            Err(e) => (4u8, e),
        };
        if s == 5 {
            if let Ok(mut q) = UPDATE_NEW_PATH.lock() {
                *q = new_path;
            }
        }
        if let Ok(mut q) = UPDATE_RESULT.lock() {
            *q = Some((s, String::new(), msg));
        }
        unsafe { post(WM_APP_UPDATE_DONE, 0, 0) };
    });
}

/// 应用更新：复制自身为 updater，带参启动，然后退出当前进程。
/// 不覆盖运行中的 exe（被锁），也不和新实例抢单实例互斥 —— 交给 updater。
fn apply_update(st: &mut State) {
    let new_path = UPDATE_NEW_PATH.lock().map(|s| s.clone()).unwrap_or_default();
    if new_path.is_empty() || !std::path::Path::new(&new_path).exists() {
        st.update_st = 4;
        st.update_msg = "找不到已下载的新版本，请重新检查".into();
        return;
    }
    let exe = exe_path();
    let updater = format!("{}\\.pipie-updater.exe", exe_dir());
    if std::fs::copy(&exe, &updater).is_err() {
        st.update_st = 4;
        st.update_msg = "无法创建 updater（目录不可写？便携版请放到用户目录）".into();
        return;
    }
    let args = format!(
        "\"{}\" --apply-update \"{}\" \"{}\" {}",
        updater,
        new_path,
        exe,
        std::process::id()
    );
    unsafe {
        use windows::core::{PCWSTR, PWSTR};
        use windows::Win32::Foundation::BOOL;
        use windows::Win32::System::Threading::{
            CreateProcessW, PROCESS_CREATION_FLAGS, PROCESS_INFORMATION, STARTUPINFOW,
        };
        let mut cmd: Vec<u16> = args.encode_utf16().chain(std::iter::once(0)).collect();
        let mut si = STARTUPINFOW::default();
        si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
        let mut pi = PROCESS_INFORMATION::default();
        let ok = CreateProcessW(
            PCWSTR::null(),
            PWSTR(cmd.as_mut_ptr()),
            None,
            None,
            BOOL(0),
            PROCESS_CREATION_FLAGS(0),
            None,
            PCWSTR::null(),
            &si,
            &mut pi,
        )
        .is_ok();
        if ok {
            std::process::exit(0); // 交棒给 updater；退出即释放 exe 锁 + 单实例互斥
        }
    }
    st.update_st = 4;
    st.update_msg = "启动更新程序失败".into();
}

/// updater 进程入口（`--apply-update`）：等旧进程退出 → 替换目标 exe → 启动新版。
unsafe fn updater_apply(new_path: &str, target: &str, parent_pid: u32) {
    use windows::core::{PCWSTR, PWSTR};
    use windows::Win32::Foundation::{CloseHandle, BOOL};
    use windows::Win32::Storage::FileSystem::{MoveFileExW, MOVEFILE_REPLACE_EXISTING};
    use windows::Win32::System::Threading::{
        CreateProcessW, OpenProcess, WaitForSingleObject, PROCESS_CREATION_FLAGS,
        PROCESS_INFORMATION, PROCESS_SYNCHRONIZE, STARTUPINFOW,
    };
    if let Ok(h) = OpenProcess(PROCESS_SYNCHRONIZE, false, parent_pid) {
        let _ = WaitForSingleObject(h, 30000);
        let _ = CloseHandle(h);
    }
    let wn = wide(new_path);
    let wt = wide(target);
    // 等不到 / 替换失败也照常启动目标（最坏就是旧版，不把人晾着）
    let _ = MoveFileExW(
        PCWSTR(wn.as_ptr()),
        PCWSTR(wt.as_ptr()),
        MOVEFILE_REPLACE_EXISTING,
    );
    let mut si = STARTUPINFOW::default();
    si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
    let mut pi = PROCESS_INFORMATION::default();
    let mut cmd: Vec<u16> = wt.clone();
    let _ = CreateProcessW(
        PCWSTR(wt.as_ptr()),
        PWSTR(cmd.as_mut_ptr()),
        None,
        None,
        BOOL(0),
        PROCESS_CREATION_FLAGS(0),
        None,
        PCWSTR::null(),
        &si,
        &mut pi,
    );
    if !pi.hProcess.is_invalid() {
        let _ = CloseHandle(pi.hProcess);
    }
    if !pi.hThread.is_invalid() {
        let _ = CloseHandle(pi.hThread);
    }
}

// ───────────────────── 入口 ─────────────────────

fn main() {
    let args: Vec<String> = std::env::args().collect();

    if args.len() >= 2 && args[1] == "--quit" {
        unsafe { kill_other_instances() };
        println!("[*] 已发送退出指令");
        return;
    }
    // updater 模式：必须是**最早**的分支 —— 不抢单实例互斥、不建窗口。
    if args.len() >= 5 && args[1] == "--apply-update" {
        let pid = args[4].parse::<u32>().unwrap_or(0);
        unsafe { updater_apply(&args[2], &args[3], pid) };
        return;
    }
    if args.len() >= 3 && args[1] == "--sha256" {
        // 诊断：算文件 SHA256（用 CNG），与 sha256sum 交叉验证用。
        match std::fs::read(&args[2]) {
            Ok(b) => match sha256_hex(&b) {
                Some(h) => println!("{h}"),
                None => {
                    eprintln!("sha256 计算失败");
                    std::process::exit(2);
                }
            },
            Err(e) => {
                eprintln!("读取失败: {e}");
                std::process::exit(2);
            }
        }
        return;
    }
    if args.len() >= 2 && args[1] == "--check-update" {
        // 诊断/测试：同步查一次更新并打印结果（不启动 UI）。供本地演练与脚本用。
        match check_update_sync() {
            UpdateOutcome::Available {
                latest,
                exe_url,
                sums_url,
            } => {
                println!("[update] 有新版本: v{latest}（当前 v{}）", cur_version());
                println!("         exe={exe_url}");
                println!("         sums={sums_url}");
            }
            UpdateOutcome::UpToDate { latest } => {
                println!("[update] 已是最新: v{latest}（当前 v{}）", cur_version())
            }
            UpdateOutcome::Failed(e) => {
                eprintln!("[update] 检查失败: {e}");
                std::process::exit(2);
            }
        }
        return;
    }
    if args.len() >= 3 && args[1] == "--update-fetch" {
        // 诊断：检查 + 下载 + 校验（同步），把新 exe 落到 <dir>\PiPie.exe。端到端演练用。
        let dir = args[2].clone();
        match check_update_sync() {
            UpdateOutcome::Available {
                latest,
                exe_url,
                sums_url,
            } => {
                let dest = format!("{dir}\\PiPie.exe");
                match download_and_verify(&exe_url, &sums_url, &dest) {
                    Ok(()) => println!("[update] 下载并校验通过 v{latest} → {dest}"),
                    Err(e) => {
                        eprintln!("[update] 失败: {e}");
                        std::process::exit(2);
                    }
                }
            }
            UpdateOutcome::UpToDate { latest } => println!("[update] 已最新 v{latest}"),
            UpdateOutcome::Failed(e) => {
                eprintln!("[update] 检查失败: {e}");
                std::process::exit(2);
            }
        }
        return;
    }
    if args.len() >= 2 && args[1] == "--dump" {
        let dir = args.get(2).cloned().unwrap_or_else(|| ".".into());
        unsafe {
            let _ = windows::Win32::System::Com::CoInitializeEx(
                None,
                windows::Win32::System::Com::COINIT_APARTMENTTHREADED,
            );
            dump_all(&dir)
        };
        return;
    }
    // 诊断：解析目标 → 找「同程序的已开窗口」（只打印，不真的启动）
    if args.len() >= 2 && args[1] == "--findwin" {
        unsafe {
            let _ = windows::Win32::System::Com::CoInitializeEx(
                None,
                windows::Win32::System::Com::COINIT_APARTMENTTHREADED,
            );
            let t = args.get(2).cloned().unwrap_or_default();
            println!("target = {t}");
            match resolve_target_exe(&t) {
                Some(exe) => {
                    println!("exe    = {exe}");
                    match find_window_of_exe(&exe) {
                        Some(hw) => {
                            let mut b = [0u16; 256];
                            let n = GetWindowTextW(hw, &mut b).max(0) as usize;
                            println!("found  = HWND({:?}) title={:?}", hw.0,
                                String::from_utf16_lossy(&b[..n.min(b.len())]));
                        }
                        None => println!("found  = <无>"),
                    }
                }
                None => println!("exe    = <目标不是可执行文件>"),
            }
        }
        return;
    }
    // 诊断：只抓网页标题（测 WinINet 那条路）
    if args.len() >= 2 && args[1] == "--fetchtitle" {
        unsafe {
            let u = args.get(2).cloned().unwrap_or_default();
            match fetch_page_title(&u) {
                Some(t) => println!("title = {t}"),
                None => println!("title = <没抓到>"),
            }
        }
        return;
    }
    // 诊断：模拟「把若干文件拖进黑白名单卡片」。
    // `--dropsim <px> <py> <path>...`（px/py = **卡片内逻辑坐标**的落点）
    // ⚠️ 不能用合成的 `WM_DROPFILES` 跨进程测：HDROP 是**本进程私有堆**的句柄，
    //    另一个进程 GlobalLock 不上（实测 DragQueryFile 恒返回 0）。
    //    所以这里直接调与拖放处理器**同一个** `card_at` + `exe_name_of`。
    if args.len() >= 5 && args[1] == "--dropsim" {
        unsafe {
            let _ = windows::Win32::System::Com::CoInitializeEx(
                None,
                windows::Win32::System::Com::COINIT_APARTMENTTHREADED,
            );
            let mut st = State::new();
            init_config_path(&mut st);
            load_ini(&mut st);
            let px = args[2].parse::<i32>().unwrap_or(-1);
            let py = args[3].parse::<i32>().unwrap_or(-1);
            let card = card_at(px, py);
            // 拖放只进当前生效的卡片（与真实 WM_DROPFILES 处理器一致）
            let c = st.exc_mode as usize;
            let mut added = 0;
            for p in &args[4..] {
                if let Some(n) = exe_name_of(p) {
                    let bucket = if c == 0 { &mut st.exc } else { &mut st.exc_w };
                    if !bucket.iter().any(|e| e == &n) {
                        bucket.push(n);
                        added += 1;
                    }
                }
            }
            if added > 0 {
                save_settings(&st);
            }
            println!(
                "[dropsim] card={:?} 命中卡片={} added={} 黑={:?} 白={:?}",
                card, c, added, st.exc, st.exc_w
            );
        }
        return;
    }
    // 诊断：完整的「网址已打开就切过去」（抓标题 → UIA 找标签 → 选中 + 置前）
    if args.len() >= 2 && args[1] == "--focusurl" {
        unsafe {
            // ⚠️ UIA（CUIAutomation）是 COM 对象，这个分支必须自己 CoInitializeEx，
            //    否则 CoCreateInstance 静默失败、整个函数 20ms 就返回（第一版就踩了）。
            let _ = windows::Win32::System::Com::CoInitializeEx(
                None,
                windows::Win32::System::Com::COINIT_APARTMENTTHREADED,
            );
            let u = args.get(2).cloned().unwrap_or_default();
            println!("browser windows = {}", windows_of_class("Chrome_WidgetWin_1").len());
            let t0 = Instant::now();
            let hit = switch_to_open_tab(&u);
            println!("switch = {hit}   (耗时 {:?})", t0.elapsed());
        }
        return;
    }
    // 诊断：列出每个应用目标的图标提取结果（原生尺寸 / 是否有图标）
    if args.len() >= 2 && args[1] == "--icons" {
        unsafe {
            let _ = windows::Win32::System::Com::CoInitializeEx(
                None,
                windows::Win32::System::Com::COINIT_APARTMENTTHREADED,
            );
            let mut st = State::new();
            init_config_path(&mut st);
            load_ini(&mut st);
            for a in &st.apps {
                eprintln!(
                    "[app] {:<12} target={:<40} icon={}",
                    a.name,
                    a.target,
                    match &a.icon {
                        Some(t) => {
                            let base = t.levels.first().map(|l| l.0).unwrap_or(0);
                            let mips: Vec<String> =
                                t.levels.iter().skip(1).map(|l| l.0.to_string()).collect();
                            format!("{}px-texture + mip[{}]", base, mips.join(","))
                        }
                        None => "none(字母磁贴)".to_string(),
                    }
                );
            }
        }
        return;
    }

    // 诊断：把设置窗口的四个页面离屏渲染成 BMP（配 bmp2png.py 核对排版）
    if args.len() >= 2 && args[1] == "--settings-dump" {
        let dir = args.get(2).cloned().unwrap_or_else(|| ".".into());
        unsafe {
            let _ = windows::Win32::System::Com::CoInitializeEx(
                None,
                windows::Win32::System::Com::COINIT_APARTMENTTHREADED,
            );
            let mut st = State::new();
            st.ini_path = format!("{}\\settings.ini", dir);
            load_ini(&mut st);
            read_persistent(&mut st);
            init_fonts(&mut st);
            settings_dump(&dir, &mut st);
        }
        return;
    }

    // COM 初始化必须尽早：IShellItemImageFactory（高清图标）依赖 STA，
    // 而 load_ini() 里就会去解码图标 —— 漏了这句会让主流程拿不到高清图标。
    // 注意：拖放（RegisterDragDrop）需要 OleInitialize 的 OLE 服务层，
    // 只 CoInitializeEx 的线程上注册会返回 E_OUTOFMEMORY —— 两个都要。
    unsafe {
        let _ = windows::Win32::System::Com::CoInitializeEx(
            None,
            windows::Win32::System::Com::COINIT_APARTMENTTHREADED,
        );
        let hr = windows::Win32::System::Ole::OleInitialize(None);
        if hr.is_err() {
            drag_log(&format!("OleInitialize {:?}", hr));
        }
    }

    // ── 输入卡顿诊断：看门狗线程 + 日志（LocalAppData\PiPie\logs）──
    // 排查「闲置后鼠标卡 30 秒」：HOOK_SLOW=我们的钩子回调慢；
    // WATCHDOG_GAP=我们整个进程被冻结（待机/换页）；两者都干净=卡在别处。
    {
        // 控制台 QuickEdit 是一个免费的假死源：用户（或误触）在控制台窗口里
        // 选中一段文本，所有 println! 的线程都会被挂起直到取消选中 —— 先禁掉。
        const STD_OUTPUT_HANDLE: u32 = -11i32 as u32;
        const ENABLE_QUICK_EDIT_MODE: u32 = 0x0040;
        const ENABLE_EXTENDED_FLAGS: u32 = 0x0080;
        extern "system" {
            #[link_name = "GetStdHandle"]
            fn get_std_handle(which: u32) -> isize;
            #[link_name = "GetConsoleMode"]
            fn get_console_mode(h: isize, mode: *mut u32) -> i32;
            #[link_name = "SetConsoleMode"]
            fn set_console_mode(h: isize, mode: u32) -> i32;
        }
        unsafe {
            let h = get_std_handle(STD_OUTPUT_HANDLE);
            let mut mode = 0u32;
            if get_console_mode(h, &mut mode) != 0 {
                set_console_mode(h, (mode & !ENABLE_QUICK_EDIT_MODE) | ENABLE_EXTENDED_FLAGS);
            }
        }
        std::thread::spawn(|| {
            use std::time::Duration;
            let dir = log_dir();
            let _ = std::fs::create_dir_all(&dir);
            let path = format!("{}\\radial_log.txt", dir);
            // 超 8MB 重建，防止无限膨胀
            if let Ok(m) = std::fs::metadata(&path) {
                if m.len() > 8_000_000 {
                    let _ = std::fs::remove_file(&path);
                }
            }
            let build = std::fs::metadata(std::env::current_exe().unwrap_or_default())
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            diag_line(&path, &format!(
                "=== 启动 exe_build_unix={} ===", build));
            diag_line(&path,
                "--- 日志怎么读（出问题时按这个顺序看）---");
            diag_line(&path,
                "  SNAPSHOT 每 5 秒一条：列出**所有跨线程开关的当前值**。");
            diag_line(&path,
                "    · 「全清」= 正常。若某个开关该清没清（MENU_OPEN 尤其），就是它卡住了。");
            diag_line(&path,
                "    · last_mouse=N ms ago = 距上次鼠标事件的毫秒。持续涨 = 输入进不来。");
            diag_line(&path,
                "  MOUSE_IDLE 每 5 秒以上无鼠标记一条（比 HOOK_GAP 细 6 倍，用于定位失灵**起止时刻**）。");
            diag_line(&path,
                "  HOOK_SLOW/KEY_SLOW = 钩子回调耗时。成簇出现 = 事件在别处堵住后涌入。");
            diag_line(&path,
                "  POST_DROP = PostMessage 失败（UI 线程队列满/窗口已销毁 → 菜单不响应）。");
            diag_line(&path,
                "  HOOK_LOST = 钩子线程退场，程序会自行重启（已自愈）。");
            diag_line(&path,
                "  WATCHDOG_GAP = 本进程整体被冻结（待机/换页）。");
            diag_line(&path, "--- 开始 ---");
            let mut consumed: u64 = 0;
            let mut lines: u32 = 0;
            let mut hb_ticks: u32 = 0;
            loop {
                let t0 = std::time::Instant::now();
                std::thread::sleep(Duration::from_secs(1));
                let dt = t0.elapsed();
                if dt > Duration::from_secs(3) {
                    diag_line(&path, &format!(
                        "WATCHDOG_GAP {}ms —— 本进程整线程被冻结（待机/换页/系统级停顿）",
                        dt.as_millis()));
                    lines += 1;
                }
                // ── 钩子存活检查 ──
                // 钩子线程一旦非正常退场（GetMessage 出错等），内核里会残留一个
                // 挂在死线程上的钩子：系统继续往那条链投事件，但没人能处理 →
                // 症状是「只有当前窗口能点，任务栏和其它窗口全点不动，菜单也弹不出来」。
                // 这状态**无法在进程内修复**（死线程摘不到自己的钩子），唯一出路是重启。
                // 所以这里每 3 秒查一次：钩子该在却不在了 → 摘干净 + 重启自己。
                {
                    let listening = ST_RUNNING_FLAG.load(Ordering::Relaxed);
                    let tid = HOOK_TID.load(Ordering::Relaxed);
                    let mut dead = false;
                    if listening && tid != 0 {
                        use windows::Win32::Foundation::CloseHandle;
                        use windows::Win32::System::Threading::{
                            OpenThread, WaitForSingleObject, THREAD_ALL_ACCESS,
                        };
                        unsafe {
                            match OpenThread(THREAD_ALL_ACCESS, false, tid) {
                                Ok(th) => {
                                    // WAIT_OBJECT_0 = 0 → 线程已退场
                                    let w = WaitForSingleObject(th, 0);
                                    let _ = CloseHandle(th);
                                    dead = w.0 == 0;
                                }
                                Err(_) => dead = true, // 线程已销毁
                            }
                        }
                    }
                    if dead {
                        // 走到这里马上就要 exit，所以不需要「只打一次」的标志位
                        diag_line(&path,
                            "HOOK_LOST 钩子线程已退场，内核里可能残留死钩子 → 立即重启自愈");
                        diag_push(DK_HOOK_LOST, 0);
                        // 先摘（可能失败但无害），再重启
                        unsafe {
                            let h = HOOK_HANDLE.swap(0, Ordering::SeqCst);
                            if h != 0 {
                                let _ = UnhookWindowsHookEx(HHOOK(h as *mut c_void));
                            }
                            let k = KEY_HOOK_HANDLE.swap(0, Ordering::SeqCst);
                            if k != 0 {
                                let _ = UnhookWindowsHookEx(HHOOK(k as *mut c_void));
                            }
                        }
                        std::thread::sleep(Duration::from_millis(200));
                        std::process::exit(0x0000_0009); // 看门狗自愈码，父进程/脚本可识别
                    }
                }
                // 清空环形缓冲
                let head = LOG_HEAD.load(Ordering::Relaxed);
                let n = (head - consumed).min(LOG_RING_SLOTS as u64);
                for k in 0..n {
                    let i = ((head - n + k) & (LOG_RING_SLOTS as u64 - 1)) as usize;
                    let v = LOG_RING[i].load(Ordering::Acquire);
                    if v == 0 {
                        continue;
                    }
                    let tick = (v >> 32) as u32;
                    let kind = ((v >> 24) & 0xff) as u8;
                    let aux = (v & 0x00ff_ffff) as u32;
                    // SNAPSHOT 的 aux 是**位掩码**，单独展开成人能读的状态串；
                    // 其余事件的 aux 是毫秒数。
                    if kind == DK_SNAPSHOT {
                        diag_line(&path, &format!(
                            "SNAPSHOT      {}", state_str(aux)));
                    } else {
                        diag_line(&path, &format!(
                            "{:>10} {} aux={}ms", (tick as f64) / 1000.0, diag_kind_name(kind), aux));
                    }
                    lines += 1;
                }
                consumed = head;
                if lines > 20_000 {
                    diag_line(&path, "=== 单次会话日志行数超限，暂停记录（重启程序恢复）===");
                    break;
                }

                // ── 每 5 秒一条状态快照（用户要求「尽可能多打印」）──
                // 为什么必须落盘而不是丢进环形缓冲：环形缓冲 128 槽，
                // 一次鼠标风暴（拖动 = 每秒 60+ 事件）就能把快照冲掉。
                // 快照要**保证**每小时至少落 720 条，否则出事时正好没有现场。
                //
                // `st` 在 UI 线程，看门狗不能直接读（跨线程 &mut = UB）。
                // 绕法：原子量的部分在本线程直接读，UI 私有状态（hiding/entering 等）
                // 由 UI 线程自己每帧推一份到 SNAP_STATE 原子量。
                hb_ticks += 1;
                if hb_ticks % 5 == 0 {
                    let bits = state_bits(
                            SNAP_HIDING.load(Ordering::Relaxed),
                            SNAP_ENTERING.load(Ordering::Relaxed),
                            SNAP_PENDING_RM.load(Ordering::Relaxed),
                            SNAP_FOCUS.load(Ordering::Relaxed) != usize::MAX,
                            SNAP_HOVER_CHIP.load(Ordering::Relaxed) != usize::MAX,
                            SNAP_HIT_APP.load(Ordering::Relaxed) != usize::MAX,
                        );
                    let last_mouse = unsafe { GetTickCount() }.wrapping_sub(LAST_HOOK_TICK.load(Ordering::Relaxed));
                    diag_line(&path, &format!(
                        // `kb=` = 键盘钩子当前是否挂着（按需装卸，见 KEY_HOOK_WANT）
                        "SNAPSHOT      {}  | hook_tid={} listening={} kb={} last_mouse={}ms ago",
                        state_str(bits),
                        HOOK_TID.load(Ordering::Relaxed),
                        ST_RUNNING_FLAG.load(Ordering::Relaxed) as u8,
                        (KEY_HOOK_LOCAL.load(Ordering::Relaxed) != 0) as u8,
                        last_mouse,
                    ));
                    lines += 1;
                }
            }
        });
    }

    // 单实例保护
    unsafe {
        // 互斥体名可用环境变量 `RL_INSTANCE` 覆盖：自动化测试要能并行跑多份实例，
        // 而正常使用时不要设（否则能开出好几份，钩子会打架）。
        let suffix = std::env::var("RL_INSTANCE").unwrap_or_default();
        let mname: Vec<u16> = wide(&format!(
            "Global\\PiPie_SingleInstance_v1{}",
            suffix
        ));
        let h = CreateMutexW(None, BOOL(0), PCWSTR(mname.as_ptr()));
        if h.is_ok() && GetLastError() == ERROR_ALREADY_EXISTS {
            // 互斥体被占**不等于**「已有一个可用的实例在跑」。
            // 踩过的坑：进程异常终止 / 会话切换会在进程表里留下「已退出但对象未回收」
            // 的僵尸，它们的句柄还持着互斥体 —— 于是每次启动都被静默取消，
            // 而用户看到的是「程序起不来」，且 taskkill 只会回「没有此任务的实例在运行」。
            // 判据：**真正活着的实例一定有一个宿主窗口**（RLHost）。
            // 没有窗口 = 占位的只是僵尸 → 忽略互斥体，继续正常启动。
            let host_cls: Vec<u16> = wide("RLHost");
            let live = FindWindowW(PCWSTR(host_cls.as_ptr()), PCWSTR::null()).is_ok();
            if live {
                eprintln!("[!] PiPie 已在运行，本次启动已取消。");
                return;
            }
            eprintln!("[*] 检测到残留互斥体（无存活实例），已忽略并继续启动。");
        }
    }

    unsafe {
        let _ = SetProcessDPIAware();
        let hinst = HINSTANCE(GetModuleHandleW(None).unwrap_or(HMODULE(ptr::null_mut())).0);
        let mut st = State::new();
        let p: *mut State = &mut st;

        // 配置
        init_config_path(&mut st);
        load_ini(&mut st);
        read_persistent(&mut st);
        // 更新残留清理：updater 用完的副本 / 已应用的新版本临时文件
        // （新 PiPie 由 updater 拉起时顺手删；updater 可能还没完全退出，删不掉就下次再来）
        for f in [".pipie-updater.exe", ".pipie-update.new"] {
            let _ = std::fs::remove_file(format!("{}\\{}", exe_dir(), f));
        }
        // 开机自启：**ini 是唯一真相**（缺省 = 开）→ 启动时把注册表同步过去，
        // 「默认勾选」才成立；用户在设置页主动关才会写 0 并把注册表项删掉。
        // ⚠️ 用 `autostart_sync`（只补差集）而不是 `autostart_set`：后者会把值
        // 无脑改写成**当前进程的 exe 路径**，沙箱/测试一跑就把用户自启项劫持走。
        autostart_sync(st.autostart);

        // 渲染资源
        init_surfaces(&mut st);
        init_fonts(&mut st);

        // 托盘宿主窗口（不可见普通窗口：托盘菜单需要前台窗口）
        let suffix = std::env::var("RL_INSTANCE").unwrap_or_default();
        let cls: Vec<u16> = wide("RLHost");
        RegisterClassW(&WNDCLASSW {
            lpfnWndProc: Some(host_proc),
            hInstance: hinst,
            lpszClassName: PCWSTR(cls.as_ptr()),
            ..Default::default()
        });
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            PCWSTR(cls.as_ptr()),
            // 标题带上测试实例后缀：自动化脚本要靠它区分「自己那份」，
            // 只按类名找会命中用户正在跑的实例（类名对所有实例都一样）。
            PCWSTR(
                wide(&format!("PiPieHost{}", suffix)).as_ptr(),
            ),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            HWND(ptr::null_mut()),
            HMENU(ptr::null_mut()),
            hinst,
            None,
        )
        .unwrap_or(HWND(ptr::null_mut()));

        // 圆环分层窗口
        let rcls: Vec<u16> = wide("RLRing");
        RegisterClassW(&WNDCLASSW {
            lpfnWndProc: Some(ring_proc),
            hInstance: hinst,
            lpszClassName: PCWSTR(rcls.as_ptr()),
            ..Default::default()
        });
        let rhwnd = CreateWindowExW(
            WINDOW_EX_STYLE(
                WS_EX_LAYERED.0 | WS_EX_TOOLWINDOW.0 | WS_EX_TOPMOST.0 | WS_EX_NOACTIVATE.0,
            ),
            PCWSTR(rcls.as_ptr()),
            PCWSTR(wide(&format!("PiPieRing{}", suffix)).as_ptr()),
            WS_POPUP,
            0,
            0,
            CANVAS,
            CANVAS,
            HWND(ptr::null_mut()),
            HMENU(ptr::null_mut()),
            hinst,
            None,
        )
        .unwrap_or(HWND(ptr::null_mut()));

        SetWindowLongPtrW(hwnd, GWLP_USERDATA, p as isize);
        SetWindowLongPtrW(rhwnd, GWLP_USERDATA, p as isize);
        MAIN_HWND.store(hwnd.0 as isize, Ordering::SeqCst);
        RING_HWND.store(rhwnd.0 as isize, Ordering::SeqCst);
        // 实时玻璃的后台抓屏线程（菜单没开时它挂在条件变量上，0 唤醒）
        glass_live_spawn();

        // 在线升级：启动静默检查（**只查不装**）。首次启动必查，之后 ≥6h 一次（防限流）。
        // 后台线程 sleep 3s 再联网，避免和启动抢资源；结果 post 回 UI 刷关于页。
        if st.update_auto && unix_now() - st.update_last_check >= update_min_interval() {
            start_update_check(&mut st, false);
        }

        // 「例外名单」常驻刷新定时器：在 MAIN_HWND 上只 SetTimer 一次、永不 KillTimer，
        // 独立于渲染帧定时器（帧定时器在菜单关 / 静止时会停）。
        // 修用户 2026-10-11 报的「黑名单里能开一次、之后切到哪都打不开」（EXEMPT 冻结）。
        let _ = SetTimer(hwnd, TIMER_EXEMPT, 250, None);

        // 设置窗口：分层窗口（ULW）+ 自绘。
        // 必须分层 —— 普通窗口的圆角只能靠 SetWindowRgn，那条边是**硬边**，
        // 放大看就是楼梯；分层窗口能带 AA 圆角和我们自己画的投影。
        let scls: Vec<u16> = wide("RLSetup");
        RegisterClassW(&WNDCLASSW {
            lpfnWndProc: Some(setup_proc),
            hInstance: hinst,
            lpszClassName: PCWSTR(scls.as_ptr()),
            ..Default::default()
        });
        // ⚠️ 设置窗口**不能**带 `WS_EX_TOOLWINDOW` —— 那会让它在任务栏里消失
        // （用户 2026-07-11 报「任务栏找不到」）。加 `WS_EX_APPWINDOW` 强制出现在任务栏。
        let shwnd = CreateWindowExW(
            WINDOW_EX_STYLE(WS_EX_LAYERED.0 | WS_EX_APPWINDOW.0),
            PCWSTR(scls.as_ptr()),
            PCWSTR::null(),
            WS_POPUP,
            0,
            0,
            S_WW,
            S_HH,
            HWND(ptr::null_mut()),
            HMENU(ptr::null_mut()),
            hinst,
            None,
        )
        .unwrap_or(HWND(ptr::null_mut()));
        SetWindowLongPtrW(shwnd, GWLP_USERDATA, p as isize);
        SET_HWND.store(shwnd.0 as isize, Ordering::SeqCst);
        // 接受从资源管理器拖进来的文件（拖进黑白名单卡片）
        DragAcceptFiles(shwnd, true);
        // --settings：启动后直接把设置页调出来（便于做快捷方式 / 自动化核对）
        if args.iter().any(|a| a == "--settings") {
            open_settings(&mut st);
        }

        // 拖放入环：注册 OLE 放置目标 + 放行跨完整性级别的拖放消息（提权场景）
        let drop_target: IDropTarget = RingDropTarget.into();
        match RegisterDragDrop(rhwnd, &drop_target) {
            Ok(()) => drag_log("RegisterDragDrop OK"),
            Err(e) => {
                drag_log(&format!("RegisterDragDrop FAIL {:?}", e.code()));
                eprintln!("[!] 拖放目标注册失败：{:?}", e.code());
            }
        }
        DragAcceptFiles(rhwnd, true);
        let _ = ChangeWindowMessageFilterEx(rhwnd, 0x0233, MSGFLT_ALLOW, None); // WM_DROPFILES
        let _ = ChangeWindowMessageFilterEx(rhwnd, 0x004A, MSGFLT_ALLOW, None); // WM_COPYDATA
        let _ = ChangeWindowMessageFilterEx(rhwnd, 0x0049, MSGFLT_ALLOW, None);

        add_tray_icon(hwnd);
        let _ = RegisterHotKey(hwnd, HOTKEY_TOGGLE, MOD_CONTROL | MOD_ALT, 0x52); // Ctrl+Alt+R

        // 钩子装在专用线程里（见 set_running 注释）
        let listen_on = st.listening;
        set_running(&mut st, listen_on);

        // 启动「亮个相」：在光标处把环亮出来 + 头像说一句，2 秒后自己收起。
        // ⚠️ 环上没东西就不展露：`draw_scene` 在 n==0 时直接 return，
        //    硬弹只会亮一个空盘 + 气泡根本画不出来。
        if !st.apps.is_empty() {
            let mut cp = POINT::default();
            let _ = GetCursorPos(&mut cp);
            show_menu(&mut st, cp.x, cp.y);
            say(&mut st, REVEAL_MSG, "", REVEAL_SECS);
            st.hide_at = now_s() + REVEAL_SECS;
        }

        println!("[*] PiPie 已就绪：{} · 托盘菜单 / Ctrl+Alt+R 开关监听", about_hotkeys(&st));

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
            let _ = TranslateMessage(&msg);
            let _ = DispatchMessageW(&msg);
        }
        std::mem::forget(st);
    }
}
