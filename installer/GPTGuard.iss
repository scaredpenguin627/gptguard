#define AppName "GPTGuard"
#define AppVersion "0.4.0"
#define AppPublisher "GPTGuard"
#define AppExeName "GPTGuard.exe"

[Setup]
AppId={{B47A4AE6-0C1D-4C12-9B55-3F8B1F0BD6F3}
AppName={#AppName}
AppVersion={#AppVersion}
AppPublisher={#AppPublisher}
DefaultDirName={localappdata}\Programs\GPTGuard
DefaultGroupName={#AppName}
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
OutputDir=..\dist
OutputBaseFilename=GPTGuard-Setup-v{#AppVersion}-windows-x64
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
UninstallDisplayName={#AppName}
CloseApplications=force
RestartApplications=no

[Languages]
Name: "chinesesimplified"; MessagesFile: "compiler:Languages\ChineseSimplified.isl"

[Tasks]
Name: "autostart"; Description: "登录 Windows 时启动 GPTGuard"; GroupDescription: "启动选项："
Name: "desktopicon"; Description: "创建桌面快捷方式"; GroupDescription: "快捷方式："

[Files]
Source: "..\target\release\GPTGuard.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\LICENSE"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\THIRD_PARTY_NOTICES.md"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\GPTGuard"; Filename: "{app}\GPTGuard.exe"
Name: "{autodesktop}\GPTGuard"; Filename: "{app}\GPTGuard.exe"; Tasks: desktopicon

[Registry]
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueType: string; ValueName: "GPTGuard"; ValueData: "{app}\GPTGuard.exe"; Flags: uninsdeletevalue; Tasks: autostart

[Run]
Filename: "{app}\GPTGuard.exe"; Description: "启动 GPTGuard"; Flags: nowait postinstall skipifsilent

[UninstallRun]
Filename: "{app}\GPTGuard.exe"; Parameters: "--cli codex restore"; Flags: runhidden waituntilterminated; RunOnceId: "GPTGuardRestoreCodex"
