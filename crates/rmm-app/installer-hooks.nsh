; The installer's extra steps (tauri.conf.json > bundle.windows.nsis.installerHooks): rmm.exe, the
; command line, sits beside the window, and the install folder goes on the user's PATH so `rmm`
; works in any terminal. Per user: HKCU, no admin rights.
;
; rmm.exe edits PATH itself (`rmm installer-path`): NSIS reads at most 1024 characters, reads a
; longer PATH as empty, and would write back only this folder.

!macro NSIS_HOOK_POSTINSTALL
  nsExec::Exec '"$INSTDIR\rmm.exe" installer-path "$INSTDIR"'
  Pop $0
!macroend

; Before the files go: rmm.exe does the work.
!macro NSIS_HOOK_PREUNINSTALL
  nsExec::Exec '"$INSTDIR\rmm.exe" installer-path --remove "$INSTDIR"'
  Pop $0
!macroend
