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
& $javac --release 25 -encoding UTF-8 -cp $JavaJar -d $probeRoot (Join-Path $root "src/juicebox/tools/utils/dev/HiCNormalizationFingerprint.java")
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
& $javac --release 25 -encoding UTF-8 -cp $JavaJar -d $probeRoot (Join-Path $root "src/juicebox/tools/utils/dev/HiCExpectedFingerprint.java")
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
& $javac --release 25 -encoding UTF-8 -cp $JavaJar -d $probeRoot (Join-Path $root "src/juicebox/tools/utils/dev/HiCExpectedFingerprint.java")
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
$java = & $javaExe -cp "$JavaJar;$probeRoot" juicebox.tools.utils.dev.HiCReaderFingerprint $HicFile 1_1
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

$rustRows = @{}
foreach ($line in $rust) {
    if ($line -match 'norm=(\S+).*bin_size=(\d+).*blocks=(\d+).*records=(\d+).*finite=(\d+).*stored_count_sum=([0-9.Ee+-]+).*fingerprint=([0-9a-f]+)') {
        $key = "$($matches[1])_$($matches[2])"
        $rustRows[$key] = @(
            [int64]$matches[3], [int64]$matches[4], [int64]$matches[5], [math]::Round([double]$matches[6], 6), $matches[7]
        )
    }
}
$javaRows = @{}
foreach ($line in $java) {
    if ($line -match 'norm=(\S+).*bin_size=(\d+).*blocks=(\d+).*records=(\d+).*finite=(\d+).*stored_count_sum=([0-9.Ee+-]+).*fingerprint=([0-9a-f]+)') {
        $key = "$($matches[1])_$($matches[2])"
        $javaRows[$key] = @(
            [int64]$matches[3], [int64]$matches[4], [int64]$matches[5], [math]::Round([double]$matches[6], 6), $matches[7]
        )
    }
}

if ($rustRows.Count -eq 0 -or $rustRows.Count -ne $javaRows.Count) {
    throw "Rust/Java matrix comparison did not produce the same number of rows"
}
foreach ($key in $javaRows.Keys) {
    if (!$rustRows.ContainsKey($key) -or @(Compare-Object $rustRows[$key] $javaRows[$key]).Count -ne 0) {
        throw "Reader mismatch at ${key}: Rust=$($rustRows[$key] -join ',') Java=$($javaRows[$key] -join ',')"
    }
    Write-Output "Reader match: $key | blocks=$($javaRows[$key][0]) records=$($javaRows[$key][1]) finite=$($javaRows[$key][2]) fingerprint=$($javaRows[$key][4])"
}

$rustOeRows = @{}
foreach ($line in $rust) {
    if ($line -match 'oe_norm=(\S+).*bin_size=(\d+).*records=(\d+).*finite=(\d+).*sum=([0-9.Ee+-]+).*fingerprint=([0-9a-f]+)') {
        $key = "$($matches[1])_$($matches[2])"
        $rustOeRows[$key] = @(
            [int64]$matches[3], [int64]$matches[4], [math]::Round([double]$matches[5], 6), $matches[6]
        )
    }
}
$javaOeRows = @{}
foreach ($line in $java) {
    if ($line -match 'oe_norm=(\S+).*bin_size=(\d+).*records=(\d+).*finite=(\d+).*sum=([0-9.Ee+-]+).*fingerprint=([0-9a-f]+)') {
        $key = "$($matches[1])_$($matches[2])"
        $javaOeRows[$key] = @(
            [int64]$matches[3], [int64]$matches[4], [math]::Round([double]$matches[5], 6), $matches[6]
        )
    }
}
if ($rustOeRows.Count -eq 0 -or $rustOeRows.Count -ne $javaOeRows.Count) {
    throw "Rust/Java O/E comparison did not produce the same number of rows"
}
foreach ($key in $javaOeRows.Keys) {
    if (!$rustOeRows.ContainsKey($key) -or @(Compare-Object $rustOeRows[$key] $javaOeRows[$key]).Count -ne 0) {
        throw "O/E mismatch at ${key}: Rust=$($rustOeRows[$key] -join ',') Java=$($javaOeRows[$key] -join ',')"
    }
    Write-Output "O/E match: $key | records=$($javaOeRows[$key][0]) fingerprint=$($javaOeRows[$key][3])"
}

