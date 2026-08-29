# Extracts the bundled archives into a scratch install directory so the Layer 3
# tests can run against real Apache, MySQL, PHP and phpMyAdmin binaries.
#
#   .\scripts\provision-test-stack.ps1 -Dest C:\Temp\rampp-test
#
# rampp.exe itself is copied on a best-effort basis only: Layer 3 tests drive the
# stack directly through rampp's library API (process::spawn_service, health
# checks, mysql_conf lifecycle) rather than launching the GUI binary, per the
# controller ruling that automating the egui window is out of scope. So a missing
# or stale target\release\rampp.exe never blocks provisioning.
param(
    [Parameter(Mandatory = $true)][string]$Dest
)

$ErrorActionPreference = 'Stop'
$sources = Join-Path $PSScriptRoot '..\sources'

New-Item -ItemType Directory -Force -Path $Dest | Out-Null

function Expand-Into {
    param([string]$Zip, [string]$Target, [string]$StripPrefix)
    $staging = Join-Path $env:TEMP ([System.Guid]::NewGuid().ToString())
    Expand-Archive -Path $Zip -DestinationPath $staging -Force
    $root = if ($StripPrefix) { Join-Path $staging $StripPrefix } else { $staging }
    New-Item -ItemType Directory -Force -Path $Target | Out-Null
    Copy-Item -Path (Join-Path $root '*') -Destination $Target -Recurse -Force
    Remove-Item -Recurse -Force $staging
}

Expand-Into (Get-ChildItem "$sources\httpd-*.zip"       | Select-Object -First 1).FullName (Join-Path $Dest 'apache')     'Apache24'
Expand-Into (Get-ChildItem "$sources\mysql-*.zip"       | Select-Object -First 1).FullName (Join-Path $Dest 'mysql')      'mysql-9.7.0-winx64'
Expand-Into (Get-ChildItem "$sources\php-*.zip"         | Select-Object -First 1).FullName (Join-Path $Dest 'php')        ''
Expand-Into (Get-ChildItem "$sources\phpMyAdmin-*.zip"  | Select-Object -First 1).FullName (Join-Path $Dest 'phpmyadmin') 'phpMyAdmin-5.2.3-english'

# The official Apache Lounge zip ships its own working sample conf at exactly
# apache\conf\httpd.conf. Neither the MySQL nor PHP zip ships an equivalent
# my.ini/php.ini, so this collision is Apache-only.
#
# rampp's config reconciler (src/provision.rs `is_rampp_owned`) is marker-gated
# by design: a config file without the "# RAMPP -- generated ..." marker is
# treated as user-owned and is never touched. On a genuinely fresh extraction
# that vendor sample file has no marker, so the reconciler leaves it in place
# instead of writing rampp's generated httpd.conf -- Apache then starts with
# none of RAMPP's health endpoint, PHP proxy or configured port. Removing the
# vendor sample here reproduces what a truly empty install_dir looks like, so
# the Layer 3 tests exercise rampp's OWN generated conf rather than Apache's.
Remove-Item -Force -ErrorAction SilentlyContinue (Join-Path $Dest 'apache\conf\httpd.conf')

$ramppExe = Join-Path $PSScriptRoot '..\target\release\rampp.exe'
if (Test-Path $ramppExe) {
    Copy-Item -Path $ramppExe -Destination $Dest -Force
} else {
    Write-Warning "rampp.exe not found at $ramppExe - skipping (Layer 3 tests do not launch it)"
}

Write-Host "Stack provisioned at $Dest"
