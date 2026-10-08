//! Canon CR3: the ISO base media file format container (ISO/IEC 14496-12).
//!
//! A CR3 file is a sequence of boxes (32-bit big-endian size, four-character
//! type; size 1 means a 64-bit size follows, size 0 runs to the end of the
//! enclosing box; `uuid` boxes carry a 16-byte extended type). The layout,
//! from Laurent Clévy's "Describing the Canon Raw v3 (CR3) file format"
//! (a prose description) and checked against samples:
//!
//! * `ftyp` with major brand `crx `.
//! * `moov` holding the Canon `uuid` box 85c0b687-820f-11e0-8111-f4ce462b6a48
//!   (`CNCV` compressor version, `CMT1`–`CMT4` TIFF metadata: IFD0, EXIF,
//!   maker note, GPS; `THMB` a 160×120 JPEG thumbnail) and one `trak` per
//!   image. Each track's `stsd` holds a `CRAW` visual sample entry whose
//!   child is either `JPEG` (track 1: the full-size camera JPEG) or `CMP1`
//!   (a CRX-coded raw image: width, height, tile size, bit depth, plane
//!   count / CFA layout and wavelet levels), and the image's position in
//!   `mdat` comes from `co64` (or `stco`) and its size from `stsz`.
//! * A `uuid` box eaf42b5e-1c98-4b88-b9fb-b7dc406e4d16 holding `PRVW`, a
//!   1620×1080 JPEG preview.
//!
//! The sensor data itself is CRX-coded (tiles → planes → subbands; lossless
//! "RAW" with an adaptive Golomb-Rice code, run mode and median prediction;
//! "C-RAW" adds a Le Gall 5/3 wavelet and quantization). The public prose
//! descriptions and Canon's patent (US 2016/0323602 A1) give the general
//! shape but not the bit-exact entropy code (parameter adaptation rules,
//! run-length tables, escape codes), whose only complete descriptions are
//! decoder source code this crate may not use (clean-room). So CR3 sensor data
//! is reported as unsupported and callers use the embedded JPEGs, which this
//! module finds for [`crate::embedded_preview`].

use crate::error::{RawError, Result};
use crate::tiff::{Tiff, tag};

const CANON_UUID: [u8; 16] = [0x85, 0xc0, 0xb6, 0x87, 0x82, 0x0f, 0x11, 0xe0, 0x81, 0x11, 0xf4, 0xce, 0x46, 0x2b, 0x6a, 0x48];
const PREVIEW_UUID: [u8; 16] = [0xea, 0xf4, 0x2b, 0x5e, 0x1c, 0x98, 0x4b, 0x88, 0xb9, 0xfb, 0xb7, 0xdc, 0x40, 0x6e, 0x4d, 0x16];

/// Most boxes read from one container box.
const MAX_BOXES: usize = 1024;
/// Most tracks considered.
const MAX_TRACKS: usize = 16;

/// One box: its type, extended type (`uuid` boxes) and payload position.
#[derive(Debug, Clone, Copy)]
struct Bx {
    typ: [u8; 4],
    uuid: Option<[u8; 16]>,
    /// Absolute offset of the payload (after the header and any uuid).
    at: usize,
    len: usize,
}

fn be16(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_be_bytes(b.get(at..at.checked_add(2)?)?.try_into().ok()?))
}
fn be32(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(b.get(at..at.checked_add(4)?)?.try_into().ok()?))
}
fn be64(b: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_be_bytes(b.get(at..at.checked_add(8)?)?.try_into().ok()?))
}

