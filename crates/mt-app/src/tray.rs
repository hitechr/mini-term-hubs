//! 系统托盘状态灯(audit #21 / T 批)。对照 `src-tauri/src/tray.rs` + `src/store.ts`
//! 的 `syncTrayStatus` / `collectAiProjects`。
//!
//! # 颜色语义(与主窗口 StatusDot 按**语义**对齐,非逐色复刻)
//!
//! ```text
//! 黄 = 有 pane 需要用户确认(授权/输入请求,含 error 异常)
//! 蓝 = 有 pane 处理中(ai-working,含 API 重试)
//! 绿 = 有已完成且未读的回答(激活主窗口即清除)
//! 灰 = 全部安静(静止不闪)
//! ```
//!
//! 静止时停在最高优先级色(黄>蓝>绿)。闪烁三档:**聚焦不闪**;失焦多状态 =
//! 同一灯位颜色轮转;失焦单状态 = 状态变化后短促亮暗呼吸 [`BURST_FRAMES`] 帧再
//! 定格全亮(原注释:「持续呼吸闪太抢注意力(用户反馈)」)。
//!
//! # 与装机版的分工翻转
//!
//! 装机版的 Rust 侧**零业务逻辑** —— 裁剪/排序/emoji/i18n/tooltip 拼接/去重签名
//! 全在前端 TS 里,`set_tray_status` 只是个搬运工。GPUI 侧没有「前端」,这些整个
//! 搬进本模块([`menu_entries`] / [`tooltip`] / [`build_snapshot`]);装机版那套
//! `seq` 乱序裁决**整个删掉** —— 快照是主线程同步算出来的,不存在乱序覆盖。
//!
//! # 线程模型
//!
//! ```text
//! GPUI 主线程                          托盘线程(mt-tray)
//! ───────────                          ────────────────
//! store 变化 → build_snapshot          隐藏消息窗口 + GetMessage 循环
//!            → Tray::push(去重)  ──┐   ├─ WM_TRAY_SYNC  : 取快照 → 重画图标/tooltip
//!                                  └──►├─ WM_TIMER 600ms: 走一帧闪烁
//!                                      ├─ WM_TRAY_CB    : 左键/右键
//!  Workspace::on_tray_event  ◄─────────┘                  └→ 唤主窗 + 事件回主线程
//!  (futures channel + 前台任务)
//! ```
//!
//! 托盘回调**不碰任何 Entity**:与 `ai.rs` 同一套路数(后台线程只管往 channel
//! 里丢,主线程上的前台任务醒来后再改 store)。唤起主窗口那一步是纯 Win32
//! (`ShowWindow`/`SetForegroundWindow`),在托盘线程里做 —— 点击托盘图标时
//! 前台锁正好允许本进程抢前台,换到主线程就错过这个窗口了。
//!
//! macOS 侧结构不同:`NSStatusItem` 必须在主线程,于是没有托盘线程 —— push 直接
//! 落在 GPUI 主线程上,闪烁由主 runloop 的 `NSTimer` 推(详见该 `platform` 模块)。
//!
//! # 为什么不用 `tray-icon` crate
//!
//! 它会拉进 muda + 一套全局事件循环钩子,与 gpui 自己的 Windows 消息循环共存有
//! 风险(装机版是靠 tauri 的集成才避开的)。这里 Win32 直写 `Shell_NotifyIconW`,
//! 代价是 **HICON 生命周期得自己管**(见 [`OwnedIcon`],换一次图标销毁一个旧句柄)。

use futures::channel::mpsc::{self, UnboundedReceiver};

use crate::i18n::{t, tr};
use crate::store::{AiProjectKind, AiProjects};

// ─── 常量(逐条对齐 src-tauri/src/tray.rs:31-45) ──────────────

/// 闪烁帧间隔(ms)。
const BLINK_MS: u32 = 600;
/// 单状态时状态变化后的短促闪烁帧数(约 3.6s),之后定格全亮 ——
/// 持续呼吸闪太抢注意力(用户反馈),只在「有新变化」时闪一阵提醒。
const BURST_FRAMES: usize = 6;
/// 暗帧的 alpha 系数。
const DIM: f32 = 0.35;
/// `>` 笔画半宽与画布边长之比。16px 画布上约 2px 宽,再细就在 Win32 的
/// 100% DPI 下糊成一片。
const STROKE_RATIO: f32 = 0.065;

// 外框(那个「终端窗口」轮廓)的几何,均为画布边长之比。
/// 边框到画布边缘的留白。
const FRAME_INSET: f32 = 0.06;
/// 圆角半径。
const FRAME_RADIUS: f32 = 0.16;
/// 描边线宽的一半。
const FRAME_STROKE: f32 = 0.04;

/// 安静(灰)态下 `>` 保持全亮的帧数,之后开始淡出。
const IDLE_HOLD_FRAMES: usize = 3;
/// 淡出用掉的帧数(约 3s)。淡完只剩外框,图标不会整个消失 ——
/// 消失会被当成「程序退了」。
const IDLE_FADE_FRAMES: usize = 5;

// Apple 系统色板(装机版是 macOS 菜单栏优先设计,颜色照搬)
const GRAY: [u8; 3] = [0x8E, 0x8E, 0x93];
const BLUE: [u8; 3] = [0x0A, 0x84, 0xFF];
const YELLOW: [u8; 3] = [0xFF, 0xCC, 0x00];
const GREEN: [u8; 3] = [0x34, 0xC7, 0x59];

// 外框的中性色。macOS 按菜单栏明暗二选一(那边的惯例是单色描边图标);
// Win32 用中性灰 —— 深浅两种任务栏都看得见,省掉读注册表判主题那一步。
//
// 三个都只在各自的 `platform` 模块里用,Linux 上那两个模块都不编译 ——
// 与本文件的画帧纯函数同一道门控。
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
const FRAME_LIGHT: [u8; 3] = [0xFF, 0xFF, 0xFF];
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
const FRAME_DARK: [u8; 3] = [0x1D, 0x1D, 0x1F];
#[cfg_attr(not(windows), allow(dead_code))]
const FRAME_NEUTRAL: [u8; 3] = GRAY;

/// 托盘菜单里的档位 emoji(`store.ts:330` 的 `KIND_EMOJI`)。
pub fn kind_emoji(kind: AiProjectKind) -> &'static str {
    match kind {
        AiProjectKind::Attention => "🟡",
        AiProjectKind::Working => "🔵",
        AiProjectKind::Done => "🟢",
        AiProjectKind::Idle => "⚪",
    }
}

// ─── 主线程算出来、推给托盘线程的一份全量快照 ─────────────────

/// 托盘右键菜单里的一条(label 含 emoji 灯色与 i18n 状态文案)。
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct TrayEntry {
    pub id: String,
    pub label: String,
}

/// 三盏灯的亮灭。`false/false/false` = 灰(安静)。
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Lamps {
    pub attention: bool,
    pub working: bool,
    pub done: bool,
}

/// 一次推送的全部内容。
#[derive(Clone, Default, PartialEq, Eq, Debug)]
pub struct TraySnapshot {
    /// 设置页「状态栏图标」开关。false = 图标隐藏且全部逻辑早退。
    pub enabled: bool,
    /// 主窗口是否聚焦(聚焦不闪)。
    pub focused: bool,
    pub lamps: Lamps,
    /// 空串 = 不设 tooltip。
    pub tooltip: String,
    pub projects: Vec<TrayEntry>,
    /// 去重签名(`store.ts:336-338`):`enabled|focused|attention|working|done|labels`。
    /// **不含 tooltip** —— 它是三个计数的等价导出,计数没变 tooltip 必然没变。
    pub signature: String,
}

/// 托盘线程送回主线程的交互。
pub enum TrayEvent {
    /// 左键点了图标。窗口**已经被唤起**(不看开关),这里只负责
    /// `trayClickFocus` 门控下的「跳到待办 pane」。
    Clicked,
    /// 右键菜单点了某个项目。**不受 `trayClickFocus` 管辖**。
    ProjectClicked(String),
}

// ─── 纯函数:菜单 / tooltip / 快照 ────────────────────────────

/// 菜单条目:取前 `max` 个,label = `emoji + 空格 + 项目名 + " · " + 状态文案`
/// (`store.ts:331-334` 逐字)。**超出上限的直接不显示,没有省略提示**。
pub fn menu_entries(projects: &AiProjects, max: usize) -> Vec<TrayEntry> {
    projects
        .entries
        .iter()
        .take(max)
        .map(|entry| TrayEntry {
            id: entry.id.clone(),
            label: format!(
                "{} {} · {}",
                kind_emoji(entry.kind),
                entry.name,
                t("app", entry.kind.tray_status_key())
            ),
        })
        .collect()
}

/// tooltip:三个 **pane 级**计数(`ai-idle` 不计入),0 的那档整条不出现,
/// 用 ` · ` 连接(`store.ts:339-348`)。
pub fn tooltip(attention: usize, working: usize, done: usize) -> String {
    let mut parts: Vec<String> = Vec::new();
    if attention > 0 {
        parts.push(tr!("app", "trayAttention", count = attention));
    }
    if working > 0 {
        parts.push(tr!("app", "trayWorking", count = working));
    }
    if done > 0 {
        parts.push(tr!("app", "trayDone", count = done));
    }
    parts.join(" · ")
}

