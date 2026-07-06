; ============================================================================
; KONG Installer — Windows x64
; ============================================================================

!include "MUI2.nsh"

; ── Product info ─────────────────────────────────────────────────────────────
!define PRODUCT_NAME    "KONG"
!define PRODUCT_VERSION "0.8.5"
!define PRODUCT_PUBLISHER "KONG Project"
!define EXE_NAME        "kong.exe"

Name "${PRODUCT_NAME} ${PRODUCT_VERSION}"
OutFile "Kong-${PRODUCT_VERSION}-windows-x64-setup.exe"
InstallDir "C:\kong"
InstallDirRegKey HKLM "Software\${PRODUCT_NAME}" "InstallDir"
RequestExecutionLevel admin

; ── MUI Settings ─────────────────────────────────────────────────────────────
!define MUI_ABORTWARNING

; ── Installer pages ──────────────────────────────────────────────────────────
!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_DIRECTORY          ; <-- asks the user for install path
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH

; ── Uninstaller pages ────────────────────────────────────────────────────────
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES

; ── Language ─────────────────────────────────────────────────────────────────
!insertmacro MUI_LANGUAGE "English"

; ============================================================================
; Install section
; ============================================================================
Section "Install" SEC_INSTALL
    SetOutPath "$INSTDIR"

    ; Copy the kong binary
    File "..\target\release\${EXE_NAME}"

    ; Create the store directory next to the binary
    CreateDirectory "$INSTDIR\store"

    ; ── Add to system PATH ───────────────────────────────────────────────────
    ; Use PowerShell to safely add to PATH — avoids NSIS 1024-byte ReadRegStr
    ; truncation bug that would destroy the entire system PATH.
    nsExec::ExecToLog 'powershell.exe -NoProfile -NonInteractive -Command \
        "$p = [System.Environment]::GetEnvironmentVariable(''PATH'',''Machine''); \
         if ($p -notlike ''*$INSTDIR*'') { \
             [System.Environment]::SetEnvironmentVariable(''PATH'', \
             $p + '';$INSTDIR'', ''Machine'') }"'

    ; Broadcast WM_SETTINGCHANGE so open shells pick up the new PATH
    SendMessage ${HWND_BROADCAST} ${WM_SETTINGCHANGE} 0 "STR:Environment" /TIMEOUT=5000

    ; ── Registry keys (for uninstall + future upgrades) ──────────────────────
    WriteRegStr HKLM "Software\${PRODUCT_NAME}" "InstallDir" "$INSTDIR"
    WriteRegStr HKLM "Software\${PRODUCT_NAME}" "Version"    "${PRODUCT_VERSION}"

    WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\${PRODUCT_NAME}" \
        "DisplayName"     "${PRODUCT_NAME} ${PRODUCT_VERSION}"
    WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\${PRODUCT_NAME}" \
        "UninstallString" "$\"$INSTDIR\uninstall.exe$\""
    WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\${PRODUCT_NAME}" \
        "InstallLocation" "$INSTDIR"
    WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\${PRODUCT_NAME}" \
        "Publisher"       "${PRODUCT_PUBLISHER}"
    WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\${PRODUCT_NAME}" \
        "DisplayVersion"  "${PRODUCT_VERSION}"
    WriteRegDWORD HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\${PRODUCT_NAME}" \
        "NoModify" 1
    WriteRegDWORD HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\${PRODUCT_NAME}" \
        "NoRepair" 1

    ; Size computed automatically from the File directives above
    SectionGetSize ${SEC_INSTALL} $0
    WriteRegDWORD HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\${PRODUCT_NAME}" \
        "EstimatedSize" $0

    ; Write uninstaller
    WriteUninstaller "$INSTDIR\uninstall.exe"

SectionEnd

