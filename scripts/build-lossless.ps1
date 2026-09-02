$ErrorActionPreference = "Stop"
$repoRoot = Split-Path -Parent $PSScriptRoot

Push-Location (Join-Path $repoRoot "ui")
try {
    pnpm build
} finally {
    Pop-Location
}

Push-Location $repoRoot
try {
    pnpm dlx "@tauri-apps/cli@^2" build --config src-tauri/tauri.lossless.conf.json @args
} finally {
    Pop-Location
}
