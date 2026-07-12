//! Load state machine, folder listing, preload cache, and the decode worker pool.

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

pub const WM_APP_DECODE_COMPLETE: u32 = WM_APP + 1;

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
    pub path: PathBuf,
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
    pub path: PathBuf,
    pub file_size: u64,
    pub stage: DecodeStage,
    pub result: Result<Arc<DecodedImage>, DecodeError>,
}

pub struct CurrentImage {
    pub path: PathBuf,
    pub image: Arc<DecodedImage>,
}

struct CacheEntry {
    file_size: u64,
    image: Arc<DecodedImage>,
}

pub struct ImageCore {
    pool: DecodePool,
    options: CoreOptions,
    folder_directory: Option<PathBuf>,
    entries: Vec<FolderEntry>,
    /// Path awaiting display; replacing it invalidates the previous load.
    pending_display: Option<PathBuf>,
    in_flight: HashMap<PathBuf, Arc<AtomicBool>>,
    cache: HashMap<PathBuf, CacheEntry>,
    pub current: Option<CurrentImage>,
    pub load_error: Option<(PathBuf, DecodeError)>,
}

impl ImageCore {
    pub fn new(window: HWND, options: CoreOptions) -> Self {
        Self {
            pool: DecodePool::new(window.0 as isize),
            options,
            folder_directory: None,
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
        if list_affected && let Some(directory) = self.folder_directory.clone() {
            self.rescan_folder(&directory);
        }
        self.preload_neighbors();
    }

    pub fn folder_position(&self) -> Option<(usize, usize)> {
        let current = self.current.as_ref()?;
        let index = self.position_of(&current.path)?;
        Some((index + 1, self.entries.len()))
    }

    pub fn has_folder_entries(&self) -> bool {
        !self.entries.is_empty()
    }

    pub fn reload_current(&mut self) -> bool {
        let Some(path) = self
            .pending_display
            .clone()
            .or_else(|| self.current.as_ref().map(|current| current.path.clone()))
        else {
            return false;
        };
        self.cache.remove(&path);
        if let Some(directory) = self.folder_directory.clone() {
            self.rescan_folder(&directory);
        }
        self.load_file(&path)
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
            return self.load_file(&first);
        }
        let directory = path.parent().map(Path::to_path_buf);
        if let Some(directory) = directory
            && self.folder_directory.as_deref() != Some(&directory)
        {
            self.rescan_folder(&directory);
        }
        self.load_file(&path)
    }

    fn load_file(&mut self, path: &Path) -> bool {
        let file_size = match std::fs::metadata(path) {
            Ok(metadata) => metadata.len(),
            Err(error) => {
                self.load_error = Some((
                    path.to_path_buf(),
                    DecodeError {
                        code: error.raw_os_error().unwrap_or(0),
                        message: error.to_string(),
                        store_extension: None,
                    },
                ));
                return false;
            }
        };
        if let Some(entry) = self.cache.get(path)
            && entry.file_size == file_size
        {
            self.current = Some(CurrentImage {
                path: path.to_path_buf(),
                image: entry.image.clone(),
            });
            self.pending_display = None;
            self.load_error = None;
            self.preload_neighbors();
            return true;
        }
        self.pending_display = Some(path.to_path_buf());
        if let Some(cancellation) = self.in_flight.get(path) {
            // Already queued as a preload: revoke any cancellation and promote.
            cancellation.store(false, Ordering::Relaxed);
            self.pool.promote(path);
        } else {
            let cancellation = Arc::new(AtomicBool::new(false));
            self.in_flight
                .insert(path.to_path_buf(), cancellation.clone());
            self.pool
                .submit(path.to_path_buf(), file_size, cancellation, true);
        }
        self.cancel_irrelevant_decodes();
        false
    }

    pub fn navigate(&mut self, command: NavigationCommand) -> Option<bool> {
        self.refresh_folder_if_current_missing();
        let current_path = self.navigation_anchor();
        let target = self.navigation_target(command)?;
        if current_path.is_some_and(|current| paths_equal(&current, &target)) {
            return None; // same file, nothing to do
        }
        Some(self.load_file(&target))
    }

