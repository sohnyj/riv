//! Load state machine, item listing, preload cache, and the decode worker pool.

use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::c_void;
use std::hash::{Hash, Hasher};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_HIDDEN;
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};
use windows::Win32::UI::Shell::StrCmpLogicalW;
use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_APP};
use windows::core::PCWSTR;

use super::decode::{self, DecodeError, DecodedImage};
use crate::archive::reader as archive_reader;
use crate::network::curl;

pub const WM_APP_DECODE_COMPLETE: u32 = WM_APP + 1;
pub const WM_APP_DOWNLOAD_PROGRESS: u32 = WM_APP + 7;
pub const WM_APP_LISTING_READY: u32 = WM_APP + 8;

/// UI updates at most this often while a remote image downloads.
const DOWNLOAD_PROGRESS_INTERVAL: Duration = Duration::from_millis(100);

/// Viewable item identity; paths compare case-insensitively, member names and URLs exactly.
#[derive(Clone)]
pub enum ItemLocation {
    File(PathBuf),
    ArchiveMember { archive: PathBuf, member: String },
    Url(String),
}

impl PartialEq for ItemLocation {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::File(first), Self::File(second)) => paths_equal(first, second),
            (
                Self::ArchiveMember {
                    archive: first_archive,
                    member: first_member,
                },
                Self::ArchiveMember {
                    archive: second_archive,
                    member: second_member,
                },
            ) => paths_equal(first_archive, second_archive) && first_member == second_member,
            (Self::Url(first), Self::Url(second)) => first == second,
            _ => false,
        }
    }
}

impl Eq for ItemLocation {}

impl Hash for ItemLocation {
    fn hash<H: Hasher>(&self, state: &mut H) {
        match self {
            Self::File(path) => {
                0u8.hash(state);
                path_identity(path).hash(state);
            }
            Self::ArchiveMember { archive, member } => {
                1u8.hash(state);
                path_identity(archive).hash(state);
                member.hash(state);
            }
            Self::Url(url) => {
                2u8.hash(state);
                url.hash(state);
            }
        }
    }
}

impl ItemLocation {
    /// Leaf name for titles and messages (member basename inside archives).
    pub fn display_name(&self) -> String {
        match self {
            Self::File(path) => path
                .file_name()
                .map_or_else(String::new, |name| name.to_string_lossy().into_owned()),
            Self::ArchiveMember { member, .. } => member
                .rsplit(['/', '\\'])
                .next()
                .unwrap_or(member)
                .to_string(),
            Self::Url(url) => curl::file_name(url).to_string(),
        }
    }

    /// Full user-facing location text ("archive \u{203a} member" for members).
    pub fn display_text(&self) -> String {
        match self {
            Self::File(path) => path.display().to_string(),
            Self::ArchiveMember { archive, member } => {
                format!("{} \u{203a} {member}", archive.display())
            }
            Self::Url(url) => url.clone(),
        }
    }

    /// Parent folder leaf for "folder\file" titles (a member's folder inside its archive).
    pub fn folder_name(&self) -> Option<String> {
        match self {
            Self::File(path) => path
                .parent()
                .and_then(Path::file_name)
                .map(|name| name.to_string_lossy().into_owned()),
            Self::ArchiveMember { archive, member } => {
                // The member's immediate parent within the archive, else the archive file.
                let segments: Vec<&str> = member
                    .split(['/', '\\'])
                    .filter(|part| !part.is_empty())
                    .collect();
                match segments.len() {
                    count if count >= 2 => Some(segments[count - 2].to_string()),
                    _ => archive
                        .file_name()
                        .map(|name| name.to_string_lossy().into_owned()),
                }
            }
            Self::Url(_) => None,
        }
    }

    /// The file that carries this item on disk (the archive for members).
    pub fn containing_file(&self) -> Option<&Path> {
        match self {
            Self::File(path) => Some(path),
            Self::ArchiveMember { archive, .. } => Some(archive),
            Self::Url(_) => None,
        }
    }

    /// Some only for plain files; members cannot take file operations.
    pub fn as_file(&self) -> Option<&Path> {
        match self {
            Self::File(path) => Some(path),
            Self::ArchiveMember { .. } | Self::Url(_) => None,
        }
    }

    fn extension_lowercase(&self) -> Option<String> {
        let name_path = match self {
            Self::File(path) => path.as_path(),
            Self::ArchiveMember { member, .. } => Path::new(member),
            Self::Url(url) => return curl::extension_lowercase(url),
        };
        name_path
            .extension()
            .map(|extension| extension.to_string_lossy().to_lowercase())
    }
}

/// What the entry listing was scanned from.
#[derive(Clone)]
enum ListingScope {
    Directory(PathBuf),
    Archive(PathBuf),
}

impl ListingScope {
    /// Same kind over the same path, compared like item identity (case-insensitive).
    fn covers(&self, other: &ListingScope) -> bool {
        match (self, other) {
            (Self::Directory(first), Self::Directory(second))
            | (Self::Archive(first), Self::Archive(second)) => paths_equal(first, second),
            _ => false,
        }
    }

    fn path(&self) -> &Path {
        match self {
            Self::Directory(path) | Self::Archive(path) => path,
        }
    }
}

/// Preload mode 0/1/2 -> (backward distance, forward distance, cache budget in bytes).
const PRELOAD_SPECIFICATIONS: [(usize, usize, u64); 3] = [
    (0, 0, 0),
    (1, 3, 1024 * 1024 * 1024),
    (2, 6, 2 * 1024 * 1024 * 1024),
];

