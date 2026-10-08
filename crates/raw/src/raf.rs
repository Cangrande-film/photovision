//! Fujifilm RAF with uncompressed sensor data (Bayer and X-Trans).
//!
//! ## Container
//!
//! A RAF file starts with `FUJIFILMCCD-RAW `, a format version, a camera ID
//! and the model name (32 bytes at offset 28), followed by a big-endian
//! directory: at byte 84 the offset and length of the camera's JPEG (whose
//! EXIF gives make, model and orientation), at 92 the offset and length of a
//! metadata block, and at 100 the offset and length of the sensor-data
//! container. The metadata block is a big-endian record count followed by
//! records of a 16-bit tag, a 16-bit byte length and the payload; the records
//! used here are RawImageFullSize (0x0100, height then width),
//! RawImageCropTopLeft (0x0110, top then left), RawImageCroppedSize (0x0111,
//! height then width), XTransLayout (0x0131, 36 bytes) and WB_GRGBLevels
//! (0x2FF0).
//!
//! The sensor-data container of the X-series bodies is a small TIFF (byte
//! order from its own header) whose IFD0 points through tag 0xF000 to the
//! Fujifilm raw IFD: RawImageFullWidth / Height (0xF001 / 0xF002),
//! BitsPerSample (0xF003), StripOffsets / StripByteCounts (0xF007 / 0xF008,
//! relative to the container), BlackLevel (0xF00A, one value per CFA cell)
//! and WB_GRBLevels (0xF00E). Files without that container (older SuperCCD
//! models) are not decoded.
//!
//! ## Sensor data (established by observation of sample files)
//!
//! * Strip byte count = width × height × 2: 16-bit samples in the
//!   container's byte order (X-E2, 14-bit).
//! * Strip byte count = width × height × 1.5 with 12 bits per sample: pairs of
//!   samples packed into three bytes least-significant bits first,
//!   `s0 = b0 | (b1 & 0x0F) << 8`, `s1 = b1 >> 4 | b2 << 4` (X-E1, X-A1; the
//!   most-significant-first reading gives noise).
//! * Anything smaller is Fujifilm's compressed (lossless or lossy) RAF, whose
//!   code has no public description: unsupported, the JPEG preview is used.
//!
//! The 36 XTransLayout bytes (0 = red, 1 = green, 2 = blue) list the 6×6
//! pattern in reverse order from the sensor-data origin: the cell at data
//! (x, y) has colour `layout[35 - (6 · (y mod 6) + x mod 6)]`. This was
//! established on X-E1 and X-E2 samples, whose patterns are stored with
//! different phases: per-cell means over a grid of image regions cluster
//! into the green set only under this reading, and red and blue were told
//! apart by comparing the developed image with the camera's JPEG. Bayer RAFs
//! (X-A series) have no layout record: the green diagonal is measured from
//! the data as for CR2, with red in the even columns (RGGB on the X-A1).
//!
//! Sources: Phil Harvey's ExifTool FujiFilm tag documentation (RAF header,
//! RAF and FujiIFD tags), the libopenraw RAF format write-up (prose), and CC0
//! samples from raw.pixls.us.

use crate::cr2::{clip_level, measured_red_row};
use crate::error::{RawError, Result};
use crate::sensor::{BlackLevels, Cfa, Rect, Sensor};
use crate::tiff::{Tiff, tag};
use crate::{Limits, RawFormat};

pub(crate) const MAGIC: &[u8] = b"FUJIFILMCCD-RAW";

const FUJI_IFD: u16 = 0xF000;
const RAW_WIDTH: u16 = 0xF001;
const RAW_HEIGHT: u16 = 0xF002;
const RAW_BITS: u16 = 0xF003;
const RAW_OFFSET: u16 = 0xF007;
const RAW_LENGTH: u16 = 0xF008;
const RAW_BLACK: u16 = 0xF00A;
const RAW_WB_GRB: u16 = 0xF00E;

const META_CROP_TOP_LEFT: u16 = 0x0110;
const META_CROPPED_SIZE: u16 = 0x0111;
const META_XTRANS_LAYOUT: u16 = 0x0131;
const META_WB_GRGB: u16 = 0x2FF0;

