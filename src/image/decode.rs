//! 디코더 레지스트리 · 포맷 디스패치 · WIC 어댑터 (PORTING_PLAN §5, SPEC §4.2·§10)
//!
//! 어댑터는 디코드 워커 스레드(COM MTA)에서 호출된다 — WIC 팩토리·디코더는
//! 스레드별 생성(thread_local). R2는 WIC 어댑터 1개만 등록하며, R5에서 내장
//! fallback(APNG·애니 WebP·EXR·SVG·HEIF)이 추가될 때 어댑터 선택 필드가 생긴다.

use std::cell::RefCell;
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;
use std::sync::OnceLock;

use windows::Win32::Foundation::{GENERIC_READ, WINCODEC_ERR_COMPONENTNOTFOUND};
use windows::Win32::Graphics::Imaging::{
    CLSID_WICImagingFactory, GUID_WICPixelFormat32bppPBGRA, GUID_WICPixelFormat64bppPRGBAHalf,
    IWICBitmapDecoder, IWICBitmapFrameDecode, IWICBitmapSource, IWICColorContext,
    IWICImagingFactory, IWICMetadataQueryReader, IWICPixelFormatInfo, WICBitmapDitherTypeNone,
    WICBitmapInterpolationModeFant, WICBitmapPaletteTypeCustom, WICBitmapTransformFlipHorizontal,
    WICBitmapTransformFlipVertical, WICBitmapTransformOptions, WICBitmapTransformRotate90,
    WICBitmapTransformRotate180, WICBitmapTransformRotate270, WICColorContextProfile,
    WICDecodeMetadataCacheOnDemand,
};
use windows::Win32::System::Com::StructuredStorage::{
    PROPVARIANT, PropVariantClear, PropVariantToDouble, PropVariantToFileTime,
    PropVariantToStringAlloc, PropVariantToUInt32,
};
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};
use windows::core::{HSTRING, Interface, PCWSTR, w};

/// 디코드 결과 프레임 — `PixelStorage` 형식, 캔버스 전체로 합성 완료 (SPEC §3.1·§4.6)
pub struct Frame {
    pub pixels: Vec<u8>,
    /// 정지 이미지는 0 — 애니메이션 스케줄러가 소비 (SPEC §4.6)
    pub delay_milliseconds: u32,
}

/// 소스 픽셀 저장 형식 (SPEC §7 Q6) — 렌더러의 비트맵 포맷·스트라이드 결정
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PixelStorage {
    /// premultiplied BGRA 8bpc — 기본 (SPEC §3.1)
    Bgra8,
    /// premultiplied RGBA FP16 — 고심도 소스 (WIC 64bppPRGBAHalf, Q6 2026-07-11)
    RgbaHalf,
}

impl PixelStorage {
    pub fn bytes_per_pixel(self) -> u32 {
        match self {
            Self::Bgra8 => 4,
            Self::RgbaHalf => 8,
        }
    }
}

pub struct DecodedImage {
    /// 논리 크기 = 원본 픽셀 — 표시 크기 기준 (DP3 다운스케일과 무관, SPEC §3.4)
    pub width: u32,
    pub height: u32,
    /// 픽셀 버퍼 크기 — 디바이스 한계 초과 시에만 논리 크기보다 작다 (DP3)
    pub pixel_width: u32,
    pub pixel_height: u32,
    /// 디코더 레지스트리 포맷명 — 정보 오버레이 소스 (SPEC §3.6)
    pub format_name: &'static str,
    /// 임베디드 색 프로파일(ICC 바이트) — ColorManagement 이펙트 소스, 없으면 sRGB 가정 (SPEC §7)
    pub icc_profile: Option<Vec<u8>>,
    /// EXIF 촬영 정보 (SPEC §3.6 확장, 2026-07-11) — WIC 경로만, 전 필드 부재면 None
    pub exif: Option<ExifInfo>,
    pub storage: PixelStorage,
    /// HDR 소스(PQ/HLG)의 콘텐츠 피크 휘도(nits) — HdrToneMap 게이트·InputMaxLuminance
    /// (SPEC §7 Q6). SDR 색공간 소스는 고심도여도 None(톤맵 비대상)
    pub peak_luminance_nits: Option<f32>,
    pub frames: Vec<Frame>,
}

/// EXIF 촬영 정보 — WIC 포토 메타데이터 정책 조회 (System.Photo.*, System.Rating).
/// 노출 포맷 = JPEG·TIFF·HEIC·RAW·JXL(실측) — 그 외·fallback 디코더는 필드 부재.
pub struct ExifInfo {
    pub date_taken: Option<std::time::SystemTime>,
    /// System.Rating 원값 (1~99 — 별 환산은 표시 측)
    pub rating: Option<u32>,
    pub camera_maker: Option<String>,
    pub camera_model: Option<String>,
    pub f_stop: Option<f64>,
    pub exposure_time_seconds: Option<f64>,
    pub iso_speed: Option<u32>,
    pub exposure_bias: Option<f64>,
    pub focal_length_millimeters: Option<f64>,
    pub max_aperture: Option<f64>,
    /// EXIF MeteringMode 원값 (표시 문자열 매핑은 표시 측)
    pub metering_mode: Option<u32>,
    /// EXIF Flash 비트필드 원값
    pub flash: Option<u32>,
    pub focal_length_35mm: Option<u32>,
}

impl ExifInfo {
    fn any_present(&self) -> bool {
        self.date_taken.is_some()
            || self.rating.is_some()
            || self.camera_maker.is_some()
            || self.camera_model.is_some()
            || self.f_stop.is_some()
            || self.exposure_time_seconds.is_some()
            || self.iso_speed.is_some()
            || self.exposure_bias.is_some()
            || self.focal_length_millimeters.is_some()
            || self.max_aperture.is_some()
            || self.metering_mode.is_some()
            || self.flash.is_some()
            || self.focal_length_35mm.is_some()
    }
}

impl DecodedImage {
    /// 캐시 비용 = 디코드 픽셀 메모리 (SPEC §4.5)
    pub fn pixel_bytes(&self) -> usize {
        self.frames.iter().map(|frame| frame.pixels.len()).sum()
    }
}

/// 디코드 실패 — 에러 코드·문자열 보존 (SPEC §4.2, 오버레이 표시는 R4)
#[derive(Clone)]
pub struct DecodeError {
    pub code: i32,
    pub message: String,
    /// WIC 확장 부재 시 설치 안내에 쓸 Store 확장 이름 (SPEC §10 — 문구만, 링크 없음)
    pub store_extension: Option<&'static str>,
}

impl From<windows::core::Error> for DecodeError {
    fn from(error: windows::core::Error) -> Self {
        Self {
            code: error.code().0,
            message: error.message(),
            store_extension: None,
        }
    }
}

/// 프레임 구성 의미 — 다중 프레임을 애니메이션으로 볼지, 크기 변형으로 볼지
enum FrameSemantics {
    /// 첫 프레임만 사용 (PNG·JPEG·BMP)
    Single,
    /// 프레임 = 애니메이션 시퀀스 (GIF — APNG·애니 WebP는 R5 fallback에서 합류)
    Animation,
    /// 프레임 = 해상도 변형 — 최대 해상도 선택 (ICO)
    SizeVariants,
}

/// D3D11 FL11·D2D 비트맵 한계 — 초과 시 업로드 전 다운스케일 (SPEC §3.4 DP3)
const MAXIMUM_TEXTURE_DIMENSION: u32 = 16384;

/// 매직 시그니처 1개 — (오프셋, 바이트) 전부 일치 시 매치
type MagicSignature = &'static [(usize, &'static [u8])];

