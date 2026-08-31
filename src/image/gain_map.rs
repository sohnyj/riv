//! Finding an Ultra HDR gain map: the MPF secondary JPEG and its hdrgm XMP.

use std::io::{Cursor, Read, Seek, SeekFrom};
use std::ops::Range;

/// Gain map parameters from the hdrgm XMP packet, one value per color plane.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GainMapMetadata {
    pub gain_map_minimum: [f32; 3],
    pub gain_map_maximum: [f32; 3],
    pub gamma: [f32; 3],
    pub offset_sdr: [f32; 3],
    pub offset_hdr: [f32; 3],
    pub hdr_capacity_minimum: f32,
    pub hdr_capacity_maximum: f32,
    pub base_rendition_is_hdr: bool,
}

impl GainMapMetadata {
    /// Weight W: how much of the gain the display headroom admits, clamped to [0, 1].
    pub fn weight(&self, display_headroom: f32) -> f32 {
        let headroom_log2 = display_headroom.max(f32::MIN_POSITIVE).log2();
        let span = self.hdr_capacity_maximum - self.hdr_capacity_minimum;
        ((headroom_log2 - self.hdr_capacity_minimum) / span).clamp(0.0, 1.0)
    }

    /// The full rendition's peak in nits: SDR reference white times the capacity cap.
    pub fn capacity_peak_nits(&self) -> f32 {
        crate::image::color::SDR_REFERENCE_WHITE_NITS * self.hdr_capacity_maximum.exp2()
    }
}

/// Where the gain map JPEG sits in the file, and what its XMP declares.
#[derive(Clone, Debug, PartialEq)]
pub struct UltraHdr {
    pub gain_map_range: Range<usize>,
    pub metadata: GainMapMetadata,
}