/// 把一份 [`AiProjects`](crate::store::AiProjects) 聚合结果压成推送快照。
///
/// `max_projects` 是 `config.trayMaxProjects ?? 5`(UI 限幅 1..20,这里不再钳 ——
/// 手改配置成 0 就是「菜单空着」,与 TS 的 `slice(0, 0)` 同结果)。
pub fn build_snapshot(
    enabled: bool,
    focused: bool,
    projects: &AiProjects,
    max_projects: usize,
) -> TraySnapshot {
    let entries = menu_entries(projects, max_projects);
    let signature = format!(
        "{}|{}|{}|{}|{}|{}",
        enabled,
        focused,
        projects.attention,
        projects.working,
        projects.done,
        entries
            .iter()
            .map(|e| e.label.as_str())
            .collect::<Vec<_>>()
            .join(",")
    );
    TraySnapshot {
        enabled,
        focused,
        lamps: Lamps {
            attention: projects.attention > 0,
            working: projects.working > 0,
            done: projects.done > 0,
        },
        tooltip: tooltip(projects.attention, projects.working, projects.done),
        projects: entries,
        signature,
    }
}

// ─── 纯函数:灯色与画帧 ──────────────────────────────────────

/// 当前活跃的颜色集合(顺序固定 黄→蓝→绿;灰不在集合里,空 = 灰)。
#[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
fn active_colors(lamps: Lamps) -> Vec<[u8; 3]> {
    let mut colors = Vec::new();
    if lamps.attention {
        colors.push(YELLOW);
    }
    if lamps.working {
        colors.push(BLUE);
    }
    if lamps.done {
        colors.push(GREEN);
    }
    colors
}

/// 本帧该显示的颜色与明暗。
///
/// 静止(聚焦 / 已定格 / 安静)= 最高优先级色全亮;
/// 失焦单状态 = 亮暗呼吸;失焦多状态 = 颜色轮转(全亮)。
#[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
fn frame_color(colors: &[[u8; 3]], frame: usize, blinking: bool) -> ([u8; 3], bool) {
    match colors.len() {
        0 => (GRAY, false),
        1 => (colors[0], blinking && frame % 2 == 1),
        n => {
            if blinking {
                (colors[frame % n], false)
            } else {
                (colors[0], false) // 静止时停在最高优先级色
            }
        }
    }
}

/// 点到线段的最短距离(笔画抗锯齿用的距离场)。
#[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
fn dist_to_segment(px: f32, py: f32, ax: f32, ay: f32, bx: f32, by: f32) -> f32 {
    let (dx, dy) = (bx - ax, by - ay);
    let len_sq = dx * dx + dy * dy;
    // 退化成一个点时按点距算,不让除法炸掉
    let t = if len_sq <= f32::EPSILON {
        0.0
    } else {
        (((px - ax) * dx + (py - ay) * dy) / len_sq).clamp(0.0, 1.0)
    };
    let (nx, ny) = (ax + t * dx, ay + t * dy);
    ((px - nx).powi(2) + (py - ny).powi(2)).sqrt()
}

/// 点到圆角矩形**描边**的距离(0 = 正落在描边中心线上)。
#[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
fn dist_to_round_rect(px: f32, py: f32, cx: f32, cy: f32, half: f32, radius: f32) -> f32 {
    // 标准的圆角矩形 SDF:先折到第一象限,再按「内缩半径的矩形 + 圆角」算
    let dx = (px - cx).abs() - (half - radius);
    let dy = (py - cy).abs() - (half - radius);
    let outside = (dx.max(0.0).powi(2) + dy.max(0.0).powi(2)).sqrt();
    let inside = dx.max(dy).min(0.0);
    // 取绝对值 = 到**边线**的距离(不分内外),描边就是绕这条线加粗
    (outside + inside - radius).abs()
}

/// 画状态灯:一个圆角方框 + 框内的 `>` 提示符。
///
/// # 为什么是这个形状
///
/// 原先画的是纯色圆点。安静态(灰)时,菜单栏里就是一个突兀的灰疙瘩 —— 旁边一排
/// 应用图标都有轮廓,只有它像个坏掉的指示灯。改画应用图标的主视觉:一个终端
/// 窗口的方框,里面是那个 `>` 提示符(`docs/icon.png` 里的橙色符号)。
///
/// # 颜色分工
///
/// - **外框**是"这个应用在这儿"的常驻标识,用中性色(`frame`)。macOS 侧按菜单栏
///   明暗自适应白/黑 —— 那边的惯例就是单色描边图标;Win32 侧用中性灰,深浅两种
///   任务栏都看得见。
/// - **`>`** 承载状态语义(`chevron`:黄/蓝/绿/灰),与主窗口 StatusDot 按语义对齐。
///   `chevron_alpha` 同时兼顾暗帧([`DIM`])与安静态淡出([`Blink::idle_alpha`])。
///
/// `frame` 传 `None` 就只画 `>`(留给不需要外框的调用方)。
///
/// 按到图形的距离做 1px 软边抗锯齿;返回 **RGBA**(Win32 那一层再转预乘 BGRA)。
/// 几何全按 `size` 取比例 —— Win32 画布跟着 `SM_CXSMICON` 走(16px@100%),
/// macOS 是固定的 44px(22pt@2x),同一份代码要在两边都画得出来。
#[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
fn compose_frame_rgba(
    size: u32,
    chevron: [u8; 3],
    chevron_alpha: f32,
    frame: Option<[u8; 3]>,
) -> Vec<u8> {
    let mut rgba = vec![0u8; (size * size * 4) as usize];
    let s = size as f32;
    let center = s / 2.0;
    let frame_half = center - s * FRAME_INSET;
    let frame_radius = s * FRAME_RADIUS;
    let frame_stroke = s * FRAME_STROKE;
    let chev_stroke = s * STROKE_RATIO;
    // `>` 的三个折点:左上 → 右中(尖端)→ 左下,整体在框内居中
    let (ax, ay) = (s * 0.33, s * 0.28);
    let (bx, by) = (s * 0.66, s * 0.50);
    let (cx, cy) = (s * 0.33, s * 0.72);

    for y in 0..size {
        for x in 0..size {
            // 采样点取像素中心
            let (px, py) = (x as f32 + 0.5, y as f32 + 0.5);

            let a_frame = match frame {
                Some(_) => {
                    let d = dist_to_round_rect(px, py, center, center, frame_half, frame_radius);
                    (frame_stroke + 0.5 - d).clamp(0.0, 1.0)
                }
                None => 0.0,
            };

            let d_chev = dist_to_segment(px, py, ax, ay, bx, by)
                .min(dist_to_segment(px, py, bx, by, cx, cy));
            let a_chev = (chev_stroke + 0.5 - d_chev).clamp(0.0, 1.0) * chevron_alpha;

            // 两者不重叠(`>` 在框内),真撞上时谁实谁赢
            let (color, alpha) = if a_chev >= a_frame {
                (chevron, a_chev)
            } else {
                (frame.unwrap_or(chevron), a_frame)
            };
            let alpha = (alpha * 255.0) as u8;
            if alpha > 0 {
                let idx = ((y * size + x) * 4) as usize;
                rgba[idx] = color[0];
                rgba[idx + 1] = color[1];
                rgba[idx + 2] = color[2];
                rgba[idx + 3] = alpha;
            }
        }
    }
    rgba
}

/// 闪烁相位。装机版把它散在 `TrayLightState` 的两个字段 + 线程循环里,
/// 这里收成一个可单测的小状态机(`tray.rs:224-242` 的同一条判据链)。
#[derive(Default)]
#[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
struct Blink {
    frame: usize,
    /// 单状态短促闪烁结束后已定格全亮,不再重绘。
    settled: bool,
}

#[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
impl Blink {
    /// 灯色/焦点变化 → 新状态从「全亮」帧开始,并重新允许短促闪烁。
    fn reset(&mut self) {
        self.frame = 0;
        self.settled = false;
    }

    /// 走一帧。返回**是否需要重绘**。
    ///
    /// `colors` = 活跃颜色数。三种走法:
    /// - **安静(colors == 0)**:推进 `>` 的淡出相位。**与焦点无关** —— 没事发生
    ///   就该安静下去,不管人有没有在看着窗口(闪烁才是「要你注意」,这个不是);
    /// - 聚焦 / 已定格:不动;
    /// - 其余:原来的闪烁相位。
    fn tick(&mut self, colors: usize, enabled: bool, focused: bool) -> bool {
        if !enabled || self.settled {
            return false;
        }
        if colors == 0 {
            self.frame = self.frame.wrapping_add(1);
            if self.frame >= IDLE_HOLD_FRAMES + IDLE_FADE_FRAMES {
                // 淡完了就定格(只剩外框),别再每 600ms 白画一帧
                self.settled = true;
            }
            return true;
        }
        if focused {
            return false;
        }
        if colors == 1 && self.frame >= BURST_FRAMES {
            // 短促闪烁结束:补一帧全亮定格,之后跳过
            self.settled = true;
        } else {
            self.frame = self.frame.wrapping_add(1);
        }
        true
    }

    /// 安静态下 `>` 的不透明度:前 [`IDLE_HOLD_FRAMES`] 帧全亮,之后线性淡到 0。
    ///
    /// 只在 `colors == 0` 时有意义 —— 有状态时 `>` 一律全亮(明暗由暗帧管)。
    fn idle_alpha(&self) -> f32 {
        if self.frame <= IDLE_HOLD_FRAMES {
            return 1.0;
        }
        let faded = (self.frame - IDLE_HOLD_FRAMES) as f32 / IDLE_FADE_FRAMES as f32;
        (1.0 - faded).clamp(0.0, 1.0)
    }

    /// 现在处于「该闪」的状态吗(聚焦或已定格都算静止)。
    fn blinking(&self, focused: bool) -> bool {
        !focused && !self.settled
    }
}

// ─── 对外句柄 ────────────────────────────────────────────────

/// 托盘句柄。**主线程持有**,drop 时把图标摘掉(Windows 侧还要收掉托盘线程,
/// macOS 侧则是停掉闪烁定时器 —— 见各自的 `platform` 模块)。
pub struct Tray {
    handle: Option<platform::TrayHandle>,
    /// 上一次真正推下去的签名(去重,`store.ts` 的 `lastTraySig`)。
    last_signature: String,
}

