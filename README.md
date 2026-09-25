# OCS Pi Extension

A **native chat panel for [OpenCADStudio](https://github.com/HakanSeven12/OpenCADStudio)** that talks to the
[pi](https://github.com/badlogic/pi-mono) coding agent — the same agent surface as the pi CLI/TUI, docked next to
your drawing, drawn with OCS's own iced widgets (no embedded browser, works on X11 **and** Wayland).

```
┌ Pi 助手 ─────────────────────────────────────────────┐
│ 读一下这张图的标注层，把不符合 GB/T 4458.4 的挑出来   │
│ ● Pi  ↑4.5M ↓374.7k · 缓存 343.9M/0 · 上下文 68% · $6.2 │
│ ▸ 思考   ▸ 工具·read 完成   ▸ 工具·bash 完成          │
└──────────────────────────────────────────────────────┘
```

* **Two backends, switchable in the panel**
  * `rpc` — spawns a local `pi --mode rpc` child process. **No pi-web required.**
  * `web` — talks to a local [pi-web](https://github.com/agegr/pi-web) instance (`http://127.0.0.1:30141`).
  * `auto` (default) — use pi-web if it answers, otherwise spawn `pi --mode rpc`.
    The picker lives in the panel's status strip, right after the connected-backend label, and your choice is
    remembered across restarts.
* **Full chat**: streaming transcript, thinking blocks, tool calls with output, markdown (incl. wide tables that
  scroll instead of squeezing), image attachments from the clipboard, `@file` references, `/command` completion,
  model + thinking-level pickers, manual context compaction, token/cost/context accounting.
* **Reads your selection**: the composer shows what you have highlighted in the drawing (`圆弧（1）`), so you can
  say "this arc" and the agent sees which entity you mean.
* **Answers pi's questions**: `select` / `confirm` / free-text (`input`, `editor`) extension UI requests are
  rendered as a panel dialog — approvals never leave the CAD window.

## Requirements

| | |
|---|---|
| OpenCADStudio | source tree at **v2026.38** (the patch is anchored on that release's files) |
| pi | installed and logged in (`pi` on `PATH`, or `~/.local/bin/pi`) — for the `rpc` backend |
| pi-web *(optional)* | only if you want the `web` backend |
| Rust | the same toolchain OCS uses (stable) |

## Install

```bash
git clone https://github.com/YsDirector/ocs-pi-extension
cd ocs-pi-extension
./install.sh /path/to/OpenCADStudio            # --check to dry-run first

cd /path/to/OpenCADStudio
cargo build --release
./target/release/OpenCADStudio
```

Then type **`PI`** in the OCS command line (`AI` / `AICHAT` / `OCSPI` are aliases). The panel docks on the right,
like the Properties palette — drag it to another edge, resize it, pin or close it.

`install.sh` does two things and nothing else:

1. copies **four new files** into your tree (`src/pi.rs`, `src/pi_rpc.rs`, `src/ui/pi_panel.rs`,
   `src/app/update/pi.rs`) — no existing file is overwritten by this step;
2. runs `host-patch/host_patch.py`, which makes the **minimal host-side increment** the panel needs: a dock panel
   id, module registrations, three `Message` variants, a per-tab `PiPanelState`, the message routing, the `PI`
   command, the render branch and a ~10 Hz drain subscription. It is anchor-based (no line numbers), idempotent,
   and prints exactly what it changed — `--check` gives you the report without touching anything.

> The panel cannot be a standalone `.so` plugin: OCS plugins can contribute ribbon buttons, commands and dialogs,
> but not UI panels, so the panel has to live in the host — hence the small patch instead of a plugin binary.
> If you want to see that change, it is discussed in
> [issue #1303](https://github.com/HakanSeven12/OpenCADStudio/issues/1303).

## Configuration

| Env var | Meaning | Default |
|---|---|---|
| `OCS_PI_MODE` | initial backend: `auto` / `web` / `rpc` | `auto` |
| `OCS_PI_ENDPOINT` | pi-web endpoint (`web`/`auto`) | `http://127.0.0.1:30141` |
| `OCS_PI_BIN` | `pi` executable (`rpc`) | `pi` (searched on `PATH`, then `~/.local/bin`, `~/.npm-global/bin`, `~/.cargo/bin`) |
| `OCS_PI_CWD` | project dir for new sessions and the `@file` index | `$HOME` |

The backend picker writes its choice to `~/.config/OpenCADStudio/pi-panel-backend.txt`, which takes precedence over
`OCS_PI_MODE` (deliberate: the panel is an add-on, so its preference stays out of OCS's own `settings.json`).

## Repository layout

```
install.sh                     installer (copy sources + host patch)
host-patch/host_patch.py       the anchor-based host increment (--check / --apply)
src/pi.rs                      pi-web HTTP backend + shared protocol types (sessions, events, stats, …)
src/pi_rpc.rs                  `pi --mode rpc` child-process backend
src/ui/pi_panel.rs             the iced panel (transcript, composer, pickers, approvals)
src/app/update/pi.rs           panel message handling + the canvas-selection summary
docs/DESIGN.zh.md              design notes / rationale / war stories (Chinese)
```

## Known limitations

* **UI strings are Chinese** (the panel was written for a Chinese-speaking user). English strings are welcome as
  a PR.
* Single-user desktop app: the panel spawns a local `pi` process and reads `~/.pi/agent/sessions/**`; there is no
  remote/multi-user mode.
* The `web` backend needs pi-web's HTTP API; `rpc` needs the `pi` CLI. If neither is present the panel shows an
  empty state telling you so and keeps working for everything else.
* Wide markdown tables render in a horizontally scrollable sub-region (cells are pinned to a fixed minimum width)
  rather than being squeezed into one-character columns — that is a deliberate trade-off, see `docs/DESIGN.zh.md`.
* The patch targets OCS **v2026.38**; a newer upstream may move the anchors. `host_patch.py --check` tells you
  which steps no longer match, with the exact snippets to add by hand.

## License

**GPL-3.0** — this extension is a derivative work of OpenCADStudio (GPL-3.0), and it is distributed under the same
terms. See [`LICENSE`](LICENSE).

Not affiliated with the OpenCADStudio project; it is an add-on maintained separately.
