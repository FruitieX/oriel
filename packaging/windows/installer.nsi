; oriel installer (NSIS). Built by scripts/package-windows.sh, which passes
; VERSION, PAYLOAD (staged program files), REPO and OUTFILE.
;
; Installs for every user in Program Files (administrator rights): the
; app, the hosting service (starts at boot; also shares the sign-in, lock
; and UAC screens), the firewall rule, Start-menu shortcuts and starting
; the tray app at sign-in. Settings, identity and pairings are kept on
; upgrade and uninstall.

Unicode true
!include "MUI2.nsh"
!include "LogicLib.nsh"
!include "Sections.nsh"
!include "x64.nsh"

!ifndef VERSION | PAYLOAD | REPO | OUTFILE
  !error "VERSION, PAYLOAD, REPO and OUTFILE must be defined"
!endif

!define APP "oriel"
!define REG_APP "Software\FruitieX\oriel"
!define REG_UNINSTALL "Software\Microsoft\Windows\CurrentVersion\Uninstall\oriel"
!define REG_RUN "Software\Microsoft\Windows\CurrentVersion\Run"

Name "oriel"
OutFile "${OUTFILE}"
InstallDir "$PROGRAMFILES64\oriel"
InstallDirRegKey HKLM "${REG_APP}" "InstallDir"
RequestExecutionLevel admin
SetCompressor /SOLID lzma
BrandingText "oriel ${VERSION}"

VIProductVersion "${VERSION}.0"
VIAddVersionKey "ProductName" "oriel"
VIAddVersionKey "CompanyName" "FruitieX"
VIAddVersionKey "FileDescription" "oriel installer"
VIAddVersionKey "FileVersion" "${VERSION}"
VIAddVersionKey "ProductVersion" "${VERSION}"
VIAddVersionKey "LegalCopyright" "FruitieX; MIT or Apache-2.0"

!define MUI_ICON "${REPO}/assets/oriel.ico"
!define MUI_UNICON "${REPO}/assets/oriel.ico"
!define MUI_ABORTWARNING
!define MUI_WELCOMEPAGE_TEXT "This installs oriel ${VERSION}: connect to your other computers, and share this one with your paired devices.$\r$\n$\r$\nIf oriel is running, setup closes it first."
!define MUI_COMPONENTSPAGE_SMALLDESC
!define MUI_FINISHPAGE_RUN
!define MUI_FINISHPAGE_RUN_TEXT "Open oriel"
!define MUI_FINISHPAGE_RUN_FUNCTION OpenApp

!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_COMPONENTS
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES
!insertmacro MUI_LANGUAGE "English"

Function .onInit
  ${IfNot} ${RunningX64}
    MessageBox MB_ICONSTOP "oriel needs 64-bit Windows." /SD IDOK
    Abort
  ${EndIf}
  SetRegView 64
  SetShellVarContext all
FunctionEnd

Function un.onInit
  SetRegView 64
  SetShellVarContext all
FunctionEnd

; Running copies lock the files: the service (and the host it starts) and
; anyone's tray app. Stop-Service waits until the service has stopped.
!macro StopOriel
  nsExec::Exec 'powershell.exe -NoProfile -NonInteractive -Command "Stop-Service -Name oriel -ErrorAction SilentlyContinue"'
  Pop $0
  nsExec::Exec 'taskkill.exe /F /IM oriel.exe'
  Pop $0
  Sleep 500
!macroend

; The tray app runs as the signed-in user, not elevated like setup.
Function OpenApp
  Exec '"$WINDIR\explorer.exe" "$INSTDIR\oriel.exe"'
FunctionEnd

