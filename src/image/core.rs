//! Load state machine, item listing, preload cache, and the decode worker pool.

use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::c_void;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_HIDDEN;
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};
use windows::Win32::UI::Shell::StrCmpLogicalW;
use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_APP};
use windows::core::PCWSTR;

use super::decode::{self, DecodeError, DecodedImage};
use crate::archive::reader as archive_reader;

pub const WM_APP_DECODE_COMPLETE: u32 = WM_APP + 1;

/// Viewable item identity; hashing is exact, locations_equal compares paths.
#[derive(Clone, PartialEq, Eq, Hash)]
pub enum ItemLocation {
    File(PathBuf),
    ArchiveMember { archive: PathBuf, member: String },
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
        }
    }

    /// Full user-facing location text ("archive \u{203a} member" for members).
    pub fn display_text(&self) -> String {
        match self {
            Self::File(path) => path.display().to_string(),
            Self::ArchiveMember { archive, member } => {
                format!("{} \u{203a} {member}", archive.display())
            }
        }
    }

    /// The file that carries this item on disk (the archive for members).
    pub fn containing_file(&self) -> &Path {
        match self {
            Self::File(path) => path,
            Self::ArchiveMember { archive, .. } => archive,
        }
    }

    /// Some only for plain files; members cannot take file operations.
    pub fn as_file(&self) -> Option<&Path> {
        match self {
            Self::File(path) => Some(path),
            Self::ArchiveMember { .. } => None,
        }
    }

    fn exists(&self) -> bool {
        self.containing_file().is_file()
    }

    fn extension_lowercase(&self) -> Option<String> {
        let name_path = match self {
            Self::File(path) => path.as_path(),
            Self::ArchiveMember { member, .. } => Path::new(member),
        };
        name_path
            .extension()
            .map(|extension| extension.to_string_lossy().to_lowercase())
    }
}

/// Case-insensitive equality on the path parts (Windows semantics).
pub fn locations_equal(a: &ItemLocation, b: &ItemLocation) -> bool {
    match (a, b) {
        (ItemLocation::File(a), ItemLocation::File(b)) => paths_equal(a, b),
        (
            ItemLocation::ArchiveMember {
                archive: first_archive,
                member: first_member,
            },
            ItemLocation::ArchiveMember {
                archive: second_archive,
                member: second_member,
            },
        ) => paths_equal(first_archive, second_archive) && first_member == second_member,
        _ => false,
    }
}

/// What the entry listing was scanned from.
enum ListingScope {
    Directory(PathBuf),
    Archive(PathBuf),
}

/// Preload mode 0/1/2 -> (radius, cache budget in bytes).
const PRELOAD_SPECIFICATIONS: [(usize, u64); 3] =
    [(0, 0), (1, 250 * 1024 * 1024), (4, 2 * 1024 * 1024 * 1024)];

