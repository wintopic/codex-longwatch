param(
    [string]$Target = "x86_64-pc-windows-msvc",
    [string]$Output = "dist",
    [string]$IsccPath = ""
)

$ErrorActionPreference = "Stop"
cargo build --locked --release --all-features --target $Target
$version = (Select-String -Path Cargo.toml -Pattern '^version = "([^"]+)"').Matches[0].Groups[1].Value
$outputDirectory = [IO.Path]::GetFullPath((Join-Path $PWD $Output))
$stage = [IO.Path]::GetFullPath((Join-Path $outputDirectory "Longwatch-$version-windows-x64"))
if (-not $stage.StartsWith($outputDirectory + [IO.Path]::DirectorySeparatorChar, [StringComparison]::OrdinalIgnoreCase)) {
    throw "Refusing to stage outside the requested output directory: $stage"
}
New-Item -ItemType Directory -Force -Path $outputDirectory | Out-Null
if (Test-Path -LiteralPath $stage) {
    Remove-Item -LiteralPath $stage -Recurse -Force
}
New-Item -ItemType Directory -Path $stage | Out-Null
Copy-Item "target/$Target/release/codex-longwatch.exe" (Join-Path $stage "codex-longwatch.exe")
Copy-Item "README.md" (Join-Path $stage "README.md")
Copy-Item "LICENSE" (Join-Path $stage "LICENSE")
$archive = Join-Path $outputDirectory "Longwatch-$version-windows-x64.zip"
Compress-Archive -Path "$stage/*" -DestinationPath $archive -Force

if ($IsccPath -and (Test-Path -LiteralPath $IsccPath)) {
    $env:LONGWATCH_VERSION = $version
    $env:LONGWATCH_EXE = (Resolve-Path "target/$Target/release/codex-longwatch.exe")
    & $IsccPath "packaging/windows/Longwatch.iss"
    if ($LASTEXITCODE -ne 0) {
        throw "Inno Setup failed with exit code $LASTEXITCODE"
    }
    $setup = Join-Path $PWD "Longwatch-$version-windows-x64-setup.exe"
    Copy-Item -LiteralPath $setup -Destination $outputDirectory -Force
}