Section "oriel (required)" SecApp
  SectionIn RO
  !insertmacro StopOriel
  SetOutPath "$INSTDIR"
  File /r "${PAYLOAD}/*"
  WriteUninstaller "$INSTDIR\uninstall.exe"
  CreateShortcut "$SMPROGRAMS\oriel.lnk" "$INSTDIR\oriel.exe" "" "$INSTDIR\oriel.exe" 0 SW_SHOWNORMAL "" "Remote desktop"
  WriteRegStr HKLM "${REG_APP}" "InstallDir" "$INSTDIR"
  WriteRegStr HKLM "${REG_UNINSTALL}" "DisplayName" "oriel"
  WriteRegStr HKLM "${REG_UNINSTALL}" "DisplayVersion" "${VERSION}"
  WriteRegStr HKLM "${REG_UNINSTALL}" "Publisher" "FruitieX"
  WriteRegStr HKLM "${REG_UNINSTALL}" "DisplayIcon" "$INSTDIR\oriel.exe"
  WriteRegStr HKLM "${REG_UNINSTALL}" "InstallLocation" "$INSTDIR"
  WriteRegStr HKLM "${REG_UNINSTALL}" "UninstallString" '"$INSTDIR\uninstall.exe"'
  WriteRegStr HKLM "${REG_UNINSTALL}" "QuietUninstallString" '"$INSTDIR\uninstall.exe" /S'
  WriteRegDWORD HKLM "${REG_UNINSTALL}" "NoModify" 1
  WriteRegDWORD HKLM "${REG_UNINSTALL}" "NoRepair" 1
SectionEnd

Section "Share this computer" SecService
  ; Registers and starts the service, the firewall rule (UDP 48600 on
  ; private networks) and Ctrl+Alt+Del for services.
  DetailPrint "Installing the oriel service…"
  nsExec::ExecToLog '"$INSTDIR\oriel.exe" service install'
  Pop $0
  ${If} $0 != 0
    MessageBox MB_ICONEXCLAMATION "The oriel service could not be installed (exit code $0). oriel still works for connecting to other computers." /SD IDOK
  ${EndIf}
SectionEnd

Section "Start oriel in the tray when signing in" SecTray
  WriteRegStr HKLM "${REG_RUN}" "oriel" '"$INSTDIR\oriel.exe" --background'
SectionEnd

Section /o "Desktop shortcut" SecDesktop
  CreateShortcut "$DESKTOP\oriel.lnk" "$INSTDIR\oriel.exe"
SectionEnd

; Undo what an earlier installation set up but is now deselected.
Section "-Cleanup"
  ${IfNot} ${SectionIsSelected} ${SecService}
    nsExec::Exec '"$INSTDIR\oriel.exe" service uninstall'
    Pop $0
  ${EndIf}
  ${IfNot} ${SectionIsSelected} ${SecTray}
    DeleteRegValue HKLM "${REG_RUN}" "oriel"
  ${EndIf}
  ${IfNot} ${SectionIsSelected} ${SecDesktop}
    Delete "$DESKTOP\oriel.lnk"
  ${EndIf}
SectionEnd

!insertmacro MUI_FUNCTION_DESCRIPTION_BEGIN
  !insertmacro MUI_DESCRIPTION_TEXT ${SecApp} "The oriel app, for connecting to your other computers."
  !insertmacro MUI_DESCRIPTION_TEXT ${SecService} "A service that shares this computer from startup, also on the sign-in, lock and UAC screens. Adds a firewall rule for UDP 48600 on private networks."
  !insertmacro MUI_DESCRIPTION_TEXT ${SecTray} "Start the oriel app in the notification area for every user."
  !insertmacro MUI_DESCRIPTION_TEXT ${SecDesktop} "Put an oriel shortcut on the desktop."
!insertmacro MUI_FUNCTION_DESCRIPTION_END

Section "Uninstall"
  nsExec::ExecToLog '"$INSTDIR\oriel.exe" service uninstall'
  Pop $0
  !insertmacro StopOriel
  ; Only our own files: the folder may have been chosen freely.
  Delete "$INSTDIR\oriel.exe"
  Delete "$INSTDIR\*.dll"
  Delete "$INSTDIR\*.old"
  RMDir /r "$INSTDIR\licenses"
  Delete "$INSTDIR\uninstall.exe"
  RMDir "$INSTDIR"
  Delete "$SMPROGRAMS\oriel.lnk"
  Delete "$DESKTOP\oriel.lnk"
  DeleteRegValue HKLM "${REG_RUN}" "oriel"
  ; The app's own "start at login" for this user.
  DeleteRegValue HKCU "${REG_RUN}" "oriel"
  DeleteRegKey HKLM "${REG_UNINSTALL}"
  DeleteRegKey HKLM "${REG_APP}"
  ; Kept: %ProgramData%\oriel (the service's identity and pairings) and
  ; each user's settings.
SectionEnd
