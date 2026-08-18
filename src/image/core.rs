//! Load state machine, item listing, preload cache, and the decode worker pool.

use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::{Hash, Hasher};
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use windows::Win32::Foundation::HWND;
use windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_HIDDEN;
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};
use windows::Win32::UI::WindowsAndMessaging::WM_APP;
use windows::core::HSTRING;

use crate::archive::reader as archive_reader;
use crate::image::decode::{
    self, DecodeError, DecodedImage, UploadDevice, UploadedTexture, upload_still_texture,
};
use crate::network::curl;
use crate::window::message::post_boxed;

pub const WM_APP_DECODE_COMPLETE: u32 = WM_APP + 1;
pub const WM_APP_DOWNLOAD_PROGRESS: u32 = WM_APP + 7;
pub const WM_APP_LISTING_READY: u32 = WM_APP + 8;
pub const WM_APP_PROBE_COMPLETE: u32 = WM_APP + 9;

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
                hash_path_identity(path, state);
            }
            Self::ArchiveMember { archive, member } => {
                1u8.hash(state);
                hash_path_identity(archive, state);
                member.hash(state);
            }
            Self::Url(url) => {
                2u8.hash(state);
                url.hash(state);
            }
        }
    }
}

impl ListingEntry {
    /// The listing's own record of the file, in the shape a load and the cache compare.
    fn metadata(&self) -> ItemMetadata {
        ItemMetadata {
            file_size: self.file_size,
            modified: Some(self.modified),
        }
    }
}

impl ItemLocation {
    /// Leaf name for titles and messages (member basename inside archives).
    pub fn display_name(&self) -> String {
        match self {
            Self::File(path) => crate::text::file_name_text(path),
            Self::ArchiveMember { member, .. } => member
                .rsplit(['/', '\\'])
                .next()
                .unwrap_or(member)
                .to_string(),
            Self::Url(url) => curl::file_name(url).to_string(),
        }
    }

    /// Full user-facing location text ("archive › member" for members).
    pub fn display_text(&self) -> String {
        match self {
            Self::File(path) => path.display().to_string(),
            Self::ArchiveMember { archive, member } => {
                format!("{} › {member}", archive.display())
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
        crate::text::lowercase_extension(name_path)
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
    /// Stored order: the settings value is a position here, and so is the combo row.
    pub const IN_SETTING_ORDER: [Self; 5] = [
        Self::Name,
        Self::Modified,
        Self::Created,
        Self::Size,
        Self::Type,
    ];

    pub fn from_setting(value: u32) -> Self {
        Self::IN_SETTING_ORDER
            .get(value as usize)
            .copied()
            .unwrap_or(Self::Name)
    }

    pub fn description(self) -> &'static str {
        match self {
            Self::Name => "Name",
            Self::Modified => "Date modified",
            Self::Created => "Date created",
            Self::Size => "Size",
            Self::Type => "Type",
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

/// The place a dropped anchor held, named by the entry beside it so re-sorts carry it.
enum MissingAnchor {
    Front,
    After(ItemLocation),
}

/// Where a step counts from: the anchor's own index, the one it left, or neither.
#[derive(Clone, Copy, PartialEq, Eq)]
enum AnchorIndex {
    Listed(usize),
    /// The anchor is gone from the listing; it belonged just before this index.
    Missing(usize),
    /// Never listed at all (an anchor outside the listing); steps enter from the ends.
    Unlisted,
}

pub struct ListingEntry {
    pub location: ItemLocation,
    wide_name: HSTRING,
    file_size: u64,
    modified: SystemTime,
    created: SystemTime,
    /// Format name for Type sorting; the scan already knows the extension.
    format_name: &'static str,
    /// Cache weight of the fully decoded item; a probe or an arrival records it.
    weight: DecodedWeight,
}

/// The weight outlives the cache entry: eviction drops pixels, not this knowledge.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum DecodedWeight {
    Unknown,
    Known(u64),
    /// The probe failed; the item is never speculated on.
    Unavailable,
}

pub enum DecodeStage {
    /// Preview shown while the same job goes on to the full decode.
    Preview,
    /// Preview and the job stops there; the full decode is submitted separately after it arrives.
    PreviewFinal,
    Final,
}

pub struct DecodeCompletion {
    pub location: ItemLocation,
    /// Echoed from the submit stat or the listing, never re-read by the worker.
    pub metadata: ItemMetadata,
    pub stage: DecodeStage,
    pub result: Result<Arc<DecodedImage>, DecodeError>,
    /// Worker-side upload for still frames; None falls back to the UI-thread upload.
    pub texture: Option<UploadedTexture>,
}

/// Bytes received so far for a downloading URL item; 0 means connecting.
pub struct DownloadProgress {
    pub location: ItemLocation,
    pub received_bytes: u64,
}

/// A finished weight probe; a cancelled one records nothing.
pub struct ProbeCompletion {
    location: ItemLocation,
    cancelled: bool,
    weight: Option<u64>,
}

/// The file facts a load reads once and everything downstream reuses; URL items have no time.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub struct ItemMetadata {
    pub file_size: u64,
    pub modified: Option<SystemTime>,
}

pub struct CurrentImage {
    pub location: ItemLocation,
    pub image: Arc<DecodedImage>,
    pub texture: Option<UploadedTexture>,
    metadata: ItemMetadata,
}

#[derive(Clone)]
struct CacheEntry {
    metadata: ItemMetadata,
    /// Preview (RAW embedded, sub-resolution, animation first frame) until the full decode arrives.
    preview: bool,
    image: Arc<DecodedImage>,
    texture: Option<UploadedTexture>,
}

impl CacheEntry {
    /// A textured entry keeps no pixels: the texture is the only copy.
    fn new(
        metadata: ItemMetadata,
        preview: bool,
        image: Arc<DecodedImage>,
        texture: Option<UploadedTexture>,
    ) -> Self {
        let image = if texture.is_some() && image.pixel_bytes() > 0 {
            Arc::new(image.without_pixels())
        } else {
            image
        };
        Self {
            metadata,
            preview,
            image,
            texture,
        }
    }

    /// Cached pixels stand for a file of this size and modification time, nothing else.
    fn matches(&self, metadata: ItemMetadata) -> bool {
        self.metadata == metadata
    }
}

/// Frees retired image buffers off the UI thread.
struct ImageReleaser {
    sender: mpsc::Sender<Arc<DecodedImage>>,
}

impl ImageReleaser {
    fn new() -> Self {
        let (sender, receiver) = mpsc::channel::<Arc<DecodedImage>>();
        // A spawn failure drops the receiver, so every release frees inline instead.
        let _ = std::thread::Builder::new()
            .name("riv-image-releaser".to_string())
            .spawn(move || receiver.iter().for_each(drop));
        Self { sender }
    }

    fn release(&self, image: Arc<DecodedImage>) {
        let _ = self.sender.send(image);
    }
}

pub struct PlaylistWindow {
    /// What the menu shows and acts on; each label derives from its location.
    pub locations: Vec<ItemLocation>,
    /// Absolute index of the first shown entry; doubles as the count hidden before it.
    pub first_index: usize,
    pub current_slot: Option<usize>,
    pub hidden_after: usize,
}

pub struct ImageCore {
    pool: DecodePool,
    options: CoreOptions,
    listing_scope: Option<ListingScope>,
    entries: Vec<ListingEntry>,
    /// The view's own slot: replacing it invalidates whatever the last request left.
    request: ViewRequest,
    submitted_decodes: HashMap<ItemLocation, Arc<AtomicBool>>,
    /// Separate from submitted_decodes so a probe never reads as a decode already running.
    submitted_probes: HashMap<ItemLocation, Arc<AtomicBool>>,
    /// The generation arrivals must carry; mirrored here so checks skip the pool lock.
    upload_device_generation: Option<u64>,
    cache: HashMap<ItemLocation, CacheEntry>,
    pub current: Option<CurrentImage>,
    /// Preload polarity: the deeper reach aims along the navigation direction.
    navigating_backward: bool,
    /// Consecutive steps against the polarity; the second one flips it.
    opposite_steps: u32,
    releaser: ImageReleaser,
    /// Listing scan submitted (folder or archive), awaiting its ScannedListing.
    pending_scan: Option<PendingScan>,
    /// Place a vanished anchor left behind; steps continue from there, not from the ends.
    missing_anchor: Option<MissingAnchor>,
    window: isize,
}

/// A scan request: the scope it awaits, and what its arrival is for.
struct PendingScan {
    scope: ListingScope,
    purpose: ScanPurpose,
}

/// The request that owns the view: nothing, one being decoded, or one that ended in an error.
enum ViewRequest {
    Idle,
    Pending(ItemLocation),
    Failed(ItemLocation, DecodeError),
}

impl ViewRequest {
    /// The item the request is about; the position baseline follows it.
    fn location(&self) -> Option<&ItemLocation> {
        match self {
            Self::Idle => None,
            Self::Pending(location) | Self::Failed(location, _) => Some(location),
        }
    }

    fn pending(&self) -> Option<&ItemLocation> {
        match self {
            Self::Pending(location) => Some(location),
            _ => None,
        }
    }

    fn failure(&self) -> Option<(&ItemLocation, &DecodeError)> {
        match self {
            Self::Failed(location, error) => Some((location, error)),
            _ => None,
        }
    }
}

/// What an arriving listing does to the view.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ScanPurpose {
    /// A folder or archive was the target: its first entry opens on arrival.
    OpenFirstEntry,
    /// The listing behind a directly opened file; the anchor is already loading.
    CoverAnchor,
    /// Re-collection of the scope in hand; the old listing serves until this lands.
    Refresh,
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
            "Archive contains no supported images",
        ));
    }
    sort_entries(&mut entries, options);
    Ok(entries)
}

/// What an arrived scan did: nothing (stale), the listing alone, or an open.
pub enum ListingInstall {
    Discarded,
    Installed,
    Opened { outcome: LoadOutcome },
}

/// How a load ended: on screen, refused with an error the view shows, or nothing to show yet.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LoadOutcome {
    Shown,
    Failed,
    Pending,
}

impl ImageCore {
    pub fn new(window: HWND, options: CoreOptions) -> Self {
        Self {
            pool: DecodePool::new(window.0 as isize),
            options,
            listing_scope: None,
            entries: Vec::new(),
            request: ViewRequest::Idle,
            submitted_decodes: HashMap::new(),
            submitted_probes: HashMap::new(),
            upload_device_generation: None,
            cache: HashMap::new(),
            current: None,
            navigating_backward: false,
            opposite_steps: 0,
            releaser: ImageReleaser::new(),
            pending_scan: None,
            missing_anchor: None,
            window: window.0 as isize,
        }
    }