/// The gain map decoded to BGRA pixels, kept beside the base frame until upload.
pub struct GainMapPlane {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

impl GainMapPlane {
    /// The spec keeps the gain map at or below the base size; anything larger is corrupt.
    pub fn size_fits_within(width: u32, height: u32, base_width: u32, base_height: u32) -> bool {
        width > 0 && height > 0 && width <= base_width && height <= base_height
    }
}

/// Finds the MPF secondary image whose XMP carries gain map parameters.
pub fn find_ultra_hdr(file: &[u8]) -> Option<UltraHdr> {
    for range in mpf_secondary_ranges(file) {
        let Some(candidate) = file.get(range.clone()) else {
            continue;
        };
        if let Some(metadata) = hdrgm_metadata(candidate)
            && usable(&metadata)
        {
            return Some(UltraHdr {
                gain_map_range: range,
                metadata,
            });
        }
    }
    None
}

/// A parameter set the formula can act on; anything else fails closed.
fn usable(metadata: &GainMapMetadata) -> bool {
    let finite = |values: &[f32; 3]| values.iter().all(|value| value.is_finite());
    let ordered = metadata
        .gain_map_minimum
        .iter()
        .zip(&metadata.gain_map_maximum)
        .all(|(minimum, maximum)| minimum <= maximum);
    finite(&metadata.gain_map_minimum)
        && finite(&metadata.gain_map_maximum)
        && ordered
        && metadata.gamma.iter().all(|gamma| gamma.is_finite() && *gamma > 0.0)
        && finite(&metadata.offset_sdr)
        && finite(&metadata.offset_hdr)
        // The weight formula divides by this span; an empty span has no rendition in it.
        && metadata.hdr_capacity_minimum.is_finite()
        && metadata.hdr_capacity_maximum.is_finite()
        && metadata.hdr_capacity_maximum > metadata.hdr_capacity_minimum
        // Capacity is the log2 display boost; a cap at or below zero gains nothing.
        && metadata.hdr_capacity_maximum > 0.0
        // Only the SDR-base layout is supported; an HDR base needs its own decode path.
        && !metadata.base_rendition_is_hdr
}

/// True when a header scan up to the scan data finds the MPF segment; a few KB at most.
pub fn jpeg_carries_mpf(mut reader: impl Read + Seek) -> bool {
    if !reads_start_of_image(&mut reader) {
        return false;
    }
    while let Some((kind, length)) = next_segment(&mut reader) {
        let mut remaining = length;
        if kind == APPLICATION_2 && remaining >= MPF_IDENTIFIER.len() {
            let mut identifier = [0u8; 4];
            if reader.read_exact(&mut identifier).is_err() {
                return false;
            }
            if identifier[..] == *MPF_IDENTIFIER {
                return true;
            }
            remaining -= identifier.len();
        }
        if reader.seek(SeekFrom::Current(remaining as i64)).is_err() {
            return false;
        }
    }
    false
}

fn reads_start_of_image(reader: &mut impl Read) -> bool {
    let mut start = [0u8; 2];
    reader.read_exact(&mut start).is_ok() && start == [0xFF, START_OF_IMAGE]
}

/// Next segment's marker and payload length, with the reader left at the payload.
fn next_segment(reader: &mut impl Read) -> Option<(u8, usize)> {
    loop {
        let mut marker = [0u8; 2];
        reader.read_exact(&mut marker).ok()?;
        // Fill bytes: any run of 0xFF collapses onto the marker that follows.
        while marker == [0xFF, 0xFF] {
            marker[1] = read_byte(reader)?;
        }
        if marker[0] != 0xFF {
            return None;
        }
        let kind = marker[1];
        if kind == START_OF_IMAGE || kind == 0x01 || (0xD0..=0xD7).contains(&kind) {
            continue;
        }
        if kind == END_OF_IMAGE || kind == START_OF_SCAN {
            return None;
        }
        let mut length = [0u8; 2];
        reader.read_exact(&mut length).ok()?;
        let length = u16::from_be_bytes(length) as usize;
        if length < 2 {
            return None;
        }
        return Some((kind, length - 2));
    }
}

fn read_byte(reader: &mut impl Read) -> Option<u8> {
    let mut byte = [0u8; 1];
    reader.read_exact(&mut byte).ok()?;
    Some(byte[0])
}

const START_OF_IMAGE: u8 = 0xD8;
const END_OF_IMAGE: u8 = 0xD9;
const START_OF_SCAN: u8 = 0xDA;
const APPLICATION_1: u8 = 0xE1;
const APPLICATION_2: u8 = 0xE2;

/// Payload range of each marker segment before the scan data, in file order.
fn segment_payloads(jpeg: &[u8]) -> Vec<(u8, Range<usize>)> {
    let mut payloads = Vec::new();
    let mut reader = Cursor::new(jpeg);
    if !reads_start_of_image(&mut reader) {
        return payloads;
    }
    while let Some((marker, length)) = next_segment(&mut reader) {
        let start = reader.position() as usize;
        let payload = start..start + length;
        if payload.end > jpeg.len() {
            break;
        }
        reader.set_position(payload.end as u64);
        payloads.push((marker, payload));
    }
    payloads
}

const MPF_IDENTIFIER: &[u8] = b"MPF\0";
const MP_ENTRY_TAG: u16 = 0xB002;
const MP_ENTRY_BYTES: usize = 16;

/// TIFF-order reads anchored at the MP header, in the index's declared endianness.
struct MpfReader<'bytes> {
    file: &'bytes [u8],
    header: usize,
    little_endian: bool,
}

impl MpfReader<'_> {
    fn read_u16(&self, offset: usize) -> Option<u16> {
        let bytes = self
            .file
            .get(self.header + offset..self.header + offset + 2)?;
        let value = u16::from_be_bytes(bytes.try_into().ok()?);
        Some(if self.little_endian {
            value.swap_bytes()
        } else {
            value
        })
    }

    fn read_u32(&self, offset: usize) -> Option<u32> {
        let bytes = self
            .file
            .get(self.header + offset..self.header + offset + 4)?;
        let value = u32::from_be_bytes(bytes.try_into().ok()?);
        Some(if self.little_endian {
            value.swap_bytes()
        } else {
            value
        })
    }
}

/// Byte ranges of the secondary images in the MPF index, primary excluded.
fn mpf_secondary_ranges(file: &[u8]) -> Vec<Range<usize>> {
    let mut ranges = Vec::new();
    for (marker, payload) in segment_payloads(file) {
        if marker != APPLICATION_2 || !file[payload.clone()].starts_with(MPF_IDENTIFIER) {
            continue;
        }
        let header = payload.start + MPF_IDENTIFIER.len();
        let little_endian = match file.get(header..header + 4) {
            Some(b"II\x2A\x00") => true,
            Some(b"MM\x00\x2A") => false,
            _ => continue,
        };
        let reader = MpfReader {
            file,
            header,
            little_endian,
        };
        let Some(entries) = mp_entry_list(&reader) else {
            continue;
        };
        for index in 0..entries.count {
            let entry = entries.offset + index * MP_ENTRY_BYTES;
            let (Some(size), Some(offset)) =
                (reader.read_u32(entry + 4), reader.read_u32(entry + 8))
            else {
                continue;
            };
            // The primary image writes offset zero; every other offset anchors at the MP header.
            if offset == 0 {
                continue;
            }
            let start = header + offset as usize;
            ranges.push(start..start + size as usize);
        }
    }
    ranges
}

