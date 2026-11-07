# Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
# SPDX-License-Identifier: Apache-2.0

param([int] $SetupProcess)
. C:\BenchProbe\common.ps1
$seed = Read-Seed 'BENCHPREP'
try {
    # SetupComplete launches this process and returns; shutdown is outside SetupComplete.
    Get-Process -Id $SetupProcess -ErrorAction SilentlyContinue | Wait-Process
    $query = "SELECT * FROM RegistryValueChangeEvent WHERE Hive='HKEY_LOCAL_MACHINE' AND KeyPath='SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Setup\\State' AND ValueName='ImageState'"
    Register-WmiEvent -Namespace root\default -Query $query -SourceIdentifier 'BenchSetup' | Out-Null
    try {
        while ((Get-ItemPropertyValue 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Setup\State' 'ImageState') -ne 'IMAGE_STATE_COMPLETE') {
            Wait-Event -SourceIdentifier 'BenchSetup' | Remove-Event
        }
    } finally {
        Unregister-Event -SourceIdentifier 'BenchSetup'
    }
    $admins = @(Get-LocalUser | Where-Object { $_.SID.Value -match '-500$' })
    if ($admins.Count -ne 1) { throw 'administrator-account' }
    $password = New-DiscardedPassword
    try { $admins[0] | Set-LocalUser -Password $password } finally { $password.Dispose() }
    Get-LocalUser | Disable-LocalUser
    Assert-Base
    # Copies of the installation password cannot authenticate after the reset and disable.
    # These removals reduce exposure; they are not a claim of secure disk erasure.
    foreach ($path in @('C:\Windows\Panther\unattend.xml', 'C:\Windows\Panther\Unattend\unattend.xml', 'C:\Windows\System32\Sysprep\unattend.xml')) {
        Remove-Item -LiteralPath $path -Force -ErrorAction SilentlyContinue
    }
    @{ schema = 1; password_invalidated = $true } | ConvertTo-Json | Set-Content -Encoding UTF8 -LiteralPath "$Root\prepared.json"
    $evidence = Read-GuestEvidence
    Write-BenchEvent 'prepared' $seed $evidence
    Stop-Computer -Force
} catch {
    Write-BenchEvent 'failed' $seed
    exit 1
}
