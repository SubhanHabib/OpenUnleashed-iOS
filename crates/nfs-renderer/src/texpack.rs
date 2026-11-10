//! **Replacement textures** — PORT-DEVIATION (enhancement, opt-in): the original has no texture packs.
//!
//! Every texture the game uploads passes [`replace`] first (from `Renderer::make_texture`). A texture
//! is known by [`texture_hash`] of its top level as uploaded — the decoded texels, after the 16-bit
//! cache conversion (`TextureImage::cache_mips`) — so a pack needs no knowledge of the game's files:
//!
//! * **Packs**: a PNG named `<hash>.png` (16 lower-case hex digits) anywhere under a pack folder
//!   replaces that texture, at any size (a 2× or 4× upscale of the original keeps its aspect). The
//!   port builds the mip levels; folders whose name starts with `_` are skipped (the dump lives in one).
//! * **Dump**: with a dump folder set, every texture not yet there is written to `<hash>.png` — the
//!   originals a pack maker starts from.
//! * **Enhance**: without a pack file, a texture of up to 256×256 is upscaled 2× on the CPU
//!   (premultiplied Lanczos-3, then contrast-adaptive sharpening).
//!
//! Textures that are one colour (blank movie and render buffers, 1×1 defaults) are left alone, so
//! the frames written into them later go the usual way. A replaced texture that the game rewrites
//! later (a car's page recomposed with damage) is replaced again from the new pixels; when the pack
//! has no file for those, they are scaled up to the replacement's size (see `queue_texture_level`).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, RwLock};

/// A replacement: level 0 and the levels the port built below it.
pub(crate) struct Replaced {
    pub width: u32,
    pub height: u32,
    pub mips: Vec<Vec<u8>>,
}

#[derive(Default)]
struct State {
    pack: HashMap<u64, PathBuf>,
    dump: Option<PathBuf>,
    enhance: bool,
}

static ACTIVE: AtomicBool = AtomicBool::new(false);
static STATE: RwLock<Option<State>> = RwLock::new(None);
static DUMPED: Mutex<Option<HashSet<u64>>> = Mutex::new(None);
/// Counts for the log: textures replaced from a pack, textures enhanced and the time spent enhancing (µs).
static REPLACED_N: AtomicU64 = AtomicU64::new(0);
static ENHANCED_N: AtomicU64 = AtomicU64::new(0);
static ENHANCE_US: AtomicU64 = AtomicU64::new(0);

/// The largest replacement accepted (per side).
const MAX_SIDE: u32 = 8192;
/// The largest original the enhancer upscales (per side).
const ENHANCE_MAX: u32 = 256;

fn with_state(f: impl FnOnce(&mut State)) {
    let mut g = STATE.write().unwrap_or_else(|e| e.into_inner());
    let s = g.get_or_insert_with(State::default);
    f(s);
    ACTIVE.store(!s.pack.is_empty() || s.dump.is_some() || s.enhance, Ordering::Relaxed);
}

/// Whether any of packs, dump or enhance is on (the upload path skips the hash otherwise).
pub(crate) fn active() -> bool {
    ACTIVE.load(Ordering::Relaxed)
}

/// Index the pack folders (`dirs`, in order: the first file found for a hash wins; inside a folder,
/// entries are read in name order). Replaces the previous index. Returns the number of textures found.
pub fn set_texture_pack(dirs: &[PathBuf]) -> usize {
    let mut pack = HashMap::new();
    for d in dirs {
        scan(d, 0, &mut pack);
    }
    let n = pack.len();
    with_state(|s| s.pack = pack);
    tracing::info!(textures = n, dirs = ?dirs, "texture packs indexed");
    n
}

