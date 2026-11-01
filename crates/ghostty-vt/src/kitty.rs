//! Kitty images the terminal keeps, sent on to viewers as pixels only.
//! A viewer may be on another machine, where a path names nothing, and the
//! core deletes a temporary file once it reads it, so the core loads every
//! medium itself and viewers get what it loaded. A viewer that attaches
//! later gets the stored images again with their placements.

use std::collections::HashSet;
use std::ffi::c_void;
use std::ptr;
use std::sync::Once;

use super::{Terminal, ffi};

type Graphics = *mut c_void;
type Image = *const c_void;
type Placements = *mut c_void;

const SYS_OPT_DECODE_PNG: i32 = 1;
const OPT_STORAGE_LIMIT: i32 = 15;
const OPT_MEDIUM_FILE: i32 = 16;
const OPT_MEDIUM_TEMP_FILE: i32 = 17;
const OPT_MEDIUM_SHARED_MEM: i32 = 18;
const DATA_GRAPHICS: i32 = 30;
const GRAPHICS_PLACEMENT_ITERATOR: i32 = 1;
const PLACEMENT_IMAGE_ID: i32 = 1;
const PLACEMENT_ID: i32 = 2;
const PLACEMENT_IS_VIRTUAL: i32 = 3;
const PLACEMENT_X_OFFSET: i32 = 4;
const PLACEMENT_Y_OFFSET: i32 = 5;
const PLACEMENT_COLUMNS: i32 = 10;
const PLACEMENT_ROWS: i32 = 11;
const PLACEMENT_Z: i32 = 12;
const IMAGE_WIDTH: i32 = 3;
const IMAGE_HEIGHT: i32 = 4;
const IMAGE_FORMAT: i32 = 5;
const IMAGE_DATA_PTR: i32 = 7;
const IMAGE_DATA_LEN: i32 = 8;
const FORMAT_RGB: i32 = 0;
const FORMAT_RGBA: i32 = 1;
const FORMAT_GRAY_ALPHA: i32 = 3;
const FORMAT_GRAY: i32 = 4;

/// Image bytes each screen keeps, as Herdr VSH's viewer does.
const STORAGE_LIMIT: u64 = 128 << 20;
/// Base64 bytes per transmission chunk, as the kitty protocol asks.
const CHUNK: usize = 4096;

#[repr(C)]
struct SysImage {
    width: u32,
    height: u32,
    data: *mut u8,
    data_len: usize,
}

#[repr(C)]
struct RenderInfo {
    size: usize,
    pixel_width: u32,
    pixel_height: u32,
    grid_cols: u32,
    grid_rows: u32,
    viewport_col: i32,
    viewport_row: i32,
    viewport_visible: bool,
    source_x: u32,
    source_y: u32,
    source_width: u32,
    source_height: u32,
}

unsafe extern "C" {
    fn ghostty_sys_set(opt: i32, value: *const c_void) -> i32;
    fn ghostty_alloc(alloc: *const c_void, len: usize) -> *mut u8;
    fn ghostty_kitty_graphics_get(g: Graphics, data: i32, out: *mut c_void) -> i32;
    fn ghostty_kitty_graphics_image(g: Graphics, id: u32) -> Image;
    fn ghostty_kitty_graphics_image_get(image: Image, data: i32, out: *mut c_void) -> i32;
    fn ghostty_kitty_graphics_placement_iterator_new(alloc: *const c_void, out: *mut Placements) -> i32;
    fn ghostty_kitty_graphics_placement_iterator_free(it: Placements);
    fn ghostty_kitty_graphics_placement_next(it: Placements) -> bool;
    fn ghostty_kitty_graphics_placement_get(it: Placements, data: i32, out: *mut c_void) -> i32;
    fn ghostty_kitty_graphics_placement_render_info(
        it: Placements,
        image: Image,
        t: ffi::Terminal,
        out: *mut RenderInfo,
    ) -> i32;
}