; ============================================================================
; Uninstall section
; ============================================================================
Section "Uninstall"
    ; Remove kong.exe and uninstaller
    Delete "$INSTDIR\${EXE_NAME}"
    Delete "$INSTDIR\uninstall.exe"

    ; ── Remove from system PATH ──────────────────────────────────────────────
    ; Use PowerShell to safely remove — avoids NSIS 1024-byte truncation bug.
    nsExec::ExecToLog 'powershell.exe -NoProfile -NonInteractive -Command \
        "$p = [System.Environment]::GetEnvironmentVariable(''PATH'',''Machine''); \
         $p2 = ($p -split '';'' | Where-Object { $_ -ne ''$INSTDIR'' }) -join '';''; \
         [System.Environment]::SetEnvironmentVariable(''PATH'', $p2, ''Machine'')"'
    SendMessage ${HWND_BROADCAST} ${WM_SETTINGCHANGE} 0 "STR:Environment" /TIMEOUT=5000

    ; Remove registry entries
    DeleteRegKey HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\${PRODUCT_NAME}"
    DeleteRegKey HKLM "Software\${PRODUCT_NAME}"

    ; Ask user about the store
    MessageBox MB_YESNO "Keep the package store at $INSTDIR\store?$\n$\n\
        Click Yes to keep cached packages.$\n\
        Click No to delete everything." IDYES keep_store
        RMDir /r "$INSTDIR\store"
    keep_store:

    RMDir "$INSTDIR"  ; only removes if empty

SectionEnd

; ============================================================================
; StrContains — check if needle is in haystack
; Input:  Stack: haystack, needle
; Output: Stack: needle if found, "" if not
; ============================================================================
Function StrContains
    Exch $1 ; needle
    Exch
    Exch $2 ; haystack
    Push $3
    Push $4
    Push $5
    StrLen $3 $1
    StrCpy $4 0
  strcontains_loop:
    StrCpy $5 $2 $3 $4
    StrCmp $5 "" strcontains_notfound
    StrCmp $5 $1 strcontains_found
    IntOp $4 $4 + 1
    Goto strcontains_loop
  strcontains_notfound:
    StrCpy $1 ""
    Goto strcontains_done
  strcontains_found:
    ; found
  strcontains_done:
    Pop $5
    Pop $4
    Pop $3
    Pop $2
    Exch $1
FunctionEnd

; ============================================================================
; un.RemoveFromPath — remove a directory from semicolon-separated PATH
; Input:  Stack: full_path_string, dir_to_remove
; Output: Stack: cleaned_path_string
; ============================================================================
Function un.RemoveFromPath
    Exch $1  ; dir to remove
    Exch
    Exch $0  ; full PATH string
    Push $2  ; result accumulator
    Push $3  ; current segment
    Push $4  ; remaining input
    Push $5  ; temp lengths
    Push $6  ; position of ';'
    Push $7  ; temp

    StrCpy $2 ""
    StrCpy $4 $0

  unpath_loop:
    StrLen $5 $4
    IntCmp $5 0 unpath_done unpath_done

    ; Find next ";"
    Push $4
    Push ";"
    Call un.StrContains2
    Pop $6 ; remainder from ';' onward, or "" if no ';'

    StrCmp $6 "" unpath_last_seg

    ; Segment = input up to ';'
    StrLen $7 $6
    StrLen $5 $4
    IntOp $5 $5 - $7
    StrCpy $3 $4 $5       ; segment before ';'
    StrCpy $4 $6 "" 1     ; skip the ';'
    Goto unpath_check

  unpath_last_seg:
    StrCpy $3 $4
    StrCpy $4 ""

  unpath_check:
    ; Skip if this segment matches the dir to remove
    StrCmp $3 $1 unpath_skip
    StrCmp $2 "" 0 +3
      StrCpy $2 $3
      Goto unpath_skip
    StrCpy $2 "$2;$3"

  unpath_skip:
    StrCmp $4 "" unpath_done
    Goto unpath_loop

  unpath_done:
    StrCpy $0 $2
    Pop $7
    Pop $6
    Pop $5
    Pop $4
    Pop $3
    Pop $2
    Pop $1
    Exch $0
FunctionEnd

; Find needle in haystack, return from needle position onward (or "")
Function un.StrContains2
    Exch $1 ; needle
    Exch
    Exch $2 ; haystack
    Push $3
    Push $4
    Push $5
    StrLen $3 $1
    StrCpy $4 0
  un_sc2_loop:
    StrCpy $5 $2 $3 $4
    StrCmp $5 "" un_sc2_notfound
    StrCmp $5 $1 un_sc2_found
    IntOp $4 $4 + 1
    Goto un_sc2_loop
  un_sc2_notfound:
    StrCpy $1 ""
    Goto un_sc2_done
  un_sc2_found:
    StrCpy $1 $2 "" $4
  un_sc2_done:
    Pop $5
    Pop $4
    Pop $3
    Pop $2
    Exch $1
FunctionEnd