    /// Aims the preload polarity at a declared direction (slideshow start).
    pub fn set_navigation_direction(&mut self, backward: bool) {
        self.navigating_backward = backward;
        self.opposite_steps = 0;
        self.refresh_preload();
    }

    /// A fresh listing starts with the forward default.
    fn reset_navigation_direction(&mut self) {
        self.navigating_backward = false;
        self.opposite_steps = 0;
    }

    /// Jumps declare their direction; steps flip the polarity on the second one in a row.
    fn record_navigation(&mut self, command: NavigationCommand) {
        match command {
            NavigationCommand::First => {
                self.navigating_backward = false;
                self.opposite_steps = 0;
            }
            NavigationCommand::Last => {
                self.navigating_backward = true;
                self.opposite_steps = 0;
            }
            NavigationCommand::Next => self.record_step(false),
            NavigationCommand::Previous => self.record_step(true),
        }
    }

    fn record_step(&mut self, backward: bool) {
        if backward == self.navigating_backward {
            self.opposite_steps = 0;
            return;
        }
        self.opposite_steps += 1;
        if self.opposite_steps >= 2 {
            self.navigating_backward = backward;
            self.opposite_steps = 0;
        }
    }

    /// Preload distances and budget, aimed along the current navigation direction.
    fn preload_plan(&self) -> (usize, usize, u64) {
        let (backward, forward, budget) =
            PRELOAD_SPECIFICATIONS[self.options.preloading_mode.min(2)];
        if self.navigating_backward {
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
        self.refresh_preload();
    }

    pub fn listing_position(&self) -> Option<(usize, usize)> {
        let anchor = self.navigation_anchor()?;
        let index = self.position_of(anchor)?;
        Some((index + 1, self.entries.len()))
    }

    /// An action requiring navigation targets can act: somewhere to go besides the anchor.
    pub fn has_navigation_targets(&self) -> bool {
        match self.entries.len() {
            0 => false,
            // A single listed entry is a real target only for an anchor outside it.
            1 => !matches!(self.anchor_index(), AnchorIndex::Listed(_)),
            _ => true,
        }
    }

    /// Listing window for the menu: `capacity` entries centered on the anchor, the rest a count.
    pub fn playlist_window(&self, capacity: usize) -> PlaylistWindow {
        let total = self.entries.len();
        let anchor = self.anchor_index();
        // A vanished anchor still says where to look, even with no slot to mark.
        let center = match anchor {
            AnchorIndex::Listed(index) | AnchorIndex::Missing(index) => Some(index),
            AnchorIndex::Unlisted => None,
        };
        let first_index = playlist_window_start(total, center, capacity);
        let end = (first_index + capacity).min(total);
        PlaylistWindow {
            locations: self.entries[first_index..end]
                .iter()
                .map(|entry| entry.location.clone())
                .collect(),
            first_index,
            current_slot: match anchor {
                AnchorIndex::Listed(index) if (first_index..end).contains(&index) => {
                    Some(index - first_index)
                }
                _ => None,
            },
            hidden_after: total - end,
        }
    }

    /// What the current item's load already read; None once nothing is shown.
    pub fn current_item_metadata(&self) -> Option<ItemMetadata> {
        Some(self.current.as_ref()?.metadata)
    }

    /// True while this item is the one the view waits on.
    pub fn is_pending(&self, location: &ItemLocation) -> bool {
        self.request.pending() == Some(location)
    }

    /// None when there is no anchor to reload, like the navigation entry points.
    pub fn reload_current(&mut self) -> Option<LoadOutcome> {
        // Reload retries the position baseline, so an errored item reloads itself.
        let location = self.navigation_anchor().cloned()?;
        if let Some(entry) = self.cache.remove(&location) {
            self.releaser.release(entry.image);
        }
        if let ItemLocation::Url(url) = &location {
            // Back through load_url so validation errors reproduce on retry.
            return Some(self.load_url(url));
        }
        if let ItemLocation::File(path) = &location
            && archive_reader::path_is_archive(path)
        {
            // An archive anchor retries through its scan, like a URL revalidates.
            let scope = ListingScope::Archive(path.clone());
            self.submit_scan(PendingScan {
                scope,
                purpose: ScanPurpose::OpenFirstEntry,
            });
            return Some(LoadOutcome::Pending);
        }
        // A reload re-decodes the item and re-collects the listing it sits in.
        let outcome = self.load_item(&location);
        self.submit_refresh_scan();
        Some(outcome)
    }

    pub fn load_path(&mut self, path: &Path) -> LoadOutcome {
        let Ok(path) = std::path::absolute(path) else {
            return LoadOutcome::Pending;
        };
        if path.is_dir() {
            self.submit_scan(PendingScan {
                scope: ListingScope::Directory(path),
                purpose: ScanPurpose::OpenFirstEntry,
            });
            return LoadOutcome::Pending;
        }
        if archive_reader::path_is_archive(&path) {
            self.submit_scan(PendingScan {
                scope: ListingScope::Archive(path),
                purpose: ScanPurpose::OpenFirstEntry,
            });
            return LoadOutcome::Pending;
        }
        let directory = path.parent().map(Path::to_path_buf);
        if let Some(directory) = directory
            && !self.directory_covered(&directory)
        {
            self.submit_scan(PendingScan {
                scope: ListingScope::Directory(directory),
                purpose: ScanPurpose::CoverAnchor,
            });
        }
        self.load_item(&ItemLocation::File(path))
    }

    /// True when the listing or the submitted scan already covers this directory.
    fn directory_covered(&self, directory: &Path) -> bool {
        let covers = |scope: &ListingScope| matches!(scope, ListingScope::Directory(scanned) if paths_equal(scanned, directory));
        matches!(&self.listing_scope, Some(scope) if covers(scope))
            || matches!(&self.pending_scan, Some(pending) if covers(&pending.scope))
    }

    /// Clears the listing and enumerates the scope off the UI thread; it installs on arrival.
    fn submit_scan(&mut self, pending: PendingScan) {
        let scope = pending.scope.clone();
        self.reset_navigation_direction();
        self.entries = Vec::new();
        self.listing_scope = None;
        self.missing_anchor = None;
        self.pending_scan = Some(pending);
        // The old snapshot is gone, so nothing it held is reachable any more.
        self.refresh_preload();
        self.spawn_scan(scope);
    }

    /// Re-collects the current scope while the listing in hand stays usable until it lands.
    fn submit_refresh_scan(&mut self) {
        // A scan already submitted is the newer listing; leave it to arrive.
        if self.listing_scan_pending() {
            return;
        }
        let Some(scope) = self.listing_scope.clone() else {
            return;
        };
        self.pending_scan = Some(PendingScan {
            scope: scope.clone(),
            purpose: ScanPurpose::Refresh,
        });
        self.spawn_scan(scope);
    }

    /// The worker half of a scan: enumerate off the UI thread and post the listing back.
    fn spawn_scan(&self, scope: ListingScope) {
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

    /// What an anchor the incoming listing dropped now sits behind, if it sat in it at all.
    fn missing_anchor_for(&self, incoming: &[ListingEntry]) -> Option<MissingAnchor> {
        let anchor = self.navigation_anchor()?;
        let index = match self.anchor_index() {
            AnchorIndex::Listed(index) | AnchorIndex::Missing(index) => index,
            AnchorIndex::Unlisted => return None,
        };
        if incoming.iter().any(|entry| entry.location == *anchor) {
            return None;
        }
        let arriving: HashSet<&ItemLocation> =
            incoming.iter().map(|entry| &entry.location).collect();
        // The nearest surviving predecessor keeps the place where it was, additions included.
        let predecessor = self.entries[..index]
            .iter()
            .rev()
            .find(|entry| arriving.contains(&entry.location));
        Some(match predecessor {
            Some(entry) => MissingAnchor::After(entry.location.clone()),
            None => MissingAnchor::Front,
        })
    }

    /// Hands a rescanned listing the weights already probed; an edited file starts over.
    fn carry_weights_into(&self, entries: &mut [ListingEntry]) {
        let probed: HashMap<&ItemLocation, &ListingEntry> = self
            .entries
            .iter()
            .filter(|entry| entry.weight != DecodedWeight::Unknown)
            .map(|entry| (&entry.location, entry))
            .collect();
        // Nothing probed means every lookup below would miss, and each one hashes a path.
        if probed.is_empty() {
            return;
        }
        for entry in entries {
            if let Some(previous) = probed.get(&entry.location)
                && previous.metadata() == entry.metadata()
            {
                entry.weight = previous.weight;
            }
        }
    }

    /// Installs an arrived scan; a stale one or a failed refresh is dropped.
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
                // A refresh that failed to enumerate leaves the listing it was refreshing.
                if pending.purpose == ScanPurpose::Refresh {
                    return ListingInstall::Discarded;
                }
                self.request =
                    ViewRequest::Failed(ItemLocation::File(scan.scope.path().to_path_buf()), error);
                return ListingInstall::Opened {
                    outcome: LoadOutcome::Failed,
                };
            }
        };
        // The worker sorted under an options snapshot; a change since then re-sorts here.
        if scan.sort_mode != self.options.sort_mode
            || scan.sort_descending != self.options.sort_descending
        {
            sort_entries(&mut entries, &self.options);
        }
        self.carry_weights_into(&mut entries);
        self.missing_anchor = self.missing_anchor_for(&entries);
        self.entries = entries;
        self.listing_scope = Some(scan.scope);
        if pending.purpose == ScanPurpose::OpenFirstEntry {
            let Some(first) = self.entries.first().map(|entry| entry.location.clone()) else {
                return ListingInstall::Installed;
            };
            return ListingInstall::Opened {
                outcome: self.load_item(&first),
            };
        }
        self.refresh_preload();
        ListingInstall::Installed
    }

    /// True while a listing scan is submitted; the window is loading, not empty.
    pub fn listing_scan_pending(&self) -> bool {
        self.pending_scan.is_some()
    }

    /// The failed request the view reports, if the last one ended that way.
    pub fn load_failure(&self) -> Option<(&ItemLocation, &DecodeError)> {
        self.request.failure()
    }

    /// Nothing shown, nothing awaited, no scan running: the session holds no item at all.
    pub fn holds_no_item(&self) -> bool {
        self.current.is_none() && !self.has_pending_display() && !self.listing_scan_pending()
    }

    /// Opens a remote image as a standalone item (no listing, no navigation).
    pub fn load_url(&mut self, url: &str) -> LoadOutcome {
        // Even a failed attempt leaves the single-item state; no listing survives.
        self.entries = Vec::new();
        self.listing_scope = None;
        self.pending_scan = None;
        let failure = if url.is_empty() {
            Some("No URL in the clipboard") // only the paste path can deliver an empty URL
        } else if !curl::is_supported_protocol(url) {
            Some("Unsupported URL protocol")
        } else if archive_reader::url_is_archive(url) {
            Some("Archives are not supported from a URL")
        } else {
            None
        };
        let location = ItemLocation::Url(url.to_string());
        if let Some(message) = failure {
            self.request = ViewRequest::Failed(location, decode::uncoded_error(message));
            // A refused URL still leaves the single-item state; the listing is gone.
            self.refresh_preload();
            return LoadOutcome::Failed;
        }
        self.load_item(&location)
    }

    fn load_item(&mut self, location: &ItemLocation) -> LoadOutcome {
        // Another item becoming the anchor drops the place the last one left.
        let same_anchor = self.navigation_anchor() == Some(location);
        if !same_anchor {
            self.missing_anchor = None;
        }
        // This request owns the view from here; what the last one left stops deciding what shows.
        self.request = ViewRequest::Idle;
        let metadata = match location {
            ItemLocation::File(path) => match std::fs::metadata(path) {
                Ok(file) => ItemMetadata {
                    file_size: file.len(),
                    modified: file.modified().ok(),
                },
                Err(error) => {
                    self.request = ViewRequest::Failed(
                        location.clone(),
                        DecodeError {
                            code: error.raw_os_error().unwrap_or(0),
                            message: error.to_string(),
                            store_codec_names: &[],
                        },
                    );
                    // The wait this request dropped may still hold a decode; the sweep ends it.
                    self.refresh_preload();
                    return LoadOutcome::Failed;
                }
            },
            // Member sizes are fixed by the listing; a vanished member fails here.
            ItemLocation::ArchiveMember { .. } => match self.position_of(location) {
                Some(index) => self.entries[index].metadata(),
                None => {
                    self.request = ViewRequest::Failed(
                        location.clone(),
                        decode::uncoded_error("Member no longer exists in the archive"),
                    );
                    self.refresh_preload();
                    return LoadOutcome::Failed;
                }
            },
            // A cached remote item stays valid until an explicit reload.
            ItemLocation::Url(_) => ItemMetadata {
                file_size: self
                    .cache
                    .get(location)
                    .map_or(0, |entry| entry.metadata.file_size),
                modified: None,
            },
        };
        let cached = self
            .cache
            .get(location)
            .filter(|entry| entry.matches(metadata))
            .cloned();
        let mut preview_shown = false;
        if let Some(entry) = cached {
            let preview = entry.preview;
            self.show_image(CurrentImage {
                location: location.clone(),
                image: entry.image,
                texture: entry.texture,
                metadata,
            });
            if !preview {
                // Preload starts once this image is on screen.
                return LoadOutcome::Shown;
            }
            preview_shown = true;
        }
        self.request = ViewRequest::Pending(location.clone());
        if let Some(cancellation) = self.submitted_decodes.get(location) {
            // Already queued as a preload: revoke any cancellation and promote.
            cancellation.store(false, Ordering::Relaxed);
            self.pool.promote(location);
        } else {
            self.submit_pending_decode(location.clone(), metadata, preview_shown);
        }
        // The deferral timer owns the pending full decode; this call is for the sweeps.
        self.refresh_preload();
        if preview_shown {
            LoadOutcome::Shown
        } else {
            LoadOutcome::Pending
        }
    }

    /// Registers the job as submitted and hands it to the pool under a fresh cancellation.
    fn submit_decode(
        &mut self,
        location: ItemLocation,
        metadata: ItemMetadata,
        kind: JobKind,
        awaited: bool,
    ) {
        let cancellation = Arc::new(AtomicBool::new(false));
        self.submitted_decodes
            .insert(location.clone(), cancellation.clone());
        self.pool
            .submit(location, metadata, cancellation, kind, awaited);
    }

    /// A discarded arrival resubmits what the waiting item still needs.
    fn resubmit_pending_decode(&mut self, location: ItemLocation, metadata: ItemMetadata) {
        let preview_cached = self
            .cache
            .get(&location)
            .is_some_and(|entry| entry.preview && entry.matches(metadata));
        self.submit_pending_decode(location, metadata, preview_cached);
    }

    /// Submits what a pending item still needs; a cached two-stage preview waits on the timer.
    fn submit_pending_decode(
        &mut self,
        location: ItemLocation,
        metadata: ItemMetadata,
        preview_cached: bool,
    ) {
        if !preview_cached {
            self.submit_decode(location, metadata, JobKind::Preview, true);
        } else if !is_deferred_two_stage(&location) {
            self.submit_decode(location, metadata, JobKind::Full, true);
        }
    }

    /// A pending two-stage item is showing its preview and waits for the deferred full decode.
    pub fn full_decode_pending(&self) -> bool {
        self.request.pending().is_some_and(|pending| {
            !self.submitted_decodes.contains_key(pending)
                && self.cache.get(pending).is_some_and(|entry| entry.preview)
        })
    }

    /// Submits the deferred full decode for the pending preview; a no-op when none waits.
    pub fn start_pending_full_decode(&mut self) {
        let Some(location) = self.request.pending().cloned() else {
            return;
        };
        if self.submitted_decodes.contains_key(&location) {
            return;
        }
        let Some(metadata) = self
            .cache
            .get(&location)
            .filter(|entry| entry.preview)
            .map(|entry| entry.metadata)
        else {
            return;
        };
        self.submit_decode(location, metadata, JobKind::Full, true);
    }

    pub fn navigate(&mut self, command: NavigationCommand) -> Option<LoadOutcome> {
        let anchor = self.navigation_anchor();
        let target = self.navigation_target(command)?;
        if anchor.is_some_and(|anchor| anchor == &target) {
            return None;
        }
        self.record_navigation(command);
        Some(self.load_item(&target))
    }

    /// Goes to an item the caller already identified, listed or not.
    pub fn navigate_to_location(&mut self, target: &ItemLocation) -> Option<LoadOutcome> {
        let target = target.clone();
        if self
            .navigation_anchor()
            .is_some_and(|anchor| anchor == &target)
        {
            return None;
        }
        self.opposite_steps = 0; // a jump keeps the polarity but breaks the run
        Some(self.load_item(&target))
    }

    /// Empty-window state for when a delete leaves nothing to show.
    pub fn clear_current_item(&mut self) {
        self.request = ViewRequest::Idle;
        if let Some(previous) = self.current.take() {
            self.releaser.release(previous.image);
        }
    }

    /// Hands the workers their upload device and retires every other generation's entry.
    pub fn set_upload_device(&mut self, upload_device: Option<UploadDevice>) {
        let generation = upload_device
            .as_ref()
            .map(|upload_device| upload_device.generation);
        self.upload_device_generation = generation;
        self.pool.set_upload_device(upload_device);
        for (_, entry) in self.cache.extract_if(|_, entry| {
            entry
                .texture
                .as_ref()
                .is_some_and(|uploaded| Some(uploaded.generation) != generation)
        }) {
            self.releaser.release(entry.image);
        }
        if let Some(current) = self.current.as_mut() {
            current
                .texture
                .take_if(|uploaded| Some(uploaded.generation) != generation);
        }
    }

    /// Reads a texture-only current back to pixels; returns the restored image.
    pub fn recover_current_pixels(
        &mut self,
        read_back: impl FnOnce(&UploadedTexture, &DecodedImage) -> windows::core::Result<Vec<u8>>,
    ) -> Option<Arc<DecodedImage>> {
        let current = self.current.as_mut()?;
        if current.image.pixel_bytes() > 0 {
            return None;
        }
        let uploaded = current.texture.as_ref()?;
        let pixels = read_back(uploaded, &current.image).ok()?;
        // without_pixels here just clones the metadata; the image is already slim.
        let mut restored = current.image.without_pixels();
        if let Some(frame) = restored.frames.first_mut() {
            frame.pixels = pixels;
        }
        let restored = Arc::new(restored);
        current.image = restored.clone();
        current.texture = None;
        Some(restored)
    }

    /// Replaces the displayed image, freeing the outgoing buffer off the UI thread.
    fn show_image(&mut self, current: CurrentImage) {
        if let Some(previous) = self.current.take() {
            self.releaser.release(previous.image);
        }
        self.current = Some(current);
    }

    /// Caches an entry, freeing any replaced preview off the UI thread.
    fn cache_image(&mut self, location: ItemLocation, entry: CacheEntry) {
        if let Some(replaced) = self.cache.insert(location, entry) {
            self.releaser.release(replaced.image);
        }
    }

    /// The just-attempted plain-file open failed synchronously as not-found; scans report later.
    pub fn open_failed_missing(&self, path: &Path) -> bool {
        const ERROR_FILE_NOT_FOUND: i32 = 2;
        const ERROR_PATH_NOT_FOUND: i32 = 3;
        let Some((location, error)) = self.request.failure() else {
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
        matches!(self.request.pending(), Some(ItemLocation::Url(_)))
    }

    fn has_pending_display(&self) -> bool {
        self.request.pending().is_some()
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

    /// The position baseline: the running load, else the errored item, else the display.
    pub fn navigation_anchor(&self) -> Option<&ItemLocation> {
        self.request
            .location()
            .or_else(|| self.current.as_ref().map(|current| &current.location))
    }

    /// The anchor's index in the listing; a listing that dropped it answers with its place.
    fn anchor_index(&self) -> AnchorIndex {
        if let Some(index) = self
            .navigation_anchor()
            .and_then(|location| self.position_of(location))
        {
            return AnchorIndex::Listed(index);
        }
        match &self.missing_anchor {
            Some(MissingAnchor::Front) => AnchorIndex::Missing(0),
            Some(MissingAnchor::After(location)) => match self.position_of(location) {
                Some(index) => AnchorIndex::Missing(index + 1),
                None => AnchorIndex::Unlisted,
            },
            None => AnchorIndex::Unlisted,
        }
    }

    pub fn navigation_target(&self, command: NavigationCommand) -> Option<ItemLocation> {
        if self.entries.is_empty() {
            return None;
        }
        let anchor = self.anchor_index();
        let length = self.entries.len();
        let looped = self.options.loop_within_folder;
        let index = match command {
            NavigationCommand::First => 0,
            NavigationCommand::Last => length - 1,
            NavigationCommand::Next => step_index(anchor, 1, length, looped)?,
            NavigationCommand::Previous => step_index(anchor, -1, length, looped)?,
        };
        Some(self.entries[index].location.clone())
    }

    /// None when the arrival changes nothing on screen: a stale item, or a redo already queued.
    pub fn on_decode_complete(&mut self, completion: DecodeCompletion) -> Option<LoadOutcome> {
        let is_pending = self.is_pending(&completion.location);
        if matches!(completion.stage, DecodeStage::Preview) {
            if is_pending && let Ok(image) = completion.result {
                self.show_image(CurrentImage {
                    location: completion.location,
                    image,
                    texture: completion.texture,
                    metadata: completion.metadata,
                });
                return Some(LoadOutcome::Shown);
            }
            return None;
        }
        self.submitted_decodes.remove(&completion.location);
        // A retired generation never wraps and carries no pixels; redo like a cancelled decode.
        if completion
            .texture
            .as_ref()
            .is_some_and(|uploaded| Some(uploaded.generation) != self.upload_device_generation)
        {
            if let Ok(image) = &completion.result {
                self.record_arrived_weight(&completion.location, image);
            }
            if is_pending {
                self.resubmit_pending_decode(completion.location, completion.metadata);
            }
            return None;
        }
        // Preview stages always post Ok; an Err would fall through to Final's failure paths.
        if matches!(completion.stage, DecodeStage::PreviewFinal)
            && let Ok(image) = &completion.result
        {
            self.cache_image(
                completion.location.clone(),
                CacheEntry::new(
                    completion.metadata,
                    true,
                    image.clone(),
                    completion.texture.clone(),
                ),
            );
            if is_pending {
                // Waited on: show it, then submit the full decode it precedes.
                return Some(self.load_item(&completion.location));
            }
            self.evict_cache();
            return None;
        }
        if let Err(error) = &completion.result
            && error.is_cancelled()
        {
            // Navigation can return to an item while its decode is cancelling.
            if is_pending {
                self.resubmit_pending_decode(completion.location, completion.metadata);
            }
            return None;
        }
        match completion.result {
            Ok(image) => {
                self.record_arrived_weight(&completion.location, &image);
                self.cache_image(
                    completion.location.clone(),
                    CacheEntry::new(
                        completion.metadata,
                        false,
                        image.clone(),
                        completion.texture.clone(),
                    ),
                );
                if is_pending {
                    self.show_image(CurrentImage {
                        location: completion.location,
                        image,
                        texture: completion.texture,
                        metadata: completion.metadata,
                    });
                    self.request = ViewRequest::Idle;
                    // Preload starts once this image is on screen.
                    Some(LoadOutcome::Shown)
                } else {
                    self.evict_cache();
                    None
                }
            }
            Err(error) => {
                if is_pending {
                    self.request = ViewRequest::Failed(completion.location, error);
                    self.refresh_preload();
                    Some(LoadOutcome::Failed)
                } else {
                    None
                }
            }
        }
    }

    fn rescan_folder(&mut self, directory: &Path) {
        let scope = ListingScope::Directory(directory.to_path_buf());
        let scan = ScannedListing::of(scope, &self.options);
        let mut entries = scan.result.unwrap_or_default();
        self.carry_weights_into(&mut entries);
        self.entries = entries;
        self.listing_scope = Some(scan.scope);
    }

    /// Drops a deleted item from the listing snapshot; no rescan.
    fn remove_listing_entry(&mut self, location: &ItemLocation) {
        if let Some(index) = self.position_of(location) {
            self.entries.remove(index);
        }
    }

    /// Drops a deleted item and answers what to show next, resolved while it is still listed.
    pub fn remove_deleted_item(
        &mut self,
        location: &ItemLocation,
        preferred: NavigationCommand,
    ) -> Option<PathBuf> {
        let opposite = match preferred {
            NavigationCommand::Previous => NavigationCommand::Next,
            _ => NavigationCommand::Previous,
        };
        let successor = [preferred, opposite]
            .into_iter()
            .find_map(|direction| {
                self.navigation_target(direction)
                    .filter(|candidate| candidate != location)
            })
            .and_then(|candidate| candidate.as_file().map(Path::to_path_buf));
        self.remove_listing_entry(location);
        if successor.is_none() {
            self.clear_current_item();
        }
        successor
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

    /// Speculation waits for the display; the cancel and evict sweeps do not.
    pub fn refresh_preload(&mut self) {
        // A pending upgrade of what is already on screen (RAW, animation) still speculates.
        let awaiting_first_view = self.request.pending().is_some_and(|pending| {
            self.current
                .as_ref()
                .is_none_or(|current| current.location != *pending)
        });
        let (backward, forward, budget) = self.preload_plan();
        // Candidates in priority order; a missing anchor still names adjacent entries.
        let length = self.entries.len();
        let anchor = self.anchor_index();
        let candidates: Vec<usize> = preload_offsets(backward, forward)
            .filter_map(|offset| {
                index_at_offset(anchor, offset, length, self.options.loop_within_folder)
            })
            .collect();
        let mut targets: HashSet<ItemLocation> = candidates
            .iter()
            .map(|&index| self.entries[index].location.clone())
            .collect();
        targets.extend(self.navigation_anchor().cloned());
        if backward == 0 && forward == 0 {
            for (_, entry) in self.cache.drain() {
                self.releaser.release(entry.image);
            }
        } else {
            self.drop_entries_outside(&targets);
            if !awaiting_first_view {
                self.submit_preload_decodes(&candidates, budget);
            }
        }
        self.cancel_decodes_outside(&targets);
        self.evict_cache();
    }

    /// Queues candidates while their weights fit the budget; unknown weights probe first.
    fn submit_preload_decodes(&mut self, candidates: &[usize], budget: u64) {
        let mut awaiting_probes = false;
        for &index in candidates {
            let entry = &self.entries[index];
            if entry.weight != DecodedWeight::Unknown {
                continue;
            }
            awaiting_probes = true;
            if !self.submitted_probes.contains_key(&entry.location) {
                let location = entry.location.clone();
                self.submit_probe(location);
            }
        }
        if awaiting_probes {
            return; // one pass per settled target set, re-run by probe completions
        }
        let submittable: Vec<(usize, u64)> = candidates
            .iter()
            .filter_map(|&index| {
                let entry = &self.entries[index];
                match entry.weight {
                    DecodedWeight::Known(weight)
                        if !self.submitted_decodes.contains_key(&entry.location)
                            && !self
                                .cache
                                .get(&entry.location)
                                .is_some_and(|cached| cached.matches(entry.metadata())) =>
                    {
                        Some((index, weight))
                    }
                    _ => None,
                }
            })
            .collect();
        let weights: Vec<u64> = submittable.iter().map(|&(_, weight)| weight).collect();
        let selected = fits_in_budget(self.occupied_bytes(), budget, &weights);
        for (&(index, _), selected) in submittable.iter().zip(selected) {
            if !selected {
                continue;
            }
            let entry = &self.entries[index];
            // Cheap speculation: two-stage targets get the preview; animations stop at frame one.
            let kind = if is_deferred_two_stage(&entry.location) {
                JobKind::Preview
            } else {
                JobKind::Full
            };
            let (location, metadata) = (entry.location.clone(), entry.metadata());
            self.submit_decode(location, metadata, kind, false);
        }
    }

    /// Registers the probe as submitted; it reads only the header.
    fn submit_probe(&mut self, location: ItemLocation) {
        let cancellation = Arc::new(AtomicBool::new(false));
        self.submitted_probes
            .insert(location.clone(), cancellation.clone());
        self.pool.submit(
            location,
            ItemMetadata::default(),
            cancellation,
            JobKind::Probe,
            false,
        );
    }

    /// Records an arrived probe; a stale location misses the listing lookup.
    pub fn on_probe_complete(&mut self, completion: ProbeCompletion) {
        self.submitted_probes.remove(&completion.location);
        if !completion.cancelled
            && let Some(index) = self.position_of(&completion.location)
        {
            self.entries[index].weight = match completion.weight {
                Some(weight) => DecodedWeight::Known(weight),
                None => DecodedWeight::Unavailable,
            };
        }
        // The budget pass waits for the whole target set; sweep once it settles.
        if self.submitted_probes.is_empty() {
            self.refresh_preload();
        }
    }

    /// SVG rasters at the largest monitor's size, so its weights expire with it.
    pub fn invalidate_svg_weights(&mut self) {
        decode::invalidate_monitor_size();
        for entry in &mut self.entries {
            // An unprobed entry has no weight to clear, and the name lookup is not free.
            if entry.weight == DecodedWeight::Unknown {
                continue;
            }
            let display_sized = entry
                .location
                .extension_lowercase()
                .is_some_and(|extension| decode::weight_depends_on_display(&extension));
            if display_sized {
                entry.weight = DecodedWeight::Unknown;
            }
        }
    }

    /// Bytes the budget already covers: cached items once, plus unarrived submissions.
    fn occupied_bytes(&self) -> u64 {
        let cached: u64 = self
            .cache
            .iter()
            .map(|(location, entry)| self.cached_weight(location, entry))
            .sum();
        let submitted: u64 = self
            .submitted_decodes
            .keys()
            .filter(|location| !self.cache.contains_key(location))
            .map(|location| self.listed_weight(location).unwrap_or(0))
            .sum();
        cached + submitted
    }

    /// A preview weighs its full decode; others weigh what they hold, pixels or texture.
    fn cached_weight(&self, location: &ItemLocation, entry: &CacheEntry) -> u64 {
        if entry.preview
            && let Some(weight) = self.listed_weight(location)
        {
            return weight;
        }
        let pixels = entry.image.pixel_bytes() as u64;
        if pixels > 0 {
            pixels
        } else if entry.texture.is_some() {
            entry.image.frame_byte_length() as u64
        } else {
            0
        }
    }

    fn listed_weight(&self, location: &ItemLocation) -> Option<u64> {
        let index = self.position_of(location)?;
        match self.entries[index].weight {
            DecodedWeight::Known(weight) => Some(weight),
            _ => None,
        }
    }

    /// The actual bytes replace the prediction; eviction then trims any excess.
    fn record_arrived_weight(&mut self, location: &ItemLocation, image: &DecodedImage) {
        // A slimmed arrival keeps its geometry; the frame length is the texture's size.
        let bytes = match image.pixel_bytes() {
            0 => image.frame_byte_length(),
            bytes => bytes,
        };
        if let Some(index) = self.position_of(location) {
            self.entries[index].weight = DecodedWeight::Known(bytes as u64);
        }
    }

    /// Cancels queued or running decodes and probes outside the preload targets.
    fn cancel_decodes_outside(&mut self, targets: &HashSet<ItemLocation>) {
        for location in self.pool.remove_queued_except(targets) {
            self.submitted_decodes.remove(&location);
            self.submitted_probes.remove(&location);
        }
        for (location, cancellation) in self.submitted_decodes.iter().chain(&self.submitted_probes)
        {
            if !targets.contains(location) {
                cancellation.store(true, Ordering::Relaxed);
            }
        }
    }

    /// Leaving the preload targets removes the entry whole; returning is a fresh preload.
    fn drop_entries_outside(&mut self, targets: &HashSet<ItemLocation>) {
        let cache = &mut self.cache;
        let releaser = &self.releaser;
        for (_, entry) in cache.extract_if(|location, _| !targets.contains(location)) {
            releaser.release(entry.image);
        }
    }

    /// Evicts entries in reverse preload priority until within budget.
    fn evict_cache(&mut self) {
        let (backward, forward, budget) = self.preload_plan();
        // Sum first: the usual answer is "within budget", and ranking clones and walks per key.
        let mut total: u64 = self
            .cache
            .iter()
            .map(|(location, entry)| self.cached_weight(location, entry))
            .sum();
        if total <= budget {
            return;
        }
        let weighted: Vec<(ItemLocation, u64)> = self
            .cache
            .iter()
            .map(|(location, entry)| (location.clone(), self.cached_weight(location, entry)))
            .collect();
        let anchor = self.navigation_anchor().cloned();
        let priorities = preload_priorities(
            self.anchor_index(),
            backward,
            forward,
            self.entries.len(),
            self.options.loop_within_folder,
        );
        let mut ranked: Vec<(ItemLocation, u64, usize)> = weighted
            .into_iter()
            .map(|(location, weight)| {
                // The anchor goes last even when unlisted; what left the preload targets goes first.
                let key = if anchor.as_ref() == Some(&location) {
                    0
                } else {
                    self.position_of(&location)
                        .and_then(|index| priorities.get(&index).copied())
                        .unwrap_or(usize::MAX)
                };
                (location, weight, key)
            })
            .collect();
        ranked.sort_by_key(|(_, _, key)| std::cmp::Reverse(*key));
        for (location, cost, _) in ranked {
            if total <= budget {
                break;
            }
            if let Some(entry) = self.cache.remove(&location) {
                self.releaser.release(entry.image);
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

/// Where a step or an offset counts from; None for an anchor the listing never held.
fn anchor_start(anchor: AnchorIndex, forward: bool) -> Option<isize> {
    match anchor {
        AnchorIndex::Listed(index) => Some(index as isize),
        // A missing anchor sits between two entries, so both directions land next to it.
        AnchorIndex::Missing(index) => Some(index as isize - isize::from(forward)),
        AnchorIndex::Unlisted => None,
    }
}

fn index_at_offset(
    anchor: AnchorIndex,
    offset: isize,
    length: usize,
    loop_enabled: bool,
) -> Option<usize> {
    if length == 0 {
        return None;
    }
    let index = anchor_start(anchor, offset > 0)? + offset;
    if loop_enabled {
        let wrapped = index.rem_euclid(length as isize) as usize;
        (anchor != AnchorIndex::Listed(wrapped)).then_some(wrapped)
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

/// Marks candidate weights that fit, in order; a misfit is skipped, not a stop.
fn fits_in_budget(occupied: u64, budget: u64, weights: &[u64]) -> Vec<bool> {
    let mut used = occupied;
    weights
        .iter()
        .map(|&weight| {
            let fits = used.saturating_add(weight) <= budget;
            if fits {
                used += weight;
            }
            fits
        })
        .collect()
}

/// A preview-first local file whose full decode waits for the navigation lull.
fn is_deferred_two_stage(location: &ItemLocation) -> bool {
    matches!(location, ItemLocation::File(path) if decode::is_two_stage_preview(path))
}

/// One step from the anchor index; None past a non-looping end.
fn step_index(anchor: AnchorIndex, direction: isize, length: usize, looped: bool) -> Option<usize> {
    if length == 0 {
        return None;
    }
    let length = length as isize;
    // An anchor the listing never held enters from the end the step comes from.
    let start =
        anchor_start(anchor, direction > 0).unwrap_or(if direction > 0 { -1 } else { length });
    let index = start + direction;
    if looped {
        Some(index.rem_euclid(length) as usize)
    } else {
        (0..length).contains(&index).then_some(index as usize)
    }
}

/// Entry index -> preload priority (anchor 0, then submission order); shared with eviction.
fn preload_priorities(
    anchor: AnchorIndex,
    backward: usize,
    forward: usize,
    length: usize,
    loop_enabled: bool,
) -> HashMap<usize, usize> {
    let mut priorities = match anchor {
        AnchorIndex::Listed(index) => HashMap::from([(index, 0)]),
        AnchorIndex::Missing(_) | AnchorIndex::Unlisted => HashMap::new(),
    };
    for (rank, offset) in preload_offsets(backward, forward).enumerate() {
        if let Some(index) = index_at_offset(anchor, offset, length, loop_enabled) {
            priorities.entry(index).or_insert(rank + 1);
        }
    }
    priorities
}

/// ASCII case-insensitive path equality over the stored bytes; a lossy conversion folds distinct names together.
fn paths_equal(a: &Path, b: &Path) -> bool {
    a.as_os_str()
        .as_encoded_bytes()
        .eq_ignore_ascii_case(b.as_os_str().as_encoded_bytes())
}

/// Folds the same bytes paths_equal compares, so Eq and Hash stay in step.
fn hash_path_identity<H: Hasher>(path: &Path, state: &mut H) {
    for byte in path.as_os_str().as_encoded_bytes() {
        state.write_u8(byte.to_ascii_lowercase());
    }
    // A terminator, so adjacent hashed fields cannot blur together.
    state.write_u8(0xFF);
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
        let path = entry.path();
        let format_name = crate::text::lowercase_extension(Path::new(&file_name))
            .and_then(|extension| decode::format_name_for_extension(&extension));
        let included = format_name.is_some()
            || (options.detect_format_by_content
                && decode::descriptor_for_content(&path).is_some());
        if !included {
            continue;
        }
        let wide_name = HSTRING::from(&file_name);
        entries.push(ListingEntry {
            location: ItemLocation::File(path),
            wide_name,
            file_size: metadata.len(),
            modified: metadata.modified().unwrap_or(UNIX_EPOCH),
            created: metadata.created().unwrap_or(UNIX_EPOCH),
            format_name: format_name.unwrap_or(""),
            weight: DecodedWeight::Unknown,
        });
    }
    entries
}

/// Entry for an image member; other member types drop out of the listing.
fn member_entry(archive: &Path, member: archive_reader::ArchiveMember) -> Option<ListingEntry> {
    let format_name = crate::text::lowercase_extension(Path::new(&member.name))
        .and_then(|extension| decode::format_name_for_extension(&extension))?;
    let wide_name = HSTRING::from(&member.name);
    Some(ListingEntry {
        location: ItemLocation::ArchiveMember {
            archive: archive.to_path_buf(),
            member: member.name,
        },
        wide_name,
        file_size: member.uncompressed_bytes,
        modified: member.modified,
        created: member.modified, // archives do not record creation times
        format_name,
        weight: DecodedWeight::Unknown,
    })
}

fn sort_entries(entries: &mut [ListingEntry], options: &CoreOptions) {
    match options.sort_mode {
        SortMode::Name => entries.sort_by(compare_natural_names),
        SortMode::Modified => {
            entries.sort_by(|a, b| {
                b.modified
                    .cmp(&a.modified)
                    .then_with(|| compare_natural_names(a, b))
            });
        }
        SortMode::Created => {
            entries.sort_by(|a, b| {
                b.created
                    .cmp(&a.created)
                    .then_with(|| compare_natural_names(a, b))
            });
        }
        SortMode::Size => {
            entries.sort_by(|a, b| {
                b.file_size
                    .cmp(&a.file_size)
                    .then_with(|| compare_natural_names(a, b))
            });
        }
        SortMode::Type => entries.sort_by(|a, b| {
            a.format_name
                .cmp(b.format_name)
                .then_with(|| compare_natural_names(a, b))
        }),
    }
    if options.sort_descending {
        entries.reverse();
    }
}

fn compare_natural_names(a: &ListingEntry, b: &ListingEntry) -> std::cmp::Ordering {
    crate::text::natural_order(&a.wide_name, &b.wide_name)
}

/// Fixed once a worker takes the job; Preview, Probe and the speculative flag keep preload cheap.
#[derive(Clone, Copy, PartialEq, Eq)]
enum JobKind {
    Full,
    /// Stops at a RAW embedded or sub-resolution preview when one exists; decodes fully otherwise.
    Preview,
    /// Reads only the header and posts the decode weight; no pixels.
    Probe,
}

struct DecodeJob {
    location: ItemLocation,
    /// Carried from the submit stat or the listing; the worker echoes it, never stats.
    metadata: ItemMetadata,
    cancellation: Arc<AtomicBool>,
    kind: JobKind,
    /// Preloaded on a guess, so an animation stops at its first frame.
    speculative: bool,
}

struct PoolShared {
    queue: Mutex<VecDeque<DecodeJob>>,
    upload_device: Mutex<Option<UploadDevice>>,
    available: Condvar,
}

struct DecodePool {
    shared: Arc<PoolShared>,
}

impl DecodePool {
    fn new(window: isize) -> Self {
        let shared = Arc::new(PoolShared {
            queue: Mutex::new(VecDeque::new()),
            upload_device: Mutex::new(None),
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

    fn set_upload_device(&self, upload_device: Option<UploadDevice>) {
        if let Ok(mut slot) = self.shared.upload_device.lock() {
            *slot = upload_device;
        }
    }

    fn submit(
        &self,
        location: ItemLocation,
        metadata: ItemMetadata,
        cancellation: Arc<AtomicBool>,
        kind: JobKind,
        awaited: bool,
    ) {
        let mut queue = self.shared.queue.lock().expect("decode queue poisoned");
        let job = DecodeJob {
            location,
            metadata,
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
        if let Some(position) = queue
            .iter()
            .position(|job| job.kind != JobKind::Probe && job.location == *location)
            && let Some(mut job) = queue.remove(position)
        {
            job.speculative = false; // the pending item waits on it now
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
                store_codec_names: &[],
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
                store_codec_names: &[],
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
        if job.kind == JobKind::Probe {
            run_probe_job(&job, window);
            continue;
        }
        let mut metadata = job.metadata;
        let result = match &job.location {
            ItemLocation::File(path) => {
                let post_preview = |image: DecodedImage, last: bool| {
                    post_boxed(
                        window,
                        WM_APP_DECODE_COMPLETE,
                        Box::new(DecodeCompletion {
                            location: job.location.clone(),
                            metadata: job.metadata,
                            stage: if last {
                                DecodeStage::PreviewFinal
                            } else {
                                DecodeStage::Preview
                            },
                            result: Ok(Arc::new(image)),
                            texture: None,
                        }),
                    );
                };
                if job.kind != JobKind::Full
                    && let Some(preview) = decode::decode_two_stage_preview(path, &job.cancellation)
                {
                    post_preview(preview, true);
                    continue; // the full decode is submitted separately
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
                    Ok(member_bytes) => {
                        let extension = job.location.extension_lowercase();
                        decode::decode_bytes(&member_bytes, extension.as_deref(), &job.cancellation)
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
                    Ok(bytes) => {
                        metadata.file_size = bytes.len() as u64; // the remote size becomes known here
                        let extension = curl::extension_lowercase(url);
                        decode::decode_bytes(&bytes, extension.as_deref(), &job.cancellation)
                            .map_err(url_decode_error)
                    }
                    Err(error) => Err(error.into()),
                }
            }
        }
        .map(Arc::new);
        let texture = result.as_ref().ok().and_then(|image| {
            let upload_device = shared.upload_device.lock().ok()?.clone()?;
            upload_still_texture(&upload_device, image)
        });
        // The texture is the only copy: the decode buffer frees here at the source.
        let result = if texture.is_some() {
            result.map(|image| Arc::new(image.without_pixels()))
        } else {
            result
        };
        post_boxed(
            window,
            WM_APP_DECODE_COMPLETE,
            Box::new(DecodeCompletion {
                location: job.location,
                metadata,
                stage: DecodeStage::Final,
                result,
                texture,
            }),
        );
    }
}

/// Header-only weight probe on a worker; the result lands on the listing entry.
fn run_probe_job(job: &DecodeJob, window: isize) {
    let (cancelled, weight) = if job.cancellation.load(Ordering::Relaxed) {
        (true, None)
    } else {
        match &job.location {
            ItemLocation::File(path) => (false, decode::probe_file_weight(path)),
            ItemLocation::ArchiveMember { archive, member } => {
                // The member extraction repeats at decode time; only the pixel work is saved.
                match archive_reader::read_member(archive, member, &job.cancellation) {
                    Ok(member_bytes) => {
                        let extension = job.location.extension_lowercase();
                        (
                            false,
                            decode::probe_bytes_weight(&member_bytes, extension.as_deref()),
                        )
                    }
                    Err(error) => (error.cancelled, None),
                }
            }
            ItemLocation::Url(_) => (false, None), // URLs are never listed, never probed
        }
    };
    post_boxed(
        window,
        WM_APP_PROBE_COMPLETE,
        Box::new(ProbeCompletion {
            location: job.location.clone(),
            cancelled,
            weight,
        }),
    );
}

/// Unrecognized downloaded bytes (an HTML page, most often) get a plain message.
fn url_decode_error(error: DecodeError) -> DecodeError {
    if error.is_unrecognized_format() {
        return DecodeError {
            message: "No image at this URL".to_string(),
            ..error
        };
    }
    error
}

/// A folder of numbered files, optionally anchored on one of them.
#[cfg(test)]
fn core_with_files(count: usize, anchor: Option<usize>) -> ImageCore {
    let mut core = core();
    core.entries = (0..count)
        .map(|index| ListingEntry {
            location: ItemLocation::File(PathBuf::from(format!("C:\\pictures\\{index:03}.png"))),
            wide_name: HSTRING::new(),
            file_size: 0,
            modified: UNIX_EPOCH,
            created: UNIX_EPOCH,
            format_name: "PNG",
            weight: DecodedWeight::Unknown,
        })
        .collect();
    core.request = match anchor {
        Some(index) => ViewRequest::Pending(core.entries[index].location.clone()),
        None => ViewRequest::Idle,
    };
    core
}

/// The listing options every core test starts from.
#[cfg(test)]
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

/// A temp directory holding `files`; the bytes are never decoded, only listed.
#[cfg(test)]
fn fixture_directory(name: &str, files: &[&str]) -> PathBuf {
    let directory = std::env::temp_dir().join(name);
    // The cleanup at the end of a test is best effort, so start from an empty directory.
    let _ = std::fs::remove_dir_all(&directory);
    std::fs::create_dir_all(&directory).expect("fixture directory");
    for file in files {
        std::fs::write(directory.join(file), b"listing only; never decoded").expect("fixture file");
    }
    directory
}

#[cfg(test)]
mod step_index_tests {
    use super::*;

    #[test]
    fn an_unlisted_anchor_starts_at_the_matching_end() {
        assert_eq!(step_index(AnchorIndex::Unlisted, 1, 3, false), Some(0));
        assert_eq!(step_index(AnchorIndex::Unlisted, -1, 3, false), Some(2));
    }

    #[test]
    fn steps_move_one_entry_and_ends_obey_the_loop_option() {
        assert_eq!(step_index(AnchorIndex::Listed(1), 1, 4, false), Some(2));
        assert_eq!(step_index(AnchorIndex::Listed(3), 1, 4, false), None);
        assert_eq!(step_index(AnchorIndex::Listed(3), 1, 4, true), Some(0));
        assert_eq!(step_index(AnchorIndex::Listed(0), -1, 4, false), None);
        assert_eq!(step_index(AnchorIndex::Listed(0), -1, 4, true), Some(3));
    }

    #[test]
    fn a_missing_anchor_lands_on_both_adjacent_entries() {
        assert_eq!(step_index(AnchorIndex::Missing(2), 1, 4, false), Some(2));
        assert_eq!(step_index(AnchorIndex::Missing(2), -1, 4, false), Some(1));
        // The place after the last entry: forward runs out, backward keeps the last one.
        assert_eq!(step_index(AnchorIndex::Missing(4), 1, 4, false), None);
        assert_eq!(step_index(AnchorIndex::Missing(4), 1, 4, true), Some(0));
        assert_eq!(step_index(AnchorIndex::Missing(4), -1, 4, false), Some(3));
        // The place before the first entry mirrors it.
        assert_eq!(step_index(AnchorIndex::Missing(0), -1, 4, false), None);
        assert_eq!(step_index(AnchorIndex::Missing(0), -1, 4, true), Some(3));
        assert_eq!(step_index(AnchorIndex::Missing(0), 1, 4, false), Some(0));
    }

    #[test]
    fn degenerate_lengths_stay_in_bounds() {
        assert_eq!(step_index(AnchorIndex::Unlisted, 1, 0, true), None);
        assert_eq!(step_index(AnchorIndex::Missing(0), 1, 0, true), None);
        assert_eq!(step_index(AnchorIndex::Unlisted, 1, 1, false), Some(0));
        assert_eq!(step_index(AnchorIndex::Listed(0), 1, 1, true), Some(0));
        assert_eq!(step_index(AnchorIndex::Listed(0), 1, 1, false), None);
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
    fn a_missing_anchor_preloads_the_entries_it_sat_between() {
        let (backward, forward, _) = PRELOAD_SPECIFICATIONS[1];
        let anchor = AnchorIndex::Missing(10);
        let targets: Vec<usize> = preload_offsets(backward, forward)
            .filter_map(|offset| index_at_offset(anchor, offset, 100, false))
            .collect();
        // Forward lands on the entry that took the place, backward on the one before it.
        assert_eq!(targets, [10, 11, 12, 9]);
    }

    #[test]
    fn an_unlisted_anchor_preloads_nothing() {
        let (backward, forward, _) = PRELOAD_SPECIFICATIONS[1];
        assert_eq!(
            index_at_offset(AnchorIndex::Unlisted, 1, 100, false),
            None,
            "an anchor outside the listing has no adjacent entries to speculate on"
        );
        let priorities = preload_priorities(AnchorIndex::Unlisted, backward, forward, 100, false);
        assert!(priorities.is_empty());
    }

    #[test]
    fn eviction_prefers_forward_over_backward_within_the_preload_targets() {
        let (backward, forward, _) = PRELOAD_SPECIFICATIONS[1];
        let priorities = preload_priorities(AnchorIndex::Listed(10), backward, forward, 100, false);
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
        let priorities = preload_priorities(AnchorIndex::Listed(10), backward, forward, 100, false);
        // Outside the map, so the eviction key is usize::MAX: they go first.
        assert!(!priorities.contains_key(&8));
        assert!(!priorities.contains_key(&14));
    }

    #[test]
    fn eviction_keeps_wrapped_preload_targets() {
        let (backward, forward, _) = PRELOAD_SPECIFICATIONS[1];
        // Five looping entries: +3 lands at ring offset -2 yet stays a target.
        let priorities = preload_priorities(AnchorIndex::Listed(0), backward, forward, 5, true);
        assert_eq!(priorities[&3], 3);
        assert_eq!(priorities[&4], 4);
        assert_eq!(priorities.len(), 5);
        // Three looping entries: +2 claims the slot before -1 revisits it.
        let priorities = preload_priorities(AnchorIndex::Listed(0), backward, forward, 3, true);
        assert_eq!(priorities[&2], 2);
        assert_eq!(priorities.len(), 3);
    }
}

#[cfg(test)]
mod budget_selection_tests {
    use super::*;

    #[test]
    fn candidates_land_in_priority_order_while_they_fit() {
        assert_eq!(
            fits_in_budget(0, 1000, &[400, 400, 400]),
            [true, true, false]
        );
    }

    #[test]
    fn an_oversized_candidate_is_skipped_not_a_stopping_point() {
        // The big +1 stays out, the cheap items behind it still land.
        assert_eq!(
            fits_in_budget(0, 500, &[800, 200, 200, 200]),
            [false, true, true, false]
        );
    }

    #[test]
    fn occupied_bytes_shrink_the_remaining_budget() {
        assert_eq!(fits_in_budget(722, 1024, &[173, 173]), [true, false]);
    }

    #[test]
    fn a_full_budget_admits_nothing_but_zero_weights() {
        assert_eq!(fits_in_budget(1024, 1024, &[1, 0]), [false, true]);
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
            "C:\\a.cbz › art/01.png"
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

    /// Names Windows allows but Unicode does not; the file system tells them apart, so riv does too.
    #[test]
    fn names_with_a_lone_surrogate_stay_apart() {
        use std::ffi::OsString;
        use std::os::windows::ffi::OsStringExt;

        let name = |code_unit: u16| {
            ItemLocation::File(PathBuf::from(OsString::from_wide(&[
                0x0061, code_unit, 0x002E, 0x0070, 0x006E, 0x0067,
            ])))
        };
        assert!(name(0xD800) != name(0xD801));
        let mut cache = HashMap::new();
        cache.insert(name(0xD800), "decoded");
        assert_eq!(cache.get(&name(0xD801)), None);
        assert_eq!(cache.get(&name(0xD800)), Some(&"decoded"));
    }

    #[test]
    fn member_extension_resolves_format_names() {
        let entry = |name: &str| {
            member_entry(
                Path::new("C:\\a.cbz"),
                archive_reader::ArchiveMember {
                    name: name.to_string(),
                    uncompressed_bytes: 0,
                    modified: UNIX_EPOCH,
                },
            )
        };
        assert_eq!(
            entry("art/01.png").map(|entry| entry.format_name),
            Some("PNG")
        );
        assert!(entry("readme.txt").is_none());
    }

    #[test]
    fn url_locations_stand_alone() {
        let url = |text: &str| ItemLocation::Url(text.to_string());
        let location = url("https://a.com/b/c.png?width=1");
        assert_eq!(location.display_name(), "c.png");
        assert_eq!(location.display_text(), "https://a.com/b/c.png?width=1");
        assert_eq!(location.containing_file(), None);
        assert_eq!(location.as_file(), None);
        assert_eq!(location.extension_lowercase().as_deref(), Some("png"));
        // URLs compare exactly; remote paths are case-sensitive.
        assert!(location == url("https://a.com/b/c.png?width=1"));
        assert!(location != url("https://a.com/b/C.png?width=1"));
        assert!(location != ItemLocation::File(PathBuf::from("https://a.com/b/c.png?width=1")));
        let mut cache = HashMap::new();
        cache.insert(location, "decoded");
        assert_eq!(
            cache.get(&url("https://a.com/b/c.png?width=1")),
            Some(&"decoded")
        );
    }

    #[test]
    fn member_entries_keep_images_only() {
        let archive_member = |name: &str| archive_reader::ArchiveMember {
            name: name.to_string(),
            uncompressed_bytes: 10,
            modified: UNIX_EPOCH,
        };
        let archive = Path::new("C:\\a.cbz");
        assert!(member_entry(archive, archive_member("art/01.png")).is_some());
        assert!(member_entry(archive, archive_member("info.txt")).is_none());
        assert!(member_entry(archive, archive_member("no_extension")).is_none());
        let entry = member_entry(archive, archive_member("art/01.png")).expect("image member");
        assert_eq!(entry.created, entry.modified); // archives have no creation time
        assert_eq!(entry.file_size, 10);
    }
}

/// A URL attempt owns the session alone; a local open rebuilds its listing on scan arrival.
#[cfg(test)]
mod url_session_state_tests {
    use super::*;

    fn folder_state(core: &mut ImageCore, path: &str) {
        let path = PathBuf::from(path);
        core.listing_scope = Some(ListingScope::Directory(
            path.parent().expect("parent").to_path_buf(),
        ));
        core.entries = vec![ListingEntry {
            location: ItemLocation::File(path),
            wide_name: HSTRING::new(),
            file_size: 0,
            modified: UNIX_EPOCH,
            created: UNIX_EPOCH,
            format_name: "PNG",
            weight: DecodedWeight::Unknown,
        }];
    }

    fn decode_error(message: &str) -> DecodeError {
        DecodeError {
            code: 0,
            message: message.to_string(),
            store_codec_names: &[],
        }
    }

    #[test]
    fn navigation_targets_need_more_than_the_anchor_itself() {
        let mut core = core();
        assert!(!core.has_navigation_targets());

        // A single entry that is the anchor itself leaves nowhere to go.
        folder_state(&mut core, "C:\\pictures\\a.png");
        core.request = ViewRequest::Failed(
            ItemLocation::File(PathBuf::from("C:\\pictures\\a.png")),
            decode_error("broken"),
        );
        assert!(!core.has_navigation_targets());

        // An unlisted anchor can still reach the one listed entry.
        core.request = ViewRequest::Failed(
            ItemLocation::File(PathBuf::from("C:\\pictures\\note.txt")),
            decode_error("no decoder"),
        );
        assert!(core.has_navigation_targets());

        core.entries.push(ListingEntry {
            location: ItemLocation::File(PathBuf::from("C:\\pictures\\b.png")),
            wide_name: HSTRING::new(),
            file_size: 0,
            modified: UNIX_EPOCH,
            created: UNIX_EPOCH,
            format_name: "PNG",
            weight: DecodedWeight::Unknown,
        });
        core.request = ViewRequest::Failed(
            ItemLocation::File(PathBuf::from("C:\\pictures\\a.png")),
            decode_error("broken"),
        );
        assert!(core.has_navigation_targets());
    }

    #[test]
    fn a_rejected_url_still_clears_the_listing() {
        let mut core = core();
        folder_state(&mut core, "C:\\pictures\\a.png");
        assert_eq!(core.load_url("ftp://a.com/b.png"), LoadOutcome::Failed);
        assert!(core.entries.is_empty());
        assert!(core.listing_scope.is_none());
        let (location, error) = core.load_failure().expect("error recorded");
        assert!(matches!(location, ItemLocation::Url(_)));
        assert!(error.message.contains("protocol"));
    }

    #[test]
    fn an_empty_paste_reports_no_url() {
        let mut core = core();
        folder_state(&mut core, "C:\\pictures\\a.png");
        assert_eq!(core.load_url(""), LoadOutcome::Failed);
        assert!(core.entries.is_empty());
        assert!(core.listing_scope.is_none());
        let (_, error) = core.load_failure().expect("error recorded");
        assert!(error.message.contains("clipboard"));
    }

    #[test]
    fn prose_around_a_url_is_rejected_not_parsed() {
        for text in ["see https://a/b.png look", "seehttps://a/b.pnglook"] {
            let mut core = core();
            assert_eq!(core.load_url(text), LoadOutcome::Failed);
            let (_, error) = core.load_failure().expect("error recorded");
            assert!(error.message.contains("protocol"));
        }
    }

    #[test]
    fn reload_retries_the_errored_url_not_the_previous_file() {
        let mut core = core();
        folder_state(&mut core, "C:\\pictures\\a.png");
        core.request = ViewRequest::Failed(
            ItemLocation::Url("ftp://a.com/b.png".to_string()),
            DecodeError {
                code: 0,
                message: "Download failed".to_string(),
                store_codec_names: &[],
            },
        );
        assert_eq!(core.reload_current(), Some(LoadOutcome::Failed));
        // Routed back through load_url: single-item state, validation re-ran.
        assert!(core.entries.is_empty());
        assert!(core.listing_scope.is_none());
        let (_, error) = core.load_failure().expect("error recorded");
        assert!(error.message.contains("protocol"));
    }

    #[test]
    fn unrecognized_url_bytes_read_as_no_image() {
        use windows::Win32::Foundation::WINCODEC_ERR_COMPONENTNOTFOUND;
        let error = |store_codec_names| DecodeError {
            code: WINCODEC_ERR_COMPONENTNOTFOUND.0,
            message: "component not found".to_string(),
            store_codec_names,
        };
        assert_eq!(url_decode_error(error(&[])).message, "No image at this URL");
        // A failure that names a Store codec keeps its install hint.
        let store_hinted = url_decode_error(error(&["avif"]));
        assert_eq!(store_hinted.message, "component not found");
        assert_eq!(store_hinted.store_codec_names, ["avif"]);
    }

    #[test]
    fn a_new_load_clears_the_previous_error() {
        let directory = fixture_directory("riv-error-supersede", &["a.png"]);
        let file = directory.join("a.png");
        let mut core = core();
        assert_eq!(core.load_url("ftp://a.com/b.png"), LoadOutcome::Failed);
        assert!(core.load_failure().is_some());
        core.load_path(&file);
        assert!(core.load_failure().is_none()); // the pending load owns the view now
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn a_local_open_after_a_url_restores_the_listing() {
        let directory = fixture_directory("riv-url-session-state", &["a.png"]);
        let file = directory.join("a.png");
        let mut core = core();
        assert_eq!(core.load_url("ftp://a.com/b.png"), LoadOutcome::Failed);
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
        let directory = fixture_directory("riv-url-drops-scan", &["a.png"]);
        let file = directory.join("a.png");
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

    #[test]
    fn a_stale_scan_arrival_is_discarded() {
        let directory = fixture_directory("riv-stale-scan", &["a.png"]);
        let file = directory.join("a.png");
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
        let directory = fixture_directory("riv-anchor-position", &["a.png", "b.png"]);
        let second = directory.join("b.png");
        let mut core = core();
        core.rescan_folder(&directory);
        core.load_path(&second);
        assert!(core.has_pending_display()); // the decode has not landed yet
        assert_eq!(core.listing_position(), Some((2, 2)));
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn a_refresh_keeps_the_weights_it_already_probed() {
        let directory = fixture_directory("riv-refresh-weights", &["a.png", "b.png"]);
        let edited = directory.join("b.png");
        let mut core = core();
        core.rescan_folder(&directory);
        for entry in &mut core.entries {
            entry.weight = DecodedWeight::Known(4096);
        }
        std::fs::write(&edited, b"listing only, now longer").expect("fixture file");
        core.submit_refresh_scan();
        let scope = ListingScope::Directory(directory.clone());
        core.install_listing_scan(ScannedListing::of(scope, &core.options));
        let weight_of = |name: &str| {
            core.entries
                .iter()
                .find(|entry| entry.location.display_name().eq_ignore_ascii_case(name))
                .map(|entry| entry.weight)
        };
        assert_eq!(weight_of("a.png"), Some(DecodedWeight::Known(4096)));
        // The file changed under the listing, so its old weight no longer describes it.
        assert_eq!(weight_of("b.png"), Some(DecodedWeight::Unknown));
        let _ = std::fs::remove_dir_all(&directory);
    }

    fn archive_member(archive: &Path, name: &str) -> ListingEntry {
        member_entry(
            archive,
            archive_reader::ArchiveMember {
                name: name.to_string(),
                uncompressed_bytes: 8,
                modified: UNIX_EPOCH,
            },
        )
        .expect("supported member")
    }

    #[test]
    fn an_archive_open_loads_its_first_member_on_arrival() {
        let archive = std::env::temp_dir().join("riv-archive-open.zip");
        let mut core = core();
        assert_eq!(core.load_path(&archive), LoadOutcome::Pending);
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
        assert_eq!(core.load_path(&archive), LoadOutcome::Pending);
        let resolved = std::path::absolute(&archive).expect("absolute");
        let scan = ScannedListing {
            scope: ListingScope::Archive(resolved),
            sort_mode: core.options.sort_mode,
            sort_descending: core.options.sort_descending,
            result: Err(decode::uncoded_error("enumerate failed")),
        };
        assert!(matches!(
            core.install_listing_scan(scan),
            ListingInstall::Opened {
                outcome: LoadOutcome::Failed
            }
        ));
        assert!(core.load_failure().is_some());
        assert!(!core.listing_scan_pending());
    }

    /// A request that fails on the spot still takes the view from whatever was loading.
    #[test]
    fn a_failed_load_drops_the_previous_wait() {
        let directory = fixture_directory("riv-failed-load-wait", &["a.png"]);
        let mut core = core();
        core.rescan_folder(&directory);
        core.load_path(&directory.join("a.png"));
        assert!(core.has_pending_display());
        let missing = directory.join("missing.png");
        assert_eq!(core.load_path(&missing), LoadOutcome::Failed);
        assert!(!core.has_pending_display());
        // The errored item is the position baseline, so the next move starts from it.
        assert!(core.navigation_anchor() == Some(&ItemLocation::File(missing)));
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn a_reload_re_collects_the_listing_it_sits_in() {
        let directory = fixture_directory("riv-reload-refresh", &["a.png"]);
        let first = directory.join("a.png");
        let mut core = core();
        core.rescan_folder(&directory);
        core.load_path(&first);
        std::fs::write(directory.join("b.png"), b"listing only").expect("fixture file");
        core.reload_current();
        assert!(core.listing_scan_pending());
        let scan = ScannedListing::of(ListingScope::Directory(directory.clone()), &core.options);
        core.install_listing_scan(scan);
        assert_eq!(core.entries.len(), 2); // the file added since the open is listed
        assert_eq!(core.listing_position(), Some((1, 2))); // the anchor kept its place
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn a_vanished_item_leaves_a_slot_adjacent_entries_still_answer() {
        let directory = fixture_directory("riv-reload-vanished", &["a.png", "b.png", "c.png"]);
        let mut core = core();
        core.rescan_folder(&directory);
        let middle = directory.join("b.png");
        // Anchored without a decode: a worker holding the file open would race the removal.
        core.request = ViewRequest::Pending(ItemLocation::File(middle.clone()));
        std::fs::remove_file(&middle).expect("fixture removal");
        assert_eq!(core.reload_current(), Some(LoadOutcome::Failed)); // nothing left to decode
        let scan = ScannedListing::of(ListingScope::Directory(directory.clone()), &core.options);
        core.install_listing_scan(scan);
        assert_eq!(core.entries.len(), 2); // the listing dropped it
        assert!(core.load_failure().is_some()); // and the error stays on screen
        fn target(core: &ImageCore, command: NavigationCommand) -> Option<PathBuf> {
            core.navigation_target(command)
                .and_then(|location| location.as_file().map(Path::to_path_buf))
        }
        let adjacent_entries = |core: &ImageCore| {
            (
                target(core, NavigationCommand::Next),
                target(core, NavigationCommand::Previous),
            )
        };
        let expected = (Some(directory.join("c.png")), Some(directory.join("a.png")));
        assert_eq!(adjacent_entries(&core), expected);
        // A second reload of the same missing item keeps the place it left.
        assert_eq!(core.reload_current(), Some(LoadOutcome::Failed));
        let scan = ScannedListing::of(ListingScope::Directory(directory.clone()), &core.options);
        core.install_listing_scan(scan);
        assert_eq!(adjacent_entries(&core), expected);
        // A digit-led name sorts first on every implementation, shifting every index; the place follows the entry beside it.
        std::fs::write(directory.join("0.png"), b"listing only").expect("fixture file");
        assert_eq!(core.reload_current(), Some(LoadOutcome::Failed));
        let scan = ScannedListing::of(ListingScope::Directory(directory.clone()), &core.options);
        core.install_listing_scan(scan);
        assert_eq!(core.entries.len(), 3);
        assert_eq!(adjacent_entries(&core), expected);
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn a_directory_open_loads_its_first_entry_on_arrival() {
        let directory = fixture_directory("riv-directory-open", &["a.png"]);
        let mut core = core();
        assert_eq!(core.load_path(&directory), LoadOutcome::Pending);
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

/// Preload polarity follows the navigation direction with a one-step grace.
#[cfg(test)]
mod navigation_direction_tests {
    use super::*;

    #[test]
    fn a_single_back_step_keeps_the_forward_polarity() {
        let mut core = core();
        core.record_navigation(NavigationCommand::Previous);
        assert!(!core.navigating_backward);
        assert_eq!(core.preload_plan(), (1, 3, 1 << 30));
    }

    #[test]
    fn two_consecutive_back_steps_flip_the_polarity() {
        let mut core = core();
        core.record_navigation(NavigationCommand::Previous);
        core.record_navigation(NavigationCommand::Previous);
        assert!(core.navigating_backward);
        assert_eq!(core.preload_plan(), (3, 1, 1 << 30));
        // The way back flips with the same grace.
        core.record_navigation(NavigationCommand::Next);
        assert!(core.navigating_backward);
        core.record_navigation(NavigationCommand::Next);
        assert!(!core.navigating_backward);
    }

    #[test]
    fn an_interrupted_run_starts_over() {
        let mut core = core();
        core.record_navigation(NavigationCommand::Previous);
        core.record_navigation(NavigationCommand::Next);
        core.record_navigation(NavigationCommand::Previous);
        assert!(!core.navigating_backward); // never two in a row
    }

    #[test]
    fn jumps_declare_their_direction() {
        let mut core = core();
        core.record_navigation(NavigationCommand::Last);
        assert!(core.navigating_backward);
        core.record_navigation(NavigationCommand::First);
        assert!(!core.navigating_backward);
    }

    #[test]
    fn a_declared_direction_aims_at_once() {
        let mut core = core();
        core.set_navigation_direction(true);
        assert_eq!(core.preload_plan(), (3, 1, 1 << 30));
        core.reset_navigation_direction();
        assert_eq!(core.preload_plan(), (1, 3, 1 << 30));
    }
}

/// Deleting commits the listing change and answers what takes the place.
#[cfg(test)]
mod deleted_item_tests {
    use super::*;

    #[test]
    fn a_deleted_item_hands_the_place_to_its_successor() {
        let file = |index: usize| {
            ItemLocation::File(PathBuf::from(format!("C:\\pictures\\{index:03}.png")))
        };
        let mut core = core_with_files(3, Some(1));
        // The successor is resolved while the deleted entry is still listed.
        let successor = core.remove_deleted_item(&file(1), NavigationCommand::Next);
        assert_eq!(successor, file(2).as_file().map(Path::to_path_buf));
        assert_eq!(core.entries.len(), 2);

        // Looping wraps at the end of the folder, as a step would.
        let mut core = core_with_files(3, Some(2));
        let successor = core.remove_deleted_item(&file(2), NavigationCommand::Next);
        assert_eq!(successor, file(0).as_file().map(Path::to_path_buf));

        // Without looping the end falls back to the other direction.
        let mut core = core_with_files(3, Some(2));
        core.options.loop_within_folder = false;
        let successor = core.remove_deleted_item(&file(2), NavigationCommand::Next);
        assert_eq!(successor, file(1).as_file().map(Path::to_path_buf));

        // Nothing left to show clears the current item instead.
        let mut core = core_with_files(1, Some(0));
        assert_eq!(
            core.remove_deleted_item(&file(0), NavigationCommand::Next),
            None
        );
        assert!(core.entries.is_empty());
        assert!(core.current.is_none() && !core.has_pending_display());
    }
}

/// The menu playlist shows a window of the asked size with the current item centered.
#[cfg(test)]
mod playlist_window_tests {
    use super::*;

    #[test]
    fn a_short_listing_shows_whole() {
        let window = core_with_files(5, Some(2)).playlist_window(25);
        assert_eq!(window.locations.len(), 5);
        assert_eq!(window.first_index, 0);
        assert_eq!(window.current_slot, Some(2));
        assert_eq!(window.hidden_after, 0);
        assert_eq!(window.locations[0].display_name(), "000.png");
    }

    #[test]
    fn the_current_item_sits_at_the_window_center() {
        let window = core_with_files(100, Some(50)).playlist_window(25);
        assert_eq!(window.first_index, 38);
        assert_eq!(window.current_slot, Some(12));
        assert_eq!(window.locations.len(), 25);
        assert_eq!(window.hidden_after, 37);
    }

    #[test]
    fn the_window_clamps_at_both_ends() {
        let near_start = core_with_files(100, Some(3)).playlist_window(25);
        assert_eq!(near_start.first_index, 0);
        assert_eq!(near_start.current_slot, Some(3));
        assert_eq!(near_start.hidden_after, 75);
        let near_end = core_with_files(100, Some(97)).playlist_window(25);
        assert_eq!(near_end.first_index, 75);
        assert_eq!(near_end.current_slot, Some(22));
        assert_eq!(near_end.hidden_after, 0);
    }

    #[test]
    fn no_anchor_starts_at_the_top() {
        let window = core_with_files(100, None).playlist_window(25);
        assert_eq!(window.first_index, 0);
        assert_eq!(window.current_slot, None);
        assert_eq!(window.hidden_after, 75);
    }
}