#[derive(Clone, PartialEq)]
pub struct CoreOptions {
    pub sort_mode: SortMode,
    pub sort_descending: bool,
    pub preloading_mode: usize,
    pub loop_within_folder: bool,
    pub skip_hidden: bool,
    pub detect_format_by_content: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SortMode {
    Name,
    Modified,
    Created,
    Size,
    Type,
}

impl SortMode {
    pub fn from_setting(value: u32) -> Self {
        match value {
            1 => Self::Modified,
            2 => Self::Created,
            3 => Self::Size,
            4 => Self::Type,
            _ => Self::Name,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum NavigationCommand {
    First,
    Previous,
    Next,
    Last,
}

pub struct ListingEntry {
    pub location: ItemLocation,
    wide_name: Vec<u16>,
    file_size: u64,
    modified: SystemTime,
    created: SystemTime,
}

pub enum DecodeStage {
    /// Preview standing in while the same job goes on to the full decode.
    Preview,
    /// Preview and the job stops there; the full decode is owed on arrival.
    PreviewFinal,
    Final,
}

pub struct DecodeCompletion {
    pub location: ItemLocation,
    pub file_size: u64,
    /// Echoed from the submit stat or the listing; None for URLs.
    pub modified: Option<SystemTime>,
    pub stage: DecodeStage,
    pub result: Result<Arc<DecodedImage>, DecodeError>,
}

/// Bytes received so far for a downloading URL item; 0 means connecting.
pub struct DownloadProgress {
    pub location: ItemLocation,
    pub received_bytes: u64,
}

pub struct CurrentImage {
    pub location: ItemLocation,
    pub image: Arc<DecodedImage>,
    file_size: u64,
    modified: Option<SystemTime>,
}

struct CacheEntry {
    file_size: u64,
    modified: Option<SystemTime>,
    /// RAW preview or animation first frame standing in until someone pays for the full decode.
    preview: bool,
    image: Arc<DecodedImage>,
}

/// Frees retired image buffers off the UI thread.
struct ImageReaper {
    sender: mpsc::Sender<Arc<DecodedImage>>,
}

impl ImageReaper {
    fn new() -> Self {
        let (sender, receiver) = mpsc::channel::<Arc<DecodedImage>>();
        // A spawn failure drops the receiver, so every reap frees inline instead.
        let _ = std::thread::Builder::new()
            .name("riv-image-reaper".to_string())
            .spawn(move || receiver.iter().for_each(drop));
        Self { sender }
    }

    fn reap(&self, image: Arc<DecodedImage>) {
        let _ = self.sender.send(image);
    }
}

pub struct PlaylistWindow {
    pub names: Vec<String>,
    pub first_index: usize,
    pub current_slot: Option<usize>,
    pub hidden_count: usize,
}

pub struct ImageCore {
    pool: DecodePool,
    options: CoreOptions,
    listing_scope: Option<ListingScope>,
    entries: Vec<ListingEntry>,
    /// Item awaiting display; replacing it invalidates the previous load.
    pending_display: Option<ItemLocation>,
    in_flight: HashMap<ItemLocation, Arc<AtomicBool>>,
    cache: HashMap<ItemLocation, CacheEntry>,
    pub current: Option<CurrentImage>,
    pub load_error: Option<(ItemLocation, DecodeError)>,
    /// Preload polarity: the deeper reach aims along the travel direction.
    travel_backward: bool,
    /// Consecutive steps against the polarity; the second one flips it.
    opposite_steps: u32,
    reaper: ImageReaper,
    /// Listing scan in flight (folder or archive), awaiting its ScannedListing.
    pending_scan: Option<PendingScan>,
    window: isize,
}

/// A scan request: the scope it awaits, and whether arrival opens the first entry.
struct PendingScan {
    scope: ListingScope,
    open_first_entry: bool,
}

/// A finished background scan, posted back as WM_APP_LISTING_READY.
pub struct ScannedListing {
    scope: ListingScope,
    sort_mode: SortMode,
    sort_descending: bool,
    /// Directories always land Ok; archives can fail to enumerate or hold no images.
    result: Result<Vec<ListingEntry>, DecodeError>,
}

impl ScannedListing {
    /// Enumerates and sorts the scope's contents: the worker half of submit_scan.
    fn of(scope: ListingScope, options: &CoreOptions) -> Self {
        let result = match &scope {
            ListingScope::Directory(directory) => {
                let mut entries = scan_folder(directory, options);
                sort_entries(&mut entries, options);
                Ok(entries)
            }
            ListingScope::Archive(archive) => enumerate_archive(archive, options),
        };
        Self {
            scope,
            sort_mode: options.sort_mode,
            sort_descending: options.sort_descending,
            result,
        }
    }
}

/// Image members of an archive, sorted; an empty archive is an error, not a listing.
fn enumerate_archive(
    archive: &Path,
    options: &CoreOptions,
) -> Result<Vec<ListingEntry>, DecodeError> {
    let members = archive_reader::enumerate(archive).map_err(DecodeError::from)?;
    let mut entries: Vec<ListingEntry> = members
        .into_iter()
        .filter_map(|member| member_entry(archive, member))
        .collect();
    if entries.is_empty() {
        return Err(decode::uncoded_error(
            "archive contains no supported images",
        ));
    }
    sort_entries(&mut entries, options);
    Ok(entries)
}

/// What an arrived scan did: nothing (stale), the listing alone, or an open that
/// displayed or errored.
pub enum ListingInstall {
    Discarded,
    Installed,
    Opened { displayed: bool },
}

impl ImageCore {
    pub fn new(window: HWND, options: CoreOptions) -> Self {
        Self {
            pool: DecodePool::new(window.0 as isize),
            options,
            listing_scope: None,
            entries: Vec::new(),
            pending_display: None,
            in_flight: HashMap::new(),
            cache: HashMap::new(),
            current: None,
            load_error: None,
            travel_backward: false,
            opposite_steps: 0,
            reaper: ImageReaper::new(),
            pending_scan: None,
            window: window.0 as isize,
        }
    }

    /// Aims the preload polarity at a declared direction (slideshow start).
    pub fn set_travel_direction(&mut self, backward: bool) {
        self.travel_backward = backward;
        self.opposite_steps = 0;
        self.preload_neighbors();
    }

    /// A fresh listing starts with the forward default.
    fn reset_travel_direction(&mut self) {
        self.travel_backward = false;
        self.opposite_steps = 0;
    }

    /// Jumps declare their direction; steps flip the polarity on the second one in a row.
    fn note_navigation(&mut self, command: NavigationCommand) {
        match command {
            NavigationCommand::First => {
                self.travel_backward = false;
                self.opposite_steps = 0;
            }
            NavigationCommand::Last => {
                self.travel_backward = true;
                self.opposite_steps = 0;
            }
            NavigationCommand::Next => self.note_step(false),
            NavigationCommand::Previous => self.note_step(true),
        }
    }

    fn note_step(&mut self, backward: bool) {
        if backward == self.travel_backward {
            self.opposite_steps = 0;
            return;
        }
        self.opposite_steps += 1;
        if self.opposite_steps >= 2 {
            self.travel_backward = backward;
            self.opposite_steps = 0;
        }
    }

    /// Preload distances and budget, aimed along the current travel direction.
    fn preload_plan(&self) -> (usize, usize, u64) {
        let (backward, forward, budget) =
            PRELOAD_SPECIFICATIONS[self.options.preloading_mode.min(2)];
        if self.travel_backward {
            (forward, backward, budget)
        } else {
            (backward, forward, budget)
        }
    }

    pub fn update_options(&mut self, options: CoreOptions) {
        if options == self.options {
            return;
        }
        let list_affected = options.sort_mode != self.options.sort_mode
            || options.sort_descending != self.options.sort_descending
            || options.skip_hidden != self.options.skip_hidden
            || options.detect_format_by_content != self.options.detect_format_by_content;
        self.options = options;
        if list_affected {
            self.rescan_listing();
        }
        self.preload_neighbors();
    }

    pub fn listing_position(&self) -> Option<(usize, usize)> {
        let anchor = self.navigation_anchor()?;
        let index = self.position_of(anchor)?;
        Some((index + 1, self.entries.len()))
    }

    /// A NavigationTargets-gated action can act: somewhere to go besides the anchor itself.
    pub fn has_navigation_targets(&self) -> bool {
        match self.entries.len() {
            0 => false,
            // A single listed entry is a real target only for an unlisted anchor.
            1 => self
                .navigation_anchor()
                .and_then(|anchor| self.position_of(anchor))
                .is_none(),
            _ => true,
        }
    }

    /// Listing window for the menu: `capacity` names centered on the anchor, the rest a count.
    pub fn playlist_window(&self, capacity: usize) -> PlaylistWindow {
        let anchor_index = self
            .navigation_anchor()
            .and_then(|location| self.position_of(location));
        let total = self.entries.len();
        let first_index = playlist_window_start(total, anchor_index, capacity);
        let end = (first_index + capacity).min(total);
        PlaylistWindow {
            names: self.entries[first_index..end]
                .iter()
                .map(|entry| entry.location.display_name())
                .collect(),
            first_index,
            current_slot: anchor_index
                .filter(|index| (first_index..end).contains(index))
                .map(|index| index - first_index),
            hidden_count: total - (end - first_index),
        }
    }

    /// (file size, modified) of the current item, carried from its load; None for URL times.
    pub fn current_item_metadata(&self) -> Option<(u64, Option<SystemTime>)> {
        let current = self.current.as_ref()?;
        Some((current.file_size, current.modified))
    }

    /// True while this item is the one the view waits on.
    pub fn is_pending(&self, location: &ItemLocation) -> bool {
        self.pending_display.as_ref() == Some(location)
    }

    pub fn reload_current(&mut self) -> bool {
        // Reload retries the position baseline, so an errored item reloads itself.
        let Some(location) = self.navigation_anchor().cloned() else {
            return false;
        };
        if let Some(entry) = self.cache.remove(&location) {
            self.reaper.reap(entry.image);
        }
        if let ItemLocation::Url(url) = &location {
            // Back through load_url so validation errors reproduce on retry.
            return self.load_url(url);
        }
        if let ItemLocation::File(path) = &location
            && location
                .extension_lowercase()
                .as_deref()
                .is_some_and(archive_reader::is_archive_extension)
        {
            // An archive anchor retries through its scan, like a URL revalidates.
            let scope = ListingScope::Archive(path.clone());
            self.submit_scan(PendingScan {
                scope,
                open_first_entry: true,
            });
            return false;
        }
        // A reload re-decodes the item; the listing snapshot stays put.
        self.load_item(&location)
    }

    pub fn load_path(&mut self, path: &Path) -> bool {
        let Ok(path) = std::path::absolute(path) else {
            return false;
        };
        if path.is_dir() {
            self.submit_scan(PendingScan {
                scope: ListingScope::Directory(path),
                open_first_entry: true,
            });
            return false;
        }
        let extension = path
            .extension()
            .map(|extension| extension.to_string_lossy().to_lowercase());
        if extension
            .as_deref()
            .is_some_and(archive_reader::is_archive_extension)
        {
            self.submit_scan(PendingScan {
                scope: ListingScope::Archive(path),
                open_first_entry: true,
            });
            return false;
        }
        let directory = path.parent().map(Path::to_path_buf);
        if let Some(directory) = directory
            && !self.directory_covered(&directory)
        {
            self.submit_scan(PendingScan {
                scope: ListingScope::Directory(directory),
                open_first_entry: false,
            });
        }
        self.load_item(&ItemLocation::File(path))
    }

    /// True when the listing or the scan in flight already covers this directory.
    fn directory_covered(&self, directory: &Path) -> bool {
        let covers = |scope: &ListingScope| matches!(scope, ListingScope::Directory(scanned) if paths_equal(scanned, directory));
        matches!(&self.listing_scope, Some(scope) if covers(scope))
            || matches!(&self.pending_scan, Some(pending) if covers(&pending.scope))
    }

    /// Clears the listing and enumerates the scope off the UI thread; it installs on arrival.
    fn submit_scan(&mut self, pending: PendingScan) {
        let scope = pending.scope.clone();
        self.reset_travel_direction();
        self.entries = Vec::new();
        self.listing_scope = None;
        self.pending_scan = Some(pending);
        let options = self.options.clone();
        let window = self.window;
        let _ = std::thread::Builder::new()
            .name("riv-listing-scan".to_string())
            .spawn(move || {
                post_boxed(
                    window,
                    WM_APP_LISTING_READY,
                    Box::new(ScannedListing::of(scope, &options)),
                );
            });
    }

    /// Installs an arrived scan; a stale one is dropped, an archive failure becomes the error.
    pub fn install_listing_scan(&mut self, scan: ScannedListing) -> ListingInstall {
        let Some(pending) = self
            .pending_scan
            .take_if(|pending| pending.scope.covers(&scan.scope))
        else {
            return ListingInstall::Discarded;
        };
        let mut entries = match scan.result {
            Ok(entries) => entries,
            Err(error) => {
                self.pending_display = None;
                self.load_error =
                    Some((ItemLocation::File(scan.scope.path().to_path_buf()), error));
                return ListingInstall::Opened { displayed: false };
            }
        };
        // The worker sorted under an options snapshot; a change since then re-sorts here.
        if scan.sort_mode != self.options.sort_mode
            || scan.sort_descending != self.options.sort_descending
        {
            sort_entries(&mut entries, &self.options);
        }
        self.entries = entries;
        self.listing_scope = Some(scan.scope);
        if pending.open_first_entry {
            let Some(first) = self.entries.first().map(|entry| entry.location.clone()) else {
                return ListingInstall::Installed;
            };
            return ListingInstall::Opened {
                displayed: self.load_item(&first),
            };
        }
        self.preload_neighbors();
        ListingInstall::Installed
    }

    /// True while a listing scan is in flight; the window is loading, not empty.
    pub fn listing_scan_pending(&self) -> bool {
        self.pending_scan.is_some()
    }

    /// Opens a remote image as a standalone item (no listing, no navigation).
    pub fn load_url(&mut self, url: &str) -> bool {
        // Even a failed attempt leaves the single-item state; no listing survives.
        self.entries = Vec::new();
        self.listing_scope = None;
        self.pending_scan = None;
        let failure = if url.is_empty() {
            Some("no URL in the clipboard") // only the paste path can deliver an empty URL
        } else if !curl::is_supported_protocol(url) {
            Some("unsupported URL protocol")
        } else if !curl::available() {
            Some("URL support is unavailable on this Windows")
        } else if curl::extension_lowercase(url)
            .is_some_and(|extension| archive_reader::is_archive_extension(&extension))
        {
            Some("archives are not supported from a URL")
        } else {
            None
        };
        let location = ItemLocation::Url(url.to_string());
        if let Some(message) = failure {
            self.pending_display = None;
            self.load_error = Some((
                location,
                DecodeError {
                    code: 0,
                    message: message.to_string(),
                    store_extension: None,
                },
            ));
            return false;
        }
        self.load_item(&location)
    }

    fn load_item(&mut self, location: &ItemLocation) -> bool {
        let (file_size, modified) = match location {
            ItemLocation::File(path) => match std::fs::metadata(path) {
                Ok(metadata) => (metadata.len(), metadata.modified().ok()),
                Err(error) => {
                    self.load_error = Some((
                        location.clone(),
                        DecodeError {
                            code: error.raw_os_error().unwrap_or(0),
                            message: error.to_string(),
                            store_extension: None,
                        },
                    ));
                    return false;
                }
            },
            // Member sizes are fixed by the listing; a vanished member fails here.
            ItemLocation::ArchiveMember { .. } => match self.position_of(location) {
                Some(index) => (
                    self.entries[index].file_size,
                    Some(self.entries[index].modified),
                ),
                None => {
                    self.load_error = Some((
                        location.clone(),
                        DecodeError {
                            code: 0,
                            message: "member no longer exists in the archive".to_string(),
                            store_extension: None,
                        },
                    ));
                    return false;
                }
            },
            // A cached remote item stays valid until an explicit reload.
            ItemLocation::Url(_) => (
                self.cache.get(location).map_or(0, |entry| entry.file_size),
                None,
            ),
        };
        let cached = self
            .cache
            .get(location)
            .filter(|entry| entry.file_size == file_size)
            .map(|entry| (entry.image.clone(), entry.modified, entry.preview));
        let mut kind = JobKind::Preview;
        if let Some((image, cached_modified, preview)) = cached {
            self.show_image(location.clone(), image, file_size, cached_modified);
            self.load_error = None;
            if !preview {
                self.pending_display = None;
                self.preload_neighbors();
                return true;
            }
            kind = JobKind::Full; // the preview is already on screen
        }
        let displayed = kind == JobKind::Full;
        self.pending_display = Some(location.clone());
        // The new load owns the view; a leftover error would mask its progress.
        self.load_error = None;
        if let Some(cancellation) = self.in_flight.get(location) {
            // Already queued as a preload: revoke any cancellation and promote.
            cancellation.store(false, Ordering::Relaxed);
            self.pool.promote(location);
        } else if kind == JobKind::Preview {
            let cancellation = Arc::new(AtomicBool::new(false));
            self.in_flight
                .insert(location.clone(), cancellation.clone());
            self.pool.submit(
                location.clone(),
                file_size,
                modified,
                cancellation,
                kind,
                true,
            );
        }
        // A deferred full decode waits for navigation to rest; the caller schedules it.
        self.preload_neighbors();
        displayed
    }

    /// A pending item is showing its preview and waits for the deferred full decode.
    pub fn full_decode_pending(&self) -> bool {
        self.pending_display.as_ref().is_some_and(|pending| {
            !self.in_flight.contains_key(pending)
                && self.cache.get(pending).is_some_and(|entry| entry.preview)
        })
    }

    /// Submits the deferred full decode for the pending preview; a no-op when none waits.
    pub fn start_pending_full_decode(&mut self) {
        if !self.full_decode_pending() {
            return;
        }
        let Some(location) = self.pending_display.clone() else {
            return;
        };
        let Some(entry) = self.cache.get(&location) else {
            return;
        };
        let (file_size, modified) = (entry.file_size, entry.modified);
        let cancellation = Arc::new(AtomicBool::new(false));
        self.in_flight
            .insert(location.clone(), cancellation.clone());
        self.pool.submit(
            location,
            file_size,
            modified,
            cancellation,
            JobKind::Full,
            true,
        );
    }

    pub fn navigate(&mut self, command: NavigationCommand) -> Option<bool> {
        let anchor = self.navigation_anchor();
        let target = self.navigation_target(command)?;
        if anchor.is_some_and(|anchor| anchor == &target) {
            return None; // same item, nothing to do
        }
        self.note_navigation(command);
        Some(self.load_item(&target))
    }

    /// Jumps to a listing entry; the index maps the open menu's snapshot, so no rescan first.
    pub fn navigate_to_entry(&mut self, index: usize) -> Option<bool> {
        let target = self.entries.get(index)?.location.clone();
        if self
            .navigation_anchor()
            .is_some_and(|anchor| anchor == &target)
        {
            return None; // same item, nothing to do
        }
        self.opposite_steps = 0; // a jump keeps the polarity but breaks the run
        Some(self.load_item(&target))
    }

    pub fn peek_navigation_target(&self, command: NavigationCommand) -> Option<ItemLocation> {
        self.navigation_target(command)
    }

    /// Empty-window state for when a delete leaves nothing to show.
    pub fn clear_current_item(&mut self) {
        self.pending_display = None;
        self.load_error = None;
        if let Some(previous) = self.current.take() {
            self.reaper.reap(previous.image);
        }
    }

    /// Replaces the displayed image, freeing the outgoing buffer off the UI thread.
    fn show_image(
        &mut self,
        location: ItemLocation,
        image: Arc<DecodedImage>,
        file_size: u64,
        modified: Option<SystemTime>,
    ) {
        if let Some(previous) = self.current.take() {
            self.reaper.reap(previous.image);
        }
        self.current = Some(CurrentImage {
            location,
            image,
            file_size,
            modified,
        });
    }

    /// Caches an entry, freeing any replaced stand-in off the UI thread.
    fn cache_image(&mut self, location: ItemLocation, entry: CacheEntry) {
        if let Some(replaced) = self.cache.insert(location, entry) {
            self.reaper.reap(replaced.image);
        }
    }

    /// The just-attempted plain-file open failed synchronously as not-found; scans report later.
    pub fn open_failed_missing(&self, path: &Path) -> bool {
        const ERROR_FILE_NOT_FOUND: i32 = 2;
        const ERROR_PATH_NOT_FOUND: i32 = 3;
        let Some((location, error)) = &self.load_error else {
            return false;
        };
        if !matches!(error.code, ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND) {
            return false;
        }
        let Ok(path) = std::path::absolute(path) else {
            return false;
        };
        *location == ItemLocation::File(path)
    }

    /// True while a remote image download owns the pending slot.
    pub fn url_download_pending(&self) -> bool {
        matches!(self.pending_display, Some(ItemLocation::Url(_)))
    }

    pub fn has_pending_display(&self) -> bool {
        self.pending_display.is_some()
    }

    /// The current item's path when it is a local file (not an archive member or URL).
    pub fn current_file(&self) -> Option<&Path> {
        self.current
            .as_ref()
            .and_then(|current| current.location.as_file())
    }

    /// The file backing the current item: the file itself, or the archive holding a member.
    pub fn current_containing_file(&self) -> Option<&Path> {
        self.current
            .as_ref()
            .and_then(|current| current.location.containing_file())
    }

    /// The position baseline: the in-flight load, else the errored item, else the display.
    pub fn navigation_anchor(&self) -> Option<&ItemLocation> {
        self.pending_display
            .as_ref()
            .or_else(|| self.load_error.as_ref().map(|(location, _)| location))
            .or_else(|| self.current.as_ref().map(|current| &current.location))
    }

    fn navigation_target(&self, command: NavigationCommand) -> Option<ItemLocation> {
        if self.entries.is_empty() {
            return None;
        }
        let anchor_index = self
            .navigation_anchor()
            .and_then(|location| self.position_of(location));
        let length = self.entries.len();
        let looped = self.options.loop_within_folder;
        let index = match command {
            NavigationCommand::First => 0,
            NavigationCommand::Last => length - 1,
            NavigationCommand::Next => step_index(anchor_index, 1, length, looped)?,
            NavigationCommand::Previous => step_index(anchor_index, -1, length, looped)?,
        };
        Some(self.entries[index].location.clone())
    }

    pub fn on_decode_complete(&mut self, completion: DecodeCompletion) -> bool {
        if matches!(completion.stage, DecodeStage::Preview) {
            let is_pending = self
                .pending_display
                .as_ref()
                .is_some_and(|pending| *pending == completion.location);
            if is_pending && let Ok(image) = completion.result {
                self.show_image(
                    completion.location,
                    image,
                    completion.file_size,
                    completion.modified,
                );
                self.load_error = None;
                return true;
            }
            return false;
        }
        self.in_flight.remove(&completion.location);
        let is_pending = self
            .pending_display
            .as_ref()
            .is_some_and(|pending| *pending == completion.location);
        // Preview stages always post Ok; an Err would fall through to Final's failure paths.
        if matches!(completion.stage, DecodeStage::PreviewFinal)
            && let Ok(image) = &completion.result
        {
            self.cache_image(
                completion.location.clone(),
                CacheEntry {
                    file_size: completion.file_size,
                    modified: completion.modified,
                    preview: true,
                    image: image.clone(),
                },
            );
            if is_pending {
                // Waited on: show it and go buy the full decode it stands in for.
                return self.load_item(&completion.location);
            }
            self.evict_cache();
            return false;
        }
        if let Err(error) = &completion.result
            && error.is_cancelled()
        {
            // Navigation can return to an item while its decode is cancelling. A cached
            // preview stands in and its full decode waits for the deferral timer.
            if is_pending
                && !self
                    .cache
                    .get(&completion.location)
                    .is_some_and(|entry| entry.preview && entry.file_size == completion.file_size)
            {
                let cancellation = Arc::new(AtomicBool::new(false));
                self.in_flight
                    .insert(completion.location.clone(), cancellation.clone());
                self.pool.submit(
                    completion.location,
                    completion.file_size,
                    completion.modified,
                    cancellation,
                    JobKind::Preview,
                    true,
                );
            }
            return false;
        }
        match completion.result {
            Ok(image) => {
                self.cache_image(
                    completion.location.clone(),
                    CacheEntry {
                        file_size: completion.file_size,
                        modified: completion.modified,
                        preview: false,
                        image: image.clone(),
                    },
                );
                if is_pending {
                    self.show_image(
                        completion.location,
                        image,
                        completion.file_size,
                        completion.modified,
                    );
                    self.pending_display = None;
                    self.load_error = None;
                    self.preload_neighbors();
                    true
                } else {
                    self.evict_cache();
                    false
                }
            }
            Err(error) => {
                if is_pending {
                    self.pending_display = None;
                    self.load_error = Some((completion.location, error));
                    self.preload_neighbors();
                }
                is_pending
            }
        }
    }

    fn rescan_folder(&mut self, directory: &Path) {
        let scope = ListingScope::Directory(directory.to_path_buf());
        let scan = ScannedListing::of(scope, &self.options);
        self.entries = scan.result.unwrap_or_default();
        self.listing_scope = Some(scan.scope);
    }

    /// Drops a deleted item from the listing snapshot; no rescan.
    pub fn remove_listing_entry(&mut self, location: &ItemLocation) {
        if let Some(index) = self.position_of(location) {
            self.entries.remove(index);
        }
    }

    /// Synchronous by design; only opens enumerate off the UI thread.
    pub fn rescan_listing(&mut self) {
        match &self.listing_scope {
            Some(ListingScope::Directory(directory)) => self.rescan_folder(&directory.clone()),
            Some(ListingScope::Archive(_)) => {
                // Archives are read-only in riv; order is all a rescan could change.
                sort_entries(&mut self.entries, &self.options);
            }
            None => {}
        }
    }

    fn position_of(&self, location: &ItemLocation) -> Option<usize> {
        self.entries
            .iter()
            .position(|entry| entry.location == *location)
    }

    fn preload_neighbors(&mut self) {
        let (backward, forward, budget) = self.preload_plan();
        if backward == 0 && forward == 0 {
            // preloading off: drop the cache
            for (_, entry) in self.cache.drain() {
                self.reaper.reap(entry.image);
            }
        } else if let Some(anchor_index) = self
            .navigation_anchor()
            .and_then(|location| self.position_of(location))
        {
            let length = self.entries.len();
            for offset in preload_offsets(backward, forward) {
                let Some(index) = neighbor_index(
                    anchor_index,
                    offset,
                    length,
                    self.options.loop_within_folder,
                ) else {
                    continue;
                };
                let entry = &self.entries[index];
                // Cheap speculation: RAW neighbors get the preview; animations stop at frame one.
                let kind = match &entry.location {
                    ItemLocation::File(path) if decode::is_raw_two_stage(path) => JobKind::Preview,
                    _ => JobKind::Full,
                };
                // The oversize gate is about decoded weight; previews stay cheap.
                if (kind == JobKind::Full && entry.file_size > budget / 2)
                    || self.in_flight.contains_key(&entry.location)
                    || self
                        .cache
                        .get(&entry.location)
                        .is_some_and(|cached| cached.file_size == entry.file_size)
                    || self
                        .pending_display
                        .as_ref()
                        .is_some_and(|pending| *pending == entry.location)
                {
                    continue;
                }
                let cancellation = Arc::new(AtomicBool::new(false));
                self.in_flight
                    .insert(entry.location.clone(), cancellation.clone());
                self.pool.submit(
                    entry.location.clone(),
                    entry.file_size,
                    Some(entry.modified),
                    cancellation,
                    kind,
                    false,
                );
            }
        }
        self.cancel_irrelevant_decodes();
        self.evict_cache();
    }

    /// Cancels queued or running decodes outside the preload neighborhood.
    fn cancel_irrelevant_decodes(&mut self) {
        let mut relevant: HashSet<ItemLocation> = HashSet::new();
        if let Some(pending) = &self.pending_display {
            relevant.insert(pending.clone());
        }
        if let Some(current) = &self.current {
            relevant.insert(current.location.clone());
        }
        let (backward, forward, _) = self.preload_plan();
        let anchor_index = self
            .navigation_anchor()
            .and_then(|location| self.position_of(location));
        if let Some(anchor_index) = anchor_index {
            let length = self.entries.len();
            for offset in preload_offsets(backward, forward) {
                if let Some(index) = neighbor_index(
                    anchor_index,
                    offset,
                    length,
                    self.options.loop_within_folder,
                ) {
                    relevant.insert(self.entries[index].location.clone());
                }
            }
        }
        for location in self.pool.remove_queued_except(&relevant) {
            self.in_flight.remove(&location);
        }
        for (location, cancellation) in &self.in_flight {
            if !relevant.contains(location) {
                cancellation.store(true, Ordering::Relaxed);
            }
        }
    }

    /// Evicts entries in reverse preload priority until within budget.
    fn evict_cache(&mut self) {
        let (backward, forward, budget) = self.preload_plan();
        let mut total: u64 = self
            .cache
            .values()
            .map(|entry| entry.image.pixel_bytes() as u64)
            .sum();
        if total <= budget {
            return;
        }
        let anchor = self.navigation_anchor();
        let anchor_index = anchor.and_then(|location| self.position_of(location));
        let length = self.entries.len();
        let loop_enabled = self.options.loop_within_folder;
        let priorities = anchor_index.map_or_else(HashMap::new, |anchor| {
            preload_priorities(anchor, backward, forward, length, loop_enabled)
        });
        let mut ranked: Vec<(ItemLocation, u64, (u8, usize))> = self
            .cache
            .iter()
            .map(|(location, entry)| {
                // The baseline item goes last even when unlisted (URL items).
                let key = if anchor == Some(location) {
                    (0, 0)
                } else {
                    self.position_of(location).zip(anchor_index).map_or(
                        UNLISTED_EVICTION_KEY,
                        |(index, anchor)| match priorities.get(&index) {
                            Some(priority) => (0, *priority),
                            None => (
                                1,
                                ring_offset(index, anchor, length, loop_enabled).unsigned_abs(),
                            ),
                        },
                    )
                };
                (location.clone(), entry.image.pixel_bytes() as u64, key)
            })
            .collect();
        ranked.sort_by_key(|(_, _, key)| std::cmp::Reverse(*key));
        for (location, cost, _) in ranked {
            if total <= budget {
                break;
            }
            if let Some(entry) = self.cache.remove(&location) {
                self.reaper.reap(entry.image);
            }
            total -= cost;
        }
    }
}

/// First index of a `capacity` window centered on the anchor, clamped to the list.
fn playlist_window_start(total: usize, anchor: Option<usize>, capacity: usize) -> usize {
    if total <= capacity {
        return 0;
    }
    anchor
        .unwrap_or(0)
        .saturating_sub(capacity / 2)
        .min(total - capacity)
}

fn neighbor_index(
    anchor: usize,
    offset: isize,
    length: usize,
    loop_enabled: bool,
) -> Option<usize> {
    if length == 0 {
        return None;
    }
    let index = anchor as isize + offset;
    if loop_enabled {
        let wrapped = index.rem_euclid(length as isize) as usize;
        (wrapped != anchor).then_some(wrapped)
    } else {
        (0..length as isize)
            .contains(&index)
            .then_some(index as usize)
    }
}

/// Preload targets in priority order: forward first, nearest first.
fn preload_offsets(backward: usize, forward: usize) -> impl Iterator<Item = isize> {
    (1..=forward as isize).chain((1..=backward as isize).map(|step| -step))
}

/// Next/Previous candidates in walk order; an absent anchor starts at the matching end.
/// One step from the anchor (or the matching end when absent); None past a non-looping end.
fn step_index(
    anchor: Option<usize>,
    direction: isize,
    length: usize,
    looped: bool,
) -> Option<usize> {
    if length == 0 {
        return None;
    }
    let length = length as isize;
    let start = anchor.map_or(if direction > 0 { -1 } else { length }, |index| {
        index as isize
    });
    let index = start + direction;
    if looped {
        Some(index.rem_euclid(length) as usize)
    } else {
        (0..length).contains(&index).then_some(index as usize)
    }
}

/// Signed offset from anchor to index; the nearest way round when looping.
fn ring_offset(index: usize, anchor: usize, length: usize, loop_enabled: bool) -> isize {
    let direct = index as isize - anchor as isize;
    if !loop_enabled || length == 0 {
        return direct;
    }
    let alternate = if direct > 0 {
        direct - length as isize
    } else {
        direct + length as isize
    };
    if alternate.abs() < direct.abs() {
        alternate
    } else {
        direct
    }
}

/// Cached items outside the listing; evicted before anything ranked by preload priority.
const UNLISTED_EVICTION_KEY: (u8, usize) = (2, 0);

/// Entry index -> preload priority (anchor 0, then submission order); shared with eviction.
fn preload_priorities(
    anchor: usize,
    backward: usize,
    forward: usize,
    length: usize,
    loop_enabled: bool,
) -> HashMap<usize, usize> {
    let mut priorities = HashMap::from([(anchor, 0)]);
    for (rank, offset) in preload_offsets(backward, forward).enumerate() {
        if let Some(index) = neighbor_index(anchor, offset, length, loop_enabled) {
            priorities.entry(index).or_insert(rank + 1);
        }
    }
    priorities
}

/// ASCII case-insensitive path equality; approximates Windows filesystem behavior.
fn paths_equal(a: &Path, b: &Path) -> bool {
    a.as_os_str()
        .to_string_lossy()
        .eq_ignore_ascii_case(&b.as_os_str().to_string_lossy())
}

/// Hash key for paths_equal; component-wise equality would not match this folding.
fn path_identity(path: &Path) -> String {
    path.as_os_str().to_string_lossy().to_ascii_lowercase()
}

fn scan_folder(directory: &Path, options: &CoreOptions) -> Vec<ListingEntry> {
    let Ok(reader) = std::fs::read_dir(directory) else {
        return Vec::new();
    };
    let mut entries = Vec::new();
    for entry in reader.flatten() {
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }
        if options.skip_hidden && metadata.file_attributes() & FILE_ATTRIBUTE_HIDDEN.0 != 0 {
            continue;
        }
        let file_name = entry.file_name();
        let display_name = file_name.to_string_lossy();
        if display_name.starts_with("._") {
            continue; // skip macOS metadata files
        }
        let extension_matched = Path::new(&file_name)
            .extension()
            .map(|extension| extension.to_string_lossy().to_lowercase())
            .is_some_and(|extension| decode::is_supported_extension(&extension));
        let included = extension_matched
            || (options.detect_format_by_content && decode::probe_file(&entry.path()).is_some());
        if !included {
            continue;
        }
        let wide_name: Vec<u16> = file_name.encode_wide().chain(std::iter::once(0)).collect();
        entries.push(ListingEntry {
            location: ItemLocation::File(entry.path()),
            wide_name,
            file_size: metadata.len(),
            modified: metadata.modified().unwrap_or(UNIX_EPOCH),
            created: metadata.created().unwrap_or(UNIX_EPOCH),
        });
    }
    entries
}

/// Entry for an image member; other member types drop out of the listing.
fn member_entry(archive: &Path, member: archive_reader::MemberInfo) -> Option<ListingEntry> {
    Path::new(&member.name)
        .extension()
        .map(|extension| extension.to_string_lossy().to_lowercase())
        .filter(|extension| decode::is_supported_extension(extension))?;
    let wide_name: Vec<u16> = member
        .name
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    Some(ListingEntry {
        location: ItemLocation::ArchiveMember {
            archive: archive.to_path_buf(),
            member: member.name,
        },
        wide_name,
        file_size: member.size,
        modified: member.modified,
        created: member.modified, // archives do not record creation times
    })
}

fn sort_entries(entries: &mut [ListingEntry], options: &CoreOptions) {
    match options.sort_mode {
        SortMode::Name => entries.sort_by(compare_natural_names),
        SortMode::Modified => {
            entries.sort_by(|a, b| {
                b.modified
                    .cmp(&a.modified)
                    .then(compare_natural_names(a, b))
            });
        }
        SortMode::Created => {
            entries.sort_by(|a, b| b.created.cmp(&a.created).then(compare_natural_names(a, b)));
        }
        SortMode::Size => {
            entries.sort_by(|a, b| {
                b.file_size
                    .cmp(&a.file_size)
                    .then(compare_natural_names(a, b))
            });
        }
        SortMode::Type => entries.sort_by(|a, b| {
            format_name_of(&a.location)
                .cmp(format_name_of(&b.location))
                .then(compare_natural_names(a, b))
        }),
    }
    if options.sort_descending {
        entries.reverse();
    }
}

fn compare_natural_names(a: &ListingEntry, b: &ListingEntry) -> std::cmp::Ordering {
    natural_order(&a.wide_name, &b.wide_name)
}

/// Explorer's natural order over null-terminated UTF-16 names.
pub fn natural_order(a: &[u16], b: &[u16]) -> std::cmp::Ordering {
    let result = unsafe { StrCmpLogicalW(PCWSTR(a.as_ptr()), PCWSTR(b.as_ptr())) };
    result.cmp(&0)
}

fn format_name_of(location: &ItemLocation) -> &'static str {
    location
        .extension_lowercase()
        .and_then(|extension| decode::format_name_for_extension(&extension))
        .unwrap_or("")
}

/// Fixed once a worker takes the job; PreviewOnly and the speculative flag keep preload cheap.
#[derive(Clone, Copy, PartialEq, Eq)]
enum JobKind {
    Full,
    /// Stops at a RAW's embedded preview when one exists; decodes fully otherwise.
    Preview,
}

struct DecodeJob {
    location: ItemLocation,
    file_size: u64,
    /// Carried from the submit stat or the listing; the worker echoes it, never stats.
    modified: Option<SystemTime>,
    cancellation: Arc<AtomicBool>,
    kind: JobKind,
    /// Preloaded on a guess, so an animation stops at its first frame.
    speculative: bool,
}

struct PoolShared {
    queue: Mutex<VecDeque<DecodeJob>>,
    available: Condvar,
}

struct DecodePool {
    shared: Arc<PoolShared>,
}

impl DecodePool {
    fn new(window: isize) -> Self {
        let shared = Arc::new(PoolShared {
            queue: Mutex::new(VecDeque::new()),
            available: Condvar::new(),
        });
        let worker_count =
            std::thread::available_parallelism().map_or(2, |count| count.get().min(8));
        for _ in 0..worker_count {
            let shared = shared.clone();
            std::thread::spawn(move || worker_loop(&shared, window));
        }
        Self { shared }
    }

