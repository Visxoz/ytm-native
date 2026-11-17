; Windows installer, built by the release workflow with Inno Setup 6:
;   ISCC /DAppVersion=<x.y.z> installer\lily.iss
; Installs per user, without an admin prompt, so the app can update itself
; by running a newer installer silently with /relaunch=1.

#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif
#define AppName "Lily"
#define AppExe "lily.exe"

[Setup]
; Never change: Windows matches updates and the uninstaller by it.
AppId={{B7890858-E228-45C7-90D8-AF4B2B565E3A}
AppName={#AppName}
AppVersion={#AppVersion}
AppVerName={#AppName} {#AppVersion}
AppPublisher=Visxoz
AppPublisherURL=https://github.com/Visxoz/lily
AppSupportURL=https://github.com/Visxoz/lily/issues
AppUpdatesURL=https://github.com/Visxoz/lily/releases
; Copies installed as ytm-native stay in its folder, as the AppId is the same.
DefaultDirName={localappdata}\Programs\{#AppName}
DisableDirPage=auto
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0
OutputDir=..\dist
OutputBaseFilename=lily_{#AppVersion}_x64-setup
SetupIconFile=..\icons\icon.ico
UninstallDisplayIcon={app}\{#AppExe}
UninstallDisplayName={#AppName}
WizardStyle=modern
Compression=lzma2/max
SolidCompression=yes
; Close a running copy before replacing it, without asking when silent.
CloseApplications=force
RestartApplications=no

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[InstallDelete]
; What versions named ytm-native left, so an upgrade leaves no dead shortcuts.
Type: files; Name: "{app}\ytm-native.exe"
Type: files; Name: "{autoprograms}\ytm-native.lnk"
Type: files; Name: "{autodesktop}\ytm-native.lnk"

[Files]
Source: "..\target\release\{#AppExe}"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\{#AppName}"; Filename: "{app}\{#AppExe}"
Name: "{autodesktop}\{#AppName}"; Filename: "{app}\{#AppExe}"; Tasks: desktopicon

[Run]
Filename: "{app}\{#AppExe}"; Description: "{cm:LaunchProgram,{#AppName}}"; Flags: nowait postinstall skipifsilent
; After a silent update started from the app, open it again.
Filename: "{app}\{#AppExe}"; Flags: nowait; Check: Relaunch

[Code]
function Relaunch: Boolean;
begin
  Result := ExpandConstant('{param:relaunch|0}') = '1';
end;
