# Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
# SPDX-License-Identifier: Apache-2.0

. C:\BenchProbe\common.ps1
$seed = Read-Seed 'BENCHRUN'
Write-BenchEvent 'stopping' $seed
Stop-Computer -Force