    fn submit(
        &self,
        location: ItemLocation,
        file_size: u64,
        modified: Option<SystemTime>,
        cancellation: Arc<AtomicBool>,
        kind: JobKind,
        awaited: bool,
    ) {
        let mut queue = self.shared.queue.lock().expect("decode queue poisoned");
        let job = DecodeJob {
            location,
            file_size,
            modified,
            cancellation,
            kind,
            speculative: !awaited, // only a preload goes to the back of the queue
        };
        if awaited {
            queue.push_front(job);
        } else {
            queue.push_back(job);
        }
        drop(queue);
        self.shared.available.notify_one();
    }

    /// A job a worker already took keeps its kind; PreviewFinal covers that arrival.
    fn promote(&self, location: &ItemLocation) {
        let mut queue = self.shared.queue.lock().expect("decode queue poisoned");
        if let Some(position) = queue.iter().position(|job| job.location == *location)
            && let Some(mut job) = queue.remove(position)
        {
            job.speculative = false; // someone is waiting on it now
            queue.push_front(job);
        }
    }

    /// Removes queued jobs outside the relevant set; running jobs are unaffected.
    fn remove_queued_except(&self, relevant: &HashSet<ItemLocation>) -> Vec<ItemLocation> {
        let mut queue = self.shared.queue.lock().expect("decode queue poisoned");
        let mut removed = Vec::new();
        queue.retain(|job| {
            if relevant.contains(&job.location) {
                true
            } else {
                removed.push(job.location.clone());
                false
            }
        });
        removed
    }
}

impl From<archive_reader::ArchiveError> for DecodeError {
    fn from(error: archive_reader::ArchiveError) -> Self {
        if error.cancelled {
            Self::cancelled()
        } else {
            Self {
                code: error.code,
                message: error.message,
                store_extension: None,
            }
        }
    }
}

impl From<curl::NetworkError> for DecodeError {
    fn from(error: curl::NetworkError) -> Self {
        if error.cancelled {
            Self::cancelled()
        } else {
            Self {
                code: error.code,
                message: error.message,
                store_extension: None,
            }
        }
    }
}

fn worker_loop(shared: &PoolShared, window: isize) {
    unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }
        .ok()
        .expect("CoInitializeEx MTA failed");
    loop {
        let job = {
            let mut queue = shared.queue.lock().expect("decode queue poisoned");
            loop {
                if let Some(job) = queue.pop_front() {
                    break job;
                }
                queue = shared.available.wait(queue).expect("decode queue poisoned");
            }
        };
        let mut file_size = job.file_size;
        let modified = job.modified;
        let result = match &job.location {
            ItemLocation::File(path) => {
                let post_preview = |image: DecodedImage, last: bool| {
                    post_boxed(
                        window,
                        WM_APP_DECODE_COMPLETE,
                        Box::new(DecodeCompletion {
                            location: job.location.clone(),
                            file_size: job.file_size,
                            modified,
                            stage: if last {
                                DecodeStage::PreviewFinal
                            } else {
                                DecodeStage::Preview
                            },
                            result: Ok(Arc::new(image)),
                        }),
                    );
                };
                if job.kind != JobKind::Full
                    && decode::is_raw_two_stage(path)
                    && let Some(preview) = decode::decode_raw_preview(path, &job.cancellation)
                {
                    post_preview(preview, true);
                    continue; // the full decode waits until someone asks for it
                }
                // An animation opens on its first frame; a guess stops there.
                if (job.speculative || job.kind != JobKind::Full)
                    && let Some(first_frame) =
                        decode::decode_animation_first_frame(path, &job.cancellation)
                {
                    post_preview(first_frame, job.speculative);
                    if job.speculative {
                        continue;
                    }
                }
                decode::decode_file(path, &job.cancellation)
            }
            ItemLocation::ArchiveMember { archive, member } => {
                match archive_reader::read_member(archive, member, &job.cancellation) {
                    Ok(data) => {
                        let extension = Path::new(member)
                            .extension()
                            .map(|extension| extension.to_string_lossy().to_lowercase());
                        decode::decode_bytes(&data, extension.as_deref(), &job.cancellation)
                    }
                    Err(error) => Err(error.into()),
                }
            }
            ItemLocation::Url(url) => {
                let mut last_report: Option<Instant> = None;
                let mut report = |received_bytes: u64| {
                    if last_report.is_some_and(|last| last.elapsed() < DOWNLOAD_PROGRESS_INTERVAL) {
                        return;
                    }
                    last_report = Some(Instant::now());
                    post_boxed(
                        window,
                        WM_APP_DOWNLOAD_PROGRESS,
                        Box::new(DownloadProgress {
                            location: job.location.clone(),
                            received_bytes,
                        }),
                    );
                };
                match curl::download(url, &job.cancellation, &mut report) {
                    Ok(data) => {
                        file_size = data.len() as u64; // the remote size becomes known here
                        let extension = curl::extension_lowercase(url);
                        decode::decode_bytes(&data, extension.as_deref(), &job.cancellation)
                            .map_err(url_decode_error)
                    }
                    Err(error) => Err(error.into()),
                }
            }
        }
        .map(Arc::new);
        post_boxed(
            window,
            WM_APP_DECODE_COMPLETE,
            Box::new(DecodeCompletion {
                location: job.location,
                file_size,
                modified,
                stage: DecodeStage::Final,
                result,
            }),
        );
    }
}

/// Unrecognized downloaded bytes (an HTML page, most often) get a plain message.
fn url_decode_error(error: DecodeError) -> DecodeError {
    if error.is_unrecognized_format() {
        return DecodeError {
            message: "no image at this URL".to_string(),
            ..error
        };
    }
    error
}

/// Posts an owned payload; reclaims it when the message cannot be delivered.
fn post_boxed<T>(window: isize, message: u32, payload: Box<T>) {
    let pointer = Box::into_raw(payload);
    let posted = unsafe {
        PostMessageW(
            Some(HWND(window as *mut c_void)),
            message,
            WPARAM(0),
            LPARAM(pointer as isize),
        )
    };
    if posted.is_err() {
        drop(unsafe { Box::from_raw(pointer) });
    }
}

#[cfg(test)]
mod step_index_tests {
    use super::*;

