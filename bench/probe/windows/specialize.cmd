@echo off
rem Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
rem SPDX-License-Identifier: Apache-2.0
mkdir C:\BenchProbe
xcopy "%~1\payload\*" C:\BenchProbe\ /E /I /Q /Y >nul
if errorlevel 1 exit /b 1
mkdir C:\Windows\Setup\Scripts 2>nul
copy /Y C:\BenchProbe\SetupComplete.cmd C:\Windows\Setup\Scripts\SetupComplete.cmd >nul
if errorlevel 1 exit /b 1
powershell.exe -NoProfile -NonInteractive -File C:\BenchProbe\specialize.ps1
exit /b %errorlevel%
