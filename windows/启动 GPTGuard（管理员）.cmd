@echo off
chcp 65001 >nul
cd /d "%~dp0"
net session >nul 2>&1
if %errorlevel% neq 0 (
    echo 需要管理员权限，正在请求 UAC...
    powershell -NoProfile -ExecutionPolicy Bypass -Command "Start-Process -FilePath '%~f0' -Verb RunAs"
    exit /b
)
start "" "%~dp0GPTGuard.exe"