    #[test]
    fn an_absent_anchor_starts_at_the_matching_end() {
        assert_eq!(step_index(None, 1, 3, false), Some(0));
        assert_eq!(step_index(None, -1, 3, false), Some(2));
    }

    #[test]
    fn steps_move_one_entry_and_ends_obey_the_loop_option() {
        assert_eq!(step_index(Some(1), 1, 4, false), Some(2));
        assert_eq!(step_index(Some(3), 1, 4, false), None);
        assert_eq!(step_index(Some(3), 1, 4, true), Some(0));
        assert_eq!(step_index(Some(0), -1, 4, false), None);
        assert_eq!(step_index(Some(0), -1, 4, true), Some(3));
    }

    #[test]
    fn degenerate_lengths_stay_in_bounds() {
        assert_eq!(step_index(None, 1, 0, true), None);
        assert_eq!(step_index(None, 1, 1, false), Some(0));
        assert_eq!(step_index(Some(0), 1, 1, true), Some(0));
        assert_eq!(step_index(Some(0), 1, 1, false), None);
    }
}

#[cfg(test)]
mod preload_geometry_tests {
    use super::*;

    fn offsets(mode: usize) -> Vec<isize> {
        let (backward, forward, _) = PRELOAD_SPECIFICATIONS[mode];
        preload_offsets(backward, forward).collect()
    }

