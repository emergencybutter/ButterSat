; NSIS hooks for the Windows installer (design §11).
; - Installs the bundled, unmodified WinFsp MSI silently if WinFsp is missing.
; - Offers (never silently) to exclude xporthofs folders from Defender
;   real-time scanning, which otherwise reads every streamed texture.

!macro NSIS_HOOK_POSTINSTALL
  ReadRegStr $0 HKLM "SOFTWARE\WOW6432Node\WinFsp" "InstallDir"
  ${If} $0 == ""
    DetailPrint "Installing WinFsp (Windows File System Proxy)..."
    ExecWait 'msiexec /i "$INSTDIR\resources\winfsp.msi" /qn /norestart INSTALLLEVEL=1000' $1
    DetailPrint "WinFsp installer exit code: $1"
  ${EndIf}
  MessageBox MB_YESNO|MB_ICONQUESTION "Exclude the xporthofs cache and scenery folders from Microsoft Defender real-time scanning?$\r$\n$\r$\nRecommended: otherwise Defender reads every streamed texture, which triggers needless downloads." /SD IDNO IDNO skip_defender
    nsExec::ExecToLog 'powershell -NoProfile -ExecutionPolicy Bypass -Command "Add-MpPreference -ExclusionPath $\"$LOCALAPPDATA\xporthofs$\""'
  skip_defender:
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  ; Stop the daemon so the mount is released before files are removed.
  nsExec::ExecToLog '"$INSTDIR\xof-cli.exe" shutdown'
!macroend
