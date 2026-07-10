//! 로드 상태 머신 · 폴더 목록·정렬·이동 · 프리로드 캐시 · 디코드 스레드 풀
//! (SPEC §4, PORTING_PLAN §2 스레딩 모델 — 캐시 예산·해제는 mpv demux 정책 참고)

use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::c_void;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_HIDDEN;
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};
use windows::Win32::UI::Shell::StrCmpLogicalW;
use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_APP};
use windows::core::PCWSTR;

use super::decode::{self, DecodeError, DecodedImage};

/// 디코드 완료 통지 — lparam = Box<DecodeCompletion> 포인터 (PORTING_PLAN §2)
pub const WM_APP_DECODE_COMPLETE: u32 = WM_APP + 1;

/// 프리로드 모드 0/1/2 → (거리, 예산 바이트) (SPEC §4.5)
const PRELOAD_SPECIFICATIONS: [(usize, u64); 3] =
    [(0, 0), (1, 250 * 1024 * 1024), (4, 2 * 1024 * 1024 * 1024)];

/// 목록 신선도 — 마지막 수집 3초 경과 시 이동 전 재수집 (SPEC §4.3)
const FOLDER_LIST_FRESHNESS: Duration = Duration::from_secs(3);

/// ImageCore가 소비하는 옵션 부분집합 — 설정 브로드캐스트로 갱신 (SPEC §8.2)
#[derive(Clone, PartialEq)]
pub struct CoreOptions {
    pub sort_mode: SortMode,
    pub sort_descending: bool,
    /// 0=off / 1=인접 / 2=확장 (SPEC §4.5)
    pub preloading_mode: usize,
    pub loop_folders_enabled: bool,
    pub skip_hidden: bool,
    pub allow_mime_content_detection: bool,
}

/// 정렬 모드 (SPEC §4.3)
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SortMode {
    Name,
    Modified,
    Created,
    Size,
    Type,
    Random,
}

impl SortMode {
    /// 설정값 `sortmode`(0~5) → 모드, 범위 밖은 기본(이름)
    pub fn from_setting(value: u32) -> Self {
        match value {
            1 => Self::Modified,
            2 => Self::Created,
            3 => Self::Size,
            4 => Self::Type,
            5 => Self::Random,
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
    /// StrCmpLogicalW용 널 종단 파일명 (SPEC §4.3 자연 정렬, P15)
    wide_name: Vec<u16>,
    file_size: u64,
    modified: SystemTime,
    created: SystemTime,
}

/// 디코드 결과 단계 — RAW 2단계 로드의 프리뷰는 표시만 하고 로드를 종결하지 않는다 (SPEC §4.1)
pub enum DecodeStage {
    /// RAW 임베디드 프리뷰 — 캐시·in_flight·pending 불변, 풀 디코드가 이어짐
    Preview,
    Final,
}

/// 워커 → UI 완료 통지 페이로드 (WM_APP_DECODE_COMPLETE의 lparam)
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
    folder_scanned_at: Option<Instant>,
    /// 진행 중 로드의 표시 대상 — 교체가 곧 이전 로드 무효화(세대 규칙, SPEC §4.1).
    /// 프리로드 중 파일로 이동하면 경로 일치로 그 완료를 그대로 소비한다.
    pending_display: Option<PathBuf>,
    /// 큐 대기·디코드 중 경로 — 중복 요청 방지 (SPEC §4.5)
    in_flight: HashSet<PathBuf>,
    /// 키 = 절대경로, 항목에 파일 크기 보존(캐시 키 = 경로+크기, SPEC §4.1)
    cache: HashMap<PathBuf, CacheEntry>,
    pub current: Option<CurrentImage>,
    /// 디코드 실패 보존 — 폴더 목록은 유지 (SPEC §4.2, 오버레이 표시는 R4)
    pub load_error: Option<(PathBuf, DecodeError)>,
}

impl ImageCore {
    pub fn new(window: HWND, options: CoreOptions) -> Self {
        Self {
            pool: DecodePool::new(window.0 as isize),
            options,
            folder_directory: None,
            entries: Vec::new(),
            folder_scanned_at: None,
            pending_display: None,
            in_flight: HashSet::new(),
            cache: HashMap::new(),
            current: None,
            load_error: None,
        }
    }