/// The boxes in `file[start..end]`; stops at the first malformed header.
fn boxes(file: &[u8], start: usize, end: usize) -> Vec<Bx> {
    let mut out = Vec::new();
    let end = end.min(file.len());
    let mut pos = start;
    while pos.saturating_add(8) <= end && out.len() < MAX_BOXES {
        let (Some(size32), Some(typ)) = (be32(file, pos), file.get(pos + 4..pos + 8)) else { break };
        let mut head = 8usize;
        let size = match size32 {
            0 => (end - pos) as u64,
            1 => {
                head = 16;
                match be64(file, pos + 8) {
                    Some(s) => s,
                    None => break,
                }
            }
            s => u64::from(s),
        };
        let Ok(size) = usize::try_from(size) else { break };
        let Some(box_end) = pos.checked_add(size).filter(|e| *e <= end) else { break };
        let mut t = [0u8; 4];
        t.copy_from_slice(typ);
        let uuid = if &t == b"uuid" {
            let Some(u) = file.get(pos + head..pos + head + 16) else { break };
            head += 16;
            let mut a = [0u8; 16];
            a.copy_from_slice(u);
            Some(a)
        } else {
            None
        };
        if size < head {
            break;
        }
        out.push(Bx { typ: t, uuid, at: pos + head, len: size - head });
        pos = box_end;
    }
    out
}

fn children(file: &[u8], b: &Bx) -> Vec<Bx> {
    boxes(file, b.at, b.at.saturating_add(b.len))
}

fn child<'b>(list: &'b [Bx], typ: &[u8; 4]) -> Option<&'b Bx> {
    list.iter().find(|b| &b.typ == typ)
}

fn payload<'a>(file: &'a [u8], b: &Bx) -> Option<&'a [u8]> {
    file.get(b.at..b.at.checked_add(b.len)?)
}

/// What a `CRAW` track holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TrackKind {
    /// The camera's JPEG.
    Jpeg,
    /// CRX-coded raw data, with its `CMP1` header.
    Crx(Cmp1),
}

/// The `CMP1` header of a CRX-coded image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Cmp1 {
    pub version: u16,
    pub width: u32,
    pub height: u32,
    pub tile_width: u32,
    pub tile_height: u32,
    pub bits: u8,
    pub planes: u8,
    /// 0 = RGGB, 1 = GRBG, 2 = GBRG, 3 = BGGR.
    pub cfa_layout: u8,
    /// 0 for lossless RAW, 3 for C-RAW (observed: EOS R6 Mark III RAW vs M50 / RP / R6 C-RAW).
    pub wavelet_levels: u8,
}

impl Cmp1 {
    fn parse(b: &[u8]) -> Option<Cmp1> {
        // Payload offsets (after the 8-byte box header).
        let byte = |i: usize| b.get(i).copied();
        Some(Cmp1 {
            version: be16(b, 4)?,
            width: be32(b, 8)?,
            height: be32(b, 12)?,
            tile_width: be32(b, 16)?,
            tile_height: be32(b, 20)?,
            bits: byte(24)?,
            planes: byte(25)? >> 4,
            cfa_layout: byte(25)? & 0x0F,
            wavelet_levels: byte(26)? & 0x0F,
        })
    }
}

/// One image track: what it is and where its single sample sits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Track {
    pub kind: TrackKind,
    pub offset: usize,
    pub size: usize,
}

/// The parts of a CR3 file this crate uses.
#[derive(Debug, Clone, Default)]
pub(crate) struct Cr3 {
    /// (offset, length) of the `CMT1` TIFF (IFD0: make, model, orientation).
    pub cmt1: Option<(usize, usize)>,
    /// (offset, length) of the JPEGs in `THMB` and `PRVW`.
    pub thumbnails: Vec<(usize, usize)>,
    pub tracks: Vec<Track>,
}

/// The JPEG inside a `THMB` / `PRVW` payload: its length at byte 8 (`THMB`)
/// or 12 (`PRVW`), the stream itself at byte 16.
fn embedded_jpeg(file: &[u8], b: &Bx, len_at: usize) -> Option<(usize, usize)> {
    let p = payload(file, b)?;
    let len = be32(p, len_at)? as usize;
    (p.get(16..18)? == [0xFF, 0xD8] && len <= p.len().saturating_sub(16)).then_some((b.at + 16, len))
}

