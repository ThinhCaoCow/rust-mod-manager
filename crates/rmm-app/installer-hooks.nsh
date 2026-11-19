; The installer's extra steps (tauri.conf.json > bundle.windows.nsis.installerHooks): rmm.exe, the
; command line, sits beside the window, and the install folder goes on the user's PATH so `rmm`
; works in any terminal. Per user: HKCU, no admin rights.
; Tauri's template includes StrFunc.nsh and declares StrLoc already.
${UnStrRep}

!macro RMM_ENV_CHANGED
  ; Tell open programs (Explorer, new terminals) that the environment changed.
  SendMessage 0xFFFF 0x1A 0 "STR:Environment" /TIMEOUT=5000
!macroend

; $0: a PATH. $1: that PATH with $INSTDIR added at the end, or "" when it has it already.
!macro RMM_PATH_ADD
  ${StrLoc} $1 ";$0;" ";$INSTDIR;" ">"
  ${If} $1 != ""
    StrCpy $1 ""
  ${ElseIf} $0 == ""
    StrCpy $1 "$INSTDIR"
  ${Else}
    StrCpy $1 "$0;$INSTDIR"
  ${EndIf}
!macroend

; $0: a PATH. $1: that PATH without $INSTDIR, wherever it is in the list. (Uninstaller only.)
!macro RMM_PATH_REMOVE
  ${UnStrRep} $1 ";$0;" ";$INSTDIR;" ";"
  StrCpy $1 $1 "" 1 ; the ends the search added
  StrCpy $1 $1 -1
!macroend

!macro NSIS_HOOK_POSTINSTALL
  ReadRegStr $0 HKCU "Environment" "Path"
  !insertmacro RMM_PATH_ADD
  ${If} $1 != ""
    WriteRegExpandStr HKCU "Environment" "Path" "$1"
    !insertmacro RMM_ENV_CHANGED
  ${EndIf}
!macroend

!macro NSIS_HOOK_POSTUNINSTALL
  ReadRegStr $0 HKCU "Environment" "Path"
  !insertmacro RMM_PATH_REMOVE
  ${If} $1 != $0
    WriteRegExpandStr HKCU "Environment" "Path" "$1"
    !insertmacro RMM_ENV_CHANGED
  ${EndIf}
!macroend
