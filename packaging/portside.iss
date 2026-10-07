#ifndef AppVersion
  #error AppVersion must be supplied by package.ps1
#endif

[Setup]
AppId={{AFF72E51-A6A3-43E6-A6A4-71C15C264409}
AppName=Portside
AppVersion={#AppVersion}
AppPublisher=Leon Björklund
AppPublisherURL=https://github.com/leonbjorklund/portside
DefaultDirName={localappdata}\Portside
PrivilegesRequired=lowest
ArchitecturesAllowed=x64os
ArchitecturesInstallIn64BitMode=x64os
MinVersion=10.0.22000
DisableDirPage=yes
DisableProgramGroupPage=yes
CloseApplications=no
RestartApplications=no
SetupMutex=PortsideSetup
UninstallDisplayIcon={app}\portside.exe
UninstallDisplayName=Portside
SetupIconFile=..\assets\portside.ico
OutputDir=..\target\installer
OutputBaseFilename=Portside-{#AppVersion}-x64-setup
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
TimeStampsInUTC=yes
TouchDate=none
TouchTime=00:00

[Files]
Source: "..\target\release\portside.exe"; DestDir: "{app}"; Flags: ignoreversion touch
Source: "..\target\release\portside.exe"; Flags: dontcopy
Source: "..\LICENSE"; DestDir: "{app}"; DestName: "LICENSE.txt"; Flags: ignoreversion touch
Source: "..\assets\fonts\atkinsonhyperlegible\OFL.txt"; DestDir: "{app}"; DestName: "AtkinsonHyperlegible-OFL.txt"; Flags: ignoreversion touch
Source: "licenses\*.txt"; DestDir: "{app}\licenses"; Flags: ignoreversion touch
Source: "..\target\package-tools\Rust-COPYRIGHT-library.html"; DestDir: "{app}\licenses"; Flags: ignoreversion touch

[Icons]
Name: "{userprograms}\Portside"; Filename: "{app}\portside.exe"; WorkingDir: "{app}"

[Registry]
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueType: string; ValueName: "Portside"; ValueData: """{app}\portside.exe"""; Flags: uninsdeletevalue

[UninstallDelete]
Type: filesandordirs; Name: "{app}\updates"

[Code]
var
  Reservation: THandle;
  StartFailed: Boolean;
  Stopping: Boolean;

function CreateMutex(Attributes: LongWord; InitialOwner: Boolean; Name: String): THandle;
  external 'CreateMutexW@kernel32.dll stdcall';
function CloseHandle(Handle: THandle): Boolean;
  external 'CloseHandle@kernel32.dll stdcall';

function StopPortside(Executable: String): Boolean;
var
  ExitCode: Integer;
begin
  Result := Exec(Executable, '--quit', '', SW_HIDE, ewWaitUntilTerminated, ExitCode);
  if Result then
    Result := ExitCode = 0;
  if Result then begin
    Reservation := CreateMutex(0, False, 'Portside');
    Result := (Reservation <> 0) and (DLLGetLastError <> 183);
  end;
end;

procedure ReleaseReservation;
begin
  if Reservation <> 0 then begin
    CloseHandle(Reservation);
    Reservation := 0;
  end;
end;

function PrepareToInstall(var NeedsRestart: Boolean): String;
begin
  ReleaseReservation;
  ExtractTemporaryFile('portside.exe');
  Stopping := True;
  if not StopPortside(ExpandConstant('{tmp}\portside.exe')) then
    Result := FmtMessage(SetupMessage(msgSetupAppRunningError), ['Portside'])
  // Portside runs updates from {app}\updates. Such an update stops here if
  // Portside was uninstalled, instead of putting it back. The uninstaller
  // needs the reservation this Setup now holds, so it cannot run in between.
  // {src} has links resolved, so only its last two folders are compared.
  else if PathEndsWith(ExpandConstant('{src}'), '\Portside\updates', True) and
    not FileExists(ExpandConstant('{app}\unins000.exe')) then
    Result := SetupMessage(msgSetupAborted);
end;

procedure CurStepChanged(CurStep: TSetupStep);
var
  ExitCode: Integer;
  Executable: String;
begin
  if CurStep = ssPostInstall then begin
    ReleaseReservation;
    Executable := ExpandConstant('{app}\portside.exe');
    Stopping := False;
    StartFailed := not Exec(Executable, '--start', '', SW_HIDE, ewWaitUntilTerminated, ExitCode);
    if not StartFailed then
      StartFailed := ExitCode <> 0;
    if StartFailed then
      SuppressibleMsgBox(FmtMessage(SetupMessage(msgErrorExecutingProgram), [Executable]), mbError, MB_OK, IDOK);
  end;
end;

function GetCustomSetupExitCode: Integer;
begin
  Result := 0;
  if StartFailed then
    Result := 1;
end;

procedure DeinitializeSetup;
var
  ExitCode: Integer;
begin
  ReleaseReservation;
  // Setup that stopped Portside and did not finish starts it again. When
  // Portside was uninstalled, portside.exe is gone and Exec does nothing.
  if Stopping then
    Exec(ExpandConstant('{app}\portside.exe'), '--start', '', SW_HIDE, ewWaitUntilTerminated, ExitCode);
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
var
  Waited: Integer;
begin
  if CurUninstallStep = usUninstall then begin
    // An update that is already installing finishes first, so the uninstall
    // does not run into its files.
    Waited := 0;
    while CheckForMutexes('PortsideSetup') and (Waited < 60000) do begin
      Sleep(250);
      Waited := Waited + 250;
    end;
    if not StopPortside(ExpandConstant('{app}\portside.exe')) then
      RaiseException(FmtMessage(SetupMessage(msgUninstallAppRunningError), ['Portside']));
  end;
end;

procedure DeinitializeUninstall;
begin
  ReleaseReservation;
end;