/// 디코드 어댑터 선택 (PORTING_PLAN §5) — WIC 또는 내장 fallback
enum Adapter {
    Wic,
    /// WIC + RAW 2단계 로드: 임베디드 프리뷰 먼저 표시 (SPEC §4.1)
    WicRawTwoStage,
    /// APNG — WIC PNG는 단일 프레임만, acTL 프로빙으로 분기 (`png` crate)
    Apng,
    /// SVG/SVGZ — WIC 완전 미지원, 최대 모니터 긴 변으로 래스터화 (`resvg`, SPEC §4.2)
    Svg,
    /// 애니메이션 WebP — WIC 미지원, VP8X ANIM 플래그 프로빙으로 분기 (libwebp 정적)
    WebPAnimation,
    /// EXR — OpenEXR C++ 심 (성능 우선 선택 2026-07-10)
    Exr,
    /// HEIF — WIC 우선, 코덱 부재(COMPONENTNOTFOUND) 시 안내 없이 libheif로 전환
    HeifWithWicPreferred,
}

pub struct FormatDescriptor {
    /// 포맷명 — 타입 정렬·정보 오버레이·연결 UI 그룹의 단일 소스 (SPEC §10)
    pub name: &'static str,
    pub extensions: &'static [&'static str],
    /// 대안 시그니처 목록(OR) — 빈 배열 = 매직 프로빙 비대상
    magic: &'static [MagicSignature],
    semantics: FrameSemantics,
    adapter: Adapter,
    /// WIC 코덱 부재(COMPONENTNOTFOUND) 시 설치 안내할 Store 확장 이름 (SPEC §10)
    store_extension: Option<&'static str>,
}

/// 디코더 레지스트리 — 지원 확장자 목록·파일 필터·연결 그룹은 전부 여기서 파생
/// (PORTING_PLAN §5 — 수작업 테이블 금지). R2는 WIC 1차 포맷.
/// APNG는 확장자 또는 PNG 매직 + acTL 프로빙으로 분기(`refine_by_content`) —
/// R2에서는 WIC PNG로 첫 프레임만 표시하고, R5에서 `png` crate fallback으로 대체.
static REGISTRY: &[FormatDescriptor] = &[
    FormatDescriptor {
        name: "PNG",
        extensions: &["png"],
        magic: &[&[(0, b"\x89PNG\r\n\x1a\n")]],
        semantics: FrameSemantics::Single,
        adapter: Adapter::Wic,
        store_extension: None,
    },
    FormatDescriptor {
        name: "APNG",
        extensions: &["apng"],
        magic: &[],
        semantics: FrameSemantics::Animation,
        adapter: Adapter::Apng,
        store_extension: None,
    },
    FormatDescriptor {
        name: "SVG",
        extensions: &["svg", "svgz"],
        magic: &[&[(0, b"<svg")], &[(0, b"<?xml")]],
        semantics: FrameSemantics::Single,
        adapter: Adapter::Svg,
        store_extension: None,
    },
    FormatDescriptor {
        name: "JPEG",
        extensions: &["jpg", "jpeg", "jpe", "jfif"],
        magic: &[&[(0, b"\xFF\xD8\xFF")]],
        semantics: FrameSemantics::Single,
        adapter: Adapter::Wic,
        store_extension: None,
    },
    FormatDescriptor {
        name: "GIF",
        extensions: &["gif"],
        magic: &[&[(0, b"GIF8")]],
        semantics: FrameSemantics::Animation,
        adapter: Adapter::Wic,
        store_extension: None,
    },
    // WIC WebP는 애니메이션 미지원(2026-07-10 확인) — 정지 전용, 애니메이션은 R5에서
    // VP8X ANIM 플래그 프로빙 분기 + libwebp 정적 fallback (PORTING_PLAN §5)
    FormatDescriptor {
        name: "WebP",
        extensions: &["webp"],
        magic: &[&[(0, b"RIFF"), (8, b"WEBP")]],
        semantics: FrameSemantics::Single,
        adapter: Adapter::Wic,
        store_extension: Some("WebP Image Extensions"),
    },
    FormatDescriptor {
        name: "BMP",
        extensions: &["bmp", "dib"],
        magic: &[&[(0, b"BM")]],
        semantics: FrameSemantics::Single,
        adapter: Adapter::Wic,
        store_extension: None,
    },
    FormatDescriptor {
        name: "ICO",
        extensions: &["ico"],
        magic: &[&[(0, &[0x00, 0x00, 0x01, 0x00])]],
        semantics: FrameSemantics::SizeVariants,
        adapter: Adapter::Wic,
        store_extension: None,
    },
    FormatDescriptor {
        name: "TIFF",
        extensions: &["tif", "tiff"],
        magic: &[&[(0, b"II*\x00")], &[(0, b"MM\x00*")]],
        semantics: FrameSemantics::Single,
        adapter: Adapter::Wic,
        store_extension: None,
    },
    // HEIF는 WIC 부재 시 내장 fallback(libheif)으로 안내 없이 전환 (PORTING_PLAN §5)
    FormatDescriptor {
        name: "HEIF",
        extensions: &["heic", "heif", "hif"],
        magic: &[
            &[(4, b"ftypheic")],
            &[(4, b"ftypheix")],
            &[(4, b"ftypmif1")],
            &[(4, b"ftypmsf1")],
            &[(4, b"ftyphevc")],
        ],
        semantics: FrameSemantics::Single,
        adapter: Adapter::HeifWithWicPreferred,
        store_extension: None,
    },
    FormatDescriptor {
        name: "EXR",
        extensions: &["exr"],
        magic: &[&[(0, b"\x76\x2F\x31\x01")]],
        semantics: FrameSemantics::Single,
        adapter: Adapter::Exr,
        store_extension: None,
    },
    FormatDescriptor {
        name: "AVIF",
        extensions: &["avif"],
        magic: &[&[(4, b"ftypavif")], &[(4, b"ftypavis")]],
        semantics: FrameSemantics::Single,
        adapter: Adapter::Wic,
        store_extension: Some("AV1 Video Extension"),
    },
    FormatDescriptor {
        name: "JPEG XL",
        extensions: &["jxl"],
        magic: &[
            &[(0, b"\x00\x00\x00\x0CJXL \r\n\x87\n")],
            &[(0, b"\xFF\x0A")],
        ],
        semantics: FrameSemantics::Single,
        adapter: Adapter::Wic,
        store_extension: Some("JPEG XL Image Extension (Windows 11 24H2+)"),
    },
    // RAW는 확장자 전용(컨테이너 매직이 TIFF와 겹침 — DNG·CR2 등), 2단계 로드 (SPEC §4.1)
    FormatDescriptor {
        name: "RAW",
        extensions: &[
            "arw", "cr2", "cr3", "crw", "dng", "erf", "kdc", "mrw", "nef", "nrw", "orf", "pef",
            "raf", "raw", "rw2", "rwl", "sr2", "srw", "x3f",
        ],
        magic: &[],
        semantics: FrameSemantics::Single,
        adapter: Adapter::WicRawTwoStage,
        store_extension: Some("Raw Image Extension"),
    },
];

/// (포맷명, 확장자 목록) — 파일 연결 트리의 동적 그룹핑 소스 (SPEC §8.3 —
/// 수작업 테이블 금지). 정렬·헤더 생략 규칙은 UI 측 책임.
pub fn format_groups() -> impl Iterator<Item = (&'static str, &'static [&'static str])> {
    REGISTRY
        .iter()
        .map(|descriptor| (descriptor.name, descriptor.extensions))
}

/// 지원 확장자 전체 — 파일 필터·연결 UI 파생 소스 (SPEC §10 — 수작업 테이블 금지)
pub fn supported_extensions() -> impl Iterator<Item = &'static str> {
    REGISTRY
        .iter()
        .flat_map(|descriptor| descriptor.extensions.iter().copied())
}

