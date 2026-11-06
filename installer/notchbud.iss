; notchbud — Inno Setup installer (per-user, no admin, no UAC).
; Builds Notchbud-Setup-{version}.exe from the single portable exe:
;   cargo build --release  ->  target\release\notchbud.exe
;
; Build the installer (after cargo build), from the repo root.
; Requires Inno Setup 6 (ISCC.exe). Either put its directory on PATH:
;   set PATH=%PATH%;"C:\Program Files (x86)\Inno Setup 6"
;   ISCC.exe installer\notchbud.iss
; or invoke the compiler by full path:
;   "C:\Program Files (x86)\Inno Setup 6\ISCC.exe" installer\notchbud.iss
; Output: installer\Output\Notchbud-Setup-0.1.2.exe
;
; What the user sees:
;   - Folder picker (default: %LOCALAPPDATA%\Programs\Notchbud)
;   - Start Menu shortcuts, optional desktop shortcut
;   - Optional "Start with Windows" (current-user Run value "Notchbud")
;   - Settings -> Apps uninstall entry (Inno auto-creates unins000.exe —
;     that IS the uninstaller, no separate file needed).
; No admin rights needed at any point: the app itself only ever writes
; HKCU (Run key) + %APPDATA%\notchbud\auth.bin at runtime.

#define MyAppName "Notchbud"
#define MyAppVersion "0.1.2"
#define MyAppExe "notchbud.exe"
#define MyAppPublisher "Notchbud"

[Setup]
AppId={{8E3C9A1B-4F2D-4C7A-9E1B-0A1B2C3D4E50}}
AppName={#MyAppName}
AppVersion={#MyAppVersion}
AppVerName={#MyAppName} {#MyAppVersion}
AppPublisher={#MyAppPublisher}
DefaultDirName={localappdata}\Programs\{#MyAppName}
DefaultGroupName={#MyAppName}
OutputDir=Output
OutputBaseFilename=Notchbud-Setup-{#MyAppVersion}
PrivilegesRequired=lowest
Compression=lzma2/max
SolidCompression=yes
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
CloseApplications=yes
RestartApplications=no
UninstallDisplayName={#MyAppName}
UninstallDisplayIcon={app}\{#MyAppExe}
; Icon for the Setup.exe itself. web\public\favicon.ico ships with the repo.
SetupIconFile=..\web\public\favicon.ico
WizardStyle=modern

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "Create a &desktop shortcut"; GroupDescription: "Additional shortcuts:"; Flags: unchecked
Name: "autostart"; Description: "Start Notchbud with Windows"; GroupDescription: "Startup:"

[Files]
; The ONLY file needed to run notchbud. Everything else in target\release\
; (.pdb, .rlib, .d) is developer build output — never ship it.
Source: "..\target\release\{#MyAppExe}"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{group}\{#MyAppName}"; Filename: "{app}\{#MyAppExe}"
Name: "{group}\Uninstall {#MyAppName}"; Filename: "{uninstallexe}"
Name: "{autodesktop}\{#MyAppName}"; Filename: "{app}\{#MyAppExe}"; Tasks: desktopicon

; Installer-managed autostart (current user only — correct for this
; per-user install). Deleted automatically on uninstall
; (uninsdeletevalue). Note: the app can also create this same Run value
; itself via tray -> "Start with Windows" (prefs.rs) — the [Code] section
; below deletes it unconditionally, plus a stale machine-wide one if a
; previous admin install ever left it, so no orphan entry survives either way.
[Registry]
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueType: string; ValueName: "Notchbud"; ValueData: """{app}\{#MyAppExe}"""; Flags: uninsdeletevalue; Tasks: autostart

[Run]
Filename: "{app}\{#MyAppExe}"; Description: "Launch Notchbud"; Flags: nowait postinstall skipifsilent runasoriginaluser

[Code]
// Always clean the tray-created autostart entry too, plus optionally the
// DPAPI login blob at %APPDATA%\notchbud\auth.bin.
procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
var
  RunKey: string;
  DataDir: string;
  AuthFile: string;
  ResultCode: Integer;
begin
  if CurUninstallStep = usUninstall then
  begin
    // 1. Kill a running instance so the exe + tray icon can be removed.
    Exec('taskkill.exe', '/F /IM notchbud.exe', '', SW_HIDE, ewWaitUntilTerminated, ResultCode);

    // 2. Remove "Start with Windows" no matter who created it
    //    (installer task OR in-app tray toggle in prefs.rs).
    RunKey := 'Software\Microsoft\Windows\CurrentVersion\Run';
    RegDeleteValue(HKCU, RunKey, 'Notchbud');
    RegDeleteValue(HKLM, RunKey, 'Notchbud');

    // 3. Ask about sign-in data. Default is KEEP (No) so a reinstall stays
    //    signed in; Yes wipes %APPDATA%\notchbud entirely.
    DataDir := ExpandConstant('{userappdata}\notchbud');
    AuthFile := DataDir + '\auth.bin';
    if FileExists(AuthFile) then
    begin
      if MsgBox('Delete your Notchbud sign-in data?' + #13#10 + #13#10 +
        AuthFile + #13#10 + #13#10 +
        'Click Yes to sign out this PC completely, or No to keep it for a reinstall.',
        mbConfirmation, MB_YESNO) = IDYES then
        DelTree(DataDir, True, True, True);
    end
    else if DirExists(DataDir) then
    begin
      DelTree(DataDir, True, True, True);
    end;
  end;
end;
