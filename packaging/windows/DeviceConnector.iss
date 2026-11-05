; Inno-Setup-Skript für den Windows-Installer.
; Aufruf: ISCC.exe /DAppVersion=0.1.0-alpha.3 packaging\windows\DeviceConnector.iss
; Signiert (Setup + Deinstallation), siehe release.yml:
;   ISCC.exe /DAppVersion=... /DSign "/Sdcsign=pwsh -NoProfile -File <repo>\packaging\windows\sign.ps1 $f" ...

#ifndef AppVersion
  #define AppVersion "0.0.0-dev"
#endif

[Setup]
AppId={{6A0C1E3B-2D4F-4B8A-9E61-DC0C0DE00001}
AppName=Device Connector
AppVersion={#AppVersion}
AppPublisher=gigafrosxh
AppPublisherURL=https://github.com/gigafrosxh/device-connector
DefaultDirName={autopf}\Device Connector
DefaultGroupName=Device Connector
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog
OutputDir=..\..\dist
OutputBaseFilename=DeviceConnector-{#AppVersion}-windows-x64-setup
Compression=lzma2
SolidCompression=yes
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
UninstallDisplayName=Device Connector
CloseApplications=yes
#ifdef Sign
SignTool=dcsign
SignedUninstaller=yes
#endif

[Languages]
Name: "german"; MessagesFile: "compiler:Languages\German.isl"
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "autostart"; Description: "Beim Anmelden automatisch starten"; GroupDescription: "Zusätzlich:"
Name: "desktopicon"; Description: "Verknüpfung auf dem Desktop"; GroupDescription: "Zusätzlich:"; Flags: unchecked

[Files]
Source: "..\..\target\release\device-connector.exe"; DestDir: "{app}"; DestName: "DeviceConnector.exe"; Flags: ignoreversion
Source: "..\..\README.md"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\..\LICENSE"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{group}\Device Connector"; Filename: "{app}\DeviceConnector.exe"
Name: "{autodesktop}\Device Connector"; Filename: "{app}\DeviceConnector.exe"; Tasks: desktopicon
Name: "{userstartup}\Device Connector"; Filename: "{app}\DeviceConnector.exe"; Tasks: autostart

[Run]
Filename: "{app}\DeviceConnector.exe"; Description: "Device Connector jetzt starten"; Flags: nowait postinstall skipifsilent

[UninstallRun]
Filename: "taskkill.exe"; Parameters: "/IM DeviceConnector.exe /F"; Flags: runhidden; RunOnceId: "KillApp"
