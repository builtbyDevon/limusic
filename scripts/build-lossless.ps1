$ErrorActionPreference = "Stop"
$repoRoot = Split-Path -Parent $PSScriptRoot

Push-Location (Join-Path $repoRoot "ui")
$previousLosslessBuild = $env:VITE_LIMUSIC_LOSSLESS
try {
	$env:VITE_LIMUSIC_LOSSLESS = "true"
    pnpm build
} finally {
	if ($null -eq $previousLosslessBuild) {
		Remove-Item Env:\VITE_LIMUSIC_LOSSLESS -ErrorAction SilentlyContinue
	} else {
		$env:VITE_LIMUSIC_LOSSLESS = $previousLosslessBuild
	}
    Pop-Location
}

Push-Location $repoRoot
try {
    pnpm dlx "@tauri-apps/cli@^2" build --config src-tauri/tauri.lossless.conf.json @args
} finally {
    Pop-Location
}