/// 폴더 목록 확장자 매칭 (SPEC §4.3) — 소문자 확장자 기준
pub fn is_supported_extension(extension: &str) -> bool {
    REGISTRY
        .iter()
        .any(|descriptor| descriptor.extensions.contains(&extension))
}

/// 타입 정렬용 포맷명 — 확장자만으로 조회(디코드·프로빙 없음) (SPEC §4.3)
pub fn format_name_for_extension(extension: &str) -> Option<&'static str> {
    descriptor_for_extension(extension).map(|descriptor| descriptor.name)
}

fn descriptor_for_extension(extension: &str) -> Option<&'static FormatDescriptor> {
    REGISTRY
        .iter()
        .find(|descriptor| descriptor.extensions.contains(&extension))
}

/// 매직 프로빙 — `allowmimecontentdetection`의 판별 경로 (PORTING_PLAN §5)
pub fn probe_file(path: &Path) -> Option<&'static FormatDescriptor> {
    let header = read_header(path)?;
    probe_magic(&header).map(|descriptor| refine_by_content(descriptor, &header))
}

fn probe_magic(header: &[u8]) -> Option<&'static FormatDescriptor> {
    REGISTRY.iter().find(|descriptor| {
        descriptor.magic.iter().any(|signature| {
            signature.iter().all(|(offset, bytes)| {
                header
                    .get(*offset..offset + bytes.len())
                    .is_some_and(|slice| slice == *bytes)
            })
        })
    })
}

/// 애니메이션 WebP 전용 디스크립터 — 확장자·매직 조회 비대상, VP8X ANIM 프로빙
/// 세분화(`refine_by_content`)로만 도달한다. 포맷명·확장자 파생은 정지 WebP 항목 소유.
static ANIMATED_WEBP: FormatDescriptor = FormatDescriptor {
    name: "WebP",
    extensions: &[],
    magic: &[],
    semantics: FrameSemantics::Animation,
    adapter: Adapter::WebPAnimation,
    store_extension: None,
};

/// 내용 기반 세분화 — PNG + acTL = APNG, WebP + VP8X ANIM 플래그 = 애니 WebP
fn refine_by_content(
    descriptor: &'static FormatDescriptor,
    header: &[u8],
) -> &'static FormatDescriptor {
    if descriptor.name == "PNG" && png_has_animation_control(header) {
        return descriptor_for_extension("apng").unwrap_or(descriptor);
    }
    if descriptor.name == "WebP" && webp_has_animation_flag(header) {
        return &ANIMATED_WEBP;
    }
    descriptor
}

/// VP8X 확장 청크의 ANIM 플래그(비트 0x02) — 애니메이션 WebP 판별 (PORTING_PLAN §5)
fn webp_has_animation_flag(header: &[u8]) -> bool {
    header.get(12..16) == Some(b"VP8X") && header.get(20).is_some_and(|flags| flags & 0x02 != 0)
}

/// PNG 청크 순회로 IDAT 이전의 acTL 존재 확인 (헤더 버퍼 범위 내)
fn png_has_animation_control(header: &[u8]) -> bool {
    let mut offset = 8; // PNG 시그니처
    while let Some(chunk_header) = header.get(offset..offset + 8) {
        let length = u32::from_be_bytes(chunk_header[..4].try_into().unwrap()) as usize;
        let chunk_type = &chunk_header[4..8];
        match chunk_type {
            b"acTL" => return true,
            b"IDAT" | b"IEND" => return false,
            _ => offset += 8 + length + 4, // 헤더 + 데이터 + CRC
        }
    }
    false
}

/// 디스패치: 확장자 우선 + 매직 프로빙 (PORTING_PLAN §5).
/// 확장자 불일치·부재 파일은 매직으로 판별. WIC 디코더 자체도 내용 기반으로
/// 선택되므로(CreateDecoderFromFilename) 여기 결과는 어댑터 선택·포맷명에 쓰인다.
fn descriptor_for_path(path: &Path) -> Option<&'static FormatDescriptor> {
    let header = read_header(path);
    let by_extension = path
        .extension()
        .map(|extension| extension.to_string_lossy().to_lowercase())
        .and_then(|extension| descriptor_for_extension(&extension));
    match (by_extension, header) {
        (Some(descriptor), Some(header)) => Some(refine_by_content(descriptor, &header)),
        (Some(descriptor), None) => Some(descriptor),
        (None, Some(header)) => {
            probe_magic(&header).map(|descriptor| refine_by_content(descriptor, &header))
        }
        (None, None) => None,
    }
}

/// 매직·acTL 프로빙에 충분한 선두 바이트 (acTL은 관례상 IHDR 직후)
fn read_header(path: &Path) -> Option<Vec<u8>> {
    let mut file = File::open(path).ok()?;
    let mut buffer = vec![0u8; 4096];
    let read_bytes = file.read(&mut buffer).ok()?;
    buffer.truncate(read_bytes);
    Some(buffer)
}

/// 디코드 진입점 (워커 스레드 전용 — COM MTA 전제)
pub fn decode_file(path: &Path) -> Result<DecodedImage, DecodeError> {
    let descriptor = descriptor_for_path(path);
    // 레지스트리 미등록 포맷도 WIC 내용 판별에 맡긴다
    let format_name = descriptor.map_or("Unknown", |descriptor| descriptor.name);
    let semantics = descriptor.map_or(&FrameSemantics::Single, |descriptor| &descriptor.semantics);
    let adapter = descriptor.map_or(&Adapter::Wic, |descriptor| &descriptor.adapter);
    match adapter {
        Adapter::Wic | Adapter::WicRawTwoStage => decode_with_wic(path, format_name, semantics)
            .map_err(|mut error| {
                // WIC 확장 부재 → 에러 오버레이에 설치 안내 문구 (SPEC §10 — 링크 없음)
                if error.code == WINCODEC_ERR_COMPONENTNOTFOUND.0
                    && let Some(descriptor) = descriptor
                {
                    error.store_extension = descriptor.store_extension;
                }
                error
            }),
        Adapter::Apng => decode_apng(path, format_name),
        Adapter::Svg => decode_svg(path, format_name),
        Adapter::WebPAnimation => super::fallback::decode_webp_animation(path, format_name),
        Adapter::Exr => super::fallback::decode_exr(path, format_name),
        // WIC 우선, 코덱 부재 시 안내 없이 내장 전환 (PORTING_PLAN §5)
        Adapter::HeifWithWicPreferred => {
            decode_with_wic(path, format_name, semantics).or_else(|error| {
                if error.code == WINCODEC_ERR_COMPONENTNOTFOUND.0 {
                    super::fallback::decode_heif(path, format_name)
                } else {
                    Err(error)
                }
            })
        }
    }
}

/// RAW 2단계 로드 대상 여부 — 워커가 프리뷰 → 풀 디코드 순차 수행 (SPEC §4.1)
pub fn is_raw_two_stage(path: &Path) -> bool {
    descriptor_for_path(path)
        .is_some_and(|descriptor| matches!(descriptor.adapter, Adapter::WicRawTwoStage))
}

