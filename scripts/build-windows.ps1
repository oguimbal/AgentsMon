$ErrorActionPreference = "Stop"
$Root = Resolve-Path "$PSScriptRoot\.."
Set-Location $Root

$env:RUSTC = rustup which --toolchain stable rustc
$env:RUSTDOC = rustup which --toolchain stable rustdoc
rustup run stable cargo build --release -p agentsmon -p agentsmon-helper

$Dist = Join-Path $Root "dist\windows"
New-Item -ItemType Directory -Force $Dist | Out-Null
Copy-Item "target\release\agentsmon.exe" $Dist -Force
Copy-Item "target\release\agentsmon-helper.exe" $Dist -Force
$CodexPlugin = Join-Path $Dist "codex-plugin"
Copy-Item "plugins\agentsmon-codex" $CodexPlugin -Recurse -Force
New-Item -ItemType Directory -Force (Join-Path $CodexPlugin "bin") | Out-Null
Copy-Item "target\release\agentsmon-helper.exe" (Join-Path $CodexPlugin "bin\agentsmon-helper.exe") -Force
Write-Host "AgentsMon staged in $Dist"