/// Most metadata records read.
const MAX_RECORDS: usize = 4096;

fn be32(b: &[u8], at: usize) -> Option<usize> {
    let s: [u8; 4] = b.get(at..at.checked_add(4)?)?.try_into().ok()?;
    Some(u32::from_be_bytes(s) as usize)
}

fn be16s(v: &[u8]) -> Vec<u16> {
    v.as_chunks::<2>().0.iter().map(|c| u16::from_be_bytes(*c)).collect()
}

/// The JPEG preview's (offset, length) from the RAF directory.
pub(crate) fn jpeg_range(b: &[u8]) -> Option<(usize, usize)> {
    Some((be32(b, 84)?, be32(b, 88)?))
}

/// The metadata records as (tag, payload); damaged records end the list.
fn meta_records(b: &[u8]) -> Vec<(u16, &[u8])> {
    let mut out = Vec::new();
    let (Some(off), Some(len)) = (be32(b, 92), be32(b, 96)) else { return out };
    let Some(block) = off.checked_add(len).and_then(|end| b.get(off..end)) else { return out };
    let Some(count) = be32(block, 0) else { return out };
    let mut pos = 4usize;
    for _ in 0..count.min(MAX_RECORDS) {
        let Some(head) = block.get(pos..pos + 4) else { break };
        let tag = u16::from_be_bytes([head[0], head[1]]);
        let size = usize::from(u16::from_be_bytes([head[2], head[3]]));
        let Some(payload) = block.get(pos + 4..pos + 4 + size) else { break };
        out.push((tag, payload));
        pos += 4 + size;
    }
    out
}

/// The EXIF TIFF inside a JPEG's APP1 segment.
pub(crate) fn jpeg_exif(jpeg: &[u8]) -> Option<Tiff<'_>> {
    if jpeg.get(0..2)? != [0xFF, 0xD8] {
        return None;
    }
    let mut pos = 2usize;
    for _ in 0..64 {
        if *jpeg.get(pos)? != 0xFF {
            return None;
        }
        let m = *jpeg.get(pos + 1)?;
        let len = usize::from(u16::from_be_bytes([*jpeg.get(pos + 2)?, *jpeg.get(pos + 3)?]));
        if m == 0xDA || m == 0xD9 || len < 2 {
            return None;
        }
        let body = jpeg.get(pos + 4..pos + 2 + len)?;
        if m == 0xE1 && body.starts_with(b"Exif\0\0") {
            return Tiff::new(body.get(6..)?);
        }
        pos = pos.checked_add(2 + len)?;
    }
    None
}

/// Unpacks 12-bit samples stored two per three bytes, least-significant bits first.
fn unpack12(src: &[u8], n: usize) -> Option<Vec<u16>> {
    let bytes = src.get(..n.checked_mul(3)? / 2)?;
    let mut out = Vec::with_capacity(n);
    for c in bytes.as_chunks::<3>().0 {
        out.push(u16::from(c[0]) | (u16::from(c[1] & 0x0F) << 8));
        out.push(u16::from(c[1] >> 4) | (u16::from(c[2]) << 4));
    }
    (out.len() == n).then_some(out)
}

