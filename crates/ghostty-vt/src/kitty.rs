//! Kitty images the terminal keeps, sent on to viewers as PNG, never as a name.
//! A viewer may be on another machine, where a path names nothing, and the
//! core deletes a temporary file once it reads it, so the core loads every
//! medium itself and viewers get what it loaded. A viewer that attaches
//! later gets the stored images again with their placements.

use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::ptr;
use std::sync::{Arc, Once};

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
const POINT_SCREEN: i32 = 2;
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
const IMAGE_GENERATION: i32 = 9;
const FORMAT_RGB: i32 = 0;
const FORMAT_RGBA: i32 = 1;
const FORMAT_GRAY_ALPHA: i32 = 3;
const FORMAT_GRAY: i32 = 4;

/// The most a decoded image may take: Ghostty's own cap for one image.
const MAX_IMAGE_BYTES: u64 = 400 * 1000 * 1000;
/// Base64 bytes per transmission chunk, as the kitty protocol asks.
const CHUNK: usize = 4096;

#[repr(C)]
struct SysImage {
    width: u32,
    height: u32,
    data: *mut u8,
    data_len: usize,
}

/// Filled by libghostty; every field keeps the layout, read or not.
#[repr(C)]
#[allow(dead_code)]
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
    fn ghostty_kitty_graphics_placement_rect(
        it: Placements,
        image: Image,
        t: ffi::Terminal,
        out: *mut ffi::Selection,
    ) -> i32;
    fn ghostty_terminal_point_from_grid_ref(
        t: ffi::Terminal,
        r: *const ffi::GridRef,
        tag: i32,
        out: *mut ffi::Coord,
    ) -> i32;
}

/// What a repaint sends to show the images again: images placed in history,
/// first; their placements, made while history is written, at a line counted
/// from the first history line sent; then, after the screen is cleared and
/// painted, the other images and the placements on the screen or by
/// placeholder cells, the cursor left where it was. (Clearing the screen
/// deletes images with no placement yet, so those are sent after it.)
#[derive(Default)]
pub(crate) struct Replay {
    pub images: Vec<u8>,
    pub history: Vec<(u32, Vec<u8>)>,
    pub screen: Vec<u8>,
}

/// Where a placement goes again.
enum At {
    Screen,
    History(u32),
}

/// How a terminal keeps images and sends them to viewers.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ImageSettings {
    /// Image bytes each screen keeps, decoded (Ghostty's default: 320 MB).
    pub storage_bytes: u64,
    /// A repaint sends each image at most as large as it is drawn, in the
    /// viewer's pixels, instead of at full resolution.
    pub scale_to_display: bool,
    /// How hard PNG compresses what viewers get.
    pub png: PngLevel,
}

