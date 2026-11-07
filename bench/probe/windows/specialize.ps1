# Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
# SPDX-License-Identifier: Apache-2.0

. C:\BenchProbe\common.ps1
$seed = Read-Seed 'BENCHPREP'
try {
    $capability = Get-WindowsCapability -Online -Name 'OpenSSH.Server~~~~0.0.1.0'
    if ($capability.State -ne 'Installed') { throw 'ssh-not-installed' }
    # Server 2025 includes OpenSSH; never start it on the disk that will be cached.
    Stop-Service sshd
    Set-Service sshd -StartupType Disabled
    New-Item -ItemType Directory -Force 'C:\ProgramData\ssh' | Out-Null
    if (@(Get-ChildItem 'C:\ProgramData\ssh' -Filter '*key*' -Recurse -Force).Count -ne 0) { throw 'unexpected-keys' }
    Copy-Item -LiteralPath ($seed.Path + 'seed.json') -Destination "$Root\source.json"
    Copy-Item -LiteralPath "$Root\sshd_config" -Destination 'C:\ProgramData\ssh\sshd_config'
    $action = New-ScheduledTaskAction -Execute 'powershell.exe' -Argument '-NoProfile -NonInteractive -File C:\BenchProbe\boot.ps1'
    $trigger = New-ScheduledTaskTrigger -AtStartup
    $principal = New-ScheduledTaskPrincipal -UserId 'SYSTEM' -LogonType ServiceAccount -RunLevel Highest
    Register-ScheduledTask -TaskName 'BenchProbe' -Action $action -Trigger $trigger -Principal $principal | Out-Null
} catch {
    Write-BenchEvent 'failed' $seed
    exit 1
}