pub(crate) fn decode(b: &[u8], limits: &Limits) -> Result<Sensor> {
    if !b.starts_with(MAGIC) {
        return Err(RawError::NotRaw);
    }
    let (cfa_off, cfa_len) = match (be32(b, 100), be32(b, 104)) {
        (Some(o), Some(l)) => (o, l),
        _ => return Err(RawError::malformed("RAF directory is truncated")),
    };
    let container = cfa_off
        .checked_add(cfa_len)
        .and_then(|end| b.get(cfa_off..end))
        .or_else(|| b.get(cfa_off..))
        .ok_or_else(|| RawError::malformed("RAF sensor data lies outside the file"))?;
    let t = Tiff::new(container).ok_or_else(|| RawError::unsupported("RAF without a raw IFD (older Fujifilm models)"))?;
    let ifd0 = t.ifd_at(t.first_ifd, 0).ok_or_else(|| RawError::malformed("RAF raw container has no IFD"))?;
    let raw_at = t.tag_uint(&ifd0, FUJI_IFD).ok_or_else(|| RawError::unsupported("RAF without a raw IFD (older Fujifilm models)"))?;
    let raw = t.ifd_at(raw_at as usize, 0).ok_or_else(|| RawError::malformed("RAF raw IFD is damaged"))?;
    let width = t.tag_uint(&raw, RAW_WIDTH).unwrap_or(0) as usize;
    let height = t.tag_uint(&raw, RAW_HEIGHT).unwrap_or(0) as usize;
    let bits = t.tag_uint(&raw, RAW_BITS).unwrap_or(0);
    limits.check(width as u64, height as u64, 2)?;
    if !(8..=16).contains(&bits) {
        return Err(RawError::unsupported(format!("{bits}-bit RAF data")));
    }
    let off = t.tag_uint(&raw, RAW_OFFSET).ok_or_else(|| RawError::malformed("RAF raw data offset missing"))? as usize;
    let len = t.tag_uint(&raw, RAW_LENGTH).ok_or_else(|| RawError::malformed("RAF raw data length missing"))? as usize;
    let n = width * height;
    let src = off.checked_add(len).and_then(|end| container.get(off..end)).ok_or_else(|| RawError::malformed("RAF raw data lies outside the file"))?;
    let data = if len >= n * 2 {
        let le = t.le;
        src.get(..n * 2)
            .ok_or_else(|| RawError::malformed("RAF raw data is truncated"))?
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| if le { u16::from_le_bytes(*c) } else { u16::from_be_bytes(*c) })
            .collect()
    } else if bits == 12 && width.is_multiple_of(2) && len >= n / 2 * 3 {
        unpack12(src, n).ok_or_else(|| RawError::malformed("RAF raw data is truncated"))?
    } else {
        let kind = match b.get(108..112) {
            Some([0, 0, 0, 2]) => "lossless ",
            Some([0, 0, 0, 3]) => "lossy ",
            _ => "",
        };
        return Err(RawError::unsupported(format!("Fujifilm {kind}compressed RAF (no public description of the code)")));
    };

    let mut warnings = Vec::new();
    let meta = meta_records(b);
    let rec = |tag: u16| meta.iter().find(|(t, _)| *t == tag).map(|(_, p)| *p);
    let full = Rect::new(0, 0, width, height);

    // Black level: one value per CFA cell (stored in the layout's reverse order for 6×6).
    let blacks: Vec<f32> = t.tag_uints(&raw, RAW_BLACK).into_iter().map(|v| v as f32).collect();
    let black = match blacks.len() {
        0 => {
            warnings.push("no black level in the file; assumed to be 0".to_string());
            BlackLevels::uniform(0.0)
        }
        _ if blacks.windows(2).all(|w| w[0] == w[1]) => BlackLevels::uniform(blacks[0]),
        4 => BlackLevels { rows: 2, cols: 2, values: blacks, delta_h: Vec::new(), delta_v: Vec::new() },
        36 => BlackLevels { rows: 6, cols: 6, values: blacks.into_iter().rev().collect(), delta_h: Vec::new(), delta_v: Vec::new() },
        k => BlackLevels::uniform(blacks.iter().sum::<f32>() / k as f32),
    };

    let white = clip_level(&data, bits);

    // As-shot white balance: FujiIFD WB_GRBLevels, else the RAF WB_GRGBLevels record.
    let grb = t.tag_uints(&raw, RAW_WB_GRB);
    let camera_wb = match (grb.as_slice(), rec(META_WB_GRGB).map(be16s).as_deref()) {
        ([g, r, b, ..], _) if *g > 0 && *r > 0 && *b > 0 => Some([f64::from(*r) / f64::from(*g), 1.0, f64::from(*b) / f64::from(*g)]),
        (_, Some([g, r, _, b, ..])) if *g > 0 && *r > 0 && *b > 0 => Some([f64::from(*r) / f64::from(*g), 1.0, f64::from(*b) / f64::from(*g)]),
        _ => None,
    };

    // Image area: crop top-left and cropped size.
    let crop = match (rec(META_CROP_TOP_LEFT).map(be16s).as_deref(), rec(META_CROPPED_SIZE).map(be16s).as_deref()) {
        (Some([top, left, ..]), Some([h, w, ..])) => Rect::new(usize::from(*left), usize::from(*top), usize::from(*w), usize::from(*h)).intersect(&full),
        _ => full,
    };
    let crop = if crop.is_empty() { full } else { crop };

    // Colour filter pattern.
    let cfa = match rec(META_XTRANS_LAYOUT) {
        Some(l) if l.len() == 36 => {
            let colors: Vec<u8> = (0..36).map(|i| l[35 - i]).collect();
            let c = Cfa { width: 6, height: 6, colors, origin_x: 0, origin_y: 0 };
            if !c.is_three_colour() {
                return Err(RawError::malformed("RAF X-Trans layout is not a red/green/blue pattern"));
            }
            c
        }
        _ => {
            let mean_black = black.values.iter().sum::<f32>() / black.values.len().max(1) as f32;
            let red_row = measured_red_row(&data, width, crop, mean_black, white).unwrap_or(0);
            let colors = if red_row == 0 { vec![0, 1, 1, 2] } else { vec![1, 2, 0, 1] };
            Cfa { width: 2, height: 2, colors, origin_x: 0, origin_y: 0 }
        }
    };


    // Make, model and orientation from the JPEG's EXIF; the model also sits in the header.
    let exif = jpeg_range(b).and_then(|(o, l)| b.get(o..o.checked_add(l)?)).and_then(jpeg_exif);
    let exif_ifd0 = exif.as_ref().and_then(|e| e.ifd_at(e.first_ifd, 0));
    let exif_ascii = |tg: u16| exif.as_ref().zip(exif_ifd0.as_ref()).and_then(|(e, i)| e.tag_ascii(i, tg));
    let header_model = b.get(28..60).map(|m| String::from_utf8_lossy(m.split(|c| *c == 0).next().unwrap_or(&[])).trim().to_string()).filter(|m| !m.is_empty());
    let orientation = exif
        .as_ref()
        .zip(exif_ifd0.as_ref())
        .and_then(|(e, i)| e.tag_uint(i, tag::ORIENTATION))
        .map(|o| o as u16)
        .filter(|o| (1..=8).contains(o))
        .unwrap_or(1);

    Ok(Sensor {
        format: RawFormat::Raf,
        make: exif_ascii(tag::MAKE).or_else(|| Some("FUJIFILM".to_string())),
        model: exif_ascii(tag::MODEL).or(header_model),
        width,
        height,
        samples: 1,
        data,
        cfa: Some(cfa),
        linearization: None,
        black,
        white: [white; 3],
        active: full,
        crop,
        color: Default::default(),
        camera_wb,
        orientation,
        baseline_exposure: 0.0,
        gain_maps: Vec::new(),
        warnings,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn twelve_bit_unpacking_is_lsb_first() {
        // 0xABC, 0x123 → bytes BC 3A 12.
        assert_eq!(unpack12(&[0xBC, 0x3A, 0x12], 2), Some(vec![0xABC, 0x123]));
        assert_eq!(unpack12(&[0xBC, 0x3A], 2), None);
    }

    #[test]
    fn meta_records_stop_at_damage() {
        let mut b = vec![0u8; 120];
        b[92..96].copy_from_slice(&120u32.to_be_bytes());
        // Claims 1000 records, holds one complete and one cut short.
        let block = [0, 0, 3, 232, 0x01, 0x00, 0, 4, 1, 2, 3, 4, 0x01, 0x10, 0, 9, 1];
        b[96..100].copy_from_slice(&(block.len() as u32).to_be_bytes());
        b.extend_from_slice(&block);
        let r = meta_records(&b);
        assert_eq!(r, vec![(0x0100, &[1u8, 2, 3, 4][..])]);
    }

    #[test]
    fn exif_in_jpeg() {
        assert!(jpeg_exif(&[0xFF, 0xD8, 0xFF, 0xE1, 0x00, 0x01]).is_none());
        assert!(jpeg_exif(b"not a jpeg").is_none());
    }
}