    pub fn peek_navigation_target(&mut self, command: NavigationCommand) -> Option<PathBuf> {
        self.refresh_folder_if_current_missing();
        self.navigation_target(command)
    }

    pub fn refresh_folder(&mut self) {
        if let Some(directory) = self.folder_directory.clone() {
            self.rescan_folder(&directory);
        }
    }

    fn navigation_anchor(&self) -> Option<PathBuf> {
        self.pending_display
            .clone()
            .or_else(|| self.load_error.as_ref().map(|(path, _)| path.clone()))
            .or_else(|| self.current.as_ref().map(|current| current.path.clone()))
    }

    fn navigation_target(&self, command: NavigationCommand) -> Option<PathBuf> {
        if self.entries.is_empty() {
            return None;
        }
        let current_index = self
            .navigation_anchor()
            .as_deref()
            .and_then(|path| self.position_of(path));
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
                .as_deref()
                .is_some_and(|pending| paths_equal(pending, &completion.path));
            if is_pending && let Ok(image) = completion.result {
                self.current = Some(CurrentImage {
                    path: completion.path,
                    image,
                });
                self.load_error = None;
                return true;
            }
            return false;
        }
        self.in_flight.remove(&completion.path);
        let is_pending = self
            .pending_display
            .as_deref()
            .is_some_and(|pending| paths_equal(pending, &completion.path));
        if let Err(error) = &completion.result
            && error.is_cancelled()
        {
            // Navigation can return to a path while its decode is cancelling.
            if is_pending {
                let cancellation = Arc::new(AtomicBool::new(false));
                self.in_flight
                    .insert(completion.path.clone(), cancellation.clone());
                self.pool
                    .submit(completion.path, completion.file_size, cancellation, true);
            }
            return false;
        }
        match completion.result {
            Ok(image) => {
                self.cache.insert(
                    completion.path.clone(),
                    CacheEntry {
                        file_size: completion.file_size,
                        image: image.clone(),
                    },
                );
                if is_pending {
                    self.current = Some(CurrentImage {
                        path: completion.path,
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
                    self.load_error = Some((completion.path, error));
                }
                is_pending
            }
        }
    }

    fn rescan_folder(&mut self, directory: &Path) {
        let mut entries = scan_folder(directory, &self.options);
        sort_entries(&mut entries, &self.options);
        self.entries = entries;
        self.folder_directory = Some(directory.to_path_buf());
    }

    /// The listing is fixed at load time; only a vanished current file forces
    /// a rescan (files added later belong to a new instance).
    fn refresh_folder_if_current_missing(&mut self) {
        let Some(directory) = self.folder_directory.clone() else {
            return;
        };
        let current_missing = self
            .current
            .as_ref()
            .is_some_and(|current| !current.path.is_file());
        if current_missing {
            self.rescan_folder(&directory);
        }
    }

    fn position_of(&self, path: &Path) -> Option<usize> {
        self.entries
            .iter()
            .position(|entry| paths_equal(&entry.path, path))
    }

    fn first_existing_entry(&self) -> Option<PathBuf> {
        self.entries
            .iter()
            .find(|entry| entry.path.is_file())
            .map(|entry| entry.path.clone())
    }

    fn last_existing_entry(&self) -> Option<PathBuf> {
        self.entries
            .iter()
            .rev()
            .find(|entry| entry.path.is_file())
            .map(|entry| entry.path.clone())
    }

    fn step_existing_entry(&self, current: Option<usize>, direction: isize) -> Option<PathBuf> {
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
            if entry.path.is_file() {
                return Some(entry.path.clone());
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
            .map(|current| current.path.clone())
            .and_then(|path| self.position_of(&path))
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
                        || self.in_flight.contains_key(&entry.path)
                        || self
                            .cache
                            .get(&entry.path)
                            .is_some_and(|cached| cached.file_size == entry.file_size)
                        || self
                            .pending_display
                            .as_deref()
                            .is_some_and(|pending| paths_equal(pending, &entry.path))
                    {
                        continue;
                    }
                    let cancellation = Arc::new(AtomicBool::new(false));
                    self.in_flight
                        .insert(entry.path.clone(), cancellation.clone());
                    self.pool
                        .submit(entry.path.clone(), entry.file_size, cancellation, false);
                }
            }
        }
        self.cancel_irrelevant_decodes();
        self.evict_cache();
    }

    /// Decodes for paths outside the pending/current preload neighborhood are
    /// dropped from the queue (no completion follows) or flagged if running.
    fn cancel_irrelevant_decodes(&mut self) {
        let mut relevant: HashSet<PathBuf> = HashSet::new();
        if let Some(pending) = &self.pending_display {
            relevant.insert(pending.clone());
        }
        if let Some(current) = &self.current {
            relevant.insert(current.path.clone());
        }
        let (distance, _) = PRELOAD_SPECIFICATIONS[self.options.preloading_mode.min(2)];
        let anchor_index = self
            .navigation_anchor()
            .as_deref()
            .and_then(|path| self.position_of(path));
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
                        relevant.insert(self.entries[index].path.clone());
                    }
                }
            }
        }
        for path in self.pool.remove_queued_except(&relevant) {
            self.in_flight.remove(&path);
        }
        for (path, cancellation) in &self.in_flight {
            if !relevant.contains(path) {
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
            .and_then(|current| self.position_of(&current.path));
        let length = self.entries.len();
        let loop_enabled = self.options.loop_folders_enabled;
        let mut ranked: Vec<(PathBuf, u64, usize)> = self
            .cache
            .iter()
            .map(|(path, entry)| {
                let distance = self
                    .position_of(path)
                    .zip(current_index)
                    .map_or(usize::MAX, |(index, current)| {
                        ring_distance(index, current, length, loop_enabled)
                    });
                (path.clone(), entry.image.pixel_bytes() as u64, distance)
            })
            .collect();
        ranked.sort_by_key(|(_, _, distance)| std::cmp::Reverse(*distance));
        for (path, cost, _) in ranked {
            if total <= budget {
                break;
            }
            self.cache.remove(&path);
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
            path: entry.path(),
            wide_name,
            file_size: metadata.len(),
            modified: metadata.modified().unwrap_or(UNIX_EPOCH),
            created: metadata.created().unwrap_or(UNIX_EPOCH),
        });
    }
    entries
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
            format_name_of(&a.path)
                .cmp(format_name_of(&b.path))
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