struct MpEntryList {
    offset: usize,
    count: usize,
}

/// MP entry array position and image count from the index IFD.
fn mp_entry_list(reader: &MpfReader) -> Option<MpEntryList> {
    let index_ifd = reader.read_u32(4)? as usize;
    let entry_count = reader.read_u16(index_ifd)? as usize;
    for index in 0..entry_count {
        let entry = index_ifd + 2 + index * 12;
        if reader.read_u16(entry)? != MP_ENTRY_TAG {
            continue;
        }
        let byte_count = reader.read_u32(entry + 4)? as usize;
        if byte_count < MP_ENTRY_BYTES {
            return None;
        }
        let offset = reader.read_u32(entry + 8)? as usize;
        // The declared size is a claim; the entries still have to sit inside the file.
        let available = reader.file.len().saturating_sub(reader.header + offset);
        return Some(MpEntryList {
            offset,
            count: byte_count.min(available) / MP_ENTRY_BYTES,
        });
    }
    None
}

const XMP_IDENTIFIER: &[u8] = b"http://ns.adobe.com/xap/1.0/\0";
const HDRGM_NAMESPACE: &str = "http://ns.adobe.com/hdr-gain-map/1.0/";

/// Gain map parameters from the JPEG's XMP packet, when one declares hdrgm.
fn hdrgm_metadata(gain_map_jpeg: &[u8]) -> Option<GainMapMetadata> {
    for (marker, payload) in segment_payloads(gain_map_jpeg) {
        if marker != APPLICATION_1 {
            continue;
        }
        let Some(packet) = gain_map_jpeg[payload].strip_prefix(XMP_IDENTIFIER) else {
            continue;
        };
        let Ok(xml) = std::str::from_utf8(packet) else {
            continue;
        };
        if let Some(metadata) = parse_hdrgm(xml) {
            return Some(metadata);
        }
    }
    None
}

/// The prefix the packet binds to the hdrgm namespace, usually "hdrgm".
fn hdrgm_prefix(xml: &str) -> Option<&str> {
    let mut rest = xml;
    while let Some(position) = rest.find("xmlns:") {
        rest = &rest[position + 6..];
        let equals = rest.find('=')?;
        let prefix = &rest[..equals];
        let value = &rest[equals + 1..];
        let unquoted = value.strip_prefix('"').or_else(|| value.strip_prefix('\''));
        if unquoted.is_some_and(|value| value.starts_with(HDRGM_NAMESPACE))
            && !prefix.is_empty()
            && prefix.len() <= 32
            && prefix
                .chars()
                .all(|character| character.is_ascii_alphanumeric())
        {
            return Some(prefix);
        }
    }
    None
}

/// Property values as text: the attribute value, the element body, or its list items.
fn property_values<'xml>(xml: &'xml str, prefix: &str, name: &str) -> Option<Vec<&'xml str>> {
    let qualified = format!("{prefix}:{name}");
    if let Some(position) = xml.find(&format!("{qualified}=")) {
        let value = &xml[position + qualified.len() + 1..];
        let quote = value.chars().next()?;
        if quote == '"' || quote == '\'' {
            let body = &value[1..];
            return Some(vec![&body[..body.find(quote)?]]);
        }
    }
    let element = &xml[xml.find(&format!("<{qualified}"))?..];
    let body_open = element.find('>')? + 1;
    let body_close = element.find(&format!("</{qualified}"))?;
    // An invalid element can place the close tag before the opening '>'; get() fails closed.
    let body = element.get(body_open..body_close)?;
    let mut items = Vec::new();
    let mut rest = body;
    while let Some(position) = rest.find("<rdf:li") {
        let item = &rest[position..];
        let item_open = item.find('>')? + 1;
        let item_close = item.find("</rdf:li")?;
        let text = item.get(item_open..item_close)?;
        items.push(text.trim());
        rest = &item[item_close + 8..];
    }
    if items.is_empty() {
        items.push(body.trim());
    }
    Some(items)
}

