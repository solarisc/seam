; Seam installer for Windows. Build with Inno Setup 6:
;   iscc /DVersion=1.2.0 /DSource=..\..\target\release installer\windows\seam.iss
;
; Unattended use:
;   Seam-<version>-windows-setup.exe /VERYSILENT /SUPPRESSMSGBOXES
;   "%ProgramFiles%\Seam\unins000.exe" /VERYSILENT [/PURGE]
; /PURGE also deletes every user's Seam settings and pairings.

#ifndef Version
  #define Version "0.0.0"
#endif
#ifndef Source
  #define Source "..\..\target\release"
#endif

[Setup]
AppId={{7E5EDFB0-E9CD-47DE-88F5-2A422B430FFA}
AppName=Seam
AppVersion={#Version}
AppVerName=Seam {#Version}
AppPublisher=Seam
AppPublisherURL=https://github.com/solarisc/seam
AppSupportURL=https://github.com/solarisc/seam/issues
AppUpdatesURL=https://github.com/solarisc/seam/releases
AppCopyright=Copyright (c) The Seam contributors
VersionInfoVersion={#Version}
VersionInfoProductName=Seam
VersionInfoDescription=Seam installer
DefaultDirName={autopf}\Seam
DefaultGroupName=Seam
DisableProgramGroupPage=yes
DisableDirPage=auto
PrivilegesRequired=admin
; x64 binaries; Windows 11 on Arm runs them through emulation.
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0.17763
OutputDir=..\..\target\installer
OutputBaseFilename=Seam-{#Version}-windows-setup
SetupIconFile=..\..\assets\seam.ico
UninstallDisplayIcon={app}\seam.exe
UninstallDisplayName=Seam
WizardStyle=modern
Compression=lzma2
SolidCompression=yes
CloseApplications=no
; Names both licenses; their full text is installed with Seam.
LicenseFile=license.txt
SetupLogging=yes

[InstallDelete]
; The MIT-only license file earlier versions installed.
Type: files; Name: "{app}\LICENSE.txt"

[Files]
Source: "{#Source}\seam.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#Source}\seam-agent.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\..\LICENSE-APACHE"; DestDir: "{app}"; DestName: "LICENSE-APACHE.txt"
Source: "..\..\LICENSE-MIT"; DestDir: "{app}"; DestName: "LICENSE-MIT.txt"
Source: "..\..\assets\fonts\OFL.txt"; DestDir: "{app}"; DestName: "Inter-LICENSE.txt"

[Icons]
Name: "{autoprograms}\Seam"; Filename: "{app}\seam.exe"; Comment: "Share a keyboard and mouse between computers"

[UninstallDelete]
; Also when the folder existed before this installer, as after a manual install.
Type: dirifempty; Name: "{app}"

[Run]
Filename: "{app}\seam.exe"; Description: "Open Seam"; Flags: postinstall nowait skipifsilent runasoriginaluser

[Code]
var
  SetupFailed: Boolean;

// Where `seam-agent setup` logs when it cannot secure its own folder: the
// temporary folder Windows gives it.
function TempSetupLog: String;
begin
  Result := AddBackslash(GetTempDir) + 'seam-setup.log';
end;

// Where `seam-agent setup` explained a failure. Setup runs with no
// temporary log present, so one there now is from this run.
function SetupLog: String;
begin
  Result := TempSetupLog;
  if not FileExists(Result) then
    Result := ExpandConstant('{commonappdata}\Seam\setup.log');
end;

procedure Run(const Exe, Parameters: String);
var
  Code: Integer;
begin
  Exec(Exe, Parameters, '', SW_HIDE, ewWaitUntilTerminated, Code);
end;

procedure StopSeam(Directory: String);
var
  Agent: String;
begin
  // Keep Windows from restarting the sign-in service while files are
  // replaced; `seam-agent setup` restores its start type and recovery.
  Run(ExpandConstant('{sys}\sc.exe'), 'config SeamSignIn start= disabled');
  Run(ExpandConstant('{sys}\sc.exe'), 'failure SeamSignIn reset= 0 actions= ""');
  Agent := Directory + '\seam-agent.exe';
  if FileExists(Agent) then
  begin
    Run(Agent, 'stop-service');
    // Lets the agent release held keys and restore pointer settings.
    Run(Agent, 'stop');
  end;
  Run(ExpandConstant('{sys}\sc.exe'), 'stop SeamSignIn');
  if FileExists(Directory + '\seam.exe') then
    Run(Directory + '\seam.exe', '--quit-tray');
  Run(ExpandConstant('{sys}\taskkill.exe'), '/F /IM seam.exe');
  Run(ExpandConstant('{sys}\taskkill.exe'), '/F /IM seam-agent.exe');
end;

function PrepareToInstall(var NeedsRestart: Boolean): String;
begin
  StopSeam(ExpandConstant('{app}'));
  Result := '';
end;

procedure CurStepChanged(CurStep: TSetupStep);
var
  Code: Integer;
begin
  // Registers the sign-in tasks, firewall rule and sign-in service, then
  // starts sharing.
  if CurStep = ssPostInstall then
  begin
    WizardForm.StatusLabel.Caption := 'Setting up Seam...';
    DeleteFile(TempSetupLog);
    if not Exec(ExpandConstant('{app}\seam-agent.exe'), 'setup', '', SW_HIDE, ewWaitUntilTerminated, Code) or (Code <> 0) then
    begin
      SetupFailed := True;
      // Unattended installs report the failure through the exit code.
      if not WizardSilent then
        MsgBox('Seam was installed, but it could not be set up to start automatically.' + #13#10#13#10 +
          'Details are in ' + SetupLog + '. Restart your PC and run this installer again.',
          mbError, MB_OK);
    end;
  end;
end;

function GetCustomSetupExitCode: Integer;
begin
  if SetupFailed then
    Result := 1
  else
    Result := 0;
end;

function HasParameter(const Name: String): Boolean;
var
  I: Integer;
begin
  Result := False;
  for I := 1 to ParamCount do
    if CompareText(ParamStr(I), Name) = 0 then
      Result := True;
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
var
  Parameters: String;
begin
  if CurUninstallStep = usUninstall then
  begin
    StopSeam(ExpandConstant('{app}'));
    Parameters := 'uninstall';
    if HasParameter('/PURGE') then
      Parameters := Parameters + ' --purge'
    else if not UninstallSilent then
      if MsgBox('Also delete Seam''s settings and the list of paired computers?' + #13#10#13#10 +
        'Keep them if you plan to install Seam again.', mbConfirmation, MB_YESNO or MB_DEFBUTTON2) = IDYES then
        Parameters := Parameters + ' --purge';
    Run(ExpandConstant('{app}\seam-agent.exe'), Parameters);
  end;
end;