impl Default for ImageSettings {
    fn default() -> Self {
        ImageSettings { storage_bytes: 320 * 1000 * 1000, scale_to_display: true, png: PngLevel::Fast }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PngLevel {
    Fastest,
    Fast,
    Balanced,
    High,
}

impl PngLevel {
    fn compression(self) -> png::Compression {
        match self {
            PngLevel::Fastest => png::Compression::Fastest,
            PngLevel::Fast => png::Compression::Fast,
            PngLevel::Balanced => png::Compression::Balanced,
            PngLevel::High => png::Compression::High,
        }
    }
}

/// One placement, as a repaint makes it again.
struct Placed {
    id: u32,
    pid: u32,
    at: At,
    /// Drawn pixels per image pixel.
    drawn_scale: f64,
    shape: Shape,
}

enum Shape {
    /// Unicode placeholder cells (text, repainted with the screen) show it.
    Placeholder { cols: u32, rows: u32 },
    /// At a cell: on screen, its row; in history, written at the start of its line.
    Placed { row: u32, col: u32, source: [u32; 4], offset: [u32; 2], cells: [u32; 2], grid: [u32; 2], z: i32 },
}

impl Placed {
    /// The command that makes it, for an image sent `scaled` (its pixels
    /// per original pixel) or at full size.
    fn command(&self, scaled: Option<f64>) -> Vec<u8> {
        let (id, p) = (self.id, if self.pid == 0 { String::new() } else { format!(",p={}", self.pid) });
        let (row, col, [sx, sy, sw, sh], [x, y], [cols, rows], grid, z) = match self.shape {
            Shape::Placeholder { cols, rows } => {
                return apc(&format!("a=p,U=1,i={id}{p},c={cols},r={rows},q=2"), b"");
            }
            Shape::Placed { row, col, source, offset, cells, grid, z } => (row, col, source, offset, cells, grid, z),
        };
        // A smaller image: its rectangle in its own pixels, and the cells it
        // took at full size, so it is drawn as large as before.
        let (source, cells) = match scaled {
            Some(k) => {
                let px = |v: u32| (v as f64 * k).round() as u32;
                let cells = [if cols > 0 { cols } else { grid[0] }, if rows > 0 { rows } else { grid[1] }];
                ([px(sx), px(sy), px(sw).max(1), px(sh).max(1)], cells)
            }
            None => ([sx, sy, sw, sh], [cols, rows]),
        };
        let size: String = [(b'c', cells[0]), (b'r', cells[1])]
            .iter()
            .filter(|(_, n)| *n > 0)
            .map(|(k, n)| format!(",{}={n}", *k as char))
            .collect();
        let [sx, sy, sw, sh] = source;
        let place = apc(&format!("a=p,i={id}{p},x={sx},y={sy},w={sw},h={sh},X={x},Y={y}{size},z={z},C=1,q=2"), b"");
        match self.at {
            At::Screen => [format!("\x1b[{};{}H", row + 1, col + 1).into_bytes(), place].concat(),
            // Written at the start of its line; back there for the line's text.
            At::History(_) => [format!("\x1b[{}G", col + 1).into_bytes(), place, b"\r".to_vec()].concat(),
        }
    }
}

/// `pixels` (`channels` bytes each, `from` wide and high) as `to`, each new
/// pixel the average of the ones it covers.
fn downscale(pixels: &[u8], channels: usize, from: (u32, u32), to: (u32, u32)) -> Vec<u8> {
    let ((fw, fh), (tw, th)) = ((from.0 as usize, from.1 as usize), (to.0 as usize, to.1 as usize));
    let mut out = vec![0u8; tw * th * channels];
    let mut sum = vec![0u64; channels];
    for ty in 0..th {
        let (y0, y1) = (ty * fh / th, ((ty + 1) * fh / th).max(ty * fh / th + 1));
        for tx in 0..tw {
            let (x0, x1) = (tx * fw / tw, ((tx + 1) * fw / tw).max(tx * fw / tw + 1));
            sum.iter_mut().for_each(|s| *s = 0);
            for y in y0..y1 {
                let row = &pixels[(y * fw + x0) * channels..(y * fw + x1) * channels];
                for px in row.chunks_exact(channels) {
                    sum.iter_mut().zip(px).for_each(|(s, &v)| *s += v as u64);
                }
            }
            let n = ((y1 - y0) * (x1 - x0)) as u64;
            let at = (ty * tw + tx) * channels;
            out[at..at + channels].iter_mut().zip(&sum).for_each(|(o, s)| *o = (s / n) as u8);
        }
    }
    out
}

/// Lets a terminal keep images: a storage limit, every medium on this
/// machine (as Ghostty takes them), and PNG decoding.
pub(crate) fn enable(raw: ffi::Terminal) {
    static PNG: Once = Once::new();
    PNG.call_once(|| unsafe {
        ghostty_sys_set(SYS_OPT_DECODE_PNG, decode_png as *const c_void);
    });
    let (limit, on) = (ImageSettings::default().storage_bytes, true);
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
    // Refuse what Ghostty would refuse anyway, before allocating it.
    if w as u64 * h as u64 * 4 > MAX_IMAGE_BYTES {
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

    /// How this terminal keeps images and sends them to viewers.
    pub fn set_images(&mut self, images: ImageSettings) {
        self.images = images;
        let limit = images.storage_bytes;
        unsafe { ffi::ghostty_terminal_set(self.raw, OPT_STORAGE_LIMIT, (&raw const limit).cast()) };
    }

    /// An image as viewers get it, `size` pixels wide and high at most: PNG
    /// (far smaller than raw pixels: a 4K image is 33 MB of RGBA, 44 MB once
    /// base64-encoded), in base64. Made once per image generation and size;
    /// repaints reuse it.
    fn encoded(&self, g: Graphics, id: u32, size: Option<(u32, u32)>) -> Option<Arc<Vec<u8>>> {
        let image = unsafe { ghostty_kitty_graphics_image(g, id) };
        let mut generation = 0u64;
        if image.is_null()
            || unsafe { ghostty_kitty_graphics_image_get(image, IMAGE_GENERATION, (&raw mut generation).cast()) }
                != ffi::SUCCESS
        {
            return None;
        }
        let (color, w, h, pixels) = self.stored(g, id)?;
        let (tw, th) = size.unwrap_or((w, h));
        if let Some((made, made_size, data)) = self.sent_images.borrow().get(&id)
            && (*made, *made_size) == (generation, (tw, th))
        {
            return Some(data.clone());
        }
        let scaled;
        let pixels = if (tw, th) == (w, h) {
            pixels
        } else {
            scaled = downscale(pixels, color.samples(), (w, h), (tw, th));
            &scaled
        };
        let mut png = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut png, tw, th);
            encoder.set_color(color);
            encoder.set_depth(png::BitDepth::Eight);
            encoder.set_compression(self.images.png.compression());
            encoder.write_header().ok()?.write_image_data(pixels).ok()?;
        }
        let data = Arc::new(base64(&png));
        self.sent_images.borrow_mut().insert(id, (generation, (tw, th), data.clone()));
        Some(data)
    }

    /// The pixels the storage keeps for an image (borrowed until the terminal changes).
    fn stored(&self, g: Graphics, id: u32) -> Option<(png::ColorType, u32, u32, &[u8])> {
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
        let color = match format {
            FORMAT_RGB => png::ColorType::Rgb,
            FORMAT_RGBA => png::ColorType::Rgba,
            FORMAT_GRAY_ALPHA => png::ColorType::GrayscaleAlpha,
            FORMAT_GRAY => png::ColorType::Grayscale,
            _ => return None,
        };
        Some((color, w, h, unsafe { std::slice::from_raw_parts(data, len) }))
    }

    /// An image's stored pixels as RGBA, for tests that check what a viewer kept.
    #[cfg(test)]
    pub(crate) fn kitty_rgba(&self, id: u32) -> Option<Vec<u8>> {
        let (color, _, _, data) = self.stored(self.graphics()?, id)?;
        Some(match color {
            png::ColorType::Rgb => data.chunks_exact(3).flat_map(|p| [p[0], p[1], p[2], 255]).collect(),
            png::ColorType::GrayscaleAlpha => data.chunks_exact(2).flat_map(|p| [p[0], p[0], p[0], p[1]]).collect(),
            png::ColorType::Grayscale => data.iter().flat_map(|&v| [v, v, v, 255]).collect(),
            _ => data.to_vec(),
        })
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
        // Full size: the program may still place it by pixel rectangles.
        let data = self.encoded(self.graphics()?, id, None)?;
        let mut head = "t=d,f=100".to_string();
        for (k, v) in &keys {
            if !b"tfsvSOom".contains(k) {
                head.push_str(&format!(",{}={}", *k as char, String::from_utf8_lossy(v)));
            }
        }
        let quiet = get(b'q').map(|q| format!(",q={}", String::from_utf8_lossy(q))).unwrap_or_default();
        Some(transmission(&head, &quiet, &data))
    }

    /// Every stored image that is on screen, in the history lines a repaint
    /// sends (`history`: the first such line, in screen rows), or placed by
    /// placeholder cells.
    pub(crate) fn kitty_repaint(&self, history: Option<u32>) -> Replay {
        let mut replay = Replay::default();
        let Some(g) = self.graphics() else { return replay };
        let mut it: Placements = ptr::null_mut();
        if unsafe { ghostty_kitty_graphics_placement_iterator_new(ptr::null(), &mut it) } != ffi::SUCCESS {
            return replay;
        }
        let mut placed = Vec::new();
        if unsafe { ghostty_kitty_graphics_get(g, GRAPHICS_PLACEMENT_ITERATOR, (&raw mut it).cast()) } == ffi::SUCCESS {
            while unsafe { ghostty_kitty_graphics_placement_next(it) } {
                placed.extend(self.placement(g, it, history));
            }
        }
        unsafe { ghostty_kitty_graphics_placement_iterator_free(it) };
        let in_history: HashSet<u32> = placed.iter().filter(|p| matches!(p.at, At::History(_))).map(|p| p.id).collect();
        // Each image no larger than its largest placement draws it.
        let mut scale: HashMap<u32, f64> = HashMap::new();
        for p in &placed {
            let f = if self.images.scale_to_display { p.drawn_scale } else { 1.0 };
            let s = scale.entry(p.id).or_insert(0.0);
            *s = s.max(f).min(1.0);
        }
        let mut sent = HashSet::new();
        let mut screen = Vec::new();
        for p in placed {
            let Some((_, w, h, _)) = self.stored(g, p.id) else { continue };
            let f = scale[&p.id];
            // Within a few percent of full size: not worth a second encoding.
            let size =
                (f < 0.95).then(|| (((w as f64 * f).ceil() as u32).max(1), ((h as f64 * f).ceil() as u32).max(1)));
            // An image goes out once, before its first placement.
            if sent.insert(p.id) {
                let Some(data) = self.encoded(g, p.id, size) else { continue };
                let image = transmission(&format!("a=t,i={},f=100,q=2", p.id), ",q=2", &data);
                if in_history.contains(&p.id) { replay.images.extend(image) } else { screen.extend(image) }
            }
            let place = p.command(size.map(|(sw, _)| sw as f64 / w as f64));
            match p.at {
                At::Screen => screen.extend(place),
                At::History(line) => replay.history.push((line, place)),
            }
        }
        // Encodings of images no longer placed go; one placed again is encoded again.
        self.sent_images.borrow_mut().retain(|id, _| sent.contains(id));
        replay.history.sort_by_key(|(line, _)| *line);
        if !screen.is_empty() {
            // Absolute rows for the placements, then back to the program's cursor and origin mode.
            replay.screen = [b"\x1b7\x1b[?6l".as_slice(), &screen, b"\x1b8"].concat();
        }
        replay
    }

    /// The iterator's placement, as a repaint makes it again.
    fn placement(&self, g: Graphics, it: Placements, history: Option<u32>) -> Option<Placed> {
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
        let image = unsafe { ghostty_kitty_graphics_image(g, id) };
        if virt {
            // The placeholder cells are text, repainted with the screen; the
            // image fits their box, keeping its shape.
            let (_, w, h, _) = self.stored(g, id)?;
            let (cw, ch) = self.hooks.cell;
            let fit = (cols as f64 * cw as f64 / w.max(1) as f64).min(rows as f64 * ch as f64 / h.max(1) as f64);
            return Some(Placed {
                id,
                pid,
                at: At::Screen,
                drawn_scale: fit,
                shape: Shape::Placeholder { cols, rows },
            });
        }
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
        if image.is_null() {
            return None;
        }
        let rc = unsafe { ghostty_kitty_graphics_placement_render_info(it, image, self.raw, &mut info) };
        if rc != ffi::SUCCESS || info.pixel_height == 0 {
            return None;
        }
        // Reaching the screen the program writes to: its row there. Wholly in
        // the history sent: its line there. (Not the viewport: this terminal's
        // own one may be scrolled back by a read.)
        let (top, col) = self.placement_top(it, image)?;
        let screen = self.history_rows() as i64;
        let bottom = top + info.grid_rows.max(1) as i64;
        let (at, mut row) = if bottom > screen {
            (At::Screen, top - screen)
        } else {
            let from = history?;
            if bottom <= from as i64 {
                return None;
            }
            (At::History(0), top - from as i64)
        };
        // Partly above the first row it can go on: place what is below it, from that row.
        let (mut sy, mut sh, mut grid_rows) = (info.source_y, info.source_height, info.grid_rows);
        if row < 0 {
            let hidden = row.unsigned_abs() as u32;
            // Pixels drawn above that row, in the source image's scale.
            let above = (hidden as u64 * self.hooks.cell.1 as u64).saturating_sub(y as u64);
            let cut =
                (above * info.source_height as u64 / info.pixel_height as u64).min(info.source_height as u64) as u32;
            (row, sy, sh, y) = (0, info.source_y + cut, info.source_height - cut, 0);
            rows = rows.saturating_sub(hidden);
            grid_rows = grid_rows.saturating_sub(hidden);
            if sh == 0 {
                return None;
            }
        }
        let drawn_scale = (info.pixel_width as f64 / info.source_width.max(1) as f64)
            .max(info.pixel_height as f64 / info.source_height.max(1) as f64);
        Some(Placed {
            id,
            pid,
            at: match at {
                At::Screen => At::Screen,
                At::History(_) => At::History(row as u32),
            },
            drawn_scale,
            shape: Shape::Placed {
                row: row as u32,
                col,
                source: [info.source_x, sy, info.source_width, sh],
                offset: [x, y],
                cells: [cols, rows],
                grid: [info.grid_cols, grid_rows],
                z,
            },
        })
    }

    /// A placement's top-left cell, as a row from the top of the history and a column.
    fn placement_top(&self, it: Placements, image: Image) -> Option<(i64, u32)> {
        let none = ffi::GridRef { size: size_of::<ffi::GridRef>(), node: ptr::null_mut(), x: 0, y: 0 };
        let mut rect = ffi::Selection { size: size_of::<ffi::Selection>(), start: none, end: none, rectangle: false };
        if unsafe { ghostty_kitty_graphics_placement_rect(it, image, self.raw, &mut rect) } != ffi::SUCCESS {
            return None;
        }
        let mut at = ffi::Coord { x: 0, y: 0 };
        let rc = unsafe { ghostty_terminal_point_from_grid_ref(self.raw, &rect.start, POINT_SCREEN, &mut at) };
        (rc == ffi::SUCCESS).then_some((at.y as i64, at.x as u32))
    }
}

/// A transmission of base64 `encoded` data, in the chunks the kitty protocol allows.
fn transmission(head: &str, quiet: &str, encoded: &[u8]) -> Vec<u8> {
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
