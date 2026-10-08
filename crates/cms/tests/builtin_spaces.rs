//! The video / cinema / linear built-in spaces (Rec.709 gamma 2.4 and camera OETF, P3-D65,
//! DCI-P3, Rec.2020 gamma 2.4, linear Rec.2020 and P3-D65, ACEScg): ids, primaries and white
//! points against published RGB→XYZ matrices, transfer curves, and moxcms as an oracle.

use photocraft_cms::math::{self, Mat3};
use photocraft_cms::{Builtin, Curve, Intent, Transform, TransformOptions};

const NEW: [Builtin; 8] = [
    Builtin::Rec709Bt1886,
    Builtin::Rec709Oetf,
    Builtin::P3D65,
    Builtin::DciP3,
    Builtin::Rec2020G24,
    Builtin::LinearRec2020,
    Builtin::LinearP3D65,
    Builtin::AcesCg,
];

#[test]
fn ids_round_trip_and_are_unique() {
    for b in NEW {
        assert_eq!(Builtin::from_id(b.id()), Some(b), "{b:?}");
        assert_eq!(Builtin::from_id(b.description()), Some(b), "{b:?} by description");
        assert!(!b.label().is_empty());
    }
    let mut ids: Vec<&str> = Builtin::ALL.iter().map(|b| b.id()).collect();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), Builtin::ALL.len());
    assert_eq!(Builtin::from_id("ACEScg"), Some(Builtin::AcesCg));
    assert_eq!(Builtin::from_id("DCI-P3"), Some(Builtin::DciP3));
    // "p3" stays Display P3.
    assert_eq!(Builtin::from_id("p3"), Some(Builtin::DisplayP3));
}

#[test]
fn linear_flags_match_the_curves() {
    for b in Builtin::ALL {
        let p = b.profile();
        let linear = p.trc.as_ref().is_some_and(|t| t.iter().all(Curve::is_identity));
        assert_eq!(b.is_linear(), linear, "{b:?}");
    }
}

/// The profile's RGB→XYZ matrix relative to its own white (the D50 colorants with the Bradford
/// adaptation undone).
fn native_matrix(b: Builtin, white_xy: [f64; 2]) -> Mat3 {
    let p = b.profile();
    let m50 = p.matrix.unwrap();
    let white = math::xy_to_xyz(white_xy);
    let back = math::bradford(math::D50, white);
    math::mul(&back, &m50)
}

fn assert_matrix(b: Builtin, white_xy: [f64; 2], expect: Mat3) {
    let m = native_matrix(b, white_xy);
    for (i, (row, erow)) in m.iter().zip(&expect).enumerate() {
        for (j, (v, e)) in row.iter().zip(erow).enumerate() {
            assert!((v - e).abs() < 1.5e-3, "{b:?} [{i}][{j}] = {v:.5}, published {e:.5}\n{m:?}");
        }
    }
}

#[test]
fn primaries_match_published_matrices() {
    let d65 = math::D65_XY;
    // Rec. 709 (ITU-R BT.709-6 / sRGB).
    let rec709 = [[0.4124, 0.3576, 0.1805], [0.2126, 0.7152, 0.0722], [0.0193, 0.1192, 0.9505]];
    assert_matrix(Builtin::Rec709Bt1886, d65, rec709);
    assert_matrix(Builtin::Rec709Oetf, d65, rec709);
    // Rec. 2020 (ITU-R BT.2020-2; luma row 0.2627 / 0.6780 / 0.0593).
    let rec2020 = [[0.6370, 0.1446, 0.1689], [0.2627, 0.6780, 0.0593], [0.0, 0.0281, 1.0610]];
    assert_matrix(Builtin::Rec2020G24, d65, rec2020);
    assert_matrix(Builtin::LinearRec2020, d65, rec2020);
    // P3-D65 (SMPTE EG 432-1).
    let p3d65 = [[0.4866, 0.2657, 0.1982], [0.2290, 0.6917, 0.0793], [0.0, 0.0451, 1.0439]];
    assert_matrix(Builtin::P3D65, d65, p3d65);
    assert_matrix(Builtin::LinearP3D65, d65, p3d65);
    // DCI-P3 with the DCI white (SMPTE RP 431-2).
    let dci = [[0.4452, 0.2771, 0.1723], [0.2095, 0.7216, 0.0689], [0.0, 0.0471, 0.9074]];
    assert_matrix(Builtin::DciP3, [0.314, 0.351], dci);
    // ACEScg AP1 (AMPAS S-2014-004).
    let ap1 = [[0.662_454_2, 0.134_004_2, 0.156_187_7], [0.272_228_7, 0.674_081_8, 0.053_689_5], [-0.005_574_6, 0.004_060_7, 1.010_339_1]];
    assert_matrix(Builtin::AcesCg, [0.32168, 0.33767], ap1);
}

