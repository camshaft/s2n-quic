@echo off
rem Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
rem SPDX-License-Identifier: Apache-2.0
powershell.exe -NoProfile -NonInteractive -Command "Start-Process powershell.exe -ArgumentList ('-NoProfile -NonInteractive -File C:\BenchProbe\finish.ps1 -SetupProcess ' + $PID) -WindowStyle Hidden"
exit /b 0
