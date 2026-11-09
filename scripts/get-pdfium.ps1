# Downloads a prebuilt pdfium for lumo (Windows x64) into the next to the exe.
$REV = "chromium_7350"
$ARCH = "windows-x64"
$URL = "https://github.com/bblanchon/pdfium-binaries/releases/download/$REV/pdfium-$ARCH-tglc.tgz"

Write-Host "Fetching $URL"
$out = Join-Path $env:TEMP "pdfium.tgz"
Invoke-WebRequest -Uri $URL -OutFile $out

# tgz: use tar shipped with Win10+
tar -xzf $out -C $env:TEMP\pdfium-extract 2>$null
if ($LASTEXITCODE -ne 0) {
    New-Item -ItemType Directory -Force "$env:TEMP\pdfium-extract" | Out-Null
    tar -xzf $out -C "$env:TEMP\pdfium-extract"
}

$dest = Split-Path -Parent $PSScriptRoot   # project root
Copy-Item "$env:TEMP\pdfium-extract\bin\pdfium.dll" "$dest\pdfium.dll" -Force
Write-Host "Done: $dest\pdfium.dll"
