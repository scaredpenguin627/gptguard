param(
    [string]$Destination = (Join-Path $PSScriptRoot "..\.build\mihomo")
)

$ErrorActionPreference = "Stop"
$lock = Get-Content (Join-Path $PSScriptRoot "mihomo.lock.json") -Raw | ConvertFrom-Json
$destination = [IO.Path]::GetFullPath($Destination)
$zip = Join-Path $destination $lock.asset
$extract = Join-Path $destination "extract"

New-Item -ItemType Directory -Force -Path $destination | Out-Null
if (-not (Test-Path $zip)) {
    $curl = Get-Command curl.exe -ErrorAction SilentlyContinue
    if ($null -ne $curl) {
        & $curl.Source -L --fail --retry 4 --retry-all-errors --output $zip $lock.url
        if ($LASTEXITCODE -ne 0) {
            throw "download failed with curl exit code $LASTEXITCODE"
        }
    } else {
        Invoke-WebRequest -Uri $lock.url -OutFile $zip
    }
}
$actual = (Get-FileHash -Algorithm SHA256 $zip).Hash.ToLowerInvariant()
if ($actual -ne $lock.sha256.ToLowerInvariant()) {
    Remove-Item -LiteralPath $zip -Force
    throw "Mihomo archive hash mismatch: expected $($lock.sha256), got $actual; rerun the fetch step"
}
if (Test-Path $extract) {
    Remove-Item -Recurse -Force -LiteralPath $extract
}
Expand-Archive -LiteralPath $zip -DestinationPath $extract -Force
$binary = Get-ChildItem -LiteralPath $extract -File -Recurse |
    Where-Object { $_.Name -in @("mihomo.exe", "mihomo-windows-amd64.exe") } |
    Select-Object -First 1
if ($null -eq $binary) {
    throw "mihomo.exe was not found in $zip"
}
$target = Join-Path $destination "mihomo.exe"
Copy-Item -LiteralPath $binary.FullName -Destination $target -Force | Out-Null
Write-Output $target
