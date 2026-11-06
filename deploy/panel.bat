@echo off
rem sing-box panel manager. Encoding: GBK/ANSI (cp936) so Chinese text shows
rem correctly in the default zh-CN console without chcp games. Commands, labels
rem and variable names are pure ASCII; Chinese appears only in echo/title text.
rem NOTE: after editing elsewhere as UTF-8, convert with: python tools/bat_to_gbk.py
rem Usage: panel.bat [start|startbg|stop|restart|status|open|autostart-on|autostart-off|help]
setlocal EnableExtensions
title sing-box 面板管理
cd /d "%~dp0"

if not exist "%~dp0panel.exe" (
  echo [错误] 未找到 panel.exe，请先完整解压发行包再运行本脚本。
  pause
  exit /b 1
)
call :read_listen

rem ---------- 命令分发（括号必须：if cmd1 & cmd2 的 cmd2 是无条件执行的） ----------
if /i "%~1"=="menu"         goto menu
if /i "%~1"=="start"         (call :do_start   & exit /b)
if /i "%~1"=="startbg"       (call :do_startbg & exit /b)
if /i "%~1"=="stop"          (call :do_stop    & exit /b)
if /i "%~1"=="restart"       (call :do_restart & exit /b)
if /i "%~1"=="status"        (call :do_status  & exit /b)
if /i "%~1"=="open"          (call :do_open    & exit /b)
if /i "%~1"=="autostart-on"  (call :do_as_on   & exit /b)
if /i "%~1"=="autostart-off" (call :do_as_off  & exit /b)
if /i "%~1"=="help"          (call :do_help    & exit /b)
if not "%~1"=="" (
  echo 未知命令: %~1
  call :do_help
  exit /b 1
)

rem ---------- 默认（无参数双击）----------
call :is_running
if %errorlevel%==0 goto menu
call :do_startbg
if %errorlevel% neq 0 (pause & exit /b 1)
call :do_open
echo.
echo 面板已转入后台，本窗口即将自动关闭（托盘右键可启停/退出）。
echo 需要管理操作（停止/开机自启/状态）时，再次双击本脚本即可打开菜单。
ping -n 6 127.0.0.1 >nul
exit /b 0

rem ---------- 交互菜单 ----------
rem 用 choice 而不是 set /p：单键即选、无需回车，管道/EOF 下也不会死循环
:menu
cls
echo ==========================================
echo   sing-box 面板管理    %OPEN_URL%
echo ==========================================
echo   1. 前台运行（控制台日志，Ctrl+C 停止）
echo   2. 后台运行（隐藏窗口，托盘图标可用）
echo   3. 停止面板与内核
echo   4. 重启（后台）
echo   5. 查看状态
echo   6. 打开网页配置
echo   7. 开机自启：开启
echo   8. 开机自启：关闭
echo   0. 退出
echo ==========================================
choice /C 123456780 /N /M "请输入序号: "
if errorlevel 9 exit /b 0
if errorlevel 8 (call :do_as_off   & goto menu)
if errorlevel 7 (call :do_as_on    & goto menu)
if errorlevel 6 (call :do_open     & goto menu)
if errorlevel 5 (call :do_status   & pause & goto menu)
if errorlevel 4 (call :do_restart  & pause & goto menu)
if errorlevel 3 (call :do_stop     & pause & goto menu)
if errorlevel 2 call :m_bg
if errorlevel 1 (call :do_start    & pause & goto menu)
exit /b 0

rem ---------- 子过程 ----------
rem 菜单「后台运行」：启动成功就转后台并自动关窗
:m_bg
call :do_startbg
if %errorlevel%==0 (
  call :do_open
  echo.
  echo 面板已转入后台，本窗口即将自动关闭（托盘右键可启停/退出）。
  ping -n 6 127.0.0.1 >nul
  exit /b 0
)
pause
goto menu

rem 读 panel.conf 里的监听地址，算出浏览器可用的 URL
:read_listen
set "LISTEN=127.0.0.1:9090"
for /f "tokens=2 delims==" %%a in ('findstr /b /c:"listen=" panel.conf 2^>nul') do set "LISTEN=%%a"
set "OPEN_URL=http://%LISTEN%"
if "%OPEN_URL:~7,7%"=="0.0.0.0" set "OPEN_URL=http://127.0.0.1%OPEN_URL:~14%"
exit /b 0

rem RUNNING 置为 PID（在运行）或空（未运行）；返回 0=运行中
:is_running
set "RUNNING="
for /f "tokens=2" %%p in ('tasklist /FI "IMAGENAME eq panel.exe" 2^>nul ^| findstr /I "panel.exe"') do set "RUNNING=%%p"
if defined RUNNING (exit /b 0) else exit /b 1

:do_start
call :is_running
if %errorlevel%==0 (
  echo 面板已在运行 [PID %RUNNING%]，可直接打开 %OPEN_URL%
  exit /b 0
)
echo 正在前台启动面板（Ctrl+C 或关闭窗口即退出，托盘图标同步可用）...
"%~dp0panel.exe"
exit /b 0

