; ============================================================================
;  FluxBridge - Inno Setup installer script
; ----------------------------------------------------------------------------
;  Compiled by the CI release job (.github/workflows/release.yml) with the Inno
;  Setup command-line compiler (ISCC.exe) into "FluxBridge-Setup-<version>.exe".
;  It is NOT compiled in the Rust/Linux sandbox (no Windows, no Inno Setup);
;  it is validated by review and by a `desktopicon` grep in CI/tests.
;
;  The CI job passes the release version in from the git tag, e.g.:
;      iscc /DAppVersion=%GITHUB_REF_NAME% installer\fluxbridge.iss
;  so the AppVersion below tracks the tag (vX.Y.Z) with no manual edits.
; ============================================================================

; ---------------------------------------------------------------------------
; #define preprocessor values. AppVersion can be overridden on the ISCC
; command line (/DAppVersion=1.2.3). The default keeps a local compile working.
; ---------------------------------------------------------------------------
#ifndef AppVersion
  #define AppVersion "0.1.0"
#endif
#define AppName "FluxBridge"
#define AppPublisher "FluxBridge contributors"
#define AppExeName "fluxbridge-desktop.exe"
#define AppUrl "https://github.com/Nub4Fun22/FluxBridge"

; ---------------------------------------------------------------------------
; [Setup]
;   Global installer identity and behavior.
;   - AppId is a stable GUID that ties every version together so upgrades and
;     the single Uninstall entry are correctly recognized across releases.
;   - AppVersion comes from the #define above (the git tag in CI).
;   - DefaultDirName {autopf}\FluxBridge installs to the right "Program Files"
;     for the architecture (Program Files vs Program Files (x86)).
;   - LicenseFile points at the repo LICENSE so the wizard shows a license page
;     the user must accept before continuing.
;   - The wizard lets the user pick the install directory (DisableDirPage=no)
;     and does not force a Start Menu folder page (DisableProgramGroupPage=yes),
;     keeping the flow "clean and simple" as required.
;   - OutputBaseFilename produces FluxBridge-Setup-<version>.exe, whose `.exe`
;     suffix is exactly what the in-app updater (crates/updater) prefers when it
;     selects a release asset to download.
;   - Compression settings keep the Setup.exe small.
; ---------------------------------------------------------------------------
[Setup]
AppId={{B3F2B7B1-7B6E-4E7A-9C3C-F1B2C3D40001}
AppName={#AppName}
AppVersion={#AppVersion}
AppPublisher={#AppPublisher}
AppPublisherURL={#AppUrl}
AppSupportURL={#AppUrl}
AppUpdatesURL={#AppUrl}/releases
DefaultDirName={autopf}\{#AppName}
DefaultGroupName={#AppName}
; Reference the project license so the wizard shows an acceptance page.
LicenseFile=..\LICENSE
; Let the user choose the install path; skip the extra program-group page.
DisableDirPage=no
DisableProgramGroupPage=yes
; Install into Program Files, so request elevation.
PrivilegesRequired=admin
ArchitecturesInstallIn64BitMode=x64compatible
OutputDir=..\dist
OutputBaseFilename=FluxBridge-Setup-{#AppVersion}
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
UninstallDisplayName={#AppName} {#AppVersion}
UninstallDisplayIcon={app}\{#AppExeName}

; ---------------------------------------------------------------------------
; [Languages]
;   The wizard UI language(s). English is enough for the scaffold.
; ---------------------------------------------------------------------------
[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

; ---------------------------------------------------------------------------
; [Tasks]
;   User-selectable options shown on the wizard's "Select Additional Tasks"
;   page. The `desktopicon` task is CHECKED BY DEFAULT (no "unchecked" flag and
;   no "checkedonce" flag), so the Desktop shortcut is created unless the user
;   explicitly clears it, exactly as required.
; ---------------------------------------------------------------------------
[Tasks]
Name: "desktopicon"; Description: "Create a &Desktop shortcut"; GroupDescription: "Additional shortcuts:"

; ---------------------------------------------------------------------------
; [Files]
;   The payload to install. The release workflow builds the optimized GUI
;   binary with `cargo build --release --features gui` and the script packages
;   that fluxbridge-desktop.exe. The LICENSE is installed alongside it so it is
;   available in the install directory.
;   Source paths are relative to this .iss file (installer/), so `..` steps up
;   to the repository root where CI drops the built binary.
; ---------------------------------------------------------------------------
[Files]
Source: "..\target\release\{#AppExeName}"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\LICENSE"; DestDir: "{app}"; Flags: ignoreversion

; ---------------------------------------------------------------------------
; [Icons]
;   Shortcuts created by the installer.
;   - The {group} entry is the Start Menu shortcut (always created).
;   - The {autodesktop} entry is the Desktop shortcut, created only when the
;     `desktopicon` task above is selected (which it is by default).
;   - A Start Menu "Uninstall" shortcut points at the auto-generated uninstaller.
; ---------------------------------------------------------------------------
[Icons]
; Start Menu program entry (always).
Name: "{group}\{#AppName}"; Filename: "{app}\{#AppExeName}"
; Desktop shortcut (default-enabled via the desktopicon task).
Name: "{autodesktop}\{#AppName}"; Filename: "{app}\{#AppExeName}"; Tasks: desktopicon
; Convenience Uninstall entry in the Start Menu group.
Name: "{group}\Uninstall {#AppName}"; Filename: "{uninstallexe}"

; ---------------------------------------------------------------------------
; [Run]
;   Offer to launch FluxBridge when installation finishes (optional, not
;   auto-checked while silent). `nowait` returns the wizard immediately.
; ---------------------------------------------------------------------------
[Run]
Filename: "{app}\{#AppExeName}"; Description: "Launch {#AppName}"; Flags: nowait postinstall skipifsilent

; ---------------------------------------------------------------------------
; [UninstallDelete]
;   Inno Setup AUTOMATICALLY registers the app in Windows'
;   "Apps & features" / "Add or remove programs" (Uninstall) list and writes
;   the matching uninstall registry keys under
;   HKLM\...\Uninstall\<AppId>_is1 - no manual [Registry] work is needed for
;   that. This section only cleans up extra files/dirs created at runtime that
;   are not tracked by the installer (for example a logs folder) so uninstall
;   leaves nothing behind.
; ---------------------------------------------------------------------------
[UninstallDelete]
Type: filesandordirs; Name: "{app}\logs"
