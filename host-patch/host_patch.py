#!/usr/bin/env python3
"""最小宿主增量：让 OCS Pi Extension 面板能在 OpenCADStudio 里加载。

用法（在 OCS 仓库根目录或任意位置执行，脚本只改你给的仓库）：

    python3 host_patch.py --check /path/to/OpenCADStudio     # 只体检，不写文件
    python3 host_patch.py --apply /path/to/OpenCADStudio     # 就地打补丁

设计要点
--------
* **锚点式**：每一步都靠上游源码里的原文片段定位，不依赖行号，所以上游小改动
  一般不影响；找不到锚点时**只报告、不猜**（会列出该文件里可能的近似行）。
* **幂等**：已打过的步骤（锚点已变成目标文本、或目标文本已存在）会跳过。
* **最小**：只动必须动的接点 —— 面板变体/注册/消息路由/渲染分支/订阅，
  外加一条"每面板宽度策略"（可选的增强项，锚点不匹配就跳过，不影响可用性）。

基准：upstream `v2026.38`（本脚本针对该版本的 dock.rs/view/mod.rs 等文件文本编写）。
上游之后若改过这些位置，`--check` 会把不匹配的步骤列出来，按提示手工补即可。
"""

import argparse
import pathlib
import sys

MARK = "OCS Pi Extension"

check_only = False

# ── 片段 ────────────────────────────────────────────────────────────────────
# 每个 step: (描述, 相对路径, 模式, 锚点, 文本)
#   模式 insert_after / insert_before : 把 文本 插到 锚点 之后/之前
#   模式 replace                      : 用 文本 替换 锚点
#   模式 ensure                       : 只有当 文本 不存在时才在 锚点 处插入（用于兼容已改过的上游）

PANEL_VARIANT = """    /// **OCS Pi Extension**: the built-in Pi assistant panel — a chat view over
    /// a local `pi` agent (the pi-web HTTP API, or a spawned `pi --mode rpc`
    /// child). An ordinary iced panel, so it docks, drags, resizes and
    /// collapses exactly like Properties, on both X11 and Wayland.
    Pi,
"""

MESSAGE_VARIANTS = """    /// Toggle the built-in Pi assistant panel (`PI` command).
    TogglePiPanel,
    /// Sub-messages of the built-in Pi assistant panel (poll/send/…).
    Pi(crate::ui::pi_panel::PiMsg),
    /// Pi composer paste result: `Ok(image)` = clipboard image,
    /// `Err(Some(text))` = text fallback, `Err(None)` = nothing to paste.
    PiImagePasted(
        Result<iced::clipboard::Image, Option<std::sync::Arc<String>>>,
    ),
"""

MIN_MAX_WIDTH = """    /// Per-panel width floor. The Pi panel is a chat column used *next to* the
    /// drawing, so it may be squeezed far below the palette-ish default.
    pub fn min_width(self) -> f32 {
        match self {
            PanelId::Pi => 150.0,
            _ => DOCK_MIN_W,
        }
    }

    /// Widest a panel may be dragged or sized to. The references table is
    /// column-rich, so it allows double the shared maximum.
    fn max_width(self) -> f32 {
        match self {
            PanelId::Pi => 1200.0,
            PanelId::ExternalReferences => DOCK_MAX_W * 2.0,
            _ => DOCK_MAX_W,
        }
    }

    /// Per-panel share of the window width the dock may ever take.
    fn max_fraction(self) -> f32 {
        match self {
            // A chat panel must stay a side column even on a small window.
            PanelId::Pi => 0.4,
            _ => 0.45,
        }
    }
"""

WIDTH_BODY = """        let floor = id.min_width();
        self.settings(id).width.clamp(
            floor,
            id.max_width()
                .min(win_w * id.max_fraction())
                .max(floor),
        )
"""