/// Lets a terminal keep images: a storage limit, every medium on this
/// machine (as Ghostty takes them), and PNG decoding.
pub(crate) fn enable(raw: ffi::Terminal) {
    static PNG: Once = Once::new();
    PNG.call_once(|| unsafe {
        ghostty_sys_set(SYS_OPT_DECODE_PNG, decode_png as *const c_void);
    });
    let (limit, on) = (STORAGE_LIMIT, true);
    let tmp = std::env::temp_dir();
    let tmp = tmp.to_string_lossy();
    let tmp = ffi::Str { ptr: tmp.as_ptr(), len: tmp.len() };
    unsafe {
        ffi::ghostty_terminal_set(raw, OPT_STORAGE_LIMIT, (&raw const limit).cast());
        ffi::ghostty_terminal_set(raw, OPT_MEDIUM_FILE, (&raw const on).cast());
        ffi::ghostty_terminal_set(raw, OPT_MEDIUM_SHARED_MEM, (&raw const on).cast());
        ffi::ghostty_terminal_set(raw, OPT_MEDIUM_TEMP_FILE, (&raw const tmp).cast());
    }
}

extern "C" fn decode_png(
    _: *mut c_void,
    alloc: *const c_void,
    data: *const u8,
    len: usize,
    out: *mut SysImage,
) -> bool {
    let Some((width, height, rgba)) = png_rgba(unsafe { std::slice::from_raw_parts(data, len) }) else {
        return false;
    };
    let buf = unsafe { ghostty_alloc(alloc, rgba.len()) };
    if buf.is_null() {
        return false;
    }
    unsafe {
        ptr::copy_nonoverlapping(rgba.as_ptr(), buf, rgba.len());
        *out = SysImage { width, height, data: buf, data_len: rgba.len() };
    }
    true
}

fn png_rgba(bytes: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder.read_info().ok()?;
    let (w, h) = (reader.info().width, reader.info().height);
    // Refuse what the storage would refuse anyway, before allocating it.
    if w as u64 * h as u64 * 4 > STORAGE_LIMIT {
        return None;
    }
    let mut buf = vec![0; reader.output_buffer_size()?];
    let frame = reader.next_frame(&mut buf).ok()?;
    buf.truncate(frame.buffer_size());
    let rgba = match frame.color_type {
        png::ColorType::Rgba => buf,
        png::ColorType::Rgb => buf.chunks_exact(3).flat_map(|p| [p[0], p[1], p[2], 255]).collect(),
        png::ColorType::GrayscaleAlpha => buf.chunks_exact(2).flat_map(|p| [p[0], p[0], p[0], p[1]]).collect(),
        png::ColorType::Grayscale => buf.iter().flat_map(|&g| [g, g, g, 255]).collect(),
        png::ColorType::Indexed => return None,
    };
    Some((frame.width, frame.height, rgba))
}

/// `k=v` pairs of a kitty graphics control string (`G` already removed).
fn keys(control: &[u8]) -> Vec<(u8, &[u8])> {
    control.split(|b| *b == b',').filter_map(|kv| Some((*kv.first()?, kv.strip_prefix(&[kv[0], b'='][..])?))).collect()
}

fn number(v: &[u8]) -> Option<u32> {
    std::str::from_utf8(v).ok()?.parse().ok()
}

impl Terminal {
    fn graphics(&self) -> Option<Graphics> {
        let mut g: Graphics = ptr::null_mut();
        let rc = unsafe { ffi::ghostty_terminal_get(self.raw, DATA_GRAPHICS, (&raw mut g).cast()) };
        (rc == ffi::SUCCESS && !g.is_null()).then_some(g)
    }

    /// An image's pixels as kitty sends them: `f` (24 or 32), width, height, bytes.
    fn pixels(&self, g: Graphics, id: u32) -> Option<(u32, u32, u32, Vec<u8>)> {
        let image = unsafe { ghostty_kitty_graphics_image(g, id) };
        if image.is_null() {
            return None;
        }
        let (mut w, mut h, mut format, mut data, mut len) = (0u32, 0u32, 0i32, ptr::null::<u8>(), 0usize);
        unsafe {
            ghostty_kitty_graphics_image_get(image, IMAGE_WIDTH, (&raw mut w).cast());
            ghostty_kitty_graphics_image_get(image, IMAGE_HEIGHT, (&raw mut h).cast());
            ghostty_kitty_graphics_image_get(image, IMAGE_FORMAT, (&raw mut format).cast());
            ghostty_kitty_graphics_image_get(image, IMAGE_DATA_LEN, (&raw mut len).cast());
            if ghostty_kitty_graphics_image_get(image, IMAGE_DATA_PTR, (&raw mut data).cast()) != ffi::SUCCESS
                || data.is_null()
            {
                return None;
            }
        }
        let bytes = unsafe { std::slice::from_raw_parts(data, len) };
        match format {
            FORMAT_RGB => Some((24, w, h, bytes.to_vec())),
            FORMAT_RGBA => Some((32, w, h, bytes.to_vec())),
            FORMAT_GRAY_ALPHA => {
                Some((32, w, h, bytes.chunks_exact(2).flat_map(|p| [p[0], p[0], p[0], p[1]]).collect()))
            }
            FORMAT_GRAY => Some((32, w, h, bytes.iter().flat_map(|&v| [v, v, v, 255]).collect())),
            _ => None,
        }
    }

