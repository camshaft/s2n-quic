# Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
# SPDX-License-Identifier: Apache-2.0

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$Root = 'C:\BenchProbe'

function Read-Seed([string] $Label) {
    $disks = @(Get-CimInstance Win32_LogicalDisk | Where-Object { $_.VolumeName -eq $Label -and $_.DriveType -eq 5 })
    if ($disks.Count -ne 1) { throw 'seed-volume' }
    $path = $disks[0].DeviceID + '\'
    $value = Get-Content -Raw -LiteralPath ($path + 'seed.json') | ConvertFrom-Json
    if ($value.schema -ne 1 -or $value.nonce -notmatch '^[a-f0-9]{32}$') { throw 'seed-record' }
    return @{ Path = $path; Value = $value }
}

function Write-BenchEvent([string] $Phase, $Seed, [hashtable] $Fields = @{}) {
    $record = @{ schema = 1; phase = $Phase; nonce = $Seed.Value.nonce }
    foreach ($key in $Fields.Keys) { $record[$key] = $Fields[$key] }
    $serial = New-Object System.IO.Ports.SerialPort 'COM1',115200,'None',8,'One'
    try {
        $serial.Open()
        $serial.WriteLine(($record | ConvertTo-Json -Compress))
        $serial.BaseStream.Flush()
    } finally {
        $serial.Dispose()
    }
}

function New-DiscardedPassword {
    $bytes = New-Object byte[] 32
    $rng = [System.Security.Cryptography.RandomNumberGenerator]::Create()
    try { $rng.GetBytes($bytes) } finally { $rng.Dispose() }
    $text = 'Aa1!' + [Convert]::ToBase64String($bytes)
    $secure = ConvertTo-SecureString $text -AsPlainText -Force
    [Array]::Clear($bytes, 0, $bytes.Length)
    $text = $null
    return $secure
}

function Assert-Base {
    if (@(Get-LocalUser | Where-Object Enabled).Count -ne 0) { throw 'enabled-base-account' }
    $service = Get-CimInstance Win32_Service -Filter "Name='sshd'"
    if ($null -eq $service -or $service.State -ne 'Stopped' -or $service.StartMode -ne 'Disabled') { throw 'base-ssh-state' }
    if (Test-Path 'C:\ProgramData\ssh') {
        if (@(Get-ChildItem 'C:\ProgramData\ssh' -Filter '*key*' -Recurse -Force).Count -ne 0) { throw 'base-ssh-keys' }
    }
    if (Test-Path 'C:\ProgramData\ssh\administrators_authorized_keys') { throw 'base-authorized-keys' }
}

function Read-GuestEvidence {
    $imageState = Get-ItemPropertyValue 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Setup\State' 'ImageState'
    if ($imageState -ne 'IMAGE_STATE_COMPLETE') { throw 'setup-incomplete' }
    $prepared = Get-Content -Raw -LiteralPath "$Root\prepared.json" | ConvertFrom-Json
    if ($prepared.schema -ne 1 -or $prepared.password_invalidated -ne $true) { throw 'preparation-record' }
    $edition = Get-ItemPropertyValue 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion' 'EditionID'
    $source = Get-Content -Raw -LiteralPath "$Root\source.json" | ConvertFrom-Json
    if ($edition -ne $source.edition -or $edition -notmatch 'Eval$') { throw 'edition' }
    $licenses = @(Get-CimInstance SoftwareLicensingProduct | Where-Object {
        $_.ApplicationID -eq '55c92734-d682-4d71-983e-d6ec3f16059f' -and $_.PartialProductKey -and -not $_.LicenseIsAddon
    })
    if ($licenses.Count -ne 1) { throw 'license-record' }
    $license = $licenses[0]
    if (-not ($license.LicenseStatus -eq 1 -or ($license.LicenseStatus -eq 2 -and $license.GracePeriodRemaining -gt 0))) { throw 'license-not-usable' }
    return @{
        setup_complete = $true
        password_invalidated = $true
        base_accounts_disabled = $true
        base_host_keys_absent = $true
        edition = $edition
        license_status = [int] $license.LicenseStatus
        grace_minutes = [int] $license.GracePeriodRemaining
    }
}

function Protect-SSHFile([string] $Path) {
    & icacls.exe $Path /inheritance:r /grant:r '*S-1-5-18:F' '*S-1-5-32-544:F' | Out-Null
    if ($LASTEXITCODE -ne 0) { throw 'file-permissions' }
    & icacls.exe $Path /setowner '*S-1-5-18' | Out-Null
    if ($LASTEXITCODE -ne 0) { throw 'file-owner' }
}