/// One value copied to every plane, or three values in plane order.
fn per_plane(values: &[&str]) -> Option<[f32; 3]> {
    match values {
        [one] => {
            let value = one.parse().ok()?;
            Some([value; 3])
        }
        [red, green, blue] => Some([red.parse().ok()?, green.parse().ok()?, blue.parse().ok()?]),
        _ => None,
    }
}

/// The single scalar the property must hold.
fn scalar(values: &[&str]) -> Option<f32> {
    match values {
        [one] => one.parse().ok(),
        _ => None,
    }
}

/// Reads hdrgm properties; a present-but-unreadable value rejects the packet.
fn parse_hdrgm(xml: &str) -> Option<GainMapMetadata> {
    let prefix = hdrgm_prefix(xml)?;
    // An absent optional property means its default; a present one must read.
    let plane_or = |name: &str, default: [f32; 3]| match property_values(xml, prefix, name) {
        Some(values) => per_plane(&values),
        None => Some(default),
    };
    let scalar_or = |name: &str, default: f32| match property_values(xml, prefix, name) {
        Some(values) => scalar(&values),
        None => Some(default),
    };
    property_values(xml, prefix, "Version")?;
    let gain_map_maximum = per_plane(&property_values(xml, prefix, "GainMapMax")?)?;
    let hdr_capacity_maximum = scalar(&property_values(xml, prefix, "HDRCapacityMax")?)?;
    let gain_map_minimum = plane_or("GainMapMin", [0.0; 3])?;
    let gamma = plane_or("Gamma", [1.0; 3])?;
    let offset_sdr = plane_or("OffsetSDR", [1.0 / 64.0; 3])?;
    let offset_hdr = plane_or("OffsetHDR", [1.0 / 64.0; 3])?;
    let hdr_capacity_minimum = scalar_or("HDRCapacityMin", 0.0)?;
    let base_rendition_is_hdr = match property_values(xml, prefix, "BaseRenditionIsHDR") {
        Some(values) => match values.as_slice() {
            [one] => one.eq_ignore_ascii_case("true"),
            _ => return None,
        },
        None => false,
    };
    Some(GainMapMetadata {
        gain_map_minimum,
        gain_map_maximum,
        gamma,
        offset_sdr,
        offset_hdr,
        hdr_capacity_minimum,
        hdr_capacity_maximum,
        base_rendition_is_hdr,
    })
}

#[cfg(test)]
mod metadata_tests {
    use super::*;

    fn xmp_packet(attributes: &str) -> String {
        format!(
            "<x:xmpmeta xmlns:x=\"adobe:ns:meta/\"><rdf:RDF \
             xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\">\
             <rdf:Description xmlns:hdrgm=\"http://ns.adobe.com/hdr-gain-map/1.0/\" \
             {attributes}/></rdf:RDF></x:xmpmeta>"
        )
    }

    fn jpeg_with_segments(segments: &[(u8, Vec<u8>)]) -> Vec<u8> {
        let mut jpeg = vec![0xFF, 0xD8];
        for (marker, payload) in segments {
            jpeg.extend([0xFF, *marker]);
            jpeg.extend(u16::try_from(payload.len() + 2).unwrap().to_be_bytes());
            jpeg.extend(payload);
        }
        jpeg.extend([0xFF, 0xD9]);
        jpeg
    }

    fn gain_map_jpeg(attributes: &str) -> Vec<u8> {
        let mut payload = XMP_IDENTIFIER.to_vec();
        payload.extend(xmp_packet(attributes).into_bytes());
        jpeg_with_segments(&[(APPLICATION_1, payload)])
    }

