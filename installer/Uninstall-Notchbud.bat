@echo off
REM Uninstall-Notchbud.bat — portable uninstaller for the ZIP distribution.
REM Use this ONLY if you installed by copying notchbud.exe manually.
REM If you installed via Notchbud-Setup-*.exe, uninstall from
REM   Settings ^> Apps ^> Notchbud ^> Uninstall  (runs unins000.exe) instead.
REM
REM Place this .bat next to notchbud.exe (or anywhere) and double-click it.
REM It does NOT need admin rights.

setlocal
echo.
echo  Notchbud portable uninstall
echo  ===========================
echo.

REM 1. Stop a running instance so the tray icon + Run key can go away cleanly.
echo [*] Stopping notchbud.exe if running...
taskkill /F /IM notchbud.exe 2^>nul
timeout /t 1 /nobreak ^>nul

REM 2. Remove "Start with Windows" (HKCU\...\Run value "Notchbud").
REM    This is the same key src\prefs.rs manages via the tray menu.
echo [*] Removing autostart entry...
reg delete "HKCU\Software\Microsoft\Windows\CurrentVersion\Run" /v "Notchbud" /f 2^>nul
if %errorlevel%==0 (
  echo     - Run value removed.
) else (
  echo     - No Run value found (nothing to do^).
)

REM 3. Remove shortcuts if the user created them manually.
echo [*] Removing shortcuts...
del "%APPDATA%\Microsoft\Windows\Start Menu\Programs\Notchbud.lnk" 2^>nul
del "%USERPROFILE%\Desktop\Notchbud.lnk" 2^>nul

REM 4. API-key data: ask before deleting so a reinstall can stay signed in.
echo.
if not exist "%APPDATA%\notchbud\auth.bin" goto noauth
choice /M "Delete your Groq API key (%APPDATA%\notchbud\auth.bin)"
if errorlevel 2 goto keepauth
if errorlevel 1 goto wipeauth
:wipeauth
echo [*] Deleting %APPDATA%\notchbud...
rmdir /s /q "%APPDATA%\notchbud" 2^>nul
goto doneauth
:keepauth
echo [*] Keeping API key.
goto doneauth
:noauth
echo [*] No API key found.
rmdir "%APPDATA%\notchbud" 2^>nul
:doneauth

echo.
echo Done. You can now delete notchbud.exe + this file.
echo (This script cannot delete notchbud.exe itself while you might
echo  still want to check the cleanup worked — delete it by hand.)
echo.
pause
