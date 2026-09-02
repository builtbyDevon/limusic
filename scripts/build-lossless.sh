#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "$0")/.." && pwd)"

(cd "$repo_root/ui" && pnpm build)
(cd "$repo_root" && pnpm dlx '@tauri-apps/cli@^2' build --config src-tauri/tauri.lossless.conf.json "$@")
