#!/usr/bin/env bash
# OCS Pi Extension —— 把面板装进一份 OpenCADStudio 源码树。
#
# 用法：
#   ./install.sh /path/to/OpenCADStudio            # 拷贝源码 + 打宿主补丁
#   ./install.sh /path/to/OpenCADStudio --check    # 只体检（不写任何文件）
#
# 之后：
#   cd /path/to/OpenCADStudio && cargo build --release
#   ./target/release/OpenCADStudio
# 在 OCS 命令行敲 `PI` 打开面板（AI / AICHAT / OCSPI 为别名）。
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
target="${1:-}"
mode="${2:-}"

if [[ -z "$target" ]]; then
  echo "用法: $0 /path/to/OpenCADStudio [--check]" >&2
  exit 2
fi
target="$(cd "$target" && pwd)"
[[ -f "$target/Cargo.toml" ]] || { echo "× $target 里没有 Cargo.toml，不像 OpenCADStudio 源码树" >&2; exit 2; }

check_only=0
[[ "$mode" == "--check" ]] && check_only=1

echo "== 目标：$target"
echo "== 模式：$([[ $check_only -eq 1 ]] && echo '体检（不写文件）' || echo '安装')"

if [[ $check_only -eq 0 ]]; then
  echo "== 1/2 拷贝面板源码"
  install -Dm644 "$here/src/pi.rs"                 "$target/src/pi.rs"
  install -Dm644 "$here/src/pi_rpc.rs"             "$target/src/pi_rpc.rs"
  install -Dm644 "$here/src/ui/pi_panel.rs"        "$target/src/ui/pi_panel.rs"
  install -Dm644 "$here/src/app/update/pi.rs"      "$target/src/app/update/pi.rs"
  echo "   已写入 4 个新文件（其余全是新增，不会覆盖你的任何文件）"
else
  echo "== 1/2 拷贝面板源码（体检模式跳过）"
fi

echo "== 2/2 宿主增量接点"
if [[ $check_only -eq 1 ]]; then
  python3 "$here/host-patch/host_patch.py" --check "$target"
else
  python3 "$here/host-patch/host_patch.py" --apply "$target"
fi

if [[ $check_only -eq 0 ]]; then
  cat <<'EOF'

✓ 安装完成。下一步：

    cd <OpenCADStudio> && cargo build --release
    菜单/命令行：PI            （别名 AI / AICHAT / OCSPI）

运行要求：
  · rpc 后端（推荐，最省事）：本机有 `pi` 可执行文件（PATH 或 ~/.local/bin），
    面板状态条把后端切到 `rpc` 即可，不需要 pi-web。
  · web 后端：本机跑着 pi-web（默认 http://127.0.0.1:30141）。

环境变量（可选）：
  OCS_PI_MODE=auto|web|rpc   初始后端模式（面板内选择会被记住，优先于它）
  OCS_PI_ENDPOINT=http://…    pi-web 端点
  OCS_PI_BIN=pi               pi 可执行文件（rpc 模式）
  OCS_PI_CWD=/path            新建会话/文件索引的默认目录（缺省 $HOME）
EOF
fi
