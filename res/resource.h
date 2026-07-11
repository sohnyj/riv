// 다이얼로그 리소스 ID (R6) — src/dialogs/resource.rs와 1:1 미러 유지
// (llvm-rc는 windows.h를 포함하지 않으므로 필요한 스타일 상수도 여기서 정의)

#ifndef RIV_RESOURCE_H
#define RIV_RESOURCE_H

// ── 다이얼로그 ──────────────────────────────────────────────────────────────
#define IDD_OPTIONS 100
#define IDD_PAGE_WINDOW 110
#define IDD_PAGE_IMAGE 120
#define IDD_PAGE_MISC 130
#define IDD_PAGE_SHORTCUTS 140
#define IDD_PAGE_ASSOCIATION 150
#define IDD_CAPTURE_KEYBOARD 160
#define IDD_CAPTURE_MOUSE 170
#define IDD_ABOUT 180

// ── 옵션 프레임 ─────────────────────────────────────────────────────────────
#define IDC_OPTIONS_TAB 1001
#define IDC_APPLY 1002
#define IDC_RESTORE_DEFAULTS 1003

// ── Window 탭 ───────────────────────────────────────────────────────────────
#define IDC_WINDOW_BGCOLOR_ENABLED 1101
#define IDC_WINDOW_BGCOLOR_BUTTON 1102
#define IDC_WINDOW_TITLEBAR_BASIC 1103
#define IDC_WINDOW_TITLEBAR_MINIMAL 1104
#define IDC_WINDOW_TITLEBAR_PRACTICAL 1105
#define IDC_WINDOW_FITMODE 1106
#define IDC_WINDOW_SAVE_POSITION 1107
#define IDC_WINDOW_CTRL_DRAG 1108

// ── Image 탭 ────────────────────────────────────────────────────────────────
#define IDC_IMAGE_FILTERING 1201
#define IDC_IMAGE_SCALEFACTOR_EDIT 1202
#define IDC_IMAGE_SCALEFACTOR_SPIN 1203
#define IDC_IMAGE_CURSOR_ZOOM 1204
#define IDC_IMAGE_FRACTIONAL_ZOOM 1205

// ── Miscellaneous 탭 ────────────────────────────────────────────────────────
#define IDC_MISC_SORT 1301
#define IDC_MISC_ASCENDING 1302
#define IDC_MISC_DESCENDING 1303
#define IDC_MISC_PRELOADING 1304
#define IDC_MISC_LOOP_FOLDERS 1305
#define IDC_MISC_SLIDESHOW_DIRECTION 1306
#define IDC_MISC_SLIDESHOW_TIMER_EDIT 1307
#define IDC_MISC_AFTER_DELETE 1308
#define IDC_MISC_ASK_DELETE 1309
#define IDC_MISC_MIME_DETECTION 1310
#define IDC_MISC_SAVE_RECENTS 1311
#define IDC_MISC_SKIP_HIDDEN 1312

// ── Shortcuts 탭 ────────────────────────────────────────────────────────────
#define IDC_SHORTCUTS_LIST 1401
#define IDC_SHORTCUTS_RESET 1402
#define IDC_SHORTCUTS_CLEAR_ALL 1403

// ── File Association 탭 ─────────────────────────────────────────────────────
#define IDC_ASSOC_TREE 1501
#define IDC_ASSOC_SELECT_ALL 1502
#define IDC_ASSOC_SELECT_NONE 1503

// ── 캡처 다이얼로그 ─────────────────────────────────────────────────────────
#define IDC_CAPTURE_KEY_FIELD 1601
#define IDC_CAPTURE_KEY_LIST 1602
#define IDC_CAPTURE_KEY_REMOVE 1603
#define IDC_CAPTURE_KEY_CLEAR 1604
#define IDC_CAPTURE_MOUSE_FIELD 1701
#define IDC_CAPTURE_MOUSE_CLEAR 1702

// ── About ───────────────────────────────────────────────────────────────────
#define IDC_ABOUT_TITLE 1801
#define IDC_ABOUT_VERSION 1802
#define IDC_ABOUT_LINK 1804

// ── 스타일 상수 (winuser.h·commctrl.h 발췌 — rc 전용) ───────────────────────
#define WS_POPUP 0x80000000L
#define WS_CAPTION 0x00C00000L
#define WS_SYSMENU 0x00080000L
#define WS_CHILD 0x40000000L
#define WS_BORDER 0x00800000L
#define WS_VSCROLL 0x00200000L
#define WS_TABSTOP 0x00010000L
#define WS_GROUP 0x00020000L
#define DS_MODALFRAME 0x80L
#define DS_CONTROL 0x0400L
#define CBS_DROPDOWNLIST 0x0003L
#define ES_AUTOHSCROLL 0x0080L
#define ES_NUMBER 0x2000L
#define LBS_NOTIFY 0x0001L
#define LBS_NOINTEGRALHEIGHT 0x0100L
#define BS_OWNERDRAW 0x000BL
#define LVS_REPORT 0x0001L
#define LVS_SINGLESEL 0x0004L
#define LVS_SHOWSELALWAYS 0x0008L
#define LVS_NOSORTHEADER 0x8000L
#define TVS_HASBUTTONS 0x0001L
#define TVS_LINESATROOT 0x0004L
#define TVS_DISABLEDRAGDROP 0x0010L
#define TVS_SHOWSELALWAYS 0x0020L
#define UDS_SETBUDDYINT 0x0002L
#define UDS_ALIGNRIGHT 0x0004L
#define UDS_AUTOBUDDY 0x0010L
#define UDS_ARROWKEYS 0x0020L
#define UDS_NOTHOUSANDS 0x0080L

#endif