fn format_name_of(path: &Path) -> &'static str {
    path.extension()
        .map(|extension| extension.to_string_lossy().to_lowercase())
        .and_then(|extension| decode::format_name_for_extension(&extension))
        .unwrap_or("")
}

struct DecodeJob {
    path: PathBuf,
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
        path: PathBuf,
        file_size: u64,
        cancellation: Arc<AtomicBool>,
        immediate: bool,
    ) {
        let mut queue = self.shared.queue.lock().expect("decode queue poisoned");
        let job = DecodeJob {
            path,
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

    fn promote(&self, path: &Path) {
        let mut queue = self.shared.queue.lock().expect("decode queue poisoned");
        if let Some(position) = queue.iter().position(|job| paths_equal(&job.path, path))
            && let Some(job) = queue.remove(position)
        {
            queue.push_front(job);
        }
    }

    /// Removes queued jobs outside the relevant set; running jobs are unaffected.
    fn remove_queued_except(&self, relevant: &HashSet<PathBuf>) -> Vec<PathBuf> {
        let mut queue = self.shared.queue.lock().expect("decode queue poisoned");
        let mut removed = Vec::new();
        queue.retain(|job| {
            if relevant.contains(&job.path) {
                true
            } else {
                removed.push(job.path.clone());
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
        if decode::is_raw_two_stage(&job.path)
            && let Some(preview) = decode::decode_raw_preview(&job.path, &job.cancellation)
        {
            post_completion(
                window,
                Box::new(DecodeCompletion {
                    path: job.path.clone(),
                    file_size: job.file_size,
                    stage: DecodeStage::Preview,
                    result: Ok(Arc::new(preview)),
                }),
            );
        }
        let result = decode::decode_file(&job.path, &job.cancellation).map(Arc::new);
        post_completion(
            window,
            Box::new(DecodeCompletion {
                path: job.path,
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
