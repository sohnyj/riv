//! 디코더 레지스트리 · 포맷 디스패치 · WIC 어댑터 (PORTING_PLAN §5, SPEC §4.2·§10)
//!
//! 어댑터는 디코드 워커 스레드(COM MTA)에서 호출된다 — WIC 팩토리·디코더는
//! 스레드별 생성(thread_local). R2는 WIC 어댑터 1개만 등록하며, R5에서 내장
//! fallback(APNG·EXR·SVG·HEIF)이 추가될 때 어댑터 선택 필드가 생긴다.

use std::cell::RefCell;
use std::fs::File;
use std::io::Read;
use std::path::Path;

use windows::Win32::Foundation::GENERIC_READ;
use windows::Win32::Graphics::Imaging::{
    CLSID_WICImagingFactory, GUID_WICPixelFormat32bppPBGRA, IWICBitmapDecoder,
    IWICBitmapFrameDecode, IWICBitmapSource, IWICImagingFactory, IWICMetadataQueryReader,
    WICBitmapDitherTypeNone, WICBitmapInterpolationModeFant, WICBitmapPaletteTypeCustom,
    WICBitmapTransformFlipHorizontal, WICBitmapTransformFlipVertical, WICBitmapTransformOptions,
    WICBitmapTransformRotate90, WICBitmapTransformRotate180, WICBitmapTransformRotate270,
    WICDecodeMetadataCacheOnDemand,
};
use windows::Win32::System::Com::StructuredStorage::{
    PROPVARIANT, PropVariantClear, PropVariantToUInt32,
};
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};
use windows::core::{HSTRING, Interface, PCWSTR, w};

/// 디코드 결과 프레임 — premultiplied BGRA8, 캔버스 전체로 합성 완료 (SPEC §3.1·§4.6)
pub struct Frame {
    pub pixels: Vec<u8>,
    /// 정지 이미지는 0. 애니메이션 스케줄러(R5)가 소비 — 연동 후 expect가 알려준다
    #[expect(dead_code)]
    pub delay_milliseconds: u32,
}

pub struct DecodedImage {
    /// 논리 크기 = 원본 픽셀 — 표시 크기 기준 (DP3 다운스케일과 무관, SPEC §3.4)
    pub width: u32,
    pub height: u32,
    /// 픽셀 버퍼 크기 — 디바이스 한계 초과 시에만 논리 크기보다 작다 (DP3)
    pub pixel_width: u32,
    pub pixel_height: u32,
    /// 디코더 레지스트리 포맷명 — 정보 오버레이(R4) 소스 (SPEC §3.6)
    #[expect(dead_code)]
    pub format_name: &'static str,
    pub frames: Vec<Frame>,
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
}

impl From<windows::core::Error> for DecodeError {
    fn from(error: windows::core::Error) -> Self {
        Self {
            code: error.code().0,
            message: error.message(),
        }
    }
}

/// 프레임 구성 의미 — 다중 프레임을 애니메이션으로 볼지, 크기 변형으로 볼지
enum FrameSemantics {
    /// 첫 프레임만 사용 (PNG·JPEG·BMP)
    Single,
    /// 프레임 = 애니메이션 시퀀스 (GIF·WebP·APNG)
    Animation,
    /// 프레임 = 해상도 변형 — 최대 해상도 선택 (ICO)
    SizeVariants,
}

/// D3D11 FL11·D2D 비트맵 한계 — 초과 시 업로드 전 다운스케일 (SPEC §3.4 DP3)
const MAXIMUM_TEXTURE_DIMENSION: u32 = 16384;

/// 매직 시그니처 — (오프셋, 바이트) 전부 일치 시 매치. 빈 배열 = 매직 프로빙 비대상.
type MagicSignature = &'static [(usize, &'static [u8])];

pub struct FormatDescriptor {
    /// 포맷명 — 타입 정렬·정보 오버레이·연결 UI 그룹의 단일 소스 (SPEC §10)
    pub name: &'static str,
    pub extensions: &'static [&'static str],
    magic: MagicSignature,
    semantics: FrameSemantics,
}