fn scan(dir: &Path, depth: u32, out: &mut HashMap<u64, PathBuf>) {
    if depth > 6 {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let mut entries: Vec<_> = rd.filter_map(|e| e.ok()).map(|e| e.path()).collect();
    entries.sort();
    for p in entries {
        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if p.is_dir() {
            if !name.starts_with('_') && !name.starts_with('.') {
                scan(&p, depth + 1, out);
            }
        } else if let Some(h) = hash_of_name(name) {
            out.entry(h).or_insert(p);
        }
    }
}

/// `0123456789abcdef.png` (either case) → the hash.
fn hash_of_name(name: &str) -> Option<u64> {
    let (stem, ext) = name.rsplit_once('.')?;
    (stem.len() == 16 && ext.eq_ignore_ascii_case("png")).then(|| u64::from_str_radix(stem, 16).ok()).flatten()
}

/// Write every texture uploaded from now on to `dir/<hash>.png` (once per hash; files already
/// there are kept). `None` stops the dump.
pub fn set_texture_dump(dir: Option<PathBuf>) {
    if let Some(d) = &dir
        && let Err(e) = std::fs::create_dir_all(d)
    {
        tracing::warn!(error = %e, dir = %d.display(), "texture dump folder not made");
    }
    with_state(|s| s.dump = dir);
}

/// Upscale textures that no pack replaces (2×, CPU, at upload).
pub fn set_texture_enhance(on: bool) {
    with_state(|s| s.enhance = on);
}

/// The stable 64-bit name of a texture: its size and its top level's RGBA texels. Packs are keyed by
/// it, so it must never change for the same pixels.
pub fn texture_hash(width: u32, height: u32, rgba: &[u8]) -> u64 {
    const K: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut h = ((u64::from(width) << 32) | u64::from(height)) ^ 0x6F70_656E_756E_6C65;
    let mut chunks = rgba.chunks_exact(8);
    for c in &mut chunks {
        let v = u64::from_le_bytes(c.try_into().expect("8 bytes"));
        h = (h ^ v).wrapping_mul(K).rotate_left(29);
    }
    for &b in chunks.remainder() {
        h = (h ^ u64::from(b)).wrapping_mul(K).rotate_left(29);
    }
    h ^= h >> 33;
    h = h.wrapping_mul(0xFF51_AFD7_ED55_8CCD);
    h ^= h >> 33;
    h = h.wrapping_mul(0xC4CE_B9FE_1A85_EC53);
    h ^ (h >> 33)
}

fn uniform(rgba: &[u8]) -> bool {
    let first = &rgba[..4.min(rgba.len())];
    rgba.chunks_exact(4).all(|p| p == first)
}

/// The replacement for a texture of `width`×`height` whose uploaded chain has `levels` levels and
/// whose top level is `level0`, or `None` to upload it as it is. `dump`: write it to the dump folder.
pub(crate) fn replace(width: u32, height: u32, levels: usize, level0: &[u8], dump: bool) -> Option<Replaced> {
    let len = 4 * width as usize * height as usize;
    if width == 0 || height == 0 || level0.len() < len {
        return None;
    }
    let level0 = &level0[..len];
    if uniform(level0) {
        return None;
    }
    let hash = texture_hash(width, height, level0);
    let (file, dump_dir, enhance) = {
        let g = STATE.read().unwrap_or_else(|e| e.into_inner());
        let s = g.as_ref()?;
        (s.pack.get(&hash).cloned(), s.dump.clone(), s.enhance)
    };
    if dump && let Some(d) = dump_dir {
        dump_one(&d, hash, width, height, level0);
    }
    if let Some(f) = file {
        match load_png(&f) {
            Ok((w, h, px)) => {
                if w * height != h * width {
                    tracing::warn!(file = %f.display(), w, h, orig_w = width, orig_h = height, "replacement texture's shape differs from the original's");
                }
                let n = REPLACED_N.fetch_add(1, Ordering::Relaxed) + 1;
                tracing::info!(file = %f.display(), w, h, replaced = n, "texture replaced");
                return Some(chain(width, levels, w, h, px));
            }
            Err(e) => tracing::warn!(file = %f.display(), error = %e, "replacement texture unreadable"),
        }
    }
    if enhance && width <= ENHANCE_MAX && height <= ENHANCE_MAX && width >= 8 && height >= 8 {
        let t = std::time::Instant::now();
        let px = enhance2x(width, height, level0);
        let spent = t.elapsed().as_micros() as u64;
        let us = ENHANCE_US.fetch_add(spent, Ordering::Relaxed) + spent;
        let n = ENHANCED_N.fetch_add(1, Ordering::Relaxed) + 1;
        if n.is_power_of_two() || n.is_multiple_of(256) {
            tracing::info!(enhanced = n, total_ms = us / 1000, "textures enhanced");
        }
        return Some(chain(width, levels, 2 * width, 2 * height, px));
    }
    None
}

fn dump_one(dir: &Path, hash: u64, w: u32, h: u32, px: &[u8]) {
    {
        let mut g = DUMPED.lock().unwrap_or_else(|e| e.into_inner());
        if !g.get_or_insert_with(HashSet::new).insert(hash) {
            return;
        }
    }
    let path = dir.join(format!("{hash:016x}.png"));
    if path.exists() {
        return;
    }
    if let Err(e) = image::save_buffer(&path, px, w, h, image::ExtendedColorType::Rgba8) {
        tracing::warn!(error = %e, path = %path.display(), "texture not dumped");
    }
}

fn load_png(path: &Path) -> anyhow::Result<(u32, u32, Vec<u8>)> {
    let bytes = std::fs::read(path)?;
    let img = image::load_from_memory_with_format(&bytes, image::ImageFormat::Png)?.to_rgba8();
    let (w, h) = img.dimensions();
    anyhow::ensure!(w > 0 && h > 0 && w <= MAX_SIDE && h <= MAX_SIDE, "size {w}x{h} out of range");
    Ok((w, h, img.into_raw()))
}

/// The levels for a replacement of `w`×`h` standing in for an original `orig_w` wide with `levels`
/// levels: as many more as the replacement is powers of two larger, so the sampler can still reach
/// the original's own resolution (a single-level original gets one level per doubling — no finer
/// filtering below it than the original had).
fn chain(orig_w: u32, levels: usize, w: u32, h: u32, px: Vec<u8>) -> Replaced {
    let extra = (w / orig_w.max(1)).max(1).ilog2() as usize;
    let full = (w.max(h).ilog2() + 1) as usize;
    let want = (levels.max(1) + extra).min(full);
    Replaced { width: w, height: h, mips: build_mips(w, h, px, want) }
}

/// `level0` and box-filtered halvings, `count` levels in all.
pub(crate) fn build_mips(w: u32, h: u32, level0: Vec<u8>, count: usize) -> Vec<Vec<u8>> {
    let mut out = vec![level0];
    let (mut cw, mut ch) = (w as usize, h as usize);
    while out.len() < count && (cw > 1 || ch > 1) {
        let (nw, nh) = ((cw / 2).max(1), (ch / 2).max(1));
        let src = out.last().expect("level");
        let mut dst = vec![0u8; nw * nh * 4];
        for y in 0..nh {
            for x in 0..nw {
                let (x0, y0) = ((2 * x).min(cw - 1), (2 * y).min(ch - 1));
                let (x1, y1) = ((2 * x + 1).min(cw - 1), (2 * y + 1).min(ch - 1));
                for c in 0..4 {
                    let s = |xx: usize, yy: usize| u32::from(src[(yy * cw + xx) * 4 + c]);
                    dst[(y * nw + x) * 4 + c] = ((s(x0, y0) + s(x1, y0) + s(x0, y1) + s(x1, y1) + 2) / 4) as u8;
                }
            }
        }
        out.push(dst);
        (cw, ch) = (nw, nh);
    }
    out
}

/// `px` (`w`×`h`) scaled to `nw`×`nh` (bilinear), with `count` levels.
pub(crate) fn resized(w: u32, h: u32, px: &[u8], nw: u32, nh: u32, count: usize) -> Replaced {
    let img = image::RgbaImage::from_raw(w, h, px[..4 * w as usize * h as usize].to_vec()).expect("size checked");
    let out = image::imageops::resize(&img, nw, nh, image::imageops::FilterType::Triangle).into_raw();
    Replaced { width: nw, height: nh, mips: build_mips(nw, nh, out, count) }
}

/// The on-device enhancer: 2× premultiplied Lanczos-3 over a wrapped border (most game textures
/// tile), then contrast-adaptive sharpening of the colour.
pub(crate) fn enhance2x(w: u32, h: u32, px: &[u8]) -> Vec<u8> {
    const PAD: u32 = 3;
    let (pw, ph) = (w + 2 * PAD, h + 2 * PAD);
    let mut padded = vec![0u8; (pw * ph * 4) as usize];
    for y in 0..ph {
        let sy = (y + h - PAD) % h;
        for x in 0..pw {
            let sx = (x + w - PAD) % w;
            let s = ((sy * w + sx) * 4) as usize;
            let d = ((y * pw + x) * 4) as usize;
            let a = u32::from(px[s + 3]);
            for c in 0..3 {
                padded[d + c] = ((u32::from(px[s + c]) * a + 127) / 255) as u8;
            }
            padded[d + 3] = a as u8;
        }
    }
    let img = image::RgbaImage::from_raw(pw, ph, padded).expect("padded size");
    let big = image::imageops::resize(&img, 2 * pw, 2 * ph, image::imageops::FilterType::Lanczos3);
    let (ow, oh) = (2 * w as usize, 2 * h as usize);
    let bw = (2 * pw) as usize;
    let off = (2 * PAD) as usize;
    let raw = big.as_raw();
    // un-premultiplied colour (0..1) and alpha of the cropped result
    let mut col = vec![[0f32; 3]; ow * oh];
    let mut alpha = vec![0u8; ow * oh];
    for y in 0..oh {
        for x in 0..ow {
            let s = ((y + off) * bw + x + off) * 4;
            let a = raw[s + 3];
            alpha[y * ow + x] = a;
            if a > 0 {
                let k = 1.0 / f32::from(a);
                col[y * ow + x] = [f32::from(raw[s]) * k, f32::from(raw[s + 1]) * k, f32::from(raw[s + 2]) * k].map(|v| v.min(1.0));
            }
        }
    }
    let out = cas(&col, ow, oh, 0.6);
    let mut rgba = vec![0u8; ow * oh * 4];
    for i in 0..ow * oh {
        for c in 0..3 {
            rgba[i * 4 + c] = (out[i][c].clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
        }
        rgba[i * 4 + 3] = alpha[i];
    }
    rgba
}

/// Contrast-adaptive sharpening (after AMD FidelityFX CAS, MIT): each channel pulled away from its
/// four neighbours by a weight that shrinks where the neighbourhood is already near black or white.
fn cas(src: &[[f32; 3]], w: usize, h: usize, sharpness: f32) -> Vec<[f32; 3]> {
    let peak = -1.0 / (8.0 - 3.0 * sharpness.clamp(0.0, 1.0));
    let at = |x: isize, y: isize| src[(y.clamp(0, h as isize - 1) as usize) * w + x.clamp(0, w as isize - 1) as usize];
    let mut out = vec![[0f32; 3]; w * h];
    for y in 0..h as isize {
        for x in 0..w as isize {
            let (e, b, d, f, hh) = (at(x, y), at(x, y - 1), at(x - 1, y), at(x + 1, y), at(x, y + 1));
            let mut o = [0f32; 3];
            for c in 0..3 {
                let mn = e[c].min(b[c]).min(d[c]).min(f[c]).min(hh[c]);
                let mx = e[c].max(b[c]).max(d[c]).max(f[c]).max(hh[c]);
                let amp = if mx > 0.0 { (mn.min(1.0 - mx) / mx).clamp(0.0, 1.0).sqrt() } else { 0.0 };
                let wgt = amp * peak;
                o[c] = ((b[c] + d[c] + f[c] + hh[c]) * wgt + e[c]) / (1.0 + 4.0 * wgt);
            }
            out[y as usize * w + x as usize] = o;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_is_stable_and_size_aware() {
        let px: Vec<u8> = (0..64u32 * 4).map(|i| (i * 7 % 251) as u8).collect();
        // pinned: packs depend on this value never changing
        assert_eq!(texture_hash(8, 8, &px), texture_hash(8, 8, &px));
        assert_ne!(texture_hash(8, 8, &px), texture_hash(16, 4, &px));
        let mut other = px.clone();
        other[100] ^= 1;
        assert_ne!(texture_hash(8, 8, &px), texture_hash(8, 8, &other));
        assert_eq!(hash_of_name(&format!("{:016x}.png", texture_hash(8, 8, &px))), Some(texture_hash(8, 8, &px)));
        assert_eq!(hash_of_name("0123456789ABCDEF.PNG"), Some(0x0123_4567_89AB_CDEF));
        assert_eq!(hash_of_name("readme.png"), None);
    }

    #[test]
    fn chain_adds_a_level_per_doubling() {
        let r = chain(64, 1, 128, 128, vec![0; 128 * 128 * 4]);
        assert_eq!(r.mips.len(), 2);
        assert_eq!(r.mips[1].len(), 64 * 64 * 4);
        let r = chain(64, 7, 256, 256, vec![0; 256 * 256 * 4]);
        assert_eq!(r.mips.len(), 9);
    }

    #[test]
    fn enhance_doubles_and_keeps_flat_areas() {
        let mut px = vec![0u8; 16 * 16 * 4];
        for (i, p) in px.chunks_exact_mut(4).enumerate() {
            let v = if (i % 16) < 8 { 40 } else { 200 };
            p.copy_from_slice(&[v, v, v, 255]);
        }
        let out = enhance2x(16, 16, &px);
        assert_eq!(out.len(), 32 * 32 * 4);
        // a pixel well inside the dark half stays dark, one inside the bright half bright
        assert!((i32::from(out[(10 * 32 + 6) * 4]) - 40).abs() <= 3);
        assert!((i32::from(out[(10 * 32 + 22) * 4]) - 200).abs() <= 3);
        assert!(out.chunks_exact(4).all(|p| p[3] == 255));
    }
}