/// RAW 임베디드 프리뷰 디코드 (SPEC §4.1) — 실패는 조용히 무시(풀 디코드가 이어짐)
pub fn decode_raw_preview(path: &Path) -> Option<DecodedImage> {
    let decoded = with_wic_factory(|factory| {
        let decoder = unsafe {
            factory.CreateDecoderFromFilename(
                &HSTRING::from(path.as_os_str()),
                None,
                GENERIC_READ,
                WICDecodeMetadataCacheOnDemand,
            )?
        };
        let preview =
            unsafe { decoder.GetPreview() }.or_else(|_| unsafe { decoder.GetThumbnail() })?;
        // 오리엔테이션·색 프로파일은 프레임 메타데이터 기준 — 풀 디코드와 동일하게 표시
        let frame = unsafe { decoder.GetFrame(0) }.ok();
        let orientation = frame.as_ref().map_or(1, exif_orientation);
        let icc_profile = frame
            .as_ref()
            .and_then(|frame| icc_profile_bytes(factory, frame));
        let exif = frame.as_ref().and_then(read_exif);
        let source = convert_to_pbgra(factory, &preview)?;
        let source = apply_orientation(factory, source, orientation)?;
        let (width, height) = source_size(&source)?;
        let (source, pixel_width, pixel_height) =
            downscale_to_device_limit(factory, source, width, height)?;
        let pixels = copy_pixels(&source, pixel_width, pixel_height, 4)?;
        Ok(DecodedFrames {
            width,
            height,
            pixel_width,
            pixel_height,
            icc_profile,
            exif,
            storage: PixelStorage::Bgra8,
            peak_luminance_nits: None,
            frames: vec![Frame {
                pixels,
                delay_milliseconds: 0,
            }],
        })
    })
    .ok()?;
    Some(DecodedImage {
        width: decoded.width,
        height: decoded.height,
        pixel_width: decoded.pixel_width,
        pixel_height: decoded.pixel_height,
        format_name: "RAW",
        icc_profile: decoded.icc_profile,
        exif: decoded.exif,
        storage: decoded.storage,
        peak_luminance_nits: decoded.peak_luminance_nits,
        frames: decoded.frames,
    })
}

thread_local! {
    /// 워커 스레드별 WIC 팩토리 (PORTING_PLAN §5 — 스레드별 디코더 생성)
    static WIC_FACTORY: RefCell<Option<IWICImagingFactory>> = const { RefCell::new(None) };
}

fn with_wic_factory<T>(
    operation: impl FnOnce(&IWICImagingFactory) -> windows::core::Result<T>,
) -> windows::core::Result<T> {
    WIC_FACTORY.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.is_none() {
            *slot = Some(unsafe {
                CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)?
            });
        }
        operation(slot.as_ref().expect("WIC factory initialized"))
    })
}

/// 어댑터 내부 결과 — 논리(원본) 크기와 픽셀 버퍼 크기 분리 (DP3)
struct DecodedFrames {
    width: u32,
    height: u32,
    pixel_width: u32,
    pixel_height: u32,
    icc_profile: Option<Vec<u8>>,
    exif: Option<ExifInfo>,
    storage: PixelStorage,
    peak_luminance_nits: Option<f32>,
    frames: Vec<Frame>,
}

/// WIC 어댑터: CreateDecoderFromFilename → 프레임 → IWICFormatConverter(PBGRA32)
/// (PORTING_PLAN §5). 애니메이션은 프레임 합성, ICO는 최대 해상도 선택.
fn decode_with_wic(
    path: &Path,
    format_name: &'static str,
    semantics: &FrameSemantics,
) -> Result<DecodedImage, DecodeError> {
    let decoded = with_wic_factory(|factory| {
        let decoder = unsafe {
            factory.CreateDecoderFromFilename(
                &HSTRING::from(path.as_os_str()),
                None,
                GENERIC_READ,
                WICDecodeMetadataCacheOnDemand,
            )?
        };
        let frame_count = unsafe { decoder.GetFrameCount()? }.max(1);
        match semantics {
            FrameSemantics::Animation if frame_count > 1 => {
                decode_animation(factory, &decoder, frame_count)
            }
            FrameSemantics::SizeVariants if frame_count > 1 => {
                decode_largest_frame(factory, &decoder, frame_count)
            }
            _ => decode_single_frame(factory, &decoder, 0),
        }
    })?;
    Ok(DecodedImage {
        width: decoded.width,
        height: decoded.height,
        pixel_width: decoded.pixel_width,
        pixel_height: decoded.pixel_height,
        format_name,
        icc_profile: decoded.icc_profile,
        exif: decoded.exif,
        storage: decoded.storage,
        peak_luminance_nits: decoded.peak_luminance_nits,
        frames: decoded.frames,
    })
}

/// DP3 — 디바이스 한계 초과 시 KeepAspectRatio 다운스케일 (WIC 스케일러, P15).
/// 표시 크기는 원본 기준 유지 — 호출자가 논리 크기를 따로 보존한다 (SPEC §3.4)
fn downscale_to_device_limit(
    factory: &IWICImagingFactory,
    source: IWICBitmapSource,
    width: u32,
    height: u32,
) -> windows::core::Result<(IWICBitmapSource, u32, u32)> {
    let longest = width.max(height);
    if longest <= MAXIMUM_TEXTURE_DIMENSION {
        return Ok((source, width, height));
    }
    let limit = u64::from(MAXIMUM_TEXTURE_DIMENSION);
    let scaled_width = (u64::from(width) * limit / u64::from(longest)).max(1) as u32;
    let scaled_height = (u64::from(height) * limit / u64::from(longest)).max(1) as u32;
    let scaler = unsafe { factory.CreateBitmapScaler()? };
    unsafe {
        scaler.Initialize(
            &source,
            scaled_width,
            scaled_height,
            WICBitmapInterpolationModeFant,
        )?
    };
    Ok((scaler.cast()?, scaled_width, scaled_height))
}

fn decode_single_frame(
    factory: &IWICImagingFactory,
    decoder: &IWICBitmapDecoder,
    index: u32,
) -> windows::core::Result<DecodedFrames> {
    let frame = unsafe { decoder.GetFrame(index)? };
    let orientation = exif_orientation(&frame);
    let icc_profile = icc_profile_bytes(factory, &frame);
    let exif = read_exif(&frame);
    // 고심도(채널 8비트 초과) 소스는 FP16으로 (SPEC §7 Q6) — 변환 실패 시 8bpc 폴백
    let (source, storage) = if frame_exceeds_8_bits_per_channel(factory, &frame) {
        match convert_pixel_format(factory, &frame.cast()?, &GUID_WICPixelFormat64bppPRGBAHalf) {
            Ok(source) => (source, PixelStorage::RgbaHalf),
            Err(_) => (
                convert_to_pbgra(factory, &frame.cast()?)?,
                PixelStorage::Bgra8,
            ),
        }
    } else {
        (
            convert_to_pbgra(factory, &frame.cast()?)?,
            PixelStorage::Bgra8,
        )
    };
    let source = apply_orientation(factory, source, orientation)?;
    let (width, height) = source_size(&source)?;
    let (source, pixel_width, pixel_height) =
        downscale_to_device_limit(factory, source, width, height)?;
    let pixels = copy_pixels(
        &source,
        pixel_width,
        pixel_height,
        storage.bytes_per_pixel(),
    )?;
    // HDR(PQ/HLG) 소스만 피크 휘도 산출 — HdrToneMap 게이트 (SPEC §7 Q6)
    let peak_luminance_nits = (storage == PixelStorage::RgbaHalf)
        .then(|| icc_profile.as_deref().and_then(icc_cicp_transfer_function))
        .flatten()
        .and_then(|transfer| peak_luminance_from_half_pixels(&pixels, transfer));
    Ok(DecodedFrames {
        width,
        height,
        pixel_width,
        pixel_height,
        icc_profile,
        exif,
        storage,
        peak_luminance_nits,
        frames: vec![Frame {
            pixels,
            delay_milliseconds: 0,
        }],
    })
}