$rustNorm = cargo run -q -p hic-core --bin hic-normalization-info -- $HicFile
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
$javaNorm = & $javaExe -cp "$JavaJar;$probeRoot" juicebox.tools.utils.dev.HiCNormalizationFingerprint $HicFile
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
$rustNormRows = @{}
foreach ($line in $rustNorm) {
    if ($line -match 'type=(\S+) chr=(\d+) unit=(\S+) resolution=(\d+) values=(\d+) finite=(\d+) sum=([0-9.Ee+-]+) fingerprint=([0-9a-f]+)') {
        $key = "$($matches[1])_$($matches[2])_$($matches[3])_$($matches[4])"
        $rustNormRows[$key] = @([int64]$matches[5], [int64]$matches[6], [math]::Round([double]$matches[7], 9), $matches[8])
    }
}
$javaNormRows = @{}
foreach ($line in $javaNorm) {
    if ($line -match 'type=(\S+) chr=(\d+) unit=(\S+) resolution=(\d+) values=(\d+) finite=(\d+) sum=([0-9.Ee+-]+) fingerprint=([0-9a-f]+)') {
        $key = "$($matches[1])_$($matches[2])_$($matches[3])_$($matches[4])"
        $javaNormRows[$key] = @([int64]$matches[5], [int64]$matches[6], [math]::Round([double]$matches[7], 9), $matches[8])
    }
}
if ($rustNormRows.Count -eq 0 -or $rustNormRows.Count -ne $javaNormRows.Count) {
    throw "Rust/Java normalization comparison did not produce the same number of rows"
}
foreach ($key in $javaNormRows.Keys) {
    if (!$rustNormRows.ContainsKey($key) -or @(Compare-Object $rustNormRows[$key] $javaNormRows[$key]).Count -ne 0) {
        throw "Normalization mismatch at ${key}: Rust=$($rustNormRows[$key] -join ',') Java=$($javaNormRows[$key] -join ',')"
    }
    Write-Output "Normalization match: $key | values=$($javaNormRows[$key][0]) finite=$($javaNormRows[$key][1]) fingerprint=$($javaNormRows[$key][3])"
}

$rustExpected = cargo run -q -p hic-core --bin hic-expected-info -- $HicFile
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
$javaExpected = & $javaExe -cp "$JavaJar;$probeRoot" juicebox.tools.utils.dev.HiCExpectedFingerprint $HicFile
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
$rustExpectedRows = @{}
foreach ($line in $rustExpected) {
    if ($line -match 'type=(\S+) unit=(\S+) resolution=(\d+) values=(\d+) finite=(\d+) sum=([0-9.Ee+-]+) factors=(\d+) factor_chr1=([0-9.Ee+-]+) value0=([0-9.Ee+-]+) past_end=([0-9.Ee+-]+) fingerprint=([0-9a-f]+)') {
        $key = "$($matches[1])_$($matches[2])_$($matches[3])"
        $rustExpectedRows[$key] = @(
            [int64]$matches[4], [int64]$matches[5], [math]::Round([double]$matches[6], 9),
            [int64]$matches[7], [math]::Round([double]$matches[8], 12),
            [math]::Round([double]$matches[9], 9), [math]::Round([double]$matches[10], 9), $matches[11]
        )
    }
}
$javaExpectedRows = @{}
foreach ($line in $javaExpected) {
    if ($line -match 'type=(\S+) unit=(\S+) resolution=(\d+) values=(\d+) finite=(\d+) sum=([0-9.Ee+-]+) factors=(\d+) factor_chr1=([0-9.Ee+-]+) value0=([0-9.Ee+-]+) past_end=([0-9.Ee+-]+) fingerprint=([0-9a-f]+)') {
        $key = "$($matches[1])_$($matches[2])_$($matches[3])"
        $javaExpectedRows[$key] = @(
            [int64]$matches[4], [int64]$matches[5], [math]::Round([double]$matches[6], 9),
            [int64]$matches[7], [math]::Round([double]$matches[8], 12),
            [math]::Round([double]$matches[9], 9), [math]::Round([double]$matches[10], 9), $matches[11]
        )
    }
}
if ($rustExpectedRows.Count -eq 0 -or $rustExpectedRows.Count -ne $javaExpectedRows.Count) {
    throw "Rust/Java expected-value comparison did not produce the same number of rows"
}
foreach ($key in $javaExpectedRows.Keys) {
    if (!$rustExpectedRows.ContainsKey($key) -or @(Compare-Object $rustExpectedRows[$key] $javaExpectedRows[$key]).Count -ne 0) {
        throw "Expected-value mismatch at ${key}: Rust=$($rustExpectedRows[$key] -join ',') Java=$($javaExpectedRows[$key] -join ',')"
    }
    Write-Output "Expected match: $key | values=$($javaExpectedRows[$key][0]) factors=$($javaExpectedRows[$key][3]) fingerprint=$($javaExpectedRows[$key][7])"
}

cargo run -q -p assembly-core --bin assembly-info -- $AssemblyFile
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
Write-Output "Real-data verification passed."
