//! Settings dialog: seven tab pages editing a transient copy applied on OK/Apply.

use std::sync::OnceLock;
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleBitmap, CreateCompatibleDC, CreateSolidBrush, DFC_BUTTON, DFCS_BUTTON3STATE,
    DFCS_BUTTONCHECK, DFCS_CHECKED, DeleteDC, DeleteObject, DrawFrameControl, FillRect, FrameRect,
    GetDC, GetSysColorBrush, ReleaseDC, SelectObject,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::Dialogs::{CC_FULLOPEN, CC_RGBINIT, CHOOSECOLORW, ChooseColorW};
use windows::Win32::UI::Controls::{
    BST_CHECKED, BST_UNCHECKED, CheckDlgButton, CheckRadioButton, DRAWITEMSTRUCT, ETDT_ENABLE,
    ETDT_USETABTEXTURE, EnableThemeDialogTexture, HIMAGELIST, HTREEITEM, ILC_COLOR32, ILC_MASK,
    ImageList_Add, ImageList_Create, IsDlgButtonChecked, LVCF_TEXT, LVCF_WIDTH, LVCOLUMNW,
    LVIF_TEXT, LVITEMW, LVM_INSERTCOLUMNW, LVM_INSERTITEMW, LVM_SETEXTENDEDLISTVIEWSTYLE,
    LVM_SETITEMTEXTW, LVS_EX_DOUBLEBUFFER, LVS_EX_FULLROWSELECT, NM_CLICK, NM_DBLCLK, NM_RETURN,
    NMHDR, NMITEMACTIVATE, NMLINK, NMTVKEYDOWN, TCIF_TEXT, TCITEMW, TCM_ADJUSTRECT, TCM_GETCURSEL,
    TCM_INSERTITEMW, TCM_SETPADDING, TCN_SELCHANGE, TVGN_CARET, TVHITTESTINFO,
    TVHT_ONITEMSTATEICON, TVI_LAST, TVI_ROOT, TVIF_PARAM, TVIF_STATE, TVIF_TEXT, TVINSERTSTRUCTW,
    TVIS_STATEIMAGEMASK, TVITEMEXW, TVM_GETITEMW, TVM_GETNEXTITEM, TVM_HITTEST, TVM_INSERTITEMW,
    TVM_SETEXTENDEDSTYLE, TVM_SETIMAGELIST, TVM_SETITEMW, TVN_KEYDOWN, TVS_EX_DOUBLEBUFFER,
    TVSIL_STATE, UDM_SETRANGE32,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{EnableWindow, VK_SPACE};
use windows::Win32::UI::WindowsAndMessaging::{
    CB_ADDSTRING, CB_GETCURSEL, CB_SETCURSEL, CreateDialogParamW, DestroyWindow, EndDialog,
    GetClientRect, GetDlgItem, GetDlgItemInt, GetMessagePos, GetSystemMetrics, MapDialogRect,
    SM_CXVSCROLL, SW_HIDE, SW_SHOW, SWP_NOACTIVATE, SendDlgItemMessageW, SendMessageW,
    SetDlgItemTextW, SetWindowLongPtrW, SetWindowPos, ShowWindow, WM_APP, WM_COMMAND, WM_DESTROY,
    WM_DRAWITEM, WM_INITDIALOG, WM_NOTIFY,
};

use windows::core::HSTRING;

use crate::actions::Action;
use crate::bindings;
use crate::dialogs::about;
use crate::dialogs::resource::*;
use crate::dialogs::shortcut_capture;
use crate::image;
use crate::image::core::SortMode;
use crate::settings::{
    AFTER_DELETION_CHOICES, MAXIMUM_SLIDESHOW_INTERVAL_SECONDS, MAXIMUM_ZOOM_STEP_PERCENT,
    MINIMUM_SLIDESHOW_INTERVAL_SECONDS, MINIMUM_ZOOM_STEP_PERCENT, Options, PRELOADING_CHOICES,
    SLIDESHOW_DIRECTION_CHOICES, SettingsFile, TITLE_BAR_TEXT_CHOICES,
};
use crate::shell::{file_association, start_menu};
use crate::view::dither::DitherMode;
use crate::view::renderer::ScalingFilter;
use crate::view::transform::FitMode;
use crate::window::message::{high_word, low_word, point_from_packed};

pub const WM_APP_OPTIONS_APPLIED: u32 = WM_APP + 5;

pub struct AppliedOptions {
    /// This dialog owns a failure the main window reports: it is the window in front.
    pub dialog: HWND,
    pub options: Options,
    pub keyboard: Vec<(String, Vec<String>)>,
    pub mouse: Vec<(String, Vec<String>)>,
}

use crate::dialogs::modal::{DWLP_USER, IDCANCEL, IDOK};

const BN_CLICKED: usize = 0;
const CBN_SELCHANGE: usize = 1;
const EN_CHANGE: usize = 0x0300;
const EN_KILLFOCUS: usize = 0x0200;

const GROUP_FLAG: isize = 0x1000_0000;

const STATE_UNCHECKED: isize = 1;
const STATE_CHECKED: isize = 2;
const STATE_PARTIAL: isize = 3;

/// Shortcut list columns; the header order, the hit test, and the refresh share them.
const ACTION_COLUMN: i32 = 0;
const KEYBOARD_COLUMN: i32 = 1;
const MOUSE_COLUMN: i32 = 2;

/// Tri-state check cell edge; the image list, the bitmap, and both rectangles share it.
const STATE_IMAGE_EDGE_PIXELS: i32 = 16;

/// TVIS state image slot: the index sits above this shift (INDEXTOSTATEIMAGEMASK).
const STATE_IMAGE_SHIFT: u32 = 12;

/// State image for a leaf row; groups pick their own from the member count.
fn check_state(checked: bool) -> isize {
    if checked {
        STATE_CHECKED
    } else {
        STATE_UNCHECKED
    }
}

#[derive(Clone, PartialEq)]
struct ShortcutRow {
    action: Action,
    keyboard: Vec<String>,
    mouse: Vec<String>,
}

struct AssociationExtension {
    extension: String,
    checked: bool,
    item: HTREEITEM,
}

struct AssociationGroup {
    item: HTREEITEM,
    members: Vec<usize>,
}

struct OptionsState {
    parent: HWND,
    dialog: HWND,
    pages: [HWND; PAGES.len()],
    saved_options: Options,
    transient_options: Options,
    saved_shortcuts: Vec<ShortcutRow>,
    transient_shortcuts: Vec<ShortcutRow>,
    saved_associations: Vec<String>,
    /// Start Menu shortcut presence; no default concept, the on-disk state is the baseline.
    start_menu_saved: bool,
    start_menu_desired: bool,
    extensions: Vec<AssociationExtension>,
    groups: Vec<AssociationGroup>,
    /// Ignore control notifications during programmatic sync.
    syncing: bool,
    state_images: HIMAGELIST,
    custom_colors: [COLORREF; 16],
    /// Fonts owned by the About page, freed when the dialog closes.
    about_fonts: about::AboutFonts,
}

impl OptionsState {
    // Sorted so comparisons with saved_associations ignore presentation order.
    fn desired_associations(&self) -> Vec<String> {
        // An unbuilt page holds no extensions, which is not the same as none checked.
        if self.extensions.is_empty() {
            return self.saved_associations.clone();
        }
        let mut checked_extensions: Vec<String> = self
            .extensions
            .iter()
            .filter(|entry| entry.checked)
            .map(|entry| entry.extension.clone())
            .collect();
        checked_extensions.sort();
        checked_extensions
    }

    /// Apply enables when the transient state differs from the saved state.
    fn is_dirty(&self) -> bool {
        self.transient_options != self.saved_options
            || self.transient_shortcuts != self.saved_shortcuts
            || self.desired_associations() != self.saved_associations
            || self.start_menu_desired != self.start_menu_saved
    }

    fn differs_from_defaults(&self) -> bool {
        self.transient_options != Options::default()
            || self.transient_shortcuts != default_shortcut_rows()
    }
}

/// Sorted, because `is_dirty` compares this against the equally sorted desired set.
fn registered_associations() -> Vec<String> {
    let mut associations = file_association::registered_extensions();
    associations.sort();
    associations
}

/// The defaults never move, and `update_buttons` compares against them on every keystroke.
fn default_shortcut_rows() -> &'static [ShortcutRow] {
    static ROWS: OnceLock<Vec<ShortcutRow>> = OnceLock::new();
    ROWS.get_or_init(|| {
        Action::all_bindable()
            .map(|action| ShortcutRow {
                action,
                keyboard: bindings::default_keyboard_sequences(action.name())
                    .iter()
                    .map(|sequence| (*sequence).to_string())
                    .collect(),
                mouse: bindings::default_mouse_encodings(action.name())
                    .iter()
                    .map(|encoding| (*encoding).to_string())
                    .collect(),
            })
            .collect()
    })
}

