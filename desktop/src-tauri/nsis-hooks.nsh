; Keep user data (settings, courses, WebView2 profile) when uninstalling.
;
; The default Tauri uninstaller offers a "delete app data" checkbox and wipes
; %LOCALAPPDATA%\chat.maic.openmaic when it is ticked. Clearing the state here
; makes an update/reinstall/repair never destroy the user's configuration.

!macro NSIS_HOOK_PREUNINSTALL
  StrCpy $DeleteAppDataCheckboxState 0
!macroend
