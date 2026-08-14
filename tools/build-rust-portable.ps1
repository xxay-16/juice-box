param(
    [string]$OutputDirectory = "dist/JuiceboxRust-portable",
    [switch]$SkipRealDataVerification
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

cargo fmt --all -- --check
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
cargo clippy --workspace --all-targets -- -D warnings
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
cargo test --workspace
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

if (!$SkipRealDataVerification `
    -and (Test-Path "../data/genome.hic") `
    -and (Test-Path "../data/genome.assembly") `
    -and (Test-Path "../app/juicebox.jar") `
    -and (Test-Path "D:/runtime/jdk-25/bin/java.exe")) {
    & "$PSScriptRoot/verify-real-data.ps1"
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
}

cargo build --release -p heatmap-wgpu
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

$distRoot = [System.IO.Path]::GetFullPath((Join-Path $root "dist"))
$output = [System.IO.Path]::GetFullPath((Join-Path $root $OutputDirectory))
$distPrefix = $distRoot.TrimEnd([System.IO.Path]::DirectorySeparatorChar) + [System.IO.Path]::DirectorySeparatorChar
if ($output -eq $distRoot -or !$output.StartsWith($distPrefix, [System.StringComparison]::OrdinalIgnoreCase)) {
    throw "OutputDirectory must resolve to a dedicated subdirectory under $distRoot"
}
if (Test-Path -LiteralPath $output) {
    Remove-Item -LiteralPath $output -Recurse -Force
}
New-Item -ItemType Directory -Force -Path $output | Out-Null
Copy-Item -LiteralPath "target/release/heatmap-wgpu.exe" `
    -Destination (Join-Path $output "JuiceboxRust.exe")
Copy-Item -LiteralPath "docs/RUST-PORTABLE.md" `
    -Destination (Join-Path $output "README.md")

$exe = Get-Item -LiteralPath (Join-Path $output "JuiceboxRust.exe")
Write-Output "Portable build ready: $($exe.FullName) ($($exe.Length) bytes)"