/// What the dialog reads from the settings, taken before the modal loop starts.
pub struct OptionsSnapshot {
    options: Options,
    shortcuts: Vec<ShortcutRow>,
}

impl OptionsSnapshot {
    pub fn capture(settings: &SettingsFile) -> Self {
        Self {
            options: settings.options.clone(),
            shortcuts: Action::all_bindable()
                .map(|action| ShortcutRow {
                    action,
                    keyboard: bindings::resolved_keyboard_sequences(
                        settings.keyboard_bindings(),
                        action.name(),
                    ),
                    mouse: bindings::resolved_mouse_encodings(
                        settings.mouse_bindings(),
                        action.name(),
                    ),
                })
                .collect(),
        }
    }
}

pub fn show(parent: HWND, snapshot: OptionsSnapshot) {
    let OptionsSnapshot { options, shortcuts } = snapshot;
    let saved_associations = registered_associations();
    let start_menu_present = start_menu::shortcut_exists();
    let mut state = OptionsState {
        parent,
        dialog: HWND::default(),
        pages: [HWND::default(); PAGES.len()],
        saved_options: options.clone(),
        transient_options: options,
        saved_shortcuts: shortcuts.clone(),
        transient_shortcuts: shortcuts,
        saved_associations,
        start_menu_saved: start_menu_present,
        start_menu_desired: start_menu_present,
        extensions: Vec::new(),
        groups: Vec::new(),
        syncing: false,
        state_images: HIMAGELIST::default(),
        custom_colors: [COLORREF(0x00FF_FFFF); 16],
        about_fonts: about::AboutFonts::default(),
    };
    crate::dialogs::modal::run_modal(
        parent,
        IDD_OPTIONS,
        frame_procedure,
        &raw mut state as isize,
    );
}

fn state_mut(dialog: HWND) -> Option<&'static mut OptionsState> {
    crate::dialogs::modal::state_mut(dialog)
}