    /// 설정 브로드캐스트 수신 (SPEC §8.2) — 정렬 변경이면 재수집, 프리로드 변경이면
    /// 예산 재적용. 현재 표시 이미지는 흐트러뜨리지 않는다 (§2 핵심 계약).
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

    /// 현재 파일의 폴더 내 위치 (1-기반, 총 개수) — 타이틀바 모드 2 "i/n" (SPEC §6.1)
    pub fn folder_position(&self) -> Option<(usize, usize)> {
        let current = self.current.as_ref()?;
        let index = self.position_of(&current.path)?;
        Some((index + 1, self.entries.len()))
    }

    pub fn has_folder_entries(&self) -> bool {
        !self.entries.is_empty()
    }

    /// 표시 대기 중 로드 존재 여부 — 지연 첫 표시 판단용 (SPEC §6.1)
    pub fn is_load_pending(&self) -> bool {
        self.pending_display.is_some()
    }

    /// Reload File — 캐시 무효화 후 현재 파일 재로드 (SPEC §5.1 reloadfile)
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

    /// 경로 인자·탐색 공용 진입 — 디렉터리면 목록 구성 후 첫 파일 (SPEC §4.1).
    /// 반환 = 표시 이미지가 동기적으로 바뀌었는지(캐시 히트).
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
                trace(|| format!("load {} failed: {error}", path.display()));
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
            trace(|| format!("load {} cache-hit", path.display()));
            self.current = Some(CurrentImage {
                path: path.to_path_buf(),
                image: entry.image.clone(),
            });
            self.pending_display = None;
            self.load_error = None;
            self.preload_neighbors();
            return true;
        }
        trace(|| format!("load {} queued", path.display()));
        self.pending_display = Some(path.to_path_buf());
        if self.in_flight.insert(path.to_path_buf()) {
            self.pool.submit(path.to_path_buf(), file_size, true);
        } else {
            // 프리로드로 이미 큐에 있으면 앞으로 승격 — 새 로드 우선 (SPEC §4.1)
            self.pool.promote(path);
        }
        false
    }

    /// First/Previous/Next/Last (SPEC §4.4) — 부재 파일 건너뜀·동일 파일 무시.
    /// 반환: None = 이동 없음(대상 없음·폴더 끝·동일 파일), Some(표시 동기 변경 여부).
    pub fn navigate(&mut self, command: NavigationCommand) -> Option<bool> {
        self.refresh_folder_if_stale();
        let current_path = self.navigation_anchor();
        let target = self.navigation_target(command)?;
        if current_path.is_some_and(|current| paths_equal(&current, &target)) {
            return None; // 같은 파일 재이동 무시
        }
        Some(self.load_file(&target))
    }

    /// 이동 대상 사전 계산 — 삭제 후 이동(afterdelete)용 (SPEC §6.4)
    pub fn peek_navigation_target(&mut self, command: NavigationCommand) -> Option<PathBuf> {
        self.refresh_folder_if_stale();
        self.navigation_target(command)
    }

    /// 폴더 목록 강제 재수집 — 삭제·rename 직후 (SPEC §4.3)
    pub fn refresh_folder(&mut self) {
        if let Some(directory) = self.folder_directory.clone() {
            self.rescan_folder(&directory);
        }
    }

    /// 위치 기준: 진행 중 로드 → 에러 파일(에러에서도 이동 가능, SPEC §4.2) → 현재 표시
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

    /// 워커 완료 수신 (WM_APP_DECODE_COMPLETE) — 반환 = 표시 상태 변경 여부
    pub fn on_decode_complete(&mut self, completion: DecodeCompletion) -> bool {
        if matches!(completion.stage, DecodeStage::Preview) {
            // RAW 프리뷰 — 표시 대상이면 먼저 보여주고 풀 디코드 완료를 계속 기다린다
            // (SPEC §4.1). 캐시 미삽입, in_flight·pending 불변 — 폐기는 경로 불일치뿐.
            let is_pending = self
                .pending_display
                .as_deref()
                .is_some_and(|pending| paths_equal(pending, &completion.path));
            if is_pending && let Ok(image) = completion.result {
                trace(|| format!("preview {} displayed", completion.path.display()));
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
                    trace(|| format!("decoded {} displayed", completion.path.display()));
                    self.current = Some(CurrentImage {
                        path: completion.path,
                        image,
                    });
                    self.pending_display = None;
                    self.load_error = None;
                    self.preload_neighbors();
                    true
                } else {
                    trace(|| format!("decoded {} cached", completion.path.display()));
                    self.evict_cache();
                    false
                }
            }
            Err(error) => {
                trace(|| {
                    format!(
                        "decode-error {} 0x{:08X} {}",
                        completion.path.display(),
                        error.code,
                        error.message
                    )
                });
                if is_pending {
                    self.pending_display = None;
                    self.load_error = Some((completion.path, error));
                }
                is_pending
            }
        }
    }

    // ── 폴더 목록 (SPEC §4.3) ───────────────────────────────────────────────

    fn rescan_folder(&mut self, directory: &Path) {
        // 랜덤 정렬은 폴더가 바뀔 때만 재셔플 (SPEC §4.3) — 같은 폴더 재수집이면
        // 기존 순서 보존(신규 파일은 뒤)
        let preserved_order: HashMap<PathBuf, usize> = if self.options.sort_mode == SortMode::Random
            && self.folder_directory.as_deref() == Some(directory)
        {
            self.entries
                .iter()
                .enumerate()
                .map(|(index, entry)| (entry.path.clone(), index))
                .collect()
        } else {
            HashMap::new()
        };
        let mut entries = scan_folder(directory, &self.options);
        sort_entries(&mut entries, &self.options);
        if !preserved_order.is_empty() {
            entries.sort_by_key(|entry| {
                preserved_order
                    .get(&entry.path)
                    .copied()
                    .unwrap_or(usize::MAX)
            });
        }
        self.entries = entries;
        self.folder_directory = Some(directory.to_path_buf());
        self.folder_scanned_at = Some(Instant::now());
        trace(|| {
            format!(
                "folder {} entries={}",
                directory.display(),
                self.entries.len()
            )
        });
    }

    /// 마지막 수집 3초 경과 or 현재 파일 소실 시 재수집 (SPEC §4.3)
    fn refresh_folder_if_stale(&mut self) {
        let Some(directory) = self.folder_directory.clone() else {
            return;
        };
        let stale = self
            .folder_scanned_at
            .is_none_or(|at| at.elapsed() > FOLDER_LIST_FRESHNESS)
            || self
                .current
                .as_ref()
                .is_some_and(|current| !current.path.is_file());
        if stale {
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

    /// 현재 인덱스에서 방향으로 1개 이동 — 순환(loop) 반영, 부재 파일 건너뜀
    fn step_existing_entry(&self, current: Option<usize>, direction: isize) -> Option<PathBuf> {
        let length = self.entries.len() as isize;
        let start = current.map_or(0, |index| index as isize);
        let mut index = start;
        for _ in 0..length {
            index += direction;
            if self.options.loop_folders_enabled {
                index = index.rem_euclid(length);
            } else if !(0..length).contains(&index) {
                return None; // 끝에서 정지 (SPEC §4.4)
            }
            let entry = &self.entries[index as usize];
            if entry.path.is_file() {
                return Some(entry.path.clone());
            }
        }
        None
    }

    // ── 프리로드 캐시 (SPEC §4.5 — 예산·해제는 mpv demux 정책 참고) ─────────

    fn preload_neighbors(&mut self) {
        let (distance, budget) = PRELOAD_SPECIFICATIONS[self.options.preloading_mode.min(2)];
        if distance == 0 {
            self.cache.clear(); // 모드 0 = off: 캐시 비움
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
                    // 파일 크기가 캐시 상한의 절반 초과면 프리로드 제외 (SPEC §4.5)
                    if entry.file_size > budget / 2
                        || self.in_flight.contains(&entry.path)
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
                    trace(|| format!("preload {}", entry.path.display()));
                    self.in_flight.insert(entry.path.clone());
                    self.pool.submit(entry.path.clone(), entry.file_size, false);
                }
            }
        }
        self.evict_cache();
    }

    /// 예산 초과 시 현재 위치에서 링 거리가 먼 항목부터 해제
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
            trace(|| format!("evict {}", path.display()));
            self.cache.remove(&path);
            total -= cost;
        }
    }
}

