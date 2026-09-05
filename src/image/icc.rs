//! Reading an ICC profile: what space it describes, and what it calls itself.

use crate::image::color;

/// Big-endian u32 at the offset, when in bounds.
fn read_u32_be(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_be_bytes(
        bytes.get(offset..offset + 4)?.try_into().ok()?,
    ))
}

/// Data offset of the first tag-table entry with this signature.
pub fn tag_offset(icc: &[u8], signature: &[u8; 4]) -> Option<usize> {
    let tag_count = read_u32_be(icc, 128)? as usize;
    for index in 0..tag_count {
        let entry = 132 + index * 12;
        if icc.get(entry..entry + 4)? == signature {
            return read_u32_be(icc, entry + 4).map(|offset| offset as usize);
        }
    }
    None
}

/// Big-endian u16 at the offset, when in bounds.
fn read_u16_be(bytes: &[u8], offset: usize) -> Option<u16> {
    Some(u16::from_be_bytes(
        bytes.get(offset..offset + 2)?.try_into().ok()?,
    ))
}

/// The signed 16.16 fixed point every ICC numeric tag is written in.
fn read_s15_fixed16(bytes: &[u8], offset: usize) -> Option<f32> {
    Some(read_u32_be(bytes, offset)? as i32 as f32 / 65536.0)
}

/// True when two runs of floats agree everywhere within the tolerance.
fn all_pairs_within<'a>(
    one: impl IntoIterator<Item = &'a f32>,
    other: impl IntoIterator<Item = &'a f32>,
    tolerance: f32,
) -> bool {
    one.into_iter()
        .zip(other)
        .all(|(one, other)| (one - other).abs() < tolerance)
}

/// Points where tone curves are compared; close enough to separate the sRGB toe from a gamma.
const TONE_CURVE_SAMPLES: usize = 33;
/// Curves this close describe one response; a pure gamma 2.2 leaves the sRGB curve by 4e-3.
const TONE_CURVE_TOLERANCE: f32 = 1e-3;
/// Primaries this close in xy describe one gamut; the nearest named gamuts sit 0.04 apart.
const PRIMARY_TOLERANCE: f32 = 2e-3;
/// Colorants this close describe one space, scale included.
const COLORANT_TOLERANCE: f32 = 1e-3;

fn tone_curve_input(index: usize) -> f32 {
    index as f32 / (TONE_CURVE_SAMPLES - 1) as f32
}

/// True when the profile describes the space sRGB does.
pub fn is_srgb(icc: &[u8]) -> bool {
    // Chromaticity, not the stored colorants: two vendors round their sRGB differently.
    let Some(primaries) = primaries(icc) else {
        return false;
    };
    if !all_pairs_within(
        primaries.iter().flatten(),
        color::BT709_PRIMARIES.iter().flatten(),
        PRIMARY_TOLERANCE,
    ) {
        return false;
    }
    let reference: [f32; TONE_CURVE_SAMPLES] =
        std::array::from_fn(|index| color::srgb_to_linear(tone_curve_input(index)));
    tone_curves(icc).is_some_and(|curves| {
        curves
            .iter()
            .all(|curve| all_pairs_within(curve, &reference, TONE_CURVE_TOLERANCE))
    })
}

/// True when both profiles describe one space, so converting between them changes nothing.
pub fn same_space(one: &[u8], other: &[u8]) -> bool {
    // Both sides sit in the D50 connection space, and their scale carries the medium white.
    let (Some(one_colorants), Some(other_colorants)) = (colorants(one), colorants(other)) else {
        return false;
    };
    if !all_pairs_within(
        one_colorants.iter().flatten(),
        other_colorants.iter().flatten(),
        COLORANT_TOLERANCE,
    ) {
        return false;
    }
    let (Some(one), Some(other)) = (tone_curves(one), tone_curves(other)) else {
        return false;
    };
    all_pairs_within(
        one.iter().flatten(),
        other.iter().flatten(),
        TONE_CURVE_TOLERANCE,
    )
}

/// R, G, B tone curves sampled over [0, 1]; None when one is missing or of a form riv cannot read.
fn tone_curves(icc: &[u8]) -> Option<[[f32; TONE_CURVE_SAMPLES]; 3]> {
    Some([
        tone_curve(icc, b"rTRC")?,
        tone_curve(icc, b"gTRC")?,
        tone_curve(icc, b"bTRC")?,
    ])
}

