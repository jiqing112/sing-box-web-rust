@echo off
rem sing-box panel quick start. Encoding: GBK/ANSI (cp936) - convert with
rem tools/bat_to_gbk.py after editing as UTF-8 elsewhere.
rem Behavior: double-click starts the panel hidden in the background (tray
rem icon only, no console window), opens the browser, then this window
rem closes itself after a short countdown.
setlocal EnableExtensions
cd /d "%~dp0"

if not exist "%~dp0panel.exe" (
  echo [错误] 未找到 panel.exe，请先完整解压发行包再运行本脚本。
  pause
  exit /b 1
)

rem 读 panel.conf 里的真实监听地址（改过端口时浏览器才打得开）
set "LISTEN=127.0.0.1:9090"
for /f "tokens=2 delims==" %%a in ('findstr /b /c:"listen=" panel.conf 2^>nul') do set "LISTEN=%%a"
set "OPEN_URL=http://%LISTEN%"
if "%OPEN_URL:~7,7%"=="0.0.0.0" set "OPEN_URL=http://127.0.0.1%OPEN_URL:~14%"

rem 已在运行就直接开网页，不重复启动
set "RUNNING="
for /f "tokens=2" %%p in ('tasklist /FI "IMAGENAME eq panel.exe" 2^>nul ^| findstr /I "panel.exe"') do set "RUNNING=%%p"
if defined RUNNING goto :started

echo 正在后台启动面板（无黑窗，托盘图标可用）...
if not exist "%~dp0sing-box.exe" echo [提示] 同目录未找到 sing-box.exe，核心将无法启动（详见 README）。
powershell -NoProfile -Command "Start-Process -FilePath '%~dp0panel.exe' -WorkingDirectory '%~dp0' -ArgumentList '-data','%~dp0panel-data' -WindowStyle Hidden"
ping -n 4 127.0.0.1 >nul
set "RUNNING="
for /f "tokens=2" %%p in ('tasklist /FI "IMAGENAME eq panel.exe" 2^>nul ^| findstr /I "panel.exe"') do set "RUNNING=%%p"
if not defined RUNNING (
  echo 启动失败：请直接运行 panel.exe 查看报错信息。
  pause
  exit /b 1
)

:started
echo 面板已后台运行 [PID %RUNNING%]
echo 浏览器即将打开 %OPEN_URL%
echo 首次使用：打开网页后会要求设置登录账户密码
start "" "%OPEN_URL%"
echo 浏览器已打开 %OPEN_URL%（托盘右键可启停/退出），本窗口即将自动关闭...
ping -n 6 127.0.0.1 >nul
exit /b 0
)
start "" "%OPEN_URL%"
echo 浏览器已打开 %OPEN_URL%（托盘右键可启停/退出），本窗口即将自动关闭...
ping -n 6 127.0.0.1 >nul
exit /b 0
