#define AppName "Longwatch for Codex"
#define AppVersion GetEnv("LONGWATCH_VERSION")
#define SourceExe GetEnv("LONGWATCH_EXE")

[Setup]
AppId={{D6E547C8-6DA5-4C53-AB8E-9E64618F9B78}
AppName={#AppName}
AppVersion={#AppVersion}
AppPublisher=Longwatch Project
VersionInfoCompany=Longwatch Project
VersionInfoDescription=Longwatch for Codex Windows Installer
VersionInfoProductName=Longwatch for Codex
VersionInfoProductVersion={#AppVersion}
VersionInfoVersion={#AppVersion}.0
DefaultDirName={localappdata}\Programs\Longwatch
DefaultGroupName=Longwatch
OutputDir=..\..
OutputBaseFilename=Longwatch-{#AppVersion}-windows-x64-setup
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
SetupIconFile=Longwatch.ico
PrivilegesRequired=lowest
MinVersion=10.0.17763
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
UninstallDisplayIcon={app}\codex-longwatch.exe
DisableProgramGroupPage=yes
CloseApplications=yes
RestartApplications=no
SetupLogging=yes

[Files]
Source: "{#SourceExe}"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\..\README.md"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\..\LICENSE"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{group}\Longwatch for Codex"; Filename: "{app}\codex-longwatch.exe"; WorkingDir: "{app}"; AppUserModelID: "Longwatch.Codex"
Name: "{autodesktop}\Longwatch for Codex"; Filename: "{app}\codex-longwatch.exe"; WorkingDir: "{app}"; AppUserModelID: "Longwatch.Codex"; Tasks: desktopicon

[Tasks]
Name: "desktopicon"; Description: "Create a desktop shortcut"; GroupDescription: "Additional icons:"; Flags: unchecked

[Run]
Filename: "{app}\codex-longwatch.exe"; Description: "Launch Longwatch for Codex"; Flags: nowait postinstall skipifsilent