#[derive(Clone, PartialEq)]
pub struct CoreOptions {
    pub sort_mode: SortMode,
    pub sort_descending: bool,
    pub preloading_mode: usize,
    pub loop_folders_enabled: bool,
    pub skip_hidden: bool,
    pub allow_mime_content_detection: bool,
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

pub struct FolderEntry {
    pub location: ItemLocation,
    wide_name: Vec<u16>,
    file_size: u64,
    modified: SystemTime,
    created: SystemTime,
}

pub enum DecodeStage {
    Preview,
    Final,
}

pub struct DecodeCompletion {
    pub location: ItemLocation,
    pub file_size: u64,
    pub stage: DecodeStage,
    pub result: Result<Arc<DecodedImage>, DecodeError>,
}

pub struct CurrentImage {
    pub location: ItemLocation,
    pub image: Arc<DecodedImage>,
}

struct CacheEntry {
    file_size: u64,
    image: Arc<DecodedImage>,
}

pub struct ImageCore {
    pool: DecodePool,
    options: CoreOptions,
    listing_scope: Option<ListingScope>,
    entries: Vec<FolderEntry>,
    /// Item awaiting display; replacing it invalidates the previous load.
    pending_display: Option<ItemLocation>,
    in_flight: HashMap<ItemLocation, Arc<AtomicBool>>,
    cache: HashMap<ItemLocation, CacheEntry>,
    pub current: Option<CurrentImage>,
    pub load_error: Option<(ItemLocation, DecodeError)>,
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
        }
    }

    pub fn update_options(&mut self, options: CoreOptions) {
        if options == self.options {
            return;
        }
        let list_affected = options.sort_mode != self.options.sort_mode
            || options.sort_descending != self.options.sort_descending
            || options.skip_hidden != self.options.skip_hidden
            || options.allow_mime_content_detection != self.options.allow_mime_content_detection;
        self.options = options;
        if list_affected {
            self.rescan_listing();
        }
        self.preload_neighbors();
    }

    pub fn folder_position(&self) -> Option<(usize, usize)> {
        let current = self.current.as_ref()?;
        let index = self.position_of(&current.location)?;
        Some((index + 1, self.entries.len()))
    }

    pub fn has_folder_entries(&self) -> bool {
        !self.entries.is_empty()
    }

    /// (file size, modified) of the current item; member values from the listing.
    pub fn current_item_metadata(&self) -> Option<(u64, Option<SystemTime>)> {
        let current = self.current.as_ref()?;
        match &current.location {
            ItemLocation::File(path) => {
                let metadata = std::fs::metadata(path).ok();
                Some((
                    metadata.as_ref().map_or(0, std::fs::Metadata::len),
                    metadata.and_then(|metadata| metadata.modified().ok()),
                ))
            }
            ItemLocation::ArchiveMember { .. } => {
                let entry = self
                    .position_of(&current.location)
                    .map(|index| &self.entries[index]);
                Some((
                    entry.map_or(0, |entry| entry.file_size),
                    entry.map(|entry| entry.modified),
                ))
            }
        }
    }

    pub fn reload_current(&mut self) -> bool {
        let Some(location) = self.pending_display.clone().or_else(|| {
            self.current
                .as_ref()
                .map(|current| current.location.clone())
        }) else {
            return false;
        };
        self.cache.remove(&location);
        self.rescan_listing();
        self.load_item(&location)
    }

    pub fn load_path(&mut self, path: &Path) -> bool {
        let Ok(path) = std::path::absolute(path) else {
            return false;
        };
        if path.is_dir() {
            self.rescan_folder(&path);
            let Some(first) = self.first_existing_entry() else {
                return false;
            };
            return self.load_item(&first);
        }
        let extension = path
            .extension()
            .map(|extension| extension.to_string_lossy().to_lowercase());
        if extension
            .as_deref()
            .is_some_and(archive_reader::is_archive_extension)
        {
            return self.load_archive(&path);
        }
        let directory = path.parent().map(Path::to_path_buf);
        let already_scanned = match (&self.listing_scope, &directory) {
            (Some(ListingScope::Directory(scanned)), Some(directory)) => scanned == directory,
            _ => false,
        };
        if let Some(directory) = directory
            && !already_scanned
        {
            self.rescan_folder(&directory);
        }
        self.load_item(&ItemLocation::File(path))
    }

    /// Opens an archive as a virtual folder of its image members.
    fn load_archive(&mut self, archive: &Path) -> bool {
        self.entries = Vec::new();
        self.listing_scope = Some(ListingScope::Archive(archive.to_path_buf()));
        let members = match archive_reader::enumerate(archive) {
            Ok(members) => members,
            Err(error) => {
                self.load_error = Some((
                    ItemLocation::File(archive.to_path_buf()),
                    DecodeError {
                        code: error.code,
                        message: error.message,
                        store_extension: None,
                    },
                ));
                return false;
            }
        };
        let mut entries: Vec<FolderEntry> = members
            .into_iter()
            .filter_map(|member| member_entry(archive, member))
            .collect();
        if entries.is_empty() {
            self.load_error = Some((
                ItemLocation::File(archive.to_path_buf()),
                DecodeError {
                    code: 0,
                    message: "archive contains no supported images".to_string(),
                    store_extension: None,
                },
            ));
            return false;
        }
        sort_entries(&mut entries, &self.options);
        self.entries = entries;
        let Some(first) = self.first_existing_entry() else {
            return false;
        };
        self.load_item(&first)
    }

    fn load_item(&mut self, location: &ItemLocation) -> bool {
        let file_size = match location {
            ItemLocation::File(path) => match std::fs::metadata(path) {
                Ok(metadata) => metadata.len(),
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
                Some(index) => self.entries[index].file_size,
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
        };
        if let Some(entry) = self.cache.get(location)
            && entry.file_size == file_size
        {
            self.current = Some(CurrentImage {
                location: location.clone(),
                image: entry.image.clone(),
            });
            self.pending_display = None;
            self.load_error = None;
            self.preload_neighbors();
            return true;
        }
        self.pending_display = Some(location.clone());
        if let Some(cancellation) = self.in_flight.get(location) {
            // Already queued as a preload: revoke any cancellation and promote.
            cancellation.store(false, Ordering::Relaxed);
            self.pool.promote(location);
        } else {
            let cancellation = Arc::new(AtomicBool::new(false));
            self.in_flight
                .insert(location.clone(), cancellation.clone());
            self.pool
                .submit(location.clone(), file_size, cancellation, true);
        }
        self.cancel_irrelevant_decodes();
        false
    }

    pub fn navigate(&mut self, command: NavigationCommand) -> Option<bool> {
        self.refresh_listing_if_current_missing();
        let current_location = self.navigation_anchor();
        let target = self.navigation_target(command)?;
        if current_location.is_some_and(|current| locations_equal(&current, &target)) {
            return None; // same item, nothing to do
        }
        Some(self.load_item(&target))
    }

    pub fn peek_navigation_target(&mut self, command: NavigationCommand) -> Option<ItemLocation> {
        self.refresh_listing_if_current_missing();
        self.navigation_target(command)
    }

    pub fn refresh_folder(&mut self) {
        self.rescan_listing();
    }

    fn navigation_anchor(&self) -> Option<ItemLocation> {
        self.pending_display
            .clone()
            .or_else(|| {
                self.load_error
                    .as_ref()
                    .map(|(location, _)| location.clone())
            })
            .or_else(|| {
                self.current
                    .as_ref()
                    .map(|current| current.location.clone())
            })
    }

    fn navigation_target(&self, command: NavigationCommand) -> Option<ItemLocation> {
        if self.entries.is_empty() {
            return None;
        }
        let current_index = self
            .navigation_anchor()
            .as_ref()
            .and_then(|location| self.position_of(location));
        match command {
            NavigationCommand::First => self.first_existing_entry(),
            NavigationCommand::Last => self.last_existing_entry(),
            NavigationCommand::Next => self.step_existing_entry(current_index, 1),
            NavigationCommand::Previous => self.step_existing_entry(current_index, -1),
        }
    }

    pub fn on_decode_complete(&mut self, completion: DecodeCompletion) -> bool {
        if matches!(completion.stage, DecodeStage::Preview) {
            let is_pending = self
                .pending_display
                .as_ref()
                .is_some_and(|pending| locations_equal(pending, &completion.location));
            if is_pending && let Ok(image) = completion.result {
                self.current = Some(CurrentImage {
                    location: completion.location,
                    image,
                });
                self.load_error = None;
                return true;
            }
            return false;
        }
        self.in_flight.remove(&completion.location);
        let is_pending = self
            .pending_display
            .as_ref()
            .is_some_and(|pending| locations_equal(pending, &completion.location));
        if let Err(error) = &completion.result
            && error.is_cancelled()
        {
            // Navigation can return to an item while its decode is cancelling.
            if is_pending {
                let cancellation = Arc::new(AtomicBool::new(false));
                self.in_flight
                    .insert(completion.location.clone(), cancellation.clone());
                self.pool.submit(
                    completion.location,
                    completion.file_size,
                    cancellation,
                    true,
                );
            }
            return false;
        }
        match completion.result {
            Ok(image) => {
                self.cache.insert(
                    completion.location.clone(),
                    CacheEntry {
                        file_size: completion.file_size,
                        image: image.clone(),
                    },
                );
                if is_pending {
                    self.current = Some(CurrentImage {
                        location: completion.location,
                        image,
                    });
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
                }
                is_pending
            }
        }
    }

    fn rescan_folder(&mut self, directory: &Path) {
        let mut entries = scan_folder(directory, &self.options);
        sort_entries(&mut entries, &self.options);
        self.entries = entries;
        self.listing_scope = Some(ListingScope::Directory(directory.to_path_buf()));
    }

    /// Re-scans whatever the current listing came from (folder or archive).
    fn rescan_listing(&mut self) {
        match &self.listing_scope {
            Some(ListingScope::Directory(directory)) => self.rescan_folder(&directory.clone()),
            Some(ListingScope::Archive(archive)) => {
                let archive = archive.clone();
                let mut entries: Vec<FolderEntry> = archive_reader::enumerate(&archive)
                    .map(|members| {
                        members
                            .into_iter()
                            .filter_map(|member| member_entry(&archive, member))
                            .collect()
                    })
                    .unwrap_or_default();
                sort_entries(&mut entries, &self.options);
                self.entries = entries;
            }
            None => {}
        }
    }

    /// The listing is fixed at load time; only a vanished current item rescans.
    fn refresh_listing_if_current_missing(&mut self) {
        let current_missing = self
            .current
            .as_ref()
            .is_some_and(|current| !current.location.exists());
        if current_missing {
            self.rescan_listing();
        }
    }

    fn position_of(&self, location: &ItemLocation) -> Option<usize> {
        self.entries
            .iter()
            .position(|entry| locations_equal(&entry.location, location))
    }

    fn first_existing_entry(&self) -> Option<ItemLocation> {
        self.entries
            .iter()
            .find(|entry| entry.location.exists())
            .map(|entry| entry.location.clone())
    }

    fn last_existing_entry(&self) -> Option<ItemLocation> {
        self.entries
            .iter()
            .rev()
            .find(|entry| entry.location.exists())
            .map(|entry| entry.location.clone())
    }

    fn step_existing_entry(
        &self,
        current: Option<usize>,
        direction: isize,
    ) -> Option<ItemLocation> {
        let length = self.entries.len() as isize;
        let start = current.map_or(0, |index| index as isize);
        let mut index = start;
        for _ in 0..length {
            index += direction;
            if self.options.loop_folders_enabled {
                index = index.rem_euclid(length);
            } else if !(0..length).contains(&index) {
                return None; // stop at folder ends when not looping
            }
            let entry = &self.entries[index as usize];
            if entry.location.exists() {
                return Some(entry.location.clone());
            }
        }
        None
    }

    fn preload_neighbors(&mut self) {
        let (distance, budget) = PRELOAD_SPECIFICATIONS[self.options.preloading_mode.min(2)];
        if distance == 0 {
            self.cache.clear(); // preloading off: drop the cache
            return;
        }
        if let Some(current_index) = self
            .current
            .as_ref()
            .map(|current| current.location.clone())
            .and_then(|location| self.position_of(&location))
        {
            let length = self.entries.len();
            for step in 1..=distance {
                for direction in [1isize, -1] {
                    let offset = step as isize * direction;
                    let Some(index) = neighbor_index(
                        current_index,
                        offset,
                        length,
                        self.options.loop_folders_enabled,
                    ) else {
                        continue;
                    };
                    let entry = &self.entries[index];
                    if entry.file_size > budget / 2
                        || self.in_flight.contains_key(&entry.location)
                        || self
                            .cache
                            .get(&entry.location)
                            .is_some_and(|cached| cached.file_size == entry.file_size)
                        || self
                            .pending_display
                            .as_ref()
                            .is_some_and(|pending| locations_equal(pending, &entry.location))
                    {
                        continue;
                    }
                    let cancellation = Arc::new(AtomicBool::new(false));
                    self.in_flight
                        .insert(entry.location.clone(), cancellation.clone());
                    self.pool
                        .submit(entry.location.clone(), entry.file_size, cancellation, false);
                }
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
        let (distance, _) = PRELOAD_SPECIFICATIONS[self.options.preloading_mode.min(2)];
        let anchor_index = self
            .navigation_anchor()
            .as_ref()
            .and_then(|location| self.position_of(location));
        if let Some(anchor_index) = anchor_index {
            let length = self.entries.len();
            for step in 1..=distance {
                for direction in [1isize, -1] {
                    if let Some(index) = neighbor_index(
                        anchor_index,
                        step as isize * direction,
                        length,
                        self.options.loop_folders_enabled,
                    ) {
                        relevant.insert(self.entries[index].location.clone());
                    }
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

    /// Evicts entries farthest from the current position until within budget.
    fn evict_cache(&mut self) {
        let (_, budget) = PRELOAD_SPECIFICATIONS[self.options.preloading_mode.min(2)];
        let mut total: u64 = self
            .cache
            .values()
            .map(|entry| entry.image.pixel_bytes() as u64)
            .sum();
        if total <= budget {
            return;
        }
        let current_index = self
            .current
            .as_ref()
            .and_then(|current| self.position_of(&current.location));
        let length = self.entries.len();
        let loop_enabled = self.options.loop_folders_enabled;
        let mut ranked: Vec<(ItemLocation, u64, usize)> = self
            .cache
            .iter()
            .map(|(location, entry)| {
                let distance = self
                    .position_of(location)
                    .zip(current_index)
                    .map_or(usize::MAX, |(index, current)| {
                        ring_distance(index, current, length, loop_enabled)
                    });
                (location.clone(), entry.image.pixel_bytes() as u64, distance)
            })
            .collect();
        ranked.sort_by_key(|(_, _, distance)| std::cmp::Reverse(*distance));
        for (location, cost, _) in ranked {
            if total <= budget {
                break;
            }
            self.cache.remove(&location);
            total -= cost;
        }
    }
}

fn neighbor_index(
    current: usize,
    offset: isize,
    length: usize,
    loop_enabled: bool,
) -> Option<usize> {
    if length == 0 {
        return None;
    }
    let index = current as isize + offset;
    if loop_enabled {
        let wrapped = index.rem_euclid(length as isize) as usize;
        (wrapped != current).then_some(wrapped)
    } else {
        (0..length as isize)
            .contains(&index)
            .then_some(index as usize)
    }
}

fn ring_distance(a: usize, b: usize, length: usize, loop_enabled: bool) -> usize {
    let direct = a.abs_diff(b);
    if loop_enabled && length > 0 {
        direct.min(length - direct)
    } else {
        direct
    }
}

/// Case-insensitive path equality (Windows semantics).
fn paths_equal(a: &Path, b: &Path) -> bool {
    a == b
        || a.as_os_str()
            .to_string_lossy()
            .eq_ignore_ascii_case(&b.as_os_str().to_string_lossy())
}

fn scan_folder(directory: &Path, options: &CoreOptions) -> Vec<FolderEntry> {
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
            || (options.allow_mime_content_detection
                && decode::probe_file(&entry.path()).is_some());
        if !included {
            continue;
        }
        let wide_name: Vec<u16> = file_name.encode_wide().chain(std::iter::once(0)).collect();
        entries.push(FolderEntry {
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
fn member_entry(archive: &Path, member: archive_reader::MemberInfo) -> Option<FolderEntry> {
    Path::new(&member.name)
        .extension()
        .map(|extension| extension.to_string_lossy().to_lowercase())
        .filter(|extension| decode::is_supported_extension(extension))?;
    let wide_name: Vec<u16> = member
        .name
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    Some(FolderEntry {
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

fn sort_entries(entries: &mut [FolderEntry], options: &CoreOptions) {
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

fn compare_natural_names(a: &FolderEntry, b: &FolderEntry) -> std::cmp::Ordering {
    let result =
        unsafe { StrCmpLogicalW(PCWSTR(a.wide_name.as_ptr()), PCWSTR(b.wide_name.as_ptr())) };
    result.cmp(&0)
}

fn format_name_of(location: &ItemLocation) -> &'static str {
    location
        .extension_lowercase()
        .and_then(|extension| decode::format_name_for_extension(&extension))
        .unwrap_or("")
}

struct DecodeJob {
    location: ItemLocation,
    file_size: u64,
    cancellation: Arc<AtomicBool>,
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
            std::thread::available_parallelism().map_or(2, |count| count.get().min(4));
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
        cancellation: Arc<AtomicBool>,
        immediate: bool,
    ) {
        let mut queue = self.shared.queue.lock().expect("decode queue poisoned");
        let job = DecodeJob {
            location,
            file_size,
            cancellation,
        };
        if immediate {
            queue.push_front(job);
        } else {
            queue.push_back(job);
        }
        drop(queue);
        self.shared.available.notify_one();
    }

    fn promote(&self, location: &ItemLocation) {
        let mut queue = self.shared.queue.lock().expect("decode queue poisoned");
        if let Some(position) = queue
            .iter()
            .position(|job| locations_equal(&job.location, location))
            && let Some(job) = queue.remove(position)
        {
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
        let result = match &job.location {
            ItemLocation::File(path) => {
                if decode::is_raw_two_stage(path)
                    && let Some(preview) = decode::decode_raw_preview(path, &job.cancellation)
                {
                    post_completion(
                        window,
                        Box::new(DecodeCompletion {
                            location: job.location.clone(),
                            file_size: job.file_size,
                            stage: DecodeStage::Preview,
                            result: Ok(Arc::new(preview)),
                        }),
                    );
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
                    Err(error) if error.cancelled => Err(DecodeError::cancelled()),
                    Err(error) => Err(DecodeError {
                        code: error.code,
                        message: error.message,
                        store_extension: None,
                    }),
                }
            }
        }
        .map(Arc::new);
        post_completion(
            window,
            Box::new(DecodeCompletion {
                location: job.location,
                file_size: job.file_size,
                stage: DecodeStage::Final,
                result,
            }),
        );
    }
}

fn post_completion(window: isize, completion: Box<DecodeCompletion>) {
    let pointer = Box::into_raw(completion);
    let posted = unsafe {
        PostMessageW(
            Some(HWND(window as *mut c_void)),
            WM_APP_DECODE_COMPLETE,
            WPARAM(0),
            LPARAM(pointer as isize),
        )
    };
    if posted.is_err() {
        drop(unsafe { Box::from_raw(pointer) });
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
    fn locations_compare_with_windows_path_semantics() {
        let file = |path: &str| ItemLocation::File(PathBuf::from(path));
        assert!(locations_equal(&file("C:\\A.PNG"), &file("c:\\a.png")));
        assert!(locations_equal(
            &member("C:\\A.CBZ", "01.png"),
            &member("c:\\a.cbz", "01.png"),
        ));
        // Member names stay exact: archives distinguish case.
        assert!(!locations_equal(
            &member("C:\\a.cbz", "01.PNG"),
            &member("C:\\a.cbz", "01.png"),
        ));
        assert!(!locations_equal(
            &file("C:\\a.cbz"),
            &member("C:\\a.cbz", "01.png"),
        ));
    }

    #[test]
    fn member_extension_resolves_format_names() {
        assert_eq!(format_name_of(&member("C:\\a.cbz", "art/01.png")), "PNG");
        assert_eq!(format_name_of(&member("C:\\a.cbz", "readme.txt")), "");
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