    /// For a transmission this terminal just loaded from a file, temporary
    /// file or shared memory (`control` is the APC body before `;`, from
    /// `G`): the same command with the loaded pixels in place of the name.
    /// `None` for anything else, which viewers get as it came.
    pub fn kitty_as_pixels(&self, control: &[u8]) -> Option<Vec<u8>> {
        let keys = keys(control.strip_prefix(b"G")?);
        let get = |k: u8| keys.iter().find(|(key, _)| *key == k).map(|(_, v)| *v);
        if !matches!(get(b't'), Some(b"f" | b"t" | b"s")) || !matches!(get(b'a'), None | Some(b"t" | b"T")) {
            return None;
        }
        let id = number(get(b'i')?)?;
        let (f, w, h, data) = self.pixels(self.graphics()?, id)?;
        let mut head = format!("t=d,f={f},s={w},v={h}");
        for (k, v) in &keys {
            if !b"tfsvSOom".contains(k) {
                head.push_str(&format!(",{}={}", *k as char, String::from_utf8_lossy(v)));
            }
        }
        let quiet = get(b'q').map(|q| format!(",q={}", String::from_utf8_lossy(q))).unwrap_or_default();
        Some(transmission(&head, &quiet, &data))
    }

    /// Every stored image that is on screen or placed by placeholder cells,
    /// sent again as pixels with its placement, the cursor left where it was.
    pub(crate) fn kitty_repaint(&self) -> Vec<u8> {
        let Some(g) = self.graphics() else { return Vec::new() };
        let mut it: Placements = ptr::null_mut();
        if unsafe { ghostty_kitty_graphics_placement_iterator_new(ptr::null(), &mut it) } != ffi::SUCCESS {
            return Vec::new();
        }
        let mut out = Vec::new();
        let mut sent = HashSet::new();
        if unsafe { ghostty_kitty_graphics_get(g, GRAPHICS_PLACEMENT_ITERATOR, (&raw mut it).cast()) } == ffi::SUCCESS {
            while unsafe { ghostty_kitty_graphics_placement_next(it) } {
                if let Some((id, place)) = self.placement(g, it) {
                    if sent.insert(id) {
                        let Some((f, w, h, data)) = self.pixels(g, id) else { continue };
                        out.extend(transmission(&format!("a=t,i={id},f={f},s={w},v={h},q=2"), ",q=2", &data));
                    }
                    out.extend(place);
                }
            }
        }
        unsafe { ghostty_kitty_graphics_placement_iterator_free(it) };
        if out.is_empty() {
            return out;
        }
        // Absolute rows for the placements, then back to the program's cursor and origin mode.
        let mut wrapped = b"\x1b7\x1b[?6l".to_vec();
        wrapped.extend(out);
        wrapped.extend(b"\x1b8");
        wrapped
    }