/// 순환 설정 반영 이웃 인덱스 (SPEC §4.5 — 루프 켜짐이면 랩어라운드)
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

/// Windows 경로 동등 비교 — 대소문자 무시 (SPEC §4.3 인덱스 매칭)
fn paths_equal(a: &Path, b: &Path) -> bool {
    a == b
        || a.as_os_str()
            .to_string_lossy()
            .eq_ignore_ascii_case(&b.as_os_str().to_string_lossy())
}

// ── 폴더 스캔·정렬 (SPEC §4.3) ──────────────────────────────────────────────

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
            continue; // macOS 메타파일 제외 (SPEC §4.3)
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
        // 랜덤 재셔플 규칙(폴더 변경 시에만, SPEC §4.3)은 rescan_folder가 순서 보존으로 처리
        SortMode::Random => shuffle(entries),
    }
    if options.sort_descending && options.sort_mode != SortMode::Random {
        entries.reverse();
    }
}

/// 자연 정렬 — 탐색기와 동일한 StrCmpLogicalW (SPEC §4.3, P15)
fn compare_natural_names(a: &FolderEntry, b: &FolderEntry) -> std::cmp::Ordering {
    let result =
        unsafe { StrCmpLogicalW(PCWSTR(a.wide_name.as_ptr()), PCWSTR(b.wide_name.as_ptr())) };
    result.cmp(&0)
}