fn track(file: &[u8], trak: &Bx) -> Option<Track> {
    let mdia = children(file, trak);
    let minf = children(file, child(&mdia, b"mdia")?);
    let stbl_list = children(file, child(&minf, b"minf")?);
    let stbl = children(file, child(&stbl_list, b"stbl")?);
    // stsd: version/flags, entry count, then sample entries.
    let stsd = child(&stbl, b"stsd")?;
    let entries = boxes(file, stsd.at.checked_add(8)?, stsd.at.saturating_add(stsd.len));
    let craw = entries.iter().find(|e| &e.typ == b"CRAW")?;
    // A VisualSampleEntry is 78 bytes after its header; Canon adds 4 more before the children.
    let kids = boxes(file, craw.at.checked_add(82)?, craw.at.saturating_add(craw.len));
    let kind = if let Some(c) = child(&kids, b"CMP1") {
        TrackKind::Crx(Cmp1::parse(payload(file, c)?)?)
    } else if child(&kids, b"JPEG").is_some() {
        TrackKind::Jpeg
    } else {
        return None;
    };
    // stsz: version/flags, sample size (0 = per-sample table), count, table.
    let stsz = payload(file, child(&stbl, b"stsz")?)?;
    let size = match be32(stsz, 4)? {
        0 => be32(stsz, 12)?,
        s => s,
    } as usize;
    let offset = if let Some(c) = child(&stbl, b"co64") {
        usize::try_from(be64(payload(file, c)?, 8)?).ok()?
    } else {
        be32(payload(file, child(&stbl, b"stco")?)?, 8)? as usize
    };
    Some(Track { kind, offset, size })
}

/// Parses the container; `None` when it is not a CR3.
pub(crate) fn parse(file: &[u8]) -> Option<Cr3> {
    if file.get(4..12)? != b"ftypcrx " {
        return None;
    }
    let top = boxes(file, 0, file.len());
    let mut out = Cr3::default();
    if let Some(moov) = child(&top, b"moov") {
        for b in children(file, moov) {
            if b.uuid == Some(CANON_UUID) {
                for c in children(file, &b) {
                    match &c.typ {
                        b"CMT1" => out.cmt1 = Some((c.at, c.len)),
                        b"THMB" => out.thumbnails.extend(embedded_jpeg(file, &c, 8)),
                        _ => {}
                    }
                }
            } else if &b.typ == b"trak" && out.tracks.len() < MAX_TRACKS {
                out.tracks.extend(track(file, &b));
            }
        }
    }
    for b in top.iter().filter(|b| b.uuid == Some(PREVIEW_UUID)) {
        // 8 bytes precede the PRVW box.
        for c in boxes(file, b.at.saturating_add(8), b.at.saturating_add(b.len)) {
            if &c.typ == b"PRVW" {
                out.thumbnails.extend(embedded_jpeg(file, &c, 12));
            }
        }
    }
    Some(out)
}

/// (offset, length) of every embedded JPEG: the full-size track, `PRVW` and `THMB`.
pub(crate) fn jpeg_ranges(file: &[u8]) -> Vec<(usize, usize)> {
    let Some(c) = parse(file) else { return Vec::new() };
    let mut v: Vec<(usize, usize)> = c.tracks.iter().filter(|t| t.kind == TrackKind::Jpeg).map(|t| (t.offset, t.size)).collect();
    v.extend(c.thumbnails);
    v
}

/// Make and model from `CMT1` (IFD0).
pub(crate) fn camera(file: &[u8], c: &Cr3) -> (Option<String>, Option<String>) {
    let tiff = c.cmt1.and_then(|(o, l)| file.get(o..o.checked_add(l)?)).and_then(Tiff::new);
    let ifd = tiff.as_ref().and_then(|t| t.ifd_at(t.first_ifd, 0));
    match (tiff, ifd) {
        (Some(t), Some(i)) => (t.tag_ascii(&i, tag::MAKE), t.tag_ascii(&i, tag::MODEL)),
        _ => (None, None),
    }
}