    /// The command that places the iterator's placement again, and its image id.
    fn placement(&self, g: Graphics, it: Placements) -> Option<(u32, Vec<u8>)> {
        let (mut id, mut pid, mut virt, mut x, mut y, mut cols, mut rows, mut z) =
            (0u32, 0u32, false, 0u32, 0u32, 0u32, 0u32, 0i32);
        unsafe {
            ghostty_kitty_graphics_placement_get(it, PLACEMENT_IMAGE_ID, (&raw mut id).cast());
            ghostty_kitty_graphics_placement_get(it, PLACEMENT_ID, (&raw mut pid).cast());
            ghostty_kitty_graphics_placement_get(it, PLACEMENT_IS_VIRTUAL, (&raw mut virt).cast());
            ghostty_kitty_graphics_placement_get(it, PLACEMENT_X_OFFSET, (&raw mut x).cast());
            ghostty_kitty_graphics_placement_get(it, PLACEMENT_Y_OFFSET, (&raw mut y).cast());
            ghostty_kitty_graphics_placement_get(it, PLACEMENT_COLUMNS, (&raw mut cols).cast());
            ghostty_kitty_graphics_placement_get(it, PLACEMENT_ROWS, (&raw mut rows).cast());
            ghostty_kitty_graphics_placement_get(it, PLACEMENT_Z, (&raw mut z).cast());
        }
        let p = if pid == 0 { String::new() } else { format!(",p={pid}") };
        if virt {
            // The placeholder cells are text, repainted with the screen.
            return Some((id, apc(&format!("a=p,U=1,i={id}{p},c={cols},r={rows},q=2"), b"")));
        }
        let image = unsafe { ghostty_kitty_graphics_image(g, id) };
        let mut info = RenderInfo {
            size: size_of::<RenderInfo>(),
            pixel_width: 0,
            pixel_height: 0,
            grid_cols: 0,
            grid_rows: 0,
            viewport_col: 0,
            viewport_row: 0,
            viewport_visible: false,
            source_x: 0,
            source_y: 0,
            source_width: 0,
            source_height: 0,
        };
        let rc = unsafe { ghostty_kitty_graphics_placement_render_info(it, image, self.raw, &mut info) };
        if image.is_null() || rc != ffi::SUCCESS || !info.viewport_visible || info.pixel_height == 0 {
            return None;
        }
        // Scrolled partly above the screen: place what is still on it, from row 0.
        let (mut row, mut sy, mut sh) = (info.viewport_row, info.source_y, info.source_height);
        if row < 0 {
            let hidden = row.unsigned_abs();
            // Pixels drawn above row 0, in the source image's scale.
            let above = (hidden as u64 * self.hooks.cell.1 as u64).saturating_sub(y as u64);
            let cut =
                (above * info.source_height as u64 / info.pixel_height as u64).min(info.source_height as u64) as u32;
            (row, sy, sh, y) = (0, info.source_y + cut, info.source_height - cut, 0);
            rows = rows.saturating_sub(hidden);
            if sh == 0 {
                return None;
            }
        }
        let size = [(b'c', cols), (b'r', rows)]
            .iter()
            .filter(|(_, n)| *n > 0)
            .map(|(k, n)| format!(",{}={n}", *k as char))
            .collect::<String>();
        let at = format!("\x1b[{};{}H", row + 1, info.viewport_col.max(0) + 1);
        let mut cmd = at.into_bytes();
        cmd.extend(apc(
            &format!(
                "a=p,i={id}{p},x={},y={sy},w={},h={sh},X={x},Y={y}{size},z={z},C=1,q=2",
                info.source_x, info.source_width
            ),
            b"",
        ));
        Some((id, cmd))
    }
}

/// A transmission split into the chunks the kitty protocol allows.
fn transmission(head: &str, quiet: &str, data: &[u8]) -> Vec<u8> {
    let encoded = base64(data);
    let chunks: Vec<&[u8]> = encoded.chunks(CHUNK).collect();
    let mut out = Vec::new();
    for (n, chunk) in chunks.iter().enumerate() {
        let more = u8::from(n + 1 < chunks.len());
        let keys = if n == 0 { format!("{head},m={more}") } else { format!("m={more}{quiet}") };
        out.extend(apc(&keys, chunk));
    }
    out
}

fn apc(keys: &str, payload: &[u8]) -> Vec<u8> {
    let mut out = format!("\x1b_G{keys}").into_bytes();
    if !payload.is_empty() {
        out.push(b';');
        out.extend_from_slice(payload);
    }
    out.extend(b"\x1b\\");
    out
}

pub(crate) fn base64(data: &[u8]) -> Vec<u8> {
    const ABC: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = Vec::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        out.extend([ABC[(n >> 18) as usize & 63], ABC[(n >> 12) as usize & 63]]);
        out.push(if c.len() > 1 { ABC[(n >> 6) as usize & 63] } else { b'=' });
        out.push(if c.len() > 2 { ABC[n as usize & 63] } else { b'=' });
    }
    out
}
