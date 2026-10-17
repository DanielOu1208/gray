$env:GRAY_INSTALL_PLUGINS = '0'
& ([scriptblock]::Create((Invoke-RestMethod 'https://gray.alignment.id/install.ps1'))) @args