/// 프레임 네이티브 픽셀 포맷의 채널당 비트 수가 8을 넘는지 — FP16 디코드 대상 판별
/// (SPEC §7 Q6). 조회 실패는 false(기본 8bpc 경로).
fn frame_exceeds_8_bits_per_channel(
    factory: &IWICImagingFactory,
    frame: &IWICBitmapFrameDecode,
) -> bool {
    (|| -> windows::core::Result<bool> {
        let format = unsafe { frame.GetPixelFormat()? };
        let information: IWICPixelFormatInfo =
            unsafe { factory.CreateComponentInfo(&format)? }.cast()?;
        let bits_per_pixel = unsafe { information.GetBitsPerPixel()? };
        let channel_count = unsafe { information.GetChannelCount()? };
        Ok(channel_count > 0 && bits_per_pixel > channel_count * 8)
    })()
    .unwrap_or(false)
}

/// ICC 'cicp' 태그(ICC 4.4)의 전달 함수 코드 — 16=PQ, 18=HLG (BT.2100).
/// 태그 부재·비 HDR 전달 함수는 None (톤맵 비대상)
fn icc_cicp_transfer_function(icc: &[u8]) -> Option<u8> {
    const TRANSFER_PQ: u8 = 16;
    const TRANSFER_HLG: u8 = 18;
    let read_u32 = |offset: usize| -> Option<u32> {
        Some(u32::from_be_bytes(
            icc.get(offset..offset + 4)?.try_into().ok()?,
        ))
    };
    let tag_count = read_u32(128)? as usize;
    for index in 0..tag_count {
        let entry = 132 + index * 12;
        if icc.get(entry..entry + 4)? != b"cicp" {
            continue;
        }
        let offset = read_u32(entry + 4)? as usize;
        // 태그 데이터 = 타입 시그니처(4) + 예약(4) + 원색·전달·행렬·범위 각 1바이트
        if icc.get(offset..offset + 4)? != b"cicp" {
            return None;
        }
        let transfer = *icc.get(offset + 9)?;
        return matches!(transfer, TRANSFER_PQ | TRANSFER_HLG).then_some(transfer);
    }
    None
}

/// FP16 픽셀에서 콘텐츠 피크 휘도(nits) — PQ는 최대 코드값의 EOTF 환산(정확),
/// HLG는 공칭 피크 1000 nits (BT.2100)
fn peak_luminance_from_half_pixels(pixels: &[u8], transfer: u8) -> Option<f32> {
    match transfer {
        16 => {
            let mut maximum_code = 0.0f32;
            for pixel in pixels.chunks_exact(8) {
                for channel in 0..3 {
                    let bits = u16::from_le_bytes([pixel[channel * 2], pixel[channel * 2 + 1]]);
                    let code = half_to_f32(bits);
                    if code > maximum_code {
                        maximum_code = code;
                    }
                }
            }
            Some(perceptual_quantizer_nits(maximum_code.min(1.0)))
        }
        18 => Some(1000.0),
        _ => None,
    }
}

/// FP16 → f32 (피크 스캔 전용) — 음수·서브노멀·Inf/NaN은 피크 후보가 아니므로 0
fn half_to_f32(bits: u16) -> f32 {
    let exponent = (bits >> 10) & 0x1F;
    if bits & 0x8000 != 0 || exponent == 0 || exponent == 31 {
        return 0.0;
    }
    let mantissa = u32::from(bits & 0x03FF);
    f32::from_bits(((u32::from(exponent) + 112) << 23) | (mantissa << 13))
}

/// PQ(SMPTE ST 2084) EOTF — 인코딩 코드값 [0,1] → 절대 휘도(nits)
fn perceptual_quantizer_nits(code: f32) -> f32 {
    const M1: f32 = 2610.0 / 16384.0;
    const M2: f32 = 2523.0 / 4096.0 * 128.0;
    const C1: f32 = 3424.0 / 4096.0;
    const C2: f32 = 2413.0 / 4096.0 * 32.0;
    const C3: f32 = 2392.0 / 4096.0 * 32.0;
    let power = code.max(0.0).powf(1.0 / M2);
    let numerator = (power - C1).max(0.0);
    let denominator = C2 - C3 * power;
    10000.0 * (numerator / denominator).powf(1.0 / M1)
}

/// 임베디드 색 프로파일(ICC 바이트) 추출 (SPEC §7) — 프로파일 타입 컨텍스트만 대상,
/// Exif 색공간 지정·부재는 None(sRGB 가정)
fn icc_profile_bytes(
    factory: &IWICImagingFactory,
    frame: &IWICBitmapFrameDecode,
) -> Option<Vec<u8>> {
    let mut count = 0u32;
    unsafe { frame.GetColorContexts(&mut [], &mut count) }.ok()?;
    if count == 0 {
        return None;
    }
    let mut contexts: Vec<Option<IWICColorContext>> = (0..count)
        .map(|_| unsafe { factory.CreateColorContext() }.ok())
        .collect();
    if contexts.iter().any(Option::is_none) {
        return None;
    }
    let mut actual_count = 0u32;
    unsafe { frame.GetColorContexts(&mut contexts, &mut actual_count) }.ok()?;
    for context in contexts.into_iter().flatten() {
        if unsafe { context.GetType() } != Ok(WICColorContextProfile) {
            continue;
        }
        let mut size = 0u32;
        let _ = unsafe { context.GetProfileBytes(&mut [], &mut size) };
        if size == 0 {
            continue;
        }
        let mut buffer = vec![0u8; size as usize];
        let mut written = 0u32;
        unsafe { context.GetProfileBytes(&mut buffer, &mut written) }.ok()?;
        buffer.truncate(written as usize);
        return Some(buffer);
    }
    None
}

/// ICO 등 해상도 변형 컨테이너 — 픽셀 수 최대 프레임 선택
fn decode_largest_frame(
    factory: &IWICImagingFactory,
    decoder: &IWICBitmapDecoder,
    frame_count: u32,
) -> windows::core::Result<DecodedFrames> {
    let mut largest_index = 0;
    let mut largest_pixels = 0u64;
    for index in 0..frame_count {
        let frame = unsafe { decoder.GetFrame(index)? };
        let (width, height) = source_size(&frame.cast()?)?;
        let pixels = u64::from(width) * u64::from(height);
        if pixels > largest_pixels {
            largest_pixels = pixels;
            largest_index = index;
        }
    }
    decode_single_frame(factory, decoder, largest_index)
}

/// 프레임별 배치·타이밍 메타데이터 (GIF) — 쿼리 실패 시 기본값.
/// 딜레이 값은 실기 검증 대상 (R2 게이트, PORTING_PLAN §8)
struct FrameMetadata {
    left: u32,
    top: u32,
    delay_milliseconds: u32,
    /// GIF 디스포절: 0/1=유지, 2=배경 복원, 3=이전 프레임 복원
    disposal: u32,
}

fn frame_metadata(frame: &IWICBitmapFrameDecode) -> FrameMetadata {
    let reader = unsafe { frame.GetMetadataQueryReader() }.ok();
    let query = |name: PCWSTR| reader.as_ref().and_then(|reader| query_u32(reader, name));

    let delay_milliseconds = query(w!("/grctlext/Delay"))
        .map(|centiseconds| centiseconds * 10)
        // 관례: 미지정·20ms 미만은 100ms (브라우저·qView 동일)
        .filter(|milliseconds| *milliseconds >= 20)
        .unwrap_or(100);
    FrameMetadata {
        left: query(w!("/imgdesc/Left")).unwrap_or(0),
        top: query(w!("/imgdesc/Top")).unwrap_or(0),
        delay_milliseconds,
        disposal: query(w!("/grctlext/Disposal")).unwrap_or(0),
    }
}