impl Tray {
    /// 建托盘。返回句柄 + 交互事件的接收端。
    ///
    /// 建不起来(Linux / Windows 取不到 HWND 或注册窗口失败 / macOS 不在主线程)时
    /// 句柄是空壳、接收端立刻结束 —— 与装机版「初始化失败只 eprintln 不中断启动」
    /// 同语义。**要什么由各平台自己取**:Windows 要主窗口 HWND(托盘消息得有窗口
    /// 收),macOS 的 `NSStatusItem` 挂在应用上、只需要主线程标记。
    pub fn start(window: &gpui::Window) -> (Self, UnboundedReceiver<TrayEvent>) {
        let (tx, rx) = mpsc::unbounded();
        let handle = platform::start(window, tx);
        if handle.is_none() {
            eprintln!("[tray] 托盘未启用(平台不支持或初始化失败)");
        }
        (
            Self {
                handle,
                last_signature: String::new(),
            },
            rx,
        )
    }

    /// 推一份快照。签名相同直接丢弃 —— store 的每一次 notify 都会走到这里,
    /// 不去重的话光是移动鼠标就会疯狂重建图标。
    pub fn push(&mut self, snapshot: TraySnapshot) {
        let Some(handle) = self.handle.as_ref() else {
            return;
        };
        if snapshot.signature == self.last_signature {
            return;
        }
        self.last_signature = snapshot.signature.clone();
        handle.push(snapshot);
    }
}

/// 主窗口的 HWND(给托盘线程唤窗用)。
///
/// **必须显式走 `HasWindowHandle` trait**:gpui 的 `Window` 上有一个同名的固有
/// 方法(返回 `AnyWindowHandle`),写成 `window.window_handle()` 会静默拿错东西
/// —— 与 `notify::flash_taskbar` 同一个坑,同一条注释。
#[cfg(windows)]
fn main_window_handle(window: &gpui::Window) -> Option<isize> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};

    let handle = HasWindowHandle::window_handle(window).ok()?;
    let RawWindowHandle::Win32(win32) = handle.as_raw() else {
        return None;
    };
    Some(win32.hwnd.get())
}

// ─── 平台实现 ────────────────────────────────────────────────

/// 非 Windows / 非 macOS(即 Linux):空实现。
/// StatusNotifierItem 是另一套 API,接口留在这里,补的时候只换这个模块。
#[cfg(not(any(windows, target_os = "macos")))]
mod platform {
    use super::{TrayEvent, TraySnapshot};
    use futures::channel::mpsc::UnboundedSender;

    pub struct TrayHandle;

    pub fn start(
        _window: &gpui::Window,
        _events: UnboundedSender<TrayEvent>,
    ) -> Option<TrayHandle> {
        None
    }

    impl TrayHandle {
        pub fn push(&self, _snapshot: TraySnapshot) {}
    }
}

#[cfg(target_os = "macos")]
mod platform {
    //! `NSStatusItem` 直写。
    //!
    //! # 与 Win32 版的结构差异:**没有托盘线程**
    //!
    //! Win32 那边必须另起线程 + 一个隐藏窗口,因为托盘回调消息只能送到窗口,而主
    //! 窗口的 WndProc 归 gpui 管。AppKit 这边正好反过来:`NSStatusItem` / `NSMenu`
    //! **必须在主线程操作**,而 [`super::Tray::push`] 本来就在 GPUI 主线程上被调用
    //! —— 于是整条链路同线程,不需要 channel + `PostMessage`,闪烁也改由挂在主
    //! runloop 上的 `NSTimer` 驱动(Win32 那边是托盘线程里的 `SetTimer`)。
    //!
    //! # 点击语义按 macOS 惯例走
    //!
    //! Win32 版是「左键唤窗、右键弹菜单」;macOS 的状态栏项左右键都该弹菜单,于是
    //! 「唤起窗口」收进菜单首项(`trayOpen`),点它才发 [`TrayEvent::Clicked`]。
    //! 事件语义本身不变 —— 发之前先激活应用,与 Win32 版注释里「窗口**已经被唤起**,
    //! 这里只负责 `trayClickFocus` 门控下的跳转」对齐。

    use std::cell::RefCell;
    use std::ptr::NonNull;
    use std::rc::Rc;

    use block2::RcBlock;
    use futures::channel::mpsc::UnboundedSender;
    use objc2::rc::Retained;
    use objc2::runtime::AnyObject;
    use objc2::{
        AllocAnyThread, DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel,
    };
    use objc2_app_kit::{
        NSAppearanceCustomization, NSAppearanceNameAqua, NSAppearanceNameDarkAqua, NSApplication,
        NSBitmapImageRep, NSDeviceRGBColorSpace, NSImage, NSMenu, NSMenuItem, NSStatusBar,
        NSStatusBarButton, NSStatusItem, NSVariableStatusItemLength,
    };
    use objc2_foundation::{NSArray, NSObject, NSObjectProtocol, NSSize, NSString, NSTimer};

    use super::{
        BLINK_MS, Blink, DIM, FRAME_DARK, FRAME_LIGHT, TrayEvent, TraySnapshot, active_colors,
        compose_frame_rgba, frame_color,
    };
    use crate::i18n::t;

    /// 图标边长(**物理**像素)。菜单栏 22pt 高,Retina 是 2x —— 画 44px 再按 22pt
    /// 逻辑尺寸贴上去,非 Retina 屏由系统降采样。与 Win32 侧按 `SM_CXSMICON` 取尺寸
    /// 同理,只是这边的「小图标」尺寸是平台固定值。
    const ICON_PX: u32 = 44;
    /// 贴图用的逻辑尺寸(pt)。
    const ICON_PT: f64 = 22.0;

    // ─── RGBA → NSImage ─────────────────────────────────────

    /// 把 [`compose_frame_rgba`] 出的缓冲变成 `NSImage`。
    ///
    /// **不设 template**:模板图会被系统按菜单栏明暗重新着色,而这三盏灯的颜色
    /// 本身就是语义(黄/蓝/绿),被染成单色就全废了。
    ///
    /// ⚠️ **planes 必须传 NULL**,让 `NSBitmapImageRep` 自己分配像素缓冲,再把帧
    /// 拷进去。传我们自己的指针的话它**不拷贝、只引用**(Apple 文档:
    /// 「If planes is not NULL … the receiver doesn't copy the data」)—— 而入参
    /// 那块缓冲是调用方的局部 `Vec`,函数一返回就没了。踩过一次:菜单栏上画出
    /// 一片乱码条纹,那是已释放内存被复用后的残留。
    fn image_from_rgba(rgba: &[u8], size: u32) -> Option<Retained<NSImage>> {
        // SAFETY: planes 传 NULL 走「rep 自己分配」那条路;宽高/行距/位深与
        // `compose_frame_rgba` 的输出格式(RGBA8,4 字节/像素)逐项对应。
        let rep = unsafe {
            NSBitmapImageRep::initWithBitmapDataPlanes_pixelsWide_pixelsHigh_bitsPerSample_samplesPerPixel_hasAlpha_isPlanar_colorSpaceName_bytesPerRow_bitsPerPixel(
                NSBitmapImageRep::alloc(),
                std::ptr::null_mut(),
                size as isize,
                size as isize,
                8,
                4,
                true,
                false,
                NSDeviceRGBColorSpace,
                (size * 4) as isize,
                32,
            )
        }?;
        let dst = rep.bitmapData();
        if dst.is_null() {
            return None;
        }
        // 取 min 兜住:rep 按 bytesPerRow×height 分配,与入参同长,
        // 真要对不上也宁可少拷一点,不越界
        let len = rgba.len().min((size as usize * 4) * size as usize);
        // SAFETY: dst 是 rep 自己那块至少 len 字节的缓冲,两块内存不重叠
        unsafe { std::ptr::copy_nonoverlapping(rgba.as_ptr(), dst, len) };

        let image = NSImage::initWithSize(NSImage::alloc(), NSSize::new(ICON_PT, ICON_PT));
        image.addRepresentation(&rep);
        Some(image)
    }

    // ─── target-action 载体 ─────────────────────────────────

    struct TargetIvars {
        events: UnboundedSender<TrayEvent>,
        /// 菜单项 tag → 项目 id。菜单每次重建,tag 就是当次的下标。
        ids: RefCell<Vec<String>>,
    }

    define_class!(
        #[unsafe(super(NSObject))]
        #[thread_kind = MainThreadOnly]
        // 类名进的是 ObjC 运行时的**全局**命名空间,加前缀避免撞名
        #[name = "MiniTermTrayTarget"]
        #[ivars = TargetIvars]
        struct TrayTarget;

        impl TrayTarget {
            /// 菜单首项「打开 mini-term」。
            #[unsafe(method(miniTermTrayOpen:))]
            fn on_open(&self, _sender: &AnyObject) {
                activate_app();
                let _ = self.ivars().events.unbounded_send(TrayEvent::Clicked);
            }

            /// 项目条目。id 按 tag 回查,查不到(菜单与快照赛跑)就什么都不做。
            #[unsafe(method(miniTermTrayItem:))]
            fn on_item(&self, sender: &AnyObject) {
                // SAFETY: sender 是发起本次 action 的 NSMenuItem,`tag` 是它的固有属性
                let tag: isize = unsafe { msg_send![sender, tag] };
                let Ok(index) = usize::try_from(tag) else {
                    return;
                };
                let Some(id) = self.ivars().ids.borrow().get(index).cloned() else {
                    return;
                };
                activate_app();
                let _ = self
                    .ivars()
                    .events
                    .unbounded_send(TrayEvent::ProjectClicked(id));
            }
        }

        unsafe impl NSObjectProtocol for TrayTarget {}
    );

