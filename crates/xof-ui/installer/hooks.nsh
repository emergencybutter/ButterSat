; NSIS hooks for the Windows installer (design §11).
; - Per-user install by default (`installMode: "currentUser"`), elevating via
;   `msiexec` only when WinFsp is missing.
; - Installs the bundled, unmodified WinFsp MSI silently if WinFsp is missing.
; - Offers (never silently) to exclude ButterSat folders from Defender
;   real-time scanning, which otherwise reads every streamed texture.
; - On uninstall, stops the daemon, unmounts, removes our `scenery_packs.ini`
;   entry (`z_buttersat`), and keeps user caches unless the user ticks
;   "remove cached data" (`DeleteAppDataCheckbox`).

!macro NSIS_HOOK_PREINSTALL
  ${If} ${FileExists} "$INSTDIR\xof-cli.exe"
    DetailPrint "Stopping running ButterSat daemon before upgrade..."
    nsExec::ExecToLog '"$INSTDIR\xof-cli.exe" shutdown'
  ${EndIf}
!macroend

!macro NSIS_HOOK_POSTINSTALL
  ReadRegStr $0 HKLM "SOFTWARE\WOW6432Node\WinFsp" "InstallDir"
  ${If} $0 == ""
    ReadRegStr $0 HKLM "SOFTWARE\WinFsp" "InstallDir"
  ${EndIf}
  ${If} $0 == ""
    DetailPrint "Installing WinFsp (Windows File System Proxy)..."
    nsExec::ExecToLog 'powershell -NoProfile -ExecutionPolicy Bypass -Command "exit (Start-Process msiexec.exe -Verb RunAs -Wait -PassThru -ArgumentList \"/i\", \"`\"$INSTDIR\resources\winfsp.msi`\"\", \"/qn\", \"/norestart\", \"INSTALLLEVEL=1000\").ExitCode"'
    Pop $1
    DetailPrint "WinFsp installer exit code: $1"
  ${EndIf}
  ${If} $UpdateMode <> 1
  ${AndIf} $PassiveMode <> 1
  ${AndIfNot} ${Silent}
    MessageBox MB_YESNO|MB_ICONQUESTION "Exclude the ButterSat cache and scenery folders from Microsoft Defender real-time scanning?$\r$\n$\r$\nRecommended: otherwise Defender reads every streamed texture, which triggers needless downloads." /SD IDNO IDNO skip_defender
      nsExec::ExecToLog 'powershell -NoProfile -ExecutionPolicy Bypass -Command "Start-Process powershell -Verb RunAs -WindowStyle Hidden -Wait -ArgumentList \"-NoProfile\", \"-ExecutionPolicy\", \"Bypass\", \"-Command\", \"Add-MpPreference -ExclusionPath `\"$LOCALAPPDATA\ButterSat`\"\""'
    skip_defender:
  ${EndIf}
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  ${If} $UpdateMode = 1
    ; Upgrade / auto-update: stop the daemon and release the mount, keep config & scenery_packs.ini.
    nsExec::ExecToLog '"$INSTDIR\xof-cli.exe" shutdown'
  ${ElseIf} $DeleteAppDataCheckboxState = 1
    ; Full uninstall with "Delete app data" checked: stop daemon, unmount, remove scenery_packs.ini entry, and delete caches.
    nsExec::ExecToLog '"$INSTDIR\xof-cli.exe" uninstall --remove-cache'
  ${Else}
    ; Standard uninstall: stop daemon, unmount, remove scenery_packs.ini entry, keep user caches.
    nsExec::ExecToLog '"$INSTDIR\xof-cli.exe" uninstall'
  ${EndIf}
!macroend

!macro NSIS_HOOK_POSTUNINSTALL
  ${If} $UpdateMode <> 1
  ${AndIf} $DeleteAppDataCheckboxState = 1
    RmDir /r "$LOCALAPPDATA\ButterSat"
    RmDir /r "$APPDATA\ButterSat"
  ${EndIf}
!macroend