/// GIF 애니메이션 프레임 합성 (SPEC §4.6, PORTING_PLAN §5 — 자체 합성).
/// 캔버스 = GIF 논리 스크린(없으면 첫 프레임 크기), 프레임은 over 블렌드.
/// DP3 다운스케일 비대상(디바이스 한계 초과 애니메이션은 비현실적 — 타일링 후속 과제와 동일 취급).
fn decode_animation(
    factory: &IWICImagingFactory,
    decoder: &IWICBitmapDecoder,
    frame_count: u32,
) -> windows::core::Result<DecodedFrames> {
    let container_reader = unsafe { decoder.GetMetadataQueryReader() }.ok();
    let container_query = |name: PCWSTR| {
        container_reader
            .as_ref()
            .and_then(|reader| query_u32(reader, name))
    };
    let mut canvas_width = container_query(w!("/logscrdesc/Width")).unwrap_or(0);
    let mut canvas_height = container_query(w!("/logscrdesc/Height")).unwrap_or(0);

    let mut canvas: Vec<u8> = Vec::new();
    let mut frames = Vec::with_capacity(frame_count as usize);
    let mut icc_profile = None;
    for index in 0..frame_count {
        let frame = unsafe { decoder.GetFrame(index)? };
        if index == 0 {
            icc_profile = icc_profile_bytes(factory, &frame);
        }
        let metadata = frame_metadata(&frame);
        let source = convert_to_pbgra(factory, &frame.cast()?)?;
        let (frame_width, frame_height) = source_size(&source)?;
        if canvas_width == 0 || canvas_height == 0 {
            canvas_width = frame_width;
            canvas_height = frame_height;
        }
        if canvas.is_empty() {
            canvas = vec![0u8; canvas_width as usize * canvas_height as usize * 4];
        }
        let frame_pixels = copy_pixels(&source, frame_width, frame_height, 4)?;

        let restore_previous = (metadata.disposal == 3).then(|| canvas.clone());
        blend_over(
            &mut canvas,
            canvas_width,
            canvas_height,
            &frame_pixels,
            frame_width,
            frame_height,
            metadata.left,
            metadata.top,
        );
        frames.push(Frame {
            pixels: canvas.clone(),
            delay_milliseconds: metadata.delay_milliseconds,
        });

        match (metadata.disposal, restore_previous) {
            (2, _) => clear_rectangle(
                &mut canvas,
                canvas_width,
                metadata.left,
                metadata.top,
                frame_width,
                frame_height,
            ),
            (3, Some(previous)) => canvas = previous,
            _ => {}
        }
    }
    Ok(DecodedFrames {
        width: canvas_width,
        height: canvas_height,
        pixel_width: canvas_width,
        pixel_height: canvas_height,
        icc_profile,
        exif: None,
        storage: PixelStorage::Bgra8,
        peak_luminance_nits: None,
        frames,
    })
}

/// premultiplied over 블렌드: out = src + dst × (1 − srcA). 캔버스 밖은 클립.
#[expect(clippy::too_many_arguments)]
pub(crate) fn blend_over(
    canvas: &mut [u8],
    canvas_width: u32,
    canvas_height: u32,
    source: &[u8],
    source_width: u32,
    source_height: u32,
    left: u32,
    top: u32,
) {
    let visible_width = source_width.min(canvas_width.saturating_sub(left)) as usize;
    let visible_height = source_height.min(canvas_height.saturating_sub(top)) as usize;
    for row in 0..visible_height {
        let source_start = row * source_width as usize * 4;
        let canvas_start = ((top as usize + row) * canvas_width as usize + left as usize) * 4;
        for column in 0..visible_width {
            let source_pixel = &source[source_start + column * 4..source_start + column * 4 + 4];
            let alpha = u32::from(source_pixel[3]);
            if alpha == 0 {
                continue;
            }
            let canvas_pixel =
                &mut canvas[canvas_start + column * 4..canvas_start + column * 4 + 4];
            if alpha == 255 {
                canvas_pixel.copy_from_slice(source_pixel);
                continue;
            }
            for channel in 0..4 {
                let blended = u32::from(source_pixel[channel])
                    + (u32::from(canvas_pixel[channel]) * (255 - alpha) + 127) / 255;
                canvas_pixel[channel] = blended.min(255) as u8;
            }
        }
    }
}

/// 디스포절 2(배경 복원) — 프레임 사각형을 투명으로 클리어
pub(crate) fn clear_rectangle(
    canvas: &mut [u8],
    canvas_width: u32,
    left: u32,
    top: u32,
    width: u32,
    height: u32,
) {
    let canvas_height = canvas.len() / (canvas_width as usize * 4);
    let visible_width = width.min(canvas_width.saturating_sub(left)) as usize;
    let visible_height = (height as usize).min(canvas_height.saturating_sub(top as usize));
    for row in 0..visible_height {
        let start = ((top as usize + row) * canvas_width as usize + left as usize) * 4;
        canvas[start..start + visible_width * 4].fill(0);
    }
}

fn convert_to_pbgra(
    factory: &IWICImagingFactory,
    source: &IWICBitmapSource,
) -> windows::core::Result<IWICBitmapSource> {
    convert_pixel_format(factory, source, &GUID_WICPixelFormat32bppPBGRA)
}

fn convert_pixel_format(
    factory: &IWICImagingFactory,
    source: &IWICBitmapSource,
    target: &windows::core::GUID,
) -> windows::core::Result<IWICBitmapSource> {
    let converter = unsafe { factory.CreateFormatConverter()? };
    unsafe {
        converter.Initialize(
            source,
            target,
            WICBitmapDitherTypeNone,
            None,
            0.0,
            WICBitmapPaletteTypeCustom,
        )?;
    }
    converter.cast()
}

/// EXIF 오리엔테이션 자동 적용 (SPEC §4.2) — WIC 플립·로테이터에 위임 (P15).
/// 값→변환 매핑(5·7 조합 포함)은 실기 확인 대상 (R2 게이트)
fn apply_orientation(
    factory: &IWICImagingFactory,
    source: IWICBitmapSource,
    orientation: u32,
) -> windows::core::Result<IWICBitmapSource> {
    let options = match orientation {
        2 => WICBitmapTransformFlipHorizontal,
        3 => WICBitmapTransformRotate180,
        4 => WICBitmapTransformFlipVertical,
        5 => WICBitmapTransformOptions(
            WICBitmapTransformRotate90.0 | WICBitmapTransformFlipHorizontal.0,
        ),
        6 => WICBitmapTransformRotate90,
        7 => WICBitmapTransformOptions(
            WICBitmapTransformRotate270.0 | WICBitmapTransformFlipHorizontal.0,
        ),
        8 => WICBitmapTransformRotate270,
        _ => return Ok(source),
    };
    let rotator = unsafe { factory.CreateBitmapFlipRotator()? };
    unsafe { rotator.Initialize(&source, options)? };
    rotator.cast()
}

/// EXIF 오리엔테이션 취득 — 포토 메타데이터 정책 경로 (PORTING_PLAN §5), 부재 시 1
fn exif_orientation(frame: &IWICBitmapFrameDecode) -> u32 {
    let Ok(reader) = (unsafe { frame.GetMetadataQueryReader() }) else {
        return 1;
    };
    query_u32(&reader, w!("System.Photo.Orientation")).unwrap_or(1)
}

