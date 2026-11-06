; Inno-Setup-Skript für den Windows-Installer.
; Aufruf: ISCC.exe /DAppVersion=0.1.0-alpha.6 packaging\windows\DeviceConnector.iss
; ARM64:  ISCC.exe /DAppVersion=... /DArch=arm64 /DExeDir=target\aarch64-pc-windows-msvc\release ...
; Signiert (Setup + Deinstallation), siehe release.yml:
;   ISCC.exe /DAppVersion=... /DSign "/Sdcsign=pwsh -NoProfile -File <repo>\packaging\windows\sign.ps1 $f" ...

#ifndef AppVersion
  #define AppVersion "0.0.0-dev"
#endif
; Architektur: x64 (Standard) oder arm64. ExeDir zeigt auf den Ordner mit
; device-connector.exe, z. B. target\aarch64-pc-windows-msvc\release.
#ifndef Arch
  #define Arch "x64"
#endif
#ifndef ExeDir
  #define ExeDir "target\release"
#endif
#if Arch == "arm64"
  #define ArchAllowed "arm64"
#else
  #define ArchAllowed "x64compatible"
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
OutputBaseFilename=DeviceConnector-{#AppVersion}-windows-{#Arch}-setup
Compression=lzma2
SolidCompression=yes
ArchitecturesAllowed={#ArchAllowed}
ArchitecturesInstallIn64BitMode={#ArchAllowed}
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
Source: "..\..\{#ExeDir}\device-connector.exe"; DestDir: "{app}"; DestName: "DeviceConnector.exe"; Flags: ignoreversion
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