unsafe extern "system" fn frame_procedure(
    dialog: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> isize {
    match message {
        WM_INITDIALOG => {
            unsafe { SetWindowLongPtrW(dialog, DWLP_USER, lparam.0) };
            let state = unsafe { &mut *(lparam.0 as *mut OptionsState) };
            state.dialog = dialog;
            initialize_frame(state);
            if let Ok(tab) = unsafe { GetDlgItem(Some(dialog), IDC_OPTIONS_TAB) } {
                select_page(dialog, tab, 0);
            }
            1
        }
        WM_NOTIFY => {
            let header = unsafe { &*(lparam.0 as *const NMHDR) };
            if header.idFrom == IDC_OPTIONS_TAB as usize && header.code == TCN_SELCHANGE {
                let selected =
                    unsafe { SendMessageW(header.hwndFrom, TCM_GETCURSEL, None, None).0 };
                select_page(dialog, header.hwndFrom, selected);
                if let Ok(index) = usize::try_from(selected) {
                    sync_number_edit(dialog, index);
                }
            }
            0
        }
        WM_COMMAND => {
            let command = low_word(wparam.0) as i32;
            match command {
                command if command == IDOK as i32 => {
                    if let Some(state) = state_mut(dialog) {
                        apply(state);
                    }
                    let _ = unsafe { EndDialog(dialog, IDOK as isize) };
                    1
                }
                command if command == IDCANCEL as i32 => {
                    let _ = unsafe { EndDialog(dialog, IDCANCEL as isize) };
                    1
                }
                IDC_APPLY => {
                    if let Some(state) = state_mut(dialog) {
                        apply(state);
                    }
                    1
                }
                IDC_RESTORE_DEFAULTS => {
                    if let Some(state) = state_mut(dialog) {
                        state.transient_options = Options::default();
                        state.transient_shortcuts = default_shortcut_rows().to_vec();
                        sync_all_pages(state);
                        update_buttons(state);
                    }
                    sync_number_edit(dialog, IMAGE_PAGE);
                    sync_number_edit(dialog, MISCELLANEOUS_PAGE);
                    1
                }
                _ => 0,
            }
        }
        WM_DESTROY => {
            let Some((pages, state_images)) =
                state_mut(dialog).map(|state| (state.pages, state.state_images))
            else {
                return 0;
            };
            // Page destruction can send focus notifications back here; no borrow spans it.
            for page in pages {
                if !page.is_invalid() {
                    let _ = unsafe { DestroyWindow(page) };
                }
            }
            if !state_images.is_invalid() {
                let _ =
                    unsafe { windows::Win32::UI::Controls::ImageList_Destroy(Some(state_images)) };
            }
            if let Some(state) = state_mut(dialog) {
                state.about_fonts.destroy();
            }
            0
        }
        _ => 0,
    }
}

fn initialize_frame(state: &mut OptionsState) {
    let dialog = state.dialog;
    crate::dialogs::geometry::center_on_owner(dialog);
    let Ok(tab) = (unsafe { GetDlgItem(Some(dialog), IDC_OPTIONS_TAB) }) else {
        return;
    };
    for (index, &(_, title)) in PAGES.iter().enumerate() {
        let text = HSTRING::from(title);
        let item = TCITEMW {
            mask: TCIF_TEXT,
            pszText: windows::core::PWSTR(text.as_ptr().cast_mut()),
            ..Default::default()
        };
        unsafe {
            SendMessageW(
                tab,
                TCM_INSERTITEMW,
                Some(WPARAM(index)),
                Some(LPARAM(&raw const item as isize)),
            )
        };
    }

    // Labels sit tight against the tab edges at the default padding.
    let mut padding = RECT {
        left: 0,
        top: 0,
        right: 8,
        bottom: 0,
    };
    if unsafe { MapDialogRect(dialog, &raw mut padding) }.is_ok() {
        let sizes = ((padding.bottom as isize) << 16) | padding.right as isize;
        unsafe {
            SendMessageW(tab, TCM_SETPADDING, Some(WPARAM(0)), Some(LPARAM(sizes)));
        }
    }

    update_buttons(state);
}

/// Where a page sits inside the tab, in the frame's coordinates.
fn page_area(dialog: HWND, tab: HWND) -> RECT {
    // TCM_ADJUSTRECT only insets, so it reads the same before or after the mapping.
    let mut area = crate::dialogs::geometry::control_bounds(dialog, tab).unwrap_or_default();
    unsafe {
        SendMessageW(
            tab,
            TCM_ADJUSTRECT,
            Some(WPARAM(0)),
            Some(LPARAM(&raw mut area as isize)),
        )
    };
    area
}

fn select_page(dialog: HWND, tab: HWND, selected: isize) {
    if let Ok(index) = usize::try_from(selected)
        && index < PAGES.len()
        && let Some(state) = state_mut(dialog)
    {
        ensure_page(state, tab, index);
    }
    show_page(dialog, selected);
}

const PAGES: [(u16, &str); 7] = [
    (IDD_PAGE_WINDOW, "Window"),
    (IDD_PAGE_IMAGE, "Image"),
    (IDD_PAGE_MISCELLANEOUS, "Miscellaneous"),
    (IDD_PAGE_SHORTCUTS, "Shortcuts"),
    (IDD_PAGE_ASSOCIATION, "File association"),
    (IDD_PAGE_START_MENU, "Start menu"),
    (IDD_PAGE_ABOUT, "About"),
];

/// A page's slot in `pages`, derived from the table so a reorder moves every index with it.
const fn page_position(template: u16) -> usize {
    let mut index = 0;
    while index < PAGES.len() {
        if PAGES[index].0 == template {
            return index;
        }
        index += 1;
    }
    panic!("the template is not in PAGES");
}
const WINDOW_PAGE: usize = page_position(IDD_PAGE_WINDOW);
const IMAGE_PAGE: usize = page_position(IDD_PAGE_IMAGE);
const MISCELLANEOUS_PAGE: usize = page_position(IDD_PAGE_MISCELLANEOUS);
const SHORTCUTS_PAGE: usize = page_position(IDD_PAGE_SHORTCUTS);
const ASSOCIATION_PAGE: usize = page_position(IDD_PAGE_ASSOCIATION);
const START_MENU_PAGE: usize = page_position(IDD_PAGE_START_MENU);
const ABOUT_PAGE: usize = page_position(IDD_PAGE_ABOUT);

fn ensure_page(state: &mut OptionsState, tab: HWND, index: usize) {
    if !state.pages[index].is_invalid() {
        return;
    }
    let instance =
        unsafe { GetModuleHandleW(None) }.expect("the module handle of the running module");
    let state_pointer = state as *mut OptionsState as isize;
    let page = unsafe {
        CreateDialogParamW(
            Some(instance.into()),
            crate::dialogs::resource::template_name(PAGES[index].0),
            Some(state.dialog),
            Some(page_procedure),
            LPARAM(state_pointer),
        )
    }
    .unwrap_or_default();
    let area = page_area(state.dialog, tab);
    let _ = unsafe {
        SetWindowPos(
            page,
            Some(tab),
            area.left,
            area.top,
            area.right - area.left,
            area.bottom - area.top,
            windows::Win32::UI::WindowsAndMessaging::SET_WINDOW_POS_FLAGS(0),
        )
    };
    state.pages[index] = page;
    match index {
        WINDOW_PAGE => initialize_window_page(state),
        IMAGE_PAGE => initialize_image_page(state),
        MISCELLANEOUS_PAGE => initialize_miscellaneous_page(state),
        SHORTCUTS_PAGE => {
            fit_page_controls(page, IDC_SHORTCUTS_LIST, IDC_SHORTCUTS_CLEAR_ALL);
            initialize_shortcuts_page(state);
        }
        ASSOCIATION_PAGE => {
            fit_page_controls(page, IDC_ASSOCIATION_TREE, IDC_ASSOCIATION_SELECT_NONE);
            initialize_association_page(state);
        }
        ABOUT_PAGE => state.about_fonts = about::initialize_page(page),
        _ => {}
    }
    sync_page(state, index);
}

/// The selected page goes up before the others go down, so the bare tab never shows.
fn show_page(dialog: HWND, selected: isize) {
    let Some(pages) = state_mut(dialog).map(|state| state.pages) else {
        return;
    };
    // No borrow spans the shows: a synchronously delivered draw message would take a second &mut.
    let visible = usize::try_from(selected).ok();
    if let Some(page) = visible.and_then(|index| pages.get(index)) {
        let _ = unsafe { ShowWindow(*page, SW_SHOW) };
    }
    for (index, page) in pages.iter().enumerate() {
        if Some(index) != visible {
            let _ = unsafe { ShowWindow(*page, SW_HIDE) };
        }
    }
}

/// The tab's inner area runs a little wider than the authored template size.
fn fit_page_controls(page: HWND, stretch: i32, follow_right_edge: i32) {
    // Both measurements must be taken here: the page carries its final font and DPI now.
    let mut template = RECT {
        left: 0,
        top: 0,
        right: PAGE_TEMPLATE_WIDTH_DIALOG_UNITS,
        bottom: PAGE_TEMPLATE_HEIGHT_DIALOG_UNITS,
    };
    let mut client = RECT::default();
    if unsafe { MapDialogRect(page, &raw mut template) }.is_err()
        || unsafe { GetClientRect(page, &raw mut client) }.is_err()
    {
        return;
    }
    let widen = client.right - template.right;
    let heighten = client.bottom - template.bottom;
    let place = |control: i32, offset_x: i32, extra_width: i32, extra_height: i32| {
        let Ok(handle) = (unsafe { GetDlgItem(Some(page), control) }) else {
            return;
        };
        let Some(bounds) = crate::dialogs::geometry::control_bounds(page, handle) else {
            return;
        };
        let _ = unsafe {
            SetWindowPos(
                handle,
                None,
                bounds.left + offset_x,
                bounds.top,
                bounds.right - bounds.left + extra_width,
                bounds.bottom - bounds.top + extra_height,
                windows::Win32::UI::WindowsAndMessaging::SWP_NOZORDER | SWP_NOACTIVATE,
            )
        };
    };
    place(stretch, 0, widen, heighten);
    place(follow_right_edge, widen, 0, 0);
}

fn update_buttons(state: &OptionsState) {
    let enable = |control: i32, enabled: bool| {
        if let Ok(button) = unsafe { GetDlgItem(Some(state.dialog), control) } {
            let _ = unsafe { EnableWindow(button, enabled) };
        }
    };
    enable(IDC_APPLY, state.is_dirty());
    enable(IDC_RESTORE_DEFAULTS, state.differs_from_defaults());
}

fn apply(state: &mut OptionsState) {
    if !state.is_dirty() {
        return;
    }
    // Saved sets are re-probed: a failed write keeps Apply enabled, not shown as saved.
    let desired = state.desired_associations();
    if desired != state.saved_associations {
        file_association::set_file_associations(&desired);
        state.saved_associations = registered_associations();
    }
    if state.start_menu_desired != state.start_menu_saved {
        if state.start_menu_desired {
            start_menu::create_shortcut();
        } else {
            start_menu::remove_shortcut();
        }
        state.start_menu_saved = start_menu::shortcut_exists();
    }
    let payload = AppliedOptions {
        dialog: state.dialog,
        options: state.transient_options.clone(),
        keyboard: state
            .transient_shortcuts
            .iter()
            .map(|row| (row.action.name().to_string(), row.keyboard.clone()))
            .collect(),
        mouse: state
            .transient_shortcuts
            .iter()
            .map(|row| (row.action.name().to_string(), row.mouse.clone()))
            .collect(),
    };
    crate::window::message::send_borrowed(state.parent, WM_APP_OPTIONS_APPLIED, &payload);
    state.saved_options = state.transient_options.clone();
    state.saved_shortcuts = state.transient_shortcuts.clone();
    update_buttons(state);
}

unsafe extern "system" fn page_procedure(
    page: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> isize {
    match message {
        WM_INITDIALOG => {
            unsafe { SetWindowLongPtrW(page, DWLP_USER, lparam.0) };
            // The tab texture is the page background a property sheet gives its pages.
            let _ = unsafe { EnableThemeDialogTexture(page, ETDT_ENABLE | ETDT_USETABTEXTURE) };
            1
        }
        WM_COMMAND => {
            let Some(state) = state_mut(page) else {
                return 0;
            };
            if state.syncing {
                return 0;
            }
            let control = low_word(wparam.0) as i32;
            let notification = high_word(wparam.0) as usize;
            if control == IDC_WINDOW_BACKGROUND_COLOR_BUTTON && notification == BN_CLICKED {
                // Runs a modal that re-enters this procedure; it borrows the state in stages.
                choose_background_color(page);
                return 1;
            }
            if notification == EN_KILLFOCUS {
                // The write-back re-enters this procedure too; same staged borrows.
                return show_clamped_number(page, control);
            }
            apply_page_command(state, page, control, notification)
        }
        WM_NOTIFY => {
            // State only inside a matched branch: item insertions notify here while an initializer's borrow is live.
            let header = unsafe { &*(lparam.0 as *const NMHDR) };
            match header.idFrom as i32 {
                IDC_SHORTCUTS_LIST if header.code == NM_DBLCLK => {
                    let activate = unsafe { &*(lparam.0 as *const NMITEMACTIVATE) };
                    if activate.iItem >= 0 {
                        edit_shortcut(
                            page,
                            activate.iItem as usize,
                            activate.iSubItem == MOUSE_COLUMN,
                        );
                    }
                    1
                }
                IDC_ASSOCIATION_TREE if header.code == NM_CLICK => {
                    let Some(state) = state_mut(page) else {
                        return 0;
                    };
                    toggle_association_at_cursor(state, header.hwndFrom);
                    1
                }
                IDC_ABOUT_LINK if header.code == NM_CLICK || header.code == NM_RETURN => {
                    let link = unsafe { &*(lparam.0 as *const NMLINK) };
                    about::open_notified_link(link);
                    1
                }
                IDC_ASSOCIATION_TREE if header.code == TVN_KEYDOWN => {
                    let key_down = unsafe { &*(lparam.0 as *const NMTVKEYDOWN) };
                    if key_down.wVKey == VK_SPACE.0 {
                        let selected = unsafe {
                            SendMessageW(
                                header.hwndFrom,
                                TVM_GETNEXTITEM,
                                Some(WPARAM(TVGN_CARET as usize)),
                                None,
                            )
                        };
                        if selected.0 != 0
                            && let Some(state) = state_mut(page)
                        {
                            toggle_association_item(state, header.hwndFrom, HTREEITEM(selected.0));
                        }
                    }
                    1
                }
                _ => 0,
            }
        }
        WM_DRAWITEM => {
            let Some(state) = state_mut(page) else {
                return 0;
            };
            let draw = unsafe { &*(lparam.0 as *const DRAWITEMSTRUCT) };
            if draw.CtlID == IDC_WINDOW_BACKGROUND_COLOR_BUTTON as u32 {
                let color = rgb_to_colorref(state.transient_options.background_color);
                crate::dialogs::paint::draw_buffered(draw.hDC, draw.rcItem, |device| unsafe {
                    let brush = CreateSolidBrush(color);
                    FillRect(device, &raw const draw.rcItem, brush);
                    FrameRect(
                        device,
                        &raw const draw.rcItem,
                        GetSysColorBrush(windows::Win32::Graphics::Gdi::COLOR_BTNSHADOW),
                    );
                    let _ = DeleteObject(brush.into());
                });
                return 1;
            }
            0
        }
        _ => 0,
    }
}

fn apply_page_command(
    state: &mut OptionsState,
    page: HWND,
    control: i32,
    notification: usize,
) -> isize {
    let options = &mut state.transient_options;
    let mut handled = true;
    match (control, notification) {
        (IDC_WINDOW_BACKGROUND_COLOR_ENABLED, BN_CLICKED) => {
            options.background_color_enabled = is_checked(page, control);
            sync_background_color_button(state, page);
        }
        (IDC_WINDOW_TITLE_BAR_TEXT, CBN_SELCHANGE) => {
            options.title_bar_text = combo_selection(page, control);
        }
        (IDC_IMAGE_FIT_MODE, CBN_SELCHANGE) => {
            options.fit_mode = combo_selection(page, control);
        }
        (IDC_WINDOW_REMEMBER_WINDOW_PLACEMENT, BN_CLICKED) => {
            options.remember_window_placement = is_checked(page, control);
        }
        (IDC_WINDOW_CONTROL_DRAG, BN_CLICKED) => {
            options.control_drag_window = is_checked(page, control);
        }
        (IDC_WINDOW_HIDE_CURSOR_FULLSCREEN, BN_CLICKED) => {
            options.hide_cursor_fullscreen = is_checked(page, control);
        }
        (IDC_IMAGE_SCALING, CBN_SELCHANGE) => {
            options.scaling_filter = combo_selection(page, control);
        }
        (IDC_IMAGE_DITHER, CBN_SELCHANGE) => {
            options.dither_mode = combo_selection(page, control);
        }
        (IDC_IMAGE_ZOOM_STEP_EDIT, EN_CHANGE) => {
            let value = unsafe { GetDlgItemInt(page, control, None, false) };
            options.zoom_step_percent =
                value.clamp(MINIMUM_ZOOM_STEP_PERCENT, MAXIMUM_ZOOM_STEP_PERCENT);
        }
        (IDC_IMAGE_CURSOR_ZOOM, BN_CLICKED) => {
            options.cursor_zoom = is_checked(page, control);
        }
        (IDC_IMAGE_FRACTIONAL_WHEEL_ZOOM, BN_CLICKED) => {
            options.fractional_wheel_zoom = is_checked(page, control);
        }
        (IDC_MISCELLANEOUS_SORT, CBN_SELCHANGE) => {
            options.sort_files_by = combo_selection(page, control)
        }
        (IDC_MISCELLANEOUS_ASCENDING, BN_CLICKED) => options.sort_descending = false,
        (IDC_MISCELLANEOUS_DESCENDING, BN_CLICKED) => options.sort_descending = true,
        (IDC_IMAGE_PRELOADING, CBN_SELCHANGE) => {
            options.preloading = combo_selection(page, control);
        }
        (IDC_MISCELLANEOUS_LOOP_WITHIN_FOLDER, BN_CLICKED) => {
            options.loop_within_folder = is_checked(page, control);
        }
        (IDC_MISCELLANEOUS_SLIDESHOW_DIRECTION, CBN_SELCHANGE) => {
            options.slideshow_direction = combo_selection(page, control);
        }
        (IDC_MISCELLANEOUS_SLIDESHOW_INTERVAL_EDIT, EN_CHANGE) => {
            let value = unsafe { GetDlgItemInt(page, control, None, false) };
            options.slideshow_interval_seconds = value.clamp(
                MINIMUM_SLIDESHOW_INTERVAL_SECONDS,
                MAXIMUM_SLIDESHOW_INTERVAL_SECONDS,
            );
        }
        (IDC_MISCELLANEOUS_AFTER_DELETION, CBN_SELCHANGE) => {
            options.after_deletion = combo_selection(page, control);
        }
        (IDC_MISCELLANEOUS_ASK_DELETE, BN_CLICKED) => {
            options.ask_delete = is_checked(page, control)
        }
        (IDC_MISCELLANEOUS_DETECT_FORMAT_BY_CONTENT, BN_CLICKED) => {
            options.detect_format_by_content = is_checked(page, control);
        }
        (IDC_MISCELLANEOUS_REMEMBER_RECENTS, BN_CLICKED) => {
            options.remember_recents = is_checked(page, control)
        }
        (IDC_MISCELLANEOUS_SKIP_HIDDEN, BN_CLICKED) => {
            options.skip_hidden = is_checked(page, control)
        }
        (IDC_SHORTCUTS_RESET, BN_CLICKED) => {
            state.transient_shortcuts = default_shortcut_rows().to_vec();
            refresh_shortcut_rows(state);
        }
        (IDC_SHORTCUTS_CLEAR_ALL, BN_CLICKED) => {
            for row in &mut state.transient_shortcuts {
                row.keyboard.clear();
                row.mouse.clear();
            }
            refresh_shortcut_rows(state);
        }
        (IDC_ASSOCIATION_SELECT_ALL, BN_CLICKED) => set_all_associations(state, true),
        (IDC_ASSOCIATION_SELECT_NONE, BN_CLICKED) => set_all_associations(state, false),
        (IDC_START_MENU_SHORTCUT, BN_CLICKED) => {
            state.start_menu_desired = is_checked(page, control);
        }
        _ => handled = false,
    }
    if handled {
        update_buttons(state);
        1
    } else {
        0
    }
}

fn initialize_window_page(state: &OptionsState) {
    let page = state.pages[WINDOW_PAGE];
    combo_fill(page, IDC_WINDOW_TITLE_BAR_TEXT, &TITLE_BAR_TEXT_CHOICES);
}

fn initialize_image_page(state: &OptionsState) {
    let page = state.pages[IMAGE_PAGE];
    combo_fill(
        page,
        IDC_IMAGE_SCALING,
        &ScalingFilter::IN_SETTING_ORDER.map(ScalingFilter::description),
    );
    combo_fill(
        page,
        IDC_IMAGE_DITHER,
        &DitherMode::IN_SETTING_ORDER.map(DitherMode::description),
    );
    combo_fill(
        page,
        IDC_IMAGE_FIT_MODE,
        &FitMode::IN_SETTING_ORDER.map(FitMode::description),
    );
    combo_fill(page, IDC_IMAGE_PRELOADING, &PRELOADING_CHOICES);
    if let Ok(spin) = unsafe { GetDlgItem(Some(page), IDC_IMAGE_ZOOM_STEP_SPIN) } {
        unsafe {
            SendMessageW(
                spin,
                UDM_SETRANGE32,
                Some(WPARAM(MINIMUM_ZOOM_STEP_PERCENT as usize)),
                Some(LPARAM(MAXIMUM_ZOOM_STEP_PERCENT as isize)),
            )
        };
    }
}

fn initialize_miscellaneous_page(state: &OptionsState) {
    let page = state.pages[MISCELLANEOUS_PAGE];
    combo_fill(
        page,
        IDC_MISCELLANEOUS_SORT,
        &SortMode::IN_SETTING_ORDER.map(SortMode::description),
    );
    combo_fill(
        page,
        IDC_MISCELLANEOUS_SLIDESHOW_DIRECTION,
        &SLIDESHOW_DIRECTION_CHOICES,
    );
    combo_fill(
        page,
        IDC_MISCELLANEOUS_AFTER_DELETION,
        &AFTER_DELETION_CHOICES,
    );
    if let Ok(spin) = unsafe { GetDlgItem(Some(page), IDC_MISCELLANEOUS_SLIDESHOW_INTERVAL_SPIN) } {
        unsafe {
            SendMessageW(
                spin,
                UDM_SETRANGE32,
                Some(WPARAM(MINIMUM_SLIDESHOW_INTERVAL_SECONDS as usize)),
                Some(LPARAM(MAXIMUM_SLIDESHOW_INTERVAL_SECONDS as isize)),
            )
        };
    }
}

fn sync_all_pages(state: &mut OptionsState) {
    for index in 0..PAGES.len() {
        sync_page(state, index);
    }
}

fn sync_page(state: &mut OptionsState, index: usize) {
    state.syncing = true;
    match index {
        WINDOW_PAGE => sync_window_page(state),
        IMAGE_PAGE => sync_image_page(state),
        MISCELLANEOUS_PAGE => sync_miscellaneous_page(state),
        START_MENU_PAGE => sync_start_menu_page(state),
        _ => {}
    }
    state.syncing = false;
    if index == SHORTCUTS_PAGE {
        refresh_shortcut_rows(state);
    }
}

/// Writing a number edit re-enters through EN_CHANGE, so callers run this after their borrow ends.
fn sync_number_edit(dialog: HWND, index: usize) {
    let control = match index {
        IMAGE_PAGE => IDC_IMAGE_ZOOM_STEP_EDIT,
        MISCELLANEOUS_PAGE => IDC_MISCELLANEOUS_SLIDESHOW_INTERVAL_EDIT,
        _ => return,
    };
    let Some(page) = state_mut(dialog).map(|state| state.pages[index]) else {
        return;
    };
    if !page.is_invalid() {
        show_clamped_number(page, control);
    }
}

fn sync_window_page(state: &OptionsState) {
    let options = &state.transient_options;
    let window_page = state.pages[WINDOW_PAGE];
    set_check(
        window_page,
        IDC_WINDOW_BACKGROUND_COLOR_ENABLED,
        options.background_color_enabled,
    );
    combo_select(
        window_page,
        IDC_WINDOW_TITLE_BAR_TEXT,
        options.title_bar_text,
    );
    set_check(
        window_page,
        IDC_WINDOW_REMEMBER_WINDOW_PLACEMENT,
        options.remember_window_placement,
    );
    set_check(
        window_page,
        IDC_WINDOW_CONTROL_DRAG,
        options.control_drag_window,
    );
    set_check(
        window_page,
        IDC_WINDOW_HIDE_CURSOR_FULLSCREEN,
        options.hide_cursor_fullscreen,
    );
    sync_background_color_button(state, window_page);
}

fn sync_image_page(state: &OptionsState) {
    let options = &state.transient_options;
    let image_page = state.pages[IMAGE_PAGE];
    combo_select(image_page, IDC_IMAGE_SCALING, options.scaling_filter);
    combo_select(image_page, IDC_IMAGE_DITHER, options.dither_mode);
    combo_select(image_page, IDC_IMAGE_FIT_MODE, options.fit_mode);
    combo_select(image_page, IDC_IMAGE_PRELOADING, options.preloading);
    set_check(image_page, IDC_IMAGE_CURSOR_ZOOM, options.cursor_zoom);
    set_check(
        image_page,
        IDC_IMAGE_FRACTIONAL_WHEEL_ZOOM,
        options.fractional_wheel_zoom,
    );
}

fn sync_miscellaneous_page(state: &OptionsState) {
    let options = &state.transient_options;
    let miscellaneous_page = state.pages[MISCELLANEOUS_PAGE];
    combo_select(
        miscellaneous_page,
        IDC_MISCELLANEOUS_SORT,
        options.sort_files_by,
    );
    let _ = unsafe {
        CheckRadioButton(
            miscellaneous_page,
            IDC_MISCELLANEOUS_ASCENDING,
            IDC_MISCELLANEOUS_DESCENDING,
            if options.sort_descending {
                IDC_MISCELLANEOUS_DESCENDING
            } else {
                IDC_MISCELLANEOUS_ASCENDING
            },
        )
    };
    set_check(
        miscellaneous_page,
        IDC_MISCELLANEOUS_LOOP_WITHIN_FOLDER,
        options.loop_within_folder,
    );
    combo_select(
        miscellaneous_page,
        IDC_MISCELLANEOUS_SLIDESHOW_DIRECTION,
        options.slideshow_direction,
    );
    combo_select(
        miscellaneous_page,
        IDC_MISCELLANEOUS_AFTER_DELETION,
        options.after_deletion,
    );
    set_check(
        miscellaneous_page,
        IDC_MISCELLANEOUS_ASK_DELETE,
        options.ask_delete,
    );
    set_check(
        miscellaneous_page,
        IDC_MISCELLANEOUS_DETECT_FORMAT_BY_CONTENT,
        options.detect_format_by_content,
    );
    set_check(
        miscellaneous_page,
        IDC_MISCELLANEOUS_REMEMBER_RECENTS,
        options.remember_recents,
    );
    set_check(
        miscellaneous_page,
        IDC_MISCELLANEOUS_SKIP_HIDDEN,
        options.skip_hidden,
    );
}

fn sync_start_menu_page(state: &OptionsState) {
    set_check(
        state.pages[START_MENU_PAGE],
        IDC_START_MENU_SHORTCUT,
        state.start_menu_desired,
    );
}

fn sync_background_color_button(state: &OptionsState, page: HWND) {
    if let Ok(button) = unsafe { GetDlgItem(Some(page), IDC_WINDOW_BACKGROUND_COLOR_BUTTON) } {
        let _ = unsafe { EnableWindow(button, state.transient_options.background_color_enabled) };
        // The swatch fills its whole rectangle, so an erase would only flash under it.
        let _ = unsafe { windows::Win32::Graphics::Gdi::InvalidateRect(Some(button), None, false) };
    }
}

/// Packs an (R, G, B) triple into a Win32 COLORREF (0x00BBGGRR).
fn rgb_to_colorref((red, green, blue): (u8, u8, u8)) -> COLORREF {
    COLORREF(u32::from(red) | (u32::from(green) << 8) | (u32::from(blue) << 16))
}

fn colorref_to_rgb(color: COLORREF) -> (u8, u8, u8) {
    (
        (color.0 & 0xFF) as u8,
        ((color.0 >> 8) & 0xFF) as u8,
        ((color.0 >> 16) & 0xFF) as u8,
    )
}

/// The stored value is already clamped; the field catches up when focus leaves it.
fn show_clamped_number(page: HWND, control: i32) -> isize {
    let Some(value) = state_mut(page).and_then(|state| match control {
        IDC_IMAGE_ZOOM_STEP_EDIT => Some(state.transient_options.zoom_step_percent),
        IDC_MISCELLANEOUS_SLIDESHOW_INTERVAL_EDIT => {
            Some(state.transient_options.slideshow_interval_seconds)
        }
        _ => None,
    }) else {
        return 0;
    };
    if let Some(state) = state_mut(page) {
        state.syncing = true;
    }
    let text = HSTRING::from(value.to_string());
    let _ = unsafe { SetDlgItemTextW(page, control, &text) };
    if let Some(state) = state_mut(page) {
        state.syncing = false;
    }
    1
}

/// Borrows the state in stages: the color dialog's modal loop re-enters the procedures.
fn choose_background_color(page: HWND) {
    let Some((owner, initial, mut custom_colors)) = state_mut(page).map(|state| {
        (
            state.dialog,
            state.transient_options.background_color,
            state.custom_colors,
        )
    }) else {
        return;
    };
    // The dialog writes the custom colors through this local, not through the state.
    let mut configuration = CHOOSECOLORW {
        lStructSize: size_of::<CHOOSECOLORW>() as u32,
        hwndOwner: owner,
        rgbResult: rgb_to_colorref(initial),
        lpCustColors: custom_colors.as_mut_ptr(),
        Flags: CC_RGBINIT | CC_FULLOPEN,
        ..Default::default()
    };
    let confirmed = unsafe { ChooseColorW(&raw mut configuration) }.as_bool();
    let Some(state) = state_mut(page) else {
        return;
    };
    state.custom_colors = custom_colors;
    if confirmed {
        state.transient_options.background_color = colorref_to_rgb(configuration.rgbResult);
        sync_background_color_button(state, page);
    }
    update_buttons(state);
}

fn initialize_shortcuts_page(state: &OptionsState) {
    let page = state.pages[SHORTCUTS_PAGE];
    let Ok(list) = (unsafe { GetDlgItem(Some(page), IDC_SHORTCUTS_LIST) }) else {
        return;
    };
    let list_styles = LVS_EX_FULLROWSELECT | LVS_EX_DOUBLEBUFFER;
    unsafe {
        SendMessageW(
            list,
            LVM_SETEXTENDEDLISTVIEWSTYLE,
            Some(WPARAM(list_styles as usize)),
            Some(LPARAM(list_styles as isize)),
        )
    };
    let mut bounds = RECT::default();
    if unsafe { GetClientRect(list, &raw mut bounds) }.is_err() {
        return;
    }
    let usable = bounds.right - bounds.left - unsafe { GetSystemMetrics(SM_CXVSCROLL) };
    let action_width = usable * 36 / 100;
    let keyboard_width = usable * 32 / 100;
    let mouse_width = usable - action_width - keyboard_width;
    for (index, title, width) in [
        (ACTION_COLUMN, "Action", action_width),
        (KEYBOARD_COLUMN, "Keyboard", keyboard_width),
        (MOUSE_COLUMN, "Mouse", mouse_width),
    ] {
        let text = HSTRING::from(title);
        let column = LVCOLUMNW {
            mask: LVCF_TEXT | LVCF_WIDTH,
            cx: width,
            pszText: windows::core::PWSTR(text.as_ptr().cast_mut()),
            ..Default::default()
        };
        unsafe {
            SendMessageW(
                list,
                LVM_INSERTCOLUMNW,
                Some(WPARAM(index as usize)),
                Some(LPARAM(&raw const column as isize)),
            )
        };
    }
    for (index, row) in state.transient_shortcuts.iter().enumerate() {
        let label = HSTRING::from(row.action.label());
        let item = LVITEMW {
            mask: LVIF_TEXT,
            iItem: index as i32,
            pszText: windows::core::PWSTR(label.as_ptr().cast_mut()),
            ..Default::default()
        };
        unsafe {
            SendMessageW(
                list,
                LVM_INSERTITEMW,
                None,
                Some(LPARAM(&raw const item as isize)),
            )
        };
    }
}

fn refresh_shortcut_rows(state: &OptionsState) {
    let Ok(list) = (unsafe { GetDlgItem(Some(state.pages[SHORTCUTS_PAGE]), IDC_SHORTCUTS_LIST) })
    else {
        return;
    };
    for (index, row) in state.transient_shortcuts.iter().enumerate() {
        for (subitem, text) in [
            (KEYBOARD_COLUMN, row.keyboard.join(", ")),
            (MOUSE_COLUMN, row.mouse.join(", ")),
        ] {
            let wide_text = HSTRING::from(&text);
            let item = LVITEMW {
                mask: LVIF_TEXT,
                iSubItem: subitem,
                pszText: windows::core::PWSTR(wide_text.as_ptr().cast_mut()),
                ..Default::default()
            };
            unsafe {
                SendMessageW(
                    list,
                    LVM_SETITEMTEXTW,
                    Some(WPARAM(index)),
                    Some(LPARAM(&raw const item as isize)),
                )
            };
        }
    }
}

/// Borrows the state in stages: the capture dialog's modal loop re-enters the procedures.
fn edit_shortcut(page: HWND, row_index: usize, mouse_column: bool) {
    let Some((dialog, current, taken)) = state_mut(page).map(|state| {
        let taken: Vec<(String, &'static str)> = state
            .transient_shortcuts
            .iter()
            .enumerate()
            .filter(|(index, _)| *index != row_index)
            .flat_map(|(_, row)| {
                let encodings = if mouse_column {
                    &row.mouse
                } else {
                    &row.keyboard
                };
                encodings
                    .iter()
                    .map(|encoding| (encoding.clone(), row.action.label()))
            })
            .collect();
        let row = &state.transient_shortcuts[row_index];
        let current = if mouse_column {
            row.mouse.clone()
        } else {
            row.keyboard.clone()
        };
        (state.dialog, current, taken)
    }) else {
        return;
    };
    let taken: Vec<(&str, &str)> = taken
        .iter()
        .map(|(encoding, owner)| (encoding.as_str(), *owner))
        .collect();
    let updated = if mouse_column {
        shortcut_capture::capture_mouse_binding(dialog, current.first().map(String::as_str), &taken)
    } else {
        shortcut_capture::capture_keyboard_sequences(dialog, &current, &taken)
    };
    let Some(encodings) = updated else {
        return;
    };
    let Some(state) = state_mut(page) else {
        return;
    };
    let row = &mut state.transient_shortcuts[row_index];
    if mouse_column {
        row.mouse = encodings;
    } else {
        row.keyboard = encodings;
    }
    refresh_shortcut_rows(state);
    update_buttons(state);
}

fn initialize_association_page(state: &mut OptionsState) {
    let page = state.pages[ASSOCIATION_PAGE];
    let Ok(tree) = (unsafe { GetDlgItem(Some(page), IDC_ASSOCIATION_TREE) }) else {
        return;
    };
    // Double buffering keeps an item from erasing before it repaints.
    unsafe {
        SendMessageW(
            tree,
            TVM_SETEXTENDEDSTYLE,
            Some(WPARAM(TVS_EX_DOUBLEBUFFER as usize)),
            Some(LPARAM(TVS_EX_DOUBLEBUFFER as isize)),
        )
    };
    state.state_images = create_tristate_images();
    unsafe {
        SendMessageW(
            tree,
            TVM_SETIMAGELIST,
            Some(WPARAM(TVSIL_STATE as usize)),
            Some(LPARAM(state.state_images.0)),
        )
    };

    for (name, extension_list) in image::formats::sorted_format_groups() {
        if extension_list.len() == 1 {
            let extension = format!(".{}", extension_list[0]);
            insert_extension(
                state,
                tree,
                TVI_ROOT,
                &format!("{name} ({extension})"),
                &extension,
            );
        } else {
            let group_index = state.groups.len();
            let header = tree_insert(
                tree,
                TVI_ROOT,
                name,
                GROUP_FLAG | group_index as isize,
                STATE_UNCHECKED,
            );
            let mut members = Vec::new();
            for extension_name in extension_list {
                let extension = format!(".{extension_name}");
                members.push(insert_extension(
                    state, tree, header, &extension, &extension,
                ));
            }
            state.groups.push(AssociationGroup {
                item: header,
                members,
            });
        }
    }
    for group_index in 0..state.groups.len() {
        refresh_group_check_image(state, tree, group_index);
    }
}

fn insert_extension(
    state: &mut OptionsState,
    tree: HWND,
    parent: HTREEITEM,
    label: &str,
    extension: &str,
) -> usize {
    let checked = state
        .saved_associations
        .iter()
        .any(|saved| saved == extension);
    let index = state.extensions.len();
    let item = tree_insert(tree, parent, label, index as isize, check_state(checked));
    state.extensions.push(AssociationExtension {
        extension: extension.to_string(),
        checked,
        item,
    });
    index
}

fn create_tristate_images() -> HIMAGELIST {
    let images = unsafe {
        ImageList_Create(
            STATE_IMAGE_EDGE_PIXELS,
            STATE_IMAGE_EDGE_PIXELS,
            ILC_COLOR32 | ILC_MASK,
            4,
            0,
        )
    };
    // Without the list the tree simply shows no state images; the destroy path checks the same.
    if images.is_invalid() {
        return images;
    }
    let screen = unsafe { GetDC(None) };
    for style in [
        DFCS_BUTTONCHECK, // index 0 placeholder (state image 0 = none)
        DFCS_BUTTONCHECK,
        DFCS_BUTTONCHECK | DFCS_CHECKED,
        DFCS_BUTTON3STATE | DFCS_CHECKED,
    ] {
        unsafe {
            let memory = CreateCompatibleDC(Some(screen));
            let bitmap =
                CreateCompatibleBitmap(screen, STATE_IMAGE_EDGE_PIXELS, STATE_IMAGE_EDGE_PIXELS);
            let previous = SelectObject(memory, bitmap.into());
            let mut bounds = RECT {
                left: 1,
                top: 1,
                right: STATE_IMAGE_EDGE_PIXELS - 1,
                bottom: STATE_IMAGE_EDGE_PIXELS - 1,
            };
            FillRect(
                memory,
                &RECT {
                    left: 0,
                    top: 0,
                    right: STATE_IMAGE_EDGE_PIXELS,
                    bottom: STATE_IMAGE_EDGE_PIXELS,
                },
                GetSysColorBrush(windows::Win32::Graphics::Gdi::COLOR_WINDOW),
            );
            let _ = DrawFrameControl(memory, &raw mut bounds, DFC_BUTTON, style);
            SelectObject(memory, previous);
            ImageList_Add(images, bitmap, None);
            let _ = DeleteObject(bitmap.into());
            let _ = DeleteDC(memory);
        }
    }
    unsafe { ReleaseDC(None, screen) };
    images
}

fn tree_insert(
    tree: HWND,
    parent: HTREEITEM,
    text: &str,
    item_data: isize,
    state_image: isize,
) -> HTREEITEM {
    let label = HSTRING::from(text);
    let insert = TVINSERTSTRUCTW {
        hParent: parent,
        hInsertAfter: TVI_LAST,
        Anonymous: windows::Win32::UI::Controls::TVINSERTSTRUCTW_0 {
            itemex: TVITEMEXW {
                mask: TVIF_TEXT | TVIF_PARAM | TVIF_STATE,
                pszText: windows::core::PWSTR(label.as_ptr().cast_mut()),
                lParam: LPARAM(item_data),
                state: (state_image as u32) << STATE_IMAGE_SHIFT,
                stateMask: TVIS_STATEIMAGEMASK.0,
                ..Default::default()
            },
        },
    };
    let item = unsafe {
        SendMessageW(
            tree,
            TVM_INSERTITEMW,
            None,
            Some(LPARAM(&raw const insert as isize)),
        )
    };
    HTREEITEM(item.0)
}

fn tree_set_state_image(tree: HWND, item: HTREEITEM, state_image: isize) {
    let update = TVITEMEXW {
        mask: TVIF_STATE,
        hItem: item,
        state: (state_image as u32) << STATE_IMAGE_SHIFT,
        stateMask: TVIS_STATEIMAGEMASK.0,
        ..Default::default()
    };
    unsafe {
        SendMessageW(
            tree,
            TVM_SETITEMW,
            None,
            Some(LPARAM(&raw const update as isize)),
        )
    };
}

fn toggle_association_at_cursor(state: &mut OptionsState, tree: HWND) {
    let (x, y) = point_from_packed(unsafe { GetMessagePos() } as usize);
    let mut hit = TVHITTESTINFO {
        pt: POINT { x, y },
        ..Default::default()
    };
    let mut corner = [hit.pt];
    unsafe { windows::Win32::Graphics::Gdi::MapWindowPoints(None, Some(tree), &mut corner) };
    hit.pt = corner[0];
    let item =
        unsafe { SendMessageW(tree, TVM_HITTEST, None, Some(LPARAM(&raw mut hit as isize))) };
    if item.0 != 0 && hit.flags & TVHT_ONITEMSTATEICON != Default::default() {
        toggle_association_item(state, tree, HTREEITEM(item.0));
    }
}

fn toggle_association_item(state: &mut OptionsState, tree: HWND, item: HTREEITEM) {
    let mut query = TVITEMEXW {
        mask: TVIF_PARAM,
        hItem: item,
        ..Default::default()
    };
    unsafe {
        SendMessageW(
            tree,
            TVM_GETITEMW,
            None,
            Some(LPARAM(&raw mut query as isize)),
        )
    };
    let item_data = query.lParam.0;
    if item_data & GROUP_FLAG != 0 {
        let group_index = (item_data & !GROUP_FLAG) as usize;
        let group = &state.groups[group_index];
        let extensions = &mut state.extensions;
        let all_checked = group
            .members
            .iter()
            .all(|member| extensions[*member].checked);
        for member in &group.members {
            let entry = &mut extensions[*member];
            entry.checked = !all_checked;
            tree_set_state_image(tree, entry.item, check_state(entry.checked));
        }
        refresh_group_check_image(state, tree, group_index);
    } else {
        let extension_index = item_data as usize;
        let entry = &mut state.extensions[extension_index];
        entry.checked = !entry.checked;
        tree_set_state_image(tree, entry.item, check_state(entry.checked));
        if let Some(group_index) = state
            .groups
            .iter()
            .position(|group| group.members.contains(&extension_index))
        {
            refresh_group_check_image(state, tree, group_index);
        }
    }
    update_buttons(state);
}

fn refresh_group_check_image(state: &OptionsState, tree: HWND, group_index: usize) {
    let group = &state.groups[group_index];
    let checked_count = group
        .members
        .iter()
        .filter(|member| state.extensions[**member].checked)
        .count();
    let image = if checked_count == 0 {
        STATE_UNCHECKED
    } else if checked_count == group.members.len() {
        STATE_CHECKED
    } else {
        STATE_PARTIAL
    };
    tree_set_state_image(tree, group.item, image);
}

fn set_all_associations(state: &mut OptionsState, checked: bool) {
    let Ok(tree) =
        (unsafe { GetDlgItem(Some(state.pages[ASSOCIATION_PAGE]), IDC_ASSOCIATION_TREE) })
    else {
        return;
    };
    for entry in &mut state.extensions {
        entry.checked = checked;
        tree_set_state_image(tree, entry.item, check_state(checked));
    }
    for group_index in 0..state.groups.len() {
        refresh_group_check_image(state, tree, group_index);
    }
}

fn combo_fill(page: HWND, control: i32, entries: &[&str]) {
    let Ok(combo) = (unsafe { GetDlgItem(Some(page), control) }) else {
        return;
    };
    for &entry in entries {
        let text = HSTRING::from(entry);
        unsafe {
            SendMessageW(
                combo,
                CB_ADDSTRING,
                None,
                Some(LPARAM(text.as_ptr() as isize)),
            )
        };
    }
}

fn combo_select(page: HWND, control: i32, index: u32) {
    if let Ok(combo) = unsafe { GetDlgItem(Some(page), control) } {
        unsafe { SendMessageW(combo, CB_SETCURSEL, Some(WPARAM(index as usize)), None) };
    }
}

/// Only CBN_SELCHANGE calls this, so the combo exists and its selection is never CB_ERR.
fn combo_selection(page: HWND, control: i32) -> u32 {
    unsafe { SendDlgItemMessageW(page, control, CB_GETCURSEL, WPARAM(0), LPARAM(0)) }.0 as u32
}

fn set_check(page: HWND, control: i32, checked: bool) {
    let _ = unsafe {
        CheckDlgButton(
            page,
            control,
            if checked { BST_CHECKED } else { BST_UNCHECKED },
        )
    };
}

fn is_checked(page: HWND, control: i32) -> bool {
    unsafe { IsDlgButtonChecked(page, control) == BST_CHECKED.0 }
}