/// EXIF 촬영 정보 조회 (SPEC §3.6 확장) — 포토 정책 이름은 컨테이너 무관.
/// 전 필드 부재(EXIF 없는 포맷·파일)면 None.
fn read_exif(frame: &IWICBitmapFrameDecode) -> Option<ExifInfo> {
    let reader = unsafe { frame.GetMetadataQueryReader() }.ok()?;
    let information = ExifInfo {
        date_taken: query_filetime(&reader, w!("System.Photo.DateTaken")),
        rating: query_u32(&reader, w!("System.Rating")),
        camera_maker: query_string(&reader, w!("System.Photo.CameraManufacturer")),
        camera_model: query_string(&reader, w!("System.Photo.CameraModel")),
        f_stop: query_f64(&reader, w!("System.Photo.FNumber")),
        exposure_time_seconds: query_f64(&reader, w!("System.Photo.ExposureTime")),
        iso_speed: query_u32(&reader, w!("System.Photo.ISOSpeed")),
        exposure_bias: query_f64(&reader, w!("System.Photo.ExposureBias")),
        focal_length_millimeters: query_f64(&reader, w!("System.Photo.FocalLength")),
        max_aperture: query_f64(&reader, w!("System.Photo.MaxAperture")),
        metering_mode: query_u32(&reader, w!("System.Photo.MeteringMode")),
        flash: query_u32(&reader, w!("System.Photo.Flash")),
        focal_length_35mm: query_u32(&reader, w!("System.Photo.FocalLengthInFilm")),
    };
    information.any_present().then_some(information)
}

fn query_f64(reader: &IWICMetadataQueryReader, name: PCWSTR) -> Option<f64> {
    let mut value = PROPVARIANT::default();
    unsafe { reader.GetMetadataByName(name, &mut value) }.ok()?;
    let result = unsafe { PropVariantToDouble(&value) }.ok();
    let _ = unsafe { PropVariantClear(&mut value) };
    result.filter(|number| number.is_finite())
}

fn query_string(reader: &IWICMetadataQueryReader, name: PCWSTR) -> Option<String> {
    let mut value = PROPVARIANT::default();
    unsafe { reader.GetMetadataByName(name, &mut value) }.ok()?;
    let text = unsafe { PropVariantToStringAlloc(&value) }.ok().map(|out| {
        let result = String::from_utf16_lossy(unsafe { out.as_wide() });
        unsafe { windows::Win32::System::Com::CoTaskMemFree(Some(out.0.cast())) };
        result
    });
    let _ = unsafe { PropVariantClear(&mut value) };
    text.map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())
}

/// FILETIME 조회 → SystemTime (UTC — 로캘 표시 변환은 오버레이 측)
fn query_filetime(reader: &IWICMetadataQueryReader, name: PCWSTR) -> Option<std::time::SystemTime> {
    use windows::Win32::System::Variant::PSTF_UTC;
    let mut value = PROPVARIANT::default();
    unsafe { reader.GetMetadataByName(name, &mut value) }.ok()?;
    let file_time = unsafe { PropVariantToFileTime(&value, PSTF_UTC) }.ok();
    let _ = unsafe { PropVariantClear(&mut value) };
    let file_time = file_time?;
    let intervals =
        (u64::from(file_time.dwHighDateTime) << 32) | u64::from(file_time.dwLowDateTime);
    // FILETIME(1601 기준 100ns) → UNIX epoch
    let unix_intervals = intervals.checked_sub(116_444_736_000_000_000)?;
    Some(std::time::UNIX_EPOCH + std::time::Duration::from_nanos(unix_intervals * 100))
}

fn query_u32(reader: &IWICMetadataQueryReader, name: PCWSTR) -> Option<u32> {
    let mut value = PROPVARIANT::default();
    unsafe { reader.GetMetadataByName(name, &mut value) }.ok()?;
    let result = unsafe { PropVariantToUInt32(&value) }.ok();
    let _ = unsafe { PropVariantClear(&mut value) };
    result
}

fn source_size(source: &IWICBitmapSource) -> windows::core::Result<(u32, u32)> {
    let (mut width, mut height) = (0u32, 0u32);
    unsafe { source.GetSize(&mut width, &mut height)? };
    Ok((width, height))
}

// ── 내장 fallback 디코더 (PORTING_PLAN §5 — WIC 완전 미지원 포맷 전담) ────────

/// fallback 디코더 오류 — Win32 HRESULT가 없는 경로 (코드 0)
pub(crate) fn fallback_error(message: impl std::fmt::Display) -> DecodeError {
    DecodeError {
        code: 0,
        message: message.to_string(),
        store_extension: None,
    }
}

/// APNG (`png` crate — fdeflate, PORTING_PLAN §5). 프레임을 fcTL의 blend/dispose
/// 규칙으로 캔버스에 합성해 완성 프레임 목록을 만든다 (GIF 합성과 동일 모델).
fn decode_apng(path: &Path, format_name: &'static str) -> Result<DecodedImage, DecodeError> {
    let file = File::open(path).map_err(fallback_error)?;
    let mut decoder = png::Decoder::new(BufReader::new(file));
    // 8비트 RGB(A)로 정규화 — 팔레트·고심도·그레이 확장
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder.read_info().map_err(fallback_error)?;

    let (canvas_width, canvas_height) = {
        let information = reader.info();
        (information.width, information.height)
    };
    let icc_profile = reader
        .info()
        .icc_profile
        .as_ref()
        .map(|profile| profile.to_vec());
    let animation_frame_count = reader
        .info()
        .animation_control
        .map_or(1, |control| control.num_frames);
    // acTL은 있지만 IDAT 앞에 fcTL이 없으면 IDAT는 애니메이션 밖의 기본 이미지
    let default_image_is_first_frame = reader.info().frame_control.is_some();
    let has_animation = reader.info().animation_control.is_some();

    let buffer_size = reader
        .output_buffer_size()
        .ok_or_else(|| fallback_error("APNG output buffer size overflow"))?;
    let mut buffer = vec![0u8; buffer_size];

    // 기본 이미지가 애니메이션 밖이면 디코드 후 폐기 (APNG 사양)
    if has_animation && !default_image_is_first_frame {
        reader.next_frame(&mut buffer).map_err(fallback_error)?;
    }

    let mut canvas = vec![0u8; canvas_width as usize * canvas_height as usize * 4];
    let mut frames = Vec::with_capacity(animation_frame_count as usize);
    for index in 0..animation_frame_count {
        // 첫 프레임이 IDAT면 fcTL이 이미 읽혀 있다 — 이후 프레임은 next_frame_info로 전진
        if !(index == 0 && (default_image_is_first_frame || !has_animation)) {
            reader.next_frame_info().map_err(fallback_error)?;
        }
        // 비-APNG 단일 프레임(fcTL 부재)은 전체 캔버스 Source 교체로 처리
        let frame_control = reader.info().frame_control.unwrap_or(png::FrameControl {
            width: canvas_width,
            height: canvas_height,
            blend_op: png::BlendOp::Source,
            ..Default::default()
        });
        let output = reader.next_frame(&mut buffer).map_err(fallback_error)?;
        let region_pixels = pixels_to_premultiplied_bgra(
            &buffer[..output.buffer_size()],
            output.color_type,
            frame_control.width,
            frame_control.height,
        )?;

        let restore_previous =
            (frame_control.dispose_op == png::DisposeOp::Previous).then(|| canvas.clone());
        if frame_control.blend_op == png::BlendOp::Source {
            copy_rectangle(
                &mut canvas,
                canvas_width,
                canvas_height,
                &region_pixels,
                frame_control.width,
                frame_control.height,
                frame_control.x_offset,
                frame_control.y_offset,
            );
        } else {
            blend_over(
                &mut canvas,
                canvas_width,
                canvas_height,
                &region_pixels,
                frame_control.width,
                frame_control.height,
                frame_control.x_offset,
                frame_control.y_offset,
            );
        }
        let delay_denominator = if frame_control.delay_den == 0 {
            100
        } else {
            u32::from(frame_control.delay_den)
        };
        frames.push(Frame {
            pixels: canvas.clone(),
            delay_milliseconds: (u32::from(frame_control.delay_num) * 1000 / delay_denominator)
                .max(10),
        });
        match (frame_control.dispose_op, restore_previous) {
            (png::DisposeOp::Background, _) => clear_rectangle(
                &mut canvas,
                canvas_width,
                frame_control.x_offset,
                frame_control.y_offset,
                frame_control.width,
                frame_control.height,
            ),
            (png::DisposeOp::Previous, Some(previous)) => canvas = previous,
            _ => {}
        }
    }
    Ok(DecodedImage {
        width: canvas_width,
        height: canvas_height,
        pixel_width: canvas_width,
        pixel_height: canvas_height,
        format_name,
        icc_profile,
        exif: None,
        storage: PixelStorage::Bgra8,
        peak_luminance_nits: None,
        frames,
    })
}