TOGGLE_HANDLER = """            Message::TogglePiPanel => {
                use crate::ui::dock::PanelId;
                self.show_pi_panel ^= true;
                if self.show_pi_panel {
                    // Dock it on the right, appending after whatever is there.
                    self.dock.ensure_settings();
                    let _ = self.dock.dock(PanelId::Pi, crate::app::config::DockSide::Right, usize::MAX);
                    // Arm the worker: the panel renders from whatever the
                    // backend thread has pushed so far.
                    let tab = self.active_tab;
                    self.tabs[tab].pi_panel.ensure_worker();
                } else {
                    // Panel closed: stop streaming + free the worker thread.
                    let tab = self.active_tab;
                    self.tabs[tab].pi_panel.stop_worker();
                }
                Task::none()
            }
"""

STEPS = [
    # ── 模块注册 ────────────────────────────────────────────────────────────
    ("注册 src/pi.rs / src/pi_rpc.rs", "src/lib.rs", "insert_after",
     "pub mod app;\n", "pub mod pi;\npub mod pi_rpc;\n"),
    ("注册 src/ui/pi_panel.rs", "src/ui/mod.rs", "insert_before",
     "pub mod properties;\n", "pub mod pi_panel;\n"),

    # ── 停靠面板 ────────────────────────────────────────────────────────────
    ("PanelId 增加 Pi 变体", "src/ui/dock.rs", "replace",
     "    BlockPalette,\n    ExternalReferences,\n",
     "    BlockPalette,\n" + PANEL_VARIANT + "    ExternalReferences,\n"),
    ("ensure_settings 收录 Pi（设置表自愈）", "src/ui/dock.rs", "replace",
     "        for id in [\n            PanelId::Properties,\n            PanelId::BlockPalette,\n            PanelId::ExternalReferences,\n            PanelId::Browser,\n        ] {\n",
     "        for id in [\n            PanelId::Properties,\n            PanelId::BlockPalette,\n            PanelId::Pi,\n            PanelId::ExternalReferences,\n            PanelId::Browser,\n        ] {\n"),
    ("面板标题", "src/ui/dock.rs", "insert_after",
     '            PanelId::BlockPalette => "Block Palette",\n',
     "            PanelId::Pi => crate::pi::panel_title(),\n"),
    ("默认宽度 340", "src/ui/dock.rs", "insert_after",
     "            PanelId::BlockPalette => 260.0,\n",
     "            // A chat column should not steal the drawing area by default.\n            PanelId::Pi => 340.0,\n"),
    ("宽度上下限 + 占比（含 min_width / max_fraction 钩子）", "src/ui/dock.rs", "replace",
     """    /// Widest a panel may be dragged or sized to. The references table is
    /// column-rich, so it allows double the shared maximum.
    fn max_width(self) -> f32 {
        match self {
            PanelId::ExternalReferences => DOCK_MAX_W * 2.0,
            _ => DOCK_MAX_W,
        }
    }
""",
     MIN_MAX_WIDTH),
    ("宽度 clamp 用每面板下限/占比", "src/ui/dock.rs", "replace",
     "        self.settings(id)\n            .width\n            .clamp(DOCK_MIN_W, id.max_width().min(win_w * 0.45).max(DOCK_MIN_W))\n",
     WIDTH_BODY),
    ("set_width 用每面板下限", "src/ui/dock.rs", "replace",
     "        entry.width = width.clamp(DOCK_MIN_W, id.max_width());\n",
     "        entry.width = width.clamp(id.min_width(), id.max_width());\n"),
    ("Pi 面板默认自动收起", "src/ui/dock.rs", "replace",
     "    fn for_id(id: PanelId) -> Self {\n        Self {\n            width: id.default_width(),\n            auto_collapse: false,\n        }\n    }\n",
     "    fn for_id(id: PanelId) -> Self {\n        Self {\n            width: id.default_width(),\n            // The Pi panel starts auto-collapsed: it lives as a rail unless the\n            // pointer is over it, so it never eats the drawing area by default.\n            auto_collapse: matches!(id, PanelId::Pi),\n        }\n    }\n"),

    # ── 应用状态与消息 ──────────────────────────────────────────────────────
    ("Message 增加 Pi / TogglePiPanel / PiImagePasted", "src/app/mod.rs", "insert_after",
     "    Tick(Instant),\n", MESSAGE_VARIANTS),
    ("App 增加 show_pi_panel 字段", "src/app/mod.rs", "insert_after",
     "    show_properties: bool,\n",
     "    /// Is the **OCS Pi Extension** panel docked/visible?\n    show_pi_panel: bool,\n"),
    ("App 初始化 show_pi_panel", "src/app/mod.rs", "insert_after",
     "            show_properties: true,\n", "            show_pi_panel: false,\n"),
    ("Tab 增加 pi_panel 状态", "src/app/document.rs", "insert_after",
     "    pub(super) properties: PropertiesPanel,\n",
     "    /// OCS Pi Extension panel state (chat view over a local `pi` agent).\n    pub(super) pi_panel: crate::ui::pi_panel::PiPanelState,\n"),
    ("Tab 初始化 pi_panel", "src/app/document.rs", "insert_after",
     "            properties: PropertiesPanel::empty(),\n",
     "            pi_panel: crate::ui::pi_panel::PiPanelState::empty(),\n"),

    # ── 消息路由 ────────────────────────────────────────────────────────────
    ("注册 update/pi.rs 模块", "src/app/update/mod.rs", "insert_after",
     "mod page_setup_import;\n", "mod pi;\n"),
    ("Pi 消息分发", "src/app/update/mod.rs", "insert_after",
     "            Message::Tick(t) => self.on_tick(t),\n",
     "\n            // OCS Pi Extension panel.\n            Message::Pi(msg) => self.on_pi_msg(msg),\n\n            // Pi composer paste completion.\n            Message::PiImagePasted(payload) => self.on_pi_image_pasted(payload),\n"),
    ("PI 命令处理（开关面板 + 起后端）", "src/app/update/mod.rs", "insert_before",
     "            Message::ToggleFileTabs => {\n", TOGGLE_HANDLER),
    ("PI 命令注册", "src/app/commands/display.rs", "insert_before",
     "            // ── FILETAB — toggle file/document tabs",
     """            // ── PI — OCS Pi Extension: toggle the built-in Pi assistant panel ───
            // A native chat panel over a local `pi` agent; docked like any other
            // panel. (AI / AICHAT kept as legacy aliases.)
            "PI" | "OCSPI" | "AI" | "AICHAT" => {
                return Some(Task::done(Message::TogglePiPanel));
            }

"""),

    # ── 停靠面板的开关/可见性 ───────────────────────────────────────────────
    ("关闭面板时停后端", "src/app/update/dialog.rs", "insert_before",
     "                    PanelId::ExternalReferences => {\n                        self.show_external_references = false;\n                    }\n",
     "                    // Closing hides the panel (it keeps its dock slot, like the\n                    // others, so the `PI` command brings it back where it was).\n                    PanelId::Pi => {\n                        self.show_pi_panel = false;\n                        // Stop streaming + free the worker thread.\n                        let tab = self.active_tab;\n                        self.tabs[tab].pi_panel.stop_worker();\n                    }\n"),
    ("面板可见性", "src/app/update/dialog.rs", "insert_after",
     "            PanelId::Properties => self.show_properties,\n",
     "            PanelId::Pi => self.show_pi_panel,\n"),

    # ── 渲染与轮询 ──────────────────────────────────────────────────────────
    ("停靠列可见性分支", "src/app/view/mod.rs", "insert_after",
     "                crate::ui::dock::PanelId::BlockPalette => self.show_block_palette,\n",
     "                crate::ui::dock::PanelId::Pi => self.show_pi_panel,\n"),
    ("面板渲染分支", "src/app/view/mod.rs", "insert_before",
     "            crate::ui::dock::PanelId::ExternalReferences => self.xref_manager.view(",
     """            // OCS Pi Extension: native chat panel. Plain iced rendering, so it
            // behaves and looks like every other dockable panel.
            crate::ui::dock::PanelId::Pi => self.tabs[self.active_tab]
                .pi_panel
                .view(
                    width,
                    auto_collapse,
                    &tab.pi_panel.selection_label,
                    &self.active_theme,
                )
            .into(),
"""),
    ("~10Hz 轮询订阅", "src/app/view/mod.rs", "insert_before",
     "        let hatch_pattern_keys = if self.tabs[self.active_tab]\n",
     """        // OCS Pi Extension: drain the backend client at ~10 Hz while the panel
        // is open (worker events land in a channel; `PiMsg::Poll` drains it).
        #[cfg(not(target_arch = "wasm32"))]
        let pi_poll = if self.show_pi_panel && self.tabs[self.active_tab].pi_panel.worker.is_some() {
            iced::time::every(std::time::Duration::from_millis(100))
                .map(|_| Message::Pi(crate::ui::pi_panel::PiMsg::Poll))
        } else {
            Subscription::none()
        };
        #[cfg(target_arch = "wasm32")]
        let pi_poll = Subscription::none();
"""),
    ("订阅列表加入 pi_poll", "src/app/view/mod.rs", "insert_after",
     "            plugin_drain,\n", "            pi_poll,\n"),
]


