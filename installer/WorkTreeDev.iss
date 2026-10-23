; WorkTreeDev per-user installer (no admin). Built by release.yml / build-local.ps1:
;   iscc /DAppVersion=X.Y.Z /DPayloadDir=<payload> /O<outdir> WorkTreeDev.iss
; Lays the payload down at {app}\versions\<ver>, points {app}\current at it, then hands off to
; `wtd setup`, which does the rest of the install (shims, hooks, MCP, vsix, tray).
; Silent installs take /DEVROOT=<dir> (default %USERPROFILE%\dev\worktree-dev).

#ifndef AppVersion
  #error Pass /DAppVersion=X.Y.Z
#endif
#ifndef PayloadDir
  #error Pass /DPayloadDir=<payload dir>
#endif
#define NumericVersion Copy(AppVersion, 1, (Pos("-", AppVersion) > 0 ? Pos("-", AppVersion) - 1 : Len(AppVersion)))

[Setup]
AppId={{AA92EE6E-F30A-4C12-890C-40B5C7A96A51}
AppName=WorkTreeDev
AppVersion={#AppVersion}
AppVerName=WorkTreeDev {#AppVersion}
AppPublisher=D-Luop
AppPublisherURL=https://github.com/D-Luop/worktree-dev
VersionInfoVersion={#NumericVersion}
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=
DefaultDirName={localappdata}\WorkTreeDev
DisableDirPage=yes
DisableProgramGroupPage=yes
UsePreviousAppDir=yes
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
OutputBaseFilename=WorkTreeDev-Setup-{#AppVersion}
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
UninstallDisplayName=WorkTreeDev
UninstallDisplayIcon={app}\current\bin\wtd.exe
SetupLogging=yes
#if Ver >= EncodeVer(6, 7, 0)
; Inno 6.7's default RedirectionGuard is inherited by child processes and blocks traversing the
; user-created `current` junction, so `wtd setup` (and the tray it starts) can't run from it.
; This installer never elevates, which is the case the guard protects.
RedirectionGuard=no
#endif

[Files]
Source: "{#PayloadDir}\*"; DestDir: "{app}\versions\{#AppVersion}"; Flags: recursesubdirs createallsubdirs ignoreversion

[UninstallRun]
Filename: "{app}\current\bin\wtd.exe"; Parameters: "setup --uninstall --keep-data --yes"; Flags: runhidden waituntilterminated; RunOnceId: "WtdSetupUninstall"

[UninstallDelete]
; `current` first: it's a junction into versions\ (or a copy)
Type: filesandordirs; Name: "{app}\current"
Type: filesandordirs; Name: "{app}\versions"
Type: filesandordirs; Name: "{app}\pending"
Type: files; Name: "{app}\setup.log"

[Code]
const
  PrereqCount = 3;

var
  DevRootPage: TInputDirWizardPage;
  PrereqPage: TInputOptionWizardPage;
  PrereqNames: array[0..PrereqCount - 1] of String;
  PrereqIds: array[0..PrereqCount - 1] of String;
  PrereqMissing: array[0..PrereqCount - 1] of Boolean;
  PrereqOption: array[0..PrereqCount - 1] of Integer;
  SetupError: String;

function FindOnPath(const Exe: String): Boolean;
var
  Path, Dir: String;
  I: Integer;
begin
  Result := False;
  Path := GetEnv('PATH') + ';';
  while (not Result) and (Path <> '') do
  begin
    I := Pos(';', Path);
    Dir := Trim(Copy(Path, 1, I - 1));
    Delete(Path, 1, I);
    if Dir <> '' then
      Result := FileExists(AddBackslash(RemoveQuotes(Dir)) + Exe);
  end;
end;

function HasGit: Boolean;
begin
  Result := FindOnPath('git.exe')
    or FileExists(ExpandConstant('{autopf}\Git\cmd\git.exe'))
    or FileExists(ExpandConstant('{commonpf64}\Git\cmd\git.exe'))
    or FileExists(ExpandConstant('{localappdata}\Programs\Git\cmd\git.exe'));
end;

function HasVsCode: Boolean;
begin
  Result := FindOnPath('code.cmd')
    or FileExists(ExpandConstant('{localappdata}\Programs\Microsoft VS Code\Code.exe'))
    or FileExists(ExpandConstant('{commonpf64}\Microsoft VS Code\Code.exe'));
end;

function HasClaude: Boolean;
begin
  Result := FindOnPath('claude.exe') or FindOnPath('claude.cmd')
    or FileExists(ExpandConstant('{%USERPROFILE}\.local\bin\claude.exe'));
end;

function WingetPath: String;
begin
  Result := ExpandConstant('{localappdata}\Microsoft\WindowsApps\winget.exe');
  if not FileExists(Result) then
    Result := '';
end;

procedure InitializeWizard;
var
  I: Integer;
  Hint: String;
begin
  DevRootPage := CreateInputDirPage(wpSelectDir, 'Dev root',
    'Where should WorkTreeDev keep your repositories and worktrees?',
    'Bare clones, worktrees, reference checkouts and WorkTreeDev''s state live under this folder. ' +
    'The program itself installs separately, under %LOCALAPPDATA%\WorkTreeDev.',
    False, '');
  DevRootPage.Add('');
  DevRootPage.Values[0] := ExpandConstant('{param:DEVROOT|' +
    GetPreviousData('DevRoot', ExpandConstant('{%USERPROFILE}\dev\worktree-dev')) + '}');

  PrereqNames[0] := 'Git (with Git Bash)';  PrereqIds[0] := 'Git.Git';
  PrereqNames[1] := 'Visual Studio Code';   PrereqIds[1] := 'Microsoft.VisualStudioCode';
  PrereqNames[2] := 'Claude Code';          PrereqIds[2] := 'Anthropic.ClaudeCode';
  PrereqMissing[0] := not HasGit;
  PrereqMissing[1] := not HasVsCode;
  PrereqMissing[2] := not HasClaude;

  if WingetPath <> '' then
    Hint := 'Checked items are installed with winget before WorkTreeDev is set up.'
  else
    Hint := 'winget was not found: install these yourself, then re-run Setup (or run `wtd setup --repair`).';
  PrereqPage := CreateInputOptionPage(DevRootPage.ID, 'Prerequisites',
    'WorkTreeDev needs these, and they were not found.', Hint, False, False);
  for I := 0 to PrereqCount - 1 do
  begin
    PrereqOption[I] := -1;
    if PrereqMissing[I] then
    begin
      PrereqOption[I] := PrereqPage.Add(PrereqNames[I] + '  (winget install ' + PrereqIds[I] + ')');
      PrereqPage.Values[PrereqOption[I]] := WingetPath <> '';
      PrereqPage.CheckListBox.ItemEnabled[PrereqOption[I]] := WingetPath <> '';
    end;
  end;
end;

function ShouldSkipPage(PageID: Integer): Boolean;
var
  I: Integer;
begin
  Result := False;
  if PageID = PrereqPage.ID then
  begin
    Result := True;
    for I := 0 to PrereqCount - 1 do
      if PrereqMissing[I] then
        Result := False;
  end;
end;

function NextButtonClick(CurPageID: Integer): Boolean;
begin
  Result := True;
  if (CurPageID = DevRootPage.ID) and (Trim(DevRootPage.Values[0]) = '') then
  begin
    MsgBox('Choose a dev root folder.', mbError, MB_OK);
    Result := False;
  end;
end;

procedure RegisterPreviousData(PreviousDataKey: Integer);
begin
  SetPreviousData(PreviousDataKey, 'DevRoot', DevRootPage.Values[0]);
end;

function UpdateReadyMemo(Space, NewLine, MemoUserInfoInfo, MemoDirInfo, MemoTypeInfo,
  MemoComponentsInfo, MemoGroupInfo, MemoTasksInfo: String): String;
var
  I: Integer;
begin
  Result := 'Program folder:' + NewLine + Space + ExpandConstant('{app}') + NewLine + NewLine +
            'Dev root:' + NewLine + Space + DevRootPage.Values[0];
  for I := 0 to PrereqCount - 1 do
    if (PrereqOption[I] >= 0) and PrereqPage.Values[PrereqOption[I]] then
      Result := Result + NewLine + NewLine + 'winget install ' + PrereqIds[I];
end;

procedure InstallPrereqs;
var
  I, Code: Integer;
begin
  for I := 0 to PrereqCount - 1 do
    if (PrereqOption[I] >= 0) and PrereqPage.Values[PrereqOption[I]] then
    begin
      WizardForm.StatusLabel.Caption := 'Installing ' + PrereqNames[I] + ' with winget...';
      if not Exec(WingetPath, 'install --id ' + PrereqIds[I] +
          ' -e --silent --accept-package-agreements --accept-source-agreements',
          '', SW_HIDE, ewWaitUntilTerminated, Code) or (Code <> 0) then
        Log(Format('winget install %s failed (%d)', [PrereqIds[I], Code]));
    end;
end;

// Point {app}\current at the version just laid down: a junction, else a plain copy.
procedure PointCurrent(const Target: String);
var
  Current: String;
  Code: Integer;
begin
  Current := ExpandConstant('{app}\current');
  if DirExists(Current) then
  begin
    // rmdir without /s removes a junction without touching its target; a real dir needs DelTree
    Exec(ExpandConstant('{cmd}'), '/C rmdir "' + Current + '"', '', SW_HIDE, ewWaitUntilTerminated, Code);
    if DirExists(Current) then
      DelTree(Current, True, True, True);
  end;
  if Exec(ExpandConstant('{cmd}'), '/C mklink /J "' + Current + '" "' + Target + '"', '', SW_HIDE,
      ewWaitUntilTerminated, Code) and (Code = 0) then
    Exit;
  Log(Format('mklink /J failed (%d); copying instead', [Code]));
  if not Exec(ExpandConstant('{cmd}'), '/C xcopy "' + Target + '" "' + Current + '\" /E /I /Q /Y', '',
      SW_HIDE, ewWaitUntilTerminated, Code) or (Code <> 0) then
    RaiseException(Format('Could not create %s (xcopy %d)', [Current, Code]));
end;

procedure RunWtdSetup(const From: String);
var
  Wtd, DevRoot, LogFile, Args: String;
  Code: Integer;
begin
  Wtd := ExpandConstant('{app}\current\bin\wtd.exe');
  DevRoot := DevRootPage.Values[0];
  LogFile := ExpandConstant('{app}\setup.log');
  ForceDirectories(DevRoot);
  Args := 'setup --from "' + From + '" --dev-root "' + DevRoot + '" --yes';
  Log('wtd ' + Args);
  WizardForm.StatusLabel.Caption := 'Running wtd setup...';
  if not Exec(ExpandConstant('{cmd}'), '/S /C ""' + Wtd + '" ' + Args + ' > "' + LogFile + '" 2>&1"', '',
      SW_HIDE, ewWaitUntilTerminated, Code) then
    Code := -1;
  Log(Format('wtd setup exited %d', [Code]));
  if Code <> 0 then
    SetupError := Format('wtd setup failed (exit %d). See %s, then run:', [Code, LogFile]) + #13#10#13#10 +
      '"' + Wtd + '" ' + Args;
end;

procedure CurStepChanged(CurStep: TSetupStep);
var
  Version: String;
begin
  if CurStep = ssPostInstall then
  begin
    Version := ExpandConstant('{app}\versions\{#AppVersion}');
    InstallPrereqs;
    PointCurrent(Version);
    RunWtdSetup(Version);
    if SetupError <> '' then
      SuppressibleMsgBox(SetupError, mbError, MB_OK, IDOK);
  end;
end;