fn tone_curve(icc: &[u8], tag: &[u8; 4]) -> Option<[f32; TONE_CURVE_SAMPLES]> {
    let offset = tag_offset(icc, tag)?;
    match icc.get(offset..offset + 4)? {
        b"curv" => {
            let count = read_u32_be(icc, offset + 8)? as usize;
            match count {
                // No entries is the identity; one entry is a u8Fixed8 gamma.
                0 => Some(std::array::from_fn(tone_curve_input)),
                1 => {
                    let gamma = f32::from(read_u16_be(icc, offset + 12)?) / 256.0;
                    Some(std::array::from_fn(|index| {
                        tone_curve_input(index).powf(gamma)
                    }))
                }
                _ => {
                    let entry = |index: usize| -> Option<f32> {
                        Some(f32::from(read_u16_be(icc, offset + 12 + index * 2)?) / 65535.0)
                    };
                    let mut samples = [0.0f32; TONE_CURVE_SAMPLES];
                    for (index, sample) in samples.iter_mut().enumerate() {
                        let position = tone_curve_input(index) * (count - 1) as f32;
                        let low = position.floor() as usize;
                        let high = (low + 1).min(count - 1);
                        let fraction = position - low as f32;
                        *sample = entry(low)? * (1.0 - fraction) + entry(high)? * fraction;
                    }
                    Some(samples)
                }
            }
        }
        b"para" => {
            let parameter = |index: usize| read_s15_fixed16(icc, offset + 12 + index * 4);
            let gamma = parameter(0)?;
            let function = read_u16_be(icc, offset + 8)?;
            // Every function is (a*x + b)^g + e above d, and c*x + f below it.
            let (a, b, slope, split, above, below) = match function {
                0 => (1.0, 0.0, 0.0, f32::MIN, 0.0, 0.0),
                1 | 2 => {
                    let (a, b) = (parameter(1)?, parameter(2)?);
                    if a == 0.0 {
                        return None;
                    }
                    let constant = if function == 2 { parameter(3)? } else { 0.0 };
                    (a, b, 0.0, -b / a, constant, constant)
                }
                3 => (
                    parameter(1)?,
                    parameter(2)?,
                    parameter(3)?,
                    parameter(4)?,
                    0.0,
                    0.0,
                ),
                4 => (
                    parameter(1)?,
                    parameter(2)?,
                    parameter(3)?,
                    parameter(4)?,
                    parameter(5)?,
                    parameter(6)?,
                ),
                _ => return None,
            };
            Some(std::array::from_fn(|index| {
                let input = tone_curve_input(index);
                if input >= split {
                    (a * input + b).max(0.0).powf(gamma) + above
                } else {
                    slope * input + below
                }
            }))
        }
        _ => None,
    }
}

/// Nearest gamut label from an ICC's matrix primaries; None for non-matrix profiles.
pub fn gamut_label(icc: &[u8]) -> Option<&'static str> {
    Some(color::nearest_gamut_label(primaries(icc)?))
}

/// Bradford adaptation out of the ICC PCS white (D50) into the D65 named gamuts use.
const D50_TO_D65: [[f32; 3]; 3] = [
    [0.955_577, -0.023_039, 0.063_164],
    [-0.028_290, 1.009_942, 0.021_008],
    [0.012_298, -0.020_483, 1.329_91],
];

/// R, G, B colorants as the profile stores them, in the D50 PCS; None for non-matrix profiles.
fn colorants(icc: &[u8]) -> Option<[[f32; 3]; 3]> {
    let column = |tag: &[u8; 4]| -> Option<[f32; 3]> {
        let offset = tag_offset(icc, tag)?;
        if icc.get(offset..offset + 4)? != b"XYZ " {
            return None;
        }
        Some([
            read_s15_fixed16(icc, offset + 8)?,
            read_s15_fixed16(icc, offset + 12)?,
            read_s15_fixed16(icc, offset + 16)?,
        ])
    };
    Some([column(b"rXYZ")?, column(b"gXYZ")?, column(b"bXYZ")?])
}

/// CIE xy of an ICC's matrix primaries, D65 referred; None for non-matrix profiles.
pub fn primaries(icc: &[u8]) -> Option<[[f32; 2]; 3]> {
    let mut primaries = [[0.0f32; 2]; 3];
    for (stored, primary) in colorants(icc)?.iter().zip(&mut primaries) {
        // The colorants are stored against the D50 PCS; adapt out of it to name the gamut.
        let mut adapted = [0.0f32; 3];
        for (row, channel) in D50_TO_D65.iter().zip(&mut adapted) {
            *channel = row[0] * stored[0] + row[1] * stored[1] + row[2] * stored[2];
        }
        let sum = adapted[0] + adapted[1] + adapted[2];
        if sum <= 0.0 {
            return None;
        }
        *primary = [adapted[0] / sum, adapted[1] / sum];
    }
    Some(primaries)
}