/// Validates the container and reports why the sensor data is not decoded.
pub(crate) fn decode(file: &[u8]) -> Result<crate::Sensor> {
    let c = parse(file).ok_or(RawError::NotRaw)?;
    let raw = c
        .tracks
        .iter()
        .filter_map(|t| match &t.kind {
            TrackKind::Crx(h) => Some(*h),
            TrackKind::Jpeg => None,
        })
        .max_by_key(|h| u64::from(h.width) * u64::from(h.height))
        .ok_or_else(|| RawError::malformed("CR3 has no raw image track"))?;
    let (_, model) = camera(file, &c);
    // Phrased to read as "<reason> is not supported" (photocraft-io's preview fallback).
    Err(RawError::unsupported(format!(
        "{} CRX-coded sensor data ({}, {}×{}, {}-bit; the CRX code has no public specification)",
        model.unwrap_or_else(|| "Canon CR3".to_string()),
        if raw.wavelet_levels > 0 { "C-RAW, wavelet-coded" } else { "lossless CRX" },
        raw.width,
        raw.height,
        raw.bits,
    )))
}

/// Lists the box structure (debugging aid).
pub(crate) fn dump(file: &[u8]) -> String {
    fn walk(file: &[u8], start: usize, end: usize, depth: usize, out: &mut String) {
        if depth > 8 {
            return;
        }
        for b in boxes(file, start, end) {
            let name = String::from_utf8_lossy(&b.typ).into_owned();
            out.push_str(&format!("{:indent$}{name} @{} len {}\n", "", b.at, b.len, indent = depth * 2));
            let container = matches!(&b.typ, b"moov" | b"trak" | b"mdia" | b"minf" | b"stbl") || b.uuid == Some(CANON_UUID);
            if container {
                walk(file, b.at, b.at.saturating_add(b.len), depth + 1, out);
            } else if b.uuid == Some(PREVIEW_UUID) {
                walk(file, b.at.saturating_add(8), b.at.saturating_add(b.len), depth + 1, out);
            }
        }
    }
    let mut out = String::new();
    walk(file, 0, file.len(), 0, &mut out);
    if let Some(c) = parse(file) {
        for t in &c.tracks {
            out.push_str(&format!("track {:?} at {} size {}\n", t.kind, t.offset, t.size));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn box_sizes_are_bounded() {
        // A box claiming more bytes than exist, a zero-size box, a 64-bit size.
        let mut f = Vec::new();
        f.extend_from_slice(&100u32.to_be_bytes());
        f.extend_from_slice(b"free");
        assert!(boxes(&f, 0, f.len()).is_empty());
        let mut z = 0u32.to_be_bytes().to_vec();
        z.extend_from_slice(b"mdat1234");
        assert_eq!(boxes(&z, 0, z.len()).len(), 1);
        let mut l = 1u32.to_be_bytes().to_vec();
        l.extend_from_slice(b"free");
        l.extend_from_slice(&17u64.to_be_bytes());
        l.push(0);
        let b = boxes(&l, 0, l.len());
        assert_eq!((b.len(), b[0].len), (1, 1));
        // A size smaller than its header ends the walk.
        let mut s = 4u32.to_be_bytes().to_vec();
        s.extend_from_slice(b"free");
        assert!(boxes(&s, 0, s.len()).is_empty());
    }

    #[test]
    fn cmp1_fields() {
        let mut p = vec![0u8; 0x30];
        p[4..6].copy_from_slice(&0x0100u16.to_be_bytes());
        p[8..12].copy_from_slice(&6288u32.to_be_bytes());
        p[12..16].copy_from_slice(&4056u32.to_be_bytes());
        p[16..20].copy_from_slice(&3144u32.to_be_bytes());
        p[20..24].copy_from_slice(&4056u32.to_be_bytes());
        p[24] = 14;
        p[25] = 0x41;
        p[26] = 0x03;
        let c = Cmp1::parse(&p).unwrap();
        assert_eq!((c.width, c.height, c.tile_width, c.bits, c.planes, c.cfa_layout, c.wavelet_levels), (6288, 4056, 3144, 14, 4, 1, 3));
        assert!(Cmp1::parse(&p[..20]).is_none());
    }
}