    /// Container with an MPF index whose second entry points at the appended JPEG.
    fn ultra_hdr_file(gain_map: &[u8]) -> Vec<u8> {
        let mut index = MPF_IDENTIFIER.to_vec();
        index.extend(b"MM\x00\x2A");
        index.extend(8u32.to_be_bytes());
        index.extend(1u16.to_be_bytes());
        index.extend(MP_ENTRY_TAG.to_be_bytes());
        index.extend(7u16.to_be_bytes());
        index.extend(32u32.to_be_bytes());
        index.extend(26u32.to_be_bytes());
        index.extend(0u32.to_be_bytes());
        let entries_placeholder = index.len();
        index.extend([0u8; 32]);
        let mut file = jpeg_with_segments(&[(APPLICATION_2, index)]);
        let header = file
            .windows(4)
            .position(|window| window == b"MM\x00\x2A")
            .unwrap();
        let gain_map_offset = (file.len() - header) as u32;
        let payload_start = header - MPF_IDENTIFIER.len() + entries_placeholder;
        let second_entry = payload_start + MP_ENTRY_BYTES;
        file[second_entry + 4..second_entry + 8]
            .copy_from_slice(&(gain_map.len() as u32).to_be_bytes());
        file[second_entry + 8..second_entry + 12].copy_from_slice(&gain_map_offset.to_be_bytes());
        file.extend(gain_map);
        file
    }

    const REQUIRED: &str =
        "hdrgm:Version=\"1.0\" hdrgm:GainMapMax=\"4.709\" hdrgm:HDRCapacityMax=\"4.709\"";

    #[test]
    fn a_declared_entry_count_cannot_outrun_the_file() {
        let gain_map = gain_map_jpeg(REQUIRED);
        let mut file = ultra_hdr_file(&gain_map);
        let header = file
            .windows(4)
            .position(|window| window == b"MM\x00\x2A")
            .unwrap();
        // The index claims four gigabytes of entries in a file of a few kilobytes.
        file[header + 14..header + 18].copy_from_slice(&u32::MAX.to_be_bytes());
        let reader = MpfReader {
            file: &file,
            header,
            little_endian: false,
        };
        let entries = mp_entry_list(&reader).expect("entry list");
        assert!(
            entries.count <= file.len() / MP_ENTRY_BYTES,
            "{}",
            entries.count
        );
        // The entries that are really there still parse.
        assert!(find_ultra_hdr(&file).is_some());
    }

    #[test]
    fn finds_gain_map_and_metadata_defaults() {
        let gain_map = gain_map_jpeg(REQUIRED);
        let file = ultra_hdr_file(&gain_map);
        let found = find_ultra_hdr(&file).expect("gain map");
        assert_eq!(&file[found.gain_map_range.clone()], gain_map.as_slice());
        let metadata = found.metadata;
        assert_eq!(metadata.gain_map_maximum, [4.709; 3]);
        assert_eq!(metadata.gain_map_minimum, [0.0; 3]);
        assert_eq!(metadata.gamma, [1.0; 3]);
        assert_eq!(metadata.offset_sdr, [1.0 / 64.0; 3]);
        assert_eq!(metadata.offset_hdr, [1.0 / 64.0; 3]);
        assert_eq!(metadata.hdr_capacity_minimum, 0.0);
        assert_eq!(metadata.hdr_capacity_maximum, 4.709);
        assert!(!metadata.base_rendition_is_hdr);
    }

    #[test]
    fn reads_explicit_values_and_boolean() {
        let attributes = "hdrgm:Version=\"1.0\" hdrgm:GainMapMin=\"-0.5\" \
             hdrgm:GainMapMax=\"3.0\" hdrgm:Gamma=\"1.5\" hdrgm:OffsetSDR=\"0.25\" \
             hdrgm:OffsetHDR=\"0.125\" hdrgm:HDRCapacityMin=\"0.5\" \
             hdrgm:HDRCapacityMax=\"3.0\" hdrgm:BaseRenditionIsHDR=\"True\"";
        let metadata = parse_hdrgm(&xmp_packet(attributes)).expect("metadata");
        assert_eq!(metadata.gain_map_minimum, [-0.5; 3]);
        assert_eq!(metadata.gamma, [1.5; 3]);
        assert_eq!(metadata.offset_sdr, [0.25; 3]);
        assert_eq!(metadata.offset_hdr, [0.125; 3]);
        assert_eq!(metadata.hdr_capacity_minimum, 0.5);
        assert!(metadata.base_rendition_is_hdr);
        // The HDR-base layout parses but stays out of scope, so the find rejects it.
        assert!(!usable(&metadata));
    }

    #[test]
    fn reads_per_plane_list_elements() {
        let xml = xmp_packet(REQUIRED).replace(
            "/></rdf:RDF>",
            "><hdrgm:GainMapMin><rdf:Seq><rdf:li>0.1</rdf:li><rdf:li>0.2</rdf:li>\
             <rdf:li>0.3</rdf:li></rdf:Seq></hdrgm:GainMapMin></rdf:Description></rdf:RDF>",
        );
        let metadata = parse_hdrgm(&xml).expect("metadata");
        assert_eq!(metadata.gain_map_minimum, [0.1, 0.2, 0.3]);
    }