#[test]
fn transfer_curves() {
    let trc = |b: Builtin| b.profile().trc.clone().unwrap()[0].clone();
    for (b, g) in [(Builtin::Rec709Bt1886, 2.4), (Builtin::Rec2020G24, 2.4), (Builtin::P3D65, 2.6), (Builtin::DciP3, 2.6)] {
        for x in [0.05, 0.18, 0.5, 0.9] {
            let y = trc(b).eval64(x);
            assert!((y - f64::powf(x, g)).abs() < 1e-6, "{b:?} at {x}: {y}");
        }
    }
    // BT.709 OETF: V = 1.099 L^0.45 − 0.099 above L = 0.018, 4.5 L below; the curve is its inverse.
    let oetf = |l: f64| if l < 0.018 { 4.5 * l } else { 1.099 * l.powf(0.45) - 0.099 };
    for l in [0.001, 0.01, 0.018, 0.1, 0.18, 0.5, 1.0] {
        let back = trc(Builtin::Rec709Oetf).eval64(oetf(l));
        assert!((back - l).abs() < 1e-4, "L {l}: {back}");
    }
}

#[test]
fn srgb_acescg_srgb_round_trip_in_float() {
    let opts = TransformOptions { intent: Intent::RelativeColorimetric, bpc: false, precise_float: true };
    let to = Transform::with_options(Builtin::Srgb.profile(), Builtin::AcesCg.profile(), opts).unwrap();
    let back = Transform::with_options(Builtin::AcesCg.profile(), Builtin::Srgb.profile(), opts).unwrap();
    let src: Vec<f32> = (0..512u32).flat_map(|i| [(i * 37 % 256) as f32 / 255.0, (i * 91 % 256) as f32 / 255.0, (i * 13 % 256) as f32 / 255.0]).collect();
    let mut mid = vec![0.0f32; src.len()];
    to.convert_f32(&src, 3, &mut mid, 3, false);
    let mut out = vec![0.0f32; src.len()];
    back.convert_f32(&mid, 3, &mut out, 3, false);
    for (a, b) in src.iter().zip(&out) {
        assert!((a - b).abs() < 1e-4, "{a} vs {b}");
    }
    // sRGB white is ACEScg (1, 1, 1) under relative colorimetric; mid gray is linear 0.214.
    let mut w = [0.0f32; 3];
    to.eval(&[1.0, 1.0, 1.0], &mut w);
    assert!(w.iter().all(|v| (v - 1.0).abs() < 2e-3), "{w:?}");
    to.eval(&[0.5, 0.5, 0.5], &mut w);
    assert!(w.iter().all(|v| (v - 0.214).abs() < 2e-3), "{w:?}");
}

/// moxcms reads our profile bytes and agrees on sRGB → each new space (8-bit).
#[test]
fn oracle_moxcms_reads_the_new_profiles() {
    use moxcms::{ColorProfile, Layout, TransformOptions as MoxOpts};
    let srgb = Builtin::Srgb.profile();
    let s = ColorProfile::new_from_slice(&srgb.to_bytes()).unwrap();
    let src: Vec<u8> = (0..4096u32).flat_map(|i| [(i * 37 % 256) as u8, (i * 91 % 256) as u8, (i * 13 % 256) as u8]).collect();
    for b in NEW {
        let d = ColorProfile::new_from_slice(&b.profile().to_bytes()).unwrap();
        let mt = s.create_transform_8bit(Layout::Rgb, &d, Layout::Rgb, MoxOpts::default()).unwrap();
        let mut theirs = vec![0u8; src.len()];
        mt.transform(&src, &mut theirs).unwrap();
        let t = Transform::new(srgb, b.profile(), Intent::RelativeColorimetric, false).unwrap();
        let mut ours = vec![0u8; src.len()];
        t.convert_u8(&src, 3, &mut ours, 3, false);
        let worst = ours.iter().zip(&theirs).map(|(a, b)| (*a as i32 - *b as i32).abs()).max().unwrap();
        // Linear 8-bit output quantizes shadows coarsely, so allow a little more there.
        let limit = if b.is_linear() { 3 } else { 2 };
        assert!(worst <= limit, "{b:?}: max diff {worst}");
    }
}

/// moxcms's own ACEScg profile (built independently from the AP1 primaries) agrees with ours.
#[test]
fn oracle_moxcms_acescg() {
    use moxcms::{ColorProfile, Layout, TransformOptions as MoxOpts};
    let s = ColorProfile::new_srgb();
    let d = ColorProfile::new_aces_cg_linear();
    let mt = s.create_transform_f32(Layout::Rgb, &d, Layout::Rgb, MoxOpts::default()).unwrap();
    let src: Vec<f32> = (0..512u32).flat_map(|i| [(i * 37 % 256) as f32 / 255.0, (i * 91 % 256) as f32 / 255.0, (i * 13 % 256) as f32 / 255.0]).collect();
    let mut theirs = vec![0.0f32; src.len()];
    mt.transform(&src, &mut theirs).unwrap();
    let opts = TransformOptions { intent: Intent::RelativeColorimetric, bpc: false, precise_float: true };
    let t = Transform::with_options(Builtin::Srgb.profile(), Builtin::AcesCg.profile(), opts).unwrap();
    let mut ours = vec![0.0f32; src.len()];
    t.convert_f32(&src, 3, &mut ours, 3, false);
    let worst = ours.iter().zip(&theirs).map(|(a, b)| (a - b).abs()).fold(0.0f32, f32::max);
    assert!(worst < 5e-3, "max diff {worst}");
}
