# Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
# SPDX-License-Identifier: Apache-2.0

. C:\BenchProbe\common.ps1
# The preparation disk boots without a verification seed; only an overlay receives one.
if (@(Get-CimInstance Win32_LogicalDisk | Where-Object { $_.VolumeName -eq 'BENCHRUN' -and $_.DriveType -eq 5 }).Count -eq 0) { exit 0 }
$seed = Read-Seed 'BENCHRUN'
try {
    Assert-Base
    $evidence = Read-GuestEvidence
    if (Get-LocalUser -Name 'benchprobe' -ErrorAction SilentlyContinue) { throw 'existing-probe-user' }
    $password = New-DiscardedPassword
    try { New-LocalUser -Name 'benchprobe' -Password $password | Out-Null } finally { $password.Dispose() }
    Add-LocalGroupMember -SID 'S-1-5-32-544' -Member 'benchprobe'
    $ssh = 'C:\ProgramData\ssh'
    Copy-Item -LiteralPath ($seed.Path + 'host_key') -Destination "$ssh\bench_host_ed25519_key"
    Copy-Item -LiteralPath ($seed.Path + 'client.pub') -Destination "$ssh\administrators_authorized_keys"
    Protect-SSHFile "$ssh\bench_host_ed25519_key"
    Protect-SSHFile "$ssh\administrators_authorized_keys"
    # Keep the firewall enabled and restrict this rule to QEMU's user-network gateway.
    if (@(Get-NetFirewallProfile | Where-Object { -not $_.Enabled }).Count -ne 0) { throw 'firewall-disabled' }
    Get-NetFirewallRule -Name 'OpenSSH-Server-In-TCP' -ErrorAction SilentlyContinue | Disable-NetFirewallRule
    New-NetFirewallRule -Name 'BenchProbeSSH' -Direction Inbound -Action Allow -Protocol TCP -LocalPort 22 -RemoteAddress '10.0.2.2' | Out-Null
    & 'C:\Windows\System32\OpenSSH\sshd.exe' -t -f "$ssh\sshd_config"
    if ($LASTEXITCODE -ne 0) { throw 'ssh-configuration' }
    Set-Service sshd -StartupType Manual
    Start-Service sshd
    (Get-Service sshd).WaitForStatus('Running')
    # One local connection confirms that the listener, not only the service, is ready.
    $connection = New-Object System.Net.Sockets.TcpClient
    try { $connection.Connect('127.0.0.1', 22) } finally { $connection.Dispose() }
    Write-BenchEvent 'ready' $seed $evidence
} catch {
    Write-BenchEvent 'failed' $seed
    exit 1
}
