//! Real camera files (feature `corpus`): every file under `corpus/raw/` (gitignored, copied in
//! by hand, e.g. CC0 samples from https://raw.pixls.us) must be recognised, have an embedded
//! preview, and either decode and develop sanely or fail with `Unsupported` for a format this
//! crate documents as unsupported (CR3 sensor data, compressed RAF / NEF / ORF / PEF…). A
//! missing or empty `corpus/raw/` fails. Known samples (by file name, see below) are also
//! checked against the values observed when the decoders were written.
#![cfg(feature = "corpus")]

use photocraft_raw::*;
use std::path::PathBuf;

fn corpus_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../corpus/raw")
}

/// Developed size and CFA description.
type Expect = Option<(u32, u32, &'static str)>;

/// File name → (format, developed size and CFA, or `None` when unsupported) observed on CC0
/// samples from raw.pixls.us.
const KNOWN: &[(&str, RawFormat, Expect)] = &[
    ("fuji_xa1_bayer_12bit.raf", RawFormat::Raf, Some((4896, 3264, "RGGB"))),
    ("fuji_xe1_xtrans_12bit.raf", RawFormat::Raf, Some((4896, 3264, "6x6 X-Trans"))),
    ("fuji_xe2_xtrans_14bit.raf", RawFormat::Raf, Some((4896, 3264, "6x6 X-Trans"))),
    ("fuji_xe4_compressed.raf", RawFormat::Raf, None),
    ("canon_eos_r6_crop.cr3", RawFormat::Cr3, None),
    ("canon_eos_rp.cr3", RawFormat::Cr3, None),
    ("canon_eos_m50_craw.cr3", RawFormat::Cr3, None),
    ("canon_eos_r6m3_raw_crop.cr3", RawFormat::Cr3, None),
];

/// CR3 samples and the variant their CMP1 header must report (wavelet levels 0 = lossless, 3 = C-RAW).
const CR3_KIND: &[(&str, &str)] = &[
    ("canon_eos_r6_crop.cr3", "C-RAW"),
    ("canon_eos_rp.cr3", "C-RAW"),
    ("canon_eos_m50_craw.cr3", "C-RAW"),
    ("canon_eos_r6m3_raw_crop.cr3", "lossless CRX"),
];

#[test]
fn raw_corpus() {
    let dir = corpus_dir();
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("{}: {e} (copy camera raw samples into corpus/raw/)", dir.display()))
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|x| x.to_str()).is_some_and(|x| EXTENSIONS.contains(&x.to_ascii_lowercase().as_str())))
        .collect();
    files.sort();
    assert!(!files.is_empty(), "no raw files in {}", dir.display());
    let mut decoded = 0;
    for path in &files {
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default().to_string();
        let bytes = std::fs::read(path).unwrap();
        let format = identify(&bytes).unwrap_or_else(|| panic!("{name}: not recognised"));
        let preview = embedded_preview(&bytes).unwrap_or_else(|| panic!("{name}: no embedded preview"));
        assert!(preview.width >= 160 && preview.height >= 120, "{name}: preview {}x{}", preview.width, preview.height);
        let known = KNOWN.iter().find(|k| k.0 == name);
        if let Some((_, f, _)) = known {
            assert_eq!(format, *f, "{name}");
        }
        let opts = DevelopOptions { demosaic: Demosaic::Bilinear, ..Default::default() };
        match develop(&bytes, &opts) {
            Ok(d) => {
                decoded += 1;
                let n = d.rgb.len() as f64;
                let mean = d.rgb.iter().map(|&v| f64::from(v)).sum::<f64>() / n;
                println!("{name}: {format:?} {}x{} cfa {:?} wb {:?} mean {mean:.0}", d.width, d.height, d.info.cfa, d.info.wb_multipliers);
                assert!(mean > 1000.0 && mean < 60000.0, "{name}: implausible mean level {mean}");
                if let Some((_, _, Some((w, h, cfa)))) = known {
                    let (ow, oh) = if d.info.orientation >= 5 { (d.height, d.width) } else { (d.width, d.height) };
                    assert_eq!((ow, oh), (*w, *h), "{name}");
                    assert_eq!(d.info.cfa.as_deref(), Some(*cfa), "{name}");
                }
            }
            Err(RawError::Unsupported(m)) => {
                if let Some((_, kind)) = CR3_KIND.iter().find(|k| k.0 == name) {
                    assert!(m.contains(kind), "{name}: {m}");
                }
                println!("{name}: {format:?} unsupported ({m}); preview {}x{}", preview.width, preview.height);
                if let Some((_, _, expect)) = known {
                    assert!(expect.is_none(), "{name}: expected to decode, got: {m}");
                }
            }
            Err(e) => panic!("{name}: {e}"),
        }
    }
    println!("{decoded} of {} files decoded", files.len());
}
