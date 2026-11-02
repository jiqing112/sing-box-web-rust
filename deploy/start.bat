@echo off
setlocal
cd /d "%~dp0"

if not exist "panel.exe" (
  echo [ERROR] panel.exe not found. Extract the whole zip first.
  pause
  exit /b 1
)
if not exist "sing-box.exe" (
  echo [WARN] sing-box.exe not found next to panel.exe; the sing-box core cannot start.
  echo        Put the official sing-box.exe in this folder.
)
if not exist "mihomo.exe" (
  echo [INFO] mihomo.exe not found - optional second core, sing-box still works.
)

echo Starting sing-box panel ...
start "" /D "%~dp0" "%~dp0panel.exe"

rem Give the panel a moment to create data files, then show the initial password
timeout /t 3 /nobreak >nul
if exist "%~dp0panel-data\initial-password" (
  echo.
  echo ==================================================
  echo  First run - initial password:
  type "%~dp0panel-data\initial-password"
  echo  ^(change it in Settings after login, then delete
  echo   panel-data\initial-password^)
  echo ==================================================
  echo.
)

start "" http://127.0.0.1:9090
echo Panel opened at http://127.0.0.1:9090
echo TUN mode needs "Run as administrator".
echo Close this window when done.
timeout /t 15 /nobreak >nul