    #[test]
    fn rejects_missing_required_and_empty_capacity_span() {
        let no_version = "hdrgm:GainMapMax=\"4.709\" hdrgm:HDRCapacityMax=\"4.709\"";
        assert_eq!(parse_hdrgm(&xmp_packet(no_version)), None);
        let empty_span = "hdrgm:Version=\"1.0\" hdrgm:GainMapMax=\"4.709\" \
             hdrgm:HDRCapacityMin=\"2.0\" hdrgm:HDRCapacityMax=\"2.0\"";
        let metadata = parse_hdrgm(&xmp_packet(empty_span)).expect("metadata");
        assert!(!usable(&metadata));
    }

    #[test]
    fn weight_tracks_display_headroom() {
        let metadata = parse_hdrgm(&xmp_packet(REQUIRED)).expect("metadata");
        assert_eq!(metadata.weight(0.0), 0.0);
        assert_eq!(metadata.weight(1.0), 0.0);
        assert!((metadata.weight((4.709f32 / 2.0).exp2()) - 0.5).abs() < 1e-6);
        assert!((metadata.weight(4.709f32.exp2()) - 1.0).abs() < 1e-6);
        assert_eq!(metadata.weight(1024.0), 1.0);
        let peak = metadata.capacity_peak_nits();
        assert!((peak - 80.0 * 4.709f32.exp2()).abs() < 0.5);
    }

    #[test]
    fn invalid_elements_fail_closed_instead_of_panicking() {
        // Close tags placed before the opening '>' used to reverse the slice range.
        let element = "<hdrgm:GainMapMax</hdrgm:GainMapMax bar >";
        assert_eq!(property_values(element, "hdrgm", "GainMapMax"), None);
        let list_item = "<hdrgm:GainMapMin><rdf:Seq><rdf:li</rdf:li ></rdf:Seq></hdrgm:GainMapMin>";
        assert_eq!(property_values(list_item, "hdrgm", "GainMapMin"), None);
    }

    #[test]
    fn plain_jpeg_has_no_gain_map() {
        assert_eq!(find_ultra_hdr(&jpeg_with_segments(&[])), None);
        assert_eq!(find_ultra_hdr(b"not a jpeg"), None);
    }

    #[test]
    fn mpf_probe_matches_the_full_parse() {
        use std::io::Cursor;
        let file = ultra_hdr_file(&gain_map_jpeg(REQUIRED));
        assert!(jpeg_carries_mpf(Cursor::new(&file)));
        assert!(!jpeg_carries_mpf(Cursor::new(&jpeg_with_segments(&[]))));
        assert!(!jpeg_carries_mpf(Cursor::new(b"not a jpeg" as &[u8])));
        let other_application_2 = jpeg_with_segments(&[(APPLICATION_2, b"ICC\0data".to_vec())]);
        assert!(!jpeg_carries_mpf(Cursor::new(&other_application_2)));
    }

    #[test]
    #[ignore = "needs test/test_uhdr fixtures"]
    fn fixture_originals_carry_gain_maps() {
        for index in 1..=10 {
            let path =
                format!("test/test_uhdr/Originals/Ultra_HDR_Samples_Originals_{index:02}.jpg");
            let file = std::fs::read(&path).expect("fixture");
            let found = find_ultra_hdr(&file).unwrap_or_else(|| panic!("no gain map: {path}"));
            assert!(found.metadata.hdr_capacity_maximum > 0.0, "{path}");
            let gain_map = &file[found.gain_map_range.clone()];
            assert_eq!(gain_map.get(0..2), Some(&[0xFF, 0xD8][..]), "{path}");
        }
    }

    #[test]
    #[ignore = "needs test/test_uhdr fixtures"]
    fn fixture_emulations_are_plain_jpegs() {
        for index in 1..=10 {
            for kind in ["base", "hdr"] {
                let path = format!(
                    "test/test_uhdr/SDR Emulation/Ultra_HDR_Samples_Emulated_{index:02}_{kind}.jpg"
                );
                let file = std::fs::read(&path).expect("fixture");
                assert_eq!(find_ultra_hdr(&file), None, "{path}");
            }
        }
    }
}