/// 정규화된 PNG 출력(RGB8/RGBA8) → premultiplied BGRA8
fn pixels_to_premultiplied_bgra(
    pixels: &[u8],
    color_type: png::ColorType,
    width: u32,
    height: u32,
) -> Result<Vec<u8>, DecodeError> {
    let pixel_count = width as usize * height as usize;
    let mut output = Vec::with_capacity(pixel_count * 4);
    match color_type {
        png::ColorType::Rgba => {
            for pixel in pixels[..pixel_count * 4].chunks_exact(4) {
                let alpha = u16::from(pixel[3]);
                output.push((u16::from(pixel[2]) * alpha / 255) as u8);
                output.push((u16::from(pixel[1]) * alpha / 255) as u8);
                output.push((u16::from(pixel[0]) * alpha / 255) as u8);
                output.push(pixel[3]);
            }
        }
        png::ColorType::Rgb => {
            for pixel in pixels[..pixel_count * 3].chunks_exact(3) {
                output.extend_from_slice(&[pixel[2], pixel[1], pixel[0], 255]);
            }
        }
        other => {
            return Err(fallback_error(format!(
                "unsupported PNG color type after normalization: {other:?}"
            )));
        }
    }
    Ok(output)
}

/// blend_op Source — 사각형을 알파 포함 그대로 교체 (APNG 사양)
#[expect(clippy::too_many_arguments)]
pub(crate) fn copy_rectangle(
    canvas: &mut [u8],
    canvas_width: u32,
    canvas_height: u32,
    source: &[u8],
    source_width: u32,
    source_height: u32,
    left: u32,
    top: u32,
) {
    let visible_width = source_width.min(canvas_width.saturating_sub(left)) as usize;
    let visible_height = source_height.min(canvas_height.saturating_sub(top)) as usize;
    for row in 0..visible_height {
        let source_start = row * source_width as usize * 4;
        let canvas_start = ((top as usize + row) * canvas_width as usize + left as usize) * 4;
        canvas[canvas_start..canvas_start + visible_width * 4]
            .copy_from_slice(&source[source_start..source_start + visible_width * 4]);
    }
}

/// SVG/SVGZ (`resvg` — PORTING_PLAN §5). 가장 큰 모니터의 긴 변 해상도로
/// 래스터화해 확대 여유를 확보한다 (SPEC §4.2). SVGZ(gzip)는 usvg가 판별.
fn decode_svg(path: &Path, format_name: &'static str) -> Result<DecodedImage, DecodeError> {
    let data = std::fs::read(path).map_err(fallback_error)?;
    let options = resvg::usvg::Options {
        fontdb: font_database().clone(),
        ..Default::default()
    };
    let tree = resvg::usvg::Tree::from_data(&data, &options).map_err(fallback_error)?;
    let size = tree.size();
    if !(size.width() > 0.0 && size.height() > 0.0) {
        return Err(fallback_error("SVG has no intrinsic size"));
    }
    let target = largest_monitor_long_side().min(MAXIMUM_TEXTURE_DIMENSION) as f32;
    let scale = target / size.width().max(size.height());
    let pixel_width = (size.width() * scale).round().max(1.0) as u32;
    let pixel_height = (size.height() * scale).round().max(1.0) as u32;
    let mut pixmap = resvg::tiny_skia::Pixmap::new(pixel_width, pixel_height)
        .ok_or_else(|| fallback_error("SVG raster target allocation failed"))?;
    resvg::render(
        &tree,
        resvg::tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );
    // tiny-skia 픽스맵 = premultiplied RGBA → BGRA 스왑
    let mut pixels = pixmap.take();
    for pixel in pixels.chunks_exact_mut(4) {
        pixel.swap(0, 2);
    }
    Ok(DecodedImage {
        width: pixel_width,
        height: pixel_height,
        pixel_width,
        pixel_height,
        format_name,
        icc_profile: None,
        exif: None,
        storage: PixelStorage::Bgra8,
        peak_luminance_nits: None,
        frames: vec![Frame {
            pixels,
            delay_milliseconds: 0,
        }],
    })
}

/// SVG 텍스트용 폰트 데이터베이스 — 시스템 폰트 로드는 1회 (프로세스 수명)
fn font_database() -> &'static std::sync::Arc<resvg::usvg::fontdb::Database> {
    static DATABASE: OnceLock<std::sync::Arc<resvg::usvg::fontdb::Database>> = OnceLock::new();
    DATABASE.get_or_init(|| {
        let mut database = resvg::usvg::fontdb::Database::new();
        database.load_system_fonts();
        std::sync::Arc::new(database)
    })
}

/// 가장 큰 모니터의 긴 변(물리 픽셀) — SVG 래스터화 목표 해상도 (SPEC §4.2)
fn largest_monitor_long_side() -> u32 {
    use windows::Win32::Foundation::{LPARAM, RECT};
    use windows::Win32::Graphics::Gdi::{EnumDisplayMonitors, HDC, HMONITOR};
    use windows::core::BOOL;

    extern "system" fn monitor_callback(
        _monitor: HMONITOR,
        _device_context: HDC,
        bounds: *mut RECT,
        state: LPARAM,
    ) -> BOOL {
        let longest = unsafe { &mut *(state.0 as *mut i32) };
        let bounds = unsafe { &*bounds };
        *longest = (*longest)
            .max(bounds.right - bounds.left)
            .max(bounds.bottom - bounds.top);
        true.into()
    }

    let mut longest = 0i32;
    let _ = unsafe {
        EnumDisplayMonitors(
            None,
            None,
            Some(monitor_callback),
            LPARAM(&raw mut longest as isize),
        )
    };
    if longest > 0 { longest as u32 } else { 1920 }
}

fn copy_pixels(
    source: &IWICBitmapSource,
    width: u32,
    height: u32,
    bytes_per_pixel: u32,
) -> windows::core::Result<Vec<u8>> {
    let stride = width * bytes_per_pixel;
    let mut pixels = vec![0u8; stride as usize * height as usize];
    unsafe { source.CopyPixels(std::ptr::null(), stride, &mut pixels)? };
    Ok(pixels)
}