/// Human-readable profile name from the ICC 'desc' tag (v2 text or v4 mluc).
pub fn profile_description(icc: &[u8]) -> Option<String> {
    let offset = tag_offset(icc, b"desc")?;
    let description = match icc.get(offset..offset + 4)? {
        b"desc" => {
            let length = read_u32_be(icc, offset + 8)? as usize;
            let bytes = icc.get(offset + 12..offset + 12 + length)?;
            let end = bytes
                .iter()
                .position(|&byte| byte == 0)
                .unwrap_or(bytes.len());
            std::str::from_utf8(&bytes[..end]).ok()?.to_string()
        }
        b"mluc" => {
            // First record: length at +20, offset at +24, UTF-16BE.
            let length = read_u32_be(icc, offset + 20)? as usize;
            let start = offset + read_u32_be(icc, offset + 24)? as usize;
            let bytes = icc.get(start..start + length)?;
            let units = bytes
                .as_chunks::<2>()
                .0
                .iter()
                .map(|pair| u16::from_be_bytes(*pair));
            char::decode_utf16(units)
                .collect::<Result<String, _>>()
                .ok()?
        }
        _ => return None,
    };
    let trimmed = description.trim_matches(['\0', ' ', '\t', '\r', '\n']);
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

#[cfg(test)]
mod description_tests {
    use super::*;

    fn profile(tag_type: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut icc = vec![0u8; 128];
        icc.extend_from_slice(&1u32.to_be_bytes());
        icc.extend_from_slice(b"desc");
        icc.extend_from_slice(&144u32.to_be_bytes());
        icc.extend_from_slice(&(8 + payload.len() as u32).to_be_bytes());
        icc.extend_from_slice(tag_type);
        icc.extend_from_slice(&[0u8; 4]);
        icc.extend_from_slice(payload);
        icc
    }

    #[test]
    fn reads_a_version2_text_description() {
        let name = b"Adobe RGB (1998)\0";
        let mut payload = (name.len() as u32).to_be_bytes().to_vec();
        payload.extend_from_slice(name);
        let icc = profile(b"desc", &payload);
        assert_eq!(
            profile_description(&icc).as_deref(),
            Some("Adobe RGB (1998)")
        );
    }

    #[test]
    fn reads_a_version4_mluc_description() {
        let text: Vec<u8> = "Display P3"
            .encode_utf16()
            .flat_map(|unit| unit.to_be_bytes())
            .collect();
        let mut payload = 1u32.to_be_bytes().to_vec();
        payload.extend_from_slice(&12u32.to_be_bytes());
        payload.extend_from_slice(b"enUS");
        payload.extend_from_slice(&(text.len() as u32).to_be_bytes());
        payload.extend_from_slice(&28u32.to_be_bytes());
        payload.extend_from_slice(&text);
        let icc = profile(b"mluc", &payload);
        assert_eq!(profile_description(&icc).as_deref(), Some("Display P3"));
    }

    #[test]
    fn garbage_profiles_yield_none() {
        assert_eq!(profile_description(&[0u8; 16]), None);
        assert_eq!(profile_description(b"not an icc profile"), None);
    }
}

#[cfg(test)]
mod space_tests {
    use super::*;

    /// sRGB colorants as a profile stores them, in the D50 connection space.
    const SRGB_COLORANTS: [[f32; 3]; 3] = [
        [0.4360, 0.2225, 0.0139],
        [0.3851, 0.7169, 0.0971],
        [0.1431, 0.0606, 0.7141],
    ];
    /// Display P3 colorants, likewise D50 referred.
    const DISPLAY_P3_COLORANTS: [[f32; 3]; 3] = [
        [0.5151, 0.2412, -0.0011],
        [0.2920, 0.6922, 0.0419],
        [0.1571, 0.0666, 0.7841],
    ];

    fn fixed(value: f32) -> [u8; 4] {
        ((value * 65536.0).round() as i32).to_be_bytes()
    }

    fn profile(colorants: [[f32; 3]; 3], curve: &[u8]) -> Vec<u8> {
        let names: [&[u8; 4]; 6] = [b"rXYZ", b"gXYZ", b"bXYZ", b"rTRC", b"gTRC", b"bTRC"];
        let payloads: Vec<Vec<u8>> = colorants
            .iter()
            .map(|values| {
                let mut tag = b"XYZ \0\0\0\0".to_vec();
                values.iter().for_each(|value| tag.extend(fixed(*value)));
                tag
            })
            .chain(std::iter::repeat_n(curve.to_vec(), 3))
            .collect();
        let mut icc = vec![0u8; 128];
        icc.extend_from_slice(&(names.len() as u32).to_be_bytes());
        let mut offset = 132 + names.len() * 12;
        for (name, payload) in names.iter().zip(&payloads) {
            icc.extend_from_slice(*name);
            icc.extend_from_slice(&(offset as u32).to_be_bytes());
            icc.extend_from_slice(&(payload.len() as u32).to_be_bytes());
            offset += payload.len();
        }
        payloads
            .iter()
            .for_each(|payload| icc.extend_from_slice(payload));
        icc
    }

    /// Parametric curve type 3, which is how a version 4 profile writes sRGB.
    fn parametric_srgb_curve() -> Vec<u8> {
        let mut tag = b"para\0\0\0\0".to_vec();
        tag.extend_from_slice(&3u16.to_be_bytes());
        tag.extend_from_slice(&[0u8; 2]);
        for value in [2.4, 1.0 / 1.055, 0.055 / 1.055, 1.0 / 12.92, 0.04045] {
            tag.extend(fixed(value));
        }
        tag
    }

    /// Sampled curve, which is how a version 2 profile writes the same response.
    fn sampled_curve(shape: impl Fn(f32) -> f32) -> Vec<u8> {
        const ENTRIES: usize = 1024;
        let mut tag = b"curv\0\0\0\0".to_vec();
        tag.extend_from_slice(&(ENTRIES as u32).to_be_bytes());
        for index in 0..ENTRIES {
            let value = shape(index as f32 / (ENTRIES - 1) as f32);
            tag.extend_from_slice(&((value * 65535.0).round() as u16).to_be_bytes());
        }
        tag
    }

    fn gamma_curve(gamma: f32) -> Vec<u8> {
        let mut tag = b"curv\0\0\0\0".to_vec();
        tag.extend_from_slice(&1u32.to_be_bytes());
        tag.extend_from_slice(&((gamma * 256.0).round() as u16).to_be_bytes());
        tag
    }

    #[test]
    fn both_ways_of_writing_srgb_read_as_srgb() {
        assert!(is_srgb(&profile(SRGB_COLORANTS, &parametric_srgb_curve())));
        assert!(is_srgb(&profile(
            SRGB_COLORANTS,
            &sampled_curve(color::srgb_to_linear)
        )));
    }

    #[test]
    fn a_pure_gamma_is_not_the_srgb_curve() {
        // The two curves diverge below the toe.
        assert!(!is_srgb(&profile(SRGB_COLORANTS, &gamma_curve(2.2))));
        assert!(!is_srgb(&profile(
            SRGB_COLORANTS,
            &sampled_curve(|value| value.powf(2.2))
        )));
    }

    #[test]
    fn a_wider_gamut_is_not_srgb_whatever_its_curve() {
        assert!(!is_srgb(&profile(
            DISPLAY_P3_COLORANTS,
            &parametric_srgb_curve()
        )));
    }

    #[test]
    fn a_profile_riv_cannot_read_is_never_srgb() {
        assert!(!is_srgb(&[0u8; 16]));
        assert!(!is_srgb(b"not an icc profile"));
        // Matrix primaries but no tone curves.
        let mut without_curves = profile(SRGB_COLORANTS, &parametric_srgb_curve());
        without_curves[128..132].copy_from_slice(&3u32.to_be_bytes());
        assert!(!is_srgb(&without_curves));
    }

    #[test]
    fn one_space_written_two_ways_compares_equal() {
        let version2 = profile(SRGB_COLORANTS, &sampled_curve(color::srgb_to_linear));
        let version4 = profile(SRGB_COLORANTS, &parametric_srgb_curve());
        assert!(same_space(&version2, &version4));
        assert!(same_space(&version4, &version2));
    }

    #[test]
    fn one_gamut_at_two_white_points_compares_unequal() {
        // Scaling a colorant keeps its chromaticity and moves the white the three add up to.
        let scaled = |column: [f32; 3], factor: f32| column.map(|value| value * factor);
        let shifted = [
            scaled(SRGB_COLORANTS[0], 0.94),
            scaled(SRGB_COLORANTS[1], 1.05),
            SRGB_COLORANTS[2],
        ];
        let one = profile(SRGB_COLORANTS, &parametric_srgb_curve());
        let other = profile(shifted, &parametric_srgb_curve());
        let (Some(one_xy), Some(other_xy)) = (primaries(&one), primaries(&other)) else {
            panic!("both profiles carry matrix primaries");
        };
        assert!(
            one_xy
                .iter()
                .flatten()
                .zip(other_xy.iter().flatten())
                .all(|(one, other)| (one - other).abs() < 1e-4),
            "the chromaticities are the same, so they alone cannot separate the two"
        );
        assert!(!same_space(&one, &other));
    }

    #[test]
    fn a_different_gamut_or_curve_compares_unequal() {
        let srgb = profile(SRGB_COLORANTS, &parametric_srgb_curve());
        let wide = profile(DISPLAY_P3_COLORANTS, &parametric_srgb_curve());
        let gamma = profile(SRGB_COLORANTS, &gamma_curve(2.2));
        assert!(!same_space(&srgb, &wide));
        assert!(!same_space(&srgb, &gamma));
        assert!(same_space(&wide, &wide));
    }
}