def locate(content: str, anchor: str):
    """返回 (起始下标, 出现次数)。"""
    return content.find(anchor), content.count(anchor)


def apply_step(root: pathlib.Path, desc: str, rel: str, mode: str, anchor: str, text: str):
    path = root / rel
    if not path.exists():
        return "missing-file", f"{rel} 不存在"
    content = path.read_text(encoding="utf-8")

    if mode == "insert_after" and text in content:
        return "already", "已打过"
    if mode == "replace" and text in content and anchor not in content:
        return "already", "已打过"
    if mode == "insert_before" and text in content:
        return "already", "已打过"

    idx, count = locate(content, anchor)
    if idx < 0:
        return "anchor-missing", f"锚点未找到：{anchor.strip()[:60]!r}"
    if count > 1 and mode == "replace":
        return "anchor-ambiguous", f"锚点出现 {count} 次，需人工确认"

    if mode == "insert_after":
        new = content[: idx + len(anchor)] + text + content[idx + len(anchor):]
    elif mode == "insert_before":
        new = content[:idx] + text + content[idx:]
    elif mode == "replace":
        new = content[:idx] + text + content[idx + len(anchor):]
    else:
        return "bad-mode", mode

    if not check_only:
        path.write_text(new, encoding="utf-8")
    return "ok", "已应用" if not check_only else "可应用"


