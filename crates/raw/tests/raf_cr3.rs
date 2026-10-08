//! Synthetic Fujifilm RAF (Bayer and X-Trans, 12-bit packed and 16-bit),
//! X-Trans DNG, and Canon CR3 containers (previews, metadata, CMP1).

use photocraft_raw::testgen::{DngSpec, RafSpec, XTRANS, cr3, cr3_cmp1, mosaic, mosaic_pattern, scene};
use photocraft_raw::*;

fn grey(w: usize, h: usize, v: f32) -> Vec<[f32; 3]> {
    vec![[v, v, v]; w * h]
}

fn raf_spec(w: usize, h: usize, data: Vec<u16>, bits: u32, xtrans: bool) -> RafSpec {
    RafSpec {
        width: w,
        height: h,
        data,
        bits,
        xtrans_layout: xtrans.then(|| RafSpec::layout_for(&XTRANS)),
        black: vec![256; if xtrans { 36 } else { 4 }],
        wb_grb: [302, 604, 453],
        crop: (2, 4, (h - 4) as u16, (w - 8) as u16),
        orientation: 6,
        truncate_data_to: None,
    }
}

#[test]
fn raf_bayer_12_bit_packed_decodes_exactly() {
    let (w, h) = (48, 24);
    let data = mosaic(&scene(w, h), w, [0, 1, 1, 2], 256, 4000);
    let spec = raf_spec(w, h, data.clone(), 12, false);
    let b = spec.build();
    assert_eq!(identify(&b), Some(RawFormat::Raf));
    let s = decode(&b, &Limits::default()).unwrap();
    assert_eq!((s.width, s.height), (w, h));
    assert_eq!(s.data, data);
    assert_eq!(s.cfa.as_ref().unwrap().phase(0, 0), [0, 1, 1, 2], "green diagonal measured, red in the even columns");
    assert_eq!(s.crop, Rect::new(4, 2, w - 8, h - 4));
    assert_eq!(s.black.at(0, 0, 0, 1), 256.0);
    let wb = s.camera_wb.unwrap();
    assert!((wb[0] - 2.0).abs() < 1e-9 && (wb[2] - 1.5).abs() < 1e-9, "{wb:?}");
    assert_eq!(s.orientation, 6);
    assert_eq!(s.make.as_deref(), Some("FUJIFILM"));
    assert_eq!(s.model.as_deref(), Some("X-Synthetic"));
    let d = develop_sensor(&s, &DevelopOptions::default()).unwrap();
    assert_eq!((d.width, d.height), ((h - 4) as u32, (w - 8) as u32), "rotated by the EXIF orientation");
}

#[test]
fn raf_xtrans_layout_is_read_reversed_from_the_data_origin() {
    let (w, h) = (36, 30);
    let data = mosaic_pattern(&scene(w, h), w, &XTRANS, 6, 256, 16000);
    let b = raf_spec(w, h, data.clone(), 14, true).build();
    let s = decode(&b, &Limits::default()).unwrap();
    assert_eq!(s.data, data);
    let cfa = s.cfa.as_ref().unwrap();
    for y in 0..12 {
        for x in 0..12 {
            assert_eq!(cfa.color(x, y), XTRANS[(y % 6) * 6 + x % 6], "({x}, {y})");
        }
    }
    assert!(cfa.is_three_colour() && !cfa.is_bayer());
}

#[test]
fn raf_xtrans_develops_neutral_grey_to_neutral() {
    // A grey scene recorded through the white-balance gains develops to equal channels.
    let (w, h) = (48, 36);
    let gains = [2.0f32, 1.0, 1.5];
    let rgb: Vec<[f32; 3]> = grey(w, h, 0.4).into_iter().map(|p| [p[0] / gains[0], p[1], p[2] / gains[2]]).collect();
    let data = mosaic_pattern(&rgb, w, &XTRANS, 6, 256, 16000);
    let mut spec = raf_spec(w, h, data, 16, true);
    spec.orientation = 1;
    let b = spec.build();
    for m in [Demosaic::Ahd, Demosaic::Bilinear] {
        let d = develop(&b, &DevelopOptions { demosaic: m, ..Default::default() }).unwrap();
        assert_eq!(d.info.cfa.as_deref(), Some("6x6 X-Trans"));
        for p in d.rgb.as_chunks::<3>().0 {
            let (lo, hi) = (p.iter().min().unwrap(), p.iter().max().unwrap());
            assert!(hi - lo <= 64, "{p:?}");
        }
    }
}

