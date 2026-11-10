; Inno Setup script for the AgentsMon Windows installer.
;
; Built by .github/workflows/release.yml on tag push, alongside the macOS
; .dmg and the portable .zip. Per-user install (no UAC), signed in CI with
; Oganbo's Azure Artifact Signing identity (see Signed below).
;
; No [Registry] and no [UninstallDelete] section: AgentsMon writes its own
; integration files, and user data in %APPDATA%\gmbl\AgentsMon survives an
; uninstall, as it does when the macOS .app is removed.
;
; Local build (after scripts\build-windows.ps1):
;   & "${env:ProgramFiles(x86)}\Inno Setup 6\iscc.exe" `
;       /DAppVersion=0.0.0-dev bundle\windows\installer.iss

#define AppName       "AgentsMon"
#define AppPublisher  "Olivier Guimbal"
#define AppExeName    "agentsmon.exe"
#define AppURL        "https://github.com/oguimbal/AgentsMon"

#ifndef AppVersion
  #error AppVersion must be supplied: iscc /DAppVersion=X.Y.Z installer.iss
#endif

[Setup]
; AppId keys the uninstaller and Inno's upgrade detection — must stay
; constant across every release. Do not regenerate.
AppId={{65F3DD12-1AE5-49AB-B0CC-45829F1F6DED}
AppName={#AppName}
AppVersion={#AppVersion}
AppVerName={#AppName} {#AppVersion}
AppPublisher={#AppPublisher}
AppPublisherURL={#AppURL}
AppSupportURL={#AppURL}
AppUpdatesURL={#AppURL}
DefaultDirName={autopf}\{#AppName}
DefaultGroupName={#AppName}
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
ArchitecturesInstallIn64BitMode=x64compatible
ArchitecturesAllowed=x64compatible
OutputDir=..\..\dist
; Stable filename (no version, no arch suffix) so the canonical download URL
;   https://github.com/oguimbal/AgentsMon/releases/latest/download/AgentsMon-setup.exe
; works across releases. Mirrors the macOS .dmg artefact's naming.
OutputBaseFilename=AgentsMon-setup
UninstallDisplayIcon={app}\{#AppExeName}
UninstallDisplayName={#AppName}
WizardStyle=modern
Compression=lzma2
SolidCompression=yes
LicenseFile=..\..\LICENSE
; Force-close the running instance so Inno can replace the exe in place.
CloseApplications=force
; Restart Manager only restarts applications that registered for it, which
; AgentsMon does not. The silent [Run] entry below relaunches it instead.
RestartApplications=no
MinVersion=10.0

; CI passes /DSigned and /Sazuresign=<bundle\windows\sign.ps1 $f>: Inno then
; signs the setup and the uninstaller it embeds. Local builds stay unsigned.
#ifdef Signed
SignTool=azuresign
SignedUninstaller=yes
#endif

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "Create a &desktop shortcut"; GroupDescription: "Additional shortcuts:"; Flags: unchecked

[Files]
; Same layout as scripts\build-windows.ps1. The helper must sit next to the
; application: AgentsMon copies it from there into its data directory.
Source: "..\..\target\release\agentsmon.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\..\target\release\agentsmon-helper.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\..\plugins\agentsmon-codex\*"; DestDir: "{app}\codex-plugin"; Flags: ignoreversion recursesubdirs createallsubdirs
Source: "..\..\target\release\agentsmon-helper.exe"; DestDir: "{app}\codex-plugin\bin"; Flags: ignoreversion
Source: "..\..\LICENSE"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\..\README.md"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{group}\{#AppName}"; Filename: "{app}\{#AppExeName}"
Name: "{autodesktop}\{#AppName}"; Filename: "{app}\{#AppExeName}"; Tasks: desktopicon

[Run]
Filename: "{app}\{#AppExeName}"; Description: "Launch {#AppName}"; Flags: nowait postinstall skipifsilent
; In-app update (setup run with /SILENT): start the new version again.
Filename: "{app}\{#AppExeName}"; Flags: nowait; Check: WizardSilent