:do_startbg
call :is_running
if %errorlevel%==0 (
  echo 面板已在运行 [PID %RUNNING%]，无需重复启动。
  exit /b 0
)
if not exist "%~dp0sing-box.exe" echo [提示] 同目录未找到 sing-box.exe，核心将无法启动（详见 README）。
echo 正在后台启动面板...
powershell -NoProfile -Command "Start-Process -FilePath '%~dp0panel.exe' -WorkingDirectory '%~dp0' -ArgumentList '-data','%~dp0panel-data' -WindowStyle Hidden"
ping -n 3 127.0.0.1 >nul
call :is_running
if %errorlevel%==0 (
  echo 面板已后台运行 [PID %RUNNING%]，双击托盘图标打开 %OPEN_URL%
) else (
  echo 启动失败：请直接运行 panel.exe 查看报错信息。
  exit /b 1
)
exit /b 0

:do_stop
call :is_running
if %errorlevel%==0 (
  echo 正在停止面板 [PID %RUNNING%]...
  taskkill /PID %RUNNING% /T /F >nul 2>&1
)
rem 兜底：只清「本目录数据目录」的孤儿内核（按命令行匹配，与面板自身的孤儿清扫同规则），
rem 不误杀机器上其他实例的 sing-box / mihomo。
powershell -NoProfile -Command "$d=('%~dp0panel-data').ToLower(); Get-CimInstance Win32_Process -Filter \"name='sing-box.exe' or name='mihomo.exe'\" | Where-Object { $_.CommandLine -and $_.CommandLine.ToLower().Contains($d) } | ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }"
ping -n 2 127.0.0.1 >nul
call :is_running
if %errorlevel%==0 (
  echo 停止失败：可能权限不足，请以管理员身份重试。
  exit /b 1
)
echo 已停止面板与内核进程。
exit /b 0

:do_restart
call :do_stop
call :do_startbg
exit /b 0

:do_status
echo 目录:     %~dp0
echo 面板地址: %OPEN_URL%
call :is_running
if %errorlevel%==0 (
  echo 面板进程: 运行中 [PID %RUNNING%]
) else (
  echo 面板进程: 未运行
)
schtasks /Query /TN "sing-box-panel" >nul 2>&1 && (
  echo 开机自启: 已开启（计划任务 sing-box-panel，SYSTEM 账户）
) || (
  echo 开机自启: 未开启
)
exit /b 0

:do_open
call :is_running
if %errorlevel% neq 0 (
  echo 面板未运行，请先启动（菜单 1 或 2）。
  exit /b 1
)
start "" "%OPEN_URL%"
echo 已在默认浏览器打开 %OPEN_URL%
exit /b 0

rem 开机自启与面板内开关用的是同一个计划任务 sing-box-panel，两边状态互通。
:do_as_on
call :ensure_admin autostart-on
if %errorlevel% neq 0 exit /b 1
schtasks /Create /TN "sing-box-panel" /TR "\"%~dp0panel.exe\" -data \"%~dp0panel-data\"" /SC ONSTART /RU SYSTEM /RL HIGHEST /F >nul 2>&1
schtasks /Query /TN "sing-box-panel" >nul 2>&1 && (
  echo 开机自启：已开启（计划任务 sing-box-panel，SYSTEM 账户，最高权限）
) || (
  echo 开机自启：设置失败，请检查管理员权限。
)
pause
exit /b 0

:do_as_off
call :ensure_admin autostart-off
if %errorlevel% neq 0 exit /b 1
schtasks /Delete /TN "sing-box-panel" /F >nul 2>&1
schtasks /Query /TN "sing-box-panel" >nul 2>&1 && (
  echo 开机自启：关闭失败，请检查管理员权限。
) || (
  echo 开机自启：已关闭。
)
pause
exit /b 0

rem 非管理员时用 UAC 重跑自身（带同一个命令参数），成功弹出提权窗口后返回 1
:ensure_admin
net session >nul 2>&1 && exit /b 0
echo 需要管理员权限，正在弹出 UAC 提权窗口（请在弹出的窗口里查看结果）...
powershell -NoProfile -Command "Start-Process -FilePath '%~f0' -ArgumentList '%~1' -Verb RunAs"
exit /b 1

:do_help
echo 用法: panel.bat [命令]    （不带参数 = 未运行则后台启动，已运行则进菜单）
echo   menu          直接打开管理菜单
echo   start         前台运行（控制台日志）
echo   startbg       后台运行（隐藏窗口 + 托盘图标）
echo   stop          停止面板与内核
echo   restart       重启（后台）
echo   status        查看运行状态
echo   open          打开网页配置
echo   autostart-on  开启开机自启（需管理员，自动弹 UAC）
echo   autostart-off 关闭开机自启（需管理员，自动弹 UAC）
echo   help          显示本帮助
exit /b 0
