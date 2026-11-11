@echo off
setlocal
chcp 65001 >nul
set "APP=%~dp0ccodex-sleep-state.exe"
if not exist "%APP%" set "APP=%~dp0..\ccodex-sleep-state.exe"
if not exist "%APP%" (
  echo 未找到 ccodex-sleep-state.exe。请完整解压发布包后，再双击 start.cmd。
  pause
  exit /b 1
)
echo 正在启动。请保持这个窗口打开；退出时按 Ctrl+C，以便恢复 Codex 配置。
"%APP%" setup %*
set "RESULT=%ERRORLEVEL%"
if not "%RESULT%"=="0" echo 启动未完成，请保留上方错误信息。不要删除配置或备份。
pause
exit /b %RESULT%