#[test]
fn raf_compressed_data_is_unsupported_but_previewed() {
    let (w, h) = (24, 12);
    let mut spec = raf_spec(w, h, vec![300; w * h], 14, true);
    spec.truncate_data_to = Some(w * h); // fewer bytes than any uncompressed layout
    let b = spec.build();
    match decode(&b, &Limits::default()) {
        Err(RawError::Unsupported(m)) => assert!(m.contains("compressed"), "{m}"),
        other => panic!("{other:?}"),
    }
    let p = embedded_preview(&b).unwrap();
    assert_eq!((p.width, p.height), (160, 120));
}

#[test]
fn raf_absurd_dimensions_are_rejected() {
    let mut spec = raf_spec(24, 12, vec![300; 24 * 12], 16, false);
    spec.width = 300_000;
    assert!(matches!(decode(&spec.build(), &Limits::default()), Err(RawError::LimitExceeded(_))));
}

#[test]
fn xtrans_dng_develops() {
    let (w, h) = (36, 24);
    let data = mosaic_pattern(&scene(w, h), w, &XTRANS, 6, 0, 65535);
    let mut d = DngSpec::cfa(w, h, data);
    d.cfa_pattern = Some((6, 6, XTRANS.to_vec()));
    d.as_shot_neutral = Some([1.0, 1.0, 1.0]);
    let s = decode(&d.build(), &Limits::default()).unwrap();
    assert!(!s.cfa.as_ref().unwrap().is_bayer());
    let dev = develop_sensor(&s, &DevelopOptions::default()).unwrap();
    assert_eq!((dev.width, dev.height), (w as u32, h as u32));
    // A CFA without blue sites is still refused.
    let mut d2 = DngSpec::cfa(w, h, vec![0; w * h]);
    d2.cfa_pattern = Some((2, 2, vec![0, 1, 1, 0]));
    assert!(matches!(decode(&d2.build(), &Limits::default()), Err(RawError::Unsupported(_))));
}

#[test]
fn cr3_container_previews_and_metadata() {
    let b = cr3("Canon EOS Synthetic", Some((6000, 4000)), cr3_cmp1(6288, 4056, 14, 0));
    assert_eq!(identify(&b), Some(RawFormat::Cr3));
    assert!(is_raw(&b));
    // The full-size JPEG track wins over PRVW (1620×1080) and THMB (160×120).
    let p = embedded_preview(&b).unwrap();
    assert_eq!((p.width, p.height), (6000, 4000));
    match decode(&b, &Limits::default()) {
        Err(RawError::Unsupported(m)) => {
            assert!(m.contains("Canon EOS Synthetic CRX-coded") && m.contains("6288×4056") && m.contains("14-bit"), "{m}");
            assert!(m.contains("lossless CRX"), "{m}");
        }
        other => panic!("{other:?}"),
    }
    let dump = dump_structure(&b);
    assert!(dump.contains("moov") && dump.contains("CMT1") && dump.contains("Crx("), "{dump}");
    // Without the JPEG track, the PRVW preview is used; C-RAW is reported as wavelet-coded.
    let b = cr3("Canon EOS Synthetic", None, cr3_cmp1(6288, 4056, 14, 3));
    let p = embedded_preview(&b).unwrap();
    assert_eq!((p.width, p.height), (1620, 1080));
    assert!(matches!(decode(&b, &Limits::default()), Err(RawError::Unsupported(m)) if m.contains("C-RAW")));
}
