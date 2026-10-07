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
  StartAttempted: Boolean;

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

function SetupRunning: Boolean;
var
  Mutex: THandle;
begin
  Mutex := CreateMutex(0, False, 'PortsideSetup');
  Result := (Mutex <> 0) and (DLLGetLastError = 183);
  if Mutex <> 0 then
    CloseHandle(Mutex);
end;

// An update that is installing finishes before Portside is removed, so it
// cannot put Portside back afterwards.
procedure WaitForSetup;
var
  Waited: Integer;
begin
  Waited := 0;
  while SetupRunning and (Waited < 60000) do begin
    Sleep(250);
    Waited := Waited + 250;
  end;
  if SetupRunning then
    RaiseException(FmtMessage(SetupMessage(msgUninstallAppRunningError), ['Portside']));
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
    Result := FmtMessage(SetupMessage(msgSetupAppRunningError), ['Portside']);
end;

procedure CurStepChanged(CurStep: TSetupStep);
var
  ExitCode: Integer;
  Executable: String;
begin
  if CurStep = ssPostInstall then begin
    ReleaseReservation;
    Executable := ExpandConstant('{app}\portside.exe');
    StartAttempted := True;
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
  // An upgrade that fails or is cancelled after stopping Portside leaves
  // the previous copy in place, so start it again.
  if Stopping and not StartAttempted then
    Exec(ExpandConstant('{app}\portside.exe'), '--start', '', SW_HIDE, ewWaitUntilTerminated, ExitCode);
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if CurUninstallStep = usUninstall then begin
    WaitForSetup;
    if not StopPortside(ExpandConstant('{app}\portside.exe')) then
      RaiseException(FmtMessage(SetupMessage(msgUninstallAppRunningError), ['Portside']));
  end;
end;

procedure DeinitializeUninstall;
begin
  ReleaseReservation;
end;