/// 타입 정렬 키 — 디코더 레지스트리 포맷명 (SPEC §4.3)
fn format_name_of(path: &Path) -> &'static str {
    path.extension()
        .map(|extension| extension.to_string_lossy().to_lowercase())
        .and_then(|extension| decode::format_name_for_extension(&extension))
        .unwrap_or("")
}

/// 의존성 없는 Fisher–Yates + xorshift (P3 — 유틸 crate 금지)
fn shuffle(entries: &mut [FolderEntry]) {
    let mut state = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0x9E3779B9, |duration| duration.as_nanos() as u64)
        | 1;
    for index in (1..entries.len()).rev() {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        entries.swap(index, (state % (index as u64 + 1)) as usize);
    }
}

// ── 디코드 스레드 풀 (PORTING_PLAN §2 — std::thread + 큐 + PostMessageW) ────

struct DecodeJob {
    path: PathBuf,
    file_size: u64,
}

struct PoolShared {
    queue: Mutex<VecDeque<DecodeJob>>,
    available: Condvar,
}

/// 소형 워커 풀 — 종료 처리는 없다(프로세스 종료 시 소멸, 뷰어 수명과 동일)
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

    /// immediate = 현재 로드(큐 앞), 아니면 프리로드(큐 뒤) — 새 로드 우선
    fn submit(&self, path: PathBuf, file_size: u64, immediate: bool) {
        let mut queue = self.shared.queue.lock().expect("decode queue poisoned");
        let job = DecodeJob { path, file_size };
        if immediate {
            queue.push_front(job);
        } else {
            queue.push_back(job);
        }
        drop(queue);
        self.shared.available.notify_one();
    }

    /// 큐에 대기 중인 프리로드를 현재 로드로 승격
    fn promote(&self, path: &Path) {
        let mut queue = self.shared.queue.lock().expect("decode queue poisoned");
        if let Some(position) = queue.iter().position(|job| paths_equal(&job.path, path))
            && let Some(job) = queue.remove(position)
        {
            queue.push_front(job);
        }
    }
}

fn worker_loop(shared: &PoolShared, window: isize) {
    // 워커 = COM MTA, UI 스레드 = STA (PORTING_PLAN §2·§3 매핑)
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
        // RAW 2단계: 동일 워커에서 프리뷰 → 풀 디코드 순차 — 레이스 자체가 없다 (SPEC §4.1)
        if decode::is_raw_two_stage(&job.path)
            && let Some(preview) = decode::decode_raw_preview(&job.path)
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
        let result = decode::decode_file(&job.path).map(Arc::new);
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
        // 창 소멸 등으로 전달 실패 — 결과 폐기
        drop(unsafe { Box::from_raw(pointer) });
    }
}

/// R2 검증 트레이스 (임시 — wine은 합성 키 입력 불가라 stderr 로그로 캐시 히트·
/// 디코드 흐름을 확인한다. RIV_R2_TRACE=1로 활성화, R3 입력 구현 후 제거)
fn trace(message: impl FnOnce() -> String) {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    if *ENABLED.get_or_init(|| std::env::var_os("RIV_R2_TRACE").is_some()) {
        eprintln!("[riv] {}", message());
    }
}
