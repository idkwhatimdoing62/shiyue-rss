#define MyAppName "拾阅 Shiyue"
#define MyAppPublisher "Shiyue"
#define MyAppURL "https://github.com/idkwhatimdoing62/shiyue-rss"
#ifndef MyAppVersion
  #define MyAppVersion "0.0.0-dev"
#endif

[Setup]
AppId={{A9DF0E86-0A5A-4D22-9E3D-6C8F44D5A1D5}
AppName={#MyAppName}
AppVersion={#MyAppVersion}
AppPublisher={#MyAppPublisher}
AppPublisherURL={#MyAppURL}
AppSupportURL={#MyAppURL}
AppUpdatesURL={#MyAppURL}
DefaultDirName={autopf}\Shiyue
DefaultGroupName={#MyAppName}
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
OutputDir=Output
OutputBaseFilename=shiyue-{#MyAppVersion}-windows-x64-setup
SetupIconFile=..\assets\shiyue-icon.ico
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
UninstallDisplayIcon={app}\shiyue.exe

[Files]
Source: "..\target\release\shiyue.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\target\release\shiyue-cli.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\README.md"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\LICENSE"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\THIRD_PARTY_NOTICES.md"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\{#MyAppName}"; Filename: "{app}\shiyue.exe"; IconFilename: "{app}\shiyue.exe"
Name: "{autodesktop}\{#MyAppName}"; Filename: "{app}\shiyue.exe"; IconFilename: "{app}\shiyue.exe"; Tasks: desktopicon

[Tasks]
Name: "desktopicon"; Description: "创建桌面快捷方式"; GroupDescription: "附加快捷方式："

[Run]
Filename: "{app}\shiyue.exe"; Description: "启动{#MyAppName}"; Flags: nowait postinstall skipifsilent
