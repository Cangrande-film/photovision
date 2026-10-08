# photocraft-raw

A clean-room, pure-Rust camera raw decoder and developer. The crate is standalone (no workspace
dependencies), has no `unsafe`, does no I/O (`&[u8]` in), builds for `wasm32-unknown-unknown`
(sequential there, rayon-parallel on native) and never panics on hostile input: every offset is
bounds-checked and sizes are checked against `Limits` before allocating.

```rust
use photocraft_raw::{develop, DevelopOptions, Demosaic};

let dev = develop(&bytes, &DevelopOptions { demosaic: Demosaic::Ahd, ..Default::default() })?;
// dev.rgb: interleaved 16-bit RGB in ProPhoto RGB (ROMM primaries, D50, gamma 1.8)
// dev.warnings: anything approximated or not applied
```

`photocraft-io` uses it so opening a raw file yields a normal 16-bit RGB document tagged with the
built-in ProPhoto-compatible profile.

## Sources (clean-room)

Implemented only from public specifications, papers and observation of files:

* TIFF 6.0, TIFF/EP (ISO 12234-2) and the Adobe DNG Specification 1.7.
* ITU-T T.81 (ISO 10918-1) Annex H: lossless JPEG, process 14 ("LJ92").
* The published description of Canon's CR2 container (header, raw IFD, slice tag 0xC640).
* Publicly documented maker-note / private tags (ExifTool's tag tables): Canon ModelID (0x0010),
  SensorInfo (0x00E0) and ColorData (0x4001), Nikon WB_RBLevels (0x000C) and BlackLevel (0x003D), Sony
  BlackLevel (0x7310), WB_RGGBLevels (0x7313), SonyRawFileType (0x7000) and SonyToneCurve
  (0x7010), the PanasonicRaw IFD0 tags, Olympus ImageProcessing (0x2040) and CameraSettings
  (0x2020) preview tags.
* Sony cRAW (ARW 2): H. Dietz, "Sony ARW2 Compression: Artifacts And Credible Repair"
  (Electronic Imaging 2016) and the RawDigger / diglloyd write-ups of the 11 + 7-bit scheme;
  the exact bit layout and tone-curve scale were established by observation of sample files.
* Panasonic RW2 RawFormat 5 and uncompressed Olympus ORF: established by observation of sample
  files (bit packing, page layout, sample justification).
* Fujifilm RAF: Phil Harvey's ExifTool FujiFilm tag documentation (RAF header, RAF and
  FujiIFD tags) and the libopenraw RAF format write-up (prose: header directory, big-endian
  metadata records); the 12-bit packing, the X-Trans layout order and the Bayer phase were
  established by observation of CC0 samples from raw.pixls.us (X-A1, X-E1, X-E2, X-E4).
* Canon CR3: ISO/IEC 14496-12 (ISO base media file format) and Laurent Clévy, "Describing the
  Canon Raw v3 (CR3) file format" (prose: box layout, CMT1–4, THMB, PRVW, CRAW / CMP1 headers);
  the CMP1 wavelet-level field (0 = lossless RAW, 3 = C-RAW) was checked on samples. Canon's
  patent US 2016/0323602 A1 was read for the CRX code, see the support matrix.
* Demosaicing: Malvar, He & Cutler (ICASSP 2004); Hirakawa & Parks, "Adaptive
  homogeneity-directed demosaicing" (IEEE TIP 2005); for non-Bayer CFAs, colour-difference
  (constant-hue) interpolation after D. R. Cok, US patent 4,642,678 (1987).
* McCamy's CCT approximation (1992); the Bradford chromatic adaptation transform.

