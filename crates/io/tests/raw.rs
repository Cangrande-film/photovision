//! Camera raw import: synthetic DNG / CR2 develop into a 16-bit ProPhoto
//! document; unsupported raw variants fall back to the embedded preview.

use photocraft_codecs::{ChannelLayout, EncodeOptions, Format, Image};
use photocraft_color::{ColorMode, SampleType};
use photocraft_io::{IoError, import};
use photocraft_raw::testgen::{Cr2Spec, DngSpec, RafSpec, TiffBuilder, Val, XTRANS, cr3_cmp1, cr3_with_jpeg, mosaic, mosaic_pattern, scene};

#[test]
fn dng_opens_as_16_bit_prophoto() {
    let (w, h) = (40, 24);
    let mut spec = DngSpec::cfa(w, h, mosaic(&scene(w, h), w, [0, 1, 1, 2], 0, 65535));
    spec.as_shot_neutral = Some([0.6, 1.0, 0.8]);
    let r = import("shot.dng", &spec.build()).unwrap();
    let d = &r.document;
    assert_eq!((d.size.width, d.size.height), (40, 24));
    assert_eq!(d.mode, ColorMode::Rgb);
    assert_eq!(d.depth, SampleType::U16);
    assert_eq!(d.layers.len(), 1);
    let icc = d.icc_profile.as_ref().expect("profile");
    assert_eq!(icc.as_slice(), &photocraft_cms::Builtin::ProPhotoCompat.profile().to_bytes()[..]);
    assert!(r.warnings.iter().any(|w| w.contains("DNG") && w.contains("ProPhoto")), "{:?}", r.warnings);
}

#[test]
fn cr2_opens() {
    let (w, h) = (48, 16);
    let data = mosaic(&scene(w, h), w, [0, 1, 1, 2], 256, 12000);
    let spec =
        Cr2Spec { width: w, height: h, data, precision: 14, components: 2, slices: vec![24, 24], borders: None, wb_rggb: None, orientation: 6, model_id: None };
    let r = import("IMG_0001.CR2", &spec.build()).unwrap();
    // Orientation 6 rotates the 48×16 sensor image to 16×48.
    assert_eq!((r.document.size.width, r.document.size.height), (16, 48));
    assert_eq!(r.document.depth, SampleType::U16);
}

/// A NEF-like file with Nikon's (undocumented) compression and a full-size
/// baseline JPEG preview in IFD0.
fn nef_with_preview() -> Vec<u8> {
    let img = Image::from_u8(32, 20, ChannelLayout::Rgb, vec![180; 32 * 20 * 3]).unwrap();
    let jpeg = photocraft_codecs::encode(&img, Format::Jpeg, &EncodeOptions::default()).unwrap();
    let mut t = TiffBuilder::default();
    let strip = t.blob(vec![0; 64]);
    let preview = t.blob(jpeg.clone());
    let raw = t.ifd(vec![
        (256, Val::Long(vec![8])),
        (257, Val::Long(vec![8])),
        (258, Val::Short(vec![12])),
        (259, Val::Short(vec![34713])),
        (262, Val::Short(vec![32803])),
        (273, Val::Blobs(vec![strip])),
        (279, Val::Long(vec![64])),
        (33421, Val::Short(vec![2, 2])),
        (33422, Val::Byte(vec![0, 1, 1, 2])),
    ]);
    let ifd0 = t.ifd(vec![
        (271, Val::Ascii("NIKON CORPORATION".into())),
        (330, Val::Ifds(vec![raw])),
        (513, Val::Blobs(vec![preview])),
        (514, Val::Long(vec![jpeg.len() as u32])),
    ]);
    t.chain = vec![ifd0];
    t.build()
}

#[test]
fn unsupported_raw_falls_back_to_the_embedded_preview() {
    let r = import("DSC_0001.NEF", &nef_with_preview()).unwrap();
    assert_eq!((r.document.size.width, r.document.size.height), (32, 20));
    assert!(r.warnings.first().is_some_and(|w| w.contains("Nikon compressed NEF") && w.contains("embedded")), "{:?}", r.warnings);
}

#[test]
fn unsupported_raw_without_preview_is_a_clear_error() {
    let mut cr3 = vec![0, 0, 0, 24];
    cr3.extend_from_slice(b"ftypcrx ");
    cr3.extend_from_slice(&[0; 12]);
    match import("IMG_0001.CR3", &cr3) {
        Err(e @ IoError::Raw(_)) => assert!(e.to_string().contains("CR3"), "{e}"),
        Err(e) => panic!("expected a raw error, got {e}"),
        Ok(_) => panic!("CR3 must not decode yet"),
    }
}

#[test]
fn xtrans_raf_opens() {
    let (w, h) = (48, 36);
    let spec = RafSpec {
        width: w,
        height: h,
        data: mosaic_pattern(&scene(w, h), w, &XTRANS, 6, 256, 16000),
        bits: 14,
        xtrans_layout: Some(RafSpec::layout_for(&XTRANS)),
        black: vec![256; 36],
        wb_grb: [302, 604, 453],
        crop: (0, 0, h as u16, w as u16),
        orientation: 6,
        truncate_data_to: None,
    };
    let r = import("DSCF0001.RAF", &spec.build()).unwrap();
    // Orientation 6 (from the preview's EXIF) rotates 48×36 to 36×48.
    assert_eq!((r.document.size.width, r.document.size.height), (36, 48));
    assert_eq!(r.document.depth, SampleType::U16);
    assert!(r.warnings.iter().any(|w| w.contains("RAF") && w.contains("X-Synthetic")), "{:?}", r.warnings);
}

#[test]
fn cr3_opens_its_embedded_jpeg() {
    // The full-size JPEG track is larger than the PRVW (1620×1080) and THMB stand-ins, as in camera files.
    let (w, h) = (1640u32, 1100u32);
    let img = Image::from_u8(w, h, ChannelLayout::Rgb, vec![90; (w * h * 3) as usize]).unwrap();
    let jpeg = photocraft_codecs::encode(&img, Format::Jpeg, &EncodeOptions::default()).unwrap();
    let b = cr3_with_jpeg("Canon EOS Synthetic", Some(jpeg), cr3_cmp1(64, 48, 14, 0));
    let r = import("IMG_0001.CR3", &b).unwrap();
    assert_eq!((r.document.size.width, r.document.size.height), (w, h));
    assert!(r.warnings.first().is_some_and(|w| w.contains("CR3") && w.contains("CRX") && w.contains("embedded")), "{:?}", r.warnings);
}

#[test]
fn truncated_raws_do_not_panic() {
    let b = nef_with_preview();
    for n in (0..b.len()).step_by(7) {
        let _ = import("x.nef", &b[..n]);
    }
}
