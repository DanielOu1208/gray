$prev = $env:GRAY_INSTALL_PLUGINS
$env:GRAY_INSTALL_PLUGINS = '0'
try {
    $site = if ($env:GRAY_CDN_URL) { $env:GRAY_CDN_URL -replace '/dl/?$', '' } else { 'https://gray.alignment.id' }
    & ([scriptblock]::Create((Invoke-RestMethod "$site/install.ps1"))) @args
} finally {
    if ($null -eq $prev) { Remove-Item Env:\GRAY_INSTALL_PLUGINS -ErrorAction SilentlyContinue }
    else { $env:GRAY_INSTALL_PLUGINS = $prev }
}
