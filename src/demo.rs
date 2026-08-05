//! `magicfs demo` — build a throwaway directory for trying the tool out.
//!
//! The files are real PNGs, so `feh *` actually opens them and you can *see*
//! the ordering change. Their names, sizes, and timestamps are deliberately
//! scrambled relative to one another: sorting by name, by size, and by time
//! each produce a different order, which is the only way to tell that a sort
//! is doing anything at all.

use anyhow::{Context, Result, bail};
use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

/// Dropped into the directory so we can recognise (and safely replace) a
/// directory we made, instead of ever clobbering one the user cares about.
const MARKER: &str = ".magicfs-demo";

/// A file to generate. `days_old` and `bytes` are chosen so that no two of the
/// name/size/time orderings coincide.
struct Sample {
    name: &'static str,
    bytes: usize,
    days_old: i64,
    color: (u8, u8, u8),
}

const SAMPLES: &[Sample] = &[
    // Alphabetically first, but mid-sized and recent.
    Sample { name: "alpha-canyon.png",   bytes:  12_000, days_old:  2, color: (214, 118,  62) },
    Sample { name: "bravo-forest.png",   bytes:   3_000, days_old: 10, color: ( 52, 116,  74) },
    Sample { name: "delta-river.png",    bytes:  60_000, days_old:  6, color: ( 58, 124, 165) },
    // Natural-sort bait: img2 must precede img10 under `sort natural`, and
    // follow it under plain `sort name`.
    Sample { name: "img2.png",           bytes: 150_000, days_old:  5, color: (176,  92, 148) },
    Sample { name: "img10.png",          bytes:  40_000, days_old:  1, color: (132, 108, 190) },
    // A space in the name, to shake out quoting bugs downstream.
    Sample { name: "mike beach day.png", bytes: 500_000, days_old:  7, color: (226, 188,  96) },
    Sample { name: "yankee-night.png",   bytes: 250_000, days_old:  3, color: ( 44,  52, 102) },
    // Alphabetically last, but the biggest and the oldest.
    Sample { name: "zulu-sunset.png",    bytes: 900_000, days_old:  9, color: (198,  78,  70) },
];

/// Non-images, so `magicfs filter images` has something to actually exclude.
const EXTRAS: &[(&str, &str, i64)] = &[
    ("notes.txt", "", 4),
    ("README.md", "# demo\n\nScratch directory made by `magicfs demo`.\n", 8),
];

/// Create the demo directory and return its path.
pub fn create(out: Option<PathBuf>, count: usize) -> Result<PathBuf> {
    let dir = out.unwrap_or_else(|| std::env::temp_dir().join("magicfs-demo"));

    if dir.exists() {
        // Only ever replace a directory we recognise as our own.
        if !dir.join(MARKER).exists() {
            bail!(
                "{} already exists and was not created by `magicfs demo` — \
                 refusing to overwrite it.\nPick another location with --out.",
                dir.display()
            );
        }
        std::fs::remove_dir_all(&dir)
            .with_context(|| format!("replacing {}", dir.display()))?;
    }
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    std::fs::write(dir.join(MARKER), b"")?;

    // Timestamps are anchored to "now" so `sort time` is meaningful, but the
    // relative spacing is what the demo actually depends on.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    let mut made = 0usize;
    for sample in SAMPLES.iter().take(count) {
        let png = encode_png(sample.bytes, sample.color);
        write_at(&dir.join(sample.name), &png, now - sample.days_old * 86_400)?;
        made += 1;
    }
    for (name, body, days_old) in EXTRAS.iter().take(count.saturating_sub(made)) {
        write_at(&dir.join(name), body.as_bytes(), now - days_old * 86_400)?;
        made += 1;
    }
    // Beyond the curated set, pad with generated images so `-n 40` still works.
    for i in 0..count.saturating_sub(made) {
        let bytes = 5_000 + (i * 7_919) % 300_000;
        let color = ((i * 53 % 200 + 40) as u8, (i * 97 % 200 + 40) as u8, (i * 31 % 200 + 40) as u8);
        let png = encode_png(bytes, color);
        write_at(&dir.join(format!("extra-{i:03}.png")), &png, now - (i as i64 % 30) * 86_400)?;
    }

    Ok(dir)
}

