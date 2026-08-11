param(
    [string]$HicFile = "../data/genome.hic",
    [string]$AssemblyFile = "../data/genome.assembly",
    [string]$JavaJar = "../app/juicebox.jar",
    [string]$JavaHome = "D:/runtime/jdk-25"
)

$ErrorActionPreference = "Stop"

$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

if (!(Test-Path $HicFile)) { throw "Missing .hic input: $HicFile" }
if (!(Test-Path $AssemblyFile)) { throw "Missing .assembly input: $AssemblyFile" }
if (!(Test-Path $JavaJar)) { throw "Missing Java reference JAR: $JavaJar" }
$javac = Join-Path $JavaHome "bin/javac.exe"
$javaExe = Join-Path $JavaHome "bin/java.exe"
if (!(Test-Path $javac) -or !(Test-Path $javaExe)) { throw "Missing JDK tools under: $JavaHome" }

cargo test --workspace
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

$rust = cargo run -q -p hic-core --bin hic-matrix-info -- $HicFile 1_1
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

$probeRoot = Join-Path $root "__artifacts_temp/java-probe"
New-Item -ItemType Directory -Force $probeRoot | Out-Null
& $javac --release 25 -encoding UTF-8 -cp $JavaJar -d $probeRoot (Join-Path $root "src/juicebox/tools/utils/dev/HiCReaderFingerprint.java")
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
$java = & $javaExe -cp "$JavaJar;$probeRoot" juicebox.tools.utils.dev.HiCReaderFingerprint $HicFile 1_1
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

$rustRows = @{}
foreach ($line in $rust) {
    if ($line -match 'bin_size=(\d+).*blocks=(\d+).*records=(\d+).*stored_count_sum=([0-9.]+).*fingerprint=([0-9a-f]+)') {
        $rustRows[$matches[1]] = @(
            [int64]$matches[2], [int64]$matches[3], [math]::Round([double]$matches[4]), $matches[5]
        )
    }
}
$javaRows = @{}
foreach ($line in $java) {
    if ($line -match 'bin_size=(\d+).*blocks=(\d+).*records=(\d+).*stored_count_sum=([0-9.]+).*fingerprint=([0-9a-f]+)') {
        $javaRows[$matches[1]] = @(
            [int64]$matches[2], [int64]$matches[3], [math]::Round([double]$matches[4]), $matches[5]
        )
    }
}

if ($rustRows.Count -eq 0 -or $rustRows.Count -ne $javaRows.Count) {
    throw "Rust/Java matrix comparison did not produce the same number of rows"
}
foreach ($binSize in $javaRows.Keys) {
    if (!$rustRows.ContainsKey($binSize) -or @(Compare-Object $rustRows[$binSize] $javaRows[$binSize]).Count -ne 0) {
        throw "Reader mismatch at $binSize bp: Rust=$($rustRows[$binSize] -join ',') Java=$($javaRows[$binSize] -join ',')"
    }
    Write-Output "Reader match: $binSize bp | blocks=$($javaRows[$binSize][0]) records=$($javaRows[$binSize][1]) counts=$($javaRows[$binSize][2]) fingerprint=$($javaRows[$binSize][3])"
}

cargo run -q -p assembly-core --bin assembly-info -- $AssemblyFile
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
Write-Output "Real-data verification passed."
