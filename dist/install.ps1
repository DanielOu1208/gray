[CmdletBinding(DefaultParameterSetName='Native')]
param(

    [Parameter(ParameterSetName='Native')][switch]$Native,
    [Parameter(Mandatory=$true, ParameterSetName='Wsl')][switch]$Wsl,
    [Parameter(ParameterSetName='Native')][ValidateSet('stable', 'beta')][string]$Channel = 'stable',
    [Parameter(ParameterSetName='Native')][string]$InstallDir = $env:GRAY_INSTALL_DIR,
    [Parameter(ParameterSetName='Native')][switch]$NoPath,
    [Parameter(ParameterSetName='Native')][string]$ArchivePath,
    [Parameter(ParameterSetName='Native')][string]$Sha256,
    [Parameter(ParameterSetName='Native')][uri]$BaseUri = 'https://gray.alignment.id/dl/'
)

$ErrorActionPreference = 'Stop'
if ($PSCmdlet.ParameterSetName -ne 'Wsl') {

    # Native is the default route. Reuse the tested native implementation and
    # never fall back to WSL on failure.
    $installer = $null
    if ($PSScriptRoot) {
        $beside = Join-Path $PSScriptRoot 'install-native.ps1'
        if (Test-Path -LiteralPath $beside -PathType Leaf) { $installer = $beside }
    }
    if (-not $installer) {
        if ($BaseUri.Scheme -ne 'https') { throw 'Downloads require HTTPS; keep install-native.ps1 beside install.ps1 for offline installs' }

        [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
        $source = New-Object Uri($BaseUri, '../install-native.ps1')
        $file = Join-Path ([IO.Path]::GetTempPath()) ('gray-install-native-' + [guid]::NewGuid().ToString('N') + '.txt')
        try {
            Invoke-WebRequest -UseBasicParsing -Uri $source -OutFile $file -TimeoutSec 60

            $installer = [scriptblock]::Create([IO.File]::ReadAllText($file))
        } finally { if ([IO.File]::Exists($file)) { [IO.File]::Delete($file) } }
    }
    & $installer -Channel $Channel -InstallDir $InstallDir -NoPath:$NoPath -ArchivePath $ArchivePath -Sha256 $Sha256 -BaseUri $BaseUri
    return
}

Write-Host ""
Write-Host "  gray installer" -ForegroundColor Cyan
Write-Host "  --------------"

$wsl = Get-Command wsl.exe -ErrorAction SilentlyContinue
if (-not $wsl) {
    Write-Host ""
    Write-Host "  -Wsl was requested, and WSL is not installed." -ForegroundColor Yellow
    Write-Host "  Install it with:   wsl --install" -ForegroundColor Yellow
    Write-Host "  (reboot, then re-run this installer)"
    Write-Host "  Or drop -Wsl: gray installs natively on Windows without it." -ForegroundColor Yellow
    throw 'WSL is not installed'
}

$distros = & wsl.exe -l -q 2>$null | Where-Object { $_ -and ($_ -replace "`0","").Trim() }
if (-not $distros) {
    Write-Host "  No WSL distro found. Installing Ubuntu (default)..."
    & wsl.exe --install -d Ubuntu
    Write-Host "  Reboot if prompted, then re-run this installer."
    return
}

Write-Host "  -> installing gray into WSL ($($distros[0]))..."

$envArgs = @()
foreach ($kv in @('GRAY_INSTALL_PLUGINS', 'GRAY_PLUGINS', 'GRAY_TELEMETRY', 'GRAY_CDN_URL')) {
    $v = [Environment]::GetEnvironmentVariable($kv, 'Process')
    if ($v -and $v -match '^[\w./:+ -]+$') { $envArgs += "$kv=$v" }
    elseif ($v) { Write-Warning "$kv contains characters unsafe to forward into WSL; dropping it" }
}
if ($envArgs.Count -gt 0) { $envArgs = @('env') + $envArgs }

& wsl.exe -e @envArgs sh -c "curl -fsSL https://gray.alignment.id/install.sh | sh"
if ($LASTEXITCODE -ne 0) {

    & wsl.exe -e @envArgs sh -c "sudo apt-get update -qq && sudo apt-get install -y -qq curl >/dev/null && curl -fsSL https://gray.alignment.id/install.sh | sh"
}

Write-Host ""
Write-Host "  Done. To use gray:" -ForegroundColor Green
Write-Host "    1. open a WSL terminal (or: wsl)"
Write-Host "    2. export GRAY_API_KEY=sk-or-...      # openrouter.ai / deepseek.com key"
Write-Host "    3. export GRAY_MODEL=deepseek/deepseek-chat"
Write-Host "    4. run: gray"