fn write_at(path: &Path, bytes: &[u8], mtime: i64) -> Result<()> {
    std::fs::write(path, bytes).with_context(|| format!("writing {}", path.display()))?;
    let c = CString::new(path.as_os_str().as_bytes())?;
    let tv = libc::timeval { tv_sec: mtime, tv_usec: 0 };
    let times = [tv, tv];
    // Best-effort: a filesystem that refuses utimes still leaves a usable
    // demo, just with uninteresting timestamps.
    unsafe { libc::utimes(c.as_ptr(), times.as_ptr()) };
    Ok(())
}

// --- Minimal PNG encoder -------------------------------------------------
//
// A solid-colour RGB image, deflate-"stored" so no compression library is
// needed. Writing raw blocks also means the encoded size tracks the requested
// size closely, which is the point: the demo needs a real spread of file
// sizes.

/// Encode a solid-colour PNG of roughly `target` bytes.
fn encode_png(target: usize, color: (u8, u8, u8)) -> Vec<u8> {
    // Each row is one filter byte plus 3 bytes per pixel, so a square image of
    // side n holds about 3n² bytes.
    let side = ((target.max(64) as f64 / 3.0).sqrt().round() as u32).max(1);
    let (w, h) = (side, side);

    // Every row is identical, so build one and repeat it.
    let mut row = vec![0u8; 1 + (w * 3) as usize]; // leading 0 = filter: None
    for pixel in row[1..].chunks_mut(3) {
        pixel.copy_from_slice(&[color.0, color.1, color.2]);
    }
    let mut raw = Vec::with_capacity(row.len() * h as usize);
    for _ in 0..h {
        raw.extend_from_slice(&row);
    }

    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&w.to_be_bytes());
    ihdr.extend_from_slice(&h.to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]); // 8-bit, truecolour RGB

    let mut png = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    push_chunk(&mut png, b"IHDR", &ihdr);
    push_chunk(&mut png, b"IDAT", &zlib_stored(&raw));
    push_chunk(&mut png, b"IEND", &[]);
    png
}

fn push_chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let mut crc_input = Vec::with_capacity(4 + data.len());
    crc_input.extend_from_slice(kind);
    crc_input.extend_from_slice(data);
    out.extend_from_slice(&crc32(&crc_input).to_be_bytes());
}

/// Wrap `data` in a zlib stream using only uncompressed deflate blocks.
fn zlib_stored(data: &[u8]) -> Vec<u8> {
    let mut out = vec![0x78, 0x01]; // deflate, 32K window, no preset dictionary
    if data.is_empty() {
        out.extend_from_slice(&[0x01, 0x00, 0x00, 0xff, 0xff]);
    }
    // A stored block carries at most u16::MAX bytes.
    for (i, block) in data.chunks(0xFFFF).enumerate() {
        let last = (i + 1) * 0xFFFF >= data.len();
        out.push(u8::from(last)); // BFINAL, BTYPE=00, then pad to a byte
        let len = block.len() as u16;
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&(!len).to_le_bytes());
        out.extend_from_slice(block);
    }
    out.extend_from_slice(&adler32(data).to_be_bytes());
    out
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for byte in data {
        crc ^= *byte as u32;
        for _ in 0..8 {
            // Reflected CRC-32 polynomial (0x04C11DB7 reversed).
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
        }
    }
    !crc
}

fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for byte in data {
        a = (a + *byte as u32) % 65521;
        b = (b + a) % 65521;
    }
    (b << 16) | a
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::scan;
    use crate::order::arrange;
    use crate::spec::{DirMode, SortKey, ViewSpec};

    struct Scratch(PathBuf);
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn demo(label: &str, count: usize) -> Scratch {
        let dir = std::env::temp_dir().join(format!("magicfs-demo-test-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        create(Some(dir.clone()), count).unwrap();
        Scratch(dir)
    }

    fn order(dir: &Path, sort: SortKey) -> Vec<String> {
        let spec = ViewSpec { sort, dirs: DirMode::Exclude, ..Default::default() };
        arrange(scan(dir, &spec).unwrap(), &spec)
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .collect()
    }

    #[test]
    fn generates_the_requested_number_of_files() {
        let d = demo("count", 10);
        assert_eq!(order(&d.0, SortKey::Name).len(), 10);
    }

    #[test]
    fn scales_past_the_curated_set() {
        let d = demo("many", 25);
        assert_eq!(order(&d.0, SortKey::Name).len(), 25);
    }

    /// The whole point of the fixture: if two orderings agreed, the demo
    /// couldn't show that sorting works.
    #[test]
    fn name_size_and_time_orders_all_differ() {
        let d = demo("distinct", 10);
        let by_name = order(&d.0, SortKey::Name);
        let by_size = order(&d.0, SortKey::Size);
        let by_time = order(&d.0, SortKey::Time);

        assert_ne!(by_name, by_size, "size order must not match name order");
        assert_ne!(by_name, by_time, "time order must not match name order");
        assert_ne!(by_size, by_time, "size and time orders must not coincide");
    }

    #[test]
    fn natural_sort_is_demonstrable() {
        // Plain name order puts img10 before img2; natural order reverses it.
        let d = demo("natural", 10);
        let pos = |v: &[String], n: &str| v.iter().position(|x| x == n).unwrap();

        let by_name = order(&d.0, SortKey::Name);
        assert!(pos(&by_name, "img10.png") < pos(&by_name, "img2.png"));

        let natural = order(&d.0, SortKey::Natural);
        assert!(pos(&natural, "img2.png") < pos(&natural, "img10.png"));
    }

    #[test]
    fn sizes_span_the_advertised_range() {
        let d = demo("sizes", 10);
        let spec = ViewSpec { dirs: DirMode::Exclude, ..Default::default() };
        let sizes: Vec<u64> = scan(&d.0, &spec).unwrap().into_iter().map(|e| e.size).collect();
        assert_eq!(*sizes.iter().min().unwrap(), 0, "expected an empty file");
        let max = *sizes.iter().max().unwrap();
        assert!((500_000..=1_100_000).contains(&max), "largest was {max}");
    }

    #[test]
    fn images_are_real_pngs() {
        let d = demo("png", 10);
        let png = std::fs::read(d.0.join("bravo-forest.png")).unwrap();
        assert_eq!(&png[..8], &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]);
        assert_eq!(&png[12..16], b"IHDR");
        assert!(png.ends_with(b"\xaeB`\x82"), "missing IEND chunk CRC");
    }

    #[test]
    fn includes_non_images_so_filtering_is_demonstrable() {
        let d = demo("mixed", 10);
        let spec = ViewSpec {
            filter: vec!["images".into()],
            dirs: DirMode::Exclude,
            ..Default::default()
        };
        let images = arrange(scan(&d.0, &spec).unwrap(), &spec).unwrap();
        assert_eq!(images.len(), 8, "8 of the 10 entries should be images");
    }

    #[test]
    fn refuses_to_overwrite_a_directory_it_did_not_create() {
        let dir = std::env::temp_dir().join(format!("magicfs-demo-guard-{}", std::process::id()));
        let _guard = Scratch(dir.clone());
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("precious.txt"), b"mine").unwrap();

        let err = create(Some(dir.clone()), 5).unwrap_err().to_string();
        assert!(err.contains("refusing to overwrite"), "got: {err}");
        assert!(dir.join("precious.txt").exists());
    }

    #[test]
    fn rerunning_replaces_its_own_directory() {
        let d = demo("rerun", 5);
        assert_eq!(order(&d.0, SortKey::Name).len(), 5);
        create(Some(d.0.clone()), 10).unwrap();
        assert_eq!(order(&d.0, SortKey::Name).len(), 10);
    }
}