    impl TrayTarget {
        fn new(mtm: MainThreadMarker, events: UnboundedSender<TrayEvent>) -> Retained<Self> {
            let this = Self::alloc(mtm).set_ivars(TargetIvars {
                events,
                ids: RefCell::new(Vec::new()),
            });
            // SAFETY: 标准的 `[[Self alloc] init]`,父类是 NSObject
            unsafe { msg_send![super(this), init] }
        }
    }

    /// 把应用切到前台。两条 action 共用 —— 事件送到主线程时窗口就该已经在前面了。
    fn activate_app() {
        let Some(mtm) = MainThreadMarker::new() else {
            return;
        };
        let app = NSApplication::sharedApplication(mtm);
        // 新的 `activate()` 要 macOS 14+,而 `Info.plist` 的 LSMinimumSystemVersion
        // 是 **11.0** —— 在 11~13 上调它是 unrecognized selector,直接崩。这个老
        // 接口虽被标记「将来会废弃」,却是覆盖得住部署目标的那一个。
        #[allow(deprecated)]
        app.activateIgnoringOtherApps(true);
    }

    // ─── 状态 ───────────────────────────────────────────────

    struct State {
        item: Retained<NSStatusItem>,
        target: Retained<TrayTarget>,
        snapshot: TraySnapshot,
        blink: Blink,
    }

    /// 菜单栏当前是深色吗 —— 决定外框画白还是画黑。
    ///
    /// 问的是**按钮自己**的 `effectiveAppearance` 而不是 `NSApp` 的:图标就挂在
    /// 菜单栏里,那里的明暗才是它要融进去的背景。
    ///
    /// macOS 那些看着「都是白的」的菜单栏图标,本体是**模板图**(系统按背景自动
    /// 反色)。这里不能整图模板化 —— 那会把承载状态语义的彩色 `>` 一起染成单色,
    /// 于是只好自己判一次明暗,只给外框用。
    fn menu_bar_is_dark(button: &NSStatusBarButton) -> bool {
        let appearance = button.effectiveAppearance();
        // SAFETY: 两个都是 AppKit 的全局外观名常量
        let names = NSArray::from_slice(&[unsafe { NSAppearanceNameAqua }, unsafe {
            NSAppearanceNameDarkAqua
        }]);
        match appearance.bestMatchFromAppearancesWithNames(&names) {
            // SAFETY: 同上
            Some(name) => &*name == unsafe { NSAppearanceNameDarkAqua },
            // 匹配不出来按深色处理:菜单栏深色是更常见的那一种
            None => true,
        }
    }

    /// 按当前快照与闪烁相位重画图标。
    fn redraw(st: &mut State, mtm: MainThreadMarker) {
        let Some(button) = st.item.button(mtm) else {
            return;
        };
        let colors = active_colors(st.snapshot.lamps);
        let (color, dim) = frame_color(
            &colors,
            st.blink.frame,
            st.blink.blinking(st.snapshot.focused),
        );
        let frame = if menu_bar_is_dark(&button) {
            FRAME_LIGHT
        } else {
            FRAME_DARK
        };
        // 安静态:`>` **与外框同色**再逐帧淡出 —— [`frame_color`] 给的那个系统灰
        // (`#8E8E93`)在菜单栏上几乎看不出来,淡出前那几秒等于一个空框。
        // 有状态时才用状态色,暗帧压到 DIM。
        let (chevron, chevron_alpha) = if colors.is_empty() {
            (frame, st.blink.idle_alpha())
        } else if dim {
            (color, DIM)
        } else {
            (color, 1.0)
        };
        let rgba = compose_frame_rgba(ICON_PX, chevron, chevron_alpha, Some(frame));
        if let Some(image) = image_from_rgba(&rgba, ICON_PX) {
            button.setImage(Some(&image));
        }
    }

    /// 重建菜单。首项固定是「打开」,其后才是项目列表。
    fn rebuild_menu(st: &mut State, mtm: MainThreadMarker) {
        let menu = NSMenu::new(mtm);

        let open = NSMenuItem::new(mtm);
        // SAFETY: 都是 NSMenuItem 的固有属性;target 是本模块自己的 TrayTarget,
        // 它被 State 强引用,活得比菜单久
        unsafe {
            open.setTitle(&NSString::from_str(&t("app", "trayOpen")));
            open.setTarget(Some(&*st.target));
            open.setAction(Some(sel!(miniTermTrayOpen:)));
        }
        menu.addItem(&open);

        let ids: Vec<String> = st.snapshot.projects.iter().map(|e| e.id.clone()).collect();
        if !ids.is_empty() {
            menu.addItem(&NSMenuItem::separatorItem(mtm));
        }
        for (index, entry) in st.snapshot.projects.iter().enumerate() {
            let mi = NSMenuItem::new(mtm);
            // SAFETY: 同上;tag 存下标,action 里按它回查 id
            unsafe {
                mi.setTitle(&NSString::from_str(&entry.label));
                mi.setTarget(Some(&*st.target));
                mi.setAction(Some(sel!(miniTermTrayItem:)));
                mi.setTag(index as isize);
            }
            menu.addItem(&mi);
        }
        *st.target.ivars().ids.borrow_mut() = ids;
        st.item.setMenu(Some(&menu));
    }

    /// 闪烁一帧。安静 / 聚焦 / 已定格 / 开关关掉时 [`Blink::tick`] 自己会拒绝推帧。
    fn tick(state: &Rc<RefCell<State>>) {
        let Some(mtm) = MainThreadMarker::new() else {
            return;
        };
        let mut st = state.borrow_mut();
        let colors = active_colors(st.snapshot.lamps).len();
        let (enabled, focused) = (st.snapshot.enabled, st.snapshot.focused);
        if !st.blink.tick(colors, enabled, focused) {
            return;
        }
        redraw(&mut st, mtm);
    }

    // ─── 对外 ───────────────────────────────────────────────

    pub struct TrayHandle {
        state: Rc<RefCell<State>>,
        timer: Retained<NSTimer>,
    }

    pub fn start(_window: &gpui::Window, events: UnboundedSender<TrayEvent>) -> Option<TrayHandle> {
        let mtm = MainThreadMarker::new()?;
        let bar = NSStatusBar::systemStatusBar();
        let item = bar.statusItemWithLength(NSVariableStatusItemLength);
        let target = TrayTarget::new(mtm, events);

        let state = Rc::new(RefCell::new(State {
            item,
            target,
            snapshot: TraySnapshot::default(),
            blink: Blink::default(),
        }));

        // 定时器拿 **Weak**:scheduled 之后 runloop 一直持有 block,强引用会让 State
        // 永远活着(Drop 里虽然 invalidate,但那要求 handle 先被 drop —— 循环成立时
        // 它永远不会被 drop)。
        let weak = Rc::downgrade(&state);
        let block = RcBlock::new(move |_timer: NonNull<NSTimer>| {
            if let Some(state) = weak.upgrade() {
                tick(&state);
            }
        });
        // SAFETY: block 签名与 NSTimer 期望的 `^(NSTimer *)` 一致
        let timer = unsafe {
            NSTimer::scheduledTimerWithTimeInterval_repeats_block(
                f64::from(BLINK_MS) / 1000.0,
                true,
                &block,
            )
        };

        Some(TrayHandle { state, timer })
    }

    impl TrayHandle {
        pub fn push(&self, snapshot: TraySnapshot) {
            let Some(mtm) = MainThreadMarker::new() else {
                return;
            };
            let mut st = self.state.borrow_mut();

            // 灯色/焦点变了就重开一轮闪烁(与 Win32 侧 WM_TRAY_SYNC 的处置同源)
            if st.snapshot.lamps != snapshot.lamps || st.snapshot.focused != snapshot.focused {
                st.blink.reset();
            }
            st.snapshot = snapshot;

            st.item.setVisible(st.snapshot.enabled);
            if !st.snapshot.enabled {
                return;
            }

            rebuild_menu(&mut st, mtm);
            if let Some(button) = st.item.button(mtm) {
                // 先绑住 `Retained`:直接把 `NSString::from_str(..)` 的结果借给
                // `setToolTip` 的话,那个临时值在调用完成前就被释放了。
                // 空串按「不设 tooltip」处理,与 [`TraySnapshot::tooltip`] 的文档一致。
                let tip = (!st.snapshot.tooltip.is_empty())
                    .then(|| NSString::from_str(&st.snapshot.tooltip));
                button.setToolTip(tip.as_deref());
            }
            redraw(&mut st, mtm);
        }
    }

    impl Drop for TrayHandle {
        fn drop(&mut self) {
            // 顺序要紧:先停表再摘图标 —— 反过来的话中间那一拍可能画到一个
            // 已经从状态栏摘掉的 item 上
            self.timer.invalidate();
            if MainThreadMarker::new().is_some() {
                let bar = NSStatusBar::systemStatusBar();
                bar.removeStatusItem(&self.state.borrow().item);
            }
        }
    }
}

#[cfg(windows)]
mod platform {
    //! Win32 直写 `Shell_NotifyIconW`。
    //!
    //! 托盘图标的回调消息只能送到**窗口**,而 gpui 的主窗口 WndProc 归 gpui 管
    //! (子类化进去等于往它的消息处理里插一脚)。这里另起一个线程 + 一个不可见的
    //! 顶层窗口专门收托盘消息:与主线程零共享,只靠 channel + `PostMessage` 通信。
    //!
    //! **为什么不是 message-only 窗口(`HWND_MESSAGE`)**:它收不到广播,而
    //! explorer.exe 重启后重新登记图标靠的正是 `TaskbarCreated` 广播消息。
    //! 于是用一个零尺寸、never-shown、带 `WS_EX_TOOLWINDOW` 的顶层窗口
    //! (不会出现在 Alt+Tab / 任务栏里)。

    use std::ffi::c_void;
    use std::sync::mpsc::{Receiver, Sender, channel};
    use std::thread::JoinHandle;
    use std::time::Duration;

