; Curator Server's scope-specific NSIS installer. The build helper supplies
; CURATOR_STAGE, PRODUCT_VERSION, and OUTPUT_FILE. ALL_USERS is defined only
; for the elevated machine-wide variant.

Unicode true
SetCompressor /SOLID lzma

!include "MUI2.nsh"
!include "LogicLib.nsh"
!include "nsDialogs.nsh"

!ifndef CURATOR_STAGE
  !error "CURATOR_STAGE must name a staged Curator Server directory"
!endif
!ifndef OUTPUT_FILE
  !error "OUTPUT_FILE must name the installer artifact"
!endif
!ifndef PRODUCT_VERSION
  !define PRODUCT_VERSION "0.3.0"
!endif

!ifdef ALL_USERS
  !define SERVER_SCOPE "AllUsers"
  !define SERVER_SCOPE_ARG "all-users"
  Name "Curator Server ${PRODUCT_VERSION} (All users)"
  InstallDir "$PROGRAMFILES\Curator Server"
  RequestExecutionLevel admin
!else
  !define SERVER_SCOPE "CurrentUser"
  !define SERVER_SCOPE_ARG "current-user"
  Name "Curator Server ${PRODUCT_VERSION}"
  InstallDir "$LOCALAPPDATA\Curator Server"
  RequestExecutionLevel user
!endif

OutFile "${OUTPUT_FILE}"

!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_DIRECTORY
Page custom PharPageCreate PharPageLeave
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES
!insertmacro MUI_LANGUAGE "English"

Var PharCheckbox
Var PharRequested

Function PharPageCreate
  nsDialogs::Create 1018
  Pop $0
  ${If} $0 == error
    Abort
  ${EndIf}
  ${NSD_CreateLabel} 0 0 100% 24u "Optional P-HAR setup"
  Pop $0
  ${NSD_CreateLabel} 0 26u 100% 30u "P-HAR is opt-in. Curator records this choice now and evaluates managed setup only after the Server starts; installer transactions never download models."
  Pop $0
  ${NSD_CreateCheckbox} 0 60u 100% 12u "Set up P-HAR after installation"
  Pop $PharCheckbox
  ${NSD_SetState} $PharCheckbox ${BST_UNCHECKED}
  nsDialogs::Show
FunctionEnd

Function PharPageLeave
  ${NSD_GetState} $PharCheckbox $PharRequested
FunctionEnd

Section "Curator Server" SecServer
  SetOutPath "$INSTDIR"
  File "${CURATOR_STAGE}\curator.exe"
  File "${CURATOR_STAGE}\Register-CuratorServer.ps1"
  File "${CURATOR_STAGE}\Unregister-CuratorServer.ps1"
  SetOutPath "$INSTDIR\static"
  File /r "${CURATOR_STAGE}\static\*.*"
  WriteUninstaller "$INSTDIR\Uninstall Curator Server.exe"

  ${If} $PharRequested == ${BST_CHECKED}
    ExecWait '"$SYSDIR\WindowsPowerShell\v1.0\powershell.exe" -NoLogo -NoProfile -ExecutionPolicy Bypass -File "$INSTDIR\Register-CuratorServer.ps1" -Scope "${SERVER_SCOPE}" -ServerPath "$INSTDIR\curator.exe" -EnablePhar' $0
  ${Else}
    ExecWait '"$SYSDIR\WindowsPowerShell\v1.0\powershell.exe" -NoLogo -NoProfile -ExecutionPolicy Bypass -File "$INSTDIR\Register-CuratorServer.ps1" -Scope "${SERVER_SCOPE}" -ServerPath "$INSTDIR\curator.exe"' $0
  ${EndIf}
  ${If} $0 != 0
    MessageBox MB_ICONSTOP "Curator Server files were installed, but background registration failed (exit $0). Run Register-CuratorServer.ps1 from this install directory to retry."
    SetErrorLevel $0
    Abort
  ${EndIf}
SectionEnd

Section "Uninstall"
  ExecWait '"$SYSDIR\WindowsPowerShell\v1.0\powershell.exe" -NoLogo -NoProfile -ExecutionPolicy Bypass -File "$INSTDIR\Unregister-CuratorServer.ps1" -Scope "${SERVER_SCOPE}"' $0
  ${If} $0 != 0
    MessageBox MB_ICONSTOP "The Curator Server registration could not be removed (exit $0). The application files and data were left in place."
    SetErrorLevel $0
    Abort
  ${EndIf}
  Delete "$INSTDIR\Uninstall Curator Server.exe"
  Delete "$INSTDIR\Register-CuratorServer.ps1"
  Delete "$INSTDIR\Unregister-CuratorServer.ps1"
  Delete "$INSTDIR\curator.exe"
  RMDir /r "$INSTDIR\static"
  RMDir "$INSTDIR"
SectionEnd