    #[test]
    fn offsets_run_forward_before_backward() {
        assert!(offsets(0).is_empty());
        assert_eq!(offsets(1), [1, 2, 3, -1]);
        assert_eq!(offsets(2), [1, 2, 3, 4, 5, 6, -1, -2]);
    }

    #[test]
    fn ring_offset_takes_the_nearest_way_round() {
        assert_eq!(ring_offset(7, 2, 10, false), 5);
        assert_eq!(ring_offset(0, 8, 10, false), -8);
        // Looping: crossing the seam is nearer than walking back.
        assert_eq!(ring_offset(0, 8, 10, true), 2);
        assert_eq!(ring_offset(8, 0, 10, true), -2);
        assert_eq!(ring_offset(2, 2, 10, true), 0);
        // Exactly half way round: the direct reading wins the tie.
        assert_eq!(ring_offset(5, 0, 10, true), 5);
        assert_eq!(ring_offset(0, 5, 10, true), -5);
        // Degenerate listings must not wrap into nonsense.
        assert_eq!(ring_offset(0, 0, 1, true), 0);
        assert_eq!(ring_offset(1, 0, 2, true), 1);
    }

    #[test]
    fn eviction_prefers_forward_over_backward_within_the_neighborhood() {
        let (backward, forward, _) = PRELOAD_SPECIFICATIONS[1];
        let priorities = preload_priorities(10, backward, forward, 100, false);
        // The anchor survives longest, then +1..+3, then -1.
        assert_eq!(priorities[&10], 0);
        assert_eq!(priorities[&11], 1);
        assert_eq!(priorities[&12], 2);
        assert_eq!(priorities[&13], 3);
        assert_eq!(priorities[&9], 4);
        assert_eq!(priorities.len(), 5);
    }

