# THOTH_INSTALL_VERSION: vX.Y.Z release tag; unset selects latest.
# THOTH_INSTALL_DIR: destination directory; default $env:USERPROFILE\.local\bin.
# THOTH_INSTALL_NO_SETUP: 1 skips guided setup.
# THOTH_INSTALL_BASE_URL: releases URL or local release-layout directory.
# Do not set THOTH_LOGIN_SKIP_TTY_CHECK; setup needs a real terminal.
# Offline: put the asset and SHA256SUMS in RELEASE/latest/download, then set
# $env:THOTH_INSTALL_BASE_URL='./RELEASE'; $env:THOTH_INSTALL_DIR='./BIN'; $env:THOTH_INSTALL_NO_SETUP='1'; & ./install.ps1
# 實測待確認: https://raw.githubusercontent.com/johnny-lo/Thoth/master/install.ps1 returns 200, UTF-8 without BOM; check curl -fsSI and bytes; fix root placement/README branch URL if needed (also check install.sh).
# 實測待確認: latest/download redirects work (including Invoke-WebRequest); test isolated installs on Windows and WSL; fallback: API tag_name then download/<tag>.
# 實測待確認: curl | sh setup reads /dev/tty in WSL; fallback: print thoth setup for manual execution.
# 實測待確認: PowerShell 5.1 irm | iex completes and adds user PATH; test isolated directory, new terminal, then remove PATH entry; fix reported 5.1 syntax errors.
& {
    Set-StrictMode -Version 2.0
    $ErrorActionPreference = 'Stop'
    $architecture = $env:PROCESSOR_ARCHITECTURE
    if ($env:PROCESSOR_ARCHITEW6432) { $architecture = $env:PROCESSOR_ARCHITEW6432 }
    if ([Environment]::OSVersion.Platform -ne 'Win32NT' -or
        -not [Environment]::Is64BitOperatingSystem -or $architecture -ne 'AMD64') {
        throw 'Unsupported platform; use cargo install --git https://github.com/johnny-lo/Thoth --locked --bin thoth'
    }
    $base = $env:THOTH_INSTALL_BASE_URL
    if (-not $base) { $base = 'https://github.com/johnny-lo/Thoth/releases' }
    $version = $env:THOTH_INSTALL_VERSION
    if ($version) {
        if ($version -cnotmatch '^v[0-9]+\.[0-9]+\.[0-9]+\z') { throw 'Expected THOTH_INSTALL_VERSION=vX.Y.Z' }
        $release = "download/$version"
    } else { $release = 'latest/download' }
    $remote = $base -match '^https?://'
    $asset = 'thoth-x86_64-pc-windows-msvc.exe'
    [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
    $tmp = Join-Path ([IO.Path]::GetTempPath()) ('thoth-install-' + [Guid]::NewGuid().ToString('N'))
    try {
        $null = New-Item -ItemType Directory -Path $tmp
        foreach ($name in @('SHA256SUMS', $asset)) {
            $destination = Join-Path $tmp $name
            if ($remote) {
                Invoke-WebRequest -UseBasicParsing -Uri ($base.TrimEnd('/') + "/$release/$name") -OutFile $destination
            } else {
                Copy-Item -LiteralPath (Join-Path $base "$release/$name") -Destination $destination
            }
        }
        $hashes = @(foreach ($line in [IO.File]::ReadAllLines((Join-Path $tmp 'SHA256SUMS'))) {
            if ($line -match ('^([0-9a-fA-F]{64}) [ *]' + [regex]::Escape($asset) + '$')) { $Matches[1] }
        })
        if ($hashes.Count -ne 1) { throw "Missing or invalid SHA256SUMS entry for $asset" }
        $expected = $hashes[0]
        $download = Join-Path $tmp $asset
        $actual = (Get-FileHash -LiteralPath $download -Algorithm SHA256).Hash
        if ($expected -ine $actual) { throw "SHA256 mismatch: expected $expected; actual $actual" }

        $dir = $env:THOTH_INSTALL_DIR
        if (-not $dir) { $dir = Join-Path $env:USERPROFILE '.local\bin' }
        $dir = [IO.Path]::GetFullPath($dir)
        $null = New-Item -ItemType Directory -Path $dir -Force
        Get-ChildItem -LiteralPath $dir -Filter 'thoth.old-*.exe' -File | ForEach-Object {
            Remove-Item -LiteralPath $_.FullName -Force -ErrorAction SilentlyContinue
        }
        $target = Join-Path $dir 'thoth.exe'
        try {
            Copy-Item -LiteralPath $download -Destination $target -Force
        } catch {
            if (-not (Test-Path -LiteralPath $target -PathType Leaf)) { throw }
            $oldName = 'thoth.old-' + (Get-Date -Format 'yyyyMMdd-HHmmss') + '.exe'
            # More than one locked upgrade can occur during the same second.
            if (Test-Path -LiteralPath (Join-Path $dir $oldName)) {
                $oldName = 'thoth.old-' + (Get-Date -Format 'yyyyMMdd-HHmmss') + '-' + [Guid]::NewGuid().ToString('N') + '.exe'
            }
            try { Rename-Item -LiteralPath $target -NewName $oldName } catch {
                throw "Cannot rename the old executable: $($_.Exception.Message)"
            }
            Write-Output "Old executable renamed to $oldName; delete it after the sessions using it close."
            Copy-Item -LiteralPath $download -Destination $target -Force
        }
        & $target --version
        if ($LASTEXITCODE -ne 0) { throw 'Installed thoth --version failed' }

        function Test-InstallPath([string] $Value) {
            foreach ($entry in ($Value -split ';')) {
                $expanded = [Environment]::ExpandEnvironmentVariables($entry.Trim().Trim('"'))
                if ($expanded.TrimEnd('\', '/') -ieq $dir.TrimEnd('\', '/')) { return $true }
            }
            return $false
        }
        $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
        if (-not (Test-InstallPath $userPath) -and -not (Test-InstallPath $env:Path)) {
            $newPath = $dir
            if ($userPath) { $newPath = $userPath.TrimEnd(';') + ';' + $dir }
            [Environment]::SetEnvironmentVariable('Path', $newPath, 'User')
            $env:Path = $env:Path.TrimEnd(';') + ';' + $dir
            Write-Output 'PATH updated; new terminals will pick up the change.'
        }
        if ($env:THOTH_INSTALL_NO_SETUP -eq '1' -or [Console]::IsInputRedirected -or [Console]::IsOutputRedirected) {
            Write-Output 'Run later: thoth setup'
            if ($env:THOTH_HOME) { Write-Output "Keep THOTH_HOME set to $env:THOTH_HOME when running thoth setup." }
        } else {
            # A setup failure does not undo a completed installation.
            try {
                & $target setup
                if ($LASTEXITCODE -ne 0) { Write-Output 'Setup still has failing items; rerun thoth setup' }
            } catch { Write-Output 'Setup still has failing items; rerun thoth setup' }
        }
    } finally {
        if (Test-Path -LiteralPath $tmp) { Remove-Item -LiteralPath $tmp -Recurse -Force }
    }
}