No code from dcraw, LibRaw, rawspeed, rawler, rawloader, darktable or RawTherapee was read or
used, and no camera colour tables were copied. Write-ups that are commented extracts of
decoder source (for example the NEF page that annotates dcraw's decompressor) were not used.

## Support matrix

| Format | Status |
|---|---|
| DNG | Uncompressed (8–16 bit, packed or not) and lossless JPEG; strips and tiles; CFA (Bayer) and LinearRaw; LinearizationTable, BlackLevel (+ repeat, DeltaH/V), WhiteLevel, ActiveArea, DefaultCrop, ColorMatrix1/2, CameraCalibration, ForwardMatrix, AnalogBalance, AsShotNeutral / AsShotWhiteXY, BaselineExposure, Orientation, OpcodeList2 GainMap (lens shading) |
| DNG (lossy JPEG, JPEG XL, floating point; opcodes other than GainMap) | Unsupported / not applied (reported) |
| CR2 | Lossless JPEG with slices, borders and as-shot white balance from the maker note, black measured on the masked border. CR2 has no CFA tag and the row phase varies by model, so it is measured from the data (the green diagonal), with a Canon model-ID table as the fallback (see `src/cr2.rs`) |
| CR2 sRAW / mRAW | Unsupported |
| NEF / NRW, ARW, PEF and other TIFF/EP raws | Uncompressed and lossless-JPEG (incl. Sony lossless ARW) CFA data |
| Sony compressed ARW ("cRAW", SonyRawFileType 2) | Decoded: 11-bit min/max + 7-bit delta blocks, SonyToneCurve to 14 bits |
| Panasonic / Leica RW2, RawFormat 5 (12- and 14-bit packed) | Decoded, with PanasonicRaw black / white / WB / sensor borders |
| Olympus ORF, uncompressed 16-bit (E-1, E-400…) | Decoded, with ImageProcessing black / WB / ValidBits / crop |
| Fujifilm RAF, uncompressed (X-series raw container) | Decoded: 16-bit samples and 12-bit packed (two samples in three bytes, LSB first); X-Trans (6×6, from the XTransLayout record) and Bayer (phase measured); FujiIFD black level and as-shot WB, RAF crop, make / model / orientation from the preview's EXIF. Tested on X-A1 (Bayer, 12-bit), X-E1 (X-Trans, 12-bit) and X-E2 (X-Trans, 14-bit) |
| Fujifilm compressed RAF (lossless and lossy), RAFs without the raw container (older SuperCCD models) | Unsupported (no public description of the compressed code); the embedded JPEG is used |
| Canon CR3 | Container parsed (ISO BMFF): `CMT1` make / model, `CMP1` raw header, and the embedded JPEGs (full-size track, `PRVW`, `THMB`) for previews and thumbnails. The CRX-coded sensor data (lossless RAW and C-RAW) is **not decoded**: the public prose (Clévy's write-up: a JPEG-LS-like adaptive Golomb-Rice code with run mode and median prediction; for C-RAW a Le Gall 5/3 wavelet and quantization) and Canon's patent describe the general scheme but not the bit-exact entropy code (parameter adaptation, run-length tables, escapes, tile / plane / subband headers in full); its only complete descriptions are decoder source (LibRaw, rawspeed, rawler), which this crate may not use. Reported as unsupported with the variant named; `photocraft-io` opens the camera's full-size JPEG |
| Nikon compressed NEF (lossless and lossy), Sony "Compressed RAW 2", Pentax compressed PEF, RW2 RawFormat 4 and older, Olympus compressed ORF | Unsupported: no public description of these codes was found apart from GPL decoder source (or write-ups annotating it), which this crate may not use (clean-room). `photocraft-io` opens the embedded JPEG preview instead |
| X-Trans and other non-Bayer three-colour CFAs (RAF, DNG) | Demosaiced by colour-difference interpolation (`Demosaic` choice applies to Bayer only) |
| CFAs without red, green and blue sites | Unsupported |

## Development pipeline

1. Linearization table, per-position black level, scale to the white level.
2. White balance (as shot; or grey-world when the file has none; or explicit multipliers),
   normalized so the smallest multiplier is 1, then clip to 1 so blown highlights stay white.
3. Demosaic: `Bilinear`, `Mhc` (Malvar–He–Cutler) or `Ahd` (default) for Bayer data;
   colour-difference interpolation (green first, then R−G / B−G from the nearest sites) for
   X-Trans and other periodic CFAs.
4. Camera → XYZ (D50) per the DNG specification (ColorMatrix interpolated by the white's
   correlated colour temperature, or ForwardMatrix), → linear ProPhoto; exposure
   (BaselineExposure + user EV); gamma 1.8; 16 bits.
5. Orientation.

Files without colour calibration (CR2, NEF, ARW, RW2, ORF, RAF…) use a documented neutral fallback: the
white-balanced camera channels are treated as linear sRGB primaries (colours are plausible but
less saturated than a calibrated profile; converting to DNG gives calibrated colour). No tone
curve is applied: the result is a scene-referred rendering, flatter than a camera JPEG.

## Tests

`cargo test -p photocraft-raw` runs the synthetic suites (generators in `src/testgen.rs`:
DNG, CR2, TIFF/EP, Sony cRAW, RW2, ORF, RAF, CR3) and the hostile-input tests (every
truncation and random corruption of each synthetic file must fail cleanly).
`cargo test --release -p photocraft-raw --features corpus --test corpus -- --nocapture`
runs over real files copied by hand into `corpus/raw/` (gitignored; e.g. CC0 samples from
raw.pixls.us) and fails when that folder is missing.

## Tools

`cargo run --release -p photocraft-raw --example rawinfo -- [--dump] [--demosaic ahd] [--png DIR] FILE...`
prints what was decoded, times decode and develop, and can write sRGB PNG previews and the
embedded JPEG previews.