    use futures::channel::mpsc::UnboundedSender;
    use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, WPARAM};
    use windows::Win32::Graphics::Gdi::{
        BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateBitmap, CreateDIBSection, DIB_RGB_COLORS,
        DeleteObject, HBITMAP,
    };
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::Shell::{
        NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW,
        Shell_NotifyIconW,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        AppendMenuW, CreateIconIndirect, CreatePopupMenu, CreateWindowExW, DefWindowProcW,
        DestroyIcon, DestroyMenu, DestroyWindow, DispatchMessageW, GWLP_USERDATA, GetCursorPos,
        GetMessageW, GetSystemMetrics, GetWindowLongPtrW, HICON, ICONINFO, IsIconic, KillTimer,
        MF_STRING, MSG, PostMessageW, PostQuitMessage, RegisterClassW, RegisterWindowMessageW,
        SM_CXSMICON, SW_RESTORE, SW_SHOW, SetForegroundWindow, SetTimer, SetWindowLongPtrW,
        ShowWindow, TPM_NONOTIFY, TPM_RETURNCMD, TPM_RIGHTBUTTON, TrackPopupMenu, TranslateMessage,
        WM_APP, WM_DESTROY, WM_LBUTTONUP, WM_NULL, WM_RBUTTONUP, WM_TIMER, WNDCLASSW,
        WS_EX_TOOLWINDOW, WS_OVERLAPPED,
    };
    use windows::core::{PCWSTR, w};

    use super::{
        BLINK_MS, Blink, DIM, FRAME_NEUTRAL, Lamps, TrayEntry, TrayEvent, TraySnapshot,
        active_colors, compose_frame_rgba, frame_color,
    };

    /// 托盘图标的回调消息(shell → 我们的窗口)。
    const WM_TRAY_CALLBACK: u32 = WM_APP + 1;
    /// 主线程往 channel 里塞了新快照的叫醒信号。
    const WM_TRAY_SYNC: u32 = WM_APP + 2;
    /// `NOTIFYICONDATAW::uID`(同一个 hWnd 下唯一即可)。
    const TRAY_UID: u32 = 1;
    /// 闪烁定时器 id。
    const TIMER_ID: usize = 1;
    /// 菜单项命令 id 的起点(`TrackPopupMenu` 返回 0 表示「没选」,不能从 0 开始)。
    const MENU_ID_BASE: usize = 1000;
    /// 取不到 `SM_CXSMICON` 时的兜底边长。
    const FALLBACK_ICON_SIZE: i32 = 16;

    enum Command {
        Sync(Box<TraySnapshot>),
        Quit,
    }

    /// HICON 的 RAII 包装 —— **换图标必须销毁旧句柄**。
    ///
    /// 装机版靠 tray-icon crate 代管,这里自己写就得自己管:600ms 一帧的闪烁下
    /// 漏一个句柄就是每分钟漏 100 个 GDI 对象,几小时就能顶到进程 10000 上限。
    struct OwnedIcon(HICON);

    impl Drop for OwnedIcon {
        fn drop(&mut self) {
            // SAFETY: 句柄由 CreateIconIndirect 造出,本结构独占,只销毁一次
            unsafe {
                let _ = DestroyIcon(self.0);
            }
        }
    }

    /// HBITMAP 的 RAII 包装(造 HICON 的中间产物,`CreateIconIndirect` 会拷贝它们)。
    struct OwnedBitmap(HBITMAP);

    impl Drop for OwnedBitmap {
        fn drop(&mut self) {
            // SAFETY: 句柄由 CreateDIBSection / CreateBitmap 造出,本结构独占
            unsafe {
                let _ = DeleteObject(self.0.into());
            }
        }
    }

    /// 主线程握着的那一端。
    pub struct TrayHandle {
        tx: Sender<Command>,
        /// 托盘线程那个隐藏窗口的 HWND(`isize` 是为了 `Send`)。
        hwnd: isize,
        thread: Option<JoinHandle<()>>,
    }

    impl TrayHandle {
        pub fn push(&self, snapshot: TraySnapshot) {
            if self.tx.send(Command::Sync(Box::new(snapshot))).is_ok() {
                self.wake();
            }
        }

        fn wake(&self) {
            // SAFETY: PostMessageW 跨线程投递是设计用法;窗口已死时返回错误,忽略
            unsafe {
                let _ = PostMessageW(
                    Some(HWND(self.hwnd as *mut c_void)),
                    WM_TRAY_SYNC,
                    WPARAM(0),
                    LPARAM(0),
                );
            }
        }
    }

    impl Drop for TrayHandle {
        fn drop(&mut self) {
            let _ = self.tx.send(Command::Quit);
            self.wake();
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    pub fn start(window: &gpui::Window, events: UnboundedSender<TrayEvent>) -> Option<TrayHandle> {
        let main_hwnd = super::main_window_handle(window)?;
        let (tx, rx) = channel::<Command>();
        let (ready_tx, ready_rx) = channel::<isize>();
        let thread = std::thread::Builder::new()
            .name("mt-tray".into())
            .spawn(move || run(main_hwnd, rx, events, ready_tx))
            .ok()?;
        // 建窗口是同步的本地调用,几毫秒就回来;给 5s 只是别在异常环境下吊死
        let hwnd = ready_rx.recv_timeout(Duration::from_secs(5)).ok()?;
        if hwnd == 0 {
            let _ = thread.join();
            return None;
        }
        Some(TrayHandle {
            tx,
            hwnd,
            thread: Some(thread),
        })
    }

    /// 托盘线程主体:建窗口 → 起定时器 → 跑消息循环。
    fn run(
        main_hwnd: isize,
        rx: Receiver<Command>,
        events: UnboundedSender<TrayEvent>,
        ready: Sender<isize>,
    ) {
        let Some(hwnd) = create_window() else {
            let _ = ready.send(0);
            return;
        };

        // SAFETY: taskbar_created 只是注册一个消息号,失败返回 0(下面显式跳过 0)
        let taskbar_created = unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) };
        // SAFETY: 取系统度量,无副作用
        let icon_size = unsafe { GetSystemMetrics(SM_CXSMICON) };
        let icon_size = if icon_size > 0 {
            icon_size
        } else {
            FALLBACK_ICON_SIZE
        };

        let state = Box::new(TrayThread {
            rx,
            events,
            main_hwnd: HWND(main_hwnd as *mut c_void),
            taskbar_created,
            icon_size,
            icon: None,
            added: false,
            // 装机版的初值:启动即认为聚焦(主窗口马上显示并获焦),避免开局按
            // 失焦语义闪烁;首次推送会带来真实值
            enabled: true,
            focused: true,
            lamps: Lamps::default(),
            blink: Blink::default(),
            tooltip: String::new(),
            projects: Vec::new(),
            menu_open: false,
            quit_pending: false,
        });
        // SAFETY: 指针存进本窗口的 USERDATA,只有本线程的 wndproc 会取用;
        // WM_DESTROY 里取回并 Box::from_raw 释放
        unsafe {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, Box::into_raw(state) as isize);
            let _ = SetTimer(Some(hwnd), TIMER_ID, BLINK_MS, None);
        }

        if ready.send(hwnd.0 as isize).is_err() {
            // 主线程等超时后已经放弃了这个托盘 —— 别留一个没人能叫停的窗口
            // 和定时器在这儿转(DestroyWindow 会同步走一遍 WM_DESTROY 收摊)。
            // SAFETY: 窗口刚由本线程创建
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
            return;
        }

        let mut msg = MSG::default();
        // SAFETY: 标准消息循环;GetMessageW 返回 -1 是错误,`> 0` 一并挡掉
        unsafe {
            while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }

    fn create_window() -> Option<HWND> {
        const CLASS_NAME: PCWSTR = w!("MiniTermTrayWindow");
        // SAFETY: 全是标准窗口创建调用;类重复注册返回 0,不视为错误
        unsafe {
            let hinstance: HINSTANCE = GetModuleHandleW(None).ok()?.into();
            let class = WNDCLASSW {
                lpfnWndProc: Some(wndproc),
                hInstance: hinstance,
                lpszClassName: CLASS_NAME,
                ..Default::default()
            };
            RegisterClassW(&class);
            CreateWindowExW(
                WS_EX_TOOLWINDOW,
                CLASS_NAME,
                w!("mini-term tray"),
                WS_OVERLAPPED,
                0,
                0,
                0,
                0,
                None,
                None,
                Some(hinstance),
                None,
            )
            .ok()
        }
    }

    /// 托盘线程的全部状态。**只在托盘线程上碰**。
    struct TrayThread {
        rx: Receiver<Command>,
        events: UnboundedSender<TrayEvent>,
        main_hwnd: HWND,
        /// explorer 重启广播的消息号;0 = 注册失败(那就不做重登记)。
        taskbar_created: u32,
        icon_size: i32,
        /// 当前挂在托盘上的图标。换图时**先 NIM_MODIFY 再 drop 旧的**。
        icon: Option<OwnedIcon>,
        /// 图标是否已经登记进 shell。
        added: bool,
        enabled: bool,
        focused: bool,
        lamps: Lamps,
        blink: Blink,
        tooltip: String,
        projects: Vec<TrayEntry>,
        /// `TrackPopupMenu` 的模态循环正在栈上跑。
        menu_open: bool,
        /// 收到过 [`Command::Quit`],还没兑现(菜单开着时要等它收了)。
        quit_pending: bool,
    }

    impl TrayThread {
        /// 取空 channel 里的全部命令。返回 true = **现在**可以拆窗口了。
        fn drain(&mut self, hwnd: HWND) -> bool {
            while let Ok(cmd) = self.rx.try_recv() {
                match cmd {
                    Command::Sync(snapshot) => self.apply(hwnd, *snapshot),
                    Command::Quit => self.quit_pending = true,
                }
            }
            // 菜单模态循环里**不许**拆窗口:`TrackPopupMenu` 还在栈上,拆掉等于
            // 把它脚下这个 TrayThread 抽走(WM_DESTROY 会 free 掉它)。
            // 菜单收了之后 `show_menu` 会自己再发一次同步信号把这一步补上。
            self.quit_pending && !self.menu_open
        }

        /// 应用一份快照:可见性 + 图标 + tooltip + 菜单数据。
        fn apply(&mut self, hwnd: HWND, snapshot: TraySnapshot) {
            // 只有灯色/焦点真的变化才重置闪烁相位 —— tooltip/菜单等无关变化不
            // 打断多状态轮转,也不重启单状态的短促闪烁(装机版 tray.rs:289-303)
            let lamps_changed = self.lamps != snapshot.lamps || self.focused != snapshot.focused;
            self.lamps = snapshot.lamps;
            self.enabled = snapshot.enabled;
            self.focused = snapshot.focused;
            self.tooltip = snapshot.tooltip;
            self.projects = snapshot.projects;
            if lamps_changed {
                self.blink.reset();
            }
            self.refresh(hwnd);
        }

        /// 按当前状态重画图标 + 写 tooltip(开关关掉则摘图标)。
        fn refresh(&mut self, hwnd: HWND) {
            if !self.enabled {
                self.remove_icon(hwnd);
                return;
            }
            let colors = active_colors(self.lamps);
            let blinking = self.blink.blinking(self.focused);
            let (color, dim) = frame_color(&colors, self.blink.frame, blinking);
            // 安静态:`>` **与外框同色**再逐帧淡出(理由同 macOS 侧那段注释 ——
            // 系统灰在托盘背景上看不出来);有状态才用状态色,暗帧压到 DIM
            let (chevron, chevron_alpha) = if colors.is_empty() {
                (FRAME_NEUTRAL, self.blink.idle_alpha())
            } else if dim {
                (color, DIM)
            } else {
                (color, 1.0)
            };
            let icon = make_icon(self.icon_size, chevron, chevron_alpha);

            let mut data = self.base_data(hwnd);
            data.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
            data.uCallbackMessage = WM_TRAY_CALLBACK;
            data.hIcon = icon.as_ref().map(|i| i.0).unwrap_or_default();
            write_tip(&mut data.szTip, &self.tooltip);

            let message = if self.added { NIM_MODIFY } else { NIM_ADD };
            // SAFETY: data 是栈上完整初始化的结构,hIcon 在本次调用期间有效
            let ok = unsafe { Shell_NotifyIconW(message, &data) }.as_bool();
            // 失败(shell 那边已经没有这一项了)就退回未登记,下一次按 NIM_ADD 重来
            self.added = ok;
            // shell 在 NIM_ADD/NIM_MODIFY 里拷贝了图标,这一刻才轮到旧句柄退场。
            // 赋值语句先写入新值、再 drop 被覆盖的旧值 —— 顺序正是我们要的。
            self.icon = icon;
        }

        fn remove_icon(&mut self, hwnd: HWND) {
            if self.added {
                let data = self.base_data(hwnd);
                // SAFETY: 同上
                unsafe {
                    let _ = Shell_NotifyIconW(NIM_DELETE, &data);
                }
                self.added = false;
            }
            self.icon = None; // DestroyIcon
        }

        fn base_data(&self, hwnd: HWND) -> NOTIFYICONDATAW {
            NOTIFYICONDATAW {
                cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
                hWnd: hwnd,
                uID: TRAY_UID,
                ..Default::default()
            }
        }

        fn on_timer(&mut self, hwnd: HWND) {
            let colors = active_colors(self.lamps).len();
            if self.blink.tick(colors, self.enabled, self.focused) {
                self.refresh(hwnd);
            }
        }

        /// 托盘图标上的鼠标事件。`lparam` 是原始鼠标消息号(经典回调协议)。
        fn on_callback(&mut self, hwnd: HWND, mouse_message: u32) {
            match mouse_message {
                // 左键:**无条件**唤起主窗口(不看 trayClickFocus 开关),
                // 跳不跳 pane 由主线程按开关决定
                WM_LBUTTONUP => {
                    focus_main_window(self.main_hwnd);
                    let _ = self.events.unbounded_send(TrayEvent::Clicked);
                }
                WM_RBUTTONUP => self.show_menu(hwnd),
                _ => {}
            }
        }

        /// 右键菜单:**只列项目**(无「显示窗口」/「退出」/分隔符),
        /// 项目为空时压根不弹(装机版的 `set_menu(None)`)。
        fn show_menu(&mut self, hwnd: HWND) {
            if self.projects.is_empty() {
                return;
            }
            // `TrackPopupMenu` 是**模态**的:它自带的消息循环会把 WM_TRAY_SYNC
            // 派回 wndproc,期间 `self.projects` 完全可能被换成另一批。菜单是
            // 弹出那一刻的快照,选中项必须按**当时**那份来解读 —— 所以先把要用
            // 的东西全拷成局部量,菜单收了之后一概不再读 self 的这两个字段。
            let labels: Vec<String> = self
                .projects
                .iter()
                .map(|entry| escape_menu_label(&entry.label))
                .collect();
            let ids: Vec<String> = self.projects.iter().map(|entry| entry.id.clone()).collect();
            let events = self.events.clone();
            let main_hwnd = self.main_hwnd;

            let mut pt = POINT::default();
            // SAFETY: 下面整段是 TrackPopupMenu 的标准用法(含 MSDN 记载的
            // SetForegroundWindow 前置与 WM_NULL 后置,少了菜单点外面不消失)
            let command = unsafe {
                if GetCursorPos(&mut pt).is_err() {
                    return;
                }
                let Ok(menu) = CreatePopupMenu() else {
                    return;
                };
                for (index, label) in labels.iter().enumerate() {
                    let text = windows::core::HSTRING::from(label.as_str());
                    let _ =
                        AppendMenuW(menu, MF_STRING, MENU_ID_BASE + index, PCWSTR(text.as_ptr()));
                }
                let _ = SetForegroundWindow(hwnd);
                // 菜单开着期间不许拆窗口(见 [`TrayThread::drain`])
                self.menu_open = true;
                let command = TrackPopupMenu(
                    menu,
                    TPM_RIGHTBUTTON | TPM_RETURNCMD | TPM_NONOTIFY,
                    pt.x,
                    pt.y,
                    None,
                    hwnd,
                    None,
                )
                .0;
                self.menu_open = false;
                let _ = DestroyMenu(menu);
                let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
                if self.quit_pending {
                    // 模态循环里压下来的退出:现在补一记同步信号去兑现它
                    let _ = PostMessageW(Some(hwnd), WM_TRAY_SYNC, WPARAM(0), LPARAM(0));
                }
                command
            };

            if command >= MENU_ID_BASE as i32
                && let Some(id) = ids.get(command as usize - MENU_ID_BASE)
            {
                focus_main_window(main_hwnd);
                let _ = events.unbounded_send(TrayEvent::ProjectClicked(id.clone()));
            }
        }

        /// explorer.exe 重启后图标没了 —— 按 NIM_ADD 重新登记。
        fn readd(&mut self, hwnd: HWND) {
            self.added = false;
            self.refresh(hwnd);
        }

        fn teardown(&mut self, hwnd: HWND) {
            // SAFETY: 定时器由本窗口持有
            unsafe {
                let _ = KillTimer(Some(hwnd), TIMER_ID);
            }
            self.remove_icon(hwnd);
        }
    }

    unsafe extern "system" fn wndproc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        // SAFETY: USERDATA 里要么是 0(还没装上/已释放),要么是 run() 存进去的
        // 那个 Box::into_raw 指针,且只有本线程会取用。
        //
        // ⚠️ 每个分支**各借各的**,不在函数头上 `let state = &mut *ptr` 一次借到底:
        // WM_TRAY_SYNC 收到 Quit 后要 `DestroyWindow`,而那一调用会**同步**递归回
        // 本函数走 WM_DESTROY 把 state 释放掉 —— 外层若还攥着一个 `&mut`,它就是
        // 悬垂引用。
        unsafe {
            let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut TrayThread;
            if ptr.is_null() {
                return DefWindowProcW(hwnd, msg, wparam, lparam);
            }

            match msg {
                WM_TRAY_SYNC => {
                    let quit = (*ptr).drain(hwnd);
                    if quit {
                        // 这一行之后 ptr 已失效(WM_DESTROY 在调用里跑完了)
                        let _ = DestroyWindow(hwnd);
                    }
                    LRESULT(0)
                }
                WM_TIMER if wparam.0 == TIMER_ID => {
                    (*ptr).on_timer(hwnd);
                    LRESULT(0)
                }
                WM_TRAY_CALLBACK => {
                    (*ptr).on_callback(hwnd, lparam.0 as u32);
                    LRESULT(0)
                }
                WM_DESTROY => {
                    (*ptr).teardown(hwnd);
                    // 先摘掉指针再释放:teardown 之后到 free 之间若还有消息进来
                    // (菜单/定时器都可能),它看到的是 null 而不是已释放的内存
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                    drop(Box::from_raw(ptr));
                    PostQuitMessage(0);
                    LRESULT(0)
                }
                other if other != 0 && other == (*ptr).taskbar_created => {
                    (*ptr).readd(hwnd);
                    LRESULT(0)
                }
                _ => DefWindowProcW(hwnd, msg, wparam, lparam),
            }
        }
    }

    /// 唤起主窗口(装机版的 show → unminimize → set_focus)。
    ///
    /// 在**托盘线程**里做:点托盘图标的这一刻 shell 给了本进程抢前台的许可,
    /// 绕一圈回主线程再调 `SetForegroundWindow` 就只会闪任务栏图标了。
    fn focus_main_window(main_hwnd: HWND) {
        // SAFETY: main_hwnd 来自 gpui 主窗口,窗口已关时这些调用只是返回 false
        unsafe {
            let _ = ShowWindow(main_hwnd, SW_SHOW);
            if IsIconic(main_hwnd).as_bool() {
                let _ = ShowWindow(main_hwnd, SW_RESTORE);
            }
            let _ = SetForegroundWindow(main_hwnd);
        }
    }

    /// 把 RGBA 帧变成一个 32bpp 带 alpha 的 HICON。
    ///
    /// shell 走 `AlphaBlend` 画托盘图标,颜色位图必须是**预乘** BGRA;
    /// 掩码位图(1bpp)对 32bpp 图标基本只是形式要求,但不能省、也不能不清零
    /// (`CreateBitmap` 的初始内容是未定义的)。
    fn make_icon(size: i32, color: [u8; 3], chevron_alpha: f32) -> Option<OwnedIcon> {
        // 外框用中性灰:深浅两种任务栏都看得见,不必去读
        // `Themes\Personalize\SystemUsesLightTheme` 判主题
        let rgba = compose_frame_rgba(size as u32, color, chevron_alpha, Some(FRAME_NEUTRAL));
        let pixels = (size * size) as usize;

        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: size,
                // 负高 = top-down,行序与 compose_frame_rgba 一致
                biHeight: -size,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };

        // SAFETY: 下面整段只操作自己刚造出来的位图与它的像素缓冲
        unsafe {
            let mut bits: *mut c_void = std::ptr::null_mut();
            let color_bitmap = OwnedBitmap(
                CreateDIBSection(None, &bmi, DIB_RGB_COLORS, &mut bits, None, 0).ok()?,
            );
            if bits.is_null() {
                return None;
            }
            let dst = std::slice::from_raw_parts_mut(bits as *mut u8, pixels * 4);
            for i in 0..pixels {
                let (r, g, b, a) = (
                    rgba[i * 4] as u32,
                    rgba[i * 4 + 1] as u32,
                    rgba[i * 4 + 2] as u32,
                    rgba[i * 4 + 3] as u32,
                );
                let premul = |c: u32| ((c * a + 127) / 255) as u8;
                dst[i * 4] = premul(b);
                dst[i * 4 + 1] = premul(g);
                dst[i * 4 + 2] = premul(r);
                dst[i * 4 + 3] = a as u8;
            }

            // 1bpp 掩码:全 0 = 处处「显示颜色位图」,行按 4 字节对齐
            let mask_stride = (((size + 31) / 32) * 4) as usize;
            let mask_bits = vec![0u8; mask_stride * size as usize];
            let mask = CreateBitmap(size, size, 1, 1, Some(mask_bits.as_ptr() as *const c_void));
            if mask.is_invalid() {
                return None;
            }
            let mask_bitmap = OwnedBitmap(mask);

            let info = ICONINFO {
                fIcon: true.into(),
                xHotspot: 0,
                yHotspot: 0,
                hbmMask: mask_bitmap.0,
                hbmColor: color_bitmap.0,
            };
            // CreateIconIndirect 会拷贝两张位图,出了这个作用域它们即可释放
            let icon = CreateIconIndirect(&info).ok()?;
            Some(OwnedIcon(icon))
        }
    }

    /// 写 `szTip`(128 个 UTF-16 码元,含结尾 NUL)。超长按码元截断 ——
    /// tooltip 是三个计数拼的短句,正常永远碰不到这个上限。
    fn write_tip(buf: &mut [u16; 128], text: &str) {
        buf.fill(0);
        for (slot, ch) in buf.iter_mut().take(127).zip(text.encode_utf16()) {
            *slot = ch;
        }
    }

    /// Win32 菜单把 `&` 当助记符前缀(`A&B` 会画成 `A_B`),项目名里真出现
    /// `&` 时要转义成 `&&`。**只在这一层做** —— 上游的 label 必须与装机版逐字
    /// 相同(它进的是签名与单测)。
    fn escape_menu_label(label: &str) -> String {
        label.replace('&', "&&")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{AiProjectEntry, AiProjectKind, AiProjects};

    fn entry(id: &str, name: &str, kind: AiProjectKind) -> AiProjectEntry {
        AiProjectEntry {
            id: id.into(),
            name: name.into(),
            kind,
        }
    }

    fn projects(entries: Vec<AiProjectEntry>, counts: (usize, usize, usize)) -> AiProjects {
        AiProjects {
            attention: counts.0,
            working: counts.1,
            done: counts.2,
            entries,
        }
    }

    /// 菜单 label = `emoji 空格 项目名 " · " 状态文案`,四个档位的 emoji 不能串。
    #[test]
    fn 菜单条目按_emoji_名字_状态文案拼() {
        let ai = projects(
            vec![
                entry("p1", "alpha", AiProjectKind::Attention),
                entry("p2", "beta", AiProjectKind::Working),
                entry("p3", "gamma", AiProjectKind::Done),
                entry("p4", "delta", AiProjectKind::Idle),
            ],
            (1, 1, 1),
        );
        let out = menu_entries(&ai, 10);
        // 文案随语言走,格式不随 —— 用 t() 现算,免得把测试钉死在中文上
        assert_eq!(
            out[0].label,
            format!("🟡 alpha · {}", t("app", "trayStatus.attention"))
        );
        assert_eq!(
            out[1].label,
            format!("🔵 beta · {}", t("app", "trayStatus.working"))
        );
        assert_eq!(
            out[2].label,
            format!("🟢 gamma · {}", t("app", "trayStatus.done"))
        );
        assert_eq!(
            out[3].label,
            format!("⚪ delta · {}", t("app", "trayStatus.idle"))
        );
        // id 原样带出去(点菜单要用它定位项目)
        assert_eq!(out[1].id, "p2");
    }

    /// `trayMaxProjects` 截断:超出的**直接不显示**,没有「还有 N 个」的省略行。
    #[test]
    fn 超过上限的项目直接截掉() {
        let ai = projects(
            vec![
                entry("p1", "a", AiProjectKind::Attention),
                entry("p2", "b", AiProjectKind::Working),
                entry("p3", "c", AiProjectKind::Done),
            ],
            (1, 1, 1),
        );
        assert_eq!(menu_entries(&ai, 2).len(), 2);
        assert_eq!(menu_entries(&ai, 0).len(), 0, "手改成 0 就是空菜单");
        assert_eq!(menu_entries(&ai, 99).len(), 3, "上限大于条数不补空行");
    }

    /// tooltip:0 的那档整条不出现;三档齐全时用 ` · ` 连接。
    #[test]
    fn tooltip_跳过零计数() {
        assert_eq!(tooltip(0, 0, 0), "");
        assert_eq!(tooltip(2, 0, 0), tr!("app", "trayAttention", count = 2));
        assert_eq!(
            tooltip(2, 1, 3),
            format!(
                "{} · {} · {}",
                tr!("app", "trayAttention", count = 2),
                tr!("app", "trayWorking", count = 1),
                tr!("app", "trayDone", count = 3)
            )
        );
        // 中间那档为 0 时不留空段
        assert_eq!(
            tooltip(2, 0, 3),
            format!(
                "{} · {}",
                tr!("app", "trayAttention", count = 2),
                tr!("app", "trayDone", count = 3)
            )
        );
    }

    /// 三盏灯由 **pane 级计数**点亮(>0 即亮),`ai-idle` 不参与计数所以不点灯。
    #[test]
    fn 灯色由计数点亮而不是由条目点亮() {
        let ai = projects(vec![entry("p1", "a", AiProjectKind::Idle)], (0, 0, 0));
        let snap = build_snapshot(true, false, &ai, 5);
        assert_eq!(snap.lamps, Lamps::default(), "只有 ai-idle 的项目不点灯");
        assert_eq!(snap.projects.len(), 1, "但它照样进菜单");
        assert_eq!(snap.tooltip, "");

        let ai = projects(vec![entry("p1", "a", AiProjectKind::Working)], (0, 2, 0));
        let snap = build_snapshot(true, false, &ai, 5);
        assert_eq!(
            snap.lamps,
            Lamps {
                attention: false,
                working: true,
                done: false
            }
        );
    }

    /// 去重签名:含开关/焦点/三计数/全部 label,**不含 tooltip**。
    #[test]
    fn 签名覆盖开关焦点计数与标签() {
        let ai = projects(vec![entry("p1", "a", AiProjectKind::Working)], (0, 1, 0));
        let base = build_snapshot(true, false, &ai, 5);

        // 焦点变了 → 签名必须变(闪烁策略要跟着变)
        assert_ne!(base.signature, build_snapshot(true, true, &ai, 5).signature);
        // 开关变了 → 签名必须变
        assert_ne!(base.signature, build_snapshot(false, false, &ai, 5).signature);
        // 计数变了 → 签名必须变(灯没变但 tooltip 变了)
        let more = projects(vec![entry("p1", "a", AiProjectKind::Working)], (0, 3, 0));
        assert_ne!(base.signature, build_snapshot(true, false, &more, 5).signature);
        // 项目名变了 → label 变 → 签名变
        let renamed = projects(vec![entry("p1", "b", AiProjectKind::Working)], (0, 1, 0));
        assert_ne!(
            base.signature,
            build_snapshot(true, false, &renamed, 5).signature
        );
        // 什么都没变 → 签名一致(否则去重失效,每次 notify 都重建图标)
        assert_eq!(base.signature, build_snapshot(true, false, &ai, 5).signature);
    }

    /// 上限把某个项目挤出去之后,签名也要跟着变(否则改上限不生效)。
    #[test]
    fn 上限变化会改签名() {
        let ai = projects(
            vec![
                entry("p1", "a", AiProjectKind::Attention),
                entry("p2", "b", AiProjectKind::Working),
            ],
            (1, 1, 0),
        );
        assert_ne!(
            build_snapshot(true, false, &ai, 1).signature,
            build_snapshot(true, false, &ai, 2).signature
        );
    }

    #[test]
    fn 活跃色按黄蓝绿固定顺序() {
        assert_eq!(active_colors(Lamps::default()).len(), 0);
        assert_eq!(
            active_colors(Lamps {
                attention: true,
                working: false,
                done: false
            }),
            vec![YELLOW]
        );
        assert_eq!(
            active_colors(Lamps {
                attention: true,
                working: true,
                done: true
            }),
            vec![YELLOW, BLUE, GREEN]
        );
    }

    #[test]
    fn 单灯位的帧色语义() {
        // 安静 → 灰,不闪
        assert_eq!(frame_color(&[], 3, true), (GRAY, false));
        // 单状态闪烁:偶帧亮奇帧暗;静止时恒亮
        assert_eq!(frame_color(&[YELLOW], 0, true), (YELLOW, false));
        assert_eq!(frame_color(&[YELLOW], 1, true), (YELLOW, true));
        assert_eq!(frame_color(&[YELLOW], 1, false), (YELLOW, false));
        // 多状态闪烁:同一灯位颜色轮转,全亮
        let colors = [YELLOW, BLUE, GREEN];
        assert_eq!(frame_color(&colors, 0, true), (YELLOW, false));
        assert_eq!(frame_color(&colors, 1, true), (BLUE, false));
        assert_eq!(frame_color(&colors, 2, true), (GREEN, false));
        assert_eq!(frame_color(&colors, 3, true), (YELLOW, false));
        // 多状态静止:停在最高优先级色
        assert_eq!(frame_color(&colors, 2, false), (YELLOW, false));
    }

    /// 闪烁三档:聚焦不闪 / 失焦多状态持续轮转 / 失焦单状态爆闪几帧后定格。
    #[test]
    fn 闪烁相位三档() {
        let mut blink = Blink::default();
        assert!(!blink.tick(1, true, true), "聚焦时不推帧");
        assert!(!blink.tick(1, false, false), "开关关掉不推帧");

        // 单状态:BURST_FRAMES 帧之后定格,再也不推
        let mut blink = Blink::default();
        for i in 0..BURST_FRAMES {
            assert!(blink.tick(1, true, false), "第 {i} 帧应当重绘");
        }
        assert!(blink.tick(1, true, false), "定格那一帧要补一次全亮重绘");
        assert!(blink.settled);
        assert!(!blink.tick(1, true, false), "定格之后不再推帧");
        assert!(!blink.blinking(false), "定格 = 静止");

        // 多状态:永远轮转,不定格
        let mut blink = Blink::default();
        for _ in 0..(BURST_FRAMES * 3) {
            assert!(blink.tick(2, true, false));
        }
        assert!(!blink.settled, "多状态不进定格");

        // 灯色变化 → 相位归零、重新允许爆闪
        blink.settled = true;
        blink.frame = 42;
        blink.reset();
        assert_eq!(blink.frame, 0);
        assert!(!blink.settled);
    }

    /// 安静态:`>` 先亮一会儿再淡到透明,淡完定格(只剩外框)。
    ///
    /// **与焦点无关** —— 没事发生就该安静下去,不管人有没有看着窗口。
    /// 这与闪烁相反:闪烁是「要你注意」,聚焦时自然不闪。
    #[test]
    fn 安静态提示符淡出后定格() {
        let mut blink = Blink::default();
        assert_eq!(blink.idle_alpha(), 1.0, "刚安静下来时全亮");

        // 保持期:仍是全亮
        for _ in 0..IDLE_HOLD_FRAMES {
            assert!(blink.tick(0, true, false), "淡出期间要重绘");
        }
        assert_eq!(blink.idle_alpha(), 1.0, "保持期内不该开始淡");

        // 淡出期:逐帧变淡
        let mut last = blink.idle_alpha();
        for _ in 0..IDLE_FADE_FRAMES {
            assert!(blink.tick(0, true, false));
            let now = blink.idle_alpha();
            assert!(now < last, "应当逐帧变淡:{last} → {now}");
            last = now;
        }
        assert_eq!(last, 0.0, "淡完是全透明");
        assert!(blink.settled, "淡完就定格");
        assert!(!blink.tick(0, true, false), "定格之后不再推帧");

        // 聚焦时照样淡(与闪烁不同)
        let mut focused = Blink::default();
        assert!(focused.tick(0, true, true), "安静态的淡出不看焦点");
    }

    /// `>` 尖端所在像素的字节下标。几何按 `size` 取比例,任何画布都落在实心笔画上
    /// —— **不能拿画布正中当采样点**:`>` 的开口正对着中心,那里是空的。
    fn chevron_tip(size: u32) -> usize {
        let s = size as f32;
        let x = (s * 0.66) as u32;
        let y = (s * 0.50) as u32;
        ((y * size + x) * 4) as usize
    }

    /// 画布:`>` 的笔画是纯色不透明,四角必须完全透明(圆角框也够不到)。
    #[test]
    fn 提示符画得出笔画且四角透明() {
        const SIZE: u32 = 16;
        let rgba = compose_frame_rgba(SIZE, GREEN, 1.0, None);
        assert_eq!(rgba.len(), (SIZE * SIZE * 4) as usize);
        let tip = chevron_tip(SIZE);
        assert_eq!(&rgba[tip..tip + 3], &GREEN);
        assert_eq!(rgba[tip + 3], 255);
        // 四角离图形很远
        assert_eq!(rgba[3], 0);
        assert_eq!(rgba[(SIZE * SIZE * 4 - 1) as usize], 0);
    }

    /// 画的确实是 `>` 而不是圆:`>` 的**开口**(左侧中部)必须是空的。
    ///
    /// 这一条是形状的真正判据 —— 上面那个「中心实心 + 四角透明」圆点同样满足,
    /// 单靠它换回实心圆也测不出来。
    #[test]
    fn 提示符左侧开口是空的() {
        for size in [16u32, 20, 24, 32, 44] {
            let rgba = compose_frame_rgba(size, GREEN, 1.0, None);
            let s = size as f32;
            // 折线的水平起点是 0.32s、尖端 0.68s;取 0.36s 处的中线,
            // 它在两条笔画之间的开口里(而半径 0.36s 的圆会盖住这里)
            let x = (s * 0.36) as u32;
            let y = (s * 0.50) as u32;
            let idx = ((y * size + x) * 4) as usize;
            assert_eq!(rgba[idx + 3], 0, "size={size} 开口处不该有笔画");
        }
    }

    /// 暗帧只压 alpha,不改颜色。
    #[test]
    fn 暗帧压低_alpha_不改颜色() {
        const SIZE: u32 = 16;
        let bright = compose_frame_rgba(SIZE, YELLOW, 1.0, None);
        let dim = compose_frame_rgba(SIZE, YELLOW, DIM, None);
        let tip = chevron_tip(SIZE);
        assert_eq!(bright[tip + 3], 255);
        let a = dim[tip + 3];
        assert!(a > 0 && a < 100, "暗帧 alpha 应当落在 (0,100),实际 {a}");
        assert_eq!(&dim[tip..tip + 3], &YELLOW);
    }

    /// 外框:`frame` 给了就画一圈,给 `None` 就只有 `>`。
    ///
    /// 外框是「这个应用在这儿」的常驻标识 —— 安静态 `>` 淡光之后全靠它,
    /// 没有它图标会整个消失,那会被当成程序退了。
    #[test]
    fn 外框可开关且用自己的颜色() {
        const SIZE: u32 = 44;
        // 框的左边线在 x≈0.06s 处,取纵向中点采样
        let x = (SIZE as f32 * FRAME_INSET) as u32;
        let y = SIZE / 2;
        let idx = ((y * SIZE + x) * 4) as usize;

        let with = compose_frame_rgba(SIZE, GREEN, 1.0, Some(FRAME_LIGHT));
        assert_eq!(with[idx + 3], 255, "该处应当有外框");
        assert_eq!(
            &with[idx..idx + 3],
            &FRAME_LIGHT,
            "外框用自己的颜色,不是状态色"
        );

        let without = compose_frame_rgba(SIZE, GREEN, 1.0, None);
        assert_eq!(without[idx + 3], 0, "不给 frame 就不该有框");
    }

    /// 安静态淡出:`>` 透明之后,外框仍在(图标不会整个消失)。
    #[test]
    fn 提示符淡光后外框仍在() {
        const SIZE: u32 = 44;
        let rgba = compose_frame_rgba(SIZE, GRAY, 0.0, Some(FRAME_LIGHT));

        // `>` 的尖端所在处应当空了
        let chev = chevron_tip(SIZE);
        assert_eq!(rgba[chev + 3], 0, "淡完之后不该还有笔画");

        // 外框仍然实心
        let fx = (SIZE as f32 * FRAME_INSET) as u32;
        let frame = (((SIZE / 2) * SIZE + fx) * 4) as usize;
        assert_eq!(rgba[frame + 3], 255, "外框必须留着");
    }

    /// 画布尺寸两边不同(Win32 跟 `SM_CXSMICON`,macOS 固定 44px),
    /// 几何全按比例取,任何尺寸都得画得出笔画。
    #[test]
    fn 不同画布尺寸都画得出笔画() {
        for size in [16u32, 20, 24, 32, 44] {
            let rgba = compose_frame_rgba(size, BLUE, 1.0, None);
            let tip = chevron_tip(size);
            assert_eq!(&rgba[tip..tip + 3], &BLUE, "size={size}");
            assert_eq!(rgba[tip + 3], 255, "size={size}");
            assert_eq!(rgba[3], 0, "size={size} 左上角应透明");
        }
    }
}