def main():
    ap = argparse.ArgumentParser(description=f"{MARK} host patch")
    ap.add_argument("root", help="OpenCADStudio 仓库根目录")
    g = ap.add_mutually_exclusive_group(required=True)
    g.add_argument("--check", action="store_true", help="只体检")
    g.add_argument("--apply", action="store_true", help="就地打补丁")
    args = ap.parse_args()

    global check_only
    check_only = args.check
    root = pathlib.Path(args.root).expanduser().resolve()
    if not (root / "Cargo.toml").exists():
        sys.exit(f"× {root} 看起来不是 OpenCADStudio 仓库（没有 Cargo.toml）")

    counts = {}
    print(f"目标仓库：{root}\n模式：{'体检（不写文件）' if check_only else '应用'}\n")
    for desc, rel, mode, anchor, text in STEPS:
        status, note = apply_step(root, desc, rel, mode, anchor, text)
        counts[status] = counts.get(status, 0) + 1
        mark = {"ok": "✓", "already": "=", "missing-file": "!", "anchor-missing": "✗",
                "anchor-ambiguous": "?", "bad-mode": "!"}[status]
        print(f" {mark} [{status:17}] {rel:28} {desc}  — {note}")

    print("\n汇总：" + "  ".join(f"{k}={v}" for k, v in sorted(counts.items())))
    if counts.get("anchor-missing") or counts.get("anchor-ambiguous") or counts.get("missing-file"):
        print("× 有步骤未能自动定位（上游可能改过这些文件）——请按上面提示手工补，"
              "或用 `git apply` 应用 host.patch（若提供）。")
        return 1
    print("✓ 全部接点就绪。" + ("" if not check_only else "（体检模式，未写文件；加 --apply 正式应用）"))
    return 0


if __name__ == "__main__":
    sys.exit(main())