    #[test]
    fn eviction_drops_outsiders_before_preload_targets() {
        let (backward, forward, _) = PRELOAD_SPECIFICATIONS[1];
        let priorities = preload_priorities(10, backward, forward, 100, false);
        // The old -1 strands at -2, outside the map: (1, distance) keys evict first.
        assert!(!priorities.contains_key(&8));
        assert!(!priorities.contains_key(&14));
        // Anything unlisted goes before even the farthest outsider.
        assert!(UNLISTED_EVICTION_KEY > (1, usize::MAX));
    }

    #[test]
    fn eviction_keeps_wrapped_preload_targets() {
        let (backward, forward, _) = PRELOAD_SPECIFICATIONS[1];
        // Five looping entries: +3 lands at ring offset -2 yet stays a target.
        let priorities = preload_priorities(0, backward, forward, 5, true);
        assert_eq!(priorities[&3], 3);
        assert_eq!(priorities[&4], 4);
        assert_eq!(priorities.len(), 5);
        // Three looping entries: +2 claims the slot before -1 revisits it.
        let priorities = preload_priorities(0, backward, forward, 3, true);
        assert_eq!(priorities[&2], 2);
        assert_eq!(priorities.len(), 3);
    }
}

#[cfg(test)]
mod item_location_tests {
    use super::*;

    fn member(archive: &str, member: &str) -> ItemLocation {
        ItemLocation::ArchiveMember {
            archive: PathBuf::from(archive),
            member: member.to_string(),
        }
    }

    #[test]
    fn member_display_name_takes_the_basename() {
        assert_eq!(member("C:\\a.cbz", "art/01.png").display_name(), "01.png");
        assert_eq!(member("C:\\a.cbz", "art\\02.png").display_name(), "02.png");
        assert_eq!(member("C:\\a.cbz", "03.png").display_name(), "03.png");
    }

    #[test]
    fn member_display_text_joins_archive_and_member() {
        assert_eq!(
            member("C:\\a.cbz", "art/01.png").display_text(),
            "C:\\a.cbz \u{203a} art/01.png"
        );
    }

    #[test]
    fn folder_name_takes_the_parent_folder_leaf() {
        let file = |path: &str| ItemLocation::File(PathBuf::from(path));
        let folder = file("C:\\photos\\vacation\\img.png").folder_name();
        assert_eq!(folder.as_deref(), Some("vacation"));
        // A file at the drive root has no folder to show; URLs never do.
        assert_eq!(file("C:\\img.png").folder_name(), None);
        let url = ItemLocation::Url("https://example.com/img.png".to_string());
        assert_eq!(url.folder_name(), None);
    }

    #[test]
    fn member_folder_name_is_the_parent_inside_the_archive() {
        let folder = |member_path: &str| member("C:\\a.cbz", member_path).folder_name();
        assert_eq!(folder("albums/2024/img.png").as_deref(), Some("2024"));
        assert_eq!(folder("albums\\img.png").as_deref(), Some("albums"));
        // A root member falls back to the archive's own name.
        assert_eq!(folder("img.png").as_deref(), Some("a.cbz"));
    }

    #[test]
    fn locations_compare_with_windows_path_semantics() {
        let file = |path: &str| ItemLocation::File(PathBuf::from(path));
        assert!(file("C:\\A.PNG") == file("c:\\a.png"));
        assert!(member("C:\\A.CBZ", "01.png") == member("c:\\a.cbz", "01.png"));
        // Member names stay exact: archives distinguish case.
        assert!(member("C:\\a.cbz", "01.PNG") != member("C:\\a.cbz", "01.png"));
        assert!(file("C:\\a.cbz") != member("C:\\a.cbz", "01.png"));
    }