/// 디코더 레지스트리 — 지원 확장자 목록·파일 필터·연결 그룹은 전부 여기서 파생
/// (PORTING_PLAN §5 — 수작업 테이블 금지). R2는 WIC 1차 포맷.
/// APNG는 확장자 또는 PNG 매직 + acTL 프로빙으로 분기(`refine_by_content`) —
/// R2에서는 WIC PNG로 첫 프레임만 표시하고, R5에서 `png` crate fallback으로 대체.
static REGISTRY: &[FormatDescriptor] = &[
    FormatDescriptor {
        name: "PNG",
        extensions: &["png"],
        magic: &[(0, b"\x89PNG\r\n\x1a\n")],
        semantics: FrameSemantics::Single,
    },
    FormatDescriptor {
        name: "APNG",
        extensions: &["apng"],
        magic: &[],
        semantics: FrameSemantics::Animation,
    },
    FormatDescriptor {
        name: "JPEG",
        extensions: &["jpg", "jpeg", "jpe", "jfif"],
        magic: &[(0, b"\xFF\xD8\xFF")],
        semantics: FrameSemantics::Single,
    },
    FormatDescriptor {
        name: "GIF",
        extensions: &["gif"],
        magic: &[(0, b"GIF8")],
        semantics: FrameSemantics::Animation,
    },
    FormatDescriptor {
        name: "WebP",
        extensions: &["webp"],
        magic: &[(0, b"RIFF"), (8, b"WEBP")],
        semantics: FrameSemantics::Animation,
    },
    FormatDescriptor {
        name: "BMP",
        extensions: &["bmp", "dib"],
        magic: &[(0, b"BM")],
        semantics: FrameSemantics::Single,
    },
    FormatDescriptor {
        name: "ICO",
        extensions: &["ico"],
        magic: &[(0, &[0x00, 0x00, 0x01, 0x00])],
        semantics: FrameSemantics::SizeVariants,
    },
];

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
        !descriptor.magic.is_empty()
            && descriptor.magic.iter().all(|(offset, bytes)| {
                header
                    .get(*offset..offset + bytes.len())
                    .is_some_and(|slice| slice == *bytes)
            })
    })
}

/// 내용 기반 세분화 — PNG + acTL 청크 = APNG (R5: SVG + gzip = SVGZ 추가)
fn refine_by_content(
    descriptor: &'static FormatDescriptor,
    header: &[u8],
) -> &'static FormatDescriptor {
    if descriptor.name == "PNG" && png_has_animation_control(header) {
        return descriptor_for_extension("apng").unwrap_or(descriptor);
    }
    descriptor
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
    // R2 어댑터는 WIC 단일 — 레지스트리 미등록 포맷도 WIC 내용 판별에 맡긴다
    let format_name = descriptor.map_or("Unknown", |descriptor| descriptor.name);
    let semantics = descriptor.map_or(&FrameSemantics::Single, |descriptor| &descriptor.semantics);
    decode_with_wic(path, format_name, semantics)
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
    let source = convert_to_pbgra(factory, &frame)?;
    let source = apply_orientation(factory, source, orientation)?;
    let (width, height) = source_size(&source)?;
    let (source, pixel_width, pixel_height) =
        downscale_to_device_limit(factory, source, width, height)?;
    let pixels = copy_pixels(&source, pixel_width, pixel_height)?;
    Ok(DecodedFrames {
        width,
        height,
        pixel_width,
        pixel_height,
        frames: vec![Frame {
            pixels,
            delay_milliseconds: 0,
        }],
    })
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

/// 프레임별 배치·타이밍 메타데이터 — GIF 쿼리 우선, WebP(ANMF) 차선, 실패 시 기본값.
/// WIC 애니 WebP의 쿼리 지원 범위는 실기 검증 대상 (R2 게이트, PORTING_PLAN §8)
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
        .or_else(|| query(w!("/ANMF/FrameDuration")))
        // 관례: 미지정·20ms 미만은 100ms (브라우저·qView 동일)
        .filter(|milliseconds| *milliseconds >= 20)
        .unwrap_or(100);
    FrameMetadata {
        left: query(w!("/imgdesc/Left"))
            .or_else(|| query(w!("/ANMF/FrameX")))
            .unwrap_or(0),
        top: query(w!("/imgdesc/Top"))
            .or_else(|| query(w!("/ANMF/FrameY")))
            .unwrap_or(0),
        delay_milliseconds,
        disposal: query(w!("/grctlext/Disposal")).unwrap_or(0),
    }
}

/// GIF·WebP 애니메이션 프레임 합성 (SPEC §4.6, PORTING_PLAN §5 — 자체 합성).
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
    for index in 0..frame_count {
        let frame = unsafe { decoder.GetFrame(index)? };
        let metadata = frame_metadata(&frame);
        let source = convert_to_pbgra(factory, &frame)?;
        let (frame_width, frame_height) = source_size(&source)?;
        if canvas_width == 0 || canvas_height == 0 {
            canvas_width = frame_width;
            canvas_height = frame_height;
        }
        if canvas.is_empty() {
            canvas = vec![0u8; canvas_width as usize * canvas_height as usize * 4];
        }
        let frame_pixels = copy_pixels(&source, frame_width, frame_height)?;

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
        frames,
    })
}

/// premultiplied over 블렌드: out = src + dst × (1 − srcA). 캔버스 밖은 클립.
#[expect(clippy::too_many_arguments)]
fn blend_over(
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
fn clear_rectangle(
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
    frame: &IWICBitmapFrameDecode,
) -> windows::core::Result<IWICBitmapSource> {
    let converter = unsafe { factory.CreateFormatConverter()? };
    unsafe {
        converter.Initialize(
            frame,
            &GUID_WICPixelFormat32bppPBGRA,
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

fn copy_pixels(
    source: &IWICBitmapSource,
    width: u32,
    height: u32,
) -> windows::core::Result<Vec<u8>> {
    let stride = width * 4;
    let mut pixels = vec![0u8; stride as usize * height as usize];
    unsafe { source.CopyPixels(std::ptr::null(), stride, &mut pixels)? };
    Ok(pixels)
}
