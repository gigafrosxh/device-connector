# Authenticode-Signatur für eine oder mehrere Dateien, danach Prüfung.
# Braucht WINDOWS_CERT_PFX (base64) und WINDOWS_CERT_PASSWORD als
# Umgebungsvariablen; optional WINDOWS_TIMESTAMP_URL.
# Wird direkt aufgerufen und von Inno Setup (SignTool=dcsign) für Setup und
# Deinstallationsprogramm, deshalb stehen keine Geheimnisse auf der Kommandozeile.
param([Parameter(Mandatory = $true, ValueFromRemainingArguments = $true)][string[]]$Files)
$ErrorActionPreference = 'Stop'

if (-not $env:WINDOWS_CERT_PFX -or -not $env:WINDOWS_CERT_PASSWORD) {
    throw 'WINDOWS_CERT_PFX/WINDOWS_CERT_PASSWORD fehlen'
}
$ts = if ($env:WINDOWS_TIMESTAMP_URL) { $env:WINDOWS_TIMESTAMP_URL } else { 'http://timestamp.digicert.com' }

$signtool = Get-ChildItem "${env:ProgramFiles(x86)}\Windows Kits\10\bin" -Recurse -Filter signtool.exe |
    Where-Object { $_.FullName -match '\\x64\\' } | Sort-Object FullName | Select-Object -Last 1
if (-not $signtool) { throw 'signtool.exe nicht gefunden (Windows SDK)' }

$tmp = if ($env:RUNNER_TEMP) { $env:RUNNER_TEMP } else { [IO.Path]::GetTempPath() }
$pfx = Join-Path $tmp ("dc-cert-" + [guid]::NewGuid() + ".pfx")
[IO.File]::WriteAllBytes($pfx, [Convert]::FromBase64String($env:WINDOWS_CERT_PFX))
try {
    foreach ($f in $Files) {
        & $signtool.FullName sign /fd sha256 /tr $ts /td sha256 /f $pfx /p $env:WINDOWS_CERT_PASSWORD $f
        if ($LASTEXITCODE -ne 0) { throw "Signieren fehlgeschlagen: $f" }
        & $signtool.FullName verify /pa /v $f
        if ($LASTEXITCODE -ne 0) { throw "Signatur ungültig: $f" }
    }
} finally {
    Remove-Item -Force $pfx -ErrorAction SilentlyContinue
}