    #[test]
    fn locations_hash_consistently_with_equality() {
        let file = |path: &str| ItemLocation::File(PathBuf::from(path));
        let mut cache = HashMap::new();
        cache.insert(file("c:\\photos\\A.PNG"), "decoded");
        // A listing entry carries the on-disk casing; the cache must still hit.
        assert_eq!(cache.get(&file("C:\\Photos\\a.png")), Some(&"decoded"));
        cache.insert(file("C:\\Photos\\a.png"), "again");
        assert_eq!(cache.len(), 1); // one file, one key

        let mut members = HashMap::new();
        members.insert(member("C:\\A.CBZ", "art/01.png"), "decoded");
        assert_eq!(
            members.get(&member("c:\\a.cbz", "art/01.png")),
            Some(&"decoded")
        );
        assert_eq!(members.get(&member("C:\\A.CBZ", "art/01.PNG")), None);
    }

    #[test]
    fn member_extension_resolves_format_names() {
        assert_eq!(format_name_of(&member("C:\\a.cbz", "art/01.png")), "PNG");
        assert_eq!(format_name_of(&member("C:\\a.cbz", "readme.txt")), "");
    }

    #[test]
    fn url_locations_stand_alone() {
        let url = |text: &str| ItemLocation::Url(text.to_string());
        let location = url("https://a.com/b/c.png?width=1");
        assert_eq!(location.display_name(), "c.png");
        assert_eq!(location.display_text(), "https://a.com/b/c.png?width=1");
        assert_eq!(location.containing_file(), None);
        assert_eq!(location.as_file(), None);
        assert_eq!(format_name_of(&location), "PNG");
        // URLs compare exactly; remote paths are case-sensitive.
        assert!(location == url("https://a.com/b/c.png?width=1"));
        assert!(location != url("https://a.com/b/C.png?width=1"));
        assert!(location != ItemLocation::File(PathBuf::from("https://a.com/b/c.png?width=1")));
        let mut cache = HashMap::new();
        cache.insert(location.clone(), "decoded");
        assert_eq!(
            cache.get(&url("https://a.com/b/c.png?width=1")),
            Some(&"decoded")
        );
    }

    #[test]
    fn member_entries_keep_images_only() {
        let info = |name: &str| archive_reader::MemberInfo {
            name: name.to_string(),
            size: 10,
            modified: UNIX_EPOCH,
        };
        let archive = Path::new("C:\\a.cbz");
        assert!(member_entry(archive, info("art/01.png")).is_some());
        assert!(member_entry(archive, info("info.txt")).is_none());
        assert!(member_entry(archive, info("no_extension")).is_none());
        let entry = member_entry(archive, info("art/01.png")).expect("image member");
        assert_eq!(entry.created, entry.modified); // archives have no creation time
        assert_eq!(entry.file_size, 10);
    }
}

/// A URL attempt owns the session alone; a local open rebuilds its listing on scan arrival.
#[cfg(test)]
mod url_session_state_tests {
    use super::*;

    fn core() -> ImageCore {
        ImageCore::new(
            HWND::default(),
            CoreOptions {
                sort_mode: SortMode::Name,
                sort_descending: false,
                preloading_mode: 1,
                loop_within_folder: true,
                skip_hidden: true,
                detect_format_by_content: false,
            },
        )
    }

    fn folder_state(core: &mut ImageCore, path: &str) {
        let path = PathBuf::from(path);
        core.listing_scope = Some(ListingScope::Directory(
            path.parent().expect("parent").to_path_buf(),
        ));
        core.entries = vec![ListingEntry {
            location: ItemLocation::File(path),
            wide_name: Vec::new(),
            file_size: 0,
            modified: UNIX_EPOCH,
            created: UNIX_EPOCH,
        }];
    }

    fn decode_error(message: &str) -> DecodeError {
        DecodeError {
            code: 0,
            message: message.to_string(),
            store_extension: None,
        }
    }

    #[test]
    fn navigation_targets_need_more_than_the_anchor_itself() {
        let mut core = core();
        assert!(!core.has_navigation_targets());

        // A single entry that is the anchor itself leaves nowhere to go.
        folder_state(&mut core, "C:\\pictures\\a.png");
        core.load_error = Some((
            ItemLocation::File(PathBuf::from("C:\\pictures\\a.png")),
            decode_error("broken"),
        ));
        assert!(!core.has_navigation_targets());

        // An unlisted anchor can still reach the one listed entry.
        core.load_error = Some((
            ItemLocation::File(PathBuf::from("C:\\pictures\\note.txt")),
            decode_error("no decoder"),
        ));
        assert!(core.has_navigation_targets());

        core.entries.push(ListingEntry {
            location: ItemLocation::File(PathBuf::from("C:\\pictures\\b.png")),
            wide_name: Vec::new(),
            file_size: 0,
            modified: UNIX_EPOCH,
            created: UNIX_EPOCH,
        });
        core.load_error = Some((
            ItemLocation::File(PathBuf::from("C:\\pictures\\a.png")),
            decode_error("broken"),
        ));
        assert!(core.has_navigation_targets());
    }

    #[test]
    fn a_rejected_url_still_clears_the_listing() {
        let mut core = core();
        folder_state(&mut core, "C:\\pictures\\a.png");
        assert!(!core.load_url("ftp://a.com/b.png"));
        assert!(core.entries.is_empty());
        assert!(core.listing_scope.is_none());
        let (location, error) = core.load_error.as_ref().expect("error recorded");
        assert!(matches!(location, ItemLocation::Url(_)));
        assert!(error.message.contains("protocol"));
    }

    #[test]
    fn an_empty_paste_reports_no_url() {
        let mut core = core();
        folder_state(&mut core, "C:\\pictures\\a.png");
        assert!(!core.load_url(""));
        assert!(core.entries.is_empty());
        assert!(core.listing_scope.is_none());
        let (_, error) = core.load_error.as_ref().expect("error recorded");
        assert!(error.message.contains("clipboard"));
    }

    #[test]
    fn prose_around_a_url_is_rejected_not_parsed() {
        for text in ["see https://a/b.png look", "seehttps://a/b.pnglook"] {
            let mut core = core();
            assert!(!core.load_url(text));
            let (_, error) = core.load_error.as_ref().expect("error recorded");
            assert!(error.message.contains("protocol"));
        }
    }

    #[test]
    fn reload_retries_the_errored_url_not_the_previous_file() {
        let mut core = core();
        folder_state(&mut core, "C:\\pictures\\a.png");
        core.load_error = Some((
            ItemLocation::Url("ftp://a.com/b.png".to_string()),
            DecodeError {
                code: 0,
                message: "download failed".to_string(),
                store_extension: None,
            },
        ));
        assert!(!core.reload_current());
        // Routed back through load_url: single-item state, validation re-ran.
        assert!(core.entries.is_empty());
        assert!(core.listing_scope.is_none());
        let (_, error) = core.load_error.as_ref().expect("error recorded");
        assert!(error.message.contains("protocol"));
    }

    #[test]
    fn unrecognized_url_bytes_read_as_no_image() {
        use windows::Win32::Foundation::WINCODEC_ERR_COMPONENTNOTFOUND;
        let error = |store_extension| DecodeError {
            code: WINCODEC_ERR_COMPONENTNOTFOUND.0,
            message: "component not found".to_string(),
            store_extension,
        };
        assert_eq!(
            url_decode_error(error(None)).message,
            "no image at this URL"
        );
        // A failure that names a Store codec keeps its install hint.
        let store_hinted = url_decode_error(error(Some("avif")));
        assert_eq!(store_hinted.message, "component not found");
        assert_eq!(store_hinted.store_extension, Some("avif"));
    }

    #[test]
    fn a_new_load_clears_the_previous_error() {
        let directory = std::env::temp_dir().join("riv-error-supersede");
        std::fs::create_dir_all(&directory).expect("fixture directory");
        let file = directory.join("a.png");
        std::fs::write(&file, b"listing only; never decoded").expect("fixture file");
        let mut core = core();
        assert!(!core.load_url("ftp://a.com/b.png"));
        assert!(core.load_error.is_some());
        core.load_path(&file);
        assert!(core.load_error.is_none()); // the pending load owns the view now
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn a_local_open_after_a_url_restores_the_listing() {
        let directory = std::env::temp_dir().join("riv-url-session-state");
        std::fs::create_dir_all(&directory).expect("fixture directory");
        let file = directory.join("a.png");
        std::fs::write(&file, b"listing only; never decoded").expect("fixture file");
        let mut core = core();
        assert!(!core.load_url("ftp://a.com/b.png"));
        core.load_path(&file);
        // The scan is asynchronous: no listing until its arrival installs one.
        assert!(core.listing_scan_pending());
        assert!(core.entries.is_empty());
        let scan = ScannedListing::of(ListingScope::Directory(directory.clone()), &core.options);
        core.install_listing_scan(scan);
        assert!(!core.listing_scan_pending());
        assert!(matches!(
            core.listing_scope,
            Some(ListingScope::Directory(_))
        ));
        assert_eq!(core.entries.len(), 1);
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn a_url_open_drops_the_pending_scan() {
        let directory = std::env::temp_dir().join("riv-url-drops-scan");
        std::fs::create_dir_all(&directory).expect("fixture directory");
        let file = directory.join("a.png");
        std::fs::write(&file, b"listing only; never decoded").expect("fixture file");
        let mut core = core();
        core.load_path(&file);
        assert!(core.listing_scan_pending());
        let _ = core.load_url("ftp://a.com/b.png");
        assert!(!core.listing_scan_pending());
        let scan = ScannedListing::of(ListingScope::Directory(directory.clone()), &core.options);
        let outcome = core.install_listing_scan(scan); // nothing waits for it any more
        assert!(matches!(outcome, ListingInstall::Discarded));
        assert!(core.entries.is_empty());
        let _ = std::fs::remove_dir_all(&directory);
    }
}

/// The listing arrives from a background scan; installs are matched against the request.
#[cfg(test)]
mod listing_scan_tests {
    use super::*;

    fn core() -> ImageCore {
        ImageCore::new(
            HWND::default(),
            CoreOptions {
                sort_mode: SortMode::Name,
                sort_descending: false,
                preloading_mode: 1,
                loop_within_folder: true,
                skip_hidden: true,
                detect_format_by_content: false,
            },
        )
    }

    #[test]
    fn a_stale_scan_arrival_is_discarded() {
        let directory = std::env::temp_dir().join("riv-stale-scan");
        std::fs::create_dir_all(&directory).expect("fixture directory");
        let file = directory.join("a.png");
        std::fs::write(&file, b"listing only; never decoded").expect("fixture file");
        let mut core = core();
        core.load_path(&file);
        assert!(core.listing_scan_pending());
        let elsewhere = std::env::temp_dir().join("riv-stale-scan-elsewhere");
        let stale = ScannedListing::of(ListingScope::Directory(elsewhere), &core.options);
        assert!(matches!(
            core.install_listing_scan(stale),
            ListingInstall::Discarded
        ));
        assert!(core.listing_scan_pending()); // the requested scan is still due
        assert!(core.listing_scope.is_none());
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn the_listing_position_follows_the_pending_anchor() {
        let directory = std::env::temp_dir().join("riv-anchor-position");
        std::fs::create_dir_all(&directory).expect("fixture directory");
        std::fs::write(directory.join("a.png"), b"listing only").expect("fixture file");
        let second = directory.join("b.png");
        std::fs::write(&second, b"listing only").expect("fixture file");
        let mut core = core();
        core.rescan_folder(&directory);
        core.load_path(&second);
        assert!(core.has_pending_display()); // the decode has not landed yet
        assert_eq!(core.listing_position(), Some((2, 2)));
        let _ = std::fs::remove_dir_all(&directory);
    }

    fn archive_member(archive: &Path, name: &str) -> ListingEntry {
        member_entry(
            archive,
            archive_reader::MemberInfo {
                name: name.to_string(),
                size: 8,
                modified: UNIX_EPOCH,
            },
        )
        .expect("supported member")
    }

    #[test]
    fn an_archive_open_loads_its_first_member_on_arrival() {
        let archive = std::env::temp_dir().join("riv-archive-open.zip");
        let mut core = core();
        assert!(!core.load_path(&archive));
        assert!(core.listing_scan_pending());
        let resolved = std::path::absolute(&archive).expect("absolute");
        let scan = ScannedListing {
            scope: ListingScope::Archive(resolved.clone()),
            sort_mode: core.options.sort_mode,
            sort_descending: core.options.sort_descending,
            result: Ok(vec![archive_member(&resolved, "one.png")]),
        };
        assert!(matches!(
            core.install_listing_scan(scan),
            ListingInstall::Opened { .. }
        ));
        assert_eq!(core.entries.len(), 1);
        assert!(core.has_pending_display()); // the first member's decode is under way
    }

    #[test]
    fn a_failed_archive_enumerate_surfaces_as_the_error() {
        let archive = std::env::temp_dir().join("riv-archive-error.zip");
        let mut core = core();
        assert!(!core.load_path(&archive));
        let resolved = std::path::absolute(&archive).expect("absolute");
        let scan = ScannedListing {
            scope: ListingScope::Archive(resolved),
            sort_mode: core.options.sort_mode,
            sort_descending: core.options.sort_descending,
            result: Err(decode::uncoded_error("enumerate failed")),
        };
        assert!(matches!(
            core.install_listing_scan(scan),
            ListingInstall::Opened { displayed: false }
        ));
        assert!(core.load_error.is_some());
        assert!(!core.listing_scan_pending());
    }

    #[test]
    fn a_directory_open_loads_its_first_entry_on_arrival() {
        let directory = std::env::temp_dir().join("riv-directory-open");
        std::fs::create_dir_all(&directory).expect("fixture directory");
        let file = directory.join("a.png");
        std::fs::write(&file, b"listing only; never decoded").expect("fixture file");
        let mut core = core();
        assert!(!core.load_path(&directory));
        assert!(core.listing_scan_pending());
        let scan = ScannedListing::of(ListingScope::Directory(directory.clone()), &core.options);
        assert!(matches!(
            core.install_listing_scan(scan),
            ListingInstall::Opened { .. }
        ));
        assert_eq!(core.entries.len(), 1);
        assert!(core.has_pending_display()); // the first entry's decode is under way
        let _ = std::fs::remove_dir_all(&directory);
    }
}

/// Preload polarity follows the travel direction with a one-step grace.
#[cfg(test)]
mod travel_direction_tests {
    use super::*;

    fn core() -> ImageCore {
        ImageCore::new(
            HWND::default(),
            CoreOptions {
                sort_mode: SortMode::Name,
                sort_descending: false,
                preloading_mode: 1,
                loop_within_folder: true,
                skip_hidden: true,
                detect_format_by_content: false,
            },
        )
    }

    #[test]
    fn a_single_back_step_keeps_the_forward_polarity() {
        let mut core = core();
        core.note_navigation(NavigationCommand::Previous);
        assert!(!core.travel_backward);
        assert_eq!(core.preload_plan(), (1, 3, 1 << 30));
    }

    #[test]
    fn two_consecutive_back_steps_flip_the_polarity() {
        let mut core = core();
        core.note_navigation(NavigationCommand::Previous);
        core.note_navigation(NavigationCommand::Previous);
        assert!(core.travel_backward);
        assert_eq!(core.preload_plan(), (3, 1, 1 << 30));
        // The way back flips with the same grace.
        core.note_navigation(NavigationCommand::Next);
        assert!(core.travel_backward);
        core.note_navigation(NavigationCommand::Next);
        assert!(!core.travel_backward);
    }

    #[test]
    fn an_interrupted_run_starts_over() {
        let mut core = core();
        core.note_navigation(NavigationCommand::Previous);
        core.note_navigation(NavigationCommand::Next);
        core.note_navigation(NavigationCommand::Previous);
        assert!(!core.travel_backward); // never two in a row
    }

    #[test]
    fn jumps_declare_their_direction() {
        let mut core = core();
        core.note_navigation(NavigationCommand::Last);
        assert!(core.travel_backward);
        core.note_navigation(NavigationCommand::First);
        assert!(!core.travel_backward);
    }

    #[test]
    fn a_declared_direction_aims_at_once() {
        let mut core = core();
        core.set_travel_direction(true);
        assert_eq!(core.preload_plan(), (3, 1, 1 << 30));
        core.reset_travel_direction();
        assert_eq!(core.preload_plan(), (1, 3, 1 << 30));
    }
}

/// The menu playlist shows a fixed window with the current item centered.
#[cfg(test)]
mod playlist_window_tests {
    use super::*;

    fn core_with_files(count: usize, anchor: Option<usize>) -> ImageCore {
        let mut core = ImageCore::new(
            HWND::default(),
            CoreOptions {
                sort_mode: SortMode::Name,
                sort_descending: false,
                preloading_mode: 1,
                loop_within_folder: true,
                skip_hidden: true,
                detect_format_by_content: false,
            },
        );
        core.entries = (0..count)
            .map(|index| ListingEntry {
                location: ItemLocation::File(PathBuf::from(format!(
                    "C:\\pictures\\{index:03}.png"
                ))),
                wide_name: Vec::new(),
                file_size: 0,
                modified: UNIX_EPOCH,
                created: UNIX_EPOCH,
            })
            .collect();
        core.pending_display = anchor.map(|index| core.entries[index].location.clone());
        core
    }

    #[test]
    fn a_short_listing_shows_whole() {
        let window = core_with_files(5, Some(2)).playlist_window(25);
        assert_eq!(window.names.len(), 5);
        assert_eq!(window.first_index, 0);
        assert_eq!(window.current_slot, Some(2));
        assert_eq!(window.hidden_count, 0);
        assert_eq!(window.names[0], "000.png");
    }

    #[test]
    fn the_current_item_sits_at_the_window_center() {
        let window = core_with_files(100, Some(50)).playlist_window(25);
        assert_eq!(window.first_index, 38);
        assert_eq!(window.current_slot, Some(12));
        assert_eq!(window.names.len(), 25);
        assert_eq!(window.hidden_count, 75);
    }

    #[test]
    fn the_window_clamps_at_both_ends() {
        let near_start = core_with_files(100, Some(3)).playlist_window(25);
        assert_eq!(near_start.first_index, 0);
        assert_eq!(near_start.current_slot, Some(3));
        let near_end = core_with_files(100, Some(97)).playlist_window(25);
        assert_eq!(near_end.first_index, 75);
        assert_eq!(near_end.current_slot, Some(22));
    }

    #[test]
    fn no_anchor_starts_at_the_top() {
        let window = core_with_files(100, None).playlist_window(25);
        assert_eq!(window.first_index, 0);
        assert_eq!(window.current_slot, None);
        assert_eq!(window.hidden_count, 75);
    }
}
